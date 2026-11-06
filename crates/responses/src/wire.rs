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
    pub output: Vec<Output>,
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
    #[serde(flatten)]
    pub kind: ResponseChunkKind,
    pub response: CompletionResponse,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
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

#[derive(Debug, Clone, Deserialize)]
pub struct ItemChunk {
    pub item_id: Option<String>,
    pub output_index: u64,
    #[serde(flatten)]
    pub data: ItemChunkKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ItemChunkKind {
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded(ItemOutput),
    #[serde(rename = "response.output_item.done")]
    OutputItemDone(ItemOutput),
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded(ContentPartChunk),
    #[serde(rename = "response.content_part.done")]
    ContentPartDone(ContentPartChunk),
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta(DeltaTextChunk),
    #[serde(rename = "response.output_text.done")]
    OutputTextDone(OutputTextChunk),
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta(DeltaTextChunk),
    #[serde(rename = "response.refusal.done")]
    RefusalDone(RefusalTextChunk),
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgsDelta(DeltaTextChunk),
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgsDone(ArgsTextChunk),
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded(SummaryPartChunk),
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone(SummaryPartChunk),
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta(SummaryTextChunk),
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone(SummaryTextDone),
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta(ReasoningTextDelta),
    #[serde(rename = "response.reasoning_text.done")]
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
