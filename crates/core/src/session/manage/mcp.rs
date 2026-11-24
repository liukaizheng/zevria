//! MCP uses a session-local adapter and the same gate at declaration and dispatch.
use tokio_util::sync::CancellationToken;
use zevria_foundation::{McpManagementRequest, McpManagementResult, TurnPolicy, is_mcp_tool_name};
use zevria_session_api::{ModelProvider, SessionEvent, SessionEventSender, mcp::McpSessionService};

use crate::session::SessionEngine;

pub(in crate::session) fn allows_tool(
    policy: &TurnPolicy,
    mcp: Option<&dyn McpSessionService>,
    name: &str,
) -> bool {
    if is_mcp_tool_name(name) {
        !policy.mcp_access.is_none()
            && mcp.is_some_and(|service| service.tool_allowed(policy.mcp_access, name))
    } else {
        policy.allows_tool(name)
    }
}

pub(in crate::session) fn query(
    mcp: Option<&dyn McpSessionService>,
    request: &McpManagementRequest,
) -> McpManagementResult {
    mcp.map_or_else(|| McpManagementResult::error("inactive", "No MCP servers are active in this session. Configure [mcp.servers] and start a new session."), |service| service.query_view(request))
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) async fn sync_mcp(&mut self, events: &SessionEventSender) {
        if let Some(mcp) = self.capabilities.mcp.clone()
            && let Some(summary) = mcp.sync_pending_updates().await
        {
            self.invalidate_mode_accounting();
            let _ = events
                .send(SessionEvent::McpChanged {
                    revision: summary.revision,
                    connected: summary.connected,
                    total: summary.total,
                    tool_count: summary.tool_count,
                })
                .await;
        }
    }

    pub(in crate::session) async fn manage_mcp(
        &mut self,
        request_id: String,
        request: McpManagementRequest,
        events: &SessionEventSender,
        cancellation: &CancellationToken,
    ) {
        let mcp = self.capabilities.mcp.clone();
        let result = if request.is_query() || mcp.is_none() {
            query(mcp.as_deref(), &request)
        } else if self.conversation.is_read_only()
            && matches!(request, McpManagementRequest::SetEnabled { .. })
        {
            McpManagementResult::error(
                "read_only",
                "MCP configuration changes require a writable session.",
            )
        } else if let Some(mcp) = mcp {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => McpManagementResult::error("cancelled", "MCP management cancelled."),
                result = mcp.manage(request) => result,
            }
        } else {
            unreachable!("checked MCP service")
        };
        if let McpManagementResult::Changed {
            revision,
            connected,
            total,
            tool_count,
            unchanged: false,
        } = &result
        {
            self.invalidate_mode_accounting();
            let _ = events
                .send(SessionEvent::McpChanged {
                    revision: revision.clone(),
                    connected: *connected,
                    total: *total,
                    tool_count: *tool_count,
                })
                .await;
        }
        let _ = events
            .send(SessionEvent::McpResult { request_id, result })
            .await;
    }
}
