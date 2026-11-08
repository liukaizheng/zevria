use super::*;
#[path = "credential_tests.rs"]
mod command_credentials;
#[path = "integration_tests.rs"]
mod integration;
#[path = "outage_tests.rs"]
mod outages;
use crate::tests::receive_http_request;
#[path = "local_summary_tests.rs"]
mod local_summary;
use crate::AnthropicProvider;
use zevria_model::ModelInputTooLarge;

fn profile(url: String) -> ResolvedModelProfile {
    let provider: ProviderConfig = serde_json::from_value(json!({
        "protocol":"anthropic.messages", "base_url":url, "api_key":"test-secret", "session_id_header":"X-Session-ID",
        "compatibility":{"strict_thinking_binding":true}, "beta_headers":["gateway-beta"],
        "models":{"claude-opus-5-5":{"context_window_tokens":1000000,"max_output_tokens":64000,"retained_user_tokens":20000,"reasoning_levels":["low","medium","high","xhigh","max"],"thinking_display":"updates","refusal_fallback":"default"}}
    })).unwrap();
    let assignment = ModelAssignment {
        provider: "anthropic".into(),
        model: "claude-opus-5-5".into(),
        reasoning_level: zevria_foundation::ReasoningLevel::High,
    };
    let modes = ModeAssignments {
        build: assignment.clone(),
        plan: assignment.clone(),
        review: assignment.clone(),
        explore: assignment.clone(),
        builder: assignment,
    };
    ModelRouting::resolve(
        &BTreeMap::from([("anthropic".into(), provider)]),
        &modes,
        90,
    )
    .unwrap()
    .for_role(ModelRole::Build)
    .clone()
}
fn response(blocks: Vec<Value>, stop: &str) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_1","model":"served-model", "content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}}}),
    ];
    for (index, block) in blocks.into_iter().enumerate() {
        events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_details":{"category":"test-category"}},"usage":{"output_tokens":4}}));
    events.push(json!({"type":"message_stop"}));
    events
        .into_iter()
        .map(|v| sse_data(v.to_string()))
        .collect()
}
fn answer() -> String {
    response(vec![json!({"type":"text","text":"hello"})], "end_turn")
}
fn runtime(profile: &ResolvedModelProfile) -> AnthropicProvider {
    let mut provider = AnthropicProvider::unconnected(
        profile,
        zevria_foundation::ReasoningLevel::High,
        "stable",
        "session-1",
    )
    .unwrap();
    provider.recovery.backoff_base = std::time::Duration::from_millis(1);
    provider
}
fn request(user: &Message) -> ModelRequest<'_> {
    ModelRequest {
        tools: &[],
        forbid_tool_calls: false,
        instructions: "stable",
        input: vec![ModelRequestItem::message(user)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    }
}

#[tokio::test]
async fn headers_streaming_usage_counting_and_local_compaction() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [answer(), json!({"input_tokens":123}).to_string()] {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(receive_http_json(&mut stream).await);
            send_http_response(&mut stream, "200 OK", "text/event-stream", body).await;
        }
        requests
    });
    let mut provider = runtime(&profile(url));
    let user = Message::user("hello");
    let (sender, mut receiver) = session_event_channel(64);
    let response = provider
        .complete(request(&user), ProgressReporter::new(sender))
        .await
        .unwrap();
    assert_eq!(response.usage.unwrap().input_tokens, 60);
    assert_eq!(
        response.record().provider_replay().unwrap().provider,
        "anthropic.messages"
    );
    let mut saw_usage = false;
    while let Ok(update) = receiver.try_recv() {
        saw_usage |= matches!(
            update,
            SessionUpdate::Lifecycle(SessionEvent::UsageUpdated { .. })
        );
    }
    assert!(saw_usage);
    assert_eq!(
        provider.count_input_tokens(request(&user)).await.unwrap(),
        InputTokenCount::Exact(123)
    );
    assert_eq!(
        provider.compact(request(&user)).await.unwrap(),
        CompactResult::Unsupported
    );
    let requests = server.await.unwrap();
    let headers = requests[0].headers.to_ascii_lowercase();
    for expected in [
        "x-api-key: test-secret",
        "anthropic-version: 2026-10-21",
        "x-session-id: session-1",
        "thinking-binding-controls-2026-08-01",
        "thinking-display-updates-2026-08-18",
        "server-side-fallback-2026-07-01",
        "gateway-beta",
        "inline-tools-2026-09-15",
    ] {
        assert!(headers.contains(expected), "{expected}");
    }
    assert!(
        requests[1]
            .headers
            .starts_with("POST /v1/messages/count_tokens ")
    );
    assert!(requests[1].headers.contains("inline-tools-2026-09-15"));
    assert_eq!(
        requests[1].body,
        zevria_anthropic::body::count_tokens_body(&requests[0].body)
    );
}

