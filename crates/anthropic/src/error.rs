//! Structured API error classification, shared by HTTP and SSE error events.
use serde_json::Value;
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Retryable(&'static str),
    InputTooLarge,
    ThinkingBindingMismatch,
    Terminal,
}
#[derive(Debug)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub retry_after: Option<Duration>,
    pub message: String,
}
impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            retry_after: None,
            message: message.into(),
        }
    }
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiError {}

pub fn classify(
    status: u16,
    body: &str,
    request_id: Option<&str>,
    retry_after: Option<Duration>,
) -> ApiError {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let message = value["error"]["message"].as_str().unwrap_or(body);
    let lower = message.to_ascii_lowercase();
    let category = value["error"]["type"].as_str().unwrap_or_default();
    let kind = if status == 413 || (status == 400 && lower.contains("prompt is too long")) {
        ErrorKind::InputTooLarge
    } else if status == 400 && lower.contains("bound to a different conversation") {
        ErrorKind::ThinkingBindingMismatch
    } else if status == 429 || category == "rate_limit_error" {
        ErrorKind::Retryable("rate_limit")
    } else if status == 529 || category == "overloaded_error" {
        ErrorKind::Retryable("overloaded")
    } else if status >= 500 || matches!(category, "api_error" | "timeout_error") {
        ErrorKind::Retryable("server_error")
    } else {
        ErrorKind::Terminal
    };
    let mut error = ApiError::new(
        kind,
        format!(
            "Anthropic Messages {status}: {message}{}",
            request_id
                .map(|id| format!(" (request-id: {id})"))
                .unwrap_or_default()
        ),
    );
    error.retry_after = if kind == ErrorKind::Retryable("rate_limit") {
        retry_after
    } else {
        None
    };
    if error
        .retry_after
        .is_some_and(|delay| delay > Duration::from_secs(60))
    {
        error.kind = ErrorKind::Terminal;
        error
            .message
            .push_str("; retry-after exceeds the 60s automatic retry limit");
    }
    error
}
