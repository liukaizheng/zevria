use super::*;
use rig_core::message::{
    AssistantContent, Document, DocumentData, DocumentMediaType, DocumentSourceKind, Image,
    ImageMediaType, Message, ToolResultContent, UserContent,
};
use serde_json::{Value, json};
use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel};
use zevria_model::{
    DirectiveContent, ModelRequest, ModelRequestItem, OwnedModelRequestItem, ProviderReplay,
    ReplayMessage,
};

#[test]
fn binding_ignores_wire_markers_but_never_user_owned_cache_control_keys() {
    let first = json!({"role":"assistant", "content":[{"type":"tool_use", "id":"t", "name":"command", "input":{"cache_control":{"type":"ephemeral", "value":1}}}]});
    let mut changed = first.clone();
    changed["content"][0]["input"]["cache_control"]["value"] = 2.into();
    assert_ne!(
        binding::request_binding(&json!([]), &Value::Null, std::slice::from_ref(&first)),
        binding::request_binding(&json!([]), &Value::Null, &[changed])
    );
    let mut marked = first.clone();
    marked["content"][0]["cache_control"] = json!({"type":"ephemeral"});
    assert_eq!(
        binding::request_binding(&json!([]), &Value::Null, &[first]),
        binding::request_binding(&json!([]), &Value::Null, &[marked])
    );
}

#[test]
fn sse_fixture_accumulates_all_block_types_and_semantic_progress() {
    let mut accumulator = stream::MessageAccumulator::default();
    let mut readable_reasoning = false;
    let mut complete_tool = false;
    for payload in include_str!("../tests/fixtures/tool-loop.sse")
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        let advanced = accumulator.push(payload).unwrap();
        assert_eq!(advanced, !payload.contains("\"type\":\"ping\""));
        if let Some(preview) = accumulator.preview() {
            let text = format!("{preview:?}");
            readable_reasoning |= text.contains("readable reasoning");
            complete_tool |= text.contains("pwd");
        }
    }
    let finished = accumulator.finish().unwrap();
    assert!(readable_reasoning && complete_tool);
    assert_eq!(finished.blocks[0]["signature"], "  exact signature\n");
    assert_eq!(finished.blocks[1]["data"], "opaque-redacted");
    assert_eq!(finished.blocks[2]["citations"].as_array().unwrap().len(), 1);
    assert_eq!(finished.blocks[3]["input"], json!({"command":"pwd"}));
    assert_eq!(finished.blocks[4]["opaque"]["value"], true);
    assert_eq!(finished.usage.canonical().total_tokens, 64);
}

#[test]
fn management_preflight_accepts_idle_history_and_empty_assistants_do_not_strand_system_turns() {
    let user = Message::user("first");
    let answer = Message::assistant("complete");
    assert!(project::preflight(&[], &target(), true).is_ok());
    assert!(
        project::preflight(
            &[
                ModelRequestItem::message(&user),
                ModelRequestItem::message(&answer)
            ],
            &target(),
            true
        )
        .is_ok()
    );
    let directive = DirectiveContent::new(zevria_instructions::DirectivePayload::SkillRevocation {
        name: zevria_instructions::SkillName::parse("example").unwrap(),
        reason: "changed".into(),
    })
    .unwrap();
    let replay = ReplayMessage::new(ProviderReplay::anthropic_messages(
        target(),
        vec![json!({"type":"thinking", "thinking":"", "signature":"opaque"})],
        Some("0".repeat(64)),
    ))
    .unwrap();
    let next = Message::user("next");
    let projected = project::project(
        &[
            ModelRequestItem::message(&user),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::replay_backed(&replay),
            ModelRequestItem::message(&next),
        ],
        &target(),
        true,
        &json!([]),
        &Value::Null,
        false,
    )
    .unwrap();
    assert_eq!(projected.messages.len(), 2);
    assert_eq!(
        projected.messages[0]["content"].as_array().unwrap().len(),
        2
    );
    assert_eq!(projected.messages[1]["role"], "system");
    let mut retry = json!({"system":[], "messages":[{"role":"user","content":[{"type":"text","text":"first"}]},{"role":"system","content":[{"type":"text","text":"directive"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"opaque"}]},{"role":"user","content":[{"type":"text","text":"next"}]}]});
    project::strip_thinking(&mut retry);
    project::validate_messages(retry["messages"].as_array().unwrap()).unwrap();
}

