//! One root session owns its connections and stages catalog changes until a turn boundary.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

use futures_util::{FutureExt as _, future::join_all};
use rig_agent::tool::server::ToolServerHandle;
use rig_core::tool::{DynamicTool, ManagedToolSink, ManagedToolToken};
use rmcp::{RoleClient, model::*, service::Peer};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use zevria_content::UserPrompt;
use zevria_foundation::*;
use zevria_session_api::mcp::{McpSessionService, McpSyncSummary};

use crate::{
    catalog::{Catalog, read_only, wire_names},
    config::{McpConfig, McpConfigPersister, McpConfigSource, McpServerConfig},
    convert::{display_text, get_prompt_result_to_user_prompt},
    resource_tools,
    transport::{self, Service, StderrTail},
};

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

struct State {
    source: McpConfigSource,
    entries: BTreeMap<McpServerName, Entry>,
    managed: HashMap<String, ManagedToolToken>,
    wire_index: HashMap<String, ToolPolicyFlags>,
    resource_access: Vec<ToolPolicyFlags>,
    generation: u64,
    // SetEnabled can change the live catalog before reaching its final commit.
    sync_needed: bool,
}

struct Entry {
    server: McpServerName,
    origin: McpServerOrigin,
    config: McpServerConfig,
    status: McpServerStatus,
    stderr: StderrTail,
    connect_ms: Option<u64>,
    catalog: Catalog,
    wire_names: BTreeMap<String, String>,
    connection: Option<Connection>,
}

struct Connection {
    peer: Peer<RoleClient>,
    stop: CancellationToken,
    _stop_on_drop: DropGuard,
    closed: CancellationToken,
    staged: Arc<Mutex<Option<Catalog>>>,
}

impl Connection {
    fn live(&self) -> bool {
        connection_live(&self.stop, &self.closed, &self.peer)
    }

    fn policy_flags(&self, plan_allowed: bool) -> ToolPolicyFlags {
        ToolPolicyFlags {
            plan_allowed,
            peer: self.peer.clone(),
            stop: self.stop.clone(),
            closed: self.closed.clone(),
        }
    }
}

fn connection_live(
    stop: &CancellationToken,
    closed: &CancellationToken,
    peer: &Peer<RoleClient>,
) -> bool {
    !stop.is_cancelled() && !closed.is_cancelled() && !peer.is_transport_closed()
}

struct ToolPolicyFlags {
    plan_allowed: bool,
    peer: Peer<RoleClient>,
    stop: CancellationToken,
    closed: CancellationToken,
}

impl ToolPolicyFlags {
    fn allows(&self, access: McpToolAccess) -> bool {
        (access == McpToolAccess::AllEnabled
            || (access == McpToolAccess::PlanScoped && self.plan_allowed))
            && connection_live(&self.stop, &self.closed, &self.peer)
    }
}

#[derive(Clone)]
pub(crate) struct Target {
    pub server: McpServerName,
    pub peer: Peer<RoleClient>,
    pub timeout: Duration,
    pub limit: usize,
    pub connection_stop: CancellationToken,
}

impl Entry {
    fn target(&self, global: &McpConfig) -> Option<Target> {
        let conn = self.connection.as_ref().filter(|conn| conn.live())?;
        Some(Target {
            server: self.server.clone(),
            peer: conn.peer.clone(),
            timeout: self.config.tool_timeout(global),
            limit: global.max_output_bytes,
            connection_stop: conn.stop.clone(),
        })
    }

    fn tool_info(&self, original: &str, wire_name: &str) -> McpToolInfo {
        let tool = &self.catalog.tools[original];
        let read_only = read_only(tool);
        McpToolInfo {
            name: original.into(),
            wire_name: wire_name.into(),
            description: tool
                .description
                .as_deref()
                .map(|description| display_text(description, 4096)),
            read_only,
            enabled: self.config.tool_enabled(original),
            plan_allowed: self.config.plan_tool_allowed(original, read_only),
        }
    }

    fn status(&self) -> McpServerStatus {
        if self.connection.as_ref().is_some_and(|conn| !conn.live()) {
            return McpServerStatus::Disconnected;
        }
        match &self.status {
            McpServerStatus::Failed { error, .. } => McpServerStatus::Failed {
                error: error.clone(),
                stderr_tail: self.stderr.text(),
            },
            status => status.clone(),
        }
    }

