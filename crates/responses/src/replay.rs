//! Side-effect-free destination projection, shared by dispatch and management.
//! The original ledger is never mutated. Tool ownership is occurrence-based,
//! not a cumulative last-writer-wins map of provider handles.

use crate::protocol::message_to_openai_input;
use rig_core::message::{
    AssistantContent, CallId, Message, Text, ToolCall, ToolFunction, UserContent,
};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};
use zevria_foundation::ModelProfileRef;
use zevria_model::ModelRequestItem;
use zevria_model::compaction::replay::ToolCorrelations;
use zevria_model::models::ReplayPreflight;
use zevria_model::provider_replay::{
    decode_responses_item, normalize_tool_arguments, responses_tool_handles,
};
use zevria_model::{ProviderReplay, ReplayProtocol, openai::Output};

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
pub struct InputProjection {
    input: Vec<serde_json::Value>,
    calls: ToolCorrelations<Call>,
    native_ids: HashSet<String>,
    minted_ids: HashSet<String>,
    reserved_native_ids: HashSet<String>,
    used_native: bool,
    used_portable: bool,
    used_untagged: bool,
}

impl InputProjection {
    fn call(
        &mut self,
        handles: &[Option<&str>],
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
        if !native && let Some(id) = &output {
            self.minted_ids.insert(id.clone());
        }
        self.calls.call(
            handles,
            Call {
                output: output.clone(),
                native,
            },
        )?;
        Ok(output)
    }

    fn result(&mut self, handles: &[Option<&str>]) -> anyhow::Result<&Call> {
        self.calls.result(handles)
    }

