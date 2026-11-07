//! Versioned, length-delimited conversation binding. Object keys are sorted;
//! array order and every string byte (especially signatures) are significant.
use serde_json::Value;
use sha2::{Digest, Sha256};

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            serde_json::to_value(sorted).expect("JSON object")
        }
        Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
        value => value.clone(),
    }
}

pub fn without_cache_control(value: &Value) -> Value {
    fn unmark(block: &mut Value) {
        if let Some(object) = block.as_object_mut() {
            object.remove("cache_control");
        }
    }
    let mut value = value.clone();
    // Remove wire annotations, never user-owned keys inside a tool's input or
    // JSON schema. Such a key changing is a real prefix edit, not a cache move.
    match &mut value {
        Value::Array(blocks) => blocks.iter_mut().for_each(unmark),
        Value::Object(object) => {
            object.remove("cache_control");
            if let Some(Value::Array(blocks)) = object.get_mut("content") {
                blocks.iter_mut().for_each(unmark);
            }
        }
        _ => {}
    }
    canonical(&value)
}

#[derive(Clone)]
pub struct PrefixBinding(Sha256);
impl PrefixBinding {
    pub fn new(system: &Value, tools: &Value) -> Self {
        let mut this = Self(Sha256::new());
        this.bytes(b"zevria.anthropic.messages.prefix.v1");
        this.push(system);
        this.push(tools);
        this
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }
    pub fn push(&mut self, message: &Value) {
        self.bytes(&serde_json::to_vec(&without_cache_control(message)).expect("JSON value"));
    }
    pub fn finish(&self) -> String {
        use std::fmt::Write as _;
        self.0
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
