//! Provider-neutral session routing, exercised through the real transports.

use super::*;
use crate::connection::session_routing_headers;
use crate::router::profile_cache_key;

const SESSION_ID: &str = "a84c721d-8e73-45f5-af65-efd5917adff8";
const MISSING_SESSION_ID: &str = r#"{"type":"error","error":{"type":"MissingSessionID","message":"Error from provider (Console Go): Request is missing x-opencode-session and cannot be routed efficiently. Please see https://opencode.ai/docs/go/#where-can-i-use-it"}}"#;

fn session_profile(url: String, header: Option<&str>, websocket: bool) -> ResolvedModelProfile {
    let mut profile = resolved_profile(
        "gateway",
        "gpt-test",
        url,
        "test-key",
        websocket,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::from([("gateway_routing".to_string(), json!("unchanged"))]),
        RemoteCompactionConfig::default(),
        128_000,
    );
    profile.endpoint.session_id_header = header.map(ToOwned::to_owned);
    profile
}

async fn connect_profile(profile: &ResolvedModelProfile, session_id: &str) -> OpenAiProvider {
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: Arc::new(AtomicUsize::new(0)),
        })
        .run();
    OpenAiProvider::connect(
        profile,
        zevria_foundation::ReasoningLevel::Medium,
        "Session routing preamble",
        tools,
        session_id,
        &profile_cache_key(session_id, profile),
    )
    .await
    .expect("session routing provider")
}

fn assert_http_header(headers: &str, name: &str, expected: Option<&str>) {
    let values = headers
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
        .collect::<Vec<_>>();
    assert_eq!(values, expected.into_iter().collect::<Vec<_>>(), "{name}");
}

fn assert_routing_headers(headers: &str, configured: Option<&str>, session_id: &str) {
    assert_http_header(headers, "authorization", Some("Bearer test-key"));
    if let Some(name) = configured {
        assert_http_header(headers, name, Some(session_id));
    }
    for name in [
        "x-opencode-session",
        "x-conversation-id",
        "session-id",
        "thread-id",
        "x-client-request-id",
        "x-route-a",
        "x-route-b",
    ] {
        if !configured.is_some_and(|configured| configured.eq_ignore_ascii_case(name)) {
            assert_http_header(headers, name, None);
        }
    }
}

async fn session_turn(
    provider: &mut OpenAiProvider,
    state: &mut AttemptState,
    text: &str,
) -> anyhow::Result<()> {
    let prompt = Message::user(text);
    let history = state.history.clone();
    let replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(
            &history,
            &replays,
            &prompt,
            "Session header instructions",
            None,
        ),
        provider,
        state,
        &discard_updates(),
        &fast_recovery_policy(4),
    )
    .await
}

