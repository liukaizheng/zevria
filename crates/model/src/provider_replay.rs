//! Provider-native response data retained for faithful request replay.

use std::collections::BTreeMap;

use anyhow::Context as _;
use rig_core::{
    message::{
        AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolCallId,
        ToolFunction,
    },
    providers::openai::responses_api::{
        AssistantContent as OpenAiAssistantContent, Output, ReasoningSummary,
    },
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    ModelProfileRef,
    compaction::{
        ContextTokenEstimate, approximate_tokens_from_bytes, estimate_message_tokens,
        opaque_safe_provider_json, semantic_json_bytes,
    },
};

/// Provider tag used by OpenAI Responses transcript records.
pub const OPENAI_RESPONSES_PROVIDER: &str = "openai.responses";
/// Current schema version for [`ProviderReplay`].
pub const PROVIDER_REPLAY_VERSION: u32 = 1;

/// An ordered provider-native output ledger attached to one assistant message.
///
/// The envelope is deliberately small and versioned. Items remain raw JSON in
/// storage; the canonical-message projection interprets known output shapes
/// while provider-specific and newly introduced fields survive persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderReplay {
    pub provider: String,
    pub version: u32,
    pub source_profile: ModelProfileRef,
    pub items: Vec<serde_json::Value>,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static CANONICAL_DERIVATIONS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Scoped to this test thread, so unrelated/concurrent tests never participate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn count_canonical_derivations<T>(run: impl FnOnce() -> T) -> (T, usize) {
    struct Reset(Option<usize>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CANONICAL_DERIVATIONS.set(self.0);
        }
    }
    let _reset = Reset(CANONICAL_DERIVATIONS.replace(Some(0)));
    let result = run();
    (result, CANONICAL_DERIVATIONS.get().expect("active scope"))
}

/// An inseparable, validated provider response and its canonical message.
///
/// The only constructor derives the message from the consumed native ledger.
/// Cloning this value copies that established invariant without decoding again;
/// callers cannot supply independent halves or mutate either half.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayMessage {
    message: Message,
    replay: ProviderReplay,
}

impl ReplayMessage {
    pub fn new(replay: ProviderReplay) -> anyhow::Result<Self> {
        let message = replay.to_message()?;
        Ok(Self { message, replay })
    }

    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn replay(&self) -> &ProviderReplay {
        &self.replay
    }

    /// Move the canonical message into an owned consumer and drop the unused
    /// native ledger, without cloning either representation.
    pub fn into_message(self) -> Message {
        self.message
    }
}

/// Provider-neutral representation of a replay for a foreign profile.
/// Correlations map every original native tool handle to the deterministic
/// handle used by the portable assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct PortableReplayProjection {
    pub message: Option<Message>,
    pub tool_correlations: BTreeMap<String, String>,
}

