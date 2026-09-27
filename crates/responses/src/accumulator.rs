//! Streaming assistant-response accumulator.

use rig_core::{
    message::{
        AssistantContent, Message, Reasoning as MessageReasoning, ReasoningContent, Text, ToolCall,
        ToolFunction,
    },
    providers::openai::responses_api::{
        AssistantContent as OpenAiAssistantContent, CompletionResponse as OpenAiCompletionResponse,
        Output, OutputText, ReasoningSummary,
        streaming::{ContentPartChunkPart, ItemChunk, ItemChunkKind, SummaryPartChunkPart},
    },
};

use zevria_model::TokenUsage;

/// Raw output items captured before Rig normalizes them. The terminal output
/// array wins when present; otherwise completed item events are reconciled by
/// their provider output index.
#[derive(Default)]
pub struct NativeOutputLedger {
    indexed: Vec<Option<serde_json::Value>>,
    terminal: Option<Vec<serde_json::Value>>,
    malformed: bool,
}

impl NativeOutputLedger {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn record_output_item_done(&mut self, payload: &str) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
            self.malformed = true;
            return;
        };
        if event.get("type").and_then(serde_json::Value::as_str)
            != Some("response.output_item.done")
        {
            return;
        }
        let Some(output_index) = event
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        else {
            self.malformed = true;
            return;
        };
        let Some(item) = event.get("item").filter(|item| item.is_object()).cloned() else {
            self.malformed = true;
            return;
        };
        if output_index >= self.indexed.len() {
            let Some(required_len) = output_index.checked_add(1) else {
                self.malformed = true;
                return;
            };
            let additional = required_len - self.indexed.len();
            if self.indexed.try_reserve(additional).is_err() {
                self.malformed = true;
                return;
            }
            self.indexed.resize(required_len, None);
        }
        self.indexed[output_index] = Some(item);
    }

    pub fn record_terminal_output(&mut self, payload: &str) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(payload) else {
            self.malformed = true;
            return;
        };
        let output = event
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(serde_json::Value::as_array);
        if let Some(output) = output {
            if output.iter().all(serde_json::Value::is_object) {
                self.terminal = Some(output.clone());
            } else {
                self.malformed = true;
            }
        }
    }

    pub fn take_complete(&mut self) -> Option<Vec<serde_json::Value>> {
        let terminal = self.terminal.take();
        let malformed = std::mem::take(&mut self.malformed);
        let indexed = std::mem::take(&mut self.indexed);
        if let Some(output) = terminal {
            return (!output.is_empty()).then_some(output);
        }
        if malformed {
            return None;
        }
        if indexed.is_empty() || indexed.iter().any(Option::is_none) {
            return None;
        }
        Some(indexed.into_iter().flatten().collect())
    }
}

#[derive(Default)]
pub struct AssistantMessageAccumulator {
    outputs: Vec<Option<PendingOutput>>,
}

pub(crate) enum PendingOutput {
    Message(PendingMessage),
    Reasoning(PendingReasoning),
    ToolCall(PendingToolCall),
    Complete {
        message_id: Option<String>,
        content: Vec<AssistantContent>,
    },
}

pub(crate) struct PendingMessage {
    id: Option<String>,
    parts: Vec<Option<PendingMessagePart>>,
}

pub(crate) enum PendingMessagePart {
    Text(Text),
    Refusal(String),
}

pub(crate) struct PendingReasoning {
    id: Option<String>,
    /// Blocks in canonical replay order: summaries and reasoning texts by
    /// provider index, followed by the encrypted payload.
    blocks: Vec<PendingReasoningBlock>,
    /// Whether the item's terminal payload has been merged. Chunks arriving
    /// after it are dropped, as for any other completed output.
    finalized: bool,
}

/// One reasoning block plus the provider index that addresses it on the wire.
enum PendingReasoningBlock {
    Summary { index: u64, text: String },
    Text { index: u64, text: String },
    Encrypted { data: String },
}

