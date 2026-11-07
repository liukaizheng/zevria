//! Provider-owned credentials. Helpers are resolved only at authentication
//! boundaries, never while parsing, validating, or preparing model routing.
use std::{
    fmt,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::header::HeaderValue;
use serde::{
    Deserialize, Serialize,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use tokio::sync::Mutex;

const OUTPUT_LIMIT: usize = 64 * 1024;

/// Literal API credential from the configuration file.
///
/// Strings are never interpolated or executed. Debug output is always redacted;
/// only request construction inside this crate can access the underlying value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LiteralApiKey(String);

impl LiteralApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LiteralApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Explicit non-interactive credential helper, executed directly without a shell.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_seconds: u64,
}

const fn default_timeout() -> u64 {
    30
}
const fn default_refresh_interval() -> u64 {
    300
}

impl fmt::Debug for ApiKeyCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyCommand")
            .field("command", &self.command)
            .field(
                "args",
                &format_args!("<redacted; {} args>", self.args.len()),
            )
            .field("timeout_seconds", &self.timeout_seconds)
            .field("refresh_interval_seconds", &self.refresh_interval_seconds)
            .finish()
    }
}

/// A cloneable, redacted credential handle. Clones share one command cache;
/// serialization contains only the user-owned configuration, never runtime state.
#[derive(Clone)]
pub struct ApiKeySource(Source);

#[derive(Clone)]
enum Source {
    Literal(LiteralApiKey),
    Command(Arc<CommandSource>),
}

struct CommandSource {
    config: ApiKeyCommand,
    directory: Option<PathBuf>,
    state: Mutex<CommandState>,
    // Tests can exercise expiry and cancellation without second-long sleeps.
    #[cfg(test)]
    durations: Option<(Duration, Duration)>,
}

#[derive(Default)]
struct CommandState {
    cached: Option<CachedKey>,
    generation: u64,
}
struct CachedKey {
    lease: ApiKeyLease,
    obtained: Instant,
}

impl ApiKeySource {
    pub fn literal(value: impl Into<String>) -> Self {
        LiteralApiKey::new(value).into()
    }

    pub fn command(config: ApiKeyCommand) -> Self {
        Self(Source::Command(Arc::new(CommandSource {
            config,
            directory: None,
            state: Mutex::new(CommandState::default()),
            #[cfg(test)]
            durations: None,
        })))
    }

    #[cfg(test)]
    pub(crate) fn with_test_durations(mut self, timeout: Duration, interval: Duration) -> Self {
        if let Source::Command(source) = &mut self.0 {
            Arc::get_mut(source)
                .expect("configure before sharing")
                .durations = Some((timeout, interval));
        }
        self
    }

    pub fn as_command(&self) -> Option<&ApiKeyCommand> {
        match &self.0 {
            Source::Literal(_) => None,
            Source::Command(source) => Some(&source.config),
        }
    }

    /// Bind before resolving profiles. Binding creates a fresh cache and is not
    /// serialized, so revisions remain independent of configuration location.
    pub fn with_config_directory(&self, directory: &Path) -> Self {
        match &self.0 {
            Source::Literal(_) => self.clone(),
            Source::Command(source) => Self(Source::Command(Arc::new(CommandSource {
                config: source.config.clone(),
                directory: Some(directory.to_owned()),
                state: Mutex::new(CommandState::default()),
                #[cfg(test)]
                durations: source.durations,
            }))),
        }
    }