fn target() -> ModelProfileRef {
    ModelProfileRef::new("anthropic", "claude-opus-5-5")
}
fn project_owned(history: &[OwnedModelRequestItem], system: &str) -> project::ProjectedMessages {
    project::project(
        &history.iter().map(|i| i.as_borrowed()).collect::<Vec<_>>(),
        &target(),
        true,
        &json!([{"type":"text","text":system}]),
        &Value::Null,
        false,
    )
    .unwrap()
}
fn reply(binding: String, text: &str) -> OwnedModelRequestItem {
    OwnedModelRequestItem::replay_backed(ProviderReplay::anthropic_messages(
        target(),
        vec![
            json!({"type":"thinking","thinking":"", "signature":"  exact\n"}),
            json!({"type":"text","text":text}),
        ],
        Some(binding),
    ))
    .unwrap()
}
#[test]
fn binding_compaction_mode_switch_and_resume_scenarios() {
    let mut history = vec![OwnedModelRequestItem::message(Message::user("first"))];
    history.push(reply(
        project_owned(&history, "Plan").prefix_binding,
        "answer",
    ));
    history.push(OwnedModelRequestItem::message(Message::user("next")));
    let plan = project_owned(&history, "Plan");
    assert_eq!(plan.stripped_thinking_turns, 0);
    assert_eq!(project_owned(&history, "Build").stripped_thinking_turns, 1);
    assert_eq!(project_owned(&history, "Plan").messages, plan.messages);
    assert_eq!(
        project_owned(&history, "changed on resume").stripped_thinking_turns,
        1
    );
    let mut compacted = history.clone();
    compacted[0] = OwnedModelRequestItem::message(Message::user("portable compaction summary"));
    let after = project_owned(&compacted, "Plan");
    assert_eq!(after.stripped_thinking_turns, 1);
    compacted.push(reply(after.prefix_binding, "post-compaction"));
    compacted.push(OwnedModelRequestItem::message(Message::user("continue")));
    let next = project_owned(&compacted, "Plan");
    assert_eq!(next.stripped_thinking_turns, 1);
    assert_eq!(next.messages[3]["content"][0]["type"], "thinking");
    let persisted = serde_json::to_string(&compacted).unwrap();
    let restored: Vec<OwnedModelRequestItem> = serde_json::from_str(&persisted).unwrap();
    assert_eq!(project_owned(&restored, "Plan").messages, next.messages);
}
#[test]
fn directives_normalize_all_positions_and_merge_roles() {
    let user1 = Message::user("one");
    let user2 = Message::user("two");
    let answer = Message::assistant("answer");
    let directive = DirectiveContent::new(zevria_instructions::DirectivePayload::SkillRevocation {
        name: zevria_instructions::SkillName::parse("example").unwrap(),
        reason: "directive".into(),
    })
    .unwrap();
    for input in [
        vec![
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::message(&user1),
        ],
        vec![
            ModelRequestItem::message(&user1),
            ModelRequestItem::DeveloperInstruction(&directive),
        ],
        vec![
            ModelRequestItem::message(&user1),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::DeveloperInstruction(&directive),
        ],
        vec![
            ModelRequestItem::message(&user1),
            ModelRequestItem::message(&answer),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::message(&user2),
        ],
        vec![
            ModelRequestItem::message(&user1),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::message(&user2),
        ],
        vec![
            ModelRequestItem::message(&user1),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::message(&answer),
            ModelRequestItem::message(&user2),
        ],
    ] {
        for support in [true, false] {
            let projected =
                project::project(&input, &target(), support, &json!([]), &Value::Null, false)
                    .unwrap();
            for (index, message) in projected.messages.iter().enumerate() {
                if message["role"] == "system" {
                    assert!(support);
                    assert!(index > 0);
                    assert_eq!(projected.messages[index - 1]["role"], "user");
                    assert!(
                        projected
                            .messages
                            .get(index + 1)
                            .is_none_or(|m| m["role"] == "assistant")
                    );
                }
            }
            let text = projected.messages.to_vec();
            assert!(serde_json::to_string(&text).unwrap().contains("directive"));
        }
    }
    let raw = Message::System {
        content: "untyped".into(),
    };
    assert!(
        project::project(
            &[ModelRequestItem::message(&raw)],
            &target(),
            true,
            &json!([]),
            &Value::Null,
            false
        )
        .is_err()
    );
    assert!(
        project::project(
            &[
                ModelRequestItem::message(&user1),
                ModelRequestItem::message(&answer)
            ],
            &target(),
            true,
            &json!([]),
            &Value::Null,
            false
        )
        .is_err()
    );
}
#[test]
fn tool_results_precede_text_and_directives_and_images_documents_errors_map() {
    let first = Message::user("go");
    let replay = ReplayMessage::new(ProviderReplay::anthropic_messages(
        target(),
        vec![json!({"type":"tool_use","id":"toolu_1","name":"command","input":{}})],
        None,
    ))
    .unwrap();
    let Message::Assistant(assistant) = replay.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[0] else {
        panic!()
    };
    let image = Image {
        data: DocumentSourceKind::Base64("image-bytes".into()),
        media_type: Some(ImageMediaType::PNG),
        ..Default::default()
    };
    let mut result = call.result(vec![
        ToolResultContent::Json {
            value: json!({"ok":false}),
        },
        ToolResultContent::Image(image.clone()),
    ]);
    result.is_error = true;
    let user = Message::User {
        content: vec![
            UserContent::text("after"),
            UserContent::Image(image),
            UserContent::Document(Document {
                data: DocumentData::File(DocumentSourceKind::Base64("pdf-bytes".into())),
                media_type: Some(DocumentMediaType::PDF),
                ..Default::default()
            }),
            UserContent::ToolResult(result),
        ],
    };
    let directive = DirectiveContent::new(zevria_instructions::DirectivePayload::SkillRevocation {
        name: zevria_instructions::SkillName::parse("example").unwrap(),
        reason: "policy".into(),
    })
    .unwrap();
    let projected = project::project(
        &[
            ModelRequestItem::message(&first),
            ModelRequestItem::replay_backed(&replay),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::message(&user),
        ],
        &target(),
        false,
        &json!([]),
        &Value::Null,
        false,
    )
    .unwrap();
    let content = &projected.messages[2]["content"];
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["is_error"], true);
    assert_eq!(content[0]["content"][0]["text"], "{\"ok\":false}");
    assert_eq!(content[0]["content"][1]["type"], "image");
    assert_eq!(content[2]["source"]["media_type"], "image/png");
    assert_eq!(content[3]["type"], "document");
    assert_eq!(content[4]["text"], directive.text);
}
#[test]
fn declared_tools_cache_movement_and_golden_body() {
    let user = Message::user("hello");
    let tools = vec![zevria_model::ToolDeclaration {
        name: "command".into(),
        description: "Run a command".into(),
        parameters: json!({"type":"object","properties":{}}),
    }];
    let compat = Compatibility::default();
    let params = Default::default();
    let target = target();
    let options = body::RequestOptions {
        target: &target,
        max_tokens: 64000,
        effort: ReasoningLevel::High,
        display: ThinkingDisplay::Summarized,
        fallback: Some(RefusalFallback::Default),
        compatibility: &compat,
        additional_params: &params,
    };
    let mut request = ModelRequest {
        tools: &tools,
        forbid_tool_calls: false,
        instructions: "stable",
        input: vec![ModelRequestItem::message(&user)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let body = body::build(&request, &options).unwrap().body;
    assert_eq!(
        format!("{}\n", serde_json::to_string_pretty(&body).unwrap()),
        include_str!("../tests/fixtures/messages-default.json")
    );
    let none = Vec::new();
    request.allowed_tool_names = Some(&none);
    let maintenance = body::build(&request, &options).unwrap().body;
    assert_eq!(
        maintenance, body,
        "permissions alone never rewrite declarations"
    );
    request.forbid_tool_calls = true;
    let replay = ReplayMessage::new(ProviderReplay::anthropic_messages(
        target.clone(),
        vec![json!({"type":"tool_use","id":"toolu_1","name":"command","input":{}})],
        None,
    ))
    .unwrap();
    let Message::Assistant(assistant) = replay.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[0] else {
        panic!()
    };
    let result = Message::from(call.result(vec![ToolResultContent::text("done")]));
    request.input = vec![
        ModelRequestItem::message(&user),
        ModelRequestItem::replay_backed(&replay),
        ModelRequestItem::message(&result),
    ];
    let maintenance = body::build(&request, &options).unwrap().body;
    assert_eq!(maintenance["tool_choice"], json!({"type":"none"}));
    assert_eq!(maintenance["tools"].as_array().unwrap().len(), 1);
    assert!(
        maintenance["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        maintenance["messages"][2]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert!(
        !maintenance["tools"][0]
            .as_object()
            .unwrap()
            .contains_key("strict")
    );
}
#[test]
fn stream_stop_matrix_unknown_redacted_eager_json_and_errors() {
    for stop in [
        "end_turn",
        "stop_sequence",
        "tool_use",
        "max_tokens",
        "model_context_window_exceeded",
        "pause_turn",
        "refusal",
        "unknown",
    ] {
        let mut acc = stream::MessageAccumulator::default();
        for event in [
            json!({"type":"message_start","message":{}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_details":{"category":"policy"}}}),
            json!({"type":"message_stop"}),
        ] {
            acc.push(&event.to_string()).unwrap();
        }
        let result = acc.finish();
        assert_eq!(
            result.is_ok(),
            matches!(stop, "end_turn" | "stop_sequence" | "tool_use")
        );
        if stop == "refusal" {
            assert!(result.err().unwrap().message.contains("policy"));
        }
    }
    let mut acc = stream::MessageAccumulator::default();
    acc.push(r#"{"type":"message_start","message":{}}"#)
        .unwrap();
    for (index, block) in [
        json!({"type":"redacted_thinking","data":" exact "}),
        json!({"type":"future_block","unknown":[1,2]}),
        json!({"type":"tool_use","id":"toolu_1","name":"command","input":{}}),
    ]
    .into_iter()
    .enumerate()
    {
        acc.push(
            &json!({"type":"content_block_start","index":index,"content_block":block}).to_string(),
        )
        .unwrap();
        if index == 2 {
            for partial in ["{\"command\":", "\"pwd\"}"] {
                acc.push(&json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":partial}}).to_string()).unwrap();
            }
        }
        acc.push(&json!({"type":"content_block_stop","index":index}).to_string())
            .unwrap();
    }
    acc.push(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#)
        .unwrap();
    acc.push(r#"{"type":"message_stop"}"#).unwrap();
    let finished = acc.finish().unwrap();
    assert_eq!(finished.blocks[0]["data"], " exact ");
    assert_eq!(finished.blocks[1]["unknown"], json!([1, 2]));
    assert_eq!(finished.blocks[2]["input"], json!({"command":"pwd"}));
    let mut acc = stream::MessageAccumulator::default();
    let error = acc
        .push(r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#)
        .unwrap_err();
    assert_eq!(error.kind, error::ErrorKind::Retryable("overloaded"));
    assert!(stream::MessageAccumulator::default().finish().is_err());
}