/// The wire address of a reasoning block: which of the item's three fields it
/// belongs to, and where within that field. Variant declaration order is the
/// canonical projection order and must remain Summary < Text < Encrypted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReasoningBlockKey {
    Summary(u64),
    Text(u64),
    Encrypted,
}

pub(crate) struct PendingToolCall {
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<serde_json::Value>,
    argument_deltas: String,
    saw_argument_delta: bool,
}

impl AssistantMessageAccumulator {
    pub fn clear(&mut self) {
        self.outputs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.outputs.iter().all(Option::is_none)
    }

    pub fn record_item(&mut self, chunk: ItemChunk) -> anyhow::Result<()> {
        let ItemChunk {
            item_id,
            output_index,
            data,
        } = chunk;

        match data {
            ItemChunkKind::OutputItemAdded(item) => {
                self.record_output_added(output_index, item.item)?;
            }
            ItemChunkKind::OutputItemDone(item) => {
                self.record_output_done(output_index, item.item)?;
            }
            ItemChunkKind::ContentPartAdded(part) => match part.part {
                ContentPartChunkPart::OutputText { text } => {
                    let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                        return Ok(());
                    };
                    message.initialize_part(
                        part.content_index,
                        PendingMessagePart::Text(Text::new(text)),
                    )?;
                }
                ContentPartChunkPart::SummaryText { text } => {
                    let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())?
                    else {
                        return Ok(());
                    };
                    reasoning.initialize_summary(part.content_index, text);
                }
                ContentPartChunkPart::Unknown(_) => {}
            },
            ItemChunkKind::ContentPartDone(part) => match part.part {
                ContentPartChunkPart::OutputText { text } => {
                    let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                        return Ok(());
                    };
                    message.set_part(
                        part.content_index,
                        PendingMessagePart::Text(Text::new(text)),
                    )?;
                }
                ContentPartChunkPart::SummaryText { text } => {
                    let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())?
                    else {
                        return Ok(());
                    };
                    reasoning.set_summary(part.content_index, text);
                }
                ContentPartChunkPart::Unknown(_) => {}
            },
            ItemChunkKind::OutputTextDelta(delta) => {
                let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                message.append_text(delta.content_index, &delta.delta)?;
            }
            ItemChunkKind::OutputTextDone(done) => {
                let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                message.set_part(
                    done.content_index,
                    PendingMessagePart::Text(Text::new(done.text)),
                )?;
            }
            ItemChunkKind::RefusalDelta(delta) => {
                let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                message.append_refusal(delta.content_index, &delta.delta)?;
            }
            ItemChunkKind::RefusalDone(done) => {
                let Some(message) = self.message_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                message.set_part(
                    done.content_index,
                    PendingMessagePart::Refusal(done.refusal),
                )?;
            }
            ItemChunkKind::FunctionCallArgsDelta(delta) => {
                let Some(tool_call) = self.tool_call_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                tool_call.append_arguments(&delta.delta);
            }
            ItemChunkKind::FunctionCallArgsDone(done) => {
                let Some(tool_call) = self.tool_call_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                tool_call.set_arguments(normalize_tool_arguments(done.arguments));
            }
            ItemChunkKind::ReasoningSummaryPartAdded(part) => {
                let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                let SummaryPartChunkPart::SummaryText { text } = part.part;
                reasoning.initialize_summary(part.summary_index, text);
            }
            ItemChunkKind::ReasoningSummaryPartDone(part) => {
                let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                let SummaryPartChunkPart::SummaryText { text } = part.part;
                reasoning.set_summary(part.summary_index, text);
            }
            ItemChunkKind::ReasoningSummaryTextDelta(delta) => {
                let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                reasoning.append_summary(delta.summary_index, &delta.delta);
            }
            ItemChunkKind::ReasoningSummaryTextDone(done) => {
                let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                reasoning.set_summary(done.summary_index, done.text);
            }
            ItemChunkKind::ReasoningTextDelta(delta) => {
                let Some(reasoning) = self.reasoning_mut(output_index, item_id.as_deref())? else {
                    return Ok(());
                };
                reasoning.append_text(delta.content_index.unwrap_or(0), &delta.delta);
            }
            ItemChunkKind::ReasoningTextDone(_) => {}
        }

        Ok(())
    }

    pub(crate) fn streaming_outputs_mut(&mut self) -> &mut Vec<Option<PendingOutput>> {
        &mut self.outputs
    }

    pub(crate) fn record_output_added(
        &mut self,
        output_index: u64,
        output: Output,
    ) -> anyhow::Result<()> {
        let outputs = self.streaming_outputs_mut();
        let Some(output) = PendingOutput::from_openai_output(output) else {
            clear_indexed_slot(outputs, output_index);
            return Ok(());
        };
        *indexed_slot_mut(outputs, output_index, "output")? = Some(output);
        Ok(())
    }

    pub fn record_output_done(&mut self, output_index: u64, output: Output) -> anyhow::Result<()> {
        let outputs = self.streaming_outputs_mut();
        let slot = indexed_slot_mut(outputs, output_index, "output")?;
        match output {
            // A finished reasoning item overlays the streamed one so final
            // values retain canonical block order; a slot that held anything
            // else is replaced, like any other done item replaces its slot.
            Output::Reasoning {
                id,
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let matches_streamed_item = matches!(
                    slot,
                    Some(PendingOutput::Reasoning(reasoning))
                        if reasoning.id.as_deref().is_none_or(|current| current == id)
                );
                if !matches_streamed_item {
                    *slot = Some(PendingOutput::Reasoning(PendingReasoning::new(None)));
                }
                if let Some(PendingOutput::Reasoning(reasoning)) = slot {
                    reasoning.set_id(Some(id.as_str()))?;
                    reasoning.merge_item(summary, content, encrypted_content);
                    reasoning.finalized = true;
                }
            }
            output => {
                let (message_id, content) = assistant_content_from_openai_output(output);
                *slot = Some(PendingOutput::Complete {
                    message_id,
                    content,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn message_mut(
        &mut self,
        output_index: u64,
        item_id: Option<&str>,
    ) -> anyhow::Result<Option<&mut PendingMessage>> {
        let outputs = self.streaming_outputs_mut();
        let output = indexed_slot_mut(outputs, output_index, "output")?
            .get_or_insert_with(|| PendingOutput::Message(PendingMessage::new(item_id)));

        match output {
            PendingOutput::Message(message) => {
                message.set_id(item_id)?;
                Ok(Some(message))
            }
            PendingOutput::Complete { .. } => Ok(None),
            // A finalized reasoning item is a completed output, not a type
            // change: chunks arriving after its terminal payload are dropped.
            PendingOutput::Reasoning(reasoning) if reasoning.finalized => Ok(None),
            PendingOutput::Reasoning(_) | PendingOutput::ToolCall(_) => Err(anyhow::anyhow!(
                "OpenAI response stream output index {output_index} changed content type to message"
            )),
        }
    }

    pub(crate) fn reasoning_mut(
        &mut self,
        output_index: u64,
        item_id: Option<&str>,
    ) -> anyhow::Result<Option<&mut PendingReasoning>> {
        let outputs = self.streaming_outputs_mut();
        let output = indexed_slot_mut(outputs, output_index, "output")?
            .get_or_insert_with(|| PendingOutput::Reasoning(PendingReasoning::new(item_id)));

        match output {
            PendingOutput::Reasoning(reasoning) => {
                // Chunks after the item's terminal payload are dropped, as
                // for any other completed output.
                if reasoning.finalized {
                    return Ok(None);
                }
                reasoning.set_id(item_id)?;
                Ok(Some(reasoning))
            }
            PendingOutput::Complete { .. } => Ok(None),
            PendingOutput::Message(_) | PendingOutput::ToolCall(_) => Err(anyhow::anyhow!(
                "OpenAI response stream output index {output_index} changed content type to reasoning"
            )),
        }
    }

    pub(crate) fn tool_call_mut(
        &mut self,
        output_index: u64,
        item_id: Option<&str>,
    ) -> anyhow::Result<Option<&mut PendingToolCall>> {
        let outputs = self.streaming_outputs_mut();
        let output = indexed_slot_mut(outputs, output_index, "output")?
            .get_or_insert_with(|| PendingOutput::ToolCall(PendingToolCall::new(item_id)));

        match output {
            PendingOutput::ToolCall(tool_call) => {
                tool_call.set_id(item_id)?;
                Ok(Some(tool_call))
            }
            PendingOutput::Complete { .. } => Ok(None),
            // A finalized reasoning item is a completed output, not a type
            // change: chunks arriving after its terminal payload are dropped.
            PendingOutput::Reasoning(reasoning) if reasoning.finalized => Ok(None),
            PendingOutput::Message(_) | PendingOutput::Reasoning(_) => Err(anyhow::anyhow!(
                "OpenAI response stream output index {output_index} changed content type to tool call"
            )),
        }
    }

    pub fn assistant_message(&self) -> anyhow::Result<Message> {
        let outputs = &self.outputs;

        let message_id = outputs.iter().flatten().find_map(PendingOutput::message_id);
        let mut content = Vec::new();
        for output in outputs.iter().flatten() {
            content.extend(output.assistant_content()?);
        }

        if content.is_empty() {
            anyhow::bail!("OpenAI response stream preview contained no assistant content");
        }

        Ok(Message::Assistant {
            id: message_id,
            content,
        })
    }
}

impl PendingOutput {
    pub(crate) fn from_openai_output(output: Output) -> Option<Self> {
        match output {
            Output::Message(message) => Some(Self::Message(PendingMessage {
                id: Some(message.id),
                parts: message
                    .content
                    .into_iter()
                    .map(|content| {
                        Some(match content {
                            OpenAiAssistantContent::OutputText(text) => {
                                PendingMessagePart::Text(normalize_openai_text(text))
                            }
                            OpenAiAssistantContent::Refusal { refusal } => {
                                PendingMessagePart::Refusal(refusal)
                            }
                        })
                    })
                    .collect(),
            })),
            Output::Reasoning {
                id,
                summary,
                content,
                encrypted_content,
                ..
            } => Some(Self::Reasoning(PendingReasoning::from_item(
                Some(id.as_str()),
                summary,
                content,
                encrypted_content,
            ))),
            Output::FunctionCall(function) => Some(Self::ToolCall(PendingToolCall {
                id: Some(function.id),
                call_id: Some(function.call_id),
                name: Some(function.name),
                arguments: Some(normalize_tool_arguments(serde_json::Value::String(
                    function.arguments.as_str().to_string(),
                ))),
                argument_deltas: String::new(),
                saw_argument_delta: false,
            })),
            Output::Unknown(_) => None,
        }
    }

    pub(crate) fn message_id(&self) -> Option<String> {
        match self {
            Self::Message(message) => message.id.clone(),
            Self::Complete { message_id, .. } => message_id.clone(),
            Self::Reasoning(_) | Self::ToolCall(_) => None,
        }
    }

    pub(crate) fn assistant_content(&self) -> anyhow::Result<Vec<AssistantContent>> {
        match self {
            Self::Message(message) => Ok(message.assistant_content()),
            Self::Reasoning(reasoning) => Ok(vec![reasoning.assistant_content()]),
            Self::ToolCall(tool_call) => Ok(vec![tool_call.assistant_content()?]),
            Self::Complete { content, .. } => Ok(content.clone()),
        }
    }
}

impl PendingMessage {
    pub(crate) fn new(id: Option<&str>) -> Self {
        Self {
            id: id.map(ToOwned::to_owned),
            parts: Vec::new(),
        }
    }

    pub(crate) fn set_id(&mut self, id: Option<&str>) -> anyhow::Result<()> {
        set_consistent_id(&mut self.id, id, "message")
    }

    pub(crate) fn initialize_part(
        &mut self,
        content_index: u64,
        part: PendingMessagePart,
    ) -> anyhow::Result<()> {
        let slot = indexed_slot_mut(&mut self.parts, content_index, "content")?;
        if let Some(existing) = slot.as_mut() {
            existing.ensure_same_kind(&part)?;
        } else {
            *slot = Some(part);
        }
        Ok(())
    }

    pub(crate) fn set_part(
        &mut self,
        content_index: u64,
        part: PendingMessagePart,
    ) -> anyhow::Result<()> {
        let slot = indexed_slot_mut(&mut self.parts, content_index, "content")?;
        if let Some(existing) = slot.as_ref() {
            existing.ensure_same_kind(&part)?;
        }
        *slot = Some(part);
        Ok(())
    }

    pub(crate) fn append_text(&mut self, content_index: u64, delta: &str) -> anyhow::Result<()> {
        let part = indexed_slot_mut(&mut self.parts, content_index, "content")?
            .get_or_insert_with(|| PendingMessagePart::Text(Text::new("")));
        match part {
            PendingMessagePart::Text(text) => {
                text.text.push_str(delta);
                Ok(())
            }
            PendingMessagePart::Refusal(_) => Err(anyhow::anyhow!(
                "OpenAI response stream content index {content_index} changed from refusal to text"
            )),
        }
    }

    pub(crate) fn append_refusal(&mut self, content_index: u64, delta: &str) -> anyhow::Result<()> {
        let part = indexed_slot_mut(&mut self.parts, content_index, "content")?
            .get_or_insert_with(|| PendingMessagePart::Refusal(String::new()));
        match part {
            PendingMessagePart::Refusal(refusal) => {
                refusal.push_str(delta);
                Ok(())
            }
            PendingMessagePart::Text(_) => Err(anyhow::anyhow!(
                "OpenAI response stream content index {content_index} changed from text to refusal"
            )),
        }
    }

    pub(crate) fn assistant_content(&self) -> Vec<AssistantContent> {
        self.parts
            .iter()
            .flatten()
            .map(|part| match part {
                PendingMessagePart::Text(text) => AssistantContent::Text(text.clone()),
                PendingMessagePart::Refusal(refusal) => AssistantContent::text(refusal.clone()),
            })
            .collect()
    }
}

impl PendingMessagePart {
    pub(crate) fn ensure_same_kind(&self, other: &Self) -> anyhow::Result<()> {
        if matches!(
            (self, other),
            (Self::Text(_), Self::Text(_)) | (Self::Refusal(_), Self::Refusal(_))
        ) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "OpenAI response stream content part changed between text and refusal"
            ))
        }
    }
}

impl PendingReasoning {
    pub(crate) fn new(id: Option<&str>) -> Self {
        Self {
            id: id.map(ToOwned::to_owned),
            blocks: Vec::new(),
            finalized: false,
        }
    }

    /// A wire item flattened on its own in canonical replay order:
    /// summaries, reasoning texts, then the encrypted payload.
    pub(crate) fn from_item(
        id: Option<&str>,
        summary: Vec<ReasoningSummary>,
        content: Vec<String>,
        encrypted_content: Option<String>,
    ) -> Self {
        let mut reasoning = Self::new(id);
        reasoning.merge_item(summary, content, encrypted_content);
        reasoning
    }

    pub(crate) fn set_id(&mut self, id: Option<&str>) -> anyhow::Result<()> {
        set_consistent_id(&mut self.id, id, "reasoning")
    }

    pub(crate) fn initialize_summary(&mut self, summary_index: u64, text: String) {
        let key = ReasoningBlockKey::Summary(summary_index);
        if !self.contains(key) {
            *self.block_mut(key) = text;
        }
    }

    pub(crate) fn set_summary(&mut self, summary_index: u64, text: String) {
        *self.block_mut(ReasoningBlockKey::Summary(summary_index)) = text;
    }

    pub(crate) fn append_summary(&mut self, summary_index: u64, delta: &str) {
        self.block_mut(ReasoningBlockKey::Summary(summary_index))
            .push_str(delta);
    }

    pub(crate) fn append_text(&mut self, content_index: u64, delta: &str) {
        self.block_mut(ReasoningBlockKey::Text(content_index))
            .push_str(delta);
    }

    /// Overlay a full wire item onto the accumulated blocks. The item decides
    /// which blocks exist and what they say; canonical key order decides where
    /// every block sits, including blocks the stream never showed.
    pub(crate) fn merge_item(
        &mut self,
        summary: Vec<ReasoningSummary>,
        content: Vec<String>,
        encrypted_content: Option<String>,
    ) {
        let summaries = summary.len() as u64;
        let texts = content.len() as u64;
        self.blocks.retain(|block| match block.key() {
            ReasoningBlockKey::Summary(index) => index < summaries,
            ReasoningBlockKey::Text(index) => index < texts,
            ReasoningBlockKey::Encrypted => encrypted_content.is_some(),
        });

        for (index, summary) in summary.iter().enumerate() {
            *self.block_mut(ReasoningBlockKey::Summary(index as u64)) = summary.text().to_owned();
        }
        for (index, text) in content.into_iter().enumerate() {
            *self.block_mut(ReasoningBlockKey::Text(index as u64)) = text;
        }
        if let Some(encrypted_content) = encrypted_content {
            *self.block_mut(ReasoningBlockKey::Encrypted) = encrypted_content;
        }
    }

    fn contains(&self, key: ReasoningBlockKey) -> bool {
        self.blocks.iter().any(|block| block.key() == key)
    }

    /// The block's value, creating the block in canonical replay order.
    fn block_mut(&mut self, key: ReasoningBlockKey) -> &mut String {
        let position = match self.blocks.iter().position(|block| block.key() == key) {
            Some(position) => position,
            None => {
                let position = self
                    .blocks
                    .iter()
                    .position(|block| key < block.key())
                    .unwrap_or(self.blocks.len());
                self.blocks
                    .insert(position, PendingReasoningBlock::new(key));
                position
            }
        };
        self.blocks[position].value_mut()
    }

    pub(crate) fn assistant_content(&self) -> AssistantContent {
        let content = self
            .blocks
            .iter()
            .map(|block| match block {
                PendingReasoningBlock::Summary { text, .. } => {
                    ReasoningContent::Summary(text.clone())
                }
                PendingReasoningBlock::Text { text, .. } => ReasoningContent::Text {
                    text: text.clone(),
                    signature: None,
                },
                PendingReasoningBlock::Encrypted { data } => {
                    ReasoningContent::Encrypted(data.clone())
                }
            })
            .collect();

        AssistantContent::Reasoning(message_reasoning(self.id.clone(), content))
    }
}

impl PendingReasoningBlock {
    fn new(key: ReasoningBlockKey) -> Self {
        match key {
            ReasoningBlockKey::Summary(index) => Self::Summary {
                index,
                text: String::new(),
            },
            ReasoningBlockKey::Text(index) => Self::Text {
                index,
                text: String::new(),
            },
            ReasoningBlockKey::Encrypted => Self::Encrypted {
                data: String::new(),
            },
        }
    }

    fn key(&self) -> ReasoningBlockKey {
        match self {
            Self::Summary { index, .. } => ReasoningBlockKey::Summary(*index),
            Self::Text { index, .. } => ReasoningBlockKey::Text(*index),
            Self::Encrypted { .. } => ReasoningBlockKey::Encrypted,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self {
            Self::Summary { text, .. } | Self::Text { text, .. } => text,
            Self::Encrypted { data } => data,
        }
    }
}

impl PendingToolCall {
    pub(crate) fn new(id: Option<&str>) -> Self {
        Self {
            id: id.map(ToOwned::to_owned),
            call_id: None,
            name: None,
            arguments: None,
            argument_deltas: String::new(),
            saw_argument_delta: false,
        }
    }

    pub(crate) fn set_id(&mut self, id: Option<&str>) -> anyhow::Result<()> {
        set_consistent_id(&mut self.id, id, "tool call")
    }

    pub(crate) fn append_arguments(&mut self, delta: &str) {
        if !self.saw_argument_delta {
            self.arguments = None;
            self.saw_argument_delta = true;
        }
        self.argument_deltas.push_str(delta);
    }

    pub(crate) fn set_arguments(&mut self, arguments: serde_json::Value) {
        self.arguments = Some(arguments);
        self.argument_deltas.clear();
        self.saw_argument_delta = false;
    }

    pub(crate) fn assistant_content(&self) -> anyhow::Result<AssistantContent> {
        let id = self.id.clone().ok_or_else(|| {
            anyhow::anyhow!("OpenAI response stream tool call completed without an item ID")
        })?;
        let name = self.name.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "OpenAI response stream tool call {id} completed without a function name"
            )
        })?;
        let arguments = match &self.arguments {
            Some(arguments) => arguments.clone(),
            None => parse_tool_arguments(&self.argument_deltas),
        };
        let tool_call = ToolCall::from_dual_wire(
            id,
            self.call_id.clone().unwrap_or_default(),
            ToolFunction::new(name, arguments),
        );

        Ok(AssistantContent::ToolCall(tool_call))
    }
}