    pub(crate) fn validate(&self, provider: &str) -> anyhow::Result<()> {
        let field = format!("providers.{provider}.api_key");
        match &self.0 {
            Source::Literal(key) => {
                anyhow::ensure!(!key.expose().trim().is_empty(), "{field} must not be blank");
                anyhow::ensure!(
                    HeaderValue::from_str(key.expose()).is_ok(),
                    "{field} must be a valid HTTP header value"
                );
            }
            Source::Command(source) => {
                let config = &source.config;
                anyhow::ensure!(
                    !config.command.trim().is_empty() && !config.command.contains('\0'),
                    "{field}.command must be a nonblank program without NUL bytes"
                );
                anyhow::ensure!(
                    config.timeout_seconds > 0,
                    "{field}.timeout_seconds must be > 0"
                );
                for (name, seconds) in [
                    ("timeout_seconds", config.timeout_seconds),
                    ("refresh_interval_seconds", config.refresh_interval_seconds),
                ] {
                    anyhow::ensure!(
                        Instant::now()
                            .checked_add(Duration::from_secs(seconds))
                            .is_some(),
                        "{field}.{name} is too large"
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn is_refreshable(&self) -> bool {
        matches!(self.0, Source::Command(_))
    }

    pub(crate) async fn current(&self, provider: &str) -> Result<ApiKeyLease, ApiKeyCommandError> {
        match &self.0 {
            Source::Literal(key) => Ok(ApiKeyLease {
                value: Arc::from(key.expose()),
                generation: 0,
            }),
            Source::Command(source) => {
                // Hold the lock through execution: concurrent cold/expired callers
                // share one helper, and cancellation releases it without caching.
                let mut state = source.state.lock().await;
                let (_, interval) = source.durations();
                if let Some(cached) = &state.cached
                    && cached.obtained.elapsed() < interval
                {
                    return Ok(cached.lease.clone());
                }
                // Never retain an expired secret after a failed refresh.
                state.cached = None;
                let value = source.run(provider).await?;
                state.generation = state.generation.checked_add(1).ok_or_else(|| {
                    source.error(
                        provider,
                        "unavailable",
                        "credential generation exhausted",
                        &[],
                    )
                })?;
                let lease = ApiKeyLease {
                    value,
                    generation: state.generation,
                };
                state.cached = Some(CachedKey {
                    lease: lease.clone(),
                    obtained: Instant::now(),
                });
                Ok(lease)
            }
        }
    }

    pub(crate) async fn reject(&self, lease: &ApiKeyLease) {
        if let Source::Command(source) = &self.0 {
            let mut state = source.state.lock().await;
            if state
                .cached
                .as_ref()
                .is_some_and(|cached| cached.lease.generation == lease.generation)
            {
                state.cached = None;
            }
        }
    }
}

impl From<LiteralApiKey> for ApiKeySource {
    fn from(value: LiteralApiKey) -> Self {
        Self(Source::Literal(value))
    }
}

impl fmt::Debug for ApiKeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Source::Literal(key) => key.fmt(f),
            Source::Command(source) => f.debug_tuple("Command").field(&source.config).finish(),
        }
    }
}

impl Serialize for ApiKeySource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Source::Literal(key) => key.serialize(serializer),
            Source::Command(source) => source.config.serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for ApiKeySource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SourceVisitor;
        impl<'de> Visitor<'de> for SourceVisitor {
            type Value = ApiKeySource;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a literal API key string or a command object (command, args, timeout_seconds, refresh_interval_seconds)")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ApiKeySource::literal(value))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ApiKeySource::literal(value))
            }
            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                ApiKeyCommand::deserialize(MapAccessDeserializer::new(map))
                    .map(ApiKeySource::command)
            }
        }
        deserializer.deserialize_any(SourceVisitor)
    }
}

#[derive(Clone)]
pub(crate) struct ApiKeyLease {
    value: Arc<str>,
    generation: u64,
}
impl ApiKeyLease {
    pub(crate) fn expose(&self) -> &str {
        &self.value
    }
}
impl fmt::Debug for ApiKeyLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A terminal local authentication failure, distinguishable from transport
/// failures without exposing helper arguments or any stdout.
#[derive(Debug)]
pub(crate) struct ApiKeyCommandError {
    message: String,
}
impl fmt::Display for ApiKeyCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiKeyCommandError {}

impl CommandSource {
    fn durations(&self) -> (Duration, Duration) {
        #[cfg(test)]
        if let Some(durations) = self.durations {
            return durations;
        }
        (
            Duration::from_secs(self.config.timeout_seconds),
            Duration::from_secs(self.config.refresh_interval_seconds),
        )
    }

    fn error(
        &self,
        provider: &str,
        status: impl fmt::Display,
        reason: &str,
        stderr: &[u8],
    ) -> ApiKeyCommandError {
        let mut stderr = String::from_utf8_lossy(stderr).into_owned();
        for arg in &self.config.args {
            if !arg.is_empty() {
                stderr = stderr.replace(arg, "[argument removed]");
            }
        }
        let excerpt = crate::http::diagnostic_detail(&stderr);
        let status = status.to_string();
        let status = status.strip_prefix("exit status: ").unwrap_or(&status);
        ApiKeyCommandError {
            message: format!(
                "providers.{provider}.api_key: helper {:?} failed (exit status: {status}): {reason}; stderr: {}",
                self.config.command,
                if excerpt.trim().is_empty() {
                    "<empty or unavailable>"
                } else {
                    &excerpt
                }
            ),
        }
    }

