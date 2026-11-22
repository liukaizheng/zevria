//! Offline MCP configuration, transactional edits, and one-shot CLI discovery.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};
use zevria_foundation::{
    McpManagementRequest, McpManagementResult, McpManagementView, McpServerName, McpServerOrigin,
    McpServerStatus, McpServerSummary,
};
use zevria_mcp::McpManager;
use zevria_session_api::mcp::McpSessionService;

pub use zevria_mcp::{McpConfig, McpConfigPersister, McpConfigSource};

pub fn parse_mcp_config(contents: &str) -> anyhow::Result<McpConfig> {
    let document = crate::config::partial_document(contents)?;
    let config: McpConfig = document
        .get("mcp")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid [mcp] structure, field name, or value type"))?
        .unwrap_or_default();
    config.validate()?;
    Ok(config)
}

fn load_contents(contents: &str, path: &Path) -> anyhow::Result<McpConfigSource> {
    let mut config = parse_mcp_config(contents)?;
    config.bind_config_directory(
        path.parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    );
    let revision = Sha256::digest(contents.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(McpConfigSource { config, revision })
}

pub fn load_mcp_config(path: &Path) -> anyhow::Result<McpConfigSource> {
    match std::fs::read_to_string(path) {
        Ok(contents) => load_contents(&contents, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => load_contents("", path),
        Err(error) => Err(error)
            .with_context(|| format!("cannot read MCP configuration at {}", path.display())),
    }
}

#[derive(Clone)]
pub struct LocalMcpConfigStore {
    path: PathBuf,
}

impl LocalMcpConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl McpConfigPersister for LocalMcpConfigStore {
    fn load(&self) -> anyhow::Result<McpConfigSource> {
        load_mcp_config(&self.path)
    }

    fn set_enabled(
        &self,
        expected_revision: &str,
        server: &McpServerName,
        enabled: bool,
    ) -> anyhow::Result<McpConfigSource> {
        crate::settings::transaction(&self.path, |before| {
            let source = load_contents(before, &self.path)?;
            anyhow::ensure!(
                source.revision == expected_revision,
                "MCP configuration changed externally; reload before writing"
            );
            let config = source
                .config
                .servers
                .get(server)
                .with_context(|| format!("unknown configured MCP server '{server}'"))?;
            if config.enabled == enabled {
                return Ok((before.into(), source));
            }
            let mut document: toml_edit::DocumentMut = before
                .parse()
                .map_err(|_| anyhow::anyhow!("cannot edit MCP configuration"))?;
            let slot = &mut document["mcp"]["servers"][server.as_str()]["enabled"];
            let mut replacement = toml_edit::Value::from(enabled);
            if let Some(previous) = slot.as_value() {
                *replacement.decor_mut() = previous.decor().clone();
            }
            *slot = toml_edit::Item::Value(replacement);
            let after = document.to_string();
            let updated = load_contents(&after, &self.path)?;
            Ok((after, updated))
        })
    }
}

/// Summarize configuration without executing servers or credential helpers.
pub fn offline_view(source: &McpConfigSource) -> McpManagementView {
    let servers = source
        .config
        .servers
        .iter()
        .map(|(name, config)| McpServerSummary {
            name: name.clone(),
            origin: McpServerOrigin::Config,
            enabled: config.enabled,
            transport: config.transport_summary(),
            status: if source.config.enabled && config.enabled {
                McpServerStatus::Disconnected
            } else {
                McpServerStatus::Disabled
            },
            plan_mode: config.plan_mode,
            tool_count: 0,
            resource_count: 0,
            prompt_count: 0,
            connect_ms: None,
        })
        .collect();
    McpManagementView {
        revision: source.revision.clone(),
        enabled: source.config.enabled,
        servers,
        selected: None,
        diagnostics: vec![],
        prompts: vec![],
    }
}

/// Deliberately bypass model/provider setup. An explicitly disabled server is
/// never started, even for a diagnostic probe; its status is still returned.
pub async fn probe(
    mut source: McpConfigSource,
    workspace: PathBuf,
    server: Option<McpServerName>,
    inspect: bool,
) -> anyhow::Result<McpManagementView> {
    if let Some(name) = &server {
        anyhow::ensure!(
            source.config.servers.contains_key(name),
            "unknown configured MCP server '{name}'"
        );
        source.config.servers.retain(|server, _| server == name);
    } else {
        source.config.servers.retain(|_, server| server.enabled);
    }
    // Report individual failures with latency and diagnostics for CLI tests.
    for server in source.config.servers.values_mut() {
        server.required = false;
    }
    let tools = rig_agent::tool::server::ToolServer::new().run();
    let manager = McpManager::start(source, vec![], workspace, tools, None).await?;
    let result = manager.query_view(&if inspect {
        McpManagementRequest::Inspect {
            server: server.context("inspect requires a server")?,
        }
    } else {
        McpManagementRequest::List
    });
    manager.shutdown(Duration::from_secs(5)).await;
    match result {
        McpManagementResult::View { view } => Ok(view),
        McpManagementResult::Error { message, .. } => anyhow::bail!("{message}"),
        _ => unreachable!("MCP probe only queries"),
    }
}