pub(crate) fn indexed_slot_mut<'a, T>(
    slots: &'a mut Vec<Option<T>>,
    index: u64,
    kind: &str,
) -> anyhow::Result<&'a mut Option<T>> {
    let slot_index = usize::try_from(index).map_err(|_| {
        anyhow::anyhow!(
            "OpenAI response stream {kind} index {index} cannot be represented on this platform"
        )
    })?;
    let required_len = slot_index.checked_add(1).ok_or_else(|| {
        anyhow::anyhow!("OpenAI response stream {kind} index {index} exceeds vector limits")
    })?;

    if required_len > slots.len() {
        slots
            .try_reserve(required_len - slots.len())
            .map_err(|error| {
                anyhow::anyhow!(
                    "OpenAI response stream could not allocate {kind} index {index}: {error}"
                )
            })?;
        slots.resize_with(required_len, || None);
    }

    Ok(&mut slots[slot_index])
}

pub(crate) fn clear_indexed_slot<T>(slots: &mut Vec<Option<T>>, index: u64) {
    let Ok(slot_index) = usize::try_from(index) else {
        return;
    };
    let Some(slot) = slots.get_mut(slot_index) else {
        return;
    };
    *slot = None;

    while slots.last().is_some_and(|slot| slot.is_none()) {
        slots.pop();
    }
}