    fn summary(&self) -> McpServerSummary {
        McpServerSummary {
            name: self.server.clone(),
            origin: self.origin,
            enabled: self.config.enabled,
            status: self.status(),
            transport: self.config.transport_summary(),
            plan_mode: self.config.plan_mode,
            tool_count: self
                .catalog
                .tools
                .keys()
                .filter(|name| self.config.tool_enabled(name))
                .count(),
            resource_count: self.catalog.resources.len() + self.catalog.templates.len(),
            prompt_count: self.catalog.prompts.len(),
            connect_ms: self.connect_ms,
        }
    }

    fn detail(&self) -> McpServerDetail {
        McpServerDetail {
            summary: self.summary(),
            server_info: self.catalog.server_info.clone(),
            instructions: self
                .catalog
                .instructions
                .as_ref()
                .map(|instr| display_text(&instr.body, MAX_MCP_INSTRUCTIONS_BYTES)),
            tools: self
                .wire_names
                .iter()
                .map(|(name, wire)| self.tool_info(name, wire))
                .collect(),
            resources: self.catalog.resources.clone(),
            templates: self.catalog.templates.clone(),
            prompts: self.catalog.prompts.clone(),
        }
    }

    fn allows_read(&self, access: McpToolAccess) -> bool {
        match access {
            McpToolAccess::None => false,
            McpToolAccess::PlanScoped => self.config.plan_mode != PlanMcpPolicy::None,
            McpToolAccess::AllEnabled => true,
        }
    }

    fn set_catalog(&mut self, catalog: Catalog) {
        self.wire_names = wire_names(&self.server, &catalog, &self.wire_names);
        self.catalog = catalog;
    }
}

impl State {
    /// Update authorization in the same write lock as the applied catalog.
    /// This only copies policy metadata and connection handles, not schemas.
    fn rebuild_access_index(&mut self) {
        self.wire_index.clear();
        self.resource_access.clear();
        if !self.source.config.enabled {
            return;
        }
        for entry in self.entries.values() {
            let Some(conn) = entry.connection.as_ref().filter(|conn| conn.live()) else {
                continue;
            };
            if entry.catalog.resource_capability {
                self.resource_access
                    .push(conn.policy_flags(entry.allows_read(McpToolAccess::PlanScoped)));
            }
            for (name, wire) in &entry.wire_names {
                if entry.config.tool_enabled(name) {
                    let plan_allowed = entry
                        .config
                        .plan_tool_allowed(name, read_only(&entry.catalog.tools[name]));
                    self.wire_index
                        .insert(wire.clone(), conn.policy_flags(plan_allowed));
                }
            }
        }
    }

    fn revision(&self) -> String {
        format!("{}:{}", self.source.revision, self.generation)
    }

    fn sync_summary(&self) -> McpSyncSummary {
        let mut connected = 0;
        let mut tool_count = 0;
        for entry in self
            .entries
            .values()
            .filter(|entry| entry.connection.as_ref().is_some_and(Connection::live))
        {
            connected += 1;
            tool_count += entry
                .catalog
                .tools
                .keys()
                .filter(|name| entry.config.tool_enabled(name))
                .count();
        }
        McpSyncSummary {
            revision: self.revision(),
            connected,
            total: self.entries.len(),
            tool_count,
        }
    }

    fn changed(&self, unchanged: bool) -> McpManagementResult {
        let McpSyncSummary {
            revision,
            connected,
            total,
            tool_count,
        } = self.sync_summary();
        McpManagementResult::Changed {
            revision,
            connected,
            total,
            tool_count,
            unchanged,
        }
    }
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

