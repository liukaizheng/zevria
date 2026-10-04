use super::*;
use zevria_content::WebSearchAttemptOutcome as Outcome;
use zevria_content::WebSearchStatus as Status;

fn search_item(id: &str, action: Value) -> Value {
    json!({"type":"web_search_call","id":id,"status":"completed","action":action})
}
fn cited_item() -> Value {
    json!({"type":"message","id":"cited","role":"assistant","status":"completed","content":[{"type":"output_text","text":"🦀 Rust is fast.","annotations":[{"type":"url_citation","start_index":2,"end_index":15,"title":"Rust guide","url":"https://www.rust-lang.org/learn"},{"type":"future_annotation","opaque":"preserved"}]}]})
}
fn search_config() -> WebSearchConfig {
    serde_json::from_value(json!({"enabled":true,"external_web_access":false,"search_context_size":"high","return_token_budget":"unlimited","filters":{"allowed_domains":["rust-lang.org"],"blocked_domains":["example.org"]},"user_location":{"country":"GB","city":"London"}})).unwrap()
}
fn parts() -> RequestParts {
    RequestParts {
        prompt: Message::user("research"),
        history: vec![],
        instructions: "Test".into(),
        allowed_tool_names: None,
    }
}
async fn provider() -> OpenAiProvider {
    let mut provider = connect_http_test_provider(
        "http://127.0.0.1:1/responses".into(),
        ToolServer::new().run(),
    )
    .await;
    provider.input_token_count_url = None;
    provider
}

/// Each server stage waits for an acknowledgement from the consumer. A timeout
/// only bounds a broken test; it never releases a delta or the completion.
pub(super) async fn held_answer_stream(websocket: bool, advertised: bool, searched: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "{}://{}/responses",
        if websocket { "ws" } else { "http" },
        listener.local_addr().unwrap()
    );
    let (release, mut stages) = tokio::sync::mpsc::channel::<()>(1);
    let first = "🦀 A long answer starts here.\n\n```rust\n";
    let tail = format!(
        "{}\n```\nThe answer is complete.",
        "let readable = true;\n".repeat(2048)
    );
    let full = format!("{first}{tail}");
    let expected_full = full.clone();
    let server = tokio::spawn(async move {
        let (mut http, _) = listener.accept().await.unwrap();
        let (mut ws, mut http, request) = if websocket {
            let mut ws = accept_async(http).await.unwrap();
            let request = receive_json(&mut ws).await;
            (Some(ws), None, request)
        } else {
            let request = receive_http_json(&mut http).await.body;
            http.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            // Keep one transport below without a second server task.
            (None, Some(http), request)
        };
        // The HTTP branch retains its socket; the WS branch owns it instead.
        assert_eq!(
            request["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "web_search")),
            advertised
        );
        let output = u64::from(searched);
        let annotation = json!({"type":"url_citation","start_index":0,"end_index":full.chars().count(),"title":"Source","url":"https://example.org"});
        let mut item = json!({"type":"message","id":"live","role":"assistant","status":"in_progress","content":[]});
        let mut initial = Vec::new();
        if searched {
            initial.push(json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":search_item("ws",json!({"type":"search","query":"streaming"}))}));
        }
        initial.extend([
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":output,"item":item}),
            json!({"type":"response.content_part.added","sequence_number":3,"output_index":output,"content_index":0,"item_id":"live","part":{"type":"output_text","text":"","annotations":[]}}),
            output_text_delta_at(first, 10, output, "live"),
        ]);
        item["status"] = json!("completed");
        item["content"] = json!([{"type":"output_text","text":full,"annotations":if searched { vec![annotation] } else { vec![] }}]);
        let mut native = Vec::new();
        if searched {
            native.push(search_item(
                "ws",
                json!({"type":"search","query":"streaming"}),
            ));
        }
        native.push(item);
        let batches = [
            initial,
            vec![output_text_delta_at(&tail, 11, output, "live")],
            vec![completed_output_event("held", native.clone())],
        ];
        for (stage, batch) in batches.into_iter().enumerate() {
            if stage > 0 {
                stages.recv().await.expect("consumer releases stage");
            }
            for event in batch {
                if let Some(ws) = &mut ws {
                    send_json(ws, event).await;
                } else {
                    let data = sse_data(event.to_string());
                    http.as_mut()
                        .unwrap()
                        .write_all(format!("{:X}\r\n{data}\r\n", data.len()).as_bytes())
                        .await
                        .unwrap();
                }
            }
        }
        native
    });
    let mut openai = if websocket {
        let (socket, _) = connect_async(&url).await.unwrap();
        test_session_with_url(&url, socket, None)
    } else {
        connect_http_test_provider(url, ToolServer::new().run()).await
    };
    if advertised {
        openai.web_search = search_config();
    }
    openai.input_token_count_url = None;
    let (sender, mut receiver) = session_event_channel(32);
    let progress = ProgressReporter::new(sender);
    let task = tokio::spawn(async move { openai.complete(parts().request(), progress).await });
    for expected in [first, expected_full.as_str()] {
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let SessionUpdate::Streams(batch) = receiver
                    .recv()
                    .await
                    .expect("provider stream must stay open")
                    && let Some(snapshot) = batch.root
                    && snapshot.message.as_ref().is_some_and(|message| {
                        zevria_content::assistant_plain_text(message) == expected
                    })
                {
                    break snapshot;
                }
            }
        })
        .await
        .expect("readable answer must arrive while completion is held");
        assert!(!task.is_finished(), "provider must still be pending");
        if advertised {
            let attempt = snapshot.attempt.unwrap();
            assert_eq!(attempt.outcome, Outcome::InProgress);
            assert_eq!(attempt.activity.is_empty(), !searched);
            assert!(attempt.presentation.iter().any(|part| matches!(&part.content, zevria_content::AssistantPresentationContent::Answer { text } if text == expected)));
        } else {
            assert!(snapshot.attempt.is_none());
        }
        release.send(()).await.unwrap();
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("released completion")
        .unwrap()
        .unwrap();
    let native = server.await.unwrap();
    assert_eq!(response.record().provider_replay().unwrap().items, native);
    let final_text = zevria_content::assistant_plain_text(response.message());
    assert!(final_text.starts_with(&expected_full));
    assert_eq!(
        final_text.matches("[Source](https://example.org/)").count(),
        usize::from(searched)
    );
}

