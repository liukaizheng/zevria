//! Coordinated local fault injection; no external provider or multi-minute waits.
use super::*;
use std::time::Duration;
use zevria_session_api::event::{NetworkStatus, NetworkTransport};

fn policy(attempts: usize) -> RecoveryPolicy {
    RecoveryPolicy {
        response_idle_timeout: Duration::from_millis(100),
        stall_warning: Duration::from_millis(25),
        ..fast_recovery_policy(attempts)
    }
}

async fn sse_headers(socket: &mut tokio::net::TcpStream) {
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
}

async fn chunk(socket: &mut tokio::net::TcpStream, data: &str) -> std::io::Result<()> {
    socket
        .write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes())
        .await
}

#[tokio::test]
async fn silent_http_attempts_expire_despite_keepalives_duplicates_and_partial_frames() {
    let mut created = completed_event("quiet", "preview", "");
    created["type"] = json!("response.created");
    created["response"]["status"] = json!("in_progress");
    created["response"]["output"] = json!([]);
    let prefixes = [
        String::new(),
        sse_data(created.to_string()),
        sse_data(output_text_delta("unfinished preview", 1).to_string()),
        sse_data(
            json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,
            "item":function_call("partial", "call_partial", "count_once", json!({}))})
            .to_string(),
        ),
        "data: {\"type\":\"response.output_text.delta\"".into(),
    ];
    for prefix in prefixes {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let duplicate = sse_data(created.to_string());
        let server = tokio::spawn(async move {
            let (mut old, _) = listener.accept().await.unwrap();
            let first = receive_http_json(&mut old).await;
            sse_headers(&mut old).await;
            if !prefix.is_empty() {
                chunk(&mut old, &prefix).await.unwrap();
            }
            let incomplete_frame = prefix.starts_with("data: {") && !prefix.ends_with("\n\n");
            let mut interval = tokio::time::interval(Duration::from_millis(5));
            let mut noise = 0;
            let (mut next, _) = loop {
                tokio::select! {
                    connection = listener.accept() => break connection.unwrap(),
                    _ = interval.tick(), if !incomplete_frame => {
                        // Arbitrary reads, empty frames, DONE, metadata and a
                        // repeating lifecycle status must not restart the clock.
                        let data = format!(": keepalive\n\ndata:\n\ndata: [DONE]\n\n{}{}",
                            sse_data(json!({"type":"relay.metadata"}).to_string()), duplicate);
                        if chunk(&mut old, &data).await.is_err() { continue; }
                        noise += 1;
                    }
                }
            };
            let second = receive_http_json(&mut next).await;
            assert_eq!(first.body, second.body);
            assert!(!second.body.to_string().contains("unfinished preview"));
            assert_eq!(first.headers, second.headers);
            assert!(second.body.get("network").is_none());
            send_http_response(
                &mut next,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("finished", "final", "recovered").to_string()),
            )
            .await;
            if !incomplete_frame {
                assert!(noise > 1);
            }
        });
        let mut provider = connect_http_test_provider(url, ToolServer::new().run()).await;
        let (sender, mut events) = session_event_channel(64);
        let mut state = AttemptState::default();
        run_turn_request_with_recovery(
            RequestParts {
                prompt: Message::user("authoritative input"),
                history: vec![],
                instructions: "Stable cache prefix".into(),
                allowed_tool_names: None,
            }
            .request(),
            &mut provider,
            &mut state,
            &ProgressReporter::new(sender),
            &policy(2),
        )
        .await
        .unwrap();
        let updates = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            updates
                .iter()
                .filter(|u| matches!(
                    u,
                    SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                        status: NetworkStatus::Quiet { .. },
                        ..
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            updates
                .iter()
                .filter(|u| matches!(
                    u,
                    SessionUpdate::Lifecycle(SessionEvent::TurnRetrying {
                        attempt: 2,
                        max_attempts: 2,
                        ..
                    })
                ))
                .count(),
            1
        );
        assert_eq!(state.history.len(), 2);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn warning_does_not_drop_a_partially_read_sse_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let (resume, resumed) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = receive_http_json(&mut stream).await;
        sse_headers(&mut stream).await;
        let event = sse_data(output_text_delta("preserved parser", 1).to_string());
        let split = event.len() / 2;
        chunk(&mut stream, &event[..split]).await.unwrap();
        resumed.await.unwrap();
        chunk(&mut stream, &event[split..]).await.unwrap();
        chunk(
            &mut stream,
            &sse_data(completed_event("r", "m", "preserved parser").to_string()),
        )
        .await
        .unwrap();
    });
    let mut provider = connect_http_test_provider(url, ToolServer::new().run()).await;
    let (sender, mut receiver) = session_event_channel(32);
    let request = tokio::spawn(async move {
        let mut state = AttemptState::default();
        run_turn_request_with_recovery(
            RequestParts {
                prompt: Message::user("question"),
                history: vec![],
                instructions: "stable".into(),
                allowed_tool_names: None,
            }
            .request(),
            &mut provider,
            &mut state,
            &ProgressReporter::new(sender),
            &policy(1),
        )
        .await
    });
    let mut resume = Some(resume);
    let mut warnings = 0;
    let mut progress_resumed = 0;
    while let Some(update) = receiver.recv().await {
        match update {
            SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::Quiet { .. },
                ..
            }) => {
                warnings += 1;
                resume.take().unwrap().send(()).unwrap();
            }
            SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::ProgressResumed,
                ..
            }) => progress_resumed += 1,
            SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { .. }) => {
                panic!("partial parser was discarded")
            }
            _ => {}
        }
    }
    request.await.unwrap().unwrap();
    assert_eq!((warnings, progress_resumed), (1, 1));
    server.await.unwrap();
}

