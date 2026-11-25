//! Bounded HTTP diagnostics and attempt failures shared by adapters.
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use std::{ops::Range, time::Duration};

const HTTP_ERROR_BODY_LIMIT: usize = 4 * 1024;
pub(crate) const URL_REDACTION: &str = "[URL removed]";

#[derive(Debug, Default)]
pub(crate) struct RetryHint {
    pub(crate) delay: Option<Duration>,
    pub(crate) should_retry: Option<bool>,
}

/// Delta-seconds only; HTTP-date Retry-After is deliberately ignored.
pub(crate) fn retry_hint(headers: &HeaderMap) -> RetryHint {
    let seconds = |name: &str, scale: f64| {
        let value = headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?
            / scale;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        Duration::try_from_secs_f64(value).ok()
    };
    RetryHint {
        delay: seconds("retry-after-ms", 1000.0).or_else(|| seconds("retry-after", 1.0)),
        should_retry: headers.get("x-should-retry").and_then(|value| {
            match value.to_str().ok()?.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
        }),
    }
}

pub(crate) fn with_http_response_request_id(
    error: anyhow::Error,
    upstream_request_id: &Option<String>,
    protocol: &str,
    header: &str,
) -> anyhow::Error {
    match upstream_request_id {
        Some(request_id) => {
            let detail =
                format!("{protocol} HTTP response carried {header} {request_id}: {error:#}");
            error.context(detail)
        }
        None => error,
    }
}

/// Return URL spans in the original text, in order and without overlaps.
pub(crate) fn diagnostic_url_ranges(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let delimiter = |ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>');
    let mut offset = 0;
    text.split_inclusive(delimiter).filter_map(move |part| {
        let start = offset;
        offset += part.len();
        let url_start = ["https://", "http://", "wss://", "ws://"]
            .into_iter()
            .filter_map(|scheme| part.find(scheme))
            .min()?;
        Some(start + url_start..start + part.trim_end_matches(delimiter).len())
    })
}

pub(crate) fn diagnostic_detail(text: &str) -> String {
    let text = zevria_content::image_diagnostics::text_copy(text);
    // Also redact URLs embedded in relay prose, beyond reqwest's without_url().
    let mut redacted = String::new();
    let mut start = 0;
    for range in diagnostic_url_ranges(&text) {
        redacted.push_str(&text[start..range.start]);
        redacted.push_str(URL_REDACTION);
        start = range.end;
    }
    redacted.push_str(&text[start..]);
    redacted.chars().take(HTTP_ERROR_BODY_LIMIT).collect()
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
                    return format!(
                        "<failed to read error body: {:#}>",
                        anyhow::Error::new(error.without_url())
                    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_hints_ignore_malformed_values_and_prefer_milliseconds() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "1.25".parse().unwrap());
        assert_eq!(
            retry_hint(&headers).delay,
            Some(Duration::from_millis(1250))
        );
        headers.insert("retry-after-ms", "42.5".parse().unwrap());
        headers.insert("x-should-retry", "false".parse().unwrap());
        let hint = retry_hint(&headers);
        assert_eq!(hint.delay, Some(Duration::from_micros(42500)));
        assert_eq!(hint.should_retry, Some(false));
        for invalid in ["NaN", "inf", "-1", "Sun, 06 Nov 1994 08:49:37 GMT", "1e999"] {
            headers.insert("retry-after-ms", invalid.parse().unwrap());
            headers.insert("retry-after", invalid.parse().unwrap());
            assert_eq!(retry_hint(&headers).delay, None);
        }
    }
}
