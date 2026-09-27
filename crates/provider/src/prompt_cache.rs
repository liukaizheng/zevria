//! Request-only compatibility for an explicitly configured diagnostic comparison.
//! No IDs are inserted automatically and no ordinary request body is normalized.
use serde_json::Value;
use std::borrow::Cow;

const OPTIONS: &str = "prompt_cache_options";
const COMPARISON: &str = "comparison_response_id";

/// Project only the diagnostic leaf. Absence differs from a pre-existing empty
/// object; remove an empty object only when removing the leaf created it.
/// Borrow all ordinary values, including malformed/non-object options.
pub(crate) fn cache_sensitive_options(options: &Value) -> Option<Cow<'_, Value>> {
    let Some(object) = options.as_object().filter(|o| o.contains_key(COMPARISON)) else {
        return Some(Cow::Borrowed(options));
    };
    if object.len() == 1 {
        return None;
    }
    let mut projected = object.clone();
    projected.remove(COMPARISON);
    Some(Cow::Owned(Value::Object(projected)))
}

/// Continuation comparison only. Stored/prepared/wire properties remain exact.
pub(crate) fn compatible(previous: &Value, next: &Value) -> bool {
    if previous == next {
        return true;
    }
    let (Some(previous), Some(next)) = (previous.as_object(), next.as_object()) else {
        return false;
    };
    let previous_len = previous.len() - usize::from(previous.contains_key(OPTIONS));
    let next_len = next.len() - usize::from(next.contains_key(OPTIONS));
    previous_len == next_len
        && previous
            .iter()
            .all(|(key, value)| key == OPTIONS || next.get(key) == Some(value))
        && previous.get(OPTIONS).and_then(cache_sensitive_options)
            == next.get(OPTIONS).and_then(cache_sensitive_options)
}

/// Counting and compaction cannot use the completion-only comparison field.
/// Preserve all siblings and avoid touching the body if it was not configured.
pub(crate) fn remove_comparison(properties: &mut Value) {
    let Some(object) = properties.as_object_mut() else {
        return;
    };
    let Some(options) = object.get_mut(OPTIONS).and_then(Value::as_object_mut) else {
        return;
    };
    if options.remove(COMPARISON).is_some() && options.is_empty() {
        object.remove(OPTIONS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_comparison_is_excluded_and_empty_normalization_is_narrow() {
        let ordinary = json!({"model":"synthetic", "reasoning":{"effort":"high"}});
        let mut comparison = ordinary.clone();
        comparison[OPTIONS] = json!({COMPARISON:"resp_synthetic"});
        assert!(compatible(&ordinary, &comparison));
        comparison[OPTIONS][COMPARISON] = json!("resp_other");
        assert!(compatible(&ordinary, &comparison));
        let mut maintenance = comparison.clone();
        remove_comparison(&mut maintenance);
        assert_eq!(maintenance, ordinary);
        for options in [
            json!({}),
            Value::Null,
            json!([]),
            json!({"mode":"explicit"}),
            json!({"ttl":"30m"}),
        ] {
            let mut changed = ordinary.clone();
            changed[OPTIONS] = options;
            assert!(!compatible(&ordinary, &changed));
            let original = changed.clone();
            remove_comparison(&mut changed);
            assert_eq!(changed, original);
        }
        comparison[OPTIONS]["mode"] = json!("implicit");
        comparison[OPTIONS]["ttl"] = json!("30m");
        comparison[OPTIONS]["future"] = json!({"keep":[1,2]});
        let mut projected = comparison.clone();
        remove_comparison(&mut projected);
        assert!(compatible(&comparison, &projected));
        for (key, value) in [
            ("mode", json!("explicit")),
            ("ttl", Value::Null),
            ("future", json!({"keep":[2,1]})),
        ] {
            let mut changed = comparison.clone();
            changed[OPTIONS][key] = value;
            assert!(!compatible(&comparison, &changed));
        }
        for (key, value) in [
            ("model", json!("other")),
            ("reasoning", json!({"effort":"low"})),
            ("tools", json!([])),
            ("instructions", json!("changed")),
            ("prompt_cache_retention", json!("24h")),
            ("prompt_cache_key", json!("other")),
        ] {
            let mut changed = comparison.clone();
            changed[key] = value;
            assert!(!compatible(&comparison, &changed));
        }
        assert!(matches!(
            cache_sensitive_options(&json!({})),
            Some(Cow::Borrowed(_))
        ));
    }
}