    async fn run(&self, provider: &str) -> Result<Arc<str>, ApiKeyCommandError> {
        let started = Instant::now();
        let directory = self
            .directory
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .map_err(|error| self.error(provider, "unavailable", &error.to_string(), &[]))?;
        let program = Path::new(&self.config.command);
        let resolved =
            if program.is_relative() && self.config.command.chars().any(std::path::is_separator) {
                directory.join(program)
            } else {
                program.to_owned()
            };
        let mut command = tokio::process::Command::new(resolved);
        command.args(&self.config.args).current_dir(directory);
        let output = zevria_foundation::process::bounded_output(
            &mut command,
            self.durations().0,
            OUTPUT_LIMIT,
        )
        .await
        .map_err(|error| self.error(provider, "unavailable", &error.to_string(), &[]))?;
        // A helper may echo its own output in an error. Do not repeat any
        // known stdout lines in the bounded stderr diagnostic either.
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.trim_start_matches('\u{feff}').trim();
            if !line.is_empty() {
                stderr = stderr.replace(line, "[stdout removed]");
            }
        }
        if !output.status.success() {
            return Err(self.error(
                provider,
                output.status,
                "helper exited unsuccessfully",
                stderr.as_bytes(),
            ));
        }
        let invalid = |reason| self.error(provider, output.status, reason, stderr.as_bytes());
        let stdout =
            std::str::from_utf8(&output.stdout).map_err(|_| invalid("stdout must be UTF-8"))?;
        let key = stdout.strip_prefix('\u{feff}').unwrap_or(stdout).trim();
        if key.is_empty() || key.contains(['\n', '\r']) || HeaderValue::from_str(key).is_err() {
            return Err(invalid(
                "stdout must contain a non-empty single-line HTTP header value",
            ));
        }
        tracing::info!(provider, program = %self.config.command, elapsed_ms = started.elapsed().as_millis() as u64, "API key helper completed");
        Ok(Arc::from(key))
    }
}

pub(crate) struct Unauthorized {
    pub(crate) upstream_request_id: Option<String>,
}

pub(crate) fn http_unauthorized<E>(result: &Result<reqwest::Response, E>) -> Option<Unauthorized> {
    response_unauthorized(result.as_ref().ok()?)
}

pub(crate) fn response_unauthorized(response: &reqwest::Response) -> Option<Unauthorized> {
    (response.status() == reqwest::StatusCode::UNAUTHORIZED).then(|| Unauthorized {
        upstream_request_id: response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    })
}

/// Refresh a rejected command key inside one exchange, outside all transport
/// deadlines and recovery accounting. The caller reuses its prepared body.
pub(crate) async fn authenticated<T, F, Fut>(
    source: &ApiKeySource,
    provider: &str,
    mut exchange: F,
    unauthorized: impl Fn(&T) -> Option<Unauthorized>,
) -> Result<T, ApiKeyCommandError>
where
    F: FnMut(ApiKeyLease) -> Fut,
    Fut: Future<Output = T>,
{
    let lease = source.current(provider).await?;
    let result = exchange(lease.clone()).await;
    if source.is_refreshable()
        && let Some(rejected) = unauthorized(&result)
    {
        tracing::info!(provider, upstream_request_id = ?rejected.upstream_request_id.as_deref().map(crate::connection::log_identifier), "command-provided API key rejected; refreshing once");
        // Release the failed response/socket before re-running the helper.
        drop(result);
        source.reject(&lease).await;
        let replacement = source.current(provider).await?;
        let result = exchange(replacement.clone()).await;
        if let Some(rejected) = unauthorized(&result) {
            tracing::info!(provider, upstream_request_id = ?rejected.upstream_request_id.as_deref().map(crate::connection::log_identifier), "command-provided API key rejected after refresh");
            source.reject(&replacement).await;
        }
        return Ok(result);
    }
    Ok(result)
}

pub(crate) fn rejected_after_refresh(provider: &str) -> String {
    format!("the key from providers.{provider}.api_key.command was rejected after a refresh")
}

#[cfg(test)]
#[path = "credential_test_support.rs"]
pub(crate) mod test_support;
#[cfg(test)]
#[path = "credential_tests.rs"]
mod tests;