#[tokio::test]
async fn websocket_ping_metadata_and_prior_terminals_cannot_keep_an_attempt_alive() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut old = accept_async(socket).await.unwrap();
        let first = receive_json(&mut old).await;
        let mut interval = tokio::time::interval(Duration::from_millis(5));
        let (next, _) = loop {
            tokio::select! {
                next = listener.accept() => break next.unwrap(),
                _ = interval.tick() => {
                    let _ = old.send(WebSocketMessage::Ping(vec![1].into())).await;
                    let _ = old.send(WebSocketMessage::Text(json!({"type":"relay.metadata"}).to_string().into())).await;
                    let _ = old.send(WebSocketMessage::Text(completed_event("old", "old_item", "late answer").to_string().into())).await;
                }
            }
        };
        let mut next = accept_async(next).await.unwrap();
        let second = receive_json(&mut next).await;
        assert_eq!(first, second);
        send_json(
            &mut next,
            completed_event("current", "current_item", "fresh answer"),
        )
        .await;
        // The old pump must be dead, not merely detached from its accumulator.
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(message)) = old.next().await {
                if matches!(message, WebSocketMessage::Close(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, Some("old"));
    let mut state = AttemptState::default();
    run_turn_request_with_recovery(
        RequestParts {
            prompt: Message::user("question"),
            history: vec![],
            instructions: "stable".into(),
            allowed_tool_names: None,
        }
        .request(),
        &mut provider,
        &mut state,
        &discard_updates(),
        &policy(3),
    )
    .await
    .unwrap();
    assert_eq!(provider.ws.socket_generation, 1);
    assert_eq!(continuation_response_id(&provider), Some("current"));
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_arguments_and_hosted_search_keep_long_http_responses_alive() {
    for kind in [
        "response.reasoning_summary_text.delta",
        "response.function_call_arguments.delta",
        "response.web_search_call.searching",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = receive_http_json(&mut stream).await;
            sse_headers(&mut stream).await;
            for sequence in 1..=8 {
                let item_id = if kind.contains("web_search") {
                    format!("search-{sequence}")
                } else {
                    "item".into()
                };
                let event = json!({"type":kind,"item_id":item_id,"output_index":0,"summary_index":0,"sequence_number":sequence,"delta":"x"});
                chunk(&mut stream, &sse_data(event.to_string()))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            chunk(
                &mut stream,
                &sse_data(completed_event("r", "m", "complete").to_string()),
            )
            .await
            .unwrap();
        });
        let mut provider = connect_http_test_provider(url, ToolServer::new().run()).await;
        let mut state = AttemptState::default();
        run_turn_request_with_recovery(
            RequestParts {
                prompt: Message::user("question"),
                history: vec![],
                instructions: "stable".into(),
                allowed_tool_names: None,
            }
            .request(),
            &mut provider,
            &mut state,
            &discard_updates(),
            &policy(1),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn core_cancellation_interrupts_every_network_wait_and_next_turn_still_works() {
    use zevria_foundation::TurnId;
    use zevria_session_api::{ControlCommand, TurnCommand};
    for phase in [
        "connect",
        "ws_send",
        "ws_stream",
        "request_start",
        "silent",
        "warning",
        "backoff",
    ] {
        tokio::time::timeout(Duration::from_secs(5), async {
            let websocket = matches!(phase, "connect" | "ws_send" | "ws_stream");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/responses", listener.local_addr().unwrap());
            let (started, ready) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut first, _) = listener.accept().await.unwrap();
                if websocket && phase != "connect" {
                    let mut first = accept_async(first).await.unwrap();
                    if phase == "ws_stream" {
                        let _ = receive_json(&mut first).await;
                        send_json(&mut first, output_text_delta("partial", 1)).await;
                    }
                    started.send(()).unwrap();
                    while let Some(Ok(message)) = first.next().await {
                        if matches!(message, WebSocketMessage::Close(_)) { break; }
                    }
                } else {
                    if phase == "connect" { let _ = receive_http_headers(&mut first).await; }
                    else {
                        let _ = receive_http_json(&mut first).await;
                        if phase == "backoff" {
                            send_http_response(&mut first, "503 Service Unavailable", "application/json", "{}").await;
                        } else if phase != "request_start" { sse_headers(&mut first).await; }
                    }
                    started.send(()).unwrap();
                    if phase != "backoff" {
                        let mut byte = [0];
                        assert!(matches!(first.read(&mut byte).await, Ok(0) | Err(_)), "{phase}: old connection must close");
                    }
                }
                let (mut next, _) = listener.accept().await.unwrap();
                if websocket {
                    let mut next = accept_async(next).await.unwrap();
                    let _ = receive_json(&mut next).await;
                    send_json(&mut next, completed_event("next", "next_message", "usable after cancel")).await;
                } else {
                    let _ = receive_http_json(&mut next).await;
                    send_http_response(&mut next, "200 OK", "text/event-stream", sse_data(completed_event("next", "next_message", "usable after cancel").to_string())).await;
                }
            });
            let tools = ToolServer::new().run();
            let mut provider = if phase == "ws_send" {
                let ws_url = url.replacen("http", "ws", 1);
                let (socket, _) = connect_async(&ws_url).await.unwrap();
                test_session_with_pump(&ws_url, OpenAiWebSocketSession::new_with_test_send_delay(socket, Duration::from_secs(60)), None)
            } else if websocket {
                let profile = resolved_profile("test", "gpt-test", url, "key", true, ReasoningSummaryLevel::Detailed,
                    ResponsesCompatibilityConfig::default(), BTreeMap::new(), RemoteCompactionConfig::default(), 128_000);
                OpenAiProvider::unconnected(&profile, ReasoningEffort::Medium, "", tools.clone(), "s", "cache").await.unwrap()
            } else { connect_http_test_provider(url, tools.clone()).await };
            provider.input_token_count_url = None;
            provider.recovery = RecoveryPolicy { stall_warning: Duration::from_millis(30), backoff_base: Duration::from_secs(60),
                backoff_cap: Duration::from_secs(60), ..RecoveryPolicy::default() };
            let directory = tempfile::tempdir().unwrap();
            let engine = SessionEngine::new(provider, tools, test_policies(), TranscriptWriter::create(directory.path()).unwrap(), Arc::new(SkillCatalog::default())).unwrap();
            let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
            let (events, mut receiver) = session_event_channel(64);
            let task = tokio::spawn(engine.run(command_rx, events));
            let submit = |text: &str| SessionCommand::Turn(TurnCommand::Submit { text: text.into(), mode: SessionMode::Build, behavior: zevria_foundation::RequestBehavior::Standard });
            commands.send(submit("cancel this request")).unwrap();
            ready.await.unwrap();
            if matches!(phase, "warning" | "backoff" | "silent" | "ws_stream" | "ws_send") {
                loop {
                    let update = receiver.recv().await.unwrap();
                    if matches!((&update, phase),
                        (SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { .. }), "backoff") |
                        (SessionUpdate::Lifecycle(SessionEvent::NetworkStatus { status: NetworkStatus::Quiet { .. }, .. }), "warning") |
                        (SessionUpdate::Lifecycle(SessionEvent::NetworkStatus { status: NetworkStatus::AwaitingResponse, .. }), "silent" | "ws_stream") |
                        (SessionUpdate::Lifecycle(SessionEvent::NetworkStatus { status: NetworkStatus::AttemptStarted, .. }), "ws_send")) { break; }
                }
            }
            commands.send(SessionCommand::Control(ControlCommand::CancelTurn { turn_id: Some(TurnId::new(1)) })).unwrap();
            let mut terminals = Vec::new();
            while let Some(update) = receiver.recv().await {
                if let SessionUpdate::Lifecycle(event) = update {
                    if matches!(event, SessionEvent::TurnCancelled { .. } | SessionEvent::TurnCompleted { .. } | SessionEvent::TurnFailed { .. }) {
                        let cancelled = matches!(event, SessionEvent::TurnCancelled { turn_id } if turn_id == TurnId::new(1));
                        terminals.push(event);
                        assert!(cancelled, "{phase}: cancellation must settle once");
                        break;
                    }
                }
            }
            commands.send(submit("next turn")).unwrap();
            loop {
                if let Some(SessionUpdate::Lifecycle(event)) = receiver.recv().await {
                    if matches!(event, SessionEvent::TurnCancelled { .. } | SessionEvent::TurnCompleted { .. } | SessionEvent::TurnFailed { .. }) {
                        assert!(matches!(event, SessionEvent::TurnCompleted { turn_id, .. } if turn_id == TurnId::new(2)), "{phase}: {event:?}");
                        terminals.push(event);
                        break;
                    }
                }
            }
            commands.send(SessionCommand::Control(ControlCommand::Shutdown)).unwrap();
            task.await.unwrap().unwrap();
            assert_eq!(terminals.len(), 2);
            assert!(!std::iter::from_fn(|| receiver.try_recv().ok()).any(|update| matches!(update,
                SessionUpdate::Lifecycle(SessionEvent::TurnCancelled { .. } | SessionEvent::TurnFailed { .. } | SessionEvent::TurnCompleted { .. }))));
            server.await.unwrap();
        }).await.unwrap_or_else(|_| panic!("cancellation did not settle during {phase}"));
    }
}

