//! Bounded HTTP diagnostics and attempt failures shared by adapters.
use futures_util::StreamExt;
use rig_core::error::ProviderError as CompletionError;

const HTTP_ERROR_BODY_LIMIT: usize = 4 * 1024;

pub(crate) enum HttpAttemptError {
    Retryable {
        category: &'static str,
        error: anyhow::Error,
        upstream_request_id: Option<String>,
    },
    Terminal {
        error: anyhow::Error,
        upstream_request_id: Option<String>,
    },
}

impl HttpAttemptError {
    pub(crate) fn retryable(category: &'static str, error: impl Into<anyhow::Error>) -> Self {
        Self::Retryable {
            category,
            error: error.into(),
            upstream_request_id: None,
        }
    }

    pub(crate) fn terminal(error: impl Into<anyhow::Error>) -> Self {
        Self::Terminal {
            error: error.into(),
            upstream_request_id: None,
        }
    }

    pub(crate) fn retryable_response(
        category: &'static str,
        error: impl Into<anyhow::Error>,
        upstream_request_id: &Option<String>,
    ) -> Self {
        Self::Retryable {
            category,
            error: with_http_response_request_id(
                error.into(),
                upstream_request_id,
                "OpenAI",
                "x-request-id",
            ),
            upstream_request_id: upstream_request_id.clone(),
        }
    }

    pub(crate) fn terminal_response(
        error: impl Into<anyhow::Error>,
        upstream_request_id: &Option<String>,
    ) -> Self {
        Self::Terminal {
            error: with_http_response_request_id(
                error.into(),
                upstream_request_id,
                "OpenAI",
                "x-request-id",
            ),
            upstream_request_id: upstream_request_id.clone(),
        }
    }

    pub(crate) fn from_http_status(
        category: Option<&'static str>,
        error: CompletionError,
        upstream_request_id: Option<String>,
    ) -> Self {
        match category {
            Some(category) => Self::Retryable {
                category,
                error: error.into(),
                upstream_request_id,
            },
            None => Self::Terminal {
                error: error.into(),
                upstream_request_id,
            },
        }
    }
}

pub(crate) fn with_http_response_request_id(
    error: anyhow::Error,
    upstream_request_id: &Option<String>,
    protocol: &str,
    header: &str,
) -> anyhow::Error {
    match upstream_request_id {
        Some(request_id) => error.context(format!(
            "{protocol} HTTP response carried {header} {request_id}"
        )),
        None => error,
    }
}

pub(crate) fn diagnostic_detail(text: &str) -> String {
    zevria_content::image_diagnostics::text_copy(text)
        .chars()
        .take(HTTP_ERROR_BODY_LIMIT)
        .collect()
}

pub(crate) async fn bounded_http_error_body(response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while bytes.len() < HTTP_ERROR_BODY_LIMIT {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let remaining = HTTP_ERROR_BODY_LIMIT - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Some(Err(error)) => {
                if bytes.is_empty() {
                    return format!("<failed to read error body: {error}>");
                }
                break;
            }
            None => break,
        }
    }
    let mut detail = String::from_utf8_lossy(&bytes).into_owned();
    if detail.len() > HTTP_ERROR_BODY_LIMIT {
        let mut end = HTTP_ERROR_BODY_LIMIT;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    diagnostic_detail(&detail)
}
