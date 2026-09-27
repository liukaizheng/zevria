//! Context-compaction primitives shared by transcripts, engines, and providers.

pub mod replay;

use rig_core::message::{
    AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent,
};
use serde::{Deserialize, Serialize};

use crate::OwnedModelRequestItem;

/// Codex's built-in checkpoint prompt, kept as a documentation artifact too.
pub const SUMMARIZATION_PROMPT: &str = include_str!("../../../docs/instructions/compact-prompt.md");
/// Prefix identifying a model-generated checkpoint summary.
pub const SUMMARY_PREFIX: &str = {
    let document = include_str!("../../../docs/instructions/compact-summary-prefix.md").as_bytes();
    let prefix = document.split_at(document.len() - 1).0;
    // The checked-in Markdown has one conventional trailing newline; the
    // protocol prefix itself is the exact Codex template before that byte.
    match std::str::from_utf8(prefix) {
        Ok(prefix) => prefix,
        Err(_) => panic!("compact summary prefix must be UTF-8"),
    }
};

/// Current schema version for persisted compaction checkpoints.
pub const COMPACTION_VERSION: u32 = 1;
/// Default model context window used by automatic compaction.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 272_000;

/// Why a checkpoint was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    AutomaticPreTurn,
    AutomaticMidTurn,
}

/// Which implementation produced a checkpoint's replacement history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionBackend {
    LocalSummary,
    OpenaiResponsesCompact,
}

/// A durable context checkpoint. The transcript around it remains intact;
/// only model projection changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionCheckpoint {
    pub version: u32,
    pub trigger: CompactionTrigger,
    pub backend: CompactionBackend,
    pub replacement_history: Vec<OwnedModelRequestItem>,
    pub retained_user_messages: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactionCheckpointRecord {
    version: u32,
    trigger: CompactionTrigger,
    backend: CompactionBackend,
    replacement_history: Vec<OwnedModelRequestItem>,
    retained_user_messages: Vec<String>,
}

impl<'de> Deserialize<'de> for CompactionCheckpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let record = CompactionCheckpointRecord::deserialize(deserializer)?;
        let checkpoint = Self {
            version: record.version,
            trigger: record.trigger,
            backend: record.backend,
            replacement_history: record.replacement_history,
            retained_user_messages: record.retained_user_messages,
        };
        checkpoint.validate().map_err(D::Error::custom)?;
        Ok(checkpoint)
    }
}

impl CompactionCheckpoint {
    pub fn new(
        trigger: CompactionTrigger,
        backend: CompactionBackend,
        replacement_history: Vec<OwnedModelRequestItem>,
        retained_user_messages: Vec<String>,
    ) -> anyhow::Result<Self> {
        let checkpoint = Self {
            version: COMPACTION_VERSION,
            trigger,
            backend,
            replacement_history,
            retained_user_messages,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.replacement_history.iter().all(|item| !matches!(
                item,
                OwnedModelRequestItem::DeveloperInstruction(_)
                    | OwnedModelRequestItem::RequestInstruction(_)
                    | OwnedModelRequestItem::Message(Message::System { .. })
            )),
            "checkpoint conversation history cannot contain instruction state"
        );
        if self.version != COMPACTION_VERSION {
            anyhow::bail!("unsupported compaction checkpoint version {}", self.version);
        }
        if self.replacement_history.is_empty() {
            anyhow::bail!("compaction replacement history must not be empty");
        }
        Ok(())
    }
}

/// Approximate model tokens as UTF-8 bytes divided by four, rounded up.
pub const fn approximate_tokens_from_bytes(bytes: usize) -> u64 {
    (bytes as u64).saturating_add(3) / 4
}

/// Approximate the token count of text using Codex's bytes/4 heuristic.
pub fn approximate_tokens(text: &str) -> u64 {
    approximate_tokens_from_bytes(text.len())
}

/// Two complementary local measurements of model-visible context.
///
/// `payload_tokens` counts semantic text and tool payload without JSON wire
/// punctuation or protocol metadata. `conservative_tokens` retains the
/// serialized envelope, but replaces opaque reasoning material with bounded
/// type markers so ciphertext size can never dominate a capacity decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTokenEstimate {
    pub payload_tokens: u64,
    pub conservative_tokens: u64,
}

impl ContextTokenEstimate {
    pub const fn new(payload_tokens: u64, conservative_tokens: u64) -> Self {
        Self {
            payload_tokens,
            conservative_tokens,
        }
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            payload_tokens: self.payload_tokens.saturating_add(other.payload_tokens),
            conservative_tokens: self
                .conservative_tokens
                .saturating_add(other.conservative_tokens),
        }
    }
}