#[tokio::test]
async fn http_answers_stream_before_completion_with_search_disabled_unused_and_used() {
    for (advertised, searched) in [(false, false), (true, false), (true, true)] {
        held_answer_stream(false, advertised, searched).await;
    }
}

#[tokio::test]
async fn hosted_request_controls_policy_choice_and_function_strictness() {
    let mut openai = provider().await;
    let mut parts = parts();
    let prepare = crate::turn::prepare_turn_request;
    assert!(
        prepare(&parts.request(), &mut openai)
            .await
            .unwrap()
            .request_properties
            .get("tools")
            .is_none()
    );
    openai.web_search = search_config();
    for role in ModelRole::ALL {
        let prepared = prepare(&parts.request_with_role(role), &mut openai)
            .await
            .unwrap();
        let tools = prepared.request_properties["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0],
            serde_json::to_value(openai.web_search.tool().unwrap()).unwrap()
        );
        assert!(tools[0].get("strict").is_none());
    }
    openai
        .additional_params
        .insert("tool_choice".into(), json!({"type":"web_search"}));
    assert_eq!(
        prepare(&parts.request(), &mut openai)
            .await
            .unwrap()
            .request_properties["tool_choice"],
        json!({"type":"web_search"})
    );
    parts.allowed_tool_names = Some(vec!["command".into()]);
    let error = prepare(&parts.request(), &mut openai)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("tool_choice")
            && error.contains("web_search.enabled")
            && error.contains("profile")
    );
    let maintenance = prepare(&parts.maintenance_request(), &mut openai)
        .await
        .unwrap();
    assert_eq!(maintenance.request_properties["tools"], json!([]));
    assert!(maintenance.request_properties.get("tool_choice").is_none());
    parts.allowed_tool_names = None;
    openai.tools = ToolServer::new()
        .tool(CountTool {
            executions: Arc::new(AtomicUsize::new(0)),
        })
        .run();
    openai.additional_params.insert("tool_choice".into(), json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"web_search"},{"type":"function","name":"count_once"}]}));
    let mixed = prepare(&parts.request(), &mut openai).await.unwrap();
    let tools = mixed.request_properties["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["strict"], true);
    assert!(tools[1].get("strict").is_none());
}

