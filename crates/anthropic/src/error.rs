//! Structured API error classification, shared by HTTP and SSE error events.
use serde_json::Value;
use std::{fmt, time::Duration};
use zevria_model::RetryCategory;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Retryable(RetryCategory),
    InputTooLarge,
    RequestBodyTooLarge,
    ThinkingBindingMismatch,
    Terminal,
}
#[derive(Debug)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub retry_after: Option<Duration>,
    pub message: String,
    /// A structured token-window rejection before a completion was accepted.
    pub pre_execution: bool,
    source: Option<anyhow::Error>,
}
impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            retry_after: None,
            message: message.into(),
            pre_execution: false,
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
    let non_size = matches!(
        category,
        "authentication_error" | "permission_error" | "rate_limit_error"
    ) || matches!(
        code,
        "invalid_api_key" | "insufficient_quota" | "max_output_tokens" | "max_tokens"
    );
    let token_rejection = !non_size
        && (matches!(code, "context_length_exceeded" | "context_too_large")
            || (status == 400
                && matches!(category, "" | "invalid_request_error")
                && lower.starts_with("prompt is too long")));
    let kind = if token_rejection {
        ErrorKind::InputTooLarge
    } else if status == 413 {
        ErrorKind::RequestBodyTooLarge
    } else if should_retry == Some(false) {
        ErrorKind::Terminal
    } else if status == 400 && lower.contains("bound to a different conversation") {
        ErrorKind::ThinkingBindingMismatch
    } else if status == 408 {
        ErrorKind::Retryable(RetryCategory::RequestTimeoutStatus)
    } else if status == 429 || category == "rate_limit_error" || code == "rate_limit_exceeded" {
        ErrorKind::Retryable(RetryCategory::RateLimit)
    } else if status == 529 || category == "overloaded_error" {
        ErrorKind::Retryable(RetryCategory::Overloaded)
    } else if status >= 500
        || matches!(category, "api_error" | "timeout_error")
        || code == "server_error"
        || should_retry == Some(true)
    {
        ErrorKind::Retryable(RetryCategory::ServerError)
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
    error.pre_execution = kind == ErrorKind::InputTooLarge
        && status == 400
        && matches!(
            category,
            "" | "invalid_request_error" | "context_length_exceeded" | "context_too_large"
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
    fn token_window_evidence_distinguishes_http_rejection_from_stream_and_non_size_errors() {
        let body = r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"prompt is too long"}}"#;
        let http = classify(400, body, None, None, None);
        assert_eq!(http.kind, ErrorKind::InputTooLarge);
        assert!(http.pre_execution);
        let stream = classify(0, body, None, None, None);
        assert_eq!(stream.kind, ErrorKind::InputTooLarge);
        assert!(!stream.pre_execution);
        for category in [
            "authentication_error",
            "permission_error",
            "rate_limit_error",
        ] {
            let body =
                serde_json::json!({"error":{"type":category,"message":"prompt is too long"}})
                    .to_string();
            assert_ne!(
                classify(400, &body, None, None, None).kind,
                ErrorKind::InputTooLarge
            );
        }
        assert!(!classify(413, "body", None, None, None).pre_execution);
    }

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
            ErrorKind::Retryable(RetryCategory::RequestTimeoutStatus)
        );
        assert_eq!(
            classify(400, "bad", None, None, Some(true)).kind,
            ErrorKind::Retryable(RetryCategory::ServerError)
        );
        for (status, body) in [(413, "size"), (400, "prompt is too long")] {
            for retry in [Some(true), Some(false)] {
                assert_eq!(
                    classify(status, body, None, delay, retry).kind,
                    if status == 413 {
                        ErrorKind::RequestBodyTooLarge
                    } else {
                        ErrorKind::InputTooLarge
                    }
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
