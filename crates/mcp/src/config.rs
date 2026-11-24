//! User configuration and the host-owned persistence boundary.
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, ensure};
use http::HeaderName;
use serde::Deserialize;
use url::Url;
use zevria_foundation::{AcpMcpServerSpec, McpServerName, McpTransportSummary, PlanMcpPolicy};

pub use crate::credential::McpCredentialSource;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub enabled: bool,
    pub startup_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
    pub max_output_bytes: usize,
    pub servers: BTreeMap<McpServerName, McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            startup_timeout_seconds: 30,
            tool_timeout_seconds: 120,
            max_output_bytes: 256 * 1024,
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub transport: Option<McpTransportKind>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub bearer_token: Option<McpCredentialSource>,
    pub headers: BTreeMap<String, McpCredentialSource>,
    pub startup_timeout_seconds: Option<u64>,
    pub tool_timeout_seconds: Option<u64>,
    pub required: bool,
    pub plan_mode: PlanMcpPolicy,
    pub enabled_tools: Option<HashSet<String>>,
    pub disabled_tools: HashSet<String>,
    pub plan_tools: HashSet<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: None,
            command: None,
            args: vec![],
            cwd: None,
            env: BTreeMap::new(),
            url: None,
            bearer_token: None,
            headers: BTreeMap::new(),
            startup_timeout_seconds: None,
            tool_timeout_seconds: None,
            required: false,
            plan_mode: PlanMcpPolicy::default(),
            enabled_tools: None,
            disabled_tools: HashSet::new(),
            plan_tools: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("enabled", &self.enabled)
            .field("transport", &self.transport)
            .field("required", &self.required)
            .field("plan_mode", &self.plan_mode)
            .finish_non_exhaustive()
    }
}

impl McpConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_seconds(self.startup_timeout_seconds)?;
        validate_seconds(self.tool_timeout_seconds)?;
        ensure!(
            self.max_output_bytes >= 1024,
            "MCP max_output_bytes must be at least 1024"
        );
        for (name, server) in &self.servers {
            server
                .validate()
                .with_context(|| format!("MCP server '{name}'"))?;
        }
        Ok(())
    }

    pub fn bind_config_directory(&mut self, directory: &Path) {
        let directory = std::path::absolute(if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            directory
        })
        .unwrap_or_else(|_| directory.to_path_buf());
        for server in self.servers.values_mut() {
            for source in server
                .bearer_token
                .iter_mut()
                .chain(server.headers.values_mut())
            {
                source.bind_directory(&directory);
            }
        }
    }
}

pub(crate) fn validate_seconds(seconds: u64) -> anyhow::Result<()> {
    ensure!(
        (1..=365 * 24 * 3600).contains(&seconds),
        "MCP timeout or refresh interval must be positive and at most one year"
    );
    Ok(())
}

impl McpServerConfig {
    pub(crate) fn delegated(spec: AcpMcpServerSpec) -> (McpServerName, Self) {
        match spec {
            AcpMcpServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => (
                name,
                Self {
                    command: Some(command),
                    args,
                    env,
                    ..Self::default()
                },
            ),
            AcpMcpServerSpec::Http { name, url, headers } => (
                name,
                Self {
                    url: Some(url),
                    headers: headers
                        .into_iter()
                        .map(|(k, v)| (k, McpCredentialSource::literal(v)))
                        .collect(),
                    ..Self::default()
                },
            ),
        }
    }