#[tokio::test]
async fn raw_activity_and_completed_blocks_reconcile_late_and_terminal_events() {
    let progress = discard_updates();
    let mut state = AttemptState {
        search: Some(crate::search::SearchStream::new(
            test_profile_ref(),
            &progress,
        )),
        ..AttemptState::default()
    };
    for event in [
        json!({"type":"response.web_search_call.searching","item_id":"ws1","output_index":0}),
        json!({"type":"response.web_search_call.completed","item_id":"ws1","output_index":0}),
        json!({"type":"response.web_search_call.in_progress","item_id":"ws1","output_index":0}),
        json!({"type":"response.output_item.done","output_index":1,"item":search_item("ws2",json!({"type":"open_page","url":"https://example.org"}))}),
        json!({"type":"response.output_item.done","output_index":2,"item":search_item("ws3",json!({"type":"find_in_page","url":"https://example.org","pattern":"rust"}))}),
        json!({"type":"response.output_item.done","output_index":3,"item":search_item("ws4",json!({"type":"future_action","opaque":[1,2]}))}),
        json!({"type":"response.output_text.done","output_index":4,"content_index":0,"text":"🦀 Rust is fast."}),
        json!({"type":"response.output_text.annotation.added","output_index":4,"content_index":0,"annotation_index":0,"annotation":cited_item()["content"][0]["annotations"][0]}),
    ] {
        state.observe_search(&event.to_string(), &progress).await;
    }
    assert_eq!(
        zevria_content::assistant_plain_text(&state.streaming_message().unwrap()),
        "🦀 Rust is fast. [Rust guide](https://www.rust-lang.org/learn)"
    );
    assert_eq!(
        state.search.as_ref().unwrap().attempt().activity[0].status,
        Status::Completed
    );
    let part = json!({"type":"response.content_part.done","output_index":4,"content_index":0,"part":cited_item()["content"][0]});
    state.observe_search(&part.to_string(), &progress).await;
    let before = zevria_content::assistant_plain_text(&state.streaming_message().unwrap());
    assert_eq!(before.matches("[Rust guide]").count(), 1);
    state.observe_search(&part.to_string(), &progress).await;
    let terminal = completed_output_event(
        "r",
        vec![
            search_item(
                "ws1",
                json!({"type":"search","queries":["Rust","language"]}),
            ),
            search_item(
                "ws2",
                json!({"type":"open_page","url":"https://example.org/final"}),
            ),
            search_item(
                "ws3",
                json!({"type":"find_in_page","pattern":"rust","url":"https://example.org"}),
            ),
            search_item("ws4", json!({"type":"future_action","opaque":[3]})),
            cited_item(),
        ],
    );
    state.observe_search(&terminal.to_string(), &progress).await;
    assert_eq!(
        zevria_content::assistant_plain_text(&state.streaming_message().unwrap()),
        before
    );
    state.finish_search(true, &progress).await.unwrap();
    let attempts = progress.drain_web_search(Outcome::Interrupted);
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Outcome::Completed);
    assert_eq!(attempts[0].activity.len(), 4);
    assert_eq!(
        attempts[0].activity[1].action.as_ref().unwrap()["url"],
        "https://example.org/final"
    );
    assert_eq!(
        attempts[0].activity[3].action.as_ref().unwrap()["opaque"],
        json!([3])
    );
    assert!(progress.drain_web_search(Outcome::Failed).is_empty());
}

