//! Readable and opaque reasoning blocks retained in Rig's native item.
//!
//! Rig's canonical reasoning is a single string; Zevria also needs the native
//! block boundaries for display and opaque-safe context accounting.

use rig_core::message::{AssistantContent, Reasoning};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReasoningContent {
    Text {
        text: String,
        signature: Option<String>,
    },
    Summary(String),
    Encrypted(String),
    Redacted {
        data: String,
    },
}

pub fn from_blocks(id: Option<String>, blocks: Vec<ReasoningContent>) -> Reasoning {
    let mut item = json!({"type":"reasoning", "summary":[], "content":[]});
    if let Some(id) = id {
        item["id"] = id.into();
    }
    let mut visible = Vec::new();
    let mut summary = Vec::new();
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ReasoningContent::Summary(text) => {
                visible.push(text.clone());
                summary.push(json!({"type":"summary_text", "text":text}));
            }
            ReasoningContent::Text { text, signature } => {
                visible.push(text.clone());
                let mut part = json!({"type":"reasoning_text", "text":text});
                if let Some(signature) = signature {
                    part["signature"] = signature.into();
                }
                content.push(part);
            }
            ReasoningContent::Encrypted(data) => item["encrypted_content"] = data.into(),
            ReasoningContent::Redacted { data } => {
                content.push(json!({"type":"redacted_thinking", "data":data}))
            }
        }
    }
    item["summary"] = summary.into();
    item["content"] = content.into();
    match AssistantContent::Reasoning(Reasoning::new(visible.join("\n\n"))).with_native(item) {
        AssistantContent::Reasoning(reasoning) => reasoning,
        _ => unreachable!("attaching native data preserves the block kind"),
    }
}

pub fn summaries(texts: Vec<String>) -> Reasoning {
    from_blocks(
        None,
        texts.into_iter().map(ReasoningContent::Summary).collect(),
    )
}

pub fn id(reasoning: &Reasoning) -> Option<&str> {
    reasoning.native.as_ref()?.item.get("id")?.as_str()
}

pub fn readable_parts(reasoning: &Reasoning) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    if let Some(native) = &reasoning.native {
        for field in ["summary", "content"] {
            for part in native
                .item
                .get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(text) = part
                    .as_str()
                    .or_else(|| part.get("text").and_then(Value::as_str))
                {
                    parts.push(text);
                }
            }
        }
    } else if !reasoning.redacted {
        parts.push(reasoning.text.as_str());
    }
    parts.into_iter().filter(|text| !text.trim().is_empty())
}

pub fn blocks(reasoning: &Reasoning) -> Vec<ReasoningContent> {
    let Some(native) = &reasoning.native else {
        return if reasoning.redacted {
            vec![ReasoningContent::Redacted {
                data: String::new(),
            }]
        } else {
            vec![ReasoningContent::Text {
                text: reasoning.text.clone(),
                signature: None,
            }]
        };
    };
    let mut blocks = Vec::new();
    for part in native
        .item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            blocks.push(ReasoningContent::Summary(text.into()));
        }
    }
    for part in native
        .item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if part.get("type").and_then(Value::as_str) == Some("redacted_thinking") {
            blocks.push(ReasoningContent::Redacted {
                data: part
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            });
        } else if let Some(text) = part
            .as_str()
            .or_else(|| part.get("text").and_then(Value::as_str))
        {
            blocks.push(ReasoningContent::Text {
                text: text.into(),
                signature: part
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    if let Some(data) = native.item.get("encrypted_content").and_then(Value::as_str) {
        blocks.push(ReasoningContent::Encrypted(data.into()));
    }
    blocks
}
