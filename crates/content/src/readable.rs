//! Readable provider text, never native replay.
use serde_json::Value;

/// Remove terminal escape sequences and directional controls while preserving
/// readable line/word boundaries. Citation title sanitization remains separate.
pub fn sanitize_readable(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        let escape = match c {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\u{9d}' => Some(']'),
            _ => None,
        };
        if let Some(escape) = escape {
            match escape {
                '[' => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                ']' | 'P' | '^' | '_' => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if (c == '\n' || c == '\t' || !c.is_control())
            && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        {
            result.push(c);
        }
    }
    result
}

/// Readable native reasoning fields only; unknown/opaque object types remain
/// exclusively in replay even if they happen to contain a `text` property.
pub fn readable_reasoning_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| match value.get("type").and_then(Value::as_str) {
            Some("summary_text" | "reasoning_text" | "text") => {
                value.get("text").and_then(Value::as_str)
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_and_opaque_reasoning_never_become_readable_output() {
        assert_eq!(sanitize_readable("a\n\t"), "a\n\t");
        assert_eq!(
            sanitize_readable("before\u{1b}[31mred\u{1b}[0m\u{202e}\u{2066}after\u{7}"),
            "beforeredafter"
        );
        assert_eq!(
            sanitize_readable("a\u{1b}]title\u{7}b\u{1b}Psecret\u{1b}\\c"),
            "abc"
        );
        assert_eq!(sanitize_readable("a\u{9b}31mb\u{9d}secret\u{7}c"), "abc");
        assert!(
            readable_reasoning_value(&serde_json::json!({"type":"future","text":"opaque"}))
                .is_none()
        );
        assert_eq!(
            readable_reasoning_value(
                &serde_json::json!({"type":"reasoning_text","text":"readable"})
            ),
            Some("readable")
        );
    }
}