    pub(crate) fn kind(&self) -> McpTransportKind {
        self.transport.unwrap_or(if self.url.is_some() {
            McpTransportKind::Http
        } else {
            McpTransportKind::Stdio
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for seconds in self
            .startup_timeout_seconds
            .into_iter()
            .chain(self.tool_timeout_seconds)
        {
            validate_seconds(seconds)?;
        }
        match self.kind() {
            McpTransportKind::Stdio => {
                ensure!(
                    self.url.is_none() && self.headers.is_empty() && self.bearer_token.is_none(),
                    "stdio transport cannot contain HTTP settings"
                );
                ensure!(
                    self.command
                        .as_ref()
                        .is_some_and(|cmd| !cmd.trim().is_empty() && !cmd.contains('\0')),
                    "stdio transport requires a valid command"
                );
                ensure!(
                    self.env.iter().all(|(k, v)| !k.is_empty()
                        && !k.contains(['=', '\0'])
                        && !v.contains('\0'))
                        && self.args.iter().all(|arg| !arg.contains('\0')),
                    "invalid stdio arguments or environment"
                );
            }
            McpTransportKind::Http => {
                ensure!(
                    self.command.is_none()
                        && self.args.is_empty()
                        && self.cwd.is_none()
                        && self.env.is_empty(),
                    "HTTP transport cannot contain stdio settings"
                );
                let url = self
                    .url
                    .as_deref()
                    .and_then(|url| Url::parse(url).ok())
                    .context("HTTP transport requires a valid URL")?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.host().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none(),
                    "HTTP transport requires an http(s) URL without URL credentials or a fragment"
                );
                let mut names = BTreeSet::new();
                for (name, credential) in &self.headers {
                    let name = HeaderName::from_bytes(name.as_bytes())
                        .ok()
                        .context("invalid HTTP header name")?;
                    ensure!(
                        names.insert(name.as_str().to_string()),
                        "duplicate HTTP header name"
                    );
                    ensure!(
                        !matches!(
                            name.as_str(),
                            "accept"
                                | "mcp-session-id"
                                | "last-event-id"
                                | "mcp-protocol-version"
                                | "mcp-method"
                                | "mcp-name"
                                | "host"
                                | "content-length"
                                | "content-type"
                                | "transfer-encoding"
                                | "connection"
                        ) && !name.as_str().starts_with("mcp-param-"),
                        "HTTP header name is reserved for the transport: {name}"
                    );
                    ensure!(
                        name != http::header::AUTHORIZATION || self.bearer_token.is_none(),
                        "use either an Authorization header or bearer_token, not both"
                    );
                    credential.validate()?;
                }
                if let Some(token) = &self.bearer_token {
                    token.validate()?;
                }
            }
        }
        Ok(())
    }

    pub fn transport_summary(&self) -> McpTransportSummary {
        match self.kind() {
            McpTransportKind::Stdio => McpTransportSummary::Stdio {
                command: self.command.clone().unwrap_or_default(),
                // Arguments may contain API keys. The view conveys their count, never their values.
                args: vec!["[redacted]".into(); self.args.len()],
            },
            McpTransportKind::Http => McpTransportSummary::Http {
                redacted_url: self.url.as_deref().map_or_else(String::new, redact_url),
            },
        }
    }

    pub(crate) fn startup_timeout(&self, config: &McpConfig) -> Duration {
        Duration::from_secs(
            self.startup_timeout_seconds
                .unwrap_or(config.startup_timeout_seconds),
        )
    }

    pub(crate) fn tool_timeout(&self, config: &McpConfig) -> Duration {
        Duration::from_secs(
            self.tool_timeout_seconds
                .unwrap_or(config.tool_timeout_seconds),
        )
    }

    pub(crate) fn tool_enabled(&self, name: &str) -> bool {
        self.enabled_tools
            .as_ref()
            .is_none_or(|names| names.contains(name))
            && !self.disabled_tools.contains(name)
    }

    pub(crate) fn plan_tool_allowed(&self, name: &str, read_only: bool) -> bool {
        self.tool_enabled(name)
            && (self.plan_tools.contains(name)
                || self.plan_mode == PlanMcpPolicy::All
                || (self.plan_mode == PlanMcpPolicy::ReadOnly && read_only))
    }
}

pub fn redact_url(value: &str) -> String {
    Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| format!("{}{}", url.origin().ascii_serialization(), url.path()))
        .unwrap_or_else(|| "[invalid URL]".into())
}

#[derive(Debug, Clone)]
pub struct McpConfigSource {
    pub config: McpConfig,
    pub revision: String,
}

pub trait McpConfigPersister: Send + Sync {
    fn load(&self) -> anyhow::Result<McpConfigSource>;
    fn set_enabled(
        &self,
        expected_revision: &str,
        server: &McpServerName,
        enabled: bool,
    ) -> anyhow::Result<McpConfigSource>;
}

pub(crate) fn direct_command(
    command: &str,
    base: &Path,
) -> std::io::Result<tokio::process::Command> {
    let path = Path::new(command);
    if path.is_absolute() || command.contains(std::path::is_separator) {
        Ok(tokio::process::Command::new(base.join(path)))
    } else {
        rmcp::transport::which_command(command)
    }
}
