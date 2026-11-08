use super::*;
use rig_core::message::Message;
use serde_json::json;
use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel};
use zevria_model::{ModelRequest, ModelRequestItem, ProviderReplay, ReplayMessage};

#[test]
fn canonical_binding_ignores_markers_and_key_order_but_not_content() {
    use binding::request_binding;
    let a = json!({"b":2,"a":1});
    let b = json!({"a":1,"cache_control":{"type":"ephemeral"},"b":2});
    assert_eq!(
        request_binding(&a, &json!([]), &[]),
        request_binding(&b, &json!([]), &[])
    );
    assert_ne!(
        request_binding(&a, &json!([]), &[]),
        request_binding(&a, &json!([]), &[json!({"role":"user","content":[]})])
    );
}

#[test]
fn thinking_binding_survives_a_tool_loop_and_strips_after_system_edit() {
    let target = ModelProfileRef::new("claude", "opus");
    let user = Message::user("hello");
    let system = json!([{"type":"text", "text":"stable"}]);
    let tools = json!([]);
    let first = project::project(
        &[ModelRequestItem::message(&user)],
        &target,
        true,
        &system,
        &tools,
        false,
    )
    .unwrap();
    let replay = ReplayMessage::new(ProviderReplay::anthropic_messages(
        target.clone(),
        vec![
            json!({"type":"thinking","thinking":"", "signature":"  bytes\n"}),
            json!({"type":"tool_use","id":"toolu_1", "name":"command","input":{}}),
        ],
        Some(first.prefix_binding),
    ))
    .unwrap();
    let Message::Assistant(assistant) = replay.message() else {
        panic!()
    };
    let rig_core::message::AssistantContent::ToolCall(call) = &assistant.content[1] else {
        panic!()
    };
    let result =
        Message::from(call.result(vec![rig_core::message::ToolResultContent::text("done")]));
    let input = [
        ModelRequestItem::message(&user),
        ModelRequestItem::replay_backed(&replay),
        ModelRequestItem::message(&result),
    ];
    let same = project::project(&input, &target, true, &system, &tools, false).unwrap();
    assert_eq!(same.messages[1]["content"][0], replay.replay().items[0]);
    let changed = project::project(
        &input,
        &target,
        true,
        &json!([{"type":"text","text":"changed"}]),
        &tools,
        false,
    )
    .unwrap();
    assert_eq!(changed.stripped_thinking_turns, 1);
    assert_eq!(changed.messages[1]["content"][0]["type"], "tool_use");
}

#[test]
fn request_body_and_counting_cache_breakpoints() {
    let target = ModelProfileRef::new("anthropic", "claude-opus-5-5");
    let user = Message::user("hello");
    let request = ModelRequest {
        tools: &[],
        forbid_tool_calls: false,
        instructions: "stable",
        input: vec![ModelRequestItem::message(&user)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let params = Default::default();
    let compat = Compatibility::default();
    let options = body::RequestOptions {
        target: &target,
        max_tokens: 64000,
        effort: ReasoningLevel::High,
        display: ThinkingDisplay::Summarized,
        fallback: Some(RefusalFallback::Default),
        compatibility: &compat,
        additional_params: &params,
    };
    let built = body::build(&request, &options).unwrap();
    assert_eq!(
        built.body["system"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(
        built.body["messages"][0]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(
        built.prefix_binding,
        binding::request_binding(
            &built.body["system"],
            &built.body["tools"],
            built.body["messages"].as_array().unwrap()
        )
    );
    assert!(
        body::count_tokens_body(&built.body)
            .get("max_tokens")
            .is_none()
    );
    let mut disabled = built.body.clone();
    body::place_cache_breakpoints(&mut disabled, false);
    assert!(!disabled.to_string().contains("cache_control"));
}

#[test]
fn stream_preserves_signature_and_rejects_invalid_json_and_unknown_deltas() {
    let mut stream = stream::MessageAccumulator::default();
    for event in [
        json!({"type":"message_start","message":{"model":"opus", "content":[], "usage":{"input_tokens":10, "cache_read_input_tokens":20,"cache_creation_input_tokens":30}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"", "signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"  opaque\n"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ] {
        stream.push(&event.to_string()).unwrap();
    }
    let finished = stream.finish().unwrap();
    assert_eq!(
        finished.blocks[0],
        json!({"type":"thinking","thinking":"","signature":"  opaque\n"})
    );
    assert_eq!(finished.usage.canonical().input_tokens, 60);
    assert_eq!(finished.usage.canonical().total_tokens, 64);
    let mut stream = stream::MessageAccumulator::default();
    assert!(
        stream
            .push(r#"{"type":"content_block_delta","index":0,"delta":{"type":"future_delta"}}"#)
            .is_err()
    );
    stream
        .push(r#"{"type":"message_start","message":{}}"#)
        .unwrap();
    stream.push(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"command","input":{}}}"#).unwrap();
    stream.push(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}"#).unwrap();
    let error = stream
        .push(r#"{"type":"content_block_stop","index":0}"#)
        .unwrap_err();
    assert_eq!(error.kind, error::ErrorKind::Retryable("invalid_tool_json"));
}

#[test]
fn fallback_echo_drops_pre_boundary_tools_thinking_and_unknown() {
    let blocks = stream::echo_blocks(vec![
        json!({"type":"text","text":"before"}),
        json!({"type":"tool_use"}),
        json!({"type":"thinking"}),
        json!({"type":"unknown"}),
        json!({"type":"fallback"}),
        json!({"type":"tool_use","id":"after"}),
    ]);
    assert_eq!(
        blocks,
        vec![
            json!({"type":"text","text":"before"}),
            json!({"type":"tool_use","id":"after"})
        ]
    );
}
