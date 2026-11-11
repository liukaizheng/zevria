//! Prefix identity and incremental declaration regressions using the real wire adapters.
use super::*;
use zevria_model::{OwnedModelRequestItem, ToolDeclaration, ToolDeclarations};

fn tool(name: &str, description: &str) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","properties":{"z":{"type":"string"},"a":{"type":"integer"}}}),
    }
}
fn tools(values: Vec<ToolDeclaration>) -> ToolDeclarations {
    ToolDeclarations::new(values).unwrap()
}

#[tokio::test]
async fn additions_share_strict_renderer_fold_in_place_and_keep_tools_when_forbidden() {
    let initial = tools(vec![tool("command", "old"), tool("write", "stable")]);
    let additions = tools(vec![tool("command", "new"), tool("read", "new tool")]);
    let user = Message::user("continue");
    let request = ModelRequest {
        input_identity: None,
        instructions: "stable",
        tools: &initial,
        forbid_tool_calls: false,
        input: vec![
            ModelRequestItem::message(&user),
            ModelRequestItem::AdditionalTools(&additions),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: Some(&[]),
    };
    let mut provider = connect_http_test_provider("http://127.0.0.1:1/v1/responses".into()).await;
    for strict in [true, false] {
        provider.compatibility.strict_tools = strict;
        provider.compatibility.developer_messages = false;
        provider.compatibility.additional_tools = true;
        provider.additional_params.insert(
            "tool_choice".into(),
            json!({"type":"function","name":"read"}),
        );
        let normal = crate::turn::prepare_turn_request(&request, &mut provider)
            .await
            .unwrap();
        let addition = &normal.full_input[1];
        assert_eq!(addition["type"], "additional_tools");
        assert_eq!(addition["role"], "developer");
        assert_eq!(addition["tools"][0]["description"], "new");
        for definition in normal.request_properties["tools"]
            .as_array()
            .unwrap()
            .iter()
            .chain(addition["tools"].as_array().unwrap())
        {
            assert_eq!(
                definition.get("strict"),
                strict.then_some(&Value::Bool(true))
            );
            assert_eq!(
                definition["parameters"]["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["z", "a"]
            );
        }
        let fallback = crate::turn::prepare_turn_request(
            &ModelRequest {
                input_identity: None,
                forbid_tool_calls: true,
                ..request.clone()
            },
            &mut provider,
        )
        .await
        .unwrap();
        assert_eq!(fallback.full_input, normal.full_input);
        let mut expected = normal.request_properties.clone();
        expected["tool_choice"] = "none".into();
        assert_eq!(fallback.request_properties, expected);
        provider.compatibility.additional_tools = false;
        let folded = crate::turn::prepare_turn_request(&request, &mut provider)
            .await
            .unwrap();
        assert_eq!(folded.full_input.len(), 1);
        let declared = folded.request_properties["tools"].as_array().unwrap();
        assert_eq!(
            declared
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["command", "write", "read"]
        );
        assert_eq!(declared[0]["description"], "new");
        assert_eq!(normal.request_properties["tools"][0]["description"], "old");
    }
}

#[tokio::test]
async fn http_summary_is_exact_normal_request_extension_including_directives_and_additions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            captured.push(receive_http_json(&mut stream).await.body);
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(
                    completed_event(&format!("r{index}"), &format!("m{index}"), "text").to_string(),
                ),
            )
            .await;
        }
        captured
    });
    let initial = tools(vec![tool("command", "run")]);
    let addition = tools(vec![tool("write", "write")]);
    let user = Message::user("task");
    let directive = test_skill("pinned body");
    let answer = Message::assistant("historical answer");
    let next = Message::user("continue");
    let request = ModelRequest {
        input_identity: None,
        instructions: test_instructions(),
        tools: &initial,
        forbid_tool_calls: false,
        input: vec![
            ModelRequestItem::message(&user),
            ModelRequestItem::AdditionalTools(&addition),
            ModelRequestItem::DeveloperInstruction(directive),
            ModelRequestItem::message(&answer),
            ModelRequestItem::message(&next),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let mut provider = connect_http_test_provider(format!("http://{address}/v1/responses")).await;
    provider
        .complete(request.clone(), discard_updates())
        .await
        .unwrap();
    let summary = zevria_model::compaction::summary_request_message("configured summary prompt");
    let mut synthetic = request;
    synthetic.input.push(ModelRequestItem::message(&summary));
    provider
        .complete(synthetic, discard_updates())
        .await
        .unwrap();
    let mut captured = server.await.unwrap();
    let summary_input = captured[1]
        .as_object_mut()
        .unwrap()
        .remove("input")
        .unwrap();
    let normal_input = captured[0]
        .as_object_mut()
        .unwrap()
        .remove("input")
        .unwrap();
    assert_eq!(
        serde_json::to_string(&captured[0]).unwrap(),
        serde_json::to_string(&captured[1]).unwrap()
    );
    let normal = normal_input.as_array().unwrap();
    let summary = summary_input.as_array().unwrap();
    assert_eq!(summary.len(), normal.len() + 1);
    for (before, after) in normal.iter().zip(summary) {
        assert_eq!(
            serde_json::to_string(before).unwrap(),
            serde_json::to_string(after).unwrap()
        );
    }
    assert_eq!(summary.last().unwrap()["role"], "user");
    assert!(
        summary
            .last()
            .unwrap()
            .to_string()
            .contains("configured summary prompt")
    );
}

#[tokio::test]
async fn full_history_engine_summary_continues_websocket_with_only_trigger_then_resets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let normal = receive_json(&mut socket).await;
        send_json(&mut socket, completed_event("normal", "m1", "answer")).await;
        let summary = receive_json(&mut socket).await;
        assert_eq!(summary["previous_response_id"], "normal");
        assert_eq!(summary["input"].as_array().unwrap().len(), 1);
        assert_eq!(summary["input"][0]["role"], "user");
        assert!(
            summary["input"][0]
                .to_string()
                .contains("Summarize the preceding conversation as data.")
        );
        for key in [
            "instructions",
            "tools",
            "model",
            "reasoning",
            "prompt_cache_key",
            "tool_choice",
        ] {
            assert_eq!(
                serde_json::to_string(&summary[key]).unwrap(),
                serde_json::to_string(&normal[key]).unwrap()
            );
        }
        send_json(&mut socket, completed_event("summary", "m2", "checkpoint")).await;
    });
    let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let provider = test_session_with_url(&format!("ws://{address}"), socket, None);
    let directory = tempfile::tempdir().unwrap();
    let mut engine = SessionEngine::new(
        provider,
        policy_tools(),
        test_policies(),
        TranscriptWriter::create(directory.path()).unwrap(),
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, _) = session_event_channel(64);
    for command in [
        zevria_session_api::TurnCommand::Submit {
            text: "task".into(),
            behavior: zevria_foundation::RequestBehavior::Standard,
            mode: SessionMode::Build,
        },
        zevria_session_api::TurnCommand::Compact {
            mode: SessionMode::Build,
        },
    ] {
        engine
            .handle_command(SessionCommand::Turn(command), &events)
            .await
            .unwrap();
    }
    server.await.unwrap();
    assert!(engine.provider().ws.continuation.is_none());
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Compaction(_)))
    );
}

