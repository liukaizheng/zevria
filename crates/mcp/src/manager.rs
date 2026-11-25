//! One root session owns its connections and stages catalog changes until a turn boundary.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, RwLock, Weak},
    time::Duration,
};

use futures_util::FutureExt as _;
use rig_agent::tool::server::ToolServerHandle;
use rig_core::tool::{DynamicTool, ManagedToolSink};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zevria_foundation::{
    AcpMcpServerSpec, McpManagementRequest, McpManagementResult, McpServerName, McpServerOrigin,
    McpServerStatus, McpToolAccess,
};
use zevria_session_api::mcp::{McpSessionService, McpSyncSummary};

use crate::{
    catalog::Catalog,
    config::{McpConfig, McpConfigPersister, McpConfigSource, McpServerConfig},
    resource_tools,
    transport::StderrTail,
};

mod connection;
mod service;
mod state;

use connection::{Connection, close_connections, signal};
pub(crate) use state::Target;
use state::{Entry, State};

pub struct McpManager {
    state: RwLock<State>,
    http: tokio::sync::OnceCell<reqwest::Client>,
    mutations: tokio::sync::Mutex<()>,
    changes: watch::Sender<u64>,
    tools: ToolServerHandle,
    workspace: PathBuf,
    root: Option<String>,
    delegated: BTreeMap<McpServerName, McpServerConfig>,
    store: Option<Arc<dyn McpConfigPersister>>,
    this: Weak<Self>,
    stop: CancellationToken,
}

impl McpManager {
    pub async fn start(
        source: McpConfigSource,
        delegated: Vec<AcpMcpServerSpec>,
        workspace: PathBuf,
        tools: ToolServerHandle,
        store: Option<Arc<dyn McpConfigPersister>>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut specs = BTreeMap::new();
        for spec in delegated {
            let (name, spec) = McpServerConfig::delegated(spec);
            spec.validate()
                .map_err(|error| anyhow::anyhow!("delegated MCP server {name}: {error}"))?;
            anyhow::ensure!(
                specs.insert(name, spec).is_none(),
                "duplicate ACP MCP server name"
            );
        }
        source.config.validate()?;
        let root = std::path::absolute(&workspace)
            .ok()
            .and_then(|path| url::Url::from_directory_path(path).ok())
            .map(|url| url.to_string());
        let (changes, _) = watch::channel(0);
        let manager = Arc::new_cyclic(|this| Self {
            state: RwLock::new(State {
                source: source.clone(),
                entries: BTreeMap::new(),
                managed: HashMap::new(),
                wire_index: HashMap::new(),
                resource_access: Vec::new(),
                generation: 0,
                sync_needed: false,
            }),
            http: tokio::sync::OnceCell::new(),
            mutations: tokio::sync::Mutex::new(()),
            changes,
            tools,
            workspace,
            root,
            delegated: specs,
            store,
            this: this.clone(),
            stop: CancellationToken::new(),
        });
        manager.validate_delegation(&source.config)?;
        let entries = manager
            .open_servers(&source.config, manager.server_configs(&source.config))
            .boxed()
            .await;
        let required_failures: Vec<_> = entries
            .values()
            .filter_map(|entry| {
                (entry.config.required
                    && matches!(
                        entry.status(),
                        McpServerStatus::Failed { .. } | McpServerStatus::Disconnected
                    ))
                .then_some(entry.server.to_string())
            })
            .collect();
        {
            let mut state = manager.state.write().unwrap();
            state.entries = entries;
            if required_failures.is_empty() {
                state.rebuild_access_index();
            }
        }
        if !required_failures.is_empty() {
            manager.shutdown(Duration::from_secs(5)).await;
            anyhow::bail!(
                "required MCP servers failed: {}",
                required_failures.join(", ")
            );
        }
        manager.publish_registry();
        Ok(manager)
    }

    fn validate_delegation(&self, config: &McpConfig) -> anyhow::Result<()> {
        if let Some(name) = self
            .delegated
            .keys()
            .find(|name| config.servers.contains_key(*name))
        {
            anyhow::bail!("ACP MCP server '{name}' conflicts with a configured server");
        }
        Ok(())
    }