pub(crate) fn set_consistent_id(
    current: &mut Option<String>,
    incoming: Option<&str>,
    kind: &str,
) -> anyhow::Result<()> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    match current {
        Some(current) if current != incoming => Err(anyhow::anyhow!(
            "OpenAI response stream {kind} item ID changed from {current} to {incoming}"
        )),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming.to_owned());
            Ok(())
        }
    }
}

pub(crate) fn parse_tool_arguments(arguments: &str) -> serde_json::Value {
    if arguments.trim().is_empty() {
        return serde_json::json!({});
    }
    // Preserve malformed model output verbatim. Dispatch will fail to
    // deserialize it against the tool's argument type, and that failure is
    // returned as a correlated tool result so the model can recover.
    serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_string()))
}

pub(crate) fn normalize_tool_arguments(arguments: serde_json::Value) -> serde_json::Value {
    match arguments {
        serde_json::Value::String(arguments) => parse_tool_arguments(&arguments),
        arguments => arguments,
    }
}

pub(crate) fn assistant_content_from_openai_output(
    output: Output,
) -> (Option<String>, Vec<AssistantContent>) {
    match output {
        Output::Message(message) => {
            let message_id = Some(message.id);
            let content = message
                .content
                .into_iter()
                .map(|content| match content {
                    OpenAiAssistantContent::OutputText(text) => {
                        AssistantContent::Text(normalize_openai_text(text))
                    }
                    OpenAiAssistantContent::Refusal { refusal } => AssistantContent::text(refusal),
                })
                .collect();
            (message_id, content)
        }
        Output::FunctionCall(function) => {
            let tool_call = ToolCall::from_dual_wire(
                function.id,
                function.call_id,
                ToolFunction::new(
                    function.name,
                    normalize_tool_arguments(serde_json::Value::String(
                        function.arguments.as_str().to_string(),
                    )),
                ),
            );
            (None, vec![AssistantContent::ToolCall(tool_call)])
        }
        Output::Reasoning {
            id,
            summary,
            content,
            encrypted_content,
            ..
        } => {
            let reasoning =
                PendingReasoning::from_item(Some(id.as_str()), summary, content, encrypted_content);
            (None, vec![reasoning.assistant_content()])
        }
        Output::Unknown(_) => (None, Vec::new()),
    }
}

