//! Side-effect-free destination projection, shared by dispatch and management.
//! The original ledger is never mutated. Tool ownership is occurrence-based,
//! not a cumulative last-writer-wins map of provider handles.

use crate::protocol::message_to_openai_input;
use rig_core::message::{AssistantContent, Message, Text, ToolCallId, UserContent};
use std::collections::{BTreeSet, HashSet};
use zevria_foundation::ModelProfileRef;
use zevria_model::ModelRequestItem;
use zevria_model::ProviderReplay;
use zevria_model::compaction::replay::ToolCorrelations;
use zevria_model::models::ReplayPreflight;

#[derive(Debug, Clone, Copy)]
pub enum ReplaySource {
    Native,
    PortableAssistant,
    GenericAssistant,
    None,
}
impl ReplaySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::PortableAssistant => "portable_assistant",
            Self::GenericAssistant => "generic_assistant",
            Self::None => "none",
        }
    }
}

#[derive(Debug)]
struct Call {
    output: Option<String>,
    native: bool,
}

#[derive(Default)]
struct Projection {
    input: Vec<serde_json::Value>,
    calls: ToolCorrelations<Call>,
    native_ids: HashSet<String>,
    reserved_native_ids: HashSet<String>,
    used_native: bool,
    used_portable: bool,
    used_untagged: bool,
}

impl Projection {
    fn call(
        &mut self,
        handles: BTreeSet<String>,
        native_id: Option<String>,
        omitted: bool,
    ) -> anyhow::Result<Option<String>> {
        if let Some(id) = &native_id {
            anyhow::ensure!(
                self.native_ids.insert(id.clone()),
                "reused native tool call ID {id:?} cannot be replayed safely"
            );
        }
        let native = native_id.is_some();
        let output = if omitted {
            None
        } else {
            Some(native_id.unwrap_or_else(|| {
                let base = format!("zevria_call_{:06}", self.calls.len());
                let mut id = base.clone();
                let mut suffix = 0_u64;
                while self.reserved_native_ids.contains(&id) {
                    suffix += 1;
                    id = format!("{base}_{suffix}");
                }
                id
            }))
        };
        self.calls.call(
            handles,
            Call {
                output: output.clone(),
                native,
            },
        )?;
        Ok(output)
    }

    fn result(&mut self, handles: &BTreeSet<String>) -> anyhow::Result<&Call> {
        self.calls.result(handles)
    }