impl ProviderReplay {
    /// Build the current OpenAI Responses replay envelope.
    pub fn openai_responses(
        source_profile: ModelProfileRef,
        items: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            provider: OPENAI_RESPONSES_PROVIDER.to_string(),
            version: PROVIDER_REPLAY_VERSION,
            source_profile,
            items,
        }
    }

    /// Validate the envelope before it is trusted as provider input.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider != OPENAI_RESPONSES_PROVIDER {
            return Err("unknown_provider");
        }
        if self.version != PROVIDER_REPLAY_VERSION {
            return Err("unsupported_version");
        }
        if self.source_profile.provider.trim().is_empty()
            || self.source_profile.model.trim().is_empty()
        {
            return Err("invalid_source_profile");
        }
        if self.items.is_empty() {
            return Err("empty_items");
        }
        if self.items.iter().any(|item| !item.is_object()) {
            return Err("invalid_item");
        }
        Ok(())
    }

    /// Native OpenAI items, when this is a valid current envelope.
    pub fn openai_responses_items(&self) -> Option<&[serde_json::Value]> {
        self.validate().ok().map(|()| self.items.as_slice())
    }

    /// Native items are safe only for the exact configured source profile.
    pub fn native_items_for(
        &self,
        target_profile: &ModelProfileRef,
    ) -> anyhow::Result<Option<&[serde_json::Value]>> {
        self.validate()
            .map_err(|reason| anyhow::anyhow!("invalid provider replay envelope: {reason}"))?;
        Ok((&self.source_profile == target_profile).then_some(self.items.as_slice()))
    }

    /// Derive a deterministic provider-neutral projection for a foreign
    /// profile. Visible text/refusals and function calls survive; reasoning,
    /// encrypted data, unknown output types, message IDs, and native output
    /// item IDs are deliberately omitted.
    pub fn portable_projection(&self) -> anyhow::Result<PortableReplayProjection> {
        self.validate()
            .map_err(|reason| anyhow::anyhow!("invalid provider replay envelope: {reason}"))?;

        let mut content = Vec::new();
        let mut tool_correlations = BTreeMap::new();
        for (index, item) in self.items.iter().cloned().enumerate() {
            let output: Output = serde_json::from_value(item).with_context(|| {
                format!("invalid OpenAI Responses output item at index {index}")
            })?;
            match output {
                Output::Message(message) => {
                    content.extend(message.content.into_iter().map(|block| match block {
                        OpenAiAssistantContent::OutputText(text) => {
                            let AssistantContent::Text(text) =
                                OpenAiAssistantContent::OutputText(text).into()
                            else {
                                unreachable!("output_text converts to text")
                            };
                            AssistantContent::Text(Text::new(crate::citations::render_text(&text)))
                        }
                        OpenAiAssistantContent::Refusal { refusal } => {
                            AssistantContent::Text(Text::new(refusal))
                        }
                    }));
                }
                Output::FunctionCall(function) => {
                    if function.call_id.is_empty() {
                        anyhow::bail!(
                            "OpenAI Responses function_call at index {index} had an empty call_id"
                        );
                    }
                    let portable_id = portable_tool_call_id(
                        &self.source_profile,
                        &function.call_id,
                        &function.name,
                        function.arguments.as_str(),
                    );
                    tool_correlations.insert(function.call_id.clone(), portable_id.clone());
                    if !function.id.is_empty() {
                        tool_correlations.insert(function.id, portable_id.clone());
                    }
                    let id = ToolCallId::new(portable_id)
                        .expect("portable tool-call identifiers are nonempty");
                    content.push(AssistantContent::ToolCall(ToolCall::new(
                        id,
                        ToolFunction::new(
                            function.name,
                            normalize_tool_arguments(function.arguments.as_str()),
                        ),
                    )));
                }
                Output::Reasoning { .. } | Output::Unknown(_) => {}
            }
        }

        let message = (!content.is_empty()).then_some(Message::Assistant { id: None, content });
        Ok(PortableReplayProjection {
            message,
            tool_correlations,
        })
    }

    /// Derive the canonical Rig assistant message represented by this replay.
    ///
    /// Native item order is authoritative. Unknown output-item types remain in
    /// the replay ledger but have no provider-neutral message representation.
    /// Known items are decoded strictly so a malformed native completion can
    /// never be persisted beside an independently assembled fallback message.
    pub fn to_message(&self) -> anyhow::Result<Message> {
        #[cfg(any(test, feature = "test-support"))]
        CANONICAL_DERIVATIONS.with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
        self.validate()
            .map_err(|reason| anyhow::anyhow!("invalid provider replay envelope: {reason}"))?;

        let mut message_id = None;
        let mut content = Vec::new();
        for (index, item) in self.items.iter().cloned().enumerate() {
            let output: Output = serde_json::from_value(item).with_context(|| {
                format!("invalid OpenAI Responses output item at index {index}")
            })?;
            match output {
                Output::Message(message) => {
                    if message_id.is_none() {
                        message_id = Some(message.id);
                    }
                    content.extend(message.content.into_iter().map(|block| match block {
                        OpenAiAssistantContent::OutputText(text) => {
                            // Rig's wire conversion retains non-empty unknown
                            // output-text siblings in `additional_params`.
                            let AssistantContent::Text(text) =
                                OpenAiAssistantContent::OutputText(text).into()
                            else {
                                unreachable!("an output_text block must convert to Rig text")
                            };
                            AssistantContent::Text(text)
                        }
                        OpenAiAssistantContent::Refusal { refusal } => {
                            AssistantContent::Text(Text::new(refusal))
                        }
                    }));
                }
                Output::FunctionCall(function) => {
                    if function.call_id.is_empty() {
                        anyhow::bail!(
                            "OpenAI Responses function_call at index {index} had an empty call_id"
                        );
                    }
                    let arguments = normalize_tool_arguments(function.arguments.as_str());
                    content.push(AssistantContent::ToolCall(ToolCall::from_dual_wire(
                        function.id,
                        function.call_id,
                        ToolFunction::new(function.name, arguments),
                    )));
                }
                Output::Reasoning {
                    id,
                    summary,
                    content: reasoning_text,
                    encrypted_content,
                    ..
                } => {
                    let reasoning_content =
                        canonical_reasoning_content(summary, reasoning_text, encrypted_content);
                    if !reasoning_content.is_empty() {
                        content.push(AssistantContent::Reasoning(Reasoning {
                            id: Some(id),
                            content: reasoning_content,
                        }));
                    }
                }
                Output::Unknown(_) => {}
            }
        }

        if content.is_empty() {
            anyhow::bail!("provider replay contained no assistant content");
        }

        Ok(Message::Assistant {
            id: message_id,
            content,
        })
    }

    /// Estimate a replay-only ledger. Known Responses output shapes are
    /// reduced to visible payload; unknown future shapes fall back to their
    /// opaque-safe serialized representation so capacity remains
    /// conservative without charging for ciphertext length.
    pub fn replay_only_token_estimate(&self) -> ContextTokenEstimate {
        let sanitized = opaque_safe_provider_json(&serde_json::Value::Array(self.items.clone()));
        let conservative_tokens = crate::compaction::estimate_responses_input_tokens(
            sanitized.as_array().expect("array projection"),
            0,
        );
        let payload_tokens = self.items.iter().fold(0_u64, |total, item| {
            let tokens = replay_item_payload_bytes(item).map_or_else(
                || {
                    let sanitized = opaque_safe_provider_json(item);
                    crate::compaction::estimate_responses_input_tokens(
                        std::slice::from_ref(&sanitized),
                        0,
                    )
                },
                approximate_tokens_from_bytes,
            );
            total.saturating_add(tokens)
        });
        ContextTokenEstimate::new(payload_tokens, conservative_tokens)
    }

    /// Canonical native estimate. Destination-aware callers must estimate the
    /// portable projection instead when crossing profiles (citations expand).
    pub fn canonical_token_estimate(&self) -> anyhow::Result<ContextTokenEstimate> {
        self.to_message()
            .map(|message| estimate_message_tokens(&message))
    }
}

