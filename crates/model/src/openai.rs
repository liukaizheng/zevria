//! Strict, application-owned Responses output DTOs for native replay.
//!
//! Rig 0.46 decodes Responses as JSON. Keep validation of known wire shapes
//! here so malformed native items cannot enter the persisted replay ledger.
//! Output projections are decode-only; the original JSON remains authoritative
//! for persistence and native provider replay.

use rig_core::message::{AssistantContent as CanonicalContent, Text, ToolName};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Message(OutputMessage),
    FunctionCall(OutputFunctionCall),
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        content: Vec<String>,
        encrypted_content: Option<String>,
        status: Option<ToolStatus>,
    },
    Unknown(Value),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputMessage {
    pub id: String,
    pub role: OutputRole,
    pub status: ResponseStatus,
    pub content: Vec<AssistantContent>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRole {
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Completed,
    Failed,
    InProgress,
    Cancelled,
    Queued,
    Incomplete,
    #[serde(untagged)]
    Other(String),
}

/// Persisted provider setting; Rig's request-only option is serialize-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryLevel {
    Auto,
    Concise,
    Detailed,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    InProgress,
    Completed,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputFunctionCall {
    #[serde(default)]
    pub id: String,
    pub arguments: FunctionCallArguments,
    pub call_id: String,
    pub name: ToolName,
    pub status: ToolStatus,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct FunctionCallArguments(String);

impl FunctionCallArguments {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummary {
    SummaryText { text: String },
}

impl ReasoningSummary {
    pub fn text(&self) -> &str {
        let Self::SummaryText { text } = self;
        text
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    OutputText(OutputText),
    Refusal { refusal: String },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputText {
    pub text: String,
    #[serde(flatten, default)]
    pub extras: Map<String, Value>,
}

impl OutputText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            extras: Map::new(),
        }
    }
}

impl From<AssistantContent> for CanonicalContent {
    fn from(content: AssistantContent) -> Self {
        match content {
            AssistantContent::OutputText(text) => {
                // Empty provider carriers are not semantic metadata. Keep the
                // canonical text unchanged while the ledger retains raw extras.
                let extras: Map<String, Value> = text
                    .extras
                    .into_iter()
                    .filter(|(_, value)| {
                        !(value.is_null()
                            || value.as_array().is_some_and(Vec::is_empty)
                            || value.as_object().is_some_and(Map::is_empty))
                    })
                    .collect();
                Self::Text(if extras.is_empty() {
                    Text::new(text.text)
                } else {
                    zevria_content::citations::text_with_openai_extras(
                        text.text,
                        Value::Object(extras),
                    )
                })
            }
            AssistantContent::Refusal { refusal } => Self::text(refusal),
        }
    }
}

#[derive(Deserialize)]
struct ReasoningItem {
    id: String,
    #[serde(default)]
    summary: Vec<ReasoningSummary>,
    #[serde(default, deserialize_with = "reasoning_texts")]
    content: Vec<String>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    status: Option<ToolStatus>,
}

fn reasoning_texts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            Value::String(text) => Ok(text),
            Value::Object(mut object) => object
                .remove("text")
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| serde::de::Error::custom("reasoning content requires text")),
            _ => Err(serde::de::Error::custom("invalid reasoning content")),
        })
        .collect()
}

impl<'de> Deserialize<'de> for Output {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value.get("type").and_then(Value::as_str) {
            Some("message") => serde_json::from_value(value)
                .map(Self::Message)
                .map_err(serde::de::Error::custom),
            Some("function_call") => serde_json::from_value(value)
                .map(Self::FunctionCall)
                .map_err(serde::de::Error::custom),
            Some("reasoning") => {
                let item: ReasoningItem =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Reasoning {
                    id: item.id,
                    summary: item.summary,
                    content: item.content,
                    encrypted_content: item.encrypted_content,
                    status: item.status,
                })
            }
            _ => Ok(Self::Unknown(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_provider_text_carriers_do_not_change_the_canonical_message() {
        for empty in [Value::Null, json!([]), json!({})] {
            let output: AssistantContent = serde_json::from_value(json!({
                "type":"output_text", "text":"answer", "annotations":empty, "logprobs":[], "future_field":{},
            })).unwrap();
            assert_eq!(
                CanonicalContent::from(output),
                CanonicalContent::text("answer")
            );
        }
        let output: AssistantContent = serde_json::from_value(json!({
            "type":"output_text", "text":"answer", "future_field":{"kept":true},
        }))
        .unwrap();
        let CanonicalContent::Text(text) = output.into() else {
            panic!("text")
        };
        assert_eq!(
            text.native.unwrap().item["future_field"],
            json!({"kept":true})
        );
    }
}
