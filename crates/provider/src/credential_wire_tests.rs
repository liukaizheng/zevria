use super::*;
use crate::credential::test_support::KeyHelper;
use std::time::Duration;

fn profile(url: String, websocket: bool, source: ApiKeySource) -> ResolvedModelProfile {
    let mut profile = resolved_profile(
        "command-provider",
        "gpt-test",
        url,
        "unused",
        websocket,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    profile.responses_endpoint_mut().api_key = source;
    profile.responses_endpoint_mut().network.max_attempts = 1;
    profile
}
async fn runtime(profile: &ResolvedModelProfile) -> OpenAiProvider {
    OpenAiProvider::unconnected(
        profile,
        ReasoningEffort::Medium,
        "stable",
        "session",
        "stable-cache-key",
    )
    .await
    .unwrap()
}
fn parts() -> RequestParts {
    RequestParts {
        tools: zevria_model::ToolDeclarations::default(),
        forbid_tool_calls: false,
        prompt: Message::user("hello"),
        history: vec![],
        instructions: "stable".into(),
        allowed_tool_names: None,
    }
}
fn assert_no_retry(receiver: &mut SessionEventReceiver) {
    while let Ok(event) = receiver.try_recv() {
        assert!(
            !matches!(
                event,
                SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { .. })
            ),
            "{event:?}"
        );
    }
}
fn assert_rotated(requests: &[String], header: &str) {
    assert_eq!(requests.len(), 2);
    let (first_headers, first_body) = requests[0].split_once("\r\n\r\n").unwrap();
    let (second_headers, second_body) = requests[1].split_once("\r\n\r\n").unwrap();
    assert_eq!(
        first_body.as_bytes(),
        second_body.as_bytes(),
        "refresh must reuse the exact body"
    );
    assert!(
        first_headers
            .to_ascii_lowercase()
            .contains(&format!("{header}key-1"))
    );
    assert!(
        second_headers
            .to_ascii_lowercase()
            .contains(&format!("{header}key-2"))
    );
    let without_auth = |headers: &str| {
        headers
            .lines()
            .filter(|line| {
                !line
                    .to_ascii_lowercase()
                    .starts_with(header.split(':').next().unwrap())
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(without_auth(first_headers), without_auth(second_headers));
    assert!(!first_body.contains("key-1") && !second_body.contains("key-2"));
}

#[tokio::test]
async fn http_refresh_reuses_body_and_prompt_cache_key_without_recovery_or_secret_logs() {
    if isolate_log_test(
        "tests::command_credentials::http_refresh_reuses_body_and_prompt_cache_key_without_recovery_or_secret_logs",
    ) {
        return;
    }
    let _guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    let logs = CapturedLogs::default();
    let subscriber = captured_log_subscriber(logs.clone());
    async {
        let helper = KeyHelper::rotating();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            false,
            helper.source.clone(),
        );
        let server = tokio::spawn(async move {
            let mut requests = vec![];
            for status in ["401 Unauthorized", "200 OK"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(receive_http_request(&mut stream).await);
                send_http_response(
                    &mut stream,
                    status,
                    "text/event-stream",
                    sse_data(completed_event("resp_1", "msg_1", "hello").to_string()),
                )
                .await;
            }
            requests
        });
        let mut provider = runtime(&profile).await;
        assert_eq!(helper.runs(), 0);
        let (sender, mut receiver) = session_event_channel(128);
        provider
            .complete(parts().request(), ProgressReporter::new(sender))
            .await
            .unwrap();
        assert_eq!(helper.runs(), 2);
        assert_no_retry(&mut receiver);
        let requests = server.await.unwrap();
        assert_rotated(&requests, "authorization: bearer ");
        let body: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["prompt_cache_key"], "stable-cache-key");
    }
    .with_subscriber(subscriber)
    .await;
    let logs = logs.contents();
    assert!(logs.contains("API key helper completed") && logs.contains("refreshing once"));
    assert!(
        !logs.contains("key-1") && !logs.contains("key-2") && !logs.contains("helper.py"),
        "{logs}"
    );
}

#[tokio::test]
async fn second_http_401_is_terminal_and_403_never_refreshes() {
    for status in ["401 Unauthorized", "403 Forbidden"] {
        let helper = KeyHelper::rotating();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}/responses", listener.local_addr().unwrap()),
            false,
            helper.source.clone(),
        );
        let count = if status.starts_with("401") { 2 } else { 1 };
        let server = tokio::spawn(async move {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().await.unwrap();
                receive_http_request(&mut stream).await;
                // A retry hint must not turn a persistent 401 into a recovery attempt.
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nx-should-retry: {}\r\nConnection: close\r\n\r\n{{}}", status.starts_with("401")).as_bytes()).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut provider = runtime(&profile).await;
        provider.recovery.max_attempts = 3;
        let (sender, mut receiver) = session_event_channel(128);
        let error = provider
            .complete(parts().request(), ProgressReporter::new(sender))
            .await
            .unwrap_err();
        if count == 2 {
            assert!(
                format!("{error:#}").contains(
                    "providers.command-provider.api_key.command was rejected after a refresh"
                ),
                "{error:#}"
            );
        }
        assert_eq!(helper.runs(), count);
        assert_no_retry(&mut receiver);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn websocket_handshake_refresh_and_key_expiry_keep_the_socket_incremental() {
    let helper = KeyHelper::rotating();
    let source = helper
        .source
        .clone()
        .with_config_directory(helper.directory.path())
        .with_test_durations(Duration::from_secs(30), Duration::from_millis(80));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = profile(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        true,
        source,
    );
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_hdr_async(stream, |request: &ServerRequest, _: ServerResponse| {
            assert_eq!(request.headers()["authorization"], "Bearer key-1");
            Err(ServerResponse::builder()
                .status(401)
                .header("x-request-id", "rejected-handshake")
                .body(Some("unauthorized".into()))
                .unwrap())
        })
        .await
        .expect_err("first handshake is rejected");
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_hdr_async(
            stream,
            |request: &ServerRequest, response: ServerResponse| {
                assert_eq!(request.headers()["authorization"], "Bearer key-2");
                Ok(response)
            },
        )
        .await
        .unwrap();
        let first = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_first", "msg_first", "first answer"),
        )
        .await;
        let second = receive_json(&mut socket).await;
        assert_eq!(second["previous_response_id"], "resp_first");
        assert_eq!(second["input"].as_array().unwrap().len(), 1);
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        assert!(!second.to_string().contains("first answer"));
        send_json(
            &mut socket,
            completed_event("resp_second", "msg_second", "second answer"),
        )
        .await;
    });
    let mut provider = runtime(&profile).await;
    let mut state = AttemptState::default();
    let (sender, mut receiver) = session_event_channel(128);
    let progress = ProgressReporter::new(sender);
    run_turn_request_with_recovery(
        parts().request(),
        &mut provider,
        &mut state,
        &progress,
        &fast_recovery_policy(1),
    )
    .await
    .unwrap();
    assert_eq!(helper.runs(), 2);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let next = Message::user("next");
    let history = state.history.clone();
    let replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(&history, &replays, &next, "stable", None),
        &mut provider,
        &mut state,
        &progress,
        &fast_recovery_policy(1),
    )
    .await
    .unwrap();
    assert_eq!(
        helper.runs(),
        2,
        "healthy socket must not acquire another key"
    );
    assert_eq!(provider.transport, OpenAiTransport::WebSocket);
    assert_eq!(provider.ws.socket_generation, 0);
    assert_no_retry(&mut receiver);
    server.await.unwrap();
}

