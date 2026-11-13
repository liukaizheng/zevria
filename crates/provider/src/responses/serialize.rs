//! Borrowed Responses bodies, with the exact insertion/removal ordering of the
//! former serde_json::Map builders. Only the small property index is owned;
//! nested properties and projected input never need a serialization-time copy.
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;

use super::prepare::PreparedRequest;

#[derive(Serialize)]
#[serde(untagged)]
enum Entry<'a> {
    Property(&'a Value),
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
    use serde_json::json;

    #[test]
    fn borrowed_bodies_match_legacy_map_bytes_for_every_property_order() {
        fn check(order: &mut [&str], index: usize) {
            if index < order.len() {
                for next in index..order.len() {
                    order.swap(index, next);
                    check(order, index + 1);
                    order.swap(index, next);
                }
                return;
            }
            for length in 0..=order.len() {
                let properties = order[..length]
                    .iter()
                    .map(|key| ((*key).to_owned(), json!({"original":["🦀", 1.25, false]})))
                    .collect::<serde_json::Map<_, _>>();
                let input = vec![
                    json!({"role":"user","content":[{"type":"input_text","text":"borrow me"}]}),
                ];
                let prepared = PreparedRequest {
                    cache_diagnostics: crate::diagnostics::Diagnostics::empty_request(),
                    request_properties: Value::Object(properties.clone()),
                    full_input: std::sync::Arc::new(input.clone()),
                    input_count: 1,
                    input_digest: [0; 32],
                    input_matches_continuation: false,
                    history_message_count: 0,
                    replay_source: crate::replay::ReplaySource::None,
                };
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
        }
        check(
            &mut ["type", "input", "previous_response_id", "stream", "nested"],
            0,
        );
    }
}