#[tokio::test]
async fn opted_in_http_search_is_durable_model_inert_and_portable_without_local_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let output = vec![
        search_item("ws", json!({"type":"search","query":"Rust"})),
        cited_item(),
    ];
    let native = output.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut socket).await;
        assert_eq!(request.body["tools"][0]["type"], "web_search");
        let events = [
            json!({"type":"response.web_search_call.searching","item_id":"ws","output_index":0}),
            output_text_delta_at("🦀 Rust is fast.", 2, 1, "cited"),
            completed_output_event("r", output),
        ];
        let body = events
            .iter()
            .map(|event| sse_data(event.to_string()))
            .collect::<String>();
        send_http_response(&mut socket, "200 OK", "text/event-stream", body).await;
    });
    let tools = ToolServer::new().run();
    let mut openai = connect_http_test_provider(url, tools.clone()).await;
    openai.web_search = search_config();
    openai.input_token_count_url = None;
    let directory = tempfile::tempdir().unwrap();
    let transcript = TranscriptWriter::create(directory.path()).unwrap();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        openai,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(256);
    engine
        .handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "Search".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    server.await.unwrap();
    let items = transcript::load(&path).unwrap();
    let attempt_index = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
        .unwrap();
    let response_index = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::ProviderMessage(_)))
        .unwrap();
    assert!(attempt_index < response_index);
    let TranscriptItem::WebSearchAttempt(attempt) = &items[attempt_index] else {
        unreachable!()
    };
    assert_eq!(attempt.outcome, Outcome::Completed);
    assert!(attempt.presentation_elided);
    assert!(attempt.presentation.is_empty());
    assert_eq!(attempt.activity.len(), 1);
    let replay = items[response_index].provider_replay().unwrap();
    assert_eq!(replay.items, native);
    let reconstructed = zevria_transcript::reconstruct_transcript(&items);
    let TranscriptItem::WebSearchAttempt(restored) = &reconstructed[attempt_index] else {
        unreachable!()
    };
    let mut expected = attempt.clone();
    expected.reconcile_native_presentation(&native);
    assert_eq!(restored, &expected);
    assert!(!restored.presentation_elided);
    assert!(!restored.presentation.is_empty());
    restored.validate().unwrap();
    let portable = replay.portable_projection().unwrap().message.unwrap();
    assert!(
        zevria_content::assistant_plain_text(&portable)
            .contains("[Rust guide](https://www.rust-lang.org/learn)")
    );
    assert!(
        matches!(&portable, Message::Assistant { content, .. } if content.iter().all(|block| !matches!(block,AssistantContent::ToolCall(_))))
    );
    let input = transcript::model_input(&items);
    assert!(
        input
            .iter()
            .all(|item| !format!("{item:?}").contains("zevria_web_search_attempt"))
    );
    let updates = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    let progress_index = updates
        .iter()
        .position(|update| {
            matches!(
                update,
                SessionUpdate::Lifecycle(SessionEvent::WebSearchUpdated { .. })
            )
        })
        .unwrap();
    let final_index = updates
        .iter()
        .position(|update| {
            matches!(
                update,
                SessionUpdate::Lifecycle(SessionEvent::TurnCompleted { .. })
            )
        })
        .unwrap();
    assert!(progress_index < final_index);
    assert!(!updates.iter().any(|update| matches!(
        update,
        SessionUpdate::Lifecycle(SessionEvent::ToolResults { .. })
    )));
}