#[tokio::test]
async fn failed_lazy_handshakes_and_http_fallback_share_the_same_attempt_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = resolved_profile(
        "test",
        "gpt-test",
        format!("http://{}/responses", listener.local_addr().unwrap()),
        "key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(receive_http_headers(&mut socket).await.starts_with("GET "));
            send_http_response(
                &mut socket,
                "503 Service Unavailable",
                "application/json",
                "{}",
            )
            .await;
        }
        let (mut http, _) = listener.accept().await.unwrap();
        let _ = receive_http_json(&mut http).await;
        send_http_response(
            &mut http,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("r", "m", "done").to_string()),
        )
        .await;
    });
    let mut provider = OpenAiProvider::unconnected(
        &profile,
        zevria_foundation::ReasoningLevel::Medium,
        "",
        ToolServer::new().run(),
        "session",
        "cache",
    )
    .await
    .unwrap();
    let (sender, mut events) = session_event_channel(32);
    run_turn_request_with_recovery(
        RequestParts {
            prompt: Message::user("question"),
            history: vec![],
            instructions: "stable".into(),
            allowed_tool_names: None,
        }
        .request(),
        &mut provider,
        &mut AttemptState::default(),
        &ProgressReporter::new(sender),
        &policy(3),
    )
    .await
    .unwrap();
    let attempts = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|update| match update {
            SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::AttemptStarted,
                attempt,
                transport,
                ..
            }) => Some((attempt, transport)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attempts,
        vec![
            (1, NetworkTransport::WebSocket),
            (2, NetworkTransport::WebSocket),
            (3, NetworkTransport::Http)
        ]
    );
    server.await.unwrap();
}