    fn server_configs<'a>(
        &'a self,
        config: &'a McpConfig,
    ) -> impl Iterator<Item = (&'a McpServerName, &'a McpServerConfig, McpServerOrigin)> {
        config
            .servers
            .iter()
            .map(|(name, spec)| (name, spec, McpServerOrigin::Config))
            .chain(
                self.delegated
                    .iter()
                    .map(|(name, spec)| (name, spec, McpServerOrigin::Acp)),
            )
    }

    /// Called with mutation ownership. Build definitions under a read lock so
    /// concurrent authorization and picker queries do not wait for schema work.
    fn publish_registry(&self) {
        let mut tools = {
            let state = self.state.read().unwrap();
            let mut tools = Vec::<DynamicTool>::new();
            if !self.stop.is_cancelled() && state.source.config.enabled {
                if state
                    .resource_access
                    .iter()
                    .any(|flags| flags.allows(McpToolAccess::AllEnabled))
                {
                    // Keep the built-ins before wire tools to preserve the
                    // advertised order and the cacheable prefix.
                    tools.extend(resource_tools::helpers(self.this.clone()));
                }
                for entry in state.entries.values() {
                    let Some(target) = entry.target(&state.source.config) else {
                        continue;
                    };
                    for (name, wire) in &entry.wire_names {
                        if entry.config.tool_enabled(name) {
                            let tool = &entry.catalog.tools[name];
                            tools.push(resource_tools::external_tool(target.clone(), tool, wire));
                        }
                    }
                }
            }
            tools
        };
        let mut state = self.state.write().unwrap();
        if self.stop.is_cancelled() {
            tools.clear();
        }
        state.managed = self
            .tools
            .reconcile_managed_tools(std::mem::take(&mut state.managed), tools);
    }

    fn sync(&self) -> Option<McpSyncSummary> {
        let mut state = self.state.write().unwrap();
        if self.stop.is_cancelled() {
            // Leave connection handles for shutdown to await.
            return None;
        }
        let mut changed = false;
        for entry in state.entries.values_mut() {
            let Some(conn) = &entry.connection else {
                continue;
            };
            if !conn.live() {
                entry.connection = None;
                entry.status = McpServerStatus::Disconnected;
                entry.set_catalog(Catalog::default());
                changed = true;
                continue;
            }
            let staged = conn.staged.lock().unwrap().take();
            if let Some(catalog) = staged
                && catalog != entry.catalog
            {
                entry.set_catalog(catalog);
                changed = true;
            }
        }
        if changed {
            state.generation += 1;
            state.rebuild_access_index();
        }
        let summary =
            (std::mem::take(&mut state.sync_needed) || changed).then(|| state.sync_summary());
        drop(state);
        if changed {
            self.publish_registry();
        }
        summary
    }

    async fn commit_opened(
        &self,
        entries: impl IntoIterator<Item = (McpServerName, Entry)>,
        update: impl FnOnce(&mut State),
    ) -> McpManagementResult {
        {
            let mut state = self.state.write().unwrap();
            if !self.stop.is_cancelled() {
                update(&mut state);
                state.entries.extend(entries);
                state.generation += 1;
                // Changed already tells the engine to invalidate its plan.
                state.sync_needed = false;
                state.rebuild_access_index();
                let changed = state.changed(false);
                drop(state);
                self.publish_registry();
                return changed;
            }
        }
        // Other servers may already have opened successfully while one was
        // cancelled. Await their closure; they were never installed in state.
        let retiring = entries
            .into_iter()
            .filter_map(|(_, entry)| entry.connection)
            .map(|conn| (conn.stop.clone(), conn.closed.clone()));
        close_connections(retiring, Duration::from_secs(5)).await;
        closed()
    }

    async fn mutate(&self, request: McpManagementRequest) -> McpManagementResult {
        let _operation = self.mutations.lock().await;
        if self.stop.is_cancelled() {
            return closed();
        }
        match request {
            McpManagementRequest::Reload => {
                let source = if let Some(store) = self.store.clone() {
                    match tokio::task::spawn_blocking(move || store.load()).await {
                        Ok(Ok(source)) => source,
                        Ok(Err(error)) => {
                            return McpManagementResult::error(
                                "config",
                                format!("Could not reload MCP configuration: {error}"),
                            );
                        }
                        Err(_) => {
                            return McpManagementResult::error(
                                "config",
                                "Could not reload MCP configuration.",
                            );
                        }
                    }
                } else {
                    self.state.read().unwrap().source.clone()
                };
                if let Err(error) = source
                    .config
                    .validate()
                    .and_then(|()| self.validate_delegation(&source.config))
                {
                    return McpManagementResult::error(
                        "config",
                        format!("Could not reload MCP configuration: {error}"),
                    );
                }
                let mut preserved = BTreeSet::new();
                let mut reopen = Vec::new();
                let retiring = {
                    let state = self.state.read().unwrap();
                    for (name, spec, origin) in self.server_configs(&source.config) {
                        if source.config.enabled
                            && state.entries.get(name).is_some_and(|entry| {
                                entry.config == *spec
                                    && entry.origin == origin
                                    && spec.startup_timeout(&source.config)
                                        == spec.startup_timeout(&state.source.config)
                                    && entry.connection.as_ref().is_some_and(Connection::live)
                            })
                        {
                            preserved.insert(name.clone());
                        } else {
                            reopen.push((name, spec, origin));
                        }
                    }
                    state
                        .entries
                        .iter()
                        .filter(|(name, _)| !preserved.contains(*name))
                        .filter_map(|(_, entry)| entry.connection.as_ref())
                        .map(|conn| (conn.stop.clone(), conn.closed.clone()))
                        .collect::<Vec<_>>()
                };
                close_connections(retiring, Duration::from_secs(5)).await;
                let entries = self.open_servers(&source.config, reopen).await;
                self.commit_opened(entries, |state| {
                    state.source = source;
                    state.entries.retain(|name, _| preserved.contains(name));
                })
                .await
            }
            McpManagementRequest::Reconnect { server } => {
                let (source, config, origin) = {
                    let state = self.state.read().unwrap();
                    let Some(entry) = state.entries.get(&server) else {
                        return McpManagementResult::error(
                            "not_found",
                            format!("Unknown MCP server '{server}'."),
                        );
                    };
                    (state.source.clone(), entry.config.clone(), entry.origin)
                };
                if !source.config.enabled || !config.enabled {
                    return McpManagementResult::error(
                        "disabled",
                        format!("MCP server '{server}' is disabled."),
                    );
                }
                let entry = self
                    .reconnect(&source.config, server.clone(), config, origin)
                    .await;
                self.commit_opened([(server, entry)], |_| {}).await
            }
            McpManagementRequest::SetEnabled {
                expected_revision,
                server,
                enabled,
            } => {
                let revision = {
                    let state = self.state.read().unwrap();
                    if state.revision() != expected_revision {
                        return conflict();
                    }
                    let Some(entry) = state.entries.get(&server) else {
                        return McpManagementResult::error(
                            "not_found",
                            format!("Unknown MCP server '{server}'."),
                        );
                    };
                    if entry.origin != McpServerOrigin::Config {
                        return McpManagementResult::error(
                            "read_only",
                            "Edit this delegated server in the ACP client.",
                        );
                    }
                    state.source.revision.clone()
                };
                let Some(store) = self.store.clone() else {
                    return McpManagementResult::error(
                        "read_only",
                        "MCP configuration is read-only.",
                    );
                };
                let result = tokio::task::spawn_blocking({
                    let server = server.clone();
                    let revision = revision.clone();
                    move || store.set_enabled(&revision, &server, enabled)
                })
                .await;
                let source = match result {
                    Ok(Ok(source)) => source,
                    Ok(Err(error)) => {
                        return McpManagementResult::error(
                            "config",
                            format!("Could not change MCP configuration: {error}"),
                        );
                    }
                    Err(_) => {
                        return McpManagementResult::error(
                            "config",
                            "Could not change MCP configuration.",
                        );
                    }
                };
                if source.revision == revision {
                    let state = self.state.read().unwrap();
                    return if self.stop.is_cancelled() {
                        closed()
                    } else {
                        state.changed(true)
                    };
                }
                let config = source.config.servers[&server].clone();
                let retiring = {
                    // The write has committed. Publish the persisted revision
                    // and configuration before waiting on any network work.
                    let mut state = self.state.write().unwrap();
                    state.source = source.clone();
                    let entry = state.entries.get_mut(&server).expect("validated server");
                    entry.config = config.clone();
                    if self.stop.is_cancelled() {
                        // Keep the committed configuration, but let shutdown
                        // own the old connection and registry cleanup.
                        return closed();
                    }
                    let conn = entry.connection.take();
                    entry.status = if source.config.enabled && config.enabled {
                        McpServerStatus::Disconnected
                    } else {
                        McpServerStatus::Disabled
                    };
                    entry.connect_ms = None;
                    entry.stderr = StderrTail::default();
                    entry.set_catalog(Catalog::default());
                    state.rebuild_access_index();
                    if conn.is_some() {
                        // A cancelled request won't return Changed. Keep the
                        // catalog invalidation pending for the next engine sync.
                        state.generation += 1;
                        state.sync_needed = true;
                    }
                    conn
                };
                self.publish_registry();
                if let Some(conn) = retiring {
                    signal(&self.changes);
                    close_connections(
                        [(conn.stop.clone(), conn.closed.clone())],
                        Duration::from_secs(5),
                    )
                    .await;
                }
                let entry = self
                    .open(
                        server.clone(),
                        config,
                        McpServerOrigin::Config,
                        &source.config,
                    )
                    .await;
                self.commit_opened([(server, entry)], |_| {}).await
            }
            query => self.query_view(&query),
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn closed() -> McpManagementResult {
    McpManagementResult::error("closed", "The MCP session is shutting down.")
}

fn conflict() -> McpManagementResult {
    McpManagementResult::error(
        "conflict",
        "The MCP configuration or catalog changed. Refresh the view and try again.",
    )
}