const ENCRYPTED_REASONING_MARKER: &str = "[encrypted reasoning]";
const REDACTED_REASONING_MARKER: &str = "[redacted reasoning]";
const SIGNATURE_MARKER: &str = "[reasoning signature]";

/// Count one JSON value as user-visible semantic data. Object keys remain
/// payload: a tool argument such as `{ "id": 7 }` must not lose `id` merely
/// because provider envelopes also happen to use that key.
pub(crate) fn semantic_json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(true) => 4,
        serde_json::Value::Bool(false) => 5,
        serde_json::Value::Number(number) => number.to_string().len(),
        serde_json::Value::String(value) => value.len(),
        serde_json::Value::Array(values) => values.iter().fold(0, |total, value| {
            total.saturating_add(semantic_json_bytes(value))
        }),
        serde_json::Value::Object(values) => values.iter().fold(0, |total, (key, value)| {
            total
                .saturating_add(key.len())
                .saturating_add(semantic_json_bytes(value))
        }),
    }
}

fn json_argument_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => serde_json::from_str(value)
            .as_ref()
            .map_or(value.len(), semantic_json_bytes),
        value => semantic_json_bytes(value),
    }
}

fn serialized_semantic_bytes<T: Serialize>(value: &T) -> usize {
    serde_json::to_value(value)
        .as_ref()
        .map_or(0, semantic_json_bytes)
}

fn message_payload_bytes(message: &Message) -> usize {
    match message {
        Message::System { content } => content.len(),
        Message::User { content } => content.iter().fold(0, |total, item| {
            let bytes = match item {
                UserContent::Text(text) => text.text.len(),
                UserContent::ToolResult(result) => result.name.len().saturating_add(
                    result.content.iter().fold(0, |total, item| {
                        total.saturating_add(match item {
                            ToolResultContent::Text(text) => text.text.len(),
                            ToolResultContent::Json { value } => semantic_json_bytes(value),
                            ToolResultContent::Image(image) => serialized_semantic_bytes(image),
                        })
                    }),
                ),
                UserContent::Image(image) => serialized_semantic_bytes(image),
                UserContent::Audio(audio) => serialized_semantic_bytes(audio),
                UserContent::Video(video) => serialized_semantic_bytes(video),
                UserContent::Document(document) => serialized_semantic_bytes(document),
            };
            total.saturating_add(bytes)
        }),
        Message::Assistant { content, .. } => content.iter().fold(0, |total, item| {
            let bytes = match item {
                AssistantContent::Text(text) => text.text.len(),
                AssistantContent::ToolCall(call) => call
                    .function
                    .name
                    .len()
                    .saturating_add(json_argument_bytes(&call.function.arguments)),
                AssistantContent::Reasoning(reasoning) => {
                    reasoning.content.iter().fold(0_usize, |total, item| {
                        total.saturating_add(match item {
                            ReasoningContent::Text { text, .. }
                            | ReasoningContent::Summary(text) => text.len(),
                            ReasoningContent::Encrypted(_) => ENCRYPTED_REASONING_MARKER.len(),
                            ReasoningContent::Redacted { .. } => REDACTED_REASONING_MARKER.len(),
                        })
                    })
                }
                AssistantContent::Image(image) => serialized_semantic_bytes(image),
            };
            total.saturating_add(bytes)
        }),
    }
}

