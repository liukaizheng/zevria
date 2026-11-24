//! Ordered, destination-aware Messages projection. Native ledgers are never
//! mutated. All foreign reasoning is omitted and call IDs are re-correlated.
use crate::binding::{PrefixBinding, request_binding};
use rig_core::message::{
    AssistantContent, DocumentData, DocumentSourceKind, Image, Message, MimeType,
    ToolResultContent, UserContent,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use zevria_foundation::ModelProfileRef;
use zevria_model::{
    ModelRequestItem, ProviderReplay, ReplayProtocol, compaction::replay::ToolCorrelations,
    models::ReplayPreflight,
};

#[derive(Debug)]
pub struct ProjectedMessages {
    pub messages: Vec<Value>,
    pub prefix_binding: String,
    pub replay_source: &'static str,
    pub stripped_thinking_turns: usize,
}

/// Validated, destination-specific history before request-prefix binding.
/// Management estimates bind it to an empty prefix; completion must bind it to
/// its actual system/tools so signed thinking is never reused under the wrong
/// prefix. Immutable history can be shared by admission, routing and body build.
pub struct ValidatedHistory {
    projection: HistoryProjection,
    target: ModelProfileRef,
    compatibility: crate::Compatibility,
    input_count: usize,
    input_digest: [u8; 32],
}

#[derive(Clone)]
struct Segment {
    blocks: Vec<Value>,
    binding: Option<String>,
    native: bool,
}
#[derive(Clone)]
struct Turn {
    role: &'static str,
    segments: Vec<Segment>,
}
#[derive(Clone)]
struct HistoryProjection {
    turns: Vec<Turn>,
    accounting_media: zevria_model::ContextTokenEstimate,
    deferred_media: bool,
    native: bool,
    portable: bool,
    generic: bool,
}

#[derive(Default)]
struct Projection {
    turns: Vec<Turn>,
    accounting: bool,
    deferred_media: bool,
    image_policy: Option<zevria_foundation::image_accounting::ImageAccountingPolicy>,
    accounting_media: zevria_model::ContextTokenEstimate,
    directives: Vec<Value>,
    calls: ToolCorrelations<String>,
    reserved: HashSet<String>,
    ids: HashSet<String>,
    native: bool,
    portable: bool,
    generic: bool,
}
impl Projection {
    fn call(&mut self, handles: &[Option<&str>], native: Option<&str>) -> anyhow::Result<String> {
        let id = if let Some(id) = native {
            anyhow::ensure!(
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid native Anthropic tool_use ID"
            );
            anyhow::ensure!(
                self.ids.insert(id.to_owned()),
                "reused native tool_use ID {id:?}"
            );
            id.to_owned()
        } else {
            let mut id = format!("zevria_call_{:06}", self.calls.len());
            while self.reserved.contains(&id) || self.ids.contains(&id) {
                id.push('_');
            }
            self.ids.insert(id.clone());
            id
        };
        self.calls.call(handles, id.clone())?;
        Ok(id)
    }
    fn push(&mut self, role: &'static str, segment: Segment) {
        if segment.blocks.is_empty() {
            return;
        }
        if let Some(last) = self.turns.last_mut().filter(|last| last.role == role) {
            last.segments.push(segment);
        } else {
            self.turns.push(Turn {
                role,
                segments: vec![segment],
            });
        }
    }
    fn blocks(&mut self, role: &'static str, blocks: Vec<Value>) {
        self.push(
            role,
            Segment {
                blocks,
                binding: None,
                native: false,
            },
        );
    }
    fn flush_directives(&mut self, system_messages: bool) -> anyhow::Result<()> {
        if self.directives.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            self.turns.last().is_some_and(|turn| turn.role == "user"),
            "ordered directives require an adjacent user turn"
        );
        let blocks = std::mem::take(&mut self.directives);
        self.blocks(if system_messages { "system" } else { "user" }, blocks);
        Ok(())
    }
    fn replay(
        &mut self,
        replay: &ProviderReplay,
        target: &ModelProfileRef,
        system_messages: bool,
    ) -> anyhow::Result<()> {
        self.flush_directives(system_messages)?;
        if replay
            .native_items_for(target, ReplayProtocol::Anthropic)?
            .is_some()
        {
            replay.validate_known_items()?;
            self.native = true;
            for (handles, _) in replay.tool_call_handles() {
                let id = handles[0].expect("validated tool_use");
                self.call(&handles, Some(id))?;
            }
            self.push(
                "assistant",
                Segment {
                    blocks: replay.items.clone(),
                    binding: replay.prefix_binding.clone(),
                    native: true,
                },
            );
        } else {
            let portable = replay.portable_projection()?;
            self.portable = true;
            let aliases = replay.tool_call_handles().collect::<Vec<_>>();
            let mut blocks = Vec::new();
            if let Some(Message::Assistant(assistant)) = portable.message {
                for block in assistant.content {
                    match block {
                        AssistantContent::Text(text) if !text.text.is_empty() => {
                            blocks.push(json!({"type":"text", "text":text.text}))
                        }
                        AssistantContent::ToolCall(call) => {
                            let handles = aliases
                                .iter()
                                .find(|(handles, _)| {
                                    handles.iter().flatten().any(|&handle| {
                                        portable
                                            .tool_correlations
                                            .get(handle)
                                            .is_some_and(|id| id == call.id.wire().as_ref())
                                    })
                                })
                                .ok_or_else(|| anyhow::anyhow!("missing portable tool handles"))?
                                .0;
                            let id = self.call(&handles, None)?;
                            blocks.push(tool_use(&call, id));
                        }
                        _ => {}
                    }
                }
            }
            self.blocks("assistant", blocks);
        }
        Ok(())
    }
    fn image(&mut self, image: &Image) -> anyhow::Result<Value> {
        let value = image_input_with_accounting(image, self.accounting)?;
        self.deferred_media |= self.accounting;
        if let Some(policy) = &self.image_policy {
            self.accounting_media = self.accounting_media.saturating_add(
                zevria_model::media_accounting::canonical_image(image, policy),
            );
        }
        Ok(value)
    }
    fn message(&mut self, message: &Message, system_messages: bool) -> anyhow::Result<()> {
        match message {
            Message::System { .. } => anyhow::bail!("raw system messages must be typed directives"),
            Message::Assistant(assistant) => {
                self.flush_directives(system_messages)?;
                self.generic = true;
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) if !text.text.is_empty() => blocks.push(json!({"type":"text", "text":zevria_content::citations::render_text(text)})),
                        AssistantContent::Text(_) | AssistantContent::Reasoning(_) => {},
                        AssistantContent::ToolCall(call) => {
                            let wire = call.id.wire();
                            let [id, call_id] = call.native.as_ref().map_or([None; 2], |native| {
                                zevria_model::provider_replay::responses_tool_handles(&native.item)
                            });
                            let id = self.call(&[Some(&wire), id, call_id], None)?;
                            blocks.push(tool_use(call, id));
                        }
                        _ => anyhow::bail!("unsupported Anthropic assistant content"),
                    }
                }
                self.blocks("assistant", blocks);
            }
            Message::User { content } => {
                let mut blocks = Vec::new();
                for block in content {
                    match block {
                        UserContent::Text(text) if !text.text.is_empty() => blocks.push(json!({"type":"text", "text":text.text})),
                        UserContent::Text(_) => {},
                        UserContent::Image(image) => blocks.push(self.image(image)?),
                        UserContent::Document(document) => blocks.push(match &document.data {
                            DocumentData::Text(text) => json!({"type":"text", "text":text}),
                            DocumentData::File(DocumentSourceKind::Base64(data)) if document.media_type.as_ref().is_none_or(|media| media.to_mime_type() == "application/pdf") => {
                                self.deferred_media |= self.accounting;
                                json!({"type":"document", "source":{"type":"base64", "media_type":"application/pdf", "data":if self.accounting { "" } else { data.as_str() }}})
                            }
                            _ => anyhow::bail!("unsupported Anthropic document source (expected base64 PDF)"),
                        }),
                        UserContent::ToolResult(result) => {
                            let id = self.calls.result(&[Some(&result.call.wire())])?.clone();
                            let content = result.content.iter().map(|block| match block {
                                ToolResultContent::Text(text) => Ok(json!({"type":"text", "text":text.text})),
                                ToolResultContent::Json { value } => Ok(json!({"type":"text", "text":value.to_string()})),
                                ToolResultContent::Image(image) => self.image(image),
                            }).collect::<anyhow::Result<Vec<_>>>()?;
                            blocks.push(json!({"type":"tool_result", "tool_use_id":id, "content":content, "is_error":result.is_error}));
                        }
                        _ => anyhow::bail!("unsupported Anthropic user content"),
                    }
                }
                self.blocks("user", blocks);
            }
        }
        Ok(())
    }
}

