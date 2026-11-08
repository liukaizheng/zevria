use crate::*;
use rig_core::message::Message;
use serde_json::{Value, json};
use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel};
use zevria_model::{
    ModelRequest, ModelRequestItem, ProviderReplay, ReplayMessage, ToolDeclaration,
    ToolDeclarations,
};

fn tool(name: &str, description: &str) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","properties":{}}),
    }
}
fn tools(values: Vec<ToolDeclaration>) -> ToolDeclarations {
    ToolDeclarations::new(values).unwrap()
}

#[test]
fn inline_additions_merge_with_directives_in_order_and_fold_only_when_ineffective() {
    let target = ModelProfileRef::new("anthropic", "claude-opus-5-5");
    let initial = tools(vec![tool("command", "old"), tool("write", "stable")]);
    let additions = tools(vec![tool("command", "replacement"), tool("read", "new")]);
    let user = Message::user("task");
    let directive = zevria_instructions::DirectiveContent::skill(
        &zevria_instructions::SkillSnapshot::new("review".parse().unwrap(), "Review", "body")
            .unwrap(),
    );
    let request = ModelRequest {
        instructions: "stable",
        tools: &initial,
        forbid_tool_calls: false,
        input: vec![
            ModelRequestItem::message(&user),
            ModelRequestItem::DeveloperInstruction(&directive),
            ModelRequestItem::AdditionalTools(&additions),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: Some(&[]),
    };
    let params = Default::default();
    for (inline_tools, system_messages) in [(true, true), (false, true), (true, false)] {
        let compatibility = Compatibility {
            inline_tools,
            system_messages,
            ..Default::default()
        };
        let options = body::RequestOptions {
            target: &target,
            max_tokens: 64000,
            effort: ReasoningLevel::High,
            display: ThinkingDisplay::Summarized,
            fallback: None,
            compatibility: &compatibility,
            additional_params: &params,
        };
        let prepared = body::build(&request, &options).unwrap();
        let body = &prepared.body;
        let betas = body::beta_headers(
            &[],
            options.display,
            None,
            false,
            inline_tools && system_messages,
        );
        assert_eq!(
            betas.contains("inline-tools-2026-09-15"),
            inline_tools && system_messages
        );
        assert!(body.get("tool_choice").is_none());
        if inline_tools && system_messages {
            assert_eq!(body["tools"].as_array().unwrap().len(), 2);
            assert_eq!(body["tools"][0]["description"], "old");
            let system = &body["messages"][1];
            assert_eq!(system["role"], "system");
            assert_eq!(system["content"][0]["text"], directive.text);
            assert_eq!(system["content"][1]["type"], "tool_addition");
            assert_eq!(system["content"][1]["tool"]["type"], "tool_definition");
            assert_eq!(
                system["content"][1]["tool"]["definition"]["name"],
                "command"
            );
            assert_eq!(
                system["content"][1]["tool"]["definition"]["eager_input_streaming"],
                true
            );
            assert_eq!(system["content"][2]["tool"]["definition"]["name"], "read");
        } else {
            assert_eq!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["command", "write", "read"]
            );
            assert_eq!(body["tools"][0]["description"], "replacement");
            assert!(!body["messages"].to_string().contains("tool_addition"));
        }
        let count = body::count_tokens_body(body);
        for key in ["system", "tools", "messages", "thinking"] {
            assert_eq!(
                serde_json::to_string(&count[key]).unwrap(),
                serde_json::to_string(&body[key]).unwrap()
            );
        }
        let fallback = body::build(
            &ModelRequest {
                forbid_tool_calls: true,
                ..request.clone()
            },
            &options,
        )
        .unwrap();
        assert_eq!(fallback.body["tools"], body["tools"]);
        assert_eq!(fallback.body["tool_choice"], json!({"type":"none"}));
        project::preflight_with_compatibility(&request.input, &target, &compatibility).unwrap();
    }
}

#[test]
fn summary_extension_keeps_tools_properties_and_historical_thinking_binding() {
    let target = ModelProfileRef::new("anthropic", "claude-opus-5-5");
    let initial = tools(vec![tool("command", "run")]);
    let addition = tools(vec![tool("write", "write")]);
    let compatibility = Compatibility::default();
    let params = Default::default();
    let options = body::RequestOptions {
        target: &target,
        max_tokens: 64000,
        effort: ReasoningLevel::High,
        display: ThinkingDisplay::Summarized,
        fallback: Some(RefusalFallback::Default),
        compatibility: &compatibility,
        additional_params: &params,
    };
    let first = Message::user("first task");
    let mut request = ModelRequest {
        instructions: "stable",
        tools: &initial,
        forbid_tool_calls: false,
        input: vec![ModelRequestItem::message(&first)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let seed = body::build(&request, &options).unwrap();
    let replay = ReplayMessage::new(ProviderReplay::anthropic_messages(
        target.clone(),
        vec![
            json!({"type":"thinking","thinking":"reasoning","signature":"exact signature"}),
            json!({"type":"text","text":"answer"}),
        ],
        Some(seed.prefix_binding),
    ))
    .unwrap();
    let next = Message::user("next task");
    let answer = Message::assistant("second answer");
    let last = Message::user("continue");
    request.input.extend([
        ModelRequestItem::replay_backed(&replay),
        ModelRequestItem::message(&next),
        ModelRequestItem::AdditionalTools(&addition),
        ModelRequestItem::message(&answer),
        ModelRequestItem::message(&last),
    ]);
    let normal = body::build(&request, &options).unwrap();
    let summary = zevria_model::compaction::summary_request_message("configured summary prompt");
    request.input.push(ModelRequestItem::message(&summary));
    let synthetic = body::build(&request, &options).unwrap();
    assert_eq!(normal.stripped_thinking_turns, 0);
    assert_eq!(synthetic.stripped_thinking_turns, 0);
    for key in [
        "tools",
        "system",
        "thinking",
        "output_config",
        "max_tokens",
        "stream",
        "fallbacks",
    ] {
        assert_eq!(
            serde_json::to_string(&normal.body[key]).unwrap(),
            serde_json::to_string(&synthetic.body[key]).unwrap(),
            "{key}"
        );
    }
    assert!(normal.body.get("tool_choice").is_none());
    assert!(synthetic.body.get("tool_choice").is_none());
    let unmark = |body: &Value| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(binding::without_cache_control)
            .collect::<Vec<_>>()
    };
    let before = unmark(&normal.body);
    let mut after = unmark(&synthetic.body);
    let trigger = after.last_mut().unwrap()["content"]
        .as_array_mut()
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        trigger["text"],
        match summary {
            Message::User { content } => match &content[0] {
                rig_core::message::UserContent::Text(text) => text.text.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        }
    );
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&after).unwrap()
    );
}