#[tokio::test]
async fn configured_session_header_fixes_missing_session_id_and_survives_tool_followup() {
    if isolate_log_test(
        "tests::session_headers::configured_session_header_fixes_missing_session_id_and_survives_tool_followup",
    ) {
        return;
    }
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    for header in ["x-opencode-session", "X-Conversation-ID"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/custom/responses?route=go",
            listener.local_addr().unwrap()
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                server_attempts.fetch_add(1, Ordering::SeqCst);
                assert!(
                    request
                        .headers
                        .starts_with("POST /custom/responses?route=go HTTP/1.1")
                );
                assert_routing_headers(
                    &request.headers,
                    (index != 0).then_some(header),
                    SESSION_ID,
                );
                if index == 0 {
                    send_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        MISSING_SESSION_ID,
                    )
                    .await;
                } else {
                    let terminal = if index == 1 {
                        completed_output_event(
                            "resp_tool",
                            vec![function_call(
                                "fc_tool",
                                "call_tool",
                                "count_once",
                                json!({}),
                            )],
                        )
                    } else {
                        completed_event("resp_final", "msg_final", "done")
                    };
                    send_http_response(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        sse_data(terminal.to_string()),
                    )
                    .await;
                }
                requests.push(request);
            }
            requests
        });

        let mut profile = session_profile(url, None, false);
        // Routing is independent of prompt-cache transmission, including for Go.
        profile.endpoint.compatibility.send_prompt_cache_key = false;
        let mut disabled = connect_profile(&profile, SESSION_ID).await;
        let mut failed_state = AttemptState::default();
        let error = session_turn(&mut disabled, &mut failed_state, "diagnostic request")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("MissingSessionID"));
        assert!(error.to_string().contains("400"));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "400 must not retry or infer a header"
        );
        assert!(failed_state.history.is_empty());
        assert!(!error.to_string().contains(SESSION_ID));

        profile.endpoint.session_id_header = Some(header.to_string());
        let mut provider = connect_profile(&profile, SESSION_ID).await;
        let logs = CapturedLogs::default();
        let mut state = AttemptState::default();
        async {
            session_turn(&mut provider, &mut state, "diagnostic request")
                .await
                .unwrap();
            state
                .history
                .push(Message::tool_result("call_tool", "count_once", "counted"));
            state.history_replays.push(None);
            session_turn(&mut provider, &mut state, "finish")
                .await
                .unwrap();
        }
        .with_subscriber(captured_log_subscriber(logs.clone()))
        .await;
        let logs = logs.contents();
        assert!(
            logs.contains("OpenAI terminal response"),
            "capture must observe the request"
        );
        assert!(!logs.contains(SESSION_ID));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(
            requests[0].body, requests[1].body,
            "opting in changes only headers"
        );
        let followup = requests[2].body["input"].to_string();
        for expected in [
            "function_call",
            "function_call_output",
            "call_tool",
            "counted",
        ] {
            assert!(followup.contains(expected), "{expected}: {followup}");
        }
        for request in requests {
            assert!(request.body.get("prompt_cache_key").is_none());
            assert!(request.body.get("previous_response_id").is_none());
            assert!(!request.body.to_string().contains(SESSION_ID));
            assert!(!request.body.to_string().contains(header));
            assert_eq!(request.body["gateway_routing"], "unchanged");
        }
    }
}

