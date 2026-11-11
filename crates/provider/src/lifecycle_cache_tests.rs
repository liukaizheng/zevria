use super::*;
use rig_core::message::{AssistantContent, ToolFunction, ToolName, ToolResultContent};
use zevria_model::ModelInputIdentity;

#[tokio::test]
async fn native_id_collision_rebuilds_wire_prefix_and_forces_full_replay_before_continuing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let first = receive_json(&mut socket).await;
        assert_eq!(first["input"][1]["call_id"], "zevria_call_000000");
        let mut completed = completed_event("first", "message", "");
        completed["response"]["output"] = json!([{
            "type":"function_call", "id":"native_item", "call_id":"zevria_call_000000",
            "name":"command", "arguments":"{\"command\":\"pwd\"}", "status":"completed"
        }]);
        send_json(&mut socket, completed).await;

        let second = receive_json(&mut socket).await;
        assert!(
            second.get("previous_response_id").is_none(),
            "renamed prefix cannot use continuation"
        );
        assert_eq!(second["input"][1]["call_id"], "zevria_call_000000_1");
        assert_eq!(second["input"][2]["call_id"], "zevria_call_000000_1");
        assert_eq!(second["input"][3]["call_id"], "zevria_call_000000");
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        assert_eq!(first["instructions"], second["instructions"]);
        assert_eq!(first["tools"], second["tools"]);
        send_json(
            &mut socket,
            completed_event("second", "message2", "safe answer"),
        )
        .await;

        let third = receive_json(&mut socket).await;
        assert_eq!(third["previous_response_id"], "second");
        assert_eq!(third["input"].as_array().unwrap().len(), 1);
        assert_eq!(third["prompt_cache_key"], first["prompt_cache_key"]);
        send_json(
            &mut socket,
            completed_event("third", "message3", "continued"),
        )
        .await;
    });
    let profile = resolved_profile(
        "test",
        "model",
        url,
        "test-key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    let mut provider = OpenAiProvider::unconnected(
        &profile,
        ReasoningEffort::Medium,
        "stable preamble",
        "session",
        "stable-cache-key",
    )
    .await
    .unwrap();
    let call = zevria_content::message::tool_call_from_wire(
        "portable_item",
        "portable_handle",
        ToolFunction::new(ToolName::new("command").unwrap(), json!({"command":"pwd"})),
    );
    let old_result = Message::from(call.result(vec![ToolResultContent::text("old result")]));
    let old_call = Message::from(vec![AssistantContent::ToolCall(call)]);
    let prompt = Message::user("continue inspecting");
    let mut input = vec![
        ModelRequestItem::message(&prompt),
        ModelRequestItem::message(&old_call),
        ModelRequestItem::message(&old_result),
    ];
    let declared = declarations(&policy_tools(), None);
    let identity = ModelInputIdentity::from_items(&input).unwrap();
    let first = provider
        .complete(
            ModelRequest {
                input_identity: Some(&identity),
                tools: &declared,
                forbid_tool_calls: false,
                instructions: test_instructions(),
                input: input.clone(),
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    input.push(first.record().model_request_item());
    let Message::Assistant(assistant) = first.message() else {
        panic!("assistant response")
    };
    let AssistantContent::ToolCall(native_call) = &assistant.content[0] else {
        panic!("native tool call")
    };
    let result = Message::from(native_call.result(vec![ToolResultContent::text("new result")]));
    input.push(ModelRequestItem::message(&result));
    let identity = ModelInputIdentity::from_items(&input).unwrap();
    let second = provider
        .complete(
            ModelRequest {
                input_identity: Some(&identity),
                tools: &declared,
                forbid_tool_calls: false,
                instructions: test_instructions(),
                input: input.clone(),
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    input.push(second.record().model_request_item());
    let next = Message::user("continue again");
    input.push(ModelRequestItem::message(&next));
    let identity = ModelInputIdentity::from_items(&input).unwrap();
    provider
        .complete(
            ModelRequest {
                input_identity: Some(&identity),
                tools: &declared,
                forbid_tool_calls: false,
                instructions: test_instructions(),
                input,
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_collector_does_not_keep_the_frontend_event_channel_alive() {
    let (sender, mut events) = session_event_channel(8);
    let progress = ProgressReporter::new(sender);
    let attempt = crate::attempt::AttemptStream::new(test_profile_ref(), &progress);
    drop(progress);
    let closed = tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
        .await
        .unwrap();
    assert!(closed.is_none());
    drop(attempt);
}

#[tokio::test(start_paused = true)]
async fn cancelling_before_the_cadence_tick_retains_the_latest_ingested_attempt() {
    use std::future::Future;
    let progress = discard_updates();
    let provider_progress = progress.clone();
    let mut request = Box::pin(async move {
        let mut state = AttemptState {
            attempt: Some(crate::attempt::AttemptStream::new(
                test_profile_ref(),
                &provider_progress,
            )),
            ..Default::default()
        };
        state.observe_attempt_value(&json!({"type":"response.output_text.delta","sequence_number":1,"output_index":0,"content_index":0,"item_id":"m","delta":"first "}));
        state.flush_stream(&provider_progress);
        state.observe_attempt_value(&json!({"type":"response.output_text.delta","sequence_number":2,"output_index":0,"content_index":0,"item_id":"m","delta":"latest"}));
        std::future::pending::<()>().await;
    });
    std::future::poll_fn(|cx| {
        assert!(request.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // This is the same future-drop boundary used by core's cancellation path.
    drop(request);
    let attempts = progress.drain_attempts();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].presentation.iter().any(|part| matches!(
        &part.content, zevria_content::AssistantPresentationContent::Answer { text } if text == "first latest"
    )));
}
