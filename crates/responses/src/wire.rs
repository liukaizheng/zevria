//! Responses wire DTOs shared by the HTTP and WebSocket transports.
//!
//! These are protocol payloads, not Rig runtime events. Keeping this boundary
//! local lets the two transports use identical strict decoding and replay.
//! These DTOs are decode-only: faithful replay serializes the original JSON,
//! never a projection that may have discarded future provider fields.

use serde::Deserialize;
use serde_json::Value;
use zevria_model::openai::{Output, ResponseStatus};

#[derive(Debug, Clone)]
pub enum ResponsesWebSocketEvent {
    Response(Box<ResponseChunk>),
    Item(ItemChunk),
    Error(ErrorEvent),
    Done(DoneEvent),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorEvent {
    pub error: ErrorPayload,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorPayload {
    pub code: Option<String>,
    pub message: Option<String>,
}

impl std::fmt::Display for ErrorEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.error.code, &self.error.message) {
            (Some(code), Some(message)) => write!(f, "{code}: {message}"),
            (None, Some(message)) => f.write_str(message),
            (Some(code), None) => f.write_str(code),
            (None, None) => f.write_str("OpenAI websocket error"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DoneEvent {
    pub response: Value,
}

impl DoneEvent {
    pub fn response_id(&self) -> Option<&str> {
        self.response.get("id").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompletionResponse {
    pub id: String,
    pub status: ResponseStatus,
    pub usage: Option<ResponsesUsage>,
    pub error: Option<ResponseError>,
    pub incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IncompleteDetails {
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesUsage {
    pub input_tokens: u64,
    pub input_tokens_details: Option<InputTokensDetails>,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseChunk {
    #[serde(rename = "type")]
    pub kind: ResponseChunkKind,
    pub response: CompletionResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub enum ResponseChunkKind {
    #[serde(rename = "response.created")]
    ResponseCreated,
    #[serde(rename = "response.in_progress")]
    ResponseInProgress,
    #[serde(rename = "response.completed")]
    ResponseCompleted,
    #[serde(rename = "response.failed")]
    ResponseFailed,
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete,
}

#[derive(Debug, Clone)]
pub struct ItemChunk {
    pub item_id: Option<String>,
    pub output_index: u64,
    pub data: ItemChunkKind,
}

impl ItemChunk {
    pub(crate) fn decode(mut value: Value) -> serde_json::Result<Self> {
        // Move the routing header out, then decode just the matching body.
        // An internally tagged/flattened DTO would buffer the whole frame.
        let item_id = serde_json::from_value(value["item_id"].take())?;
        let output_index = serde_json::from_value(value["output_index"].take())?;
        let data = match value["type"].as_str() {
            Some("response.output_item.added") => {
                ItemChunkKind::OutputItemAdded(serde_json::from_value(value)?)
            }
            Some("response.output_item.done") => {
                ItemChunkKind::OutputItemDone(serde_json::from_value(value)?)
            }
            Some("response.content_part.added") => {
                ItemChunkKind::ContentPartAdded(serde_json::from_value(value)?)
            }
            Some("response.content_part.done") => {
                ItemChunkKind::ContentPartDone(serde_json::from_value(value)?)
            }
            Some("response.output_text.delta") => {
                ItemChunkKind::OutputTextDelta(serde_json::from_value(value)?)
            }
            Some("response.output_text.done") => {
                ItemChunkKind::OutputTextDone(serde_json::from_value(value)?)
            }
            Some("response.refusal.delta") => {
                ItemChunkKind::RefusalDelta(serde_json::from_value(value)?)
            }
            Some("response.refusal.done") => {
                ItemChunkKind::RefusalDone(serde_json::from_value(value)?)
            }
            Some("response.function_call_arguments.delta") => {
                ItemChunkKind::FunctionCallArgsDelta(serde_json::from_value(value)?)
            }
            Some("response.function_call_arguments.done") => {
                ItemChunkKind::FunctionCallArgsDone(serde_json::from_value(value)?)
            }
            Some("response.reasoning_summary_part.added") => {
                ItemChunkKind::ReasoningSummaryPartAdded(serde_json::from_value(value)?)
            }
            Some("response.reasoning_summary_part.done") => {
                ItemChunkKind::ReasoningSummaryPartDone(serde_json::from_value(value)?)
            }
            Some("response.reasoning_summary_text.delta") => {
                ItemChunkKind::ReasoningSummaryTextDelta(serde_json::from_value(value)?)
            }
            Some("response.reasoning_summary_text.done") => {
                ItemChunkKind::ReasoningSummaryTextDone(serde_json::from_value(value)?)
            }
            Some("response.reasoning_text.delta") => {
                ItemChunkKind::ReasoningTextDelta(serde_json::from_value(value)?)
            }
            Some("response.reasoning_text.done") => {
                ItemChunkKind::ReasoningTextDone(serde_json::from_value(value)?)
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "unknown Responses item event type",
                ));
            }
        };
        Ok(Self {
            item_id,
            output_index,
            data,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ItemChunkKind {
    OutputItemAdded(ItemOutput),
    OutputItemDone(ItemOutput),
    ContentPartAdded(ContentPartChunk),
    ContentPartDone(ContentPartChunk),
    OutputTextDelta(DeltaTextChunk),
    OutputTextDone(OutputTextChunk),
    RefusalDelta(DeltaTextChunk),
    RefusalDone(RefusalTextChunk),
    FunctionCallArgsDelta(DeltaTextChunk),
    FunctionCallArgsDone(ArgsTextChunk),
    ReasoningSummaryPartAdded(SummaryPartChunk),
    ReasoningSummaryPartDone(SummaryPartChunk),
    ReasoningSummaryTextDelta(SummaryTextChunk),
    ReasoningSummaryTextDone(SummaryTextDone),
    ReasoningTextDelta(ReasoningTextDelta),
    ReasoningTextDone(OutputTextChunk),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemOutput {
    pub sequence_number: u64,
    pub item: Output,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentPartChunk {
    pub content_index: u64,
    pub sequence_number: u64,
    pub part: ContentPartChunkPart,
}

#[derive(Debug, Clone)]
pub enum ContentPartChunkPart {
    OutputText { text: String },
    SummaryText { text: String },
    Unknown(Value),
}

impl<'de> Deserialize<'de> for ContentPartChunkPart {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let text = || {
            value
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| serde::de::Error::custom("content part requires text"))
        };
        match value.get("type").and_then(Value::as_str) {
            Some("output_text") => Ok(Self::OutputText { text: text()? }),
            Some("summary_text") => Ok(Self::SummaryText { text: text()? }),
            _ => Ok(Self::Unknown(value)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeltaTextChunk {
    #[serde(default)]
    pub content_index: u64,
    pub sequence_number: u64,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningTextDelta {
    pub content_index: Option<u64>,
    pub sequence_number: u64,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputTextChunk {
    pub content_index: u64,
    pub sequence_number: u64,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefusalTextChunk {
    pub content_index: u64,
    pub sequence_number: u64,
    pub refusal: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArgsTextChunk {
    pub sequence_number: u64,
    pub arguments: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummaryPartChunk {
    pub summary_index: u64,
    pub sequence_number: u64,
    pub part: SummaryPartChunkPart,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SummaryPartChunkPart {
    SummaryText { text: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummaryTextChunk {
    pub summary_index: u64,
    pub sequence_number: u64,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SummaryTextDone {
    pub summary_index: u64,
    pub sequence_number: u64,
    pub text: String,
}