#[tokio::test]
async fn retryable_statuses_and_stream_errors_retry_the_identical_request() {
    for status in [
        "529 Overloaded",
        "500 Internal Server Error",
        "429 Too Many Requests",
        "200 OK",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = receive_http_json(&mut stream).await;
            let body = if status == "200 OK" {
                sse_data(json!({"type":"error","error":{"type":"overloaded_error","message":"overload"}}).to_string())
            } else {
                json!({"error":{"type":"api_error","message":"overload"}}).to_string()
            };
            send_http_response(&mut stream, status, "text/event-stream", body).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let second = receive_http_json(&mut stream).await;
            send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
            assert_eq!(first.body, second.body);
        });
        let mut provider = runtime(&profile(url));
        provider
            .complete(request(&Message::user("retry")), discard_updates())
            .await
            .unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn terminal_statuses_stops_and_prompt_capacity_are_not_retried() {
    for (status, body, too_large) in [
        (
            "401 Unauthorized",
            json!({"error":{"message":"invalid key"}}).to_string(),
            false,
        ),
        (
            "400 Bad Request",
            json!({"error":{"message":"bad request"}}).to_string(),
            false,
        ),
        (
            "400 Bad Request",
            json!({"error":{"message":"prompt is too long"}}).to_string(),
            true,
        ),
        ("413 Too Large", String::new(), true),
        (
            "200 OK",
            response(vec![json!({"type":"text","text":"partial"})], "refusal"),
            false,
        ),
        (
            "200 OK",
            response(vec![json!({"type":"text","text":"partial"})], "max_tokens"),
            false,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_json(&mut stream).await;
            send_http_response(&mut stream, status, "text/event-stream", body).await;
        });
        let mut provider = runtime(&profile(url));
        let error = provider
            .complete(request(&Message::user("terminal")), discard_updates())
            .await
            .unwrap_err();
        assert_eq!(error.is::<ModelInputTooLarge>(), too_large, "{error:?}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn unavailable_counting_is_cached_and_bearer_auth_is_supported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret")
        );
        send_http_response(&mut stream, "404 Not Found", "application/json", "{}").await;
    });
    let mut profile = profile(url);
    if let ProfileProtocol::Anthropic { endpoint, .. } = &mut profile.protocol {
        endpoint.auth = AnthropicAuth::Bearer;
    }
    let mut provider = runtime(&profile);
    for _ in 0..2 {
        assert_eq!(
            provider
                .count_input_tokens(request(&Message::user("count")))
                .await
                .unwrap(),
            InputTokenCount::Unsupported
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn binding_mismatch_retries_once_with_thinking_removed_and_new_binding() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            captured.push(receive_http_json(&mut stream).await);
            if index == 0 {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    response(
                        vec![
                            json!({"type":"thinking","thinking":"", "signature":"  opaque\n"}),
                            json!({"type":"text","text":"answer"}),
                        ],
                        "end_turn",
                    ),
                )
                .await;
            } else if index == 1 {
                send_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    json!({"error":{"message":"thinking block bound to a different conversation"}})
                        .to_string(),
                )
                .await;
            } else {
                send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
            }
        }
        captured
    });
    let mut provider = runtime(&profile(url));
    let user = Message::user("first");
    let next = Message::user("next");
    let first = provider
        .complete(request(&user), discard_updates())
        .await
        .unwrap();
    let replay =
        zevria_model::ReplayMessage::new(first.record().provider_replay().unwrap().clone())
            .unwrap();
    let req = ModelRequest {
        tools: &[],
        forbid_tool_calls: false,
        instructions: "stable",
        input: vec![
            ModelRequestItem::message(&user),
            ModelRequestItem::replay_backed(&replay),
            ModelRequestItem::message(&next),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let last = provider.complete(req, discard_updates()).await.unwrap();
    let captured = server.await.unwrap();
    assert_eq!(
        captured[1].body["messages"][1]["content"][0]["signature"],
        "  opaque\n"
    );
    assert_eq!(
        captured[2].body["messages"][1]["content"][0]["type"],
        "text"
    );
    let body = &captured[2].body;
    assert_eq!(
        last.record()
            .provider_replay()
            .unwrap()
            .prefix_binding
            .as_deref(),
        Some(
            zevria_anthropic::binding::request_binding(
                &body["system"],
                &body["tools"],
                body["messages"].as_array().unwrap()
            )
            .as_str()
        )
    );
}
