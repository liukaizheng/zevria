use super::*;
use zevria_model::models::ModelSelection;

#[tokio::test]
async fn invalid_eager_tool_json_retries_and_exhaustion_keeps_the_actionable_flag() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(receive_http_json(&mut stream).await.body);
            let events = [
                json!({"type":"message_start", "message":{}}),
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"toolu_invalid", "name":"count_once", "input":{}}}),
                json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{"}}),
                json!({"type":"content_block_stop", "index":0}),
            ];
            let body = events
                .into_iter()
                .map(|value| sse_data(value.to_string()))
                .collect::<String>();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nRequest-Id: req_invalid\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        assert_eq!(requests[0], requests[1]);
    });
    let mut provider = runtime(&profile(url));
    provider.recovery.max_attempts = 2;
    let error = provider
        .complete(request(&Message::user("invalid input")), discard_updates())
        .await
        .unwrap_err();
    let display = error.to_string();
    assert!(
        display.contains("compatibility.eager_tool_input_streaming"),
        "{display}"
    );
    assert!(display.contains("req_invalid"));
    assert!(display.contains("2 attempts"));
    server.await.unwrap();
}

#[tokio::test]
async fn streaming_preview_is_published_before_message_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let (preview_seen, preview_ack) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        receive_http_json(&mut stream).await;
        let body = answer();
        let boundary = body.find("data: {\"type\":\"message_delta\"").unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        stream
            .write_all(&body.as_bytes()[..boundary])
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), preview_ack)
            .await
            .unwrap()
            .unwrap();
        stream
            .write_all(&body.as_bytes()[boundary..])
            .await
            .unwrap();
    });
    let mut provider = runtime(&profile(url));
    let user = Message::user("stream");
    let (events, mut receiver) = session_event_channel(64);
    let observed = async move {
        let mut preview_seen = Some(preview_seen);
        while let Some(update) = receiver.recv().await {
            match update {
                SessionUpdate::Streams(batch)
                    if batch.root.as_ref().is_some_and(|root| {
                        root.message
                            .as_ref()
                            .is_some_and(|message| format!("{message:?}").contains("hello"))
                    }) =>
                {
                    if let Some(ack) = preview_seen.take() {
                        ack.send(()).unwrap();
                    }
                }
                SessionUpdate::Lifecycle(SessionEvent::UsageUpdated { .. }) => {
                    return preview_seen.is_none();
                }
                _ => {}
            }
        }
        false
    };
    let (result, observed) = tokio::join!(
        provider.complete(request(&user), ProgressReporter::new(events)),
        observed
    );
    result.unwrap();
    assert!(observed);
    server.await.unwrap();
}