#[tokio::test]
async fn second_websocket_401_is_terminal_without_fallback() {
    let helper = KeyHelper::rotating();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = profile(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        true,
        helper.source.clone(),
    );
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            accept_hdr_async(stream, |_: &ServerRequest, _: ServerResponse| {
                Err(ServerResponse::builder()
                    .status(401)
                    .body(Some("invalid key".into()))
                    .unwrap())
            })
            .await
            .expect_err("rejected handshake");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let mut provider = runtime(&profile).await;
    let error = provider
        .complete(parts().request(), discard_updates())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("rejected after a refresh"));
    assert_eq!(helper.runs(), 2);
    assert_eq!(provider.transport, OpenAiTransport::WebSocket);
    server.await.unwrap();
}

#[tokio::test]
async fn helper_failure_is_terminal_before_network_during_http_websocket_and_preconnect() {
    for (websocket, preconnect) in [(false, false), (true, false), (true, true)] {
        let helper = KeyHelper::new(
            "print('PRIVATE_STDOUT'); sys.stderr.write('login is required'); sys.exit(9)",
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}/responses", listener.local_addr().unwrap()),
            websocket,
            helper.source.clone(),
        );
        let (sender, mut receiver) = session_event_channel(128);
        let error = if preconnect {
            OpenAiProvider::connect(
                &profile,
                ReasoningEffort::Medium,
                "stable",
                "session",
                "cache-key",
            )
            .await
            .err()
            .unwrap()
        } else {
            let mut provider = runtime(&profile).await;
            provider.recovery.max_attempts = 3;
            let error = provider
                .complete(parts().request(), ProgressReporter::new(sender))
                .await
                .unwrap_err();
            assert_eq!(provider.transport == OpenAiTransport::WebSocket, websocket);
            error
        };
        assert!(
            error.is::<crate::credential::ApiKeyCommandError>(),
            "{error:#}"
        );
        let error = format!("{error:#}");
        assert!(
            error.contains("providers.command-provider.api_key")
                && error.contains("login is required")
        );
        assert!(!error.contains("PRIVATE_STDOUT") && !error.contains("helper.py"));
        assert_eq!(helper.runs(), 1);
        assert_no_retry(&mut receiver);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn failed_helper_refresh_sends_no_second_request_and_never_recovers_or_falls_back() {
    for websocket in [false, true] {
        let helper = KeyHelper::new(
            "if n == 1:\n    print('key-1')\nelse:\n    sys.stderr.write('login renewal failed')\n    sys.exit(2)",
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}/responses", listener.local_addr().unwrap()),
            websocket,
            helper.source.clone(),
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            if websocket {
                accept_hdr_async(stream, |_: &ServerRequest, _: ServerResponse| {
                    Err(ServerResponse::builder()
                        .status(401)
                        .body(Some("expired".into()))
                        .unwrap())
                })
                .await
                .expect_err("reject the first handshake");
            } else {
                receive_http_request(&mut stream).await;
                send_http_response(&mut stream, "401 Unauthorized", "application/json", "{}").await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut provider = runtime(&profile).await;
        provider.recovery.max_attempts = 3;
        let (sender, mut receiver) = session_event_channel(128);
        let error = provider
            .complete(parts().request(), ProgressReporter::new(sender))
            .await
            .unwrap_err();
        assert!(
            error.is::<crate::credential::ApiKeyCommandError>(),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("login renewal failed"));
        assert_eq!(helper.runs(), 2);
        assert_eq!(provider.transport == OpenAiTransport::WebSocket, websocket);
        assert_no_retry(&mut receiver);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_keys_rotate_after_interval_without_reconfiguration_or_restart() {
    let helper = KeyHelper::rotating();
    let source = helper
        .source
        .clone()
        .with_config_directory(helper.directory.path())
        .with_test_durations(Duration::from_secs(30), Duration::from_millis(80));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = profile(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        false,
        source,
    );
    let server = tokio::spawn(async move {
        for index in 1..=2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            assert!(
                request
                    .headers
                    .contains(&format!("authorization: Bearer key-{index}"))
            );
            assert_eq!(request.body["prompt_cache_key"], "stable-cache-key");
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("resp", "msg", "hello").to_string()),
            )
            .await;
        }
    });
    let mut provider = runtime(&profile).await;
    for _ in 0..2 {
        provider
            .complete(parts().request(), discard_updates())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    assert_eq!(helper.runs(), 2);
    server.await.unwrap();
}

#[tokio::test]
async fn count_and_compaction_refresh_once_outside_their_deadlines() {
    for compact in [false, true] {
        let helper = KeyHelper::new("time.sleep(0.25); print('key-' + str(n))");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/responses", listener.local_addr().unwrap());
        let mut profile = profile(base.clone(), false, helper.source.clone());
        profile.responses_endpoint_mut().compaction.url = Some(format!("{base}/compact"));
        let server = tokio::spawn(async move {
            let mut requests = vec![];
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(receive_http_request(&mut stream).await);
                let body = if compact {
                    json!({"output":[{"type":"compaction", "encrypted_content":"opaque"}]})
                } else {
                    json!({"input_tokens":42})
                };
                send_http_response(
                    &mut stream,
                    if index == 0 {
                        "401 Unauthorized"
                    } else {
                        "200 OK"
                    },
                    "application/json",
                    body.to_string(),
                )
                .await;
            }
            requests
        });
        let mut provider = runtime(&profile).await;
        provider.compaction_timeout = Duration::from_millis(150);
        provider.input_token_count_timeout = Duration::from_millis(150);
        if compact {
            provider
                .compact(parts().compaction_request())
                .await
                .unwrap();
        } else {
            assert_eq!(
                provider
                    .count_input_tokens(parts().request())
                    .await
                    .unwrap(),
                InputTokenCount::Exact(42)
            );
        }
        assert_eq!(helper.runs(), 2);
        assert_rotated(&server.await.unwrap(), "authorization: bearer ");
    }
}

#[tokio::test]
async fn helper_time_is_outside_http_request_start_and_websocket_connect_deadlines() {
    for websocket in [false, true] {
        let helper = KeyHelper::new("time.sleep(0.3); print('key-' + str(n))");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}/responses", listener.local_addr().unwrap()),
            websocket,
            helper.source.clone(),
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            if websocket {
                let mut socket = accept_async(stream).await.unwrap();
                receive_json(&mut socket).await;
                send_json(&mut socket, completed_event("resp_1", "msg_1", "hello")).await;
            } else {
                receive_http_request(&mut stream).await;
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("resp_1", "msg_1", "hello").to_string()),
                )
                .await;
            }
        });
        let mut provider = runtime(&profile).await;
        let mut policy = fast_recovery_policy(1);
        policy.connect_timeout = Duration::from_millis(150);
        policy.send_timeout = Duration::from_millis(150);
        run_turn_request_with_recovery(
            parts().request(),
            &mut provider,
            &mut AttemptState::default(),
            &discard_updates(),
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(helper.runs(), 1);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn routing_model_preparation_and_factory_children_share_a_lazy_provider_cache() {
    let helper = KeyHelper::rotating();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let model = json!({"context_window_tokens":128000,"retained_user_tokens":10000,"reasoning_levels":["medium"],"reasoning_summary_level":"detailed"});
    let mut config: ProviderConfig = serde_json::from_value(json!({
        "base_url":url,"api_key":helper.source,"supports_websockets":false,
        "models":{"one":model,"two":model}
    }))
    .unwrap();
    config.set_config_directory(helper.directory.path());
    let one = ModelAssignment {
        provider: "shared".into(),
        model: "one".into(),
        reasoning_level: ReasoningEffort::Medium,
    };
    let two = ModelAssignment {
        model: "two".into(),
        ..one.clone()
    };
    let modes = ModeAssignments {
        build: one.clone(),
        plan: two.clone(),
        review: one,
        explore: two.clone(),
        builder: two,
    };
    let providers = BTreeMap::from([("shared".into(), config)]);
    let routing = ModelRouting::resolve(&providers, &modes, 90).unwrap();
    let mut root = ProviderRouter::root(&routing, "stable", "session").unwrap();
    let factory = ProviderRouterFactory::new(
        ModelRole::Explore,
        routing.for_role(ModelRole::Explore).clone(),
        ReasoningEffort::Medium,
        "stable",
    );
    let mut child = factory.clone().create("child-session").unwrap();
    root.prepare_model_update(
        ModelRole::Build,
        &zevria_model::models::ModelSelection::new(
            routing.for_role(ModelRole::Plan).profile.clone(),
            ReasoningEffort::Medium,
        ),
    )
    .unwrap();
    let _revision = serde_json::to_string(&providers).unwrap();
    assert_eq!(helper.runs(), 0);
    let server = tokio::spawn(async move {
        let mut models = vec![];
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer key-1")
            );
            models.push(request.body["model"].as_str().unwrap().to_owned());
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("resp_1", "msg_1", "hello").to_string()),
            )
            .await;
        }
        models
    });
    let parts = parts();
    root.complete(parts.request_with_role(ModelRole::Build), discard_updates())
        .await
        .unwrap();
    root.complete(parts.request_with_role(ModelRole::Plan), discard_updates())
        .await
        .unwrap();
    child
        .complete(
            parts.request_with_role(ModelRole::Explore),
            discard_updates(),
        )
        .await
        .unwrap();
    assert_eq!(helper.runs(), 1);
    assert_eq!(server.await.unwrap(), ["one", "two", "two"]);
}