#[tokio::test]
async fn http_idle_retry_keeps_distinct_observed_attempts_and_identical_controls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut stalled = Vec::new();
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(receive_http_json(&mut socket).await.body);
            let progress = json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":search_item("same-provider-id",json!({"type":"search"}))});
            let mut body = sse_data(progress.to_string());
            body.push_str(&sse_data(
                output_text_delta_at(
                    if index == 0 {
                        "failed draft"
                    } else {
                        "fresh retry"
                    },
                    2,
                    1,
                    "cited",
                )
                .to_string(),
            ));
            if index == 1 {
                body.push_str(&sse_data(
                    completed_output_event(
                        "r",
                        vec![
                            search_item("same-provider-id", json!({"type":"search"})),
                            cited_item(),
                        ],
                    )
                    .to_string(),
                ));
            }
            if index == 0 {
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len() + 1000, body).as_bytes()).await.unwrap();
                stalled.push(socket);
            } else {
                send_http_response(&mut socket, "200 OK", "text/event-stream", body).await;
            }
        }
        assert_eq!(requests[0], requests[1]);
    });
    let mut openai = connect_http_test_provider(url, ToolServer::new().run()).await;
    openai.web_search = search_config();
    let progress = discard_updates();
    let mut state = AttemptState::default();
    run_turn_request_with_recovery(
        parts().request(),
        &mut openai,
        &mut state,
        &progress,
        &RecoveryPolicy {
            response_idle_timeout: std::time::Duration::from_millis(100),
            ..fast_recovery_policy(2)
        },
    )
    .await
    .unwrap();
    server.await.unwrap();
    let attempts = progress.drain_web_search(Outcome::Completed);
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].id, attempts[1].id);
    assert_eq!(attempts[0].outcome, Outcome::Failed);
    assert_eq!(attempts[0].activity[0].status, Status::Completed);
    assert_eq!(attempts[1].outcome, Outcome::Completed);
    assert!(
        serde_json::to_string(&attempts[0])
            .unwrap()
            .contains("failed draft")
    );
    let completed = serde_json::to_string(&attempts[1]).unwrap();
    assert!(!completed.contains("failed draft"));
    assert!(
        !completed.contains("fresh retry"),
        "canonical replay replaces the preview"
    );
}

#[tokio::test]
async fn websocket_to_http_fallback_retains_activity_and_search_definition() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        let ws = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({"type":"response.web_search_call.searching","item_id":"ws","output_index":0}),
        )
        .await;
        send_json(
            &mut socket,
            output_text_delta_at("abandoned websocket draft", 1, 1, "cited"),
        )
        .await;
        socket.close(None).await.unwrap();
        let (mut http, _) = listener.accept().await.unwrap();
        let http_request = receive_http_json(&mut http).await.body;
        assert_eq!(ws["tools"], http_request["tools"]);
        assert_eq!(ws["input"], http_request["input"]);
        send_http_response(
            &mut http,
            "200 OK",
            "text/event-stream",
            sse_data(
                completed_output_event(
                    "r",
                    vec![
                        search_item(
                            "ws",
                            json!({"type":"open_page","url":"https://example.org"}),
                        ),
                        cited_item(),
                    ],
                )
                .to_string(),
            ),
        )
        .await;
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut openai = test_session_with_url(&url, socket, None);
    openai.web_search = search_config();
    let progress = discard_updates();
    run_turn_request_with_recovery(
        parts().request(),
        &mut openai,
        &mut AttemptState::default(),
        &progress,
        &fast_recovery_policy(2),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let attempts = progress.drain_web_search(Outcome::Completed);
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].id, attempts[1].id);
    assert_eq!(attempts[0].activity[0].status, Status::Searching);
    assert_eq!(
        attempts[0].status_label(&attempts[0].activity[0]),
        "completion unconfirmed"
    );
    assert_eq!(attempts[1].activity[0].status, Status::Completed);
    assert!(
        serde_json::to_string(&attempts[0])
            .unwrap()
            .contains("abandoned websocket draft")
    );
    assert!(
        !serde_json::to_string(&attempts[1])
            .unwrap()
            .contains("abandoned websocket draft")
    );
}

