//! Deterministic request bodies and explicit cache breakpoints.
use crate::{Compatibility, RefusalFallback, ThinkingDisplay, project};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use zevria_foundation::{ModelProfileRef, ReasoningLevel};
use zevria_model::{ModelRequest, ModelRequestItem, ToolDeclaration, ToolDeclarations};

pub struct RequestOptions<'a> {
    pub target: &'a ModelProfileRef,
    pub max_tokens: u64,
    pub effort: ReasoningLevel,
    pub display: ThinkingDisplay,
    pub fallback: Option<RefusalFallback>,
    pub compatibility: &'a Compatibility,
    pub additional_params: &'a BTreeMap<String, Value>,
}
pub struct PreparedRequest {
    pub body: Value,
    pub prefix_binding: String,
    pub replay_source: &'static str,
    pub stripped_thinking_turns: usize,
}

pub fn tool_definition(tool: &ToolDeclaration, eager: bool) -> Value {
    let mut value =
        json!({"name":tool.name, "description":tool.description, "input_schema":tool.parameters});
    if eager {
        value["eager_input_streaming"] = true.into();
    }
    value
}

/// Render the exact declared tools, independent of engine call permissions.
pub fn build(
    request: &ModelRequest<'_>,
    options: &RequestOptions<'_>,
) -> anyhow::Result<PreparedRequest> {
    anyhow::ensure!(
        !matches!(
            options.effort,
            ReasoningLevel::None | ReasoningLevel::Minimal
        ),
        "Anthropic requires a reasoning level from low through max"
    );
    anyhow::ensure!(
        options.max_tokens > 0,
        "max_output_tokens must be greater than zero"
    );
    let mut declarations = ToolDeclarations::new(request.tools.to_vec())?;
    if !(options.compatibility.inline_tools && options.compatibility.system_messages) {
        declarations = declarations.fold(request.input.iter().filter_map(|item| match item {
            ModelRequestItem::AdditionalTools(tools) => Some(*tools),
            _ => None,
        }));
    }
    let selected = declarations
        .iter()
        .map(|tool| tool_definition(tool, options.compatibility.eager_tool_input_streaming))
        .collect::<Vec<_>>();
    let tools = if selected.is_empty() {
        Value::Null
    } else {
        Value::Array(selected)
    };
    let system = json!([{"type":"text", "text":request.instructions}]);
    let projected = project::project_with_compatibility(
        &request.input,
        options.target,
        options.compatibility,
        &system,
        &tools,
        false,
    )?;
    let mut thinking = json!({"type":"adaptive", "display":options.display});
    if options.compatibility.strict_thinking_binding {
        thinking["block_binding"] = json!({"prefix_mismatch_behavior":"error"});
    }
    let mut body =
        json!({"model":options.target.model, "max_tokens":options.max_tokens, "system":system});
    if !tools.is_null() {
        body["tools"] = tools;
    }
    body["messages"] = projected.messages.into();
    body["thinking"] = thinking;
    body["output_config"] = json!({"effort":options.effort.as_str()});
    body["stream"] = true.into();
    if let Some(fallback) = options.fallback {
        body["fallbacks"] = serde_json::to_value(fallback)?;
    }
    for (key, value) in options.additional_params {
        body[key] = value.clone();
    }
    if request.forbid_tool_calls {
        body["tool_choice"] = json!({"type":"none"});
    }
    place_cache_breakpoints(&mut body, options.compatibility.prompt_caching);
    Ok(PreparedRequest {
        body,
        prefix_binding: projected.prefix_binding,
        replay_source: projected.replay_source,
        stripped_thinking_turns: projected.stripped_thinking_turns,
    })
}

pub fn place_cache_breakpoints(body: &mut Value, enabled: bool) {
    // Clear only markers owned by this adapter, including a previous final
    // breakpoint when the binding retry removes its containing block.
    if let Some(system) = body["system"].as_array_mut() {
        for block in system {
            block
                .as_object_mut()
                .expect("system block")
                .remove("cache_control");
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            for block in message["content"].as_array_mut().expect("content") {
                block
                    .as_object_mut()
                    .expect("content block")
                    .remove("cache_control");
            }
        }
    }
    if !enabled {
        return;
    }
    body["system"][0]["cache_control"] = json!({"type":"ephemeral"});
    if let Some(last) = body["messages"]
        .as_array_mut()
        .and_then(|messages| messages.last_mut())
        && let Some(block) = last["content"]
            .as_array_mut()
            .and_then(|blocks| blocks.iter_mut().rev().find(|b| !project::is_thinking(b)))
    {
        block["cache_control"] = json!({"type":"ephemeral"});
    }
}

pub fn count_tokens_body(body: &Value) -> Value {
    let mut count = serde_json::Map::new();
    for key in [
        "model",
        "system",
        "tools",
        "tool_choice",
        "messages",
        "thinking",
    ] {
        if let Some(value) = body.get(key) {
            count.insert(key.into(), value.clone());
        }
    }
    Value::Object(count)
}

pub fn beta_headers(
    configured: &[String],
    display: ThinkingDisplay,
    fallback: Option<RefusalFallback>,
    strict_binding: bool,
    inline_tools: bool,
) -> String {
    let mut betas = configured.to_vec();
    for (enabled, beta) in [
        (
            display == ThinkingDisplay::Updates,
            "thinking-display-updates-2026-08-18",
        ),
        (fallback.is_some(), "server-side-fallback-2026-07-01"),
        (strict_binding, "thinking-binding-controls-2026-08-01"),
        (inline_tools, "inline-tools-2026-09-15"),
    ] {
        if enabled && !betas.iter().any(|b| b == beta) {
            betas.push(beta.into());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    betas.retain(|beta| seen.insert(beta.clone()));
    betas.join(",")
}
