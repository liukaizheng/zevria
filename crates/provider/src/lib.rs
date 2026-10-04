//! Model provider adapters for Zevria.
//!
//! The crate includes an OpenAI Responses and Responses-compatible gateway
//! adapter over WebSocket and HTTP/SSE.

mod accumulator;
#[cfg(feature = "cache-diagnostics")]
mod cache_diagnostics;
mod config;
mod connection;
mod prompt_cache;
mod protocol;
use zevria_responses::replay;
mod recovery;
mod router;
mod search;
mod turn;

mod websocket_session;

#[cfg(test)]
use zevria_responses::SearchReturnTokenBudget;
use zevria_responses::WebSearchConfig;

fn lowercase_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(feature = "cache-diagnostics")]
pub use cache_diagnostics::CacheDiagnosticContext;
pub use config::{
    InputTokenCountConfig, LiteralApiKey, ModeAssignments, ModelAssignment, ModelConfig,
    ModelRouting, NetworkConfig, ProviderConfig, ProviderEndpoint, RemoteCompactionConfig,
    ResolvedModelProfile, ResponsesCompatibilityConfig,
};
pub use connection::OpenAiProvider;
pub use router::{ResponsesRouter, ResponsesRouterFactory};

#[cfg(test)]
use accumulator::*;
#[cfg(test)]
use connection::{
    OpenAiParkedWebSocket, OpenAiResponseEndpoints, OpenAiTransport, OpenAiWebSocketConfig,
    OpenAiWebSocketStream, resolve_input_token_count_url,
};
#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
#[cfg(test)]
use recovery::RecoveryPolicy;
#[cfg(test)]
use rig_core::client::CompletionClient;
#[cfg(test)]
use rig_core::message::{
    AdditionalParams, AssistantContent, Message, ReasoningContent, Text, ToolCall, ToolFunction,
};
#[cfg(test)]
use rig_reqwest::{
    client::DefaultTransportBuilder,
    openai_websocket::ResponsesWebSocketEvent,
    providers::openai::{self, ResponsesCompletionModel},
};
#[cfg(test)]
use tokio_tungstenite::{
    WebSocketStream, connect_async,
    tungstenite::{Message as WebSocketMessage, http::HeaderMap},
};
#[cfg(test)]
use turn::{
    is_websocket_disconnect, message_reports_upstream_disconnect, run_turn, run_turn_request,
    run_turn_request_with_reconnect, run_turn_request_with_recovery,
};
#[cfg(test)]
use websocket_session::{
    OpenAiWebSocketInbound, OpenAiWebSocketSession, OpenAiWebSocketTerminalCategory,
};
#[cfg(test)]
use zevria_responses::accumulator::*;
#[cfg(test)]
use zevria_responses::protocol::parse_server_event;
#[cfg(test)]
use zevria_session_api::ProgressReporter;

#[cfg(test)]
mod tests;
