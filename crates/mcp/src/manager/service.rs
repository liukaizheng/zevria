//! Session-facing MCP queries, access checks, and prompt retrieval.

use std::{collections::BTreeMap, future::Future, pin::Pin};

use rmcp::model::{ContentBlock, GetPromptRequest, GetPromptRequestParams, ServerResult};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;
use zevria_content::UserPrompt;
use zevria_foundation::{
    MCP_RESOURCE_TOOL_NAMES, McpManagementRequest, McpManagementResult, McpManagementView,
    McpPromptInfo, McpResourceInfo, McpResourceTemplateInfo, McpServerInstruction, McpServerName,
    McpServerStatus, McpToolAccess,
};
use zevria_session_api::mcp::{McpSessionService, McpSyncSummary};

use super::{
    McpManager,
    connection::Connection,
    state::{Entry, Target},
};
use crate::{catalog::read_only, convert::get_prompt_result_to_user_prompt, transport};

impl McpManager {
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
                    && match access {
                        McpToolAccess::None => false,
                        McpToolAccess::AllEnabled => true,
                        McpToolAccess::PlanScoped | McpToolAccess::ReadOnlyPlanScoped => {
                            (entry.allows_read(access)
                                && (access == McpToolAccess::PlanScoped
                                    || entry.catalog.resource_capability))
                                || entry.catalog.tools.iter().any(|(name, tool)| {
                                    let read_only = read_only(tool);
                                    (access == McpToolAccess::PlanScoped || read_only)
                                        && entry.config.plan_tool_allowed(name, read_only)
                                })
                        }
                    }
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