    fn replay(&mut self, replay: &ProviderReplay, target: &ModelProfileRef) -> anyhow::Result<()> {
        // Decode known shapes even for native replay; equality is not permission
        // to transmit malformed known records. Unknown fields remain raw.
        replay.portable_projection()?;
        let native = replay.native_items_for(target)?.is_some();
        self.used_native |= native;
        self.used_portable |= !native;
        for item in &replay.items {
            let kind = item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let handles = ["id", "call_id"]
                .into_iter()
                .filter_map(|key| item.get(key)?.as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            anyhow::ensure!(
                !matches!(kind, "function_call" | "custom_tool_call") || !handles.is_empty(),
                "historical tool call has no correlation handle"
            );
            if kind == "function_call" {
                let id = item["call_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("function call has no call_id"))?;
                let output = self
                    .call(handles, native.then(|| id.to_string()), false)?
                    .expect("supported call");
                if !native {
                    let single = ProviderReplay::openai_responses(
                        replay.source_profile.clone(),
                        vec![item.clone()],
                    );
                    let mut message = single
                        .portable_projection()?
                        .message
                        .expect("function projection");
                    if let Message::Assistant { content, .. } = &mut message {
                        for block in content {
                            if let AssistantContent::ToolCall(call) = block {
                                call.id =
                                    ToolCallId::new(output.clone()).expect("nonempty neutral ID");
                                call.provider = None;
                            }
                        }
                    }
                    self.input.extend(message_to_openai_input(message)?);
                }
            } else if matches!(kind, "function_call_output" | "custom_tool_call_output") {
                anyhow::ensure!(item.get("output").is_some(), "malformed native tool output");
                let call = self.result(&handles)?;
                anyhow::ensure!(
                    !native || call.native,
                    "native replay output refers to a portable call"
                );
                if !native && let Some(id) = call.output.clone() {
                    self.input.push(serde_json::json!({"type":"function_call_output", "call_id":id, "output":item["output"]}));
                }
            } else if kind.ends_with("_call") && !handles.is_empty() {
                self.call(
                    handles,
                    native.then(|| {
                        item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap()
                            .to_owned()
                    }),
                    !native,
                )?;
            } else if !native {
                let single = ProviderReplay::openai_responses(
                    replay.source_profile.clone(),
                    vec![item.clone()],
                );
                if let Some(message) = single.portable_projection()?.message {
                    self.input.extend(message_to_openai_input(message)?);
                }
            }
            if native {
                self.input.push(item.clone());
            }
        }
        Ok(())
    }

    fn message(&mut self, message: &Message) -> anyhow::Result<()> {
        let message = match message {
            Message::System { .. } => anyhow::bail!("raw system messages must be typed directives"),
            Message::Assistant { content, .. } => {
                self.used_untagged = true;
                let mut portable = Vec::new();
                for block in content {
                    match block {
                        AssistantContent::Text(text) => portable.push(AssistantContent::Text(
                            Text::new(zevria_content::citations::render_text(text)),
                        )),
                        AssistantContent::ToolCall(call) => {
                            let mut handles = BTreeSet::from([call.id.to_string()]);
                            if let Some(provider) = &call.provider {
                                handles.insert(provider.call_id.clone());
                                handles.extend(provider.item_id.iter().cloned());
                            }
                            let id = self.call(handles, None, false)?.expect("supported call");
                            let mut call = call.clone();
                            call.id = ToolCallId::new(id).expect("nonempty neutral ID");
                            call.provider = None;
                            call.signature = None;
                            call.additional_params = None;
                            portable.push(AssistantContent::ToolCall(call));
                        }
                        AssistantContent::Reasoning(_) => {}
                        _ => anyhow::bail!(
                            "unsupported untagged assistant content cannot be projected safely"
                        ),
                    }
                }
                if portable.is_empty() {
                    return Ok(());
                }
                Message::Assistant {
                    id: None,
                    content: portable,
                }
            }
            Message::User { content } => {
                let mut portable = Vec::new();
                for block in content {
                    let UserContent::ToolResult(result) = block else {
                        portable.push(block.clone());
                        continue;
                    };
                    let mut handles = BTreeSet::from([result.call.to_string()]);
                    if let Some(provider) = &result.provider {
                        handles.insert(provider.call_id.clone());
                        handles.extend(provider.item_id.iter().cloned());
                    }
                    let call = self.result(&handles)?;
                    if let Some(output) = &call.output {
                        let mut result = result.clone();
                        result.call =
                            ToolCallId::new(output.clone()).expect("nonempty correlation ID");
                        result.provider = None;
                        portable.push(UserContent::ToolResult(result));
                    }
                }
                if portable.is_empty() {
                    return Ok(());
                }
                Message::User { content: portable }
            }
        };
        self.input.extend(message_to_openai_input(message)?);
        Ok(())
    }
}

pub fn project(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
) -> anyhow::Result<(Vec<serde_json::Value>, ReplaySource)> {
    project_with_compatibility(input, target, true)
}
pub fn preflight(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
) -> anyhow::Result<ReplayPreflight> {
    preflight_with_compatibility(input, target, true)
}

/// Opaque compatibility is a structured result, not a plain-message fallback.
pub fn preflight_with_compatibility(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    developer_messages: bool,
) -> anyhow::Result<ReplayPreflight> {
    let sources = input
        .iter()
        .filter_map(|item| match item {
            ModelRequestItem::ReplayOnly(replay) if &replay.source_profile != target => {
                Some(replay.source_profile.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    // Validate every known record before reporting recoverable incompatibility.
    for item in input {
        if let Some(replay) = item.replay_ref() {
            replay.portable_projection()?;
        }
    }
    if !sources.is_empty() {
        return Ok(ReplayPreflight::ConversionRequired {
            sources: sources.into_iter().collect(),
        });
    }
    let (projected, _) = project_with_compatibility(input, target, developer_messages)?;
    let tokens = zevria_model::compaction::estimate_responses_input_tokens(&projected, 0);
    Ok(ReplayPreflight::Compatible(
        zevria_model::ContextTokenEstimate::new(tokens, tokens),
    ))
}

pub fn project_with_compatibility(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    developer_messages: bool,
) -> anyhow::Result<(Vec<serde_json::Value>, ReplaySource)> {
    let mut projection = Projection::default();
    // Reserve future native wire handles before minting neutral ones. A
    // configured provider may itself emit IDs using Zevria's neutral prefix.
    for replay in input.iter().filter_map(|item| item.replay_ref()) {
        if let Some(items) = replay.native_items_for(target)? {
            projection.reserved_native_ids.extend(
                items
                    .iter()
                    .filter_map(|item| item.get("call_id")?.as_str().map(str::to_owned)),
            );
        }
    }
    for item in input {
        match item {
            ModelRequestItem::RequestInstruction(directive) => {
                directive.validate()?;
                projection.input.push(serde_json::json!({"type": "message", "role": if developer_messages { "developer" } else { "user" }, "content": [{"type": "input_text", "text": directive.render()}]}));
            }
            ModelRequestItem::DeveloperInstruction(directive) => {
                directive.validate()?;
                projection.input.push(serde_json::json!({"type": "message", "role": if developer_messages { "developer" } else { "user" }, "content": [{"type": "input_text", "text": directive.text}]}));
            }
            ModelRequestItem::ReplayOnly(replay) => {
                anyhow::ensure!(
                    replay.native_items_for(target)?.is_some(),
                    "cannot send replay-only openai.responses history from source profile {} to foreign target profile {target}: conversion required; select its source with /model, confirm a portable summary, or start a fresh session",
                    replay.source_profile
                );
                projection.replay(replay, target)?;
            }
            ModelRequestItem::ReplayBacked(content) => {
                projection.replay(content.replay(), target)?
            }
            ModelRequestItem::Message(message) => projection.message(message)?,
        }
    }
    let source = if projection.used_portable {
        ReplaySource::PortableAssistant
    } else if projection.used_untagged {
        ReplaySource::GenericAssistant
    } else if projection.used_native {
        ReplaySource::Native
    } else {
        ReplaySource::None
    };
    Ok((projection.input, source))
}