    fn replay(&mut self, replay: &ProviderReplay, target: &ModelProfileRef) -> anyhow::Result<()> {
        if replay.protocol() != Some(ReplayProtocol::Responses) {
            let portable = replay.portable_projection()?;
            self.used_portable = true;
            let mut handles = replay.tool_call_handles();
            if let Some(mut message) = portable.message {
                if let Message::Assistant(assistant) = &mut message {
                    for block in &mut assistant.content {
                        if let AssistantContent::ToolCall(call) = block {
                            let (aliases, _) = handles.next().expect("decoded native call");
                            call.id = CallId::from_wire(
                                self.call(&aliases, None, false)?.expect("portable call"),
                            );
                            call.native = None;
                        }
                    }
                }
                self.input.extend(message_to_openai_input(message)?);
            }
            return Ok(());
        }
        let native = replay.source_profile == *target;
        if native {
            // Native replay must still reject malformed known records.
            replay.validate_known_items()?;
        } else {
            replay
                .validate()
                .map_err(|reason| anyhow::anyhow!("invalid provider replay envelope: {reason}"))?;
        }
        self.used_native |= native;
        self.used_portable |= !native;
        for (index, item) in replay.items.iter().enumerate() {
            let decoded = if native {
                None
            } else {
                decode_responses_item(item, index)?
            };
            let kind = item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let handles = responses_tool_handles(item);
            let has_handle = handles.iter().any(Option::is_some);
            anyhow::ensure!(
                !matches!(kind, "function_call" | "custom_tool_call") || has_handle,
                "historical tool call has no correlation handle"
            );
            if kind == "function_call" {
                let id = item["call_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("function call has no call_id"))?;
                let output = self
                    .call(&handles, native.then(|| id.to_string()), false)?
                    .expect("supported call");
                if let Some(Output::FunctionCall(function)) = decoded {
                    let arguments = normalize_tool_arguments(function.arguments.as_str());
                    let message = Message::from(vec![AssistantContent::ToolCall(ToolCall::new(
                        CallId::from_wire(output),
                        ToolFunction::new(function.name, arguments),
                    ))]);
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
            } else if kind.ends_with("_call") && has_handle {
                self.call(
                    &handles,
                    native.then(|| {
                        item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap()
                            .to_owned()
                    }),
                    !native,
                )?;
            } else if let Some(Output::Message(message)) = decoded {
                let content = message.into_portable_content().collect::<Vec<_>>();
                if !content.is_empty() {
                    self.input
                        .extend(message_to_openai_input(Message::from(content))?);
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
            Message::Assistant(assistant) => {
                self.used_untagged = true;
                let mut portable = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => portable.push(AssistantContent::Text(
                            Text::new(zevria_content::citations::render_text(text)),
                        )),
                        AssistantContent::ToolCall(call) => {
                            let handle = call.id.wire();
                            let alias = call
                                .native
                                .as_ref()
                                .and_then(|native| native.item["id"].as_str())
                                .filter(|id| !id.is_empty());
                            let id = self
                                .call(&[Some(&handle), alias], None, false)?
                                .expect("supported call");
                            let mut call = call.clone();
                            call.id = CallId::from_wire(id);
                            call.native = None;
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
                Message::from(portable)
            }
            Message::User { content } => {
                let mut portable = Vec::new();
                for block in content {
                    let UserContent::ToolResult(result) = block else {
                        portable.push(block.clone());
                        continue;
                    };
                    let call = self.result(&[Some(&result.call.wire())])?;
                    if let Some(output) = &call.output {
                        let mut result = result.clone();
                        result.call = CallId::from_wire(output.clone());
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

/// Options that shape the Responses wire prefix.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionOptions<'a> {
    pub target: &'a ModelProfileRef,
    pub developer_messages: bool,
    pub tool_options: crate::tools::ToolRendererOptions,
    pub additional_tools: bool,
}

impl<'a> From<&'a ModelProfileRef> for ProjectionOptions<'a> {
    fn from(target: &'a ModelProfileRef) -> Self {
        Self {
            target,
            developer_messages: true,
            tool_options: Default::default(),
            additional_tools: true,
        }
    }
}

/// Opaque compatibility is a structured result, not a plain-message fallback.
pub fn preflight<'a>(
    input: &[ModelRequestItem<'_>],
    options: impl Into<PreflightOptions<'a>>,
) -> anyhow::Result<ReplayPreflight> {
    let PreflightOptions { projection, policy } = options.into();
    let ProjectionOptions {
        target,
        tool_options,
        additional_tools,
        ..
    } = projection;
    policy.validate(false)?;
    let sources = foreign_replay_only_sources(input, target);
    if !sources.is_empty() {
        for replay in input.iter().filter_map(|item| item.replay_ref()) {
            replay.validate_known_items()?;
        }
        return Ok(ReplayPreflight::ConversionRequired {
            sources: sources.into_iter().collect(),
        });
    }
    let (projected, _) = project(input, projection)?;
    let mut estimate = zevria_model::media_accounting::responses_input(&projected, 0, policy);
    if !additional_tools {
        // Projection omits inline items in fold mode, but their declarations
        // still consume capacity in the request's top-level tools.
        for item in input {
            if let ModelRequestItem::AdditionalTools(tools) = item {
                let rendered = crate::tools::render(tools, tool_options)?;
                let tokens = zevria_model::compaction::approximate_tokens_from_bytes(
                    serde_json::to_vec(&rendered)?.len(),
                );
                estimate = estimate
                    .saturating_add(zevria_model::ContextTokenEstimate::new(tokens, tokens));
            }
        }
    }
    Ok(ReplayPreflight::Compatible(estimate))
}

fn foreign_replay_only_sources(
    input: &[ModelRequestItem<'_>],
    target: &ModelProfileRef,
) -> BTreeSet<ModelProfileRef> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelRequestItem::ReplayOnly(replay)
                if &replay.source_profile != target
                    || replay.protocol() != Some(ReplayProtocol::Responses) =>
            {
                Some(replay.source_profile.clone())
            }
            _ => None,
        })
        .collect()
}

/// Prepared-request settings shared by one cached capacity assessment.
#[derive(Debug, Clone, Copy)]
pub struct PreflightOptions<'a> {
    pub projection: ProjectionOptions<'a>,
    pub policy: &'a zevria_foundation::image_accounting::ImageAccountingPolicy,
}

impl<'a> From<&'a ModelProfileRef> for PreflightOptions<'a> {
    fn from(target: &'a ModelProfileRef) -> Self {
        Self {
            projection: target.into(),
            policy: &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
        }
    }
}

/// One destination projection, shared by local admission and wire dispatch.
#[derive(Default)]
pub struct ResponsesSlotCache {
    prefix: Option<ProjectionPrefix>,
    generation: u64,
}

pub type PreflightCache = ResponsesSlotCache;

struct ProjectionPrefix {
    target: ModelProfileRef,
    compatibility: [bool; 3],
    policy: zevria_foundation::image_accounting::ImageAccountingPolicy,
    policy_version: u32,
    count: usize,
    digest: [u8; 32],
    projection: InputProjection,
    input: Arc<Vec<serde_json::Value>>,
    estimate: zevria_model::compaction::ResponsesInputEstimate,
    folded_tools: u64,
}

pub struct ProjectedInput {
    pub input: Arc<Vec<serde_json::Value>>,
    pub source: ReplaySource,
    /// Changes on rebuild, including admission-side rebuilds that may have
    /// renamed portable IDs in a previously transmitted prefix.
    pub generation: u64,
}

impl ResponsesSlotCache {
    pub fn projected_input(&self) -> Option<ProjectedInput> {
        self.prefix.as_ref().map(|prefix| ProjectedInput {
            input: prefix.input.clone(),
            source: prefix.projection.source(),
            generation: self.generation,
        })
    }

    pub fn folded_tool_tokens(&self, identity: &zevria_model::ModelInputIdentity) -> Option<u64> {
        self.prefix
            .as_ref()
            .filter(|prefix| prefix.count == identity.len() && prefix.digest == identity.digest())
            .map(|prefix| prefix.folded_tools)
    }

    pub fn assess(
        &mut self,
        input: &[ModelRequestItem<'_>],
        identity: &zevria_model::ModelInputIdentity,
        options: PreflightOptions<'_>,
    ) -> anyhow::Result<ReplayPreflight> {
        let PreflightOptions { projection, policy } = options;
        let ProjectionOptions {
            target,
            developer_messages,
            tool_options,
            additional_tools,
        } = projection;
        policy.validate(false)?;
        anyhow::ensure!(
            identity.len() == input.len(),
            "preflight identity length mismatch"
        );
        let compatibility = [
            developer_messages,
            tool_options.strict_tools,
            additional_tools,
        ];
        let cached = self.prefix.take().filter(|prefix| {
            prefix.target == *target
                && prefix.policy == *policy
                && prefix.policy_version == policy.version()
                && prefix.compatibility == compatibility
                && identity.prefix(prefix.count) == Some(prefix.digest)
        });
        let mut prefix = match cached {
            Some(prefix)
                if prefix
                    .projection
                    .can_extend(&input[prefix.count..], target)? =>
            {
                prefix
            }
            _ => {
                self.generation = self
                    .generation
                    .checked_add(1)
                    .expect("projection generations exhausted");
                ProjectionPrefix {
                    target: target.clone(),
                    policy: policy.clone(),
                    policy_version: policy.version(),
                    compatibility,
                    count: 0,
                    digest: [0; 32],
                    projection: InputProjection::default(),
                    input: Arc::default(),
                    estimate: Default::default(),
                    folded_tools: 0,
                }
            }
        };
        let suffix = &input[prefix.count..];
        let sources = foreign_replay_only_sources(suffix, target);
        if !sources.is_empty() {
            for replay in suffix.iter().filter_map(|item| item.replay_ref()) {
                replay.validate_known_items()?;
            }
            return Ok(ReplayPreflight::ConversionRequired {
                sources: sources.into_iter().collect(),
            });
        }
        prefix.projection.extend(suffix, projection)?;
        let mut projected = prefix.projection.take_input();
        if !projected.is_empty() {
            prefix.estimate.append_with_policy(&projected, policy);
            Arc::make_mut(&mut prefix.input).append(&mut projected);
        }
        if !additional_tools {
            for item in suffix {
                if let ModelRequestItem::AdditionalTools(tools) = item {
                    prefix.folded_tools = prefix.folded_tools.saturating_add(
                        zevria_model::compaction::approximate_tokens_from_bytes(
                            serde_json::to_vec(&crate::tools::render(tools, tool_options)?)?.len(),
                        ),
                    );
                }
            }
        }
        prefix.count = identity.len();
        prefix.digest = identity.digest();
        let tokens = prefix.folded_tools;
        let estimate = prefix
            .estimate
            .estimate(0)
            .saturating_add(zevria_model::ContextTokenEstimate::new(tokens, tokens))
            .enforce_request_constraints(policy);
        self.prefix = Some(prefix);
        Ok(ReplayPreflight::Compatible(estimate))
    }
}

pub fn project<'a>(
    input: &[ModelRequestItem<'_>],
    options: impl Into<ProjectionOptions<'a>>,
) -> anyhow::Result<(Vec<serde_json::Value>, ReplaySource)> {
    let mut projection = InputProjection::default();
    projection.extend(input, options.into())?;
    let source = projection.source();
    Ok((projection.take_input(), source))
}

impl InputProjection {
    /// A future native handle can force a different neutral ID in the old
    /// portable prefix. In that rare case callers must rebuild from history.
    pub fn can_extend(
        &self,
        input: &[ModelRequestItem<'_>],
        target: &ModelProfileRef,
    ) -> anyhow::Result<bool> {
        for replay in input.iter().filter_map(|item| item.replay_ref()) {
            if let Some(items) = replay.native_items_for(target, ReplayProtocol::Responses)?
                && items
                    .iter()
                    .filter_map(|item| item.get("call_id")?.as_str())
                    .any(|id| self.minted_ids.contains(id))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn take_input(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.input)
    }
    pub fn source(&self) -> ReplaySource {
        if self.used_portable {
            ReplaySource::PortableAssistant
        } else if self.used_untagged {
            ReplaySource::GenericAssistant
        } else if self.used_native {
            ReplaySource::Native
        } else {
            ReplaySource::None
        }
    }

    pub fn extend(
        &mut self,
        input: &[ModelRequestItem<'_>],
        options: ProjectionOptions<'_>,
    ) -> anyhow::Result<()> {
        let ProjectionOptions {
            target,
            developer_messages,
            tool_options,
            additional_tools,
        } = options;
        // Reserve future native wire handles before minting neutral ones. A
        // configured provider may itself emit IDs using Zevria's neutral prefix.
        for replay in input.iter().filter_map(|item| item.replay_ref()) {
            if let Some(items) = replay.native_items_for(target, ReplayProtocol::Responses)? {
                self.reserved_native_ids.extend(
                    items
                        .iter()
                        .filter_map(|item| item.get("call_id")?.as_str().map(str::to_owned)),
                );
            }
        }
        for item in input {
            match item {
                ModelRequestItem::AdditionalTools(tools) => {
                    tools.validate()?;
                    anyhow::ensure!(
                        !tools.is_empty(),
                        "additional tool declarations must not be empty"
                    );
                    if additional_tools {
                        self.input.push(serde_json::json!({"type":"additional_tools", "role":"developer", "tools":crate::tools::render(tools, tool_options)?}));
                    }
                }
                ModelRequestItem::RequestInstruction(directive) => {
                    directive.validate()?;
                    self.input.push(serde_json::json!({"type": "message", "role": if developer_messages { "developer" } else { "user" }, "content": [{"type": "input_text", "text": directive.render()}]}));
                }
                ModelRequestItem::DeveloperInstruction(directive) => {
                    directive.validate()?;
                    self.input.push(serde_json::json!({"type": "message", "role": if developer_messages { "developer" } else { "user" }, "content": [{"type": "input_text", "text": directive.text}]}));
                }
                ModelRequestItem::ReplayOnly(replay) => {
                    anyhow::ensure!(
                        replay
                            .native_items_for(target, ReplayProtocol::Responses)?
                            .is_some(),
                        "cannot send replay-only openai.responses history from source profile {} to foreign target profile {target}: conversion required; select its source with /model, confirm a portable summary, or start a fresh session",
                        replay.source_profile
                    );
                    self.replay(replay, target)?;
                }
                ModelRequestItem::ReplayBacked(content) => self.replay(content.replay(), target)?,
                ModelRequestItem::Message(message) => self.message(message)?,
            }
        }
        Ok(())
    }
}
