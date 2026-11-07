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

struct Segment {
    blocks: Vec<Value>,
    binding: Option<String>,
    native: bool,
}
struct Turn {
    role: &'static str,
    segments: Vec<Segment>,
}
#[derive(Default)]
struct Projection {
    turns: Vec<Turn>,
    directives: Vec<Value>,
    calls: ToolCorrelations<String>,
    reserved: HashSet<String>,
    ids: HashSet<String>,
    native: bool,
    portable: bool,
    generic: bool,
}
impl Projection {
    fn call(&mut self, handles: BTreeSet<String>, native: Option<&str>) -> anyhow::Result<String> {
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
        let portable = replay.portable_projection()?;
        self.flush_directives(system_messages)?;
        if replay
            .native_items_for(target, ReplayProtocol::Anthropic)?
            .is_some()
        {
            self.native = true;
            for (handles, _) in replay.tool_call_handles() {
                let id = handles.first().expect("validated tool_use").clone();
                self.call(handles, Some(&id))?;
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
            self.portable = true;
            let aliases = replay.tool_call_handles();
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
                                    handles.iter().any(|handle| {
                                        portable
                                            .tool_correlations
                                            .get(handle)
                                            .is_some_and(|id| id == call.id.wire().as_ref())
                                    })
                                })
                                .ok_or_else(|| anyhow::anyhow!("missing portable tool handles"))?
                                .0
                                .clone();
                            let id = self.call(handles, None)?;
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
                            let mut handles = BTreeSet::from([call.id.to_string()]);
                            if let Some(native) = &call.native {
                                handles.extend(["id", "call_id"].iter().filter_map(|key| native.item[*key].as_str()).filter(|id| !id.is_empty()).map(str::to_owned));
                            }
                            let id = self.call(handles, None)?;
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
                        UserContent::Image(image) => blocks.push(image_input(image)?),
                        UserContent::Document(document) => blocks.push(match &document.data {
                            DocumentData::Text(text) => json!({"type":"text", "text":text}),
                            DocumentData::File(DocumentSourceKind::Base64(data)) if document.media_type.as_ref().is_none_or(|media| media.to_mime_type() == "application/pdf") => json!({"type":"document", "source":{"type":"base64", "media_type":"application/pdf", "data":data}}),
                            _ => anyhow::bail!("unsupported Anthropic document source (expected base64 PDF)"),
                        }),
                        UserContent::ToolResult(result) => {
                            let id = self.calls.result(&BTreeSet::from([result.call.to_string()]))?.clone();
                            let content = result.content.iter().map(|block| match block {
                                ToolResultContent::Text(text) => Ok(json!({"type":"text", "text":text.text})),
                                ToolResultContent::Json { value } => Ok(json!({"type":"text", "text":value.to_string()})),
                                ToolResultContent::Image(image) => image_input(image),
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
fn image_input(image: &Image) -> anyhow::Result<Value> {
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
    Ok(json!({"type":"image", "source":{"type":"base64", "media_type":media, "data":data}}))
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
    project_impl(
        input,
        target,
        system_messages,
        system,
        tools,
        strip_all_thinking,
        true,
    )
}

fn project_impl(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
    system_messages: bool,
    system: &Value,
    tools: &Value,
    strip_all_thinking: bool,
    completion: bool,
) -> anyhow::Result<ProjectedMessages> {
    let mut projection = Projection::default();
    for replay in input.iter().filter_map(|item| item.replay_ref()) {
        if replay
            .native_items_for(target, ReplayProtocol::Anthropic)?
            .is_some()
        {
            projection.reserved.extend(
                replay
                    .tool_call_handles()
                    .into_iter()
                    .flat_map(|(handles, _)| handles),
            );
        }
    }
    for item in input {
        match item {
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
    let mut messages: Vec<Value> = Vec::new();
    let mut binding = PrefixBinding::new(system, tools);
    let mut stripped = 0;
    let mut pending_system = Vec::new();
    for turn in projection.turns {
        if turn.role == "system" {
            pending_system.extend(turn.segments.into_iter().flat_map(|segment| segment.blocks));
            continue;
        }
        if turn.role == "assistant" && !pending_system.is_empty() {
            let directive = json!({"role":"system", "content":std::mem::take(&mut pending_system)});
            binding.push(&directive);
            messages.push(directive);
        }
        let prefix = binding.finish();
        let mut blocks = Vec::new();
        for (index, segment) in turn.segments.into_iter().enumerate() {
            let keep =
                !strip_all_thinking && index == 0 && segment.binding.as_deref() == Some(&prefix);
            if segment.native && !keep && segment.blocks.iter().any(is_thinking) {
                stripped += 1;
            }
            blocks.extend(segment.blocks.into_iter().filter(|block| {
                if segment.native && !keep {
                    matches!(block["type"].as_str(), Some("text" | "tool_use"))
                } else {
                    !is_thinking(block) || keep
                }
            }));
        }
        if blocks.is_empty() {
            // A thinking-only assistant can disappear under the guard. Its
            // preceding system turn must move past the next user, not become
            // an illegal system -> user sequence (nor change later bindings).
            if turn.role == "assistant" && messages.last().is_some_and(|m| m["role"] == "system") {
                pending_system.extend(
                    messages.pop().unwrap()["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .cloned(),
                );
                binding = PrefixBinding::new(system, tools);
                for message in &messages {
                    binding.push(message);
                }
            }
            continue;
        }
        if turn.role == "user" {
            blocks.sort_by_key(|block| block["type"] != "tool_result");
        }
        let message = json!({"role":turn.role, "content":blocks});
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == turn.role) {
            last["content"]
                .as_array_mut()
                .expect("content")
                .extend(message["content"].as_array().unwrap().iter().cloned());
            if turn.role == "user" {
                last["content"]
                    .as_array_mut()
                    .unwrap()
                    .sort_by_key(|block| block["type"] != "tool_result");
            }
            binding = PrefixBinding::new(system, tools);
            for message in &messages {
                binding.push(message);
            }
        } else {
            binding.push(&message);
            messages.push(message);
        }
    }
    if !pending_system.is_empty() {
        let directive = json!({"role":"system", "content":pending_system});
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
    let mut sources = BTreeSet::new();
    for item in input {
        if let Some(replay) = item.replay_ref() {
            replay.portable_projection()?;
        }
        if let ModelRequestItem::ReplayOnly(replay) = item
            && replay
                .native_items_for(target, ReplayProtocol::Anthropic)?
                .is_none()
        {
            sources.insert(replay.source_profile.clone());
        }
    }
    if !sources.is_empty() {
        return Ok(ReplayPreflight::ConversionRequired {
            sources: sources.into_iter().collect(),
        });
    }
    // Management preflight assesses stored history, not a completion body:
    // an idle transcript can be empty or end in a completed assistant turn.
    let projected = project_impl(
        input,
        target,
        system_messages,
        &json!([]),
        &json!([]),
        false,
        false,
    )?;
    let tokens = zevria_model::compaction::estimate_responses_input_tokens(&projected.messages, 0);
    Ok(ReplayPreflight::Compatible(
        zevria_model::ContextTokenEstimate::new(tokens, tokens),
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
