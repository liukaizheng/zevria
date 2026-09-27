//! Temporary content-free fingerprints. This encoding is diagnostic-only, not a wire format.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Fingerprint(pub(super) [u8; 32]);

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() != 64
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(serde::de::Error::custom("invalid fingerprint"));
        }
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

pub(super) fn bytes(value: &[u8]) -> Fingerprint {
    Fingerprint(Sha256::digest(value).into())
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Stream canonical JSON directly into the hash. Only key references are sorted;
// no copied history, strings, IDs, parsed argument strings, or opaque values.
fn canonical(value: &Value, out: &mut HashWriter) {
    match value {
        Value::Object(object) => {
            object_hash(object.iter().collect(), out);
        }
        Value::Array(array) => {
            out.0.update(b"[");
            for (index, item) in array.iter().enumerate() {
                if index != 0 {
                    out.0.update(b",");
                }
                canonical(item, out);
            }
            out.0.update(b"]");
        }
        _ => {
            serde_json::to_writer(out, value).expect("Value serialization to a hash is infallible")
        }
    }
}

fn object_hash(mut entries: Vec<(&String, &Value)>, out: &mut HashWriter) {
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    out.0.update(b"{");
    for (index, (key, value)) in entries.into_iter().enumerate() {
        if index != 0 {
            out.0.update(b",");
        }
        serde_json::to_writer(&mut *out, key)
            .expect("string serialization to a hash is infallible");
        out.0.update(b":");
        canonical(value, out);
    }
    out.0.update(b"}");
}

pub(super) fn fingerprint(value: &Value) -> Fingerprint {
    let mut out = HashWriter(Sha256::new());
    canonical(value, &mut out);
    Fingerprint(out.0.finalize().into())
}

pub(super) fn optional(value: Option<&Value>) -> Fingerprint {
    let mut out = HashWriter(Sha256::new());
    if let Some(value) = value {
        out.0.update(b"present:");
        canonical(value, &mut out);
    } else {
        out.0.update(b"absent");
    }
    Fingerprint(out.0.finalize().into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Properties {
    pub instructions: Fingerprint,
    pub tools: Fingerprint,
    pub remaining: Fingerprint,
}

impl Properties {
    pub fn new(properties: &Value) -> Self {
        let mut remaining = HashWriter(Sha256::new());
        if let Some(properties) = properties.as_object() {
            // Cache-sensitive comparison only. Exact serialized bytes and
            // preparation-to-wire verification intentionally do NOT use this.
            let options = properties
                .get("prompt_cache_options")
                .and_then(crate::prompt_cache::cache_sensitive_options);
            let mut entries: Vec<_> = properties
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "instructions" | "tools" | "prompt_cache_options"
                    )
                })
                .collect();
            if let Some(options) = options.as_deref() {
                let (key, _) = properties
                    .get_key_value("prompt_cache_options")
                    .expect("projected options have a key");
                entries.push((key, options));
            }
            object_hash(entries, &mut remaining);
        } else {
            canonical(properties, &mut remaining);
        }
        Self {
            instructions: optional(properties.get("instructions")),
            tools: optional(properties.get("tools")),
            remaining: Fingerprint(remaining.0.finalize().into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
    FunctionOutput,
    Search,
    Compaction,
    Other,
}
impl ItemKind {
    fn of(item: &Value) -> Self {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => Self::Message,
            None if item.get("role").is_some() => Self::Message,
            Some("reasoning") => Self::Reasoning,
            Some("function_call") => Self::FunctionCall,
            Some("function_call_output") => Self::FunctionOutput,
            Some("web_search_call") => Self::Search,
            Some("compaction") => Self::Compaction,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ItemFingerprint {
    pub hash: Fingerprint,
    pub kind: ItemKind,
}
impl ItemFingerprint {
    pub fn new(item: &Value) -> Self {
        Self {
            hash: fingerprint(item),
            kind: ItemKind::of(item),
        }
    }
}

// Ordered, length-delimited digest of item hashes, not a hash of copied wire JSON.
pub(super) fn ordered(items: &[ItemFingerprint]) -> Fingerprint {
    let mut hash = Sha256::new();
    hash.update(b"zevria-cache-diagnostics-input-v1");
    hash.update((items.len() as u64).to_le_bytes());
    for item in items {
        hash.update(item.hash.0);
    }
    Fingerprint(hash.finalize().into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Comparison {
    pub status: &'static str,
    pub baseline_count: Option<usize>,
    pub matched_count: usize,
    pub first_difference: Option<usize>,
    pub previous_kind: Option<ItemKind>,
    pub next_kind: Option<ItemKind>,
    pub instructions_changed: Option<bool>,
    pub tools_changed: Option<bool>,
    pub properties_changed: Option<bool>,
}

pub(super) fn compare(
    previous: Option<(&[ItemFingerprint], Properties)>,
    next: &[ItemFingerprint],
    properties: Properties,
) -> Comparison {
    let mut comparison = Comparison {
        status: "unknown",
        baseline_count: None,
        matched_count: 0,
        first_difference: None,
        previous_kind: None,
        next_kind: None,
        instructions_changed: None,
        tools_changed: None,
        properties_changed: None,
    };
    let Some((baseline, old_properties)) = previous else {
        return comparison;
    };
    comparison.baseline_count = Some(baseline.len());
    comparison.instructions_changed = Some(properties.instructions != old_properties.instructions);
    comparison.tools_changed = Some(properties.tools != old_properties.tools);
    comparison.properties_changed = Some(properties.remaining != old_properties.remaining);
    let matched = baseline
        .iter()
        .zip(next)
        .take_while(|(a, b)| a.hash == b.hash)
        .count();
    comparison.matched_count = matched;
    comparison.status = if matched == baseline.len() {
        if next.len() == baseline.len() {
            "exact_equal"
        } else {
            "exact_extension"
        }
    } else if matched == next.len() {
        "truncated"
    } else {
        "mismatch"
    };
    if matched < baseline.len() {
        comparison.first_difference = Some(matched);
        comparison.previous_kind = baseline.get(matched).map(|item| item.kind);
        comparison.next_kind = next.get(matched).map(|item| item.kind);
    }
    comparison
}
