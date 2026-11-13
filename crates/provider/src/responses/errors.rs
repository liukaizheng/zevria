//! Responses errors responsibilities.
use crate::http::{HttpAttemptError, diagnostic_detail};
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use zevria_model::ModelInputTooLarge;
use zevria_model::openai::ResponseStatus;
use zevria_responses::wire::CompletionResponse as OpenAiCompletionResponse;

use crate::websocket_session::OpenAiWebSocketTerminalCategory;

/// Marks a turn that failed because a websocket connection was lost rather
/// than because the request itself was rejected — either our own transport, or
/// an upstream failure a relay reported in-band. The adapter uses this to
/// decide whether rebuilding the socket and replaying the prompt is worth
/// attempting.
#[derive(Debug)]
pub(crate) struct WebSocketDisconnected {
    pub(super) context: &'static str,
    pub(super) source: Option<WebSocketError>,
    pub(super) server_reason: Option<String>,
    pub(super) terminal_category: &'static str,
}

impl WebSocketDisconnected {
    pub(super) fn new(
        context: &'static str,
        source: WebSocketError,
        terminal_category: &'static str,
    ) -> Self {
        Self {
            context,
            source: Some(source),
            server_reason: None,
            terminal_category,
        }
    }

    pub(super) fn without_source(context: &'static str, terminal_category: &'static str) -> Self {
        Self {
            context,
            source: None,
            server_reason: None,
            terminal_category,
        }
    }

    pub(super) fn from_server_reason(
        context: &'static str,
        server_reason: String,
        terminal_category: &'static str,
    ) -> Self {
        Self {
            context,
            source: None,
            server_reason: Some(diagnostic_detail(&server_reason)),
            terminal_category,
        }
    }

    pub(super) fn from_terminal(
        context: &'static str,
        terminal: OpenAiWebSocketTerminalCategory,
    ) -> Self {
        Self::without_source(context, terminal.as_str())
    }

    pub(super) fn terminal_category(&self) -> &'static str {
        self.terminal_category
    }
}

impl std::fmt::Display for WebSocketDisconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The OpenAI websocket connection was lost while {}",
            self.context
        )?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        if let Some(reason) = &self.server_reason {
            write!(f, ": {reason}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WebSocketDisconnected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Whether a failed turn was caused by a dropped websocket connection.
pub(crate) fn is_websocket_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<WebSocketDisconnected>().is_some()
}

/// Whether a server-sent failure message describes a dead transport rather
/// than a rejected request. Relays that proxy the Responses API report their
/// own upstream websocket dying in-band — for example gorilla/websocket's
/// "websocket: close 1006 (abnormal closure): unexpected EOF" — instead of
/// dropping our socket. Those turns are as recoverable as a local disconnect,
/// so they get the same reconnect-and-replay treatment. Anything else (quota,
/// context size, auth, ...) is a genuine rejection and must fail fast.
pub(crate) fn message_reports_upstream_disconnect(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "close 1006",
        "abnormal closure",
        "unexpected eof",
        "connection reset",
        "connection closed",
        "peer closed connection",
        "closed upstream",
        "requires http replay",
        "broken pipe",
    ]
    .iter()
    .any(|marker| message.contains(marker))
        || (message.contains("timed out")
            && ["upstream", "websocket", "connection"]
                .iter()
                .any(|marker| message.contains(marker)))
}

/// Whether a server error event says the chained `previous_response_id` no
/// longer exists server-side. OpenAI names the code
/// `previous_response_not_found`, but a relay may surface the same condition
/// under a generic code, so a clearly matching message is accepted too. A
/// false positive costs one full-history replay of a request that had
/// already failed — the same round trip a failed turn would force anyway.
pub(crate) fn error_reports_missing_previous_response(
    code: Option<&str>,
    message: Option<&str>,
) -> bool {
    if code == Some("previous_response_not_found") {
        return true;
    }
    let message = message.unwrap_or_default().to_ascii_lowercase();
    message.contains("previous response") || message.contains("no longer cached")
}

