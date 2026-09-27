use super::*;
use rig_core::message::UserContent;
use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;

fn prompt() -> UserPrompt {
    let image = PromptImage::from_rgba(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap();
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
        let mut socket = accept_async(stream).await.unwrap();
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
        let mut socket = accept_async(stream).await.unwrap();
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let detail = format!("bad image data:image/png;base64,{}", image.base64());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "400 Bad Request",
            "application/json",
            json!({"error":{"code":"context_length_exceeded", "message":detail}}).to_string(),
        )
        .await;
    });
    let mut provider = connect_http_test_provider_with_options(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        "image-error",
    )
    .await;
    let parts = RequestParts {
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
    assert!(!error.to_string().contains(&image.base64()));
    assert!(
        error
            .downcast_ref::<zevria_model::ModelInputTooLarge>()
            .is_some()
    );
    server.await.unwrap();
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
        zevria_model::estimate_message_tokens(&original),
        zevria_model::estimate_message_tokens(&huge)
    );
    let small_wire = zevria_responses::protocol::message_to_openai_input(original).unwrap();
    let wire = zevria_responses::protocol::message_to_openai_input(huge).unwrap();
    let expected = zevria_model::compaction::estimate_responses_input_tokens(&small_wire, 0);
    assert_eq!(
        zevria_model::compaction::estimate_responses_input_tokens(&wire, 0),
        expected
    );
    assert!((3200..3500).contains(&expected));
    let mut with_output = wire.clone();
    with_output
        .push(json!({"type":"function_call", "call_id":"x", "name":"read", "arguments":"{}"}));
    with_output.push(
        json!({"type":"function_call_output", "call_id":"x", "output":"keep this useful output"}),
    );
    assert_eq!(
        crate::turn::trim_correlated_tool_outputs(&with_output, 0, 10_000),
        with_output
    );
    let literal = vec![
        json!({"type":"function_call_output", "call_id":"x", "output":{"type":"input_image", "image_url":"A".repeat(80_000)}}),
    ];
    assert!(zevria_model::compaction::estimate_responses_input_tokens(&literal, 0) > 20_000);
    assert_eq!(prompt.images().count(), 2);
}
#[tokio::test]
async fn full_http_sse_request_keeps_real_image_data() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
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
        ToolServer::new().run(),
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        "image-session",
    )
    .await;
    let parts = RequestParts {
        prompt: prompt().to_message(),
        history: vec![],
        instructions: "Image test".into(),
        allowed_tool_names: None,
    };
    let mut state = AttemptState::default();
    run_turn_request_with_recovery(
        parts.request(),
        &mut provider,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(1),
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
}