    async fn open_servers<'a>(
        &self,
        global: &McpConfig,
        servers: impl IntoIterator<Item = (&'a McpServerName, &'a McpServerConfig, McpServerOrigin)>,
    ) -> BTreeMap<McpServerName, Entry> {
        join_all(servers.into_iter().map(|(name, spec, origin)| async move {
            (
                name.clone(),
                self.open(name.clone(), spec.clone(), origin, global).await,
            )
        }))
        .await
        .into_iter()
        .collect()
    }

    async fn open(
        &self,
        name: McpServerName,
        config: McpServerConfig,
        origin: McpServerOrigin,
        global: &McpConfig,
    ) -> Entry {
        let mut entry = Entry {
            server: name,
            origin,
            config,
            status: McpServerStatus::Disabled,
            stderr: StderrTail::default(),
            catalog: Catalog::default(),
            wire_names: BTreeMap::new(),
            connection: None,
            connect_ms: None,
        };
        if !global.enabled || !entry.config.enabled || self.stop.is_cancelled() {
            return entry;
        }
        if entry.config.kind() == crate::config::McpTransportKind::Stdio {
            entry.stderr = StderrTail::new(&entry.config);
        }
        let (refresh, rx) = mpsc::channel(1);
        let start = Instant::now();
        let startup_timeout = entry.config.startup_timeout(global);
        let deadline = start + startup_timeout;
        let stop = self.stop.child_token();
        let opening = timeout_at(deadline, async {
            let service = transport::connect(
                &entry.server,
                &entry.config,
                &self.workspace,
                self.root.clone(),
                refresh.clone(),
                entry.stderr.clone(),
                &self.http,
            )
            .await?;
            let catalog = Catalog::fetch(
                &entry.server,
                service.peer(),
                deadline.saturating_duration_since(Instant::now()),
                &stop,
            )
            .await?;
            Ok::<_, anyhow::Error>((service, catalog))
        });
        // Dropping the handshake drops the transport; cancellation does not
        // have to wait for the server or hold the mutation lock until timeout.
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => return entry,
            result = opening => result,
        };
        entry.connect_ms = Some(start.elapsed().as_millis() as u64);
        match result {
            Ok(Ok((service, catalog))) => {
                entry.set_catalog(catalog);
                entry.status = McpServerStatus::Connected;
                entry.connection = Some(supervise(
                    service,
                    entry.server.clone(),
                    rx,
                    refresh,
                    stop,
                    startup_timeout,
                    self.changes.clone(),
                ));
            }
            failure => {
                let error = match failure {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => format!(
                        "MCP startup timed out after {} seconds",
                        startup_timeout.as_secs()
                    ),
                    _ => unreachable!(),
                };
                entry.status = McpServerStatus::Failed {
                    error,
                    stderr_tail: None,
                };
            }
        }
        entry
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

    pub async fn shutdown(&self, grace: Duration) {
        {
            // Linearize cancellation against the state-locked mutation commit,
            // without waiting for a mutation that is still opening connections.
            let _state = self.state.write().unwrap();
            self.stop.cancel();
        }
        let _operation = self.mutations.lock().await;
        let conns = {
            let mut state = self.state.write().unwrap();
            let conns = state
                .entries
                .values_mut()
                .filter_map(|entry| entry.connection.take())
                .collect::<Vec<_>>();
            state.wire_index.clear();
            state.resource_access.clear();
            conns
        };
        self.publish_registry();
        close_connections(
            conns
                .iter()
                .map(|conn| (conn.stop.clone(), conn.closed.clone())),
            grace,
        )
        .await;
    }

    pub(crate) fn output_limit(&self) -> usize {
        self.state.read().unwrap().source.config.max_output_bytes
    }

    pub(crate) fn resource_target(
        &self,
        access: McpToolAccess,
        name: &McpServerName,
    ) -> Option<Target> {
        let state = self.state.read().unwrap();
        let entry = state.entries.get(name)?;
        if !entry.catalog.resource_capability || !entry.allows_read(access) {
            return None;
        }
        entry.target(&state.source.config)
    }

    pub(crate) fn resource_view(
        &self,
        access: McpToolAccess,
        name: Option<&McpServerName>,
    ) -> Vec<(
        McpServerName,
        Vec<McpResourceInfo>,
        Vec<McpResourceTemplateInfo>,
    )> {
        let state = self.state.read().unwrap();
        state
            .entries
            .values()
            .filter(|entry| {
                name.is_none_or(|name| entry.server == *name)
                    && entry.catalog.resource_capability
                    && entry.allows_read(access)
                    && entry.connection.as_ref().is_some_and(Connection::live)
            })
            .map(|entry| {
                (
                    entry.server.clone(),
                    entry.catalog.resources.clone(),
                    entry.catalog.templates.clone(),
                )
            })
            .collect()
    }

    async fn reconnect(
        &self,
        global: &McpConfig,
        name: McpServerName,
        config: McpServerConfig,
        origin: McpServerOrigin,
    ) -> Entry {
        // Keep the old entry visible until replacement. If this operation is
        // cancelled, its stopped connection can still be retired by `sync`.
        let old = self
            .state
            .read()
            .unwrap()
            .entries
            .get(&name)
            .and_then(|entry| entry.connection.as_ref())
            .map(|conn| (conn.stop.clone(), conn.closed.clone()));
        close_connections(old, Duration::from_secs(5)).await;
        self.open(name, config, origin, global).await
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

async fn close_connections(
    conns: impl IntoIterator<Item = (CancellationToken, CancellationToken)>,
    grace: Duration,
) {
    let conns: Vec<_> = conns.into_iter().collect();
    for (stop, _) in &conns {
        stop.cancel();
    }
    let _ = timeout(
        grace,
        join_all(conns.iter().map(|(_, closed)| closed.cancelled())),
    )
    .await;
}

fn signal(changes: &watch::Sender<u64>) {
    changes.send_modify(|version| *version = version.wrapping_add(1));
}

fn supervise(
    service: Service,
    server: McpServerName,
    mut updates: mpsc::Receiver<()>,
    refresh: mpsc::Sender<()>,
    stop: CancellationToken,
    limit: Duration,
    changes: watch::Sender<u64>,
) -> Connection {
    let conn = Connection {
        peer: service.peer().clone(),
        stop: stop.clone(),
        _stop_on_drop: stop.clone().drop_guard(),
        closed: CancellationToken::new(),
        staged: Arc::new(Mutex::new(None)),
    };
    let (peer, staged, closed) = (conn.peer.clone(), conn.staged.clone(), conn.closed.clone());
    tokio::spawn(async move {
        let done = closed.drop_guard();
        let token = service.cancellation_token();
        let subscriptions = listen(&peer, refresh);
        let refresh_catalog = async {
            while updates.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                while updates.try_recv().is_ok() {}
                match Catalog::fetch(&server, &peer, limit, &stop).await {
                    Ok(catalog) => {
                        *staged.lock().unwrap() = Some(catalog);
                        signal(&changes);
                    }
                    Err(error) if !stop.is_cancelled() => {
                        tracing::warn!(target: "zevria::mcp", server = %server, %error, "could not refresh catalog; keeping last snapshot")
                    }
                    Err(_) => {}
                }
            }
        };
        let jobs = async {
            tokio::join!(subscriptions, refresh_catalog);
        };
        let waiting = service.waiting();
        tokio::pin!(waiting);
        let mut exited = false;
        tokio::select! {
            _ = &mut waiting => { exited = true; }
            _ = stop.cancelled() => {}
            _ = jobs => {}
        }
        if !exited {
            token.cancel();
            let _ = waiting.await;
        }
        drop(done);
        signal(&changes);
    });
    conn
}