fn tool_use(call: &rig_core::message::ToolCall, id: String) -> Value {
    let input = call.function.arguments_value();
    json!({"type":"tool_use", "id":id, "name":call.function.name, "input":if call.function.invalid_arguments.is_none() && input.is_object() { input } else { json!({}) }})
}
fn image_input_with_accounting(image: &Image, accounting: bool) -> anyhow::Result<Value> {
    let DocumentSourceKind::Base64(data) = &image.data else {
        anyhow::bail!("Anthropic images require base64 data");
    };
    let media = image
        .media_type
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("base64 images require a media type"))?
        .to_mime_type();
    anyhow::ensure!(
        matches!(
            media,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ),
        "unsupported Anthropic image media type {media}"
    );
    Ok(
        json!({"type":"image", "source":{"type":"base64", "media_type":media, "data":if accounting { "" } else { data.as_str() }}}),
    )
}
pub fn is_thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

pub fn project(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    system_messages: bool,
    system: &Value,
    tools: &Value,
    strip_all_thinking: bool,
) -> anyhow::Result<ProjectedMessages> {
    project_with_compatibility(
        input,
        target,
        &crate::Compatibility {
            system_messages,
            ..Default::default()
        },
        system,
        tools,
        strip_all_thinking,
    )
}

pub fn project_with_compatibility(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    system: &Value,
    tools: &Value,
    strip_all_thinking: bool,
) -> anyhow::Result<ProjectedMessages> {
    project_impl(
        input,
        target,
        compatibility,
        system,
        tools,
        strip_all_thinking,
        true,
    )
}

