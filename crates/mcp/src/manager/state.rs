//! Applied catalog, read views, and dispatch authorization.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use rig_core::tool::ManagedToolToken;
use rmcp::{RoleClient, service::Peer};
use tokio_util::sync::CancellationToken;
use zevria_foundation::{
    MAX_MCP_INSTRUCTIONS_BYTES, McpManagementResult, McpServerDetail, McpServerName,
    McpServerOrigin, McpServerStatus, McpServerSummary, McpToolAccess, McpToolInfo, PlanMcpPolicy,
};
use zevria_session_api::mcp::McpSyncSummary;

use super::connection::{Connection, connection_live};
use crate::{
    catalog::{Catalog, read_only, wire_names},
    config::{McpConfig, McpConfigSource, McpServerConfig},
    convert::display_text,
    transport::StderrTail,
};

pub(super) struct State {
    pub(super) source: McpConfigSource,
    pub(super) entries: BTreeMap<McpServerName, Entry>,
    pub(super) managed: HashMap<String, ManagedToolToken>,
    pub(super) wire_index: HashMap<String, ToolPolicyFlags>,
    pub(super) resource_access: Vec<ToolPolicyFlags>,
    pub(super) generation: u64,
    // SetEnabled can change the live catalog before reaching its final commit.
    pub(super) sync_needed: bool,
}

pub(super) struct Entry {
    pub(super) server: McpServerName,
    pub(super) origin: McpServerOrigin,
    pub(super) config: McpServerConfig,
    pub(super) status: McpServerStatus,
    pub(super) stderr: StderrTail,
    pub(super) connect_ms: Option<u64>,
    pub(super) catalog: Catalog,
    pub(super) wire_names: BTreeMap<String, String>,
    pub(super) connection: Option<Connection>,
}

impl Connection {
    pub(super) fn live(&self) -> bool {
        connection_live(&self.stop, &self.closed, &self.peer)
    }

    fn policy_flags(&self, plan_allowed: bool, read_only: bool) -> ToolPolicyFlags {
        ToolPolicyFlags {
            plan_allowed,
            read_only,
            peer: self.peer.clone(),
            stop: self.stop.clone(),
            closed: self.closed.clone(),
        }
    }
}

pub(super) struct ToolPolicyFlags {
    plan_allowed: bool,
    read_only: bool,
    peer: Peer<RoleClient>,
    stop: CancellationToken,
    closed: CancellationToken,
}

impl ToolPolicyFlags {
    pub(super) fn allows(&self, access: McpToolAccess) -> bool {
        (access == McpToolAccess::AllEnabled
            || (access == McpToolAccess::PlanScoped && self.plan_allowed)
            || (access == McpToolAccess::ReadOnlyPlanScoped && self.plan_allowed && self.read_only))
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
    pub(super) fn target(&self, global: &McpConfig) -> Option<Target> {
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

    pub(super) fn status(&self) -> McpServerStatus {
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

    pub(super) fn summary(&self) -> McpServerSummary {
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

    pub(super) fn detail(&self) -> McpServerDetail {
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

    pub(super) fn allows_read(&self, access: McpToolAccess) -> bool {
        match access {
            McpToolAccess::None => false,
            McpToolAccess::PlanScoped | McpToolAccess::ReadOnlyPlanScoped => {
                self.config.plan_mode != PlanMcpPolicy::None
            }
            McpToolAccess::AllEnabled => true,
        }
    }

    pub(super) fn set_catalog(&mut self, catalog: Catalog) {
        self.wire_names = wire_names(&self.server, &catalog, &self.wire_names);
        self.catalog = catalog;
    }
}

impl State {
    /// Update authorization in the same write lock as the applied catalog.
    /// This only copies policy metadata and connection handles, not schemas.
    pub(super) fn rebuild_access_index(&mut self) {
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
                    .push(conn.policy_flags(entry.allows_read(McpToolAccess::PlanScoped), true));
            }
            for (name, wire) in &entry.wire_names {
                if entry.config.tool_enabled(name) {
                    let plan_allowed = entry
                        .config
                        .plan_tool_allowed(name, read_only(&entry.catalog.tools[name]));
                    self.wire_index.insert(
                        wire.clone(),
                        conn.policy_flags(plan_allowed, read_only(&entry.catalog.tools[name])),
                    );
                }
            }
        }
    }

    pub(super) fn revision(&self) -> String {
        format!("{}:{}", self.source.revision, self.generation)
    }

    pub(super) fn sync_summary(&self) -> McpSyncSummary {
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

    pub(super) fn changed(&self, unchanged: bool) -> McpManagementResult {
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
