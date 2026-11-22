//! Lifecycle boundary between the engine and a session's MCP adapter.
use std::{collections::BTreeMap, future::Future, pin::Pin};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zevria_content::UserPrompt;
use zevria_foundation::{
    McpManagementRequest, McpManagementResult, McpPromptInfo, McpServerInstruction, McpServerName,
    McpToolAccess,
};

pub trait McpSessionService: Send + Sync {
    fn tool_allowed(&self, access: McpToolAccess, name: &str) -> bool;
    fn active_instructions(&self, access: McpToolAccess) -> Vec<McpServerInstruction>;
    fn prompt_catalog(&self) -> Vec<(McpServerName, McpPromptInfo)>;
    fn query_view(&self, request: &McpManagementRequest) -> McpManagementResult;
    fn changes(&self) -> watch::Receiver<u64>;
    fn sync_pending_updates(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
    fn manage(
        &self,
        request: McpManagementRequest,
    ) -> Pin<Box<dyn Future<Output = McpManagementResult> + Send + '_>>;
    fn get_prompt(
        &self,
        access: McpToolAccess,
        server: McpServerName,
        prompt: String,
        arguments: BTreeMap<String, String>,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<UserPrompt>> + Send + '_>>;
}
