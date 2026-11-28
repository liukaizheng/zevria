use super::*;
use rig_core::message::{ToolResultContent, UserContent};
use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;
use zevria_session_api::TurnEvent;

fn prompt() -> UserPrompt {
    static IMAGE: std::sync::OnceLock<PromptImage> = std::sync::OnceLock::new();
    let image = IMAGE
        .get_or_init(|| zevria_content::test_support::prepared_image().image)
        .clone();
    assert!(zevria_content::image_preparation::fits_transport(
        image.encoded_len()
    ));
    UserPrompt::new(vec![
        PromptBlock::Text("before".into()),
        PromptBlock::Image(image.clone()),
        PromptBlock::Text("between".into()),
        PromptBlock::Image(image),
        PromptBlock::Text("after".into()),
    ])
    .unwrap()
}
fn content(input: &[Value]) -> Vec<&Value> {
    input
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .collect()
}
#[tokio::test]
async fn image_websocket_delta_and_reconnect_keep_complete_occurrences() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let expected = format!(
        "data:image/png;base64,{}",
        prompt().images().next().unwrap().base64()
    );
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async_with_config(
            stream,
            Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_frame_size(Some(64 * 1024 * 1024)),
            ),
        )
        .await
        .unwrap();
        let first = receive_json(&mut socket).await;
        let assert_images = |request: &Value, count| {
            let images = content(request["input"].as_array().unwrap())
                .into_iter()
                .filter(|b| b["type"] == "input_image")
                .collect::<Vec<_>>();
            assert_eq!(images.len(), count);
            assert!(images.iter().all(|image| image["image_url"] == expected));
        };
        assert_images(&first, 2);
        send_json(
            &mut socket,
            completed_event("resp_image_1", "msg_image_1", "seen"),
        )
        .await;
        let delta = receive_json(&mut socket).await;
        assert_eq!(delta["previous_response_id"], "resp_image_1");
        assert_images(&delta, 2);
        drop(socket);
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async_with_config(
            stream,
            Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_frame_size(Some(64 * 1024 * 1024)),
            ),
        )
        .await
        .unwrap();
        let full = receive_json(&mut socket).await;
        assert!(full.get("previous_response_id").is_none());
        assert_images(&full, 4);
        send_json(
            &mut socket,
            completed_event("resp_image_2", "msg_image_2", "seen again"),
        )
        .await;
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    for _ in 0..2 {
        let parts = RequestParts {
            tools: zevria_model::ToolDeclarations::default(),
            forbid_tool_calls: false,
            prompt: prompt().to_message(),
            history: state.history.clone(),
            instructions: "Image test".into(),
            allowed_tool_names: None,
        };
        let replays = state.history_replays.clone();
        run_turn_request_with_reconnect(
            request_with_replays(
                &parts.history,
                &replays,
                &parts.prompt,
                &parts.instructions,
                None,
            ),
            &mut provider,
            &mut state,
            &discard_updates(),
        )
        .await
        .unwrap();
    }
    server.await.unwrap();
}

#[tokio::test]
async fn provider_error_diagnostics_do_not_echo_image_payloads() {
    let image = prompt().images().next().unwrap().clone();
    let base64 = image.base64();
    for full_payload in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let echoed = if full_payload {
            base64.as_str()
        } else {
            &base64[..1024]
        };
        let detail = format!("bad image data:image/png;base64,{echoed}");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = receive_http_json(&mut stream).await;
            let body =
                json!({"error":{"code":"context_length_exceeded", "message":detail}}).to_string();
            if full_payload {
                use tokio::io::AsyncWriteExt as _;
                let headers = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                // The client intentionally stops after its 4 KiB diagnostic
                // allowance; a large error body need not finish writing.
                if let Err(error) = stream.write_all(body.as_bytes()).await {
                    assert!(matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                    ));
                }
            } else {
                send_http_response(&mut stream, "400 Bad Request", "application/json", body).await;
            }
        });
        let mut provider = connect_http_test_provider_with_options(
            format!("http://{address}/v1/responses"),
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            "image-error",
        )
        .await;
        let parts = RequestParts {
            tools: zevria_model::ToolDeclarations::default(),
            forbid_tool_calls: false,
            prompt: prompt().to_message(),
            history: vec![],
            instructions: "Image test".into(),
            allowed_tool_names: None,
        };
        let error = run_turn_request_with_recovery(
            parts.request(),
            &mut provider,
            &mut AttemptState::default(),
            &discard_updates(),
            &fast_recovery_policy(1),
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains(&base64[..256]));
        assert!(!error.to_string().contains(&base64));
        // The existing 4 KiB error-body bound intentionally prevents structured
        // classification of a multi-MiB, truncated JSON diagnostic. Both forms
        // must remain payload-free; only complete JSON has a trustworthy code.
        if !full_payload {
            assert!(
                error
                    .downcast_ref::<zevria_model::ModelInputTooLarge>()
                    .is_some()
            );
        }
        server.await.unwrap();
    }
}