pub(super) fn error_reports_websocket_connection_limit(code: Option<&str>) -> bool {
    code == Some("websocket_connection_limit_reached")
}

/// Only known structured input-size codes are retry signals for core. In
/// particular output exhaustion, quota/auth failures and prose are not codes.
pub(super) fn input_size_code(code: Option<&str>) -> bool {
    matches!(code, Some("context_too_large" | "context_length_exceeded"))
}

pub(super) fn transient_response_code(code: Option<&str>) -> bool {
    matches!(code, Some("server_error" | "rate_limit_exceeded"))
}

#[derive(Debug)]
pub(super) struct TransientResponseError {
    pub(super) category: &'static str,
    pub(super) source: anyhow::Error,
}
impl std::fmt::Display for TransientResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}
impl std::error::Error for TransientResponseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(super) fn classify_response_failure(
    size: bool,
    transient: bool,
    error: anyhow::Error,
) -> anyhow::Error {
    if size {
        ModelInputTooLarge::new(error).into()
    } else if transient {
        TransientResponseError {
            category: "response_failed_transient",
            source: error,
        }
        .into()
    } else {
        error
    }
}

pub(super) fn response_attempt_error(
    error: anyhow::Error,
    request_id: &Option<String>,
) -> HttpAttemptError {
    if let Some(transient) = error.downcast_ref::<TransientResponseError>() {
        HttpAttemptError::retryable_response(transient.category, error, request_id)
    } else {
        HttpAttemptError::terminal_response(error, request_id)
    }
}

pub(super) fn classify_input_size(size_rejection: bool, error: anyhow::Error) -> anyhow::Error {
    if size_rejection {
        ModelInputTooLarge::new(error).into()
    } else {
        error
    }
}

pub(super) fn structured_input_size(value: &serde_json::Value) -> bool {
    input_size_code(
        value
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str),
    ) || input_size_code(
        value
            .pointer("/incomplete_details/reason")
            .and_then(serde_json::Value::as_str),
    )
}

pub(super) fn done_failure_error(
    response: &serde_json::Value,
    status: &ResponseStatus,
) -> anyhow::Error {
    let id = response
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing response ID>");
    let detail = response
        .get("error")
        .filter(|value| !value.is_null())
        .or_else(|| response.get("incomplete_details"));
    let detail = detail.map(|value| value.to_string()).unwrap_or_default();
    let detail = diagnostic_detail(&detail);
    classify_response_failure(
        matches!(status, ResponseStatus::Failed | ResponseStatus::Incomplete)
            && structured_input_size(response),
        matches!(status, ResponseStatus::Failed)
            && transient_response_code(
                response
                    .pointer("/error/code")
                    .and_then(serde_json::Value::as_str),
            ),
        anyhow::anyhow!("OpenAI response {id} ended with status {status:?}: {detail}"),
    )
}

pub(super) fn response_failure_error(response: &OpenAiCompletionResponse) -> anyhow::Error {
    let detail = response
        .error
        .as_ref()
        .map(|error| format!("{}: {}", error.code, error.message))
        .or_else(|| {
            response
                .incomplete_details
                .as_ref()
                .map(|details| details.reason.clone())
        })
        .unwrap_or_else(|| "no failure details were provided".to_string());

    let detail = diagnostic_detail(&detail);
    classify_response_failure(
        matches!(
            response.status,
            ResponseStatus::Failed | ResponseStatus::Incomplete
        ) && (input_size_code(response.error.as_ref().map(|error| error.code.as_str()))
            || input_size_code(
                response
                    .incomplete_details
                    .as_ref()
                    .map(|details| details.reason.as_str()),
            )),
        matches!(response.status, ResponseStatus::Failed)
            && transient_response_code(response.error.as_ref().map(|error| error.code.as_str())),
        anyhow::anyhow!(
            "OpenAI response {} ended with status {:?}: {detail}",
            response.id,
            response.status
        ),
    )
}

#[derive(Debug)]
pub(super) struct StaleResponseId;
impl std::fmt::Display for StaleResponseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("previous response is unavailable; replaying authoritative local input")
    }
}
impl std::error::Error for StaleResponseId {}