fn replace_reasoning_opaque_content(value: &mut serde_json::Value) {
    let Some(message) = value.as_object_mut() else {
        return;
    };
    if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = message
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for block in content {
        let Some(block) = block.as_object_mut() else {
            continue;
        };
        if block.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
            continue;
        }
        let Some(reasoning) = block
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for item in reasoning {
            let Some(item) = item.as_object_mut() else {
                continue;
            };
            match item.get("type").and_then(serde_json::Value::as_str) {
                Some("encrypted") => {
                    item.insert(
                        "content".to_string(),
                        serde_json::Value::String(ENCRYPTED_REASONING_MARKER.to_string()),
                    );
                }
                Some("redacted") => {
                    if let Some(payload) = item.get_mut("content") {
                        *payload = serde_json::json!({"data": REDACTED_REASONING_MARKER});
                    }
                }
                Some("text") => {
                    if let Some(signature) = item.get_mut("signature") {
                        *signature = serde_json::Value::String(SIGNATURE_MARKER.to_string());
                    }
                    if let Some(payload) = item
                        .get_mut("content")
                        .and_then(serde_json::Value::as_object_mut)
                        .and_then(|payload| payload.get_mut("signature"))
                    {
                        *payload = serde_json::Value::String(SIGNATURE_MARKER.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Return an opaque-safe clone of provider-owned JSON. This is deliberately
/// scoped to provider replay; callers must never apply it to arbitrary tool
/// arguments or results, where identically named keys are user data.
pub fn opaque_safe_provider_json(value: &serde_json::Value) -> serde_json::Value {
    fn walk(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    walk(value);
                }
            }
            serde_json::Value::Object(values) => {
                let kind = values
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                for (key, value) in values.iter_mut() {
                    if key == "encrypted_content" {
                        *value = serde_json::Value::String(ENCRYPTED_REASONING_MARKER.to_string());
                    } else if key == "signature" {
                        *value = serde_json::Value::String(SIGNATURE_MARKER.to_string());
                    } else {
                        walk(value);
                    }
                }
                if matches!(kind.as_deref(), Some("redacted" | "redacted_reasoning")) {
                    if let Some(data) = values.get_mut("data") {
                        *data = serde_json::Value::String(REDACTED_REASONING_MARKER.to_string());
                    }
                    if let Some(content) = values.get_mut("content") {
                        *content = serde_json::Value::String(REDACTED_REASONING_MARKER.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let mut sanitized = value.clone();
    walk(&mut sanitized);
    sanitized
}

/// Estimate a typed canonical message without charging for opaque reasoning
/// blobs or treating JSON escaping and protocol metadata as model prose.
pub fn estimate_message_tokens(message: &Message) -> ContextTokenEstimate {
    let (message, images) = image_accounting_projection(message);
    let image_tokens = (images as u64).saturating_mul(crate::prompt::ESTIMATED_IMAGE_TOKENS as u64);
    let payload_tokens =
        approximate_tokens_from_bytes(message_payload_bytes(&message)).saturating_add(image_tokens);
    let mut conservative = serde_json::to_value(&message).unwrap_or(serde_json::Value::Null);
    replace_reasoning_opaque_content(&mut conservative);
    let conservative_tokens = serde_json::to_vec(&conservative)
        .map_or(0, |bytes| approximate_tokens_from_bytes(bytes.len()));
    ContextTokenEstimate::new(
        payload_tokens,
        conservative_tokens.saturating_add(image_tokens),
    )
}

/// Temporary typed accounting copy only. Never use it for transport, storage,
/// request identity, or continuation comparisons.
fn image_accounting_projection(message: &Message) -> (Message, usize) {
    let mut message = message.clone();
    let mut count = 0;
    let mut strip = |image: &mut rig_core::message::Image| {
        count += 1;
        image.data = rig_core::message::DocumentSourceKind::Base64(String::new());
    };
    match &mut message {
        Message::User { content } => {
            for block in content {
                match block {
                    UserContent::Image(image) => strip(image),
                    UserContent::ToolResult(result) => {
                        for block in &mut result.content {
                            if let ToolResultContent::Image(image) = block {
                                strip(image);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Message::Assistant { content, .. } => {
            for block in content {
                if let AssistantContent::Image(image) = block {
                    strip(image);
                }
            }
        }
        _ => {}
    }
    (message, count)
}

/// Responses input accounting at actual wire image locations. Arbitrary tool
/// JSON/string lookalikes are ordinary text and are never recursively scrubbed.
/// The 1,600/image component is explicitly approximate, even when protocol
/// overhead uses the historical `conservative_tokens` field.
pub fn estimate_responses_input_tokens(input: &[serde_json::Value], property_bytes: usize) -> u64 {
    let mut projected = input.to_vec();
    let mut images = 0u64;
    for item in &mut projected {
        let kind = item.get("type").and_then(serde_json::Value::as_str);
        if matches!(kind, Some("reasoning" | "compaction")) {
            *item = opaque_safe_provider_json(item);
            continue;
        }
        if !matches!(kind, None | Some("message"))
            || !matches!(
                item.get("role").and_then(serde_json::Value::as_str),
                Some("user" | "assistant")
            )
        {
            continue;
        }
        if let Some(content) = item
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        {
            for block in content {
                if block.get("type").and_then(serde_json::Value::as_str) == Some("input_image")
                    && let Some(object) = block.as_object_mut()
                {
                    if let Some(data) = object.get_mut("image_url") {
                        *data = serde_json::Value::String(String::new());
                    }
                    if let Some(data) = object.get_mut("file_id") {
                        *data = serde_json::Value::String(String::new());
                    }
                    images = images.saturating_add(1);
                }
            }
        }
    }
    approximate_tokens_from_bytes(
        serde_json::to_vec(&projected)
            .map_or(usize::MAX, |bytes| bytes.len())
            .saturating_add(property_bytes),
    )
    .saturating_add(images.saturating_mul(crate::prompt::ESTIMATED_IMAGE_TOKENS as u64))
}

/// Whether `message` is one of our generated summary user messages.
pub fn is_summary_message(message: &str) -> bool {
    message
        .strip_prefix(SUMMARY_PREFIX)
        .is_some_and(|suffix| suffix.starts_with('\n'))
}

/// Select the newest user prompts that fit `max_tokens`, then restore their
/// chronological order. Only the oldest selected prompt may be partial.
/// Codex's middle truncation keeps both ends on valid UTF-8 boundaries and
/// inserts a token-count marker between them.
pub fn select_recent_user_messages(messages: &[String], max_tokens: u64) -> Vec<String> {
    if max_tokens == 0 {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut remaining = max_tokens;
    for message in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approximate_tokens(message);
        if tokens <= remaining {
            selected.push(message.clone());
            remaining = remaining.saturating_sub(tokens);
            continue;
        }

        selected.push(truncate_middle_to_token_budget(message, remaining));
        break;
    }
    selected.reverse();
    selected
}

fn truncate_middle_to_token_budget(text: &str, max_tokens: u64) -> String {
    let max_bytes = usize::try_from(max_tokens.saturating_mul(4)).unwrap_or(usize::MAX);
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let left_budget = max_bytes / 2;
    let right_budget = max_bytes.saturating_sub(left_budget);
    let tail_target = text.len().saturating_sub(right_budget);
    let mut prefix_end = 0;
    let mut suffix_start = text.len();
    let mut suffix_started = false;

    for (index, character) in text.char_indices() {
        let character_end = index.saturating_add(character.len_utf8());
        if character_end <= left_budget {
            prefix_end = character_end;
            continue;
        }
        if index >= tail_target && !suffix_started {
            suffix_start = index;
            suffix_started = true;
        }
    }
    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    let removed_tokens = approximate_tokens_from_bytes(text.len().saturating_sub(max_bytes));
    format!(
        "{}…{removed_tokens} tokens truncated…{}",
        &text[..prefix_end],
        &text[suffix_start..]
    )
}

/// Construct local replacement history: retained real prompts, followed by
/// exactly one generated-summary user message. An empty summary is valid.
pub fn local_replacement_history(
    retained_user_messages: &[String],
    summary: &str,
) -> Vec<OwnedModelRequestItem> {
    let mut history = retained_user_messages
        .iter()
        .map(|message| OwnedModelRequestItem::message(Message::user(message.clone())))
        .collect::<Vec<_>>();
    history.push(OwnedModelRequestItem::message(Message::user(format!(
        "{SUMMARY_PREFIX}\n{summary}"
    ))));
    history
}

/// Ordinary local compaction replaces only a nonempty prefix. The remaining
/// source items are cloned verbatim, including opaque native replay envelopes.
/// Retained-user metadata is deliberately not part of this construction.
pub fn summary_tail_history(
    source: &[OwnedModelRequestItem],
    prefix_end: usize,
    summary: &str,
) -> anyhow::Result<Vec<OwnedModelRequestItem>> {
    anyhow::ensure!(
        prefix_end > 0 && prefix_end <= source.len(),
        "invalid nonempty summary prefix boundary"
    );
    let mut history = Vec::with_capacity(1 + source.len() - prefix_end);
    history.push(OwnedModelRequestItem::message(Message::user(format!(
        "{SUMMARY_PREFIX}\n{summary}"
    ))));
    history.extend_from_slice(&source[prefix_end..]);
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multimodal_summary_tail_and_checkpoint_replay_keep_indivisible_validated_images() {
        let image = crate::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
        let prompt = crate::UserPrompt::new(vec![
            crate::PromptBlock::Text("before".into()),
            crate::PromptBlock::Image(image),
            crate::PromptBlock::Text("after".into()),
        ])
        .unwrap();
        let source = vec![
            OwnedModelRequestItem::message(Message::user("old")),
            OwnedModelRequestItem::message(prompt.to_message()),
        ];
        let mut tail = summary_tail_history(&source, 1, "textual summary").unwrap();
        assert_eq!(tail[1], source[1]);
        let serialized = serde_json::to_vec(&tail).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<OwnedModelRequestItem>>(&serialized).unwrap(),
            tail
        );
        if let OwnedModelRequestItem::Message(Message::User { content }) = &mut tail[1]
            && let UserContent::Image(image) = &mut content[1]
        {
            image.data =
                rig_core::message::DocumentSourceKind::Base64("invalid-image-payload".into());
        }
        let error = serde_json::from_slice::<Vec<OwnedModelRequestItem>>(
            &serde_json::to_vec(&tail).unwrap(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("invalid-image-payload"));
    }

    #[test]
    fn built_in_templates_are_the_documented_codex_templates() {
        assert!(SUMMARIZATION_PROMPT.starts_with("You are performing a CONTEXT CHECKPOINT"));
        assert!(SUMMARY_PREFIX.starts_with("Another language model started to solve"));
        assert_eq!(
            format!("{SUMMARY_PREFIX}\n"),
            include_str!("../../../docs/instructions/compact-summary-prefix.md")
        );
    }

    #[test]
    fn byte_estimate_rounds_up() {
        assert_eq!(approximate_tokens_from_bytes(0), 0);
        assert_eq!(approximate_tokens_from_bytes(1), 1);
        assert_eq!(approximate_tokens_from_bytes(4), 1);
        assert_eq!(approximate_tokens_from_bytes(5), 2);
    }

    #[test]
    fn opaque_reasoning_size_never_changes_message_estimates() {
        let message = |encrypted: String, redacted: String| Message::Assistant {
            id: Some("provider-message-id".to_string()),
            content: vec![AssistantContent::Reasoning(rig_core::message::Reasoning {
                id: Some("reasoning-id".to_string()),
                content: vec![
                    ReasoningContent::Summary("visible summary".to_string()),
                    ReasoningContent::Encrypted(encrypted),
                    ReasoningContent::Redacted { data: redacted },
                ],
            })],
        };
        let short = estimate_message_tokens(&message("ten bytes!".to_string(), "x".to_string()));
        let large = estimate_message_tokens(&message("x".repeat(300_000), "y".repeat(200_000)));
        assert_eq!(short, large);
    }

    #[test]
    fn semantic_text_and_tool_json_ignore_wire_escaping_but_keep_user_keys() {
        let text = "line one\nquoted \"value\" and a \\ path";
        let text_estimate = estimate_message_tokens(&Message::user(text));
        assert_eq!(text_estimate.payload_tokens, approximate_tokens(text));

        let call = |arguments| Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(
                rig_core::message::ToolCall::new(
                    rig_core::message::ToolCallId::new("call").expect("nonempty"),
                    rig_core::message::ToolFunction::new("tool".to_string(), arguments),
                ),
            )],
        };
        let empty = estimate_message_tokens(&call(serde_json::json!({})));
        let with_user_id = estimate_message_tokens(&call(serde_json::json!({
            "id": "user-owned-value",
            "nested": {"quoted": "a\\b\"c"}
        })));
        assert!(with_user_id.payload_tokens > empty.payload_tokens);
    }

    #[test]
    fn summary_recognition_requires_the_exact_prefix_and_newline() {
        assert!(is_summary_message(&format!("{SUMMARY_PREFIX}\nsummary")));
        assert!(is_summary_message(&format!("{SUMMARY_PREFIX}\n")));
        assert!(!is_summary_message(SUMMARY_PREFIX));
        assert!(!is_summary_message("Another language model\nsummary"));
    }

    #[test]
    fn recent_messages_are_returned_in_chronological_order() {
        let messages = vec![
            "old-old".to_string(),
            "middle".to_string(),
            "new".to_string(),
        ];
        assert_eq!(
            select_recent_user_messages(&messages, 3),
            vec!["middle".to_string(), "new".to_string()]
        );
    }

    #[test]
    fn oldest_partial_message_preserves_utf8_safe_ends_with_a_marker() {
        let messages = vec!["old🙂middle🙂tail".to_string(), "new".to_string()];
        let selected = select_recent_user_messages(&messages, 3);
        assert_eq!(selected.last().map(String::as_str), Some("new"));
        assert_eq!(selected.len(), 2);
        assert!(selected[0].starts_with("ol"));
        assert!(selected[0].ends_with("il"));
        assert!(selected[0].contains("tokens truncated"));
        assert!(std::str::from_utf8(selected[0].as_bytes()).is_ok());
    }

    #[test]
    fn oldest_partial_message_uses_codex_middle_truncation() {
        let messages = vec!["abcdefghijklmnopqrstuvwxyz".to_string()];
        assert_eq!(
            select_recent_user_messages(&messages, 3),
            vec!["abcdef…4 tokens truncated…uvwxyz".to_string()]
        );
    }

    #[test]
    fn empty_summary_still_produces_the_prefixed_user_message() {
        let history = local_replacement_history(&[], "");
        let message = history[0].message_ref().expect("message");
        assert_eq!(message, &Message::user(format!("{SUMMARY_PREFIX}\n")));
    }
}