#[test]
fn embedded_raster_wire_is_ordered_mime_bearing_and_lossless() {
    let prompt = prompt();
    let wire = zevria_responses::protocol::message_to_openai_input(prompt.to_message()).unwrap();
    let blocks = content(&wire);
    assert_eq!(
        blocks
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "input_text",
            "input_image",
            "input_text",
            "input_image",
            "input_text"
        ]
    );
    let data_url = format!(
        "data:image/png;base64,{}",
        prompt.images().next().unwrap().base64()
    );
    assert_eq!(blocks[1]["image_url"], data_url);
    assert_eq!(blocks[3]["image_url"], data_url);
    assert_eq!(blocks[2]["text"], "between");
}
#[test]
fn accounting_does_not_charge_base64_or_scrub_arbitrary_tool_json() {
    let prompt = prompt();
    let original = prompt.to_message();
    let mut huge = original.clone();
    if let Message::User { content } = &mut huge {
        for block in content {
            if let UserContent::Image(image) = block {
                image.data =
                    rig_core::message::DocumentSourceKind::Base64("A".repeat(5 * 1024 * 1024));
            }
        }
    }
    assert_eq!(
        zevria_model::estimate_message_tokens(&original).known_tokens(),
        zevria_model::estimate_message_tokens(&huge).known_tokens()
    );
    let small_wire = zevria_responses::protocol::message_to_openai_input(original).unwrap();
    let wire = zevria_responses::protocol::message_to_openai_input(huge).unwrap();
    let expected = zevria_model::compaction::estimate_responses_input_tokens(&small_wire, 0);
    assert_eq!(
        zevria_model::compaction::estimate_responses_input_tokens(&wire, 0).known_tokens(),
        expected.known_tokens()
    );
    assert_eq!(expected.complete_tokens(), None);
    assert_eq!(expected.unresolved_media.images, 2);
    assert!(expected.known_tokens() < 300);
    let literal = vec![
        json!({"type":"function_call_output", "call_id":"x", "output":{"type":"input_image", "image_url":"A".repeat(80_000)}}),
    ];
    assert!(
        zevria_model::compaction::estimate_responses_input_tokens(&literal, 0).known_tokens()
            > 20_000
    );
    assert_eq!(prompt.images().count(), 2);
}
#[tokio::test]
async fn full_http_sse_retry_keeps_prepared_user_and_tool_image_data() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let first = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            "retry",
        )
        .await;
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert_eq!(
            first.body, request.body,
            "retry must reuse the exact accepted payload"
        );
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_images", "msg_images", "image received").to_string()),
        )
        .await;
        request
    });
    let mut provider = connect_http_test_provider_with_options(
        format!("http://{address}/v1/responses"),
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        "image-session",
    )
    .await;
    let image = prompt().images().next().unwrap().clone();
    let call = zevria_content::message::tool_call_from_wire(
        "image-item",
        "image-call",
        rig_core::message::ToolFunction::new(
            rig_core::message::ToolName::new("read_image").unwrap(),
            json!({}),
        ),
    );
    let result = call.result(vec![
        ToolResultContent::text("prepared file"),
        image.to_tool_result_content(),
    ]);
    let parts = RequestParts {
        tools: zevria_model::ToolDeclarations::default(),
        forbid_tool_calls: false,
        prompt: prompt().to_message(),
        history: vec![
            Message::from(vec![AssistantContent::ToolCall(call)]),
            Message::from(result),
        ],
        instructions: "Image test".into(),
        allowed_tool_names: None,
    };
    let mut state = AttemptState::default();
    run_turn_request_with_recovery(
        parts.request(),
        &mut provider,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(2),
    )
    .await
    .unwrap();
    let captured = server.await.unwrap();
    let wire = zevria_responses::protocol::message_to_openai_input(parts.prompt).unwrap();
    let actual = content(captured.body["input"].as_array().unwrap())
        .into_iter()
        .filter(|b| b["type"] == "input_image")
        .collect::<Vec<_>>();
    let expected = content(&wire)
        .into_iter()
        .filter(|b| b["type"] == "input_image")
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    let tool_images = captured.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["output"].as_array())
        .flatten()
        .filter(|block| block["type"] == "input_image")
        .collect::<Vec<_>>();
    assert_eq!(tool_images.len(), 1);
    assert_eq!(
        tool_images[0]["image_url"],
        format!("data:{};base64,{}", image.mime_type(), image.base64())
    );
    for block in actual.into_iter().chain(tool_images) {
        assert!(
            block["image_url"].as_str().unwrap().len()
                <= zevria_content::image_preparation::MAX_IMAGE_TRANSPORT_BYTES
        );
    }
}