#[tokio::test]
async fn counting_fallback_does_not_remove_search_and_upstream_rejection_is_not_downgraded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut count, _) = listener.accept().await.unwrap();
        let count_request = receive_http_json(&mut count).await;
        assert!(count_request.headers.contains("/responses/input_tokens"));
        send_http_response(&mut count, "404 Not Found", "application/json", "{}").await;
        let (mut completion, _) = listener.accept().await.unwrap();
        let completion_request = receive_http_json(&mut completion).await;
        assert_eq!(
            count_request.body["tools"],
            completion_request.body["tools"]
        );
        assert_eq!(
            count_request.body["input"],
            completion_request.body["input"]
        );
        send_http_response(&mut completion,"400 Bad Request","application/json",json!({"error":{"message":"return_token_budget unsupported by this model","type":"invalid_request_error","param":"tools[0].return_token_budget"}}).to_string()).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "no search-disabled retry"
        );
    });
    let mut openai = connect_http_test_provider(url, ToolServer::new().run()).await;
    openai.web_search = search_config();
    assert_eq!(
        openai.count_input_tokens(parts().request()).await.unwrap(),
        InputTokenCount::Unsupported
    );
    let error = openai
        .complete(parts().request(), discard_updates())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("return_token_budget unsupported")
    );
    assert!(error.to_string().contains("web_search.enabled=true"));
    server.await.unwrap();
}

#[tokio::test]
async fn stale_continuation_keeps_native_search_replay_and_attempt_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        let first = receive_json(&mut socket).await;
        let terminal = completed_output_event(
            "first",
            vec![search_item("ws", json!({"type":"search"})), cited_item()],
        );
        send_json(&mut socket, terminal.clone()).await;
        // This duplicate may be read only after the next request is sent.
        send_json(&mut socket, terminal).await;
        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "first");
        assert_eq!(chained["tools"], first["tools"]);
        send_json(&mut socket, json!({"type":"response.web_search_call.searching","item_id":"retry-search","output_index":0})).await;
        send_json(&mut socket, json!({"type":"error","error":{"code":"previous_response_not_found","message":"previous response no longer cached"}})).await;
        let full = receive_json(&mut socket).await;
        assert!(full.get("previous_response_id").is_none());
        assert_eq!(full["tools"], first["tools"]);
        assert!(
            full["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "web_search_call" && item["id"] == "ws")
        );
        send_json(
            &mut socket,
            completed_output_event("last", vec![cited_item()]),
        )
        .await;
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut openai = test_session_with_url(&url, socket, None);
    openai.web_search = search_config();
    let progress = discard_updates();
    let mut state = AttemptState::default();
    run_turn("first question", &mut openai, &mut state, &progress)
        .await
        .unwrap();
    recovery_followup("second question", &mut openai, &mut state, &progress)
        .await
        .unwrap();
    server.await.unwrap();
    let attempts = progress.drain_web_search(Outcome::Completed);
    assert_eq!(attempts.len(), 3);
    assert!(attempts.windows(2).all(|pair| pair[0].id != pair[1].id));
    assert_eq!(attempts[1].outcome, Outcome::Failed);
    assert_eq!(attempts[1].activity[0].status, Status::Searching);
    assert_eq!(
        attempts[1].status_label(&attempts[1].activity[0]),
        "completion unconfirmed"
    );
    assert_eq!(attempts[2].outcome, Outcome::Completed);
}

#[tokio::test]
async fn completed_text_parts_publish_in_content_order_and_without_duplicate_citations() {
    let progress = discard_updates();
    let mut state = AttemptState {
        search: Some(crate::search::SearchStream::new(
            test_profile_ref(),
            &progress,
        )),
        ..AttemptState::default()
    };
    let part = |index, text| json!({"type":"response.content_part.done","output_index":0,"content_index":index,"part":{"type":"output_text","text":text,"annotations":[]}});
    state
        .observe_search(&part(1, "second").to_string(), &progress)
        .await;
    assert!(state.streaming_message().is_err());
    state
        .observe_search(&part(0, "first").to_string(), &progress)
        .await;
    assert_eq!(
        zevria_content::assistant_plain_text(&state.streaming_message().unwrap()),
        "first\nsecond"
    );
}