#[tokio::test]
async fn session_header_validation_precedes_io_and_values_are_sensitive() {
    // Serialize against log-capturing tests: callsite interest is process-wide.
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    let mut profile = session_profile(
        "not a Responses URL".to_string(),
        Some("X-Conversation-ID"),
        true,
    );
    for invalid in [
        "",
        " \t ",
        "private-session\r\ninjected",
        "private-session\0",
    ] {
        let error = OpenAiProvider::connect(
            &profile,
            zevria_foundation::ReasoningLevel::Medium,
            "preamble",
            ToolServer::new().run(),
            invalid,
            "valid-cache-key",
        )
        .await
        .err()
        .expect("invalid ID must fail locally");
        assert!(
            error.to_string().contains("session ID") || error.to_string().contains("session-ID")
        );
        assert!(!error.to_string().contains("endpoint URL"));
        assert!(!error.to_string().contains("private-session"));
    }
    for name in ["", "x\r\ninjected", "Authorization", "SEC-WEBSOCKET-KEY"] {
        profile.endpoint.session_id_header = Some(name.to_string());
        let error = OpenAiProvider::connect(
            &profile,
            zevria_foundation::ReasoningLevel::Medium,
            "preamble",
            ToolServer::new().run(),
            SESSION_ID,
            "valid-cache-key",
        )
        .await
        .err()
        .expect("direct profiles must validate names too");
        assert!(
            error
                .to_string()
                .contains("providers.gateway.session_id_header")
        );
        assert!(!error.to_string().contains("endpoint URL"));
    }
    profile.endpoint.base_url = "http://127.0.0.1:1/v1/responses".to_string();
    profile.endpoint.supports_websockets = false;
    profile.endpoint.session_id_header = None;
    for ignored in ["", " \t ", "unused\r\nID"] {
        let provider = connect_profile(&profile, ignored).await;
        assert!(
            provider
                .ws
                .config
                .headers
                .get("x-conversation-id")
                .is_none()
        );
    }

    profile.endpoint.session_id_header = Some("X-Conversation-ID".to_string());
    let headers = session_routing_headers(&profile, SESSION_ID).unwrap();
    assert_eq!(headers.len(), 1);
    assert!(headers["x-conversation-id"].is_sensitive());
    assert_eq!(headers["x-conversation-id"].to_str().unwrap(), SESSION_ID);
    assert!(!format!("{headers:?}").contains(SESSION_ID));
    let provider = connect_profile(&profile, SESSION_ID).await;
    assert!(provider.ws.config.headers["x-conversation-id"].is_sensitive());
    assert!(!format!("{:?}", provider.ws.config.headers).contains(SESSION_ID));
    assert!(!format!("{:?}", provider.http).contains(SESSION_ID));

    let router = ResponsesRouter::from_routes(
        [(
            ModelRole::Build,
            profile.clone(),
            zevria_foundation::ReasoningLevel::Medium,
        )],
        "preamble",
        ToolServer::new().run(),
        "\r\ninvalid",
    )
    .unwrap();
    let error = router
        .prepare_model_update(
            ModelRole::Build,
            &zevria_model::models::ModelSelection::new(
                profile.profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap_err();
    assert!(error.to_string().contains("session-ID"));
    assert_eq!(router.initialized_profile_count(), 0);
}

#[tokio::test]
async fn configured_session_headers_cover_completion_count_and_remote_compaction() {
    // Serialize against log-capturing tests: callsite interest is process-wide.
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    for header in [None, Some("X-Conversation-ID")] {
        for derive_count_url in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let auxiliary = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut profile = session_profile(
                format!("http://{}/v1/responses", listener.local_addr().unwrap()),
                header,
                false,
            );
            let auxiliary_url = format!("http://{}", auxiliary.local_addr().unwrap());
            profile.endpoint.compaction.url =
                Some(format!("{auxiliary_url}/custom/compact?route=1"));
            if !derive_count_url {
                profile.endpoint.input_token_count.url =
                    Some(format!("{auxiliary_url}/custom/count?route=1"));
            }
            profile.endpoint.compatibility.send_prompt_cache_key = false;
            let responses = tokio::spawn(async move {
                let mut requests = Vec::new();
                if derive_count_url {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = receive_http_json(&mut stream).await;
                    assert!(
                        request
                            .headers
                            .starts_with("POST /v1/responses/input_tokens HTTP/1.1")
                    );
                    send_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"input_tokens":123}"#,
                    )
                    .await;
                    requests.push(request);
                }
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(receive_http_json(&mut stream).await);
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("resp_http", "msg_http", "done").to_string()),
                )
                .await;
                requests
            });
            let auxiliary_requests = tokio::spawn(async move {
                let mut requests = Vec::new();
                if !derive_count_url {
                    let (mut stream, _) = auxiliary.accept().await.unwrap();
                    let request = receive_http_json(&mut stream).await;
                    assert!(
                        request
                            .headers
                            .starts_with("POST /custom/count?route=1 HTTP/1.1")
                    );
                    send_http_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"input_tokens":123}"#,
                    )
                    .await;
                    requests.push(request);
                }
                let (mut stream, _) = auxiliary.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                assert!(
                    request
                        .headers
                        .starts_with("POST /custom/compact?route=1 HTTP/1.1")
                );
                requests.push(request);
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
                )
                .await;
                requests
            });
            let mut provider = connect_profile(&profile, SESSION_ID).await;
            let parts = RequestParts {
                prompt: Message::user("count and compact"),
                history: vec![Message::assistant("earlier")],
                instructions: "Auxiliary request instructions".to_string(),
                allowed_tool_names: None,
            };
            assert_eq!(
                provider.count_input_tokens(parts.request()).await.unwrap(),
                InputTokenCount::Exact(123)
            );
            provider
                .complete(parts.request(), discard_updates())
                .await
                .unwrap();
            assert!(matches!(
                provider.compact(parts.maintenance_request()).await.unwrap(),
                CompactResult::Replacement(_)
            ));
            let mut requests = responses.await.unwrap();
            requests.extend(auxiliary_requests.await.unwrap());
            assert_eq!(requests.len(), 3);
            for request in requests {
                assert_routing_headers(&request.headers, header, SESSION_ID);
                assert!(!request.body.to_string().contains(SESSION_ID));
                assert!(request.body.get("session_id_header").is_none());
                assert!(request.body.get("prompt_cache_key").is_none());
                assert_eq!(request.body["gateway_routing"], "unchanged");
                if request.headers.contains("/count?") || request.headers.contains("/input_tokens ")
                {
                    for field in [
                        "stream",
                        "previous_response_id",
                        "background",
                        "include",
                        "store",
                    ] {
                        assert!(
                            request.body.get(field).is_none(),
                            "count body retained {field}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn session_headers_survive_websocket_recovery_http_retries_and_sticky_fallback() {
    // Serialize against log-capturing tests: callsite interest is process-wide.
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    for startup_426 in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = session_profile(
            format!(
                "ws://{}/v1/responses?fallback=1",
                listener.local_addr().unwrap()
            ),
            Some("X-Conversation-ID"),
            true,
        );
        let server = tokio::spawn(async move {
            let mut websocket_requests = Vec::new();
            if startup_426 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let headers = receive_http_headers(&mut stream).await;
                assert!(headers.starts_with("GET /v1/responses?fallback=1 HTTP/1.1"));
                assert_routing_headers(&headers, Some("X-Conversation-ID"), SESSION_ID);
                send_http_response(&mut stream, "426 Upgrade Required", "application/json", "")
                    .await;
            } else {
                // Lose both the initial and replacement sockets after receiving
                // the immutable request, forcing sticky HTTP fallback.
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut socket = accept_hdr_async(
                        stream,
                        AssertSessionWebSocketHandshake(Some(("X-Conversation-ID", SESSION_ID))),
                    )
                    .await
                    .unwrap();
                    websocket_requests.push(receive_json(&mut socket).await);
                }
            }
            let mut http_requests = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                assert_routing_headers(&request.headers, Some("X-Conversation-ID"), SESSION_ID);
                assert!(
                    request
                        .headers
                        .starts_with("POST /v1/responses?fallback=1 HTTP/1.1")
                );
                if index == 0 {
                    send_http_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json",
                        "{}",
                    )
                    .await;
                } else {
                    send_http_response(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        sse_data(
                            completed_event(
                                &format!("resp_{index}"),
                                &format!("msg_{index}"),
                                "done",
                            )
                            .to_string(),
                        ),
                    )
                    .await;
                }
                http_requests.push(request);
            }
            (websocket_requests, http_requests)
        });
        let mut provider = connect_profile(&profile, SESSION_ID).await;
        assert_eq!(provider.transport == OpenAiTransport::Http, startup_426);
        let mut state = AttemptState::default();
        session_turn(&mut provider, &mut state, "fallback")
            .await
            .unwrap();
        assert_eq!(provider.transport, OpenAiTransport::Http);
        session_turn(&mut provider, &mut state, "sticky HTTP followup")
            .await
            .unwrap();
        assert!(provider.ws.continuation.is_none());
        let (websocket, http) = server.await.unwrap();
        assert_eq!(http.len(), 3, "initial HTTP attempt, retry, and followup");
        assert_eq!(http[0].body, http[1].body);
        if !startup_426 {
            assert_eq!(websocket.len(), 2);
            assert_eq!(websocket[0], websocket[1]);
            assert_eq!(websocket[0]["input"], http[0].body["input"]);
        }
        for request in http {
            assert_eq!(
                request.body["prompt_cache_key"],
                profile_cache_key(SESSION_ID, &profile)
            );
            assert!(request.body.get("previous_response_id").is_none());
            assert!(request.body.get("type").is_none());
        }
    }
}