pub fn normalize_openai_text(text: OutputText) -> Text {
    let AssistantContent::Text(text) = OpenAiAssistantContent::OutputText(text).into() else {
        unreachable!("an OpenAI output-text block must convert to Rig text")
    };
    text
}

pub fn message_reasoning(id: Option<String>, content: Vec<ReasoningContent>) -> MessageReasoning {
    let mut reasoning = MessageReasoning::summaries(Vec::new());
    if let Some(id) = id {
        reasoning = reasoning.with_id(id);
    }
    reasoning.content = content;
    reasoning
}

/// Read GPT-5.6's cache-write counter from the lossless terminal event before
/// Rig drops response fields it does not model. `Some(0)` deliberately remains
/// distinct from an absent or malformed field.
pub fn response_cache_write_tokens(payload: &str) -> Option<u64> {
    let event = serde_json::from_str::<serde_json::Value>(payload).ok()?;
    if !matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some("response.completed" | "response.done")
    ) {
        return None;
    }
    event
        .pointer("/response/usage/input_tokens_details/cache_write_tokens")
        .and_then(serde_json::Value::as_u64)
}

/// Decode terminal token accounting for the frontend and transport-owned
/// diagnostics. `None` when the response carried no usage object.
pub fn response_token_usage(response: &OpenAiCompletionResponse) -> Option<TokenUsage> {
    let usage = response.usage.as_ref()?;
    let cached_tokens = usage
        .input_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    Some(TokenUsage {
        input_tokens: usage.input_tokens,
        cached_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
    })
}

impl AssistantMessageAccumulator {
    pub fn streaming_message_with_search(
        &self,
        search: &crate::search::SearchState,
    ) -> anyhow::Result<Message> {
        let mut content = std::collections::BTreeMap::new();
        for (index, output) in self.outputs.iter().enumerate() {
            if let Some(output) = output {
                for (part, block) in output.assistant_content()?.into_iter().enumerate() {
                    if !matches!(block, AssistantContent::Text(_)) {
                        content.insert((index as u64, part as u64), block);
                    }
                }
            }
        }
        for (key, text) in search.visible_text() {
            content.insert(key, AssistantContent::Text(text.clone()));
        }
        anyhow::ensure!(
            !content.is_empty(),
            "no readable search answer or live reasoning yet"
        );
        Ok(Message::Assistant {
            id: None,
            content: content.into_values().collect(),
        })
    }
}
