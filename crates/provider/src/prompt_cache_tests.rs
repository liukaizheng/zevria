//! Permanent opt-in request compatibility tests, run with the feature on/off.
use super::*;

#[tokio::test]
async fn inactivity_replay_preserves_wire_prefix_controls_headers_and_cache_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let original = receive_http_json(&mut first).await;
        let partial =
            sse_data(output_text_delta("failed answer must not enter input", 1).to_string());
        first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", partial.len() + 1000, partial).as_bytes()).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let retry = receive_http_json(&mut second).await;
        assert_eq!(original.body, retry.body);
        assert_eq!(original.headers, retry.headers);
        assert_eq!(retry.body["prompt_cache_key"], "stable-cache-key");
        assert_eq!(retry.body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(
            retry.body["prompt_cache_options"]["comparison_response_id"],
            "explicit-baseline"
        );
        for field in [
            "network",
            "attempt",
            "retry_after",
            "response_idle_timeout_seconds",
        ] {
            assert!(retry.body.get(field).is_none());
        }
        assert!(!retry.body["input"].to_string().contains("failed answer"));
        assert!(
            retry
                .headers
                .to_ascii_lowercase()
                .contains("x-session: stable-session")
        );
        send_http_response(
            &mut second,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("r", "m", "done").to_string()),
        )
        .await;
    });
    let mut profile = resolved_profile(
        "test",
        "gpt-test",
        url,
        "key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::from([(
            "prompt_cache_options".into(),
            json!({"comparison_response_id":"explicit-baseline", "mode":"implicit"}),
        )]),
        RemoteCompactionConfig::default(),
        128_000,
    );
    profile.endpoint.session_id_header = Some("x-session".into());
    profile.endpoint.network = NetworkConfig {
        connect_timeout_seconds: 11,
        request_start_timeout_seconds: 7,
        stall_warning_seconds: 3,
        response_idle_timeout_seconds: 42,
        max_attempts: 2,
    };
    let mut provider = OpenAiProvider::unconnected(
        &profile,
        ReasoningEffort::Medium,
        "cache prefix",
        policy_tools(),
        "stable-session",
        "stable-cache-key",
    )
    .await
    .unwrap();
    assert_eq!(provider.recovery.connect_timeout.as_secs(), 11);
    assert_eq!(provider.recovery.send_timeout.as_secs(), 7);
    assert_eq!(provider.recovery.stall_warning.as_secs(), 3);
    assert_eq!(provider.recovery.response_idle_timeout.as_secs(), 42);
    assert_eq!(provider.recovery.max_attempts, 2);
    let prompt = Message::user("authoritative input");
    let request = || ModelRequest {
        instructions: test_instructions(),
        input: vec![ModelRequestItem::message(&prompt)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let before = crate::turn::prepare_turn_request(&request(), &mut provider)
        .await
        .unwrap();
    provider.recovery = RecoveryPolicy {
        response_idle_timeout: std::time::Duration::from_millis(80),
        ..fast_recovery_policy(2)
    };
    let after = crate::turn::prepare_turn_request(&request(), &mut provider)
        .await
        .unwrap();
    assert_eq!(before.request_properties, after.request_properties);
    assert_eq!(before.full_input, after.full_input);
    provider
        .complete(request(), discard_updates())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn explicit_comparison_survives_http_serialization_unchanged() {
    let options = json!({"comparison_response_id":"resp_synthetic_baseline", "mode":"implicit", "ttl":"30m", "future":{"retain":true}});
    let request = capture_http_request_with_options(
        ResponsesCompatibilityConfig::default(),
        BTreeMap::from([("prompt_cache_options".into(), options.clone())]),
        policy_tools(),
    )
    .await;
    assert_eq!(request.body["prompt_cache_options"], options);
    assert!(request.body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn comparison_id_changes_preserve_socket_continuation_but_cache_mode_does_not() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut first_input = Vec::new();
        let mut first_output = Vec::new();
        for index in 0..3 {
            let request = receive_json(&mut socket).await;
            assert_eq!(
                request["prompt_cache_options"]["comparison_response_id"],
                format!("resp_explicit_baseline_{index}")
            );
            assert_eq!(
                request["prompt_cache_options"]["mode"],
                if index == 2 { "explicit" } else { "implicit" }
            );
            assert_eq!(request["prompt_cache_options"]["ttl"], "30m");
            if index == 1 {
                assert_eq!(request["previous_response_id"], "resp_synthetic_0");
                assert_eq!(request["input"].as_array().unwrap().len(), 1);
            } else {
                assert!(request.get("previous_response_id").is_none());
            }
            let event = completed_event(
                &format!("resp_synthetic_{index}"),
                &format!("msg_synthetic_{index}"),
                "synthetic answer",
            );
            if index == 0 {
                first_input = request["input"].as_array().unwrap().clone();
                first_output = event["response"]["output"].as_array().unwrap().clone();
            }
            if index == 2 {
                let prefix: Vec<_> = first_input.iter().chain(&first_output).cloned().collect();
                assert_eq!(
                    &request["input"].as_array().unwrap()[..prefix.len()],
                    prefix.as_slice()
                );
            }
            send_json(&mut socket, event).await;
        }
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, None);
    let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
    for index in 0..3 {
        provider.additional_params.insert("prompt_cache_options".into(), json!({"comparison_response_id":format!("resp_explicit_baseline_{index}"), "mode":if index == 2 {"explicit"} else {"implicit"}, "ttl":"30m"}));
        history.push(zevria_model::OwnedModelRequestItem::message(Message::user(
            format!("synthetic prompt {index}"),
        )));
        let response = provider
            .complete(
                ModelRequest {
                    instructions: test_instructions(),
                    input: history
                        .iter()
                        .map(zevria_model::OwnedModelRequestItem::as_borrowed)
                        .collect(),
                    model_role: ModelRole::Build,
                    allowed_tool_names: None,
                },
                discard_updates(),
            )
            .await
            .unwrap();
        history.push(response.into_record().into_model_request_item());
    }
    server.await.unwrap();
}
