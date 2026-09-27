//! Question schemas, identities and outcomes; no execution channels.
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;
use uuid::Uuid;

/// The registered name of the interactive question tool.
pub use crate::tool_names::QUESTION_TOOL_NAME;

/// Correlation identity for one live question batch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestionRequestId(String);

impl QuestionRequestId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuestionRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One predefined answer shown beneath a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// The input control and validation supported by a provider-neutral prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionPromptKind {
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<usize>,
    },
    SingleSelect {
        allow_other: bool,
    },
    MultiSelect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_selections: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_selections: Option<usize>,
        allow_other: bool,
    },
}

/// A value returned by the shared question UI. `String` preserves the exact
/// native-tool wire shape; `Strings` is used only by multi-select
/// ACP form fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuestionAnswerValue {
    String(String),
    Strings(Vec<String>),
}

impl QuestionAnswerValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Strings(_) => None,
        }
    }

    pub fn as_strings(&self) -> Option<&[String]> {
        match self {
            Self::String(_) => None,
            Self::Strings(values) => Some(values),
        }
    }
}

impl From<String> for QuestionAnswerValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<Vec<String>> for QuestionAnswerValue {
    fn from(value: Vec<String>) -> Self {
        Self::Strings(value)
    }
}

/// One prompt within a batch. `id` is stable model-facing correlation data;
/// `header` is the terminal label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionPrompt {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub kind: QuestionPromptKind,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<QuestionAnswerValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuestionPromptWire {
    id: String,
    header: String,
    question: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    kind: QuestionPromptKind,
    #[serde(default = "default_true")]
    required: bool,
    #[serde(default)]
    default: Option<QuestionAnswerValue>,
}

impl<'de> Deserialize<'de> for QuestionPrompt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = QuestionPromptWire::deserialize(deserializer)?;
        let kind = wire.kind;
        match &kind {
            QuestionPromptKind::Text { .. } if !wire.options.is_empty() => {
                return Err(D::Error::custom(
                    "text question prompts cannot contain select options",
                ));
            }
            QuestionPromptKind::SingleSelect { .. } | QuestionPromptKind::MultiSelect { .. }
                if wire.options.is_empty() =>
            {
                return Err(D::Error::custom(
                    "select question prompts must contain at least one option",
                ));
            }
            QuestionPromptKind::Text { .. }
            | QuestionPromptKind::SingleSelect { .. }
            | QuestionPromptKind::MultiSelect { .. } => {}
        }
        Ok(Self {
            id: wire.id,
            header: wire.header,
            question: wire.question,
            options: wire.options,
            kind,
            required: wire.required,
            default: wire.default,
        })
    }
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

/// A live batch announced to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionRequest {
    pub id: QuestionRequestId,
    pub questions: Vec<QuestionPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub dismissible: bool,
}

/// One answer correlated by the model-provided question id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    pub id: String,
    /// `None` means that an optional field was explicitly skipped.
    pub answer: Option<QuestionAnswerValue>,
}

/// The frontend's terminal resolution. Both variants are successful tool
/// results: dismissal is information for the model, not a cancelled turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionResponse {
    Answered { answers: Vec<QuestionAnswer> },
    Dismissed,
}

/// Durable terminal meaning of a root `question` tool result. Invalid model
/// arguments intentionally produce no disposition because the question was
/// never validly attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionTerminalDisposition {
    Answered,
    Dismissed,
    Unavailable,
    InvalidFrontendResponse,
}

/// Why a registered question could not produce an ordinary response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionRequestError {
    AlreadyPending,
    FrontendUnavailable,
    ResponseChannelClosed,
    Cancelled,
}

impl fmt::Display for QuestionRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyPending => "another question batch is already waiting for the user",
            Self::FrontendUnavailable => {
                "the frontend stopped before it could display the question batch"
            }
            Self::ResponseChannelClosed => {
                "the question response channel closed before the user answered"
            }
            Self::Cancelled => "the parent turn was cancelled while waiting for the user",
        })
    }
}

impl std::error::Error for QuestionRequestError {}
