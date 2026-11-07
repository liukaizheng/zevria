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
    source: Option<anyhow::Error>,
}
impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            retry_after: None,
            message: message.into(),
            source: None,
        }
    }
    pub fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self {
            source: Some(source.into()),
            ..Self::new(kind, message)
        }
    }
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| error.as_ref())
    }
}

pub fn classify(
    status: u16,
    body: &str,
    request_id: Option<&str>,
    retry_after: Option<Duration>,
    should_retry: Option<bool>,
) -> ApiError {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let message = value["error"]["message"].as_str().unwrap_or(body);
    let lower = message.to_ascii_lowercase();
    let category = value["error"]["type"].as_str().unwrap_or_default();
    let code = value["error"]["code"].as_str().unwrap_or(category);
    let kind = if status == 413 || (status == 400 && lower.contains("prompt is too long")) {
        ErrorKind::InputTooLarge
    } else if should_retry == Some(false) {
        ErrorKind::Terminal
    } else if status == 400 && lower.contains("bound to a different conversation") {
        ErrorKind::ThinkingBindingMismatch
    } else if status == 408 {
        ErrorKind::Retryable("request_timeout_status")
    } else if status == 429 || category == "rate_limit_error" || code == "rate_limit_exceeded" {
        ErrorKind::Retryable("rate_limit")
    } else if status == 529 || category == "overloaded_error" {
        ErrorKind::Retryable("overloaded")
    } else if status >= 500
        || matches!(category, "api_error" | "timeout_error")
        || code == "server_error"
        || should_retry == Some(true)
    {
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
    error.retry_after = if matches!(kind, ErrorKind::Retryable(_)) {
        retry_after
    } else {
        None
    };
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_overrides_and_hints_preserve_size_precedence() {
        let delay = Some(Duration::from_secs(90));
        for status in [408, 429, 500, 503, 529] {
            let error = classify(
                status,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
                None,
                delay,
                None,
            );
            assert!(matches!(error.kind, ErrorKind::Retryable(_)), "{status}");
            assert_eq!(error.retry_after, delay, "no fixed 60s rule for {status}");
            let error = classify(status, "unavailable", None, delay, Some(false));
            assert_eq!(error.kind, ErrorKind::Terminal);
        }
        assert_eq!(
            classify(408, "slow", None, None, None).kind,
            ErrorKind::Retryable("request_timeout_status")
        );
        assert_eq!(
            classify(400, "bad", None, None, Some(true)).kind,
            ErrorKind::Retryable("server_error")
        );
        for (status, body) in [(413, "size"), (400, "prompt is too long")] {
            for retry in [Some(true), Some(false)] {
                assert_eq!(
                    classify(status, body, None, delay, retry).kind,
                    ErrorKind::InputTooLarge
                );
            }
        }
        for code in ["server_error", "rate_limit_exceeded"] {
            let body = serde_json::json!({"error":{"code":code,"message":"transient"}}).to_string();
            assert!(matches!(
                classify(0, &body, None, None, None).kind,
                ErrorKind::Retryable(_)
            ));
        }
    }
}