async fn listen(peer: &Peer<RoleClient>, refresh: mpsc::Sender<()>) {
    let Some(info) = peer.peer_info() else {
        return;
    };
    if info.protocol_version < ProtocolVersion::V_2026_07_28 {
        return;
    }
    let mut filter = SubscriptionFilterBuilder::default();
    let mut enabled = false;
    if info
        .capabilities
        .tools
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.tools_list_changed();
        enabled = true;
    }
    if info
        .capabilities
        .resources
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.resources_list_changed();
        enabled = true;
    }
    if info
        .capabilities
        .prompts
        .as_ref()
        .is_some_and(|c| c.list_changed == Some(true))
    {
        filter = filter.prompts_list_changed();
        enabled = true;
    }
    if !enabled {
        return;
    }
    if let Ok(Ok(mut stream)) = timeout(Duration::from_secs(10), peer.listen(filter.build())).await
    {
        // Close the discovery-to-subscribe race with a fresh snapshot.
        let _ = refresh.try_send(());
        while let Ok(Some(_)) = stream.next().await {
            let _ = refresh.try_send(());
        }
    }
}

impl McpSessionService for McpManager {
    fn tool_allowed(&self, access: McpToolAccess, name: &str) -> bool {
        if access == McpToolAccess::None {
            return false;
        }
        let state = self.state.read().unwrap();
        if self.stop.is_cancelled() {
            return false;
        }
        if MCP_RESOURCE_TOOL_NAMES.contains(&name) {
            // The declarations change at a turn boundary, but dispatch still
            // requires at least one live, permitted resource server.
            return state
                .resource_access
                .iter()
                .any(|flags| flags.allows(access));
        }
        state
            .wire_index
            .get(name)
            .is_some_and(|flags| flags.allows(access))
    }

    fn active_instructions(&self, access: McpToolAccess) -> Vec<McpServerInstruction> {
        self.state
            .read()
            .unwrap()
            .entries
            .values()
            .filter(|entry| {
                // Only the applied snapshot contributes instructions. A dead
                // transport denies calls immediately but revokes at a boundary.
                entry.connection.is_some()
                    && (entry.allows_read(access)
                        || (access == McpToolAccess::PlanScoped
                            && entry.catalog.tools.iter().any(|(name, tool)| {
                                entry.config.plan_tool_allowed(name, read_only(tool))
                            })))
            })
            .filter_map(|entry| entry.catalog.instructions.clone())
            .collect()
    }

    fn prompt_catalog(&self) -> Vec<(McpServerName, McpPromptInfo)> {
        self.state
            .read()
            .unwrap()
            .entries
            .values()
            .filter(|entry| entry.connection.as_ref().is_some_and(Connection::live))
            .flat_map(|entry| {
                entry
                    .catalog
                    .prompts
                    .iter()
                    .map(|prompt| (entry.server.clone(), prompt.clone()))
            })
            .collect()
    }

