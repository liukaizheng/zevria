//! Strict decode-only views of known Messages content blocks. The raw ledger
//! remains authoritative, including unknown fields and future block types.
use rig_core::message::{AssistantContent, CallId, Text, ToolCall, ToolFunction, ToolName};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use zevria_content::reasoning::{self, ReasoningContent};

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
        citations: Option<Vec<Value>>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: ToolName,
        input: Map<String, Value>,
    },
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = Value::deserialize(deserializer)?;
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known {
            Text {
                text: String,
                citations: Option<Vec<Value>>,
            },
            Thinking {
                thinking: String,
                signature: String,
            },
            RedactedThinking {
                data: String,
            },
            ToolUse {
                id: String,
                name: ToolName,
                input: Map<String, Value>,
            },
        }
        if !value.is_object() || value["type"].as_str().is_none() {
            return Err(D::Error::custom(
                "content block requires an object and type",
            ));
        }
        if !matches!(
            value["type"].as_str(),
            Some("text" | "thinking" | "redacted_thinking" | "tool_use")
        ) {
            return Ok(Self::Unknown(value));
        }
        Ok(
            match serde_json::from_value::<Known>(value).map_err(D::Error::custom)? {
                Known::Text { text, citations } => Self::Text { text, citations },
                Known::Thinking {
                    thinking,
                    signature,
                } => Self::Thinking {
                    thinking,
                    signature,
                },
                Known::RedactedThinking { data } => Self::RedactedThinking { data },
                Known::ToolUse { id, name, input } => {
                    if id.is_empty() {
                        return Err(D::Error::custom("tool_use requires a nonempty id"));
                    }
                    Self::ToolUse { id, name, input }
                }
            },
        )
    }
}

impl ContentBlock {
    pub fn canonical(self, native: Value) -> Option<AssistantContent> {
        Some(match self {
            Self::Text { text, .. } => AssistantContent::Text(Text::new(text)),
            Self::Thinking {
                thinking,
                signature,
            } => AssistantContent::Reasoning(reasoning::from_blocks(
                None,
                vec![ReasoningContent::Text {
                    text: thinking,
                    signature: Some(signature),
                }],
            )),
            Self::RedactedThinking { data } => AssistantContent::Reasoning(reasoning::from_blocks(
                None,
                vec![ReasoningContent::Redacted { data }],
            )),
            Self::ToolUse { id, name, input } => AssistantContent::ToolCall(ToolCall::new(
                CallId::from_wire(id),
                ToolFunction::new(name, Value::Object(input)),
            ))
            .with_native(native),
            Self::Unknown(_) => return None,
        })
    }
}