/// Explicit opt-in, read-only incident validation. No endpoint is initialized.
#[test]
#[ignore = "requires ZEVRIA_ACCOUNTING_SESSION pointing to a private local transcript"]
fn incident_checkpoint_read_only_router_assessment() {
    use zevria_session_api::ModelProvider;
    let path = std::path::PathBuf::from(
        std::env::var_os("ZEVRIA_ACCOUNTING_SESSION").expect("explicit local path"),
    );
    let before = std::fs::read(&path).unwrap();
    let items = zevria_transcript::transcript::load(&path).expect("read-only session load");
    let input = zevria_transcript::transcript::model_input(&items);
    let source = input
        .iter()
        .find_map(|item| item.replay_ref())
        .map(|replay| replay.source_profile.clone())
        .unwrap_or_else(|| {
            zevria_foundation::ModelProfileRef::new("local-incident", "unmapped-model")
        });
    for policy in [
        zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
        zevria_foundation::image_accounting::ImageAccountingPolicy::preset(
            "gpt-4.1-mini-2025-04-14",
        )
        .unwrap(),
    ] {
        let mut profile = resolved_profile(
            &source.provider,
            &source.model,
            "http://127.0.0.1:1/v1".into(),
            "unused",
            false,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            1_000_000,
        );
        profile.image_accounting = policy.clone();
        let router = crate::ProviderRouter::from_routes(
            [(
                ModelRole::Build,
                profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            )],
            "local-only",
            "incident",
        )
        .unwrap();
        let context = profile.context_policy();
        let request = ModelRequest {
            instructions: "unchanged local validation prefix",
            tools: &[],
            forbid_tool_calls: false,
            input: input.clone(),
            input_identity: None,
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        let zevria_model::models::ReplayPreflight::Compatible(estimate) =
            router.assess_request(&context, &request).unwrap()
        else {
            panic!("compatible checkpoint");
        };
        assert!(estimate.image_count >= 3);
        assert!(
            estimate.known_tokens() < 100_000,
            "encoded screenshots must not become a 500K text spike"
        );
        assert_eq!(
            estimate.complete_tokens().is_none(),
            policy == zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved
        );
        assert_eq!(
            router.initialized_profile_count(),
            0,
            "local assessment may not open a socket"
        );
        eprintln!(
            "incident assessment: policy={} known={} images={} unresolved={} complete={}",
            policy.label(),
            estimate.known_tokens(),
            estimate.image_count,
            estimate.unresolved_media.images,
            estimate.complete_tokens().is_some()
        );
    }
    assert!(
        std::fs::read(&path).unwrap() == before,
        "private transcript must remain untouched"
    );
}

#[tokio::test]
async fn retained_screenshots_404_counting_resume_and_same_mode_followup_use_real_router() {
    use zevria_model::{
        CompactionBackend, CompactionCheckpoint, CompactionConfig, CompactionPolicy,
        CompactionTrigger, OwnedModelRequestItem,
    };
    let screenshot = prompt().images().next().unwrap().clone();
    assert!(
        screenshot.base64().len() * 3 / 4 > 500_000,
        "fixture must reproduce the encoded-byte spike"
    );
    let mut tail = vec![OwnedModelRequestItem::message(Message::user(
        "retained checkpoint summary",
    ))];
    for index in 0..3 {
        let call = zevria_content::message::tool_call_from_wire(
            &format!("shot-item-{index}"),
            &format!("shot-call-{index}"),
            rig_core::message::ToolFunction::new(
                rig_core::message::ToolName::new("screenshot").unwrap(),
                json!({}),
            ),
        );
        let result = call.result(vec![
            ToolResultContent::text("saved screenshot"),
            screenshot.to_tool_result_content(),
        ]);
        tail.push(OwnedModelRequestItem::message(Message::from(vec![
            AssistantContent::ToolCall(call),
        ])));
        tail.push(OwnedModelRequestItem::message(Message::from(result)));
    }
    for mapped in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let expected_url = format!(
            "data:{};base64,{}",
            screenshot.mime_type(),
            screenshot.base64()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let count = receive_http_json(&mut stream).await;
            assert!(count.body.get("stream").is_none());
            send_http_response(&mut stream, "404 Not Found", "application/json", "{}").await;
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                let images = request.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|item| item["output"].as_array())
                    .flatten()
                    .filter(|block| block["type"] == "input_image")
                    .collect::<Vec<_>>();
                assert!(
                    images
                        .iter()
                        .all(|image| image["image_url"] == expected_url)
                );
                assert_eq!(
                    images.len(),
                    3,
                    "every saved occurrence must still be transmitted"
                );
                let mut event = completed_event(
                    &format!("resume-{index}"),
                    &format!("resume-message-{index}"),
                    "done",
                );
                event["response"]["usage"] = json!({"input_tokens":171876,"input_tokens_details":{"cached_tokens":160000},"output_tokens":607,"total_tokens":172483});
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(event.to_string()),
                )
                .await;
                requests.push(request.body);
            }
            assert_eq!(requests[0]["instructions"], requests[1]["instructions"]);
            assert_eq!(
                requests[0]["prompt_cache_key"],
                requests[1]["prompt_cache_key"]
            );
        });
        let mut profile = resolved_profile(
            "screenshots",
            "unmapped-model",
            url,
            "unused",
            false,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            1_000_000,
        );
        profile.input_token_limit = 500_000;
        if mapped {
            profile.image_accounting =
                zevria_foundation::image_accounting::ImageAccountingPolicy::preset(
                    "gpt-4.1-mini-2025-04-14",
                )
                .unwrap();
        }
        let contexts = std::array::from_fn(|_| profile.context_policy());
        let router = crate::ProviderRouter::from_routes(
            ModelRole::ALL.map(|role| {
                (
                    role,
                    profile.clone(),
                    zevria_foundation::ReasoningLevel::Medium,
                )
            }),
            "same-prefix",
            "screenshots",
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer
            .rewrite(&[
                zevria_transcript::transcript::TranscriptItem::SessionMode(SessionMode::Build),
                zevria_transcript::transcript::TranscriptItem::Compaction(Box::new(
                    CompactionCheckpoint::new(
                        CompactionTrigger::AutomaticPreTurn,
                        CompactionBackend::LocalSummary,
                        tail.clone(),
                        vec![],
                    )
                    .unwrap(),
                )),
            ])
            .unwrap();
        let mut engine = SessionEngine::new(
            router,
            ToolServer::new().run(),
            test_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_compaction_policy(
            CompactionPolicy::new(CompactionConfig::default(), contexts).unwrap(),
        )
        .with_mode_management();
        for (index, text) in ["commit the changes", "ordinary text followup"]
            .into_iter()
            .enumerate()
        {
            let (events, mut receiver) = session_event_channel(128);
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                engine.handle_command(
                    SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: text.into(),
                        mode: SessionMode::Build,
                        references: Vec::new(),
                    }),
                    &events,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            let updates = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert!(updates.iter().any(|event| matches!(
                event,
                SessionUpdate::Lifecycle(SessionEvent::Turn(TurnEvent::Completed { .. }))
            )));
            let snapshot = updates
                .iter()
                .find_map(|event| match event {
                    SessionUpdate::Lifecycle(SessionEvent::Turn(
                        TurnEvent::ContextUsageUpdated { snapshot, .. },
                    )) => Some(snapshot),
                    _ => None,
                })
                .unwrap();
            assert!(snapshot.known_input_tokens < 500_000);
            if index == 0 {
                assert_eq!(snapshot.projected_input_tokens.is_some(), mapped);
                assert_eq!(snapshot.unresolved_media.images, if mapped { 0 } else { 3 });
            } else {
                assert_eq!(
                    snapshot.source,
                    zevria_model::ContextTokenSource::UsagePlusDelta
                );
                assert!(snapshot.projected_input_tokens.unwrap() >= 172_483);
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