    fn query_view(&self, request: &McpManagementRequest) -> McpManagementResult {
        let state = self.state.read().unwrap();
        let selected = match request {
            McpManagementRequest::Inspect { server } => match state.entries.get(server) {
                Some(entry) => Some(entry.detail()),
                None => {
                    return McpManagementResult::error(
                        "not_found",
                        format!("Unknown MCP server '{server}'."),
                    );
                }
            },
            McpManagementRequest::List => None,
            _ => {
                return McpManagementResult::error(
                    "not_query",
                    "This MCP action requires an idle session.",
                );
            }
        };
        let servers: Vec<_> = state.entries.values().map(Entry::summary).collect();
        let diagnostics = servers
            .iter()
            .filter_map(|server| match &server.status {
                McpServerStatus::Failed { error, .. } => {
                    Some(format!("MCP '{}': {error}", server.name))
                }
                _ => None,
            })
            .collect();
        McpManagementResult::View {
            view: McpManagementView {
                revision: state.revision(),
                enabled: state.source.config.enabled,
                servers,
                selected,
                diagnostics,
                prompts: state
                    .entries
                    .values()
                    .filter(|entry| entry.connection.as_ref().is_some_and(Connection::live))
                    .flat_map(|entry| {
                        entry
                            .catalog
                            .prompts
                            .iter()
                            .map(|prompt| (entry.server.clone(), prompt.clone()))
                    })
                    .collect(),
            },
        }
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn sync_pending_updates(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<McpSyncSummary>> + Send + '_>> {
        Box::pin(async {
            let _operation = self.mutations.lock().await;
            self.sync()
        })
    }

    fn manage(
        &self,
        request: McpManagementRequest,
    ) -> Pin<Box<dyn Future<Output = McpManagementResult> + Send + '_>> {
        Box::pin(async move {
            if request.is_query() {
                self.query_view(&request)
            } else {
                self.mutate(request).await
            }
        })
    }

    fn get_prompt(
        &self,
        access: McpToolAccess,
        server: McpServerName,
        prompt: String,
        arguments: BTreeMap<String, String>,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<UserPrompt>> + Send + '_>> {
        Box::pin(async move {
            let target = {
                let state = self.state.read().unwrap();
                let entry = state
                    .entries
                    .get(&server)
                    .ok_or_else(|| anyhow::anyhow!("unknown MCP server '{server}'"))?;
                anyhow::ensure!(
                    entry.allows_read(access),
                    "MCP prompts from '{server}' are not allowed in this mode"
                );
                let target = entry
                    .target(&state.source.config)
                    .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' is not connected"))?;
                let info = entry
                    .catalog
                    .prompts
                    .iter()
                    .find(|p| p.name == prompt)
                    .ok_or_else(|| anyhow::anyhow!("unknown MCP prompt '{server}:{prompt}'"))?;
                for arg in &info.arguments {
                    anyhow::ensure!(
                        !arg.required || arguments.contains_key(&arg.name),
                        "missing required MCP prompt argument '{}'",
                        arg.name
                    );
                }
                anyhow::ensure!(
                    arguments.len() <= 64
                        && arguments
                            .iter()
                            .map(|(k, v)| k.len() + v.len())
                            .sum::<usize>()
                            <= 256 * 1024,
                    "MCP prompt arguments exceed the size limit"
                );
                target
            };
            let mut params = GetPromptRequestParams::new(prompt.clone());
            if !arguments.is_empty() {
                params = params.with_arguments(
                    arguments
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect(),
                );
            }
            let response = transport::request(
                &target.peer,
                GetPromptRequest::new(params).into(),
                Instant::now() + target.timeout,
                &cancellation,
            )
            .await?;
            let ServerResult::GetPromptResult(result) = response else {
                anyhow::bail!("unexpected MCP prompts/get response");
            };
            anyhow::ensure!(!cancellation.is_cancelled(), "MCP prompt cancelled");
            if result
                .messages
                .iter()
                .all(|message| matches!(message.content, ContentBlock::Text(_)))
            {
                return get_prompt_result_to_user_prompt(result, &server, &prompt, target.limit);
            }
            let work = tokio::task::spawn_blocking(move || {
                get_prompt_result_to_user_prompt(result, &server, &prompt, target.limit)
            });
            tokio::select! { biased; _ = cancellation.cancelled() => anyhow::bail!("MCP prompt cancelled"), result = work => result? }
        })
    }
}