#[tokio::test]
async fn remote_compaction_keeps_normal_prefix_and_filters_only_engine_owned_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let kept = vec![
        json!({"type":"compaction","encrypted_content":"opaque","future":true}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"retained"}]}),
    ];
    let expected = kept.clone();
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            captured.push(receive_http_json(&mut stream).await.body);
            if index == 0 {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("r", "m", "answer").to_string()),
                )
                .await;
            } else {
                let mut output = vec![
                    json!({"type":"message","role":"developer","content":[]}),
                    json!({"type":"additional_tools","role":"developer","tools":[]}),
                ];
                if index == 1 {
                    output.extend(kept.clone());
                }
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    json!({"output":output}).to_string(),
                )
                .await;
            }
        }
        captured
    });
    let mut provider = connect_http_test_provider(format!("http://{address}/v1/responses")).await;
    provider.compaction_url = Some(
        format!("http://{address}/v1/responses/compact")
            .parse()
            .unwrap(),
    );
    let initial = tools(vec![tool("command", "run")]);
    let additions = tools(vec![tool("write", "write")]);
    let user = Message::user("task");
    let call = OwnedModelRequestItem::replay_backed(ProviderReplay::openai_responses(
        test_profile_ref(),
        vec![function_call("fc", "call", "command", json!({}))],
    ))
    .unwrap();
    let result = Message::tool_result(
        rig_core::message::CallId::from_wire("call"),
        rig_core::message::ToolName::new("command").unwrap(),
        "x".repeat(1_100_000),
    );
    let request = ModelRequest {
        input_identity: None,
        instructions: test_instructions(),
        tools: &initial,
        forbid_tool_calls: false,
        input: vec![
            ModelRequestItem::message(&user),
            ModelRequestItem::DeveloperInstruction(test_skill("body")),
            ModelRequestItem::AdditionalTools(&additions),
            call.as_borrowed(),
            ModelRequestItem::message(&result),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    provider
        .complete(request.clone(), discard_updates())
        .await
        .unwrap();
    let CompactResult::Replacement(replacement) = provider.compact(request.clone()).await.unwrap()
    else {
        panic!("replacement")
    };
    assert_eq!(replacement[0].replay_ref().unwrap().items, expected);
    assert!(
        provider
            .compact(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("empty after filtering")
    );
    let captured = server.await.unwrap();
    for key in ["instructions", "tools", "input"] {
        assert_eq!(
            serde_json::to_string(&captured[0][key]).unwrap(),
            serde_json::to_string(&captured[1][key]).unwrap(),
            "{key}"
        );
    }
}