fn semantic_argument_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => serde_json::from_str::<serde_json::Value>(value)
            .as_ref()
            .map_or(value.len(), semantic_json_bytes),
        value => semantic_json_bytes(value),
    }
}

fn visible_text_bytes(value: Option<&serde_json::Value>) -> usize {
    value
        .and_then(serde_json::Value::as_str)
        .map_or(0, str::len)
}

fn replay_item_payload_bytes(item: &serde_json::Value) -> Option<usize> {
    let object = item.as_object()?;
    let kind = object.get("type")?.as_str()?;
    match kind {
        "message" => {
            let content = object.get("content")?.as_array()?;
            content.iter().try_fold(0_usize, |total, block| {
                let block = block.as_object()?;
                let bytes = match block.get("type")?.as_str()? {
                    "output_text" | "input_text" => visible_text_bytes(block.get("text")),
                    "refusal" => visible_text_bytes(block.get("refusal")),
                    "input_image" => crate::prompt::ESTIMATED_IMAGE_TOKENS.saturating_mul(4),
                    _ => return None,
                };
                Some(total.saturating_add(bytes))
            })
        }
        "function_call" | "custom_tool_call" => Some(
            visible_text_bytes(object.get("name"))
                .saturating_add(object.get("arguments").map_or(0, semantic_argument_bytes)),
        ),
        "function_call_output" | "custom_tool_call_output" => {
            Some(object.get("output").map_or(0, semantic_json_bytes))
        }
        "reasoning" => {
            let mut total = 0_usize;
            if let Some(summary) = object.get("summary").and_then(serde_json::Value::as_array) {
                for block in summary {
                    total = total.saturating_add(
                        block
                            .as_object()
                            .and_then(|block| block.get("text"))
                            .map_or(0, semantic_json_bytes),
                    );
                }
            }
            if let Some(content) = object.get("content").and_then(serde_json::Value::as_array) {
                for block in content {
                    total = total.saturating_add(match block {
                        serde_json::Value::String(text) => text.len(),
                        serde_json::Value::Object(block) => block.get("text").map_or_else(
                            || semantic_json_bytes(&serde_json::Value::Object(block.clone())),
                            semantic_json_bytes,
                        ),
                        value => semantic_json_bytes(value),
                    });
                }
            }
            if object
                .get("encrypted_content")
                .is_some_and(|value| !value.is_null())
            {
                total = total.saturating_add("[encrypted reasoning]".len());
            }
            Some(total)
        }
        _ => None,
    }
}