#[tokio::test]
async fn websocket_session_headers_are_stable_through_continuation_reset_and_cancel() {
    // Serialize against log-capturing tests: callsite interest is process-wide.
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    for header in [None, Some("Session-ID")] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = session_profile(
            format!("ws://{}/v1/responses", listener.local_addr().unwrap()),
            header,
            true,
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(
                stream,
                AssertSessionWebSocketHandshake(header.map(|name| (name, SESSION_ID))),
            )
            .await
            .unwrap();
            for index in 0..3 {
                let request = receive_json(&mut socket).await;
                if index == 1 {
                    assert_eq!(request["previous_response_id"], "resp_0");
                } else {
                    assert!(request.get("previous_response_id").is_none());
                }
                send_json(
                    &mut socket,
                    completed_event(&format!("resp_{index}"), &format!("msg_{index}"), "done"),
                )
                .await;
            }
            let _ = socket.next().await;
            drop(socket);
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(
                stream,
                AssertSessionWebSocketHandshake(header.map(|name| (name, SESSION_ID))),
            )
            .await
            .unwrap();
            let request = receive_json(&mut socket).await;
            assert!(request.get("previous_response_id").is_none());
            send_json(
                &mut socket,
                completed_event("resp_after_cancel", "msg_after_cancel", "done"),
            )
            .await;
        });
        let mut provider = connect_profile(&profile, SESSION_ID).await;
        let mut state = AttemptState::default();
        session_turn(&mut provider, &mut state, "first")
            .await
            .unwrap();
        session_turn(&mut provider, &mut state, "continue")
            .await
            .unwrap();
        provider.reset();
        assert!(provider.ws.continuation.is_none());
        session_turn(&mut provider, &mut state, "after reset")
            .await
            .unwrap();
        provider.cancel();
        assert!(provider.ws.continuation.is_none());
        session_turn(&mut provider, &mut state, "after cancel")
            .await
            .unwrap();
        assert_eq!(provider.ws.socket_generation, 1);
        assert_eq!(
            continuation_response_id(&provider),
            Some("resp_after_cancel")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn router_session_headers_are_provider_scoped_and_children_are_independent() {
    // Serialize against log-capturing tests: callsite interest is process-wide.
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..7 {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(receive_http_json(&mut stream).await);
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(
                    completed_event(&format!("resp_{index}"), &format!("msg_{index}"), "done")
                        .to_string(),
                ),
            )
            .await;
        }
        requests
    });
    let mut a = session_profile(url.clone(), Some("X-Route-A"), false);
    a.profile = zevria_foundation::ModelProfileRef::new("provider-a", "model-a");
    let mut a2 = a.clone();
    a2.profile.model = "model-a2".to_string();
    let mut b = session_profile(url.clone(), Some("x-route-b"), false);
    b.profile = zevria_foundation::ModelProfileRef::new("provider-b", "model-a");
    let mut unconfigured = session_profile(url, None, false);
    unconfigured.profile = zevria_foundation::ModelProfileRef::new("unconfigured", "model-a");
    let routes = [
        (ModelRole::Build, a.clone()),
        (ModelRole::Plan, b.clone()),
        (ModelRole::Review, a2.clone()),
        (ModelRole::Explore, unconfigured.clone()),
    ];
    let mut router = ResponsesRouter::from_routes(
        routes
            .clone()
            .map(|(role, profile)| (role, profile, zevria_foundation::ReasoningLevel::Medium)),
        "preamble",
        ToolServer::new().run(),
        SESSION_ID,
    )
    .unwrap();
    assert_eq!(router.initialized_profile_count(), 0);
    let parts = RequestParts {
        prompt: Message::user("routing"),
        history: Vec::new(),
        instructions: "Routing test".to_string(),
        allowed_tool_names: None,
    };
    for (role, _) in &routes {
        router
            .complete(parts.request_with_role(*role), discard_updates())
            .await
            .unwrap();
    }
    let update = zevria_model::models::ModelSelection::new(
        b.profile.clone(),
        zevria_foundation::ReasoningLevel::Medium,
    );
    router
        .prepare_model_update(ModelRole::Build, &update)
        .unwrap();
    router.install_model_update(ModelRole::Build, &update);
    router
        .complete(parts.request(), discard_updates())
        .await
        .unwrap();
    assert_eq!(router.initialized_profile_count(), 4);

    let factory = ResponsesRouterFactory::new(
        ModelRole::Explore,
        a.clone(),
        zevria_foundation::ReasoningLevel::Medium,
        "preamble",
    );
    for id in ["child-one", "child-two"] {
        let mut child = factory.create(id, ToolServer::new().run()).unwrap();
        child
            .complete(
                parts.request_with_role(ModelRole::Explore),
                discard_updates(),
            )
            .await
            .unwrap();
    }
    let expected = [
        (&a, SESSION_ID),
        (&b, SESSION_ID),
        (&a2, SESSION_ID),
        (&unconfigured, SESSION_ID),
        (&b, SESSION_ID),
        (&a, "child-one"),
        (&a, "child-two"),
    ];
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), expected.len());
    for (request, (profile, id)) in requests.iter().zip(expected) {
        assert_routing_headers(
            &request.headers,
            profile.endpoint.session_id_header.as_deref(),
            id,
        );
        assert_eq!(request.body["model"], profile.profile.model);
        assert_eq!(
            request.body["prompt_cache_key"],
            profile_cache_key(id, profile)
        );
        assert!(request.body.get("session_id_header").is_none());
    }
    for index in [1, 2, 5, 6] {
        assert_ne!(
            requests[0].body["prompt_cache_key"],
            requests[index].body["prompt_cache_key"]
        );
    }
    assert_ne!(
        requests[5].body["prompt_cache_key"],
        requests[6].body["prompt_cache_key"]
    );
}
