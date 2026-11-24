//! Literal and command credentials. Only the resolved header, never helper output or errors,
//! leaves this module. A rejected generation is refreshed at most once by concurrent requests.
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, ensure};
use http::HeaderValue;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::{direct_command, validate_seconds};

#[derive(Clone)]
pub struct McpCredentialSource(Source);

#[derive(Clone)]
enum Source {
    Literal(String),
    Command(Arc<CommandSource>),
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
    #[serde(default = "default_refresh")]
    refresh_interval_seconds: u64,
    #[serde(skip)]
    directory: PathBuf,
}

fn default_timeout() -> u64 {
    30
}
fn default_refresh() -> u64 {
    300
}

struct CommandSource {
    config: CommandConfig,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    generation: u64,
    value: Option<(Instant, HeaderValue)>,
}

impl PartialEq for McpCredentialSource {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Source::Literal(left), Source::Literal(right)) => left == right,
            (Source::Command(left), Source::Command(right)) => left.config == right.config,
            _ => false,
        }
    }
}

impl Eq for McpCredentialSource {}

impl fmt::Debug for McpCredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Source::Literal(_) => "McpCredentialSource::Literal([redacted])",
            Source::Command(_) => "McpCredentialSource::Command([redacted])",
        })
    }
}

impl<'de> Deserialize<'de> for McpCredentialSource {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Field {
            Literal(String),
            Command(CommandConfig),
        }
        Ok(match Field::deserialize(d)? {
            Field::Literal(value) => Self::literal(value),
            Field::Command(config) => Self::command(config),
        })
    }
}

pub(crate) struct ResolvedCredential {
    pub header: HeaderValue,
    pub generation: Option<u64>,
}

fn header(value: &str) -> anyhow::Result<HeaderValue> {
    ensure!(!value.trim().is_empty(), "MCP credential cannot be blank");
    let mut header = HeaderValue::from_str(value).context("invalid MCP HTTP credential value")?;
    // reqwest accepts opaque bytes in HeaderValue, but auth templates use visible text.
    header
        .to_str()
        .context("MCP credentials must be valid visible HTTP header text")?;
    header.set_sensitive(true);
    Ok(header)
}

impl McpCredentialSource {
    pub(crate) fn is_command(&self) -> bool {
        matches!(self.0, Source::Command(_))
    }

    pub(crate) fn literal(value: String) -> Self {
        Self(Source::Literal(value))
    }

    fn command(config: CommandConfig) -> Self {
        Self(Source::Command(Arc::new(CommandSource {
            config,
            cache: Mutex::default(),
        })))
    }

    pub(crate) fn bind_directory(&mut self, directory: &Path) {
        if let Source::Command(source) = &self.0 {
            let mut config = source.config.clone();
            config.directory = directory.into();
            *self = Self::command(config);
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        match &self.0 {
            Source::Literal(value) => {
                header(value)?;
            }
            Source::Command(source) => {
                ensure!(
                    !source.config.command.trim().is_empty()
                        && !source.config.command.contains('\0')
                        && source.config.args.iter().all(|arg| !arg.contains('\0')),
                    "invalid MCP credential helper command or arguments"
                );
                validate_seconds(source.config.timeout_seconds)?;
                validate_seconds(source.config.refresh_interval_seconds)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn resolve(
        &self,
        rejected: Option<u64>,
    ) -> anyhow::Result<ResolvedCredential> {
        match &self.0 {
            Source::Literal(value) => Ok(ResolvedCredential {
                header: header(value)?,
                generation: None,
            }),
            Source::Command(source) => {
                let mut cache = source.cache.lock().await;
                if let Some((expires, header)) = &cache.value
                    && *expires > Instant::now()
                    && rejected != Some(cache.generation)
                {
                    return Ok(ResolvedCredential {
                        header: header.clone(),
                        generation: Some(cache.generation),
                    });
                }
                cache.value = None;
                let config = &source.config;
                let mut command = direct_command(&config.command, &config.directory)
                    .ok()
                    .context("MCP credential helper executable not found")?;
                command.args(&config.args);
                if !config.directory.as_os_str().is_empty() {
                    command.current_dir(&config.directory);
                }
                let output = zevria_foundation::process::bounded_output(
                    &mut command,
                    Duration::from_secs(config.timeout_seconds),
                    64 * 1024,
                )
                .await
                .ok()
                .context("MCP credential helper could not run, timed out, or exceeded 64 KiB")?;
                ensure!(
                    output.status.success(),
                    "MCP credential helper exited unsuccessfully"
                );
                let text = std::str::from_utf8(&output.stdout)
                    .context("MCP credential helper must return UTF-8")?;
                let header = header(text.trim_end_matches(['\n', '\r']))?;
                cache.generation = cache.generation.wrapping_add(1);
                cache.value = Some((
                    Instant::now() + Duration::from_secs(config.refresh_interval_seconds),
                    header.clone(),
                ));
                Ok(ResolvedCredential {
                    header,
                    generation: Some(cache.generation),
                })
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_is_cached_refreshed_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let source = McpCredentialSource::command(CommandConfig {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf x >> count; cat count; printf '\\n'".into(),
            ],
            timeout_seconds: 2,
            refresh_interval_seconds: 300,
            directory: dir.path().into(),
        });
        let first = source.resolve(None).await.unwrap();
        assert_eq!(first.header, "x");
        assert!(first.header.is_sensitive());
        assert_eq!(source.resolve(None).await.unwrap().header, "x");
        assert_eq!(source.resolve(first.generation).await.unwrap().header, "xx");
        assert_eq!(source.resolve(first.generation).await.unwrap().header, "xx");
        assert_eq!(std::fs::read_to_string(count).unwrap(), "xx");
        assert!(!format!("{source:?}").contains("/bin/sh"));
    }
}