fn project_impl(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    system: &Value,
    tools: &Value,
    strip_all_thinking: bool,
    completion: bool,
) -> anyhow::Result<ProjectedMessages> {
    bind_projection(
        &project_history(input, target, compatibility)?,
        system,
        tools,
        strip_all_thinking,
        completion,
        None,
    )
}

fn project_history(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
) -> anyhow::Result<HistoryProjection> {
    project_history_accounting(input, target, compatibility, None, 0, Default::default())
}
fn project_history_accounting(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    policy: Option<&zevria_foundation::image_accounting::ImageAccountingPolicy>,
    measured_prefix: usize,
    prefix_media: zevria_model::ContextTokenEstimate,
) -> anyhow::Result<HistoryProjection> {
    let system_messages = compatibility.system_messages;
    let mut projection = Projection {
        accounting: policy.is_some(),
        accounting_media: prefix_media,
        ..Default::default()
    };
    for replay in input.iter().filter_map(|item| item.replay_ref()) {
        if replay
            .native_items_for(target, ReplayProtocol::Anthropic)?
            .is_some()
        {
            projection.reserved.extend(
                replay
                    .tool_call_handles()
                    .flat_map(|(handles, _)| handles.into_iter().flatten())
                    .map(str::to_owned),
            );
        }
    }
    for (index, item) in input.iter().enumerate() {
        projection.image_policy = policy.filter(|_| index >= measured_prefix).cloned();
        match item {
            ModelRequestItem::AdditionalTools(tools) => {
                tools.validate()?;
                anyhow::ensure!(
                    !tools.is_empty(),
                    "additional tool declarations must not be empty"
                );
                if compatibility.inline_tools && system_messages {
                    projection.directives.extend(tools.iter().map(|tool| json!({"type":"tool_addition", "tool":{"type":"tool_definition", "definition":crate::body::tool_definition(tool, compatibility.eager_tool_input_streaming)}})));
                }
            }
            ModelRequestItem::DeveloperInstruction(directive) => {
                directive.validate()?;
                projection
                    .directives
                    .push(json!({"type":"text", "text":directive.text}));
            }
            ModelRequestItem::RequestInstruction(directive) => {
                directive.validate()?;
                projection
                    .directives
                    .push(json!({"type":"text", "text":directive.render()}));
            }
            ModelRequestItem::Message(message) => projection.message(message, system_messages)?,
            ModelRequestItem::ReplayBacked(content) => {
                projection.replay(content.replay(), target, system_messages)?
            }
            ModelRequestItem::ReplayOnly(replay) => {
                anyhow::ensure!(
                    replay
                        .native_items_for(target, ReplayProtocol::Anthropic)?
                        .is_some(),
                    "cannot send replay-only {} history from source profile {} to foreign target profile {target}: conversion required",
                    replay.provider,
                    replay.source_profile
                );
                projection.replay(replay, target, system_messages)?;
            }
        }
    }
    projection.flush_directives(system_messages)?;
    Ok(HistoryProjection {
        accounting_media: projection.accounting_media,
        deferred_media: projection.deferred_media,
        turns: projection.turns,
        native: projection.native,
        portable: projection.portable,
        generic: projection.generic,
    })
}

