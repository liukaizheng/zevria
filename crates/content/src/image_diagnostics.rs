//! Payload-free copies for diagnostics only. Never apply these helpers to model
//! input, canonical messages, accepted user data, hashes, or actual transport.

use serde_json::Value;

const OMITTED: &str = "[image bytes omitted from diagnostics]";

/// Redact known structured image locations, including ACP and Responses shapes.
pub fn redact_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let image = object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image" | "input_image"))
                || ["mimeType", "mime_type"].iter().any(|key| {
                    object
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|mime| mime.starts_with("image/"))
                });
            if image {
                for key in ["data", "image_url", "uri", "url"] {
                    if let Some(value) = object.get_mut(key) {
                        *value = Value::String(OMITTED.into());
                    }
                }
            }
            for value in object.values_mut() {
                redact_json(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json(value);
            }
        }
        Value::String(text) => {
            *text = redact_text(text);
        }
        _ => {}
    }
}

pub fn json_copy(mut value: Value) -> Value {
    redact_json(&mut value);
    value
}

/// Handles structured errors as well as truncated/prefixed request dumps. Long
/// opaque base64 runs are suppressed only in diagnostic copies; this heuristic
/// deliberately prefers losing an opaque diagnostic token to leaking a bitmap.
pub fn text_copy(text: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<Value>(text) {
        redact_json(&mut value);
        serde_json::to_string(&value).unwrap_or_else(|_| OMITTED.into())
    } else {
        redact_text(text)
    }
}

fn redact_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("data:image/") {
        result.push_str(&redact_base64(&rest[..index]));
        rest = &rest[index..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '<' | '>'))
            .unwrap_or(rest.len());
        result.push_str(OMITTED);
        rest = &rest[end..];
    }
    result.push_str(&redact_base64(rest));
    result
}

fn redact_base64(text: &str) -> String {
    text.split_inclusive(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '+' | '/' | '='))
        .map(|part| {
            let end = part
                .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '+' | '/' | '='))
                .unwrap_or(part.len());
            let token = &part[..end];
            if token.len() >= 512
                || (token.len() >= 16
                    && ["iVBORw0KGgo", "/9j/", "R0lGOD", "UklGR"]
                        .iter()
                        .any(|prefix| token.starts_with(prefix)))
            {
                format!("{OMITTED}{}", &part[end..])
            } else {
                part.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_copies_omit_structured_inline_and_truncated_image_data() {
        let image = crate::PromptImage::from_rgba(1, 1, &[0, 1, 2, 255]).unwrap();
        let data = image.base64();
        for text in [
            serde_json::json!({"type":"image", "mimeType":"image/png", "data":data}).to_string(),
            format!("gateway rejected data:image/png;base64,{data}: bad format"),
            format!("stderr: {{\"data\":\"{data}"),
            format!("stderr: {{\"data\":\"{}", "a".repeat(700)),
        ] {
            let redacted = text_copy(&text);
            assert!(!redacted.contains(&data));
            assert!(redacted.contains(OMITTED));
        }
        assert_eq!(
            text_copy("upstream failed: HTTP 403"),
            "upstream failed: HTTP 403"
        );
        let input = serde_json::json!({"error":{"message":"failed", "input":{"type":"input_image", "image_url":format!("data:image/png;base64,{data}")}}});
        let copy = json_copy(input.clone());
        assert_eq!(copy["error"]["message"], "failed");
        assert!(!copy.to_string().contains(&data));
        assert!(input.to_string().contains(&data));
    }
}