#[tokio::test]
async fn mixed_router_switches_both_ways_and_correlates_foreign_tool_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let a = profile(format!("http://{address}/v1/messages"));
    let b = resolved_profile(
        "responses",
        "gpt-test",
        format!("http://{address}/v1/responses"),
        "test-key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        100000,
    );
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(receive_http_json(&mut stream).await);
            let body = match index {
                0 => response(
                    vec![
                        json!({"type":"thinking","thinking":"native", "signature":"signed"}),
                        json!({"type":"tool_use","id":"toolu_native","name":"count_once","input":{}}),
                    ],
                    "tool_use",
                ),
                1 => sse_data(
                    completed_output_event(
                        "resp_2",
                        vec![function_call(
                            "fc_native",
                            "call_native",
                            "count_once",
                            json!({}),
                        )],
                    )
                    .to_string(),
                ),
                _ => answer(),
            };
            send_http_response(&mut stream, "200 OK", "text/event-stream", body).await;
        }
        requests
    });
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: Arc::new(AtomicUsize::new(0)),
        })
        .run();
    let mut router = ProviderRouter::from_routes(
        [
            (
                ModelRole::Build,
                a.clone(),
                zevria_foundation::ReasoningLevel::High,
            ),
            (
                ModelRole::Plan,
                b.clone(),
                zevria_foundation::ReasoningLevel::High,
            ),
        ],
        "stable",
        tools,
        "root",
    )
    .unwrap();
    assert_eq!(
        router.model_selection(ModelRole::Plan).unwrap().profile,
        b.profile
    );
    assert!(router.requires_portable_compaction());
    let user = Message::user("go");
    let first = router
        .complete(request(&user), discard_updates())
        .await
        .unwrap();
    let first_record = first.into_record();
    let Message::Assistant(assistant) = first_record.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[1] else {
        panic!()
    };
    let result1 =
        Message::from(call.result(vec![rig_core::message::ToolResultContent::text("one")]));
    let b_selection =
        ModelSelection::new(b.profile.clone(), zevria_foundation::ReasoningLevel::High);
    router
        .prepare_model_update(ModelRole::Build, &b_selection)
        .unwrap();
    router.install_model_update(ModelRole::Build, &b_selection);
    let mut input = vec![
        ModelRequestItem::message(&user),
        first_record.model_request_item(),
        ModelRequestItem::message(&result1),
    ];
    let second = router
        .complete(
            ModelRequest {
                instructions: "stable",
                input: input.clone(),
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    let second_record = second.into_record();
    let Message::Assistant(assistant) = second_record.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[0] else {
        panic!()
    };
    let result2 =
        Message::from(call.result(vec![rig_core::message::ToolResultContent::text("two")]));
    let a_selection =
        ModelSelection::new(a.profile.clone(), zevria_foundation::ReasoningLevel::High);
    router
        .prepare_model_update(ModelRole::Build, &a_selection)
        .unwrap();
    router.install_model_update(ModelRole::Build, &a_selection);
    input.extend([
        second_record.model_request_item(),
        ModelRequestItem::message(&result2),
    ]);
    router
        .complete(
            ModelRequest {
                instructions: "stable",
                input,
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    let captured = server.await.unwrap();
    let responses = &captured[1].body["input"];
    assert_ne!(responses[1]["call_id"], "toolu_native");
    assert_eq!(responses[1]["call_id"], responses[2]["call_id"]);
    assert!(!responses.to_string().contains("signed"));
    let messages = &captured[2].body["messages"];
    assert_eq!(messages[1]["content"][0]["signature"], "signed");
    assert_ne!(messages[3]["content"][0]["id"], "call_native");
    assert_eq!(
        messages[3]["content"][0]["id"],
        messages[4]["content"][0]["tool_use_id"]
    );
    assert_eq!(router.initialized_profile_count(), 2);
}

#[tokio::test]
async fn engine_tool_loop_persists_signed_thinking_and_reload_reproduces_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut profile = profile(format!("http://{address}/v1/messages"));
    if let ProfileProtocol::Anthropic { endpoint, .. } = &mut profile.protocol {
        endpoint.input_token_count.enabled = false;
    }
    let (sent, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            captured.push(receive_http_json(&mut stream).await);
            let body = if index == 0 {
                response(
                    vec![
                        json!({"type":"thinking","thinking":"", "signature":"  exact signature\n"}),
                        json!({"type":"tool_use","id":"toolu_once","name":"count_once","input":{}}),
                    ],
                    "tool_use",
                )
            } else {
                answer()
            };
            send_http_response(&mut stream, "200 OK", "text/event-stream", body).await;
            if index == 1 {
                sent.send(
                    captured[1].body["system"][0]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
                .ok();
                break;
            }
        }
        // Return the listener so the resumed request can be observed without a
        // detached task or any timing assumptions about transcript persistence.
        (listener, captured)
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: executions.clone(),
        })
        .run();
    let router = ProviderRouter::from_routes(
        [(
            ModelRole::Build,
            profile.clone(),
            zevria_foundation::ReasoningLevel::High,
        )],
        "stable",
        tools.clone(),
        "root",
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let path = writer.path().to_path_buf();
    let mut engine = SessionEngine::new(
        router,
        tools.clone(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_transcript_items(vec![TranscriptItem::SessionModels(
        zevria_model::models::SessionModels::new(
            ModelSelection::new(
                profile.profile.clone(),
                zevria_foundation::ReasoningLevel::High,
            ),
            ModelSelection::new(
                profile.profile.clone(),
                zevria_foundation::ReasoningLevel::High,
            ),
        )
        .unwrap(),
    )])
    .unwrap()
    .with_compaction_policy(
        zevria_model::CompactionPolicy::new(
            Default::default(),
            std::array::from_fn(|_| profile.context_policy()),
        )
        .unwrap(),
    );
    let (events, mut receiver) = session_event_channel(256);
    engine
        .handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "count once".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let mut saw_tool = false;
    let mut updates = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        if matches!(
            update,
            SessionUpdate::Lifecycle(SessionEvent::ToolResults { .. })
        ) {
            saw_tool = true;
        }
        updates.push(format!("{update:?}"));
    }
    assert!(saw_tool, "{updates:#?}");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    drop(engine);
    let instructions = received.await.unwrap();
    let (listener, mut captured) = server.await.unwrap();
    assert_eq!(
        captured[1].body["messages"][1]["content"][0],
        json!({"type":"thinking","thinking":"","signature":"  exact signature\n"})
    );
    let loaded = transcript::load(&path).unwrap();
    let final_index = loaded
        .iter()
        .rposition(|item| item.provider_replay().is_some())
        .unwrap();
    let input = transcript::model_input(&loaded[..final_index]);
    assert_eq!(
        loaded
            .iter()
            .filter_map(TranscriptItem::provider_replay)
            .count(),
        2
    );
    let expected = captured[1].body.clone();
    let resume_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
        request
    });
    let mut resumed = ProviderRouter::from_routes(
        [(
            ModelRole::Build,
            profile,
            zevria_foundation::ReasoningLevel::High,
        )],
        "stable",
        tools,
        "root",
    )
    .unwrap();
    resumed
        .complete(
            ModelRequest {
                instructions: &instructions,
                input,
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    captured.push(resume_server.await.unwrap());
    assert_eq!(captured[2].body, expected);
}

#[tokio::test]
async fn idle_timeout_retries_without_partial_output_and_retry_after_is_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let original = receive_http_json(&mut first).await;
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let partial=[json!({"type":"message_start","message":{}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"failed partial"}})].into_iter().map(|v|sse_data(v.to_string())).collect::<String>();
        first.write_all(partial.as_bytes()).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let retried = receive_http_json(&mut second).await;
        assert_eq!(original.body, retried.body);
        send_http_response(&mut second, "200 OK", "text/event-stream", answer()).await;
    });
    let mut provider = runtime(&profile(url));
    provider.recovery.stall_warning = std::time::Duration::from_millis(5);
    provider.recovery.response_idle_timeout = std::time::Duration::from_millis(20);
    let response = provider
        .complete(request(&Message::user("retry idle")), discard_updates())
        .await
        .unwrap();
    assert!(!format!("{:?}", response.message()).contains("failed partial"));
    server.await.unwrap();
    for delay in [0, 61] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_json(&mut stream).await;
            let body = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
            let headers = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: {delay}\r\nRequest-Id: req_rate\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            if delay == 0 {
                let (mut stream, _) = listener.accept().await.unwrap();
                receive_http_json(&mut stream).await;
                send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
            }
        });
        let mut provider = runtime(&profile(url));
        let result = provider
            .complete(request(&Message::user("rate")), discard_updates())
            .await;
        assert_eq!(result.is_ok(), delay == 0);
        if delay > 0 {
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("60s"));
            assert!(error.contains("req_rate"));
        }
        server.await.unwrap();
    }
}