impl ValidatedHistory {
    pub fn new(
        input: &[ModelRequestItem<'_>],
        target: &ModelProfileRef,
        compatibility: &crate::Compatibility,
        identity: Option<&zevria_model::ModelInputIdentity>,
    ) -> anyhow::Result<Self> {
        Self::from_projection(
            project_history(input, target, compatibility)?,
            input,
            target,
            compatibility,
            identity,
        )
    }

    fn from_projection(
        projection: HistoryProjection,
        input: &[ModelRequestItem<'_>],
        target: &ModelProfileRef,
        compatibility: &crate::Compatibility,
        identity: Option<&zevria_model::ModelInputIdentity>,
    ) -> anyhow::Result<Self> {
        let fallback;
        let identity = match identity {
            Some(identity) if identity.len() == input.len() => identity,
            _ => {
                fallback = zevria_model::ModelInputIdentity::from_items(input)?;
                &fallback
            }
        };
        Ok(Self {
            projection,
            target: target.clone(),
            compatibility: compatibility.clone(),
            input_count: identity.len(),
            input_digest: identity.digest(),
        })
    }

    pub fn matches(
        &self,
        input: &[ModelRequestItem<'_>],
        target: &ModelProfileRef,
        compatibility: &crate::Compatibility,
        identity: Option<&zevria_model::ModelInputIdentity>,
    ) -> anyhow::Result<bool> {
        if &self.target != target
            || &self.compatibility != compatibility
            || self.input_count != input.len()
        {
            return Ok(false);
        }
        let fallback;
        let identity = match identity {
            Some(identity) if identity.len() == input.len() => identity,
            _ => {
                fallback = zevria_model::ModelInputIdentity::from_items(input)?;
                &fallback
            }
        };
        Ok(self.input_digest == identity.digest())
    }

    pub fn completion(
        &self,
        system: &Value,
        tools: &Value,
        strip_all_thinking: bool,
        input: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<ProjectedMessages> {
        bind_projection(
            &self.projection,
            system,
            tools,
            strip_all_thinking,
            true,
            self.projection.deferred_media.then_some(input),
        )
    }
}

// Accounting projects the same user blocks in the same order, without their
// base64 payloads. Restore them only when binding the actual request.
fn user_payloads<'a>(input: &'a [ModelRequestItem<'a>]) -> impl Iterator<Item = &'a str> {
    input
        .iter()
        .filter_map(|item| match item.message_ref() {
            Some(Message::User { content }) => Some(content),
            _ => None,
        })
        .flatten()
        .flat_map(|block| {
            let base64 = match block {
                UserContent::Image(image) => match &image.data {
                    DocumentSourceKind::Base64(data) => Some(data.as_str()),
                    _ => None,
                },
                UserContent::Document(document) => match &document.data {
                    DocumentData::File(DocumentSourceKind::Base64(data)) => Some(data.as_str()),
                    _ => None,
                },
                _ => None,
            };
            let results = match block {
                UserContent::ToolResult(result) => Some(&result.content),
                _ => None,
            };
            base64
                .into_iter()
                .chain(
                    results
                        .into_iter()
                        .flatten()
                        .filter_map(|block| match block {
                            ToolResultContent::Image(Image {
                                data: DocumentSourceKind::Base64(data),
                                ..
                            }) => Some(data.as_str()),
                            _ => None,
                        }),
                )
        })
}

fn wire_message(role: &str, blocks: Vec<Value>) -> Value {
    let mut message = json!({"role": role});
    message["content"] = Value::Array(blocks);
    message
}

fn bind_projection(
    projection: &HistoryProjection,
    system: &Value,
    tools: &Value,
    strip_all_thinking: bool,
    completion: bool,
    transport_input: Option<&[ModelRequestItem<'_>]>,
) -> anyhow::Result<ProjectedMessages> {
    let mut messages: Vec<Value> = Vec::new();
    let mut binding = PrefixBinding::new(system, tools);
    let mut prev_binding = binding.clone();
    let mut before_directive = binding.clone();
    let mut payloads = transport_input.map(user_payloads);
    let mut stripped = 0;
    let mut pending_system = Vec::new();
    for turn in &projection.turns {
        if turn.role == "system" {
            pending_system.extend(
                turn.segments
                    .iter()
                    .flat_map(|segment| &segment.blocks)
                    .cloned(),
            );
            continue;
        }
        let staged_directive = turn.role == "assistant" && !pending_system.is_empty();
        if staged_directive {
            let directive = wire_message("system", std::mem::take(&mut pending_system));
            // A discarded assistant can undo this push without changing the
            // checkpoint used to merge into the previous committed message.
            before_directive.clone_from(&binding);
            binding.push(&directive);
            messages.push(directive);
        }
        let mut blocks = Vec::new();
        for (index, segment) in turn.segments.iter().enumerate() {
            let keep = !strip_all_thinking
                && index == 0
                && segment
                    .binding
                    .as_deref()
                    .is_some_and(|prefix| binding.finish() == prefix);
            if segment.native && !keep && segment.blocks.iter().any(is_thinking) {
                stripped += 1;
            }
            blocks.extend(
                segment
                    .blocks
                    .iter()
                    .filter(|block| {
                        if segment.native && !keep {
                            matches!(block["type"].as_str(), Some("text" | "tool_use"))
                        } else {
                            !is_thinking(block) || keep
                        }
                    })
                    .cloned(),
            );
        }
        if blocks.is_empty() {
            // A thinking-only assistant can disappear under the guard. Its
            // preceding system turn must move past the next user, not become
            // an illegal system -> user sequence (nor change later bindings).
            if staged_directive {
                let mut directive = messages.pop().expect("staged system message");
                pending_system.append(directive["content"].as_array_mut().expect("content"));
                binding.clone_from(&before_directive);
            }
            continue;
        }
        if turn.role == "user" {
            if let Some(payloads) = &mut payloads {
                let mut restore = |block: &mut Value| -> anyhow::Result<()> {
                    let data = payloads
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("missing Anthropic media payload"))?;
                    block["source"]["data"] = data.into();
                    Ok(())
                };
                for block in &mut blocks {
                    match block["type"].as_str() {
                        Some("image" | "document") => restore(block)?,
                        Some("tool_result") => {
                            for block in block["content"]
                                .as_array_mut()
                                .expect("tool result content")
                            {
                                if block["type"] == "image" {
                                    restore(block)?;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            blocks.sort_by_key(|block| block["type"] != "tool_result");
        }
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == turn.role) {
            let content = last["content"].as_array_mut().expect("content");
            content.append(&mut blocks);
            if turn.role == "user" {
                content.sort_by_key(|block| block["type"] != "tool_result");
            }
            binding.clone_from(&prev_binding);
            binding.push(last);
        } else {
            let message = wire_message(turn.role, blocks);
            prev_binding.clone_from(&binding);
            binding.push(&message);
            messages.push(message);
        }
    }
    anyhow::ensure!(
        payloads.is_none_or(|mut payloads| payloads.next().is_none()),
        "unused Anthropic media payload"
    );
    if !pending_system.is_empty() {
        let directive = wire_message("system", pending_system);
        binding.push(&directive);
        messages.push(directive);
    }
    validate_messages_for(&messages, completion)?;
    Ok(ProjectedMessages {
        prefix_binding: binding.finish(),
        messages,
        replay_source: if projection.portable {
            "portable_assistant"
        } else if projection.generic {
            "generic_assistant"
        } else if projection.native {
            "native"
        } else {
            "none"
        },
        stripped_thinking_turns: stripped,
    })
}

pub fn validate_messages(messages: &[Value]) -> anyhow::Result<()> {
    validate_messages_for(messages, true)
}
fn validate_messages_for(messages: &[Value], completion: bool) -> anyhow::Result<()> {
    if !completion && messages.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        messages.first().is_some_and(|m| m["role"] == "user"),
        "Anthropic conversation must start with a user message"
    );
    anyhow::ensure!(
        !completion || messages.last().is_some_and(|m| m["role"] != "assistant"),
        "Anthropic conversation must not end with an assistant message"
    );
    let mut expected = BTreeSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message["role"] == "system" {
            anyhow::ensure!(
                index > 0
                    && messages[index - 1]["role"] == "user"
                    && messages
                        .get(index + 1)
                        .is_none_or(|next| next["role"] == "assistant"),
                "Anthropic system messages must follow a user and be last or precede an assistant"
            );
            continue;
        }
        anyhow::ensure!(
            matches!(message["role"].as_str(), Some("user" | "assistant")),
            "invalid Anthropic message role"
        );
        let blocks = message["content"].as_array().expect("content array");
        if message["role"] == "user" {
            let results = blocks
                .iter()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| b["tool_use_id"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>();
            anyhow::ensure!(
                results.len() == expected.len()
                    && results.iter().collect::<BTreeSet<_>>().len() == results.len()
                    && results.iter().all(|id| expected.contains(id)),
                "every tool_use requires exactly one result in the next user message"
            );
            expected.clear();
        } else {
            for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
                anyhow::ensure!(
                    expected.insert(block["id"].as_str().unwrap_or_default().to_owned()),
                    "duplicate tool_use ID"
                );
            }
        }
    }
    anyhow::ensure!(expected.is_empty(), "historical tool_use has no result");
    Ok(())
}

pub fn preflight(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    system_messages: bool,
) -> anyhow::Result<ReplayPreflight> {
    preflight_with_compatibility(
        input,
        target,
        &crate::Compatibility {
            system_messages,
            ..Default::default()
        },
    )
}

pub fn preflight_with_compatibility(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
) -> anyhow::Result<ReplayPreflight> {
    assess_history_with_policy(
        input,
        target,
        compatibility,
        &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
        None,
    )
}

/// Preserve management conversion/error precedence while retaining the same
/// unbound validated history for subsequent completion/count binding.
pub fn prepare_history(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    identity: Option<&zevria_model::ModelInputIdentity>,
) -> anyhow::Result<(ReplayPreflight, Option<ValidatedHistory>)> {
    prepare_history_with_policy(
        input,
        target,
        compatibility,
        identity,
        &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
    )
}
pub fn prepare_history_with_policy(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    identity: Option<&zevria_model::ModelInputIdentity>,
    policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
) -> anyhow::Result<(ReplayPreflight, Option<ValidatedHistory>)> {
    assess_and_prepare_history(input, target, compatibility, identity, policy, None)
}

/// Keep the exact unbound projection used for admission; only media bytes
/// are deferred until actual request serialization.
pub fn assess_and_prepare_history(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    identity: Option<&zevria_model::ModelInputIdentity>,
    policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
    measured_prefix: Option<(usize, zevria_model::ContextTokenEstimate)>,
) -> anyhow::Result<(ReplayPreflight, Option<ValidatedHistory>)> {
    let (assessment, projection) =
        assess_projection(input, target, compatibility, policy, measured_prefix)?;
    let history = projection
        .map(|projection| {
            ValidatedHistory::from_projection(projection, input, target, compatibility, identity)
        })
        .transpose()?;
    Ok((assessment, history))
}

/// Pure management/admission projection. Images/PDF bytes are substituted BEFORE
/// projection allocation; actual transport history is prepared only at dispatch.
/// Trusted prefix accounting avoids re-hashing/re-parsing historical attachments.
pub fn assess_history_with_policy(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
    measured_prefix: Option<(usize, zevria_model::ContextTokenEstimate)>,
) -> anyhow::Result<ReplayPreflight> {
    assess_projection(input, target, compatibility, policy, measured_prefix)
        .map(|(assessment, _)| assessment)
}

fn assess_projection(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    compatibility: &crate::Compatibility,
    policy: &zevria_foundation::image_accounting::ImageAccountingPolicy,
    measured_prefix: Option<(usize, zevria_model::ContextTokenEstimate)>,
) -> anyhow::Result<(ReplayPreflight, Option<HistoryProjection>)> {
    policy.validate(true)?;
    let sources = input
        .iter()
        .filter_map(|item| match item {
            ModelRequestItem::ReplayOnly(replay)
                if replay.source_profile != *target
                    || replay.protocol() != Some(ReplayProtocol::Anthropic) =>
            {
                Some(replay.source_profile.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if !sources.is_empty() {
        // Even incompatible histories must not mask malformed known records.
        for replay in input.iter().filter_map(|item| item.replay_ref()) {
            replay.validate_known_items()?;
        }
        return Ok((
            ReplayPreflight::ConversionRequired {
                sources: sources.into_iter().collect(),
            },
            None,
        ));
    }
    let (prefix, media) = measured_prefix.unwrap_or_default();
    let projection =
        project_history_accounting(input, target, compatibility, Some(policy), prefix, media)?;
    let media = projection.accounting_media;
    let projected = bind_projection(&projection, &json!([]), &json!([]), false, false, None)?;
    let mut estimate = zevria_model::media_accounting::anthropic_input(
        &projected.messages,
        0,
        &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
    );
    let files = estimate.unresolved_media.files;
    let opaque_items = estimate.unresolved_media.opaque_items;
    estimate.media_tokens = media.media_tokens;
    estimate.image_count = media.image_count;
    estimate.max_image_edge = media.max_image_edge;
    estimate.unresolved_media = media.unresolved_media;
    estimate.unresolved_media.files = files;
    estimate.unresolved_media.opaque_items = opaque_items;
    if !(compatibility.inline_tools && compatibility.system_messages) {
        for item in input {
            if let ModelRequestItem::AdditionalTools(tools) = item {
                let rendered = tools
                    .iter()
                    .map(|tool| {
                        crate::body::tool_definition(tool, compatibility.eager_tool_input_streaming)
                    })
                    .collect::<Vec<_>>();
                let tokens = zevria_model::compaction::approximate_tokens_from_bytes(
                    serde_json::to_vec(&rendered)?.len(),
                );
                estimate = estimate
                    .saturating_add(zevria_model::ContextTokenEstimate::new(tokens, tokens));
            }
        }
    }
    Ok((
        ReplayPreflight::Compatible(estimate.enforce_request_constraints(policy)),
        Some(projection),
    ))
}

pub fn strip_thinking(body: &mut Value) -> String {
    if let Some(messages) = body["messages"].as_array_mut() {
        let mut normalized: Vec<Value> = Vec::new();
        let mut directives = Vec::new();
        for mut message in std::mem::take(messages) {
            let blocks = message["content"].as_array_mut().expect("content array");
            blocks.retain(|block| !is_thinking(block));
            if blocks.is_empty() {
                continue;
            }
            if message["role"] == "system" {
                directives.extend(message["content"].as_array().unwrap().iter().cloned());
                continue;
            }
            if message["role"] == "assistant" && !directives.is_empty() {
                normalized
                    .push(json!({"role":"system", "content":std::mem::take(&mut directives)}));
            }
            if let Some(last) = normalized
                .last_mut()
                .filter(|last| last["role"] == message["role"])
            {
                let blocks = last["content"].as_array_mut().unwrap();
                blocks.extend(message["content"].as_array().unwrap().iter().cloned());
                if message["role"] == "user" {
                    blocks.sort_by_key(|block| block["type"] != "tool_result");
                }
            } else {
                normalized.push(message);
            }
        }
        if !directives.is_empty() {
            normalized.push(json!({"role":"system", "content":directives}));
        }
        *messages = normalized;
    }
    request_binding(
        &body["system"],
        &body["tools"],
        body["messages"].as_array().unwrap(),
    )
}
