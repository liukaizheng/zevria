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
        let value = Value::deserialize(deserializer)?;
        Ok(Self::decode_known(&value)
            .map_err(serde::de::Error::custom)?
            .unwrap_or(Self::Unknown(value)))
    }
}

enum BlockRef<'a> {
    Text(&'a str, Option<&'a Vec<Value>>),
    Thinking(&'a str, &'a str),
    Redacted(&'a str),
    ToolUse(&'a str, &'a str, &'a Map<String, Value>),
}

impl<'a> BlockRef<'a> {
    fn decode(value: &'a Value) -> serde_json::Result<Option<Self>> {
        use serde::de::Error as _;
        let string = |key: &str| {
            value[key].as_str().ok_or_else(|| {
                serde_json::Error::custom(format!("content block requires string {key}"))
            })
        };
        Ok(Some(match string("type")? {
            "text" => Self::Text(
                string("text")?,
                match &value["citations"] {
                    Value::Array(citations) => Some(citations),
                    Value::Null => None,
                    _ => return Err(serde_json::Error::custom("citations must be an array")),
                },
            ),
            "thinking" => Self::Thinking(string("thinking")?, string("signature")?),
            "redacted_thinking" => Self::Redacted(string("data")?),
            "tool_use" => {
                let id = string("id")?;
                let name = string("name")?;
                if id.is_empty() || name.is_empty() {
                    return Err(serde_json::Error::custom(
                        "tool_use requires a nonempty id and name",
                    ));
                }
                let input = value["input"]
                    .as_object()
                    .ok_or_else(|| serde_json::Error::custom("tool_use input must be an object"))?;
                Self::ToolUse(id, name, input)
            }
            _ => return Ok(None),
        }))
    }
}

impl ContentBlock {
    pub fn validate_known(value: &Value) -> serde_json::Result<()> {
        BlockRef::decode(value).map(drop)
    }

    /// Decode known fields directly from the ledger; unknown blocks stay there.
    pub fn decode_known(value: &Value) -> serde_json::Result<Option<Self>> {
        Ok(BlockRef::decode(value)?.map(|block| match block {
            BlockRef::Text(text, citations) => Self::Text {
                text: text.into(),
                citations: citations.cloned(),
            },
            BlockRef::Thinking(thinking, signature) => Self::Thinking {
                thinking: thinking.into(),
                signature: signature.into(),
            },
            BlockRef::Redacted(data) => Self::RedactedThinking { data: data.into() },
            BlockRef::ToolUse(id, name, input) => Self::ToolUse {
                id: id.into(),
                name: ToolName::new(name).expect("nonempty name"),
                input: input.clone(),
            },
        }))
    }

    pub fn canonical(self, native: &Value) -> Option<AssistantContent> {
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
            .with_native(native.clone()),
            Self::Unknown(_) => return None,
        })
    }
}
