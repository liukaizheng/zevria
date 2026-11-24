//! Borrowed Responses bodies, with the exact insertion/removal ordering of the
//! former serde_json::Map builders. Only the small property index is owned;
//! nested properties and projected input never need a serialization-time copy.
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use std::borrow::Cow;

use super::prepare::PreparedRequest;

#[derive(Serialize)]
#[serde(untagged)]
enum Entry<'a> {
    Property(&'a Value),
    Projected(Value),
    Input(&'a [Value]),
    Text(&'a str),
    Flag(bool),
}

pub(super) struct RequestBody<'a> {
    entries: Vec<(&'a str, Entry<'a>)>,
}

impl<'a> RequestBody<'a> {
    fn properties(prepared: &'a PreparedRequest) -> anyhow::Result<Self> {
        let properties = prepared.request_properties.as_object().ok_or_else(|| {
            anyhow::anyhow!("the prepared OpenAI request properties are not an object")
        })?;
        Ok(Self {
            entries: properties
                .iter()
                .map(|(key, value)| (key.as_str(), Entry::Property(value)))
                .collect(),
        })
    }

    fn insert(&mut self, key: &'a str, value: Entry<'a>) {
        if let Some((_, existing)) = self.entries.iter_mut().find(|(name, _)| *name == key) {
            *existing = value;
        } else {
            self.entries.push((key, value));
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(index) = self.entries.iter().position(|(name, _)| *name == key) {
            // serde_json's preserve_order Map::remove is swap_remove, not
            // shift_remove. Skipping removed properties would change bytes.
            self.entries.swap_remove(index);
        }
    }

    pub(super) fn websocket(
        prepared: &'a PreparedRequest,
        input: &'a [Value],
        previous_response_id: Option<&'a str>,
    ) -> anyhow::Result<Self> {
        let mut body = Self::properties(prepared)?;
        body.insert("type", Entry::Text("response.create"));
        body.insert("input", Entry::Input(input));
        if let Some(id) = previous_response_id {
            body.insert("previous_response_id", Entry::Text(id));
        }
        Ok(body)
    }

    pub(super) fn http(prepared: &'a PreparedRequest) -> anyhow::Result<Self> {
        let mut body = Self::properties(prepared)?;
        body.remove("type");
        body.remove("previous_response_id");
        body.insert("input", Entry::Input(&prepared.full_input));
        body.insert("stream", Entry::Flag(true));
        Ok(body)
    }

    fn maintenance(prepared: &'a PreparedRequest) -> anyhow::Result<Self> {
        let mut body = Self::properties(prepared)?;
        let key = crate::prompt_cache::OPTIONS;
        if let Some(options) = prepared.request_properties.get(key) {
            match crate::prompt_cache::cache_sensitive_options(options) {
                None => body.remove(key),
                Some(Cow::Owned(value)) => body.insert(key, Entry::Projected(value)),
                Some(Cow::Borrowed(_)) => {}
            }
        }
        Ok(body)
    }

    pub(super) fn count_tokens(prepared: &'a PreparedRequest) -> anyhow::Result<Self> {
        let mut body = Self::maintenance(prepared)?;
        // Preserve the exact removal sequence of the original wire map.
        for key in [
            "stream",
            "previous_response_id",
            "background",
            "include",
            "service_tier",
            "store",
            "prompt_cache_key",
            "parallel_tool_calls",
        ] {
            body.remove(key);
        }
        body.insert("input", Entry::Input(&prepared.full_input));
        Ok(body)
    }

    pub(super) fn compact(prepared: &'a PreparedRequest) -> anyhow::Result<Self> {
        let mut body = Self::maintenance(prepared)?;
        for key in ["stream", "previous_response_id", "include", "service_tier"] {
            body.remove(key);
        }
        if !body
            .entries
            .iter()
            .any(|(name, _)| *name == "parallel_tool_calls")
        {
            body.insert("parallel_tool_calls", Entry::Flag(true));
        }
        body.insert("input", Entry::Input(&prepared.full_input));
        Ok(body)
    }
}

impl Serialize for RequestBody<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn for_each_property_order(order: &mut [&str], index: usize, check: &mut impl FnMut(&[&str])) {
        if index == order.len() {
            check(order);
            return;
        }
        for next in index..order.len() {
            order.swap(index, next);
            for_each_property_order(order, index + 1, check);
            order.swap(index, next);
        }
    }

    fn prepare(properties: &Map<String, Value>, input: &[Value]) -> PreparedRequest {
        PreparedRequest {
            cache_diagnostics: crate::diagnostics::Diagnostics::empty_request(),
            request_properties: Value::Object(properties.clone()).into(),
            http_body: Default::default(),
            projection_generation: 0,
            full_input: std::sync::Arc::new(input.to_vec()),
            input_count: 1,
            input_digest: [0; 32],
            input_matches_continuation: false,
            history_message_count: 0,
            replay_source: crate::replay::ReplaySource::None,
        }
    }

    #[test]
    fn borrowed_bodies_match_legacy_map_bytes_for_every_property_order() {
        let mut check = |order: &[&str]| {
            for length in 0..=order.len() {
                let properties = order[..length]
                    .iter()
                    .map(|key| ((*key).to_owned(), json!({"original":["🦀", 1.25, false]})))
                    .collect::<serde_json::Map<_, _>>();
                let input = vec![
                    json!({"role":"user","content":[{"type":"input_text","text":"borrow me"}]}),
                ];
                let prepared = prepare(&properties, &input);
                for previous in [None, Some("response-id")] {
                    let mut legacy = properties.clone();
                    legacy.insert("type".into(), json!("response.create"));
                    legacy.insert("input".into(), Value::Array(input.clone()));
                    if let Some(id) = previous {
                        legacy.insert("previous_response_id".into(), json!(id));
                    }
                    assert_eq!(
                        serde_json::to_vec(
                            &RequestBody::websocket(&prepared, &input, previous).unwrap()
                        )
                        .unwrap(),
                        serde_json::to_vec(&legacy).unwrap(),
                        "WebSocket {order:?}, length={length}, previous={previous:?}"
                    );
                }
                let mut legacy = properties;
                legacy.remove("type");
                legacy.remove("previous_response_id");
                legacy.insert("input".into(), Value::Array(input.clone()));
                legacy.insert("stream".into(), json!(true));
                assert_eq!(
                    serde_json::to_vec(&RequestBody::http(&prepared).unwrap()).unwrap(),
                    serde_json::to_vec(&legacy).unwrap(),
                    "HTTP {order:?}, length={length}"
                );
            }
        };
        for_each_property_order(
            &mut ["type", "input", "previous_response_id", "stream", "nested"],
            0,
            &mut check,
        );
    }

    #[test]
    fn maintenance_bodies_match_legacy_map_bytes_for_every_property_order() {
        let input =
            vec![json!({"role":"user","content":[{"type":"input_text","text":"borrow me"}]})];
        let options = [
            json!({"comparison_response_id":"old"}),
            json!({"comparison_response_id":"old", "mode":"implicit", "ttl":"30m"}),
            json!({"mode":"implicit", "ttl":"30m"}),
            json!({}),
        ];
        let mut check = |order: &[&str]| {
            for length in 0..=order.len() {
                for options in &options {
                    // The other removals are always present so the permutation
                    // also exercises their swap_remove ordering.
                    let mut legacy = Map::from_iter([
                        ("model".into(), json!("test")),
                        ("background".into(), json!(true)),
                        ("include".into(), json!(["reasoning.encrypted_content"])),
                        ("nested".into(), json!({"original":["🦀", 1.25, false]})),
                        ("service_tier".into(), json!("auto")),
                        ("store".into(), json!(false)),
                        ("prompt_cache_key".into(), json!("session")),
                    ]);
                    for &key in &order[..length] {
                        legacy.insert(
                            key.into(),
                            match key {
                                "prompt_cache_options" => options.clone(),
                                "parallel_tool_calls" => json!(false),
                                _ => json!("original"),
                            },
                        );
                    }
                    let prepared = prepare(&legacy, &input);

                    // Independently apply the original owned-map maintenance builders.
                    if legacy
                        .get_mut("prompt_cache_options")
                        .and_then(Value::as_object_mut)
                        .is_some_and(|options| {
                            options.remove("comparison_response_id").is_some() && options.is_empty()
                        })
                    {
                        legacy.remove("prompt_cache_options");
                    }
                    let mut count = legacy.clone();
                    for key in [
                        "stream",
                        "previous_response_id",
                        "background",
                        "include",
                        "service_tier",
                        "store",
                        "prompt_cache_key",
                        "parallel_tool_calls",
                    ] {
                        count.remove(key);
                    }
                    count.insert("input".into(), Value::Array(input.clone()));
                    assert_eq!(
                        serde_json::to_vec(&RequestBody::count_tokens(&prepared).unwrap()).unwrap(),
                        serde_json::to_vec(&count).unwrap(),
                        "count_tokens {order:?}, length={length}, options={options}"
                    );

                    for key in ["stream", "previous_response_id", "include", "service_tier"] {
                        legacy.remove(key);
                    }
                    legacy.entry("parallel_tool_calls").or_insert(json!(true));
                    legacy.insert("input".into(), Value::Array(input.clone()));
                    assert_eq!(
                        serde_json::to_vec(&RequestBody::compact(&prepared).unwrap()).unwrap(),
                        serde_json::to_vec(&legacy).unwrap(),
                        "compact {order:?}, length={length}, options={options}"
                    );
                }
            }
        };
        for_each_property_order(
            &mut [
                "input",
                "previous_response_id",
                "stream",
                "prompt_cache_options",
                "parallel_tool_calls",
            ],
            0,
            &mut check,
        );
    }
}