fn portable_tool_call_id(
    source_profile: &ModelProfileRef,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> String {
    let mut digest = Sha256::new();
    for component in [
        source_profile.provider.as_bytes(),
        source_profile.model.as_bytes(),
        call_id.as_bytes(),
        name.as_bytes(),
        arguments.as_bytes(),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component);
    }
    let digest = digest.finalize();
    let mut id = String::from("zevria_call_");
    for byte in digest.iter().take(12) {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").expect("writing to a string cannot fail");
    }
    id
}

fn normalize_tool_arguments(arguments: &str) -> serde_json::Value {
    if arguments.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(arguments)
        .unwrap_or_else(|_| serde_json::Value::String(arguments.to_string()))
}

fn canonical_reasoning_content(
    summary: Vec<ReasoningSummary>,
    reasoning_text: Vec<String>,
    encrypted_content: Option<String>,
) -> Vec<ReasoningContent> {
    let mut content = summary
        .into_iter()
        .map(|summary| ReasoningContent::Summary(summary.text().to_owned()))
        .collect::<Vec<_>>();
    content.extend(
        reasoning_text
            .into_iter()
            .map(|text| ReasoningContent::Text {
                text,
                signature: None,
            }),
    );
    if let Some(encrypted) = encrypted_content.filter(|encrypted| !encrypted.is_empty()) {
        content.push(ReasoningContent::Encrypted(encrypted));
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::AdditionalParams;
    use serde_json::json;

    fn test_profile() -> ModelProfileRef {
        ModelProfileRef::new("test-provider", "test-model")
    }

    #[test]
    fn replay_only_input_images_use_the_shared_allowance_without_mutating_payloads() {
        let estimate = |data: String| {
            let replay = ProviderReplay::openai_responses(
                test_profile(),
                vec![
                    json!({"type":"message", "role":"user", "content":[{"type":"input_image", "image_url":data}]}),
                ],
            );
            let original = replay.clone();
            let estimate = replay.replay_only_token_estimate();
            assert_eq!(replay, original);
            estimate
        };
        let small = estimate("data:image/png;base64,abcd".into());
        let large = estimate(format!("data:image/png;base64,{}", "a".repeat(2_000_000)));
        assert_eq!(small, large);
        assert_eq!(
            small.payload_tokens,
            crate::prompt::ESTIMATED_IMAGE_TOKENS as u64
        );
        assert!(small.conservative_tokens < 1_700);
    }

    #[test]
    fn replay_conversion_is_ordered_lossless_and_normalizes_tool_arguments() {
        let annotated_extras = json!({
            "annotations": [{"type": "citation", "title": "source"}],
            "future_text_field": {"kept": true}
        });
        let annotated_text = Text {
            text: "first block".to_string(),
            additional_params: AdditionalParams::from_entries(Some((
                "openai_responses",
                annotated_extras,
            ))),
        };
        let malformed_arguments = r#"{"city":"#;
        let items = vec![
            json!({
                "type": "future_hosted_tool",
                "id": "hosted_1",
                "unknown": [null, true]
            }),
            json!({
                "type": "message",
                "id": "msg_first",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "first block",
                        "annotations": [{"type": "citation", "title": "source"}],
                        "future_text_field": {"kept": true}
                    },
                    {"type": "refusal", "refusal": "cannot comply"}
                ],
                "future_message_field": "retained only natively"
            }),
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [
                    {"type": "summary_text", "text": "summary one"},
                    {"type": "summary_text", "text": "summary two"}
                ],
                "content": [
                    {"type": "reasoning_text", "text": "private one"},
                    {"type": "reasoning_text", "text": "private two"}
                ],
                "encrypted_content": "ciphertext",
                "status": null,
                "future_reasoning_field": {"kept": 1}
            }),
            json!({
                "type": "function_call",
                "id": "fc_valid",
                "call_id": "call_valid",
                "name": "weather",
                "arguments": "{\"city\":\"Paris\",\"units\":1.0}",
                "status": "completed",
                "future_call_field": true
            }),
            json!({
                "type": "message",
                "id": "msg_second",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {"type": "output_text", "text": "second block"}
                ]
            }),
            json!({
                "type": "function_call",
                "id": "fc_malformed",
                "call_id": "call_malformed",
                "name": "weather",
                "arguments": malformed_arguments,
                "status": "completed"
            }),
            json!({
                "type": "function_call",
                "id": "fc_empty",
                "call_id": "call_empty",
                "name": "refresh",
                "arguments": "  ",
                "status": "completed"
            }),
        ];
        let replay = ProviderReplay::openai_responses(test_profile(), items.clone());

        let message = replay.to_message().expect("replay should convert");

        assert_eq!(
            message,
            Message::Assistant {
                id: Some("msg_first".to_string()),
                content: vec![
                    AssistantContent::Text(annotated_text),
                    AssistantContent::text("cannot comply"),
                    AssistantContent::Reasoning(Reasoning {
                        id: Some("rs_1".to_string()),
                        content: vec![
                            ReasoningContent::Summary("summary one".to_string()),
                            ReasoningContent::Summary("summary two".to_string()),
                            ReasoningContent::Text {
                                text: "private one".to_string(),
                                signature: None,
                            },
                            ReasoningContent::Text {
                                text: "private two".to_string(),
                                signature: None,
                            },
                            ReasoningContent::Encrypted("ciphertext".to_string()),
                        ],
                    }),
                    AssistantContent::ToolCall(ToolCall::from_dual_wire(
                        "fc_valid",
                        "call_valid",
                        ToolFunction::new(
                            "weather".to_string(),
                            json!({"city": "Paris", "units": 1.0}),
                        ),
                    )),
                    AssistantContent::text("second block"),
                    AssistantContent::ToolCall(ToolCall::from_dual_wire(
                        "fc_malformed",
                        "call_malformed",
                        ToolFunction::new(
                            "weather".to_string(),
                            serde_json::Value::String(malformed_arguments.to_string()),
                        ),
                    )),
                    AssistantContent::ToolCall(ToolCall::from_dual_wire(
                        "fc_empty",
                        "call_empty",
                        ToolFunction::new("refresh".to_string(), json!({})),
                    )),
                ],
            }
        );
        assert_eq!(
            replay.items, items,
            "conversion must not alter native items"
        );
    }

    #[test]
    fn portable_projection_drops_provider_bound_content_and_rebuilds_tool_ids() {
        let replay = ProviderReplay::openai_responses(
            ModelProfileRef::new("source", "source-model"),
            vec![
                json!({
                    "type": "message",
                    "id": "msg_native",
                    "role": "assistant",
                    "status": "completed",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "visible answer",
                            "annotations": [{"type": "citation", "secret": "native"}]
                        },
                        {"type": "refusal", "refusal": "visible refusal"}
                    ]
                }),
                json!({
                    "type": "reasoning",
                    "id": "rs_native",
                    "summary": [{"type": "summary_text", "text": "private summary"}],
                    "content": [{"type": "reasoning_text", "text": "private reasoning"}],
                    "encrypted_content": "ciphertext",
                    "status": null
                }),
                json!({
                    "type": "function_call",
                    "id": "fc_native",
                    "call_id": "call_native",
                    "name": "command",
                    "arguments": "{\"command\":\"pwd\"}",
                    "status": "completed"
                }),
                json!({
                    "type": "future_opaque_output",
                    "id": "opaque_native",
                    "secret": true
                }),
            ],
        );

        let first = replay.portable_projection().expect("portable projection");
        let second = replay
            .portable_projection()
            .expect("deterministic projection");
        assert_eq!(first, second);
        let message = first.message.expect("visible portable message");
        let Message::Assistant { id, content } = message else {
            panic!("portable replay must be an assistant message");
        };
        assert_eq!(id, None);
        assert_eq!(content.len(), 3);
        assert_eq!(content[0], AssistantContent::text("visible answer"));
        assert_eq!(content[1], AssistantContent::text("visible refusal"));
        let AssistantContent::ToolCall(call) = &content[2] else {
            panic!("portable replay must retain the function call");
        };
        assert!(call.provider.is_none());
        assert!(call.id.as_str().starts_with("zevria_call_"));
        assert_ne!(call.id.as_str(), "call_native");
        assert_eq!(call.function.name, "command");
        assert_eq!(call.function.arguments, json!({"command": "pwd"}));
        assert_eq!(first.tool_correlations["call_native"], call.id.as_str());
        assert_eq!(first.tool_correlations["fc_native"], call.id.as_str());
        let serialized = serde_json::to_string(&content).expect("portable content JSON");
        for omitted in [
            "msg_native",
            "rs_native",
            "private summary",
            "private reasoning",
            "ciphertext",
            "fc_native",
            "call_native",
            "opaque_native",
            "annotations",
        ] {
            assert!(!serialized.contains(omitted), "portable leak: {omitted}");
        }
    }

    #[test]
    fn replay_conversion_rejects_malformed_known_items_and_no_content() {
        let malformed = ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "message",
                "id": "msg_bad",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": 7}]
            })],
        );
        assert!(malformed.to_message().is_err());

        let unknown_only = ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "future_output_type",
                "payload": true
            })],
        );
        assert!(unknown_only.to_message().is_err());

        let empty_reasoning = ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "reasoning",
                "id": "rs_empty",
                "summary": [],
                "content": [],
                "encrypted_content": "",
                "status": null
            })],
        );
        assert!(empty_reasoning.to_message().is_err());
    }

    #[test]
    fn replay_only_estimates_are_opaque_safe_for_known_and_future_shapes() {
        let known = |encrypted: String| {
            ProviderReplay::openai_responses(
                test_profile(),
                vec![json!({
                    "type": "reasoning",
                    "id": "reasoning-id",
                    "summary": [{"type": "summary_text", "text": "visible"}],
                    "content": [],
                    "encrypted_content": encrypted,
                    "status": null
                })],
            )
        };
        assert_eq!(
            known("short".to_string()).replay_only_token_estimate(),
            known("x".repeat(400_000)).replay_only_token_estimate()
        );

        let future = |encrypted: String| {
            ProviderReplay::openai_responses(
                test_profile(),
                vec![json!({
                    "type": "future_output_type",
                    "provider_payload": {"encrypted_content": encrypted},
                    "visible": "future envelope"
                })],
            )
        };
        assert_eq!(
            future("short".to_string()).replay_only_token_estimate(),
            future("x".repeat(400_000)).replay_only_token_estimate()
        );
    }

    #[test]
    fn canonical_replay_estimate_matches_its_message_projection() {
        let replay = ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "function_call",
                "id": "fc_native",
                "call_id": "call_native",
                "name": "command",
                "arguments": "{\"id\":\"user-owned\",\"command\":\"pwd\"}",
                "status": "completed"
            })],
        );
        assert_eq!(
            replay
                .canonical_token_estimate()
                .expect("canonical estimate"),
            estimate_message_tokens(&replay.to_message().expect("canonical message"))
        );
    }
}
