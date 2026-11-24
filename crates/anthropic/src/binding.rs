//! Versioned, length-delimited conversation binding. Object keys are sorted;
//! array order and every string byte (especially signatures) are significant.
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
enum Scope {
    Root,
    BlockArray,
    Block,
    Nested,
}

fn canonical(value: &Value, scope: Scope, out: &mut Vec<u8>) {
    match value {
        Value::Object(object) => {
            let unmark = matches!(scope, Scope::Root | Scope::Block);
            let mut fields = object
                .iter()
                .filter(|(key, _)| !unmark || *key != "cache_control")
                .map(|(key, value)| (key.as_str(), value))
                .collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| *key);
            out.push(b'{');
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).expect("JSON key");
                out.push(b':');
                let scope = if matches!(scope, Scope::Root) && key == "content" && value.is_array()
                {
                    Scope::BlockArray
                } else {
                    Scope::Nested
                };
                canonical(value, scope, out);
            }
            out.push(b'}');
        }
        Value::Array(array) => {
            let scope = if matches!(scope, Scope::Root | Scope::BlockArray) {
                Scope::Block
            } else {
                Scope::Nested
            };
            out.push(b'[');
            for (index, value) in array.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                canonical(value, scope, out);
            }
            out.push(b']');
        }
        value => serde_json::to_writer(out, value).expect("JSON scalar"),
    }
}

#[cfg(test)]
pub fn without_cache_control(value: &Value) -> Value {
    let mut bytes = Vec::new();
    canonical(value, Scope::Root, &mut bytes);
    serde_json::from_slice(&bytes).expect("canonical JSON")
}

pub struct PrefixBinding {
    hash: Sha256,
    scratch: Vec<u8>,
}

impl Clone for PrefixBinding {
    fn clone(&self) -> Self {
        Self {
            hash: self.hash.clone(),
            scratch: Vec::new(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.hash.clone_from(&source.hash);
    }
}

impl PrefixBinding {
    pub fn new(system: &Value, tools: &Value) -> Self {
        let mut this = Self {
            hash: Sha256::new(),
            scratch: Vec::new(),
        };
        this.bytes(b"zevria.anthropic.messages.prefix.v1");
        this.push(system);
        this.push(tools);
        this
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.hash.update((bytes.len() as u64).to_be_bytes());
        self.hash.update(bytes);
    }
    pub fn push(&mut self, message: &Value) {
        // Strip only wire annotations. Keys in tool input/schema and other
        // nested data remain part of the binding.
        self.scratch.clear();
        canonical(message, Scope::Root, &mut self.scratch);
        self.hash.update((self.scratch.len() as u64).to_be_bytes());
        self.hash.update(&self.scratch);
    }
    pub fn finish(&self) -> String {
        use std::fmt::Write as _;
        self.hash
            .clone()
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut out, b| {
                write!(out, "{b:02x}").expect("string write");
                out
            })
    }
}

pub fn request_binding(system: &Value, tools: &Value, messages: &[Value]) -> String {
    let mut binding = PrefixBinding::new(system, tools);
    for message in messages {
        binding.push(message);
    }
    binding.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_wire_bytes_match_the_v1_binding_format() {
        let system = json!([{"type":"text", "text":"ü\n\"", "cache_control":{}}]);
        let tools = json!([{"name":"tool", "input_schema":{"cache_control":true, "a":7.5}}]);
        let message = json!({"role":"assistant", "cache_control":{}, "content":[
            {"type":"tool_use", "cache_control":{}, "input":{"z":null,"cache_control":false}},
            {"type":"thinking", "signature":" sig\t "}
        ]});
        let mut expected = Sha256::new();
        for bytes in [
            &b"zevria.anthropic.messages.prefix.v1"[..],
            "[{\"text\":\"ü\\n\\\"\",\"type\":\"text\"}]".as_bytes(),
            br#"[{"input_schema":{"a":7.5,"cache_control":true},"name":"tool"}]"#,
            br#"{"content":[{"input":{"cache_control":false,"z":null},"type":"tool_use"},{"signature":" sig\t ","type":"thinking"}],"role":"assistant"}"#,
        ] {
            expected.update((bytes.len() as u64).to_be_bytes());
            expected.update(bytes);
        }
        let expected = expected
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(request_binding(&system, &tools, &[message]), expected);
    }
}
