use super::*;
use rig_core::message::{ToolCallId, ToolResult, ToolResultContent, UserContent};
use zevria_foundation::ModelProfileRef;
use zevria_model::ModelInputTooLarge;
use zevria_model::OwnedModelRequestItem;
use zevria_model::compaction::replay::replay_safe_prefixes;
use zevria_model::compaction::summary_tail_history;

fn size_event_cases() -> Vec<(Value, bool)> {
    let mut cases = Vec::new();
    for (code, expected) in [
        ("context_too_large", true),
        ("context_length_exceeded", true),
        ("invalid_request_error", false),
        ("invalid_api_key", false),
        ("rate_limit_exceeded", false),
        ("max_output_tokens", false),
    ] {
        cases.push((json!({"type":"error", "error":{"code":code, "message":"context too large diagnostic"}}), expected));
    }
    // Typed input size takes priority over the upstream disconnect prose rule.
    cases.push((json!({"type":"error", "error":{"code":"context_too_large", "message":"websocket: close 1006 (abnormal closure): unexpected EOF"}}), true));
    for (event_type, status) in [
        ("response.failed", "failed"),
        ("response.incomplete", "incomplete"),
        ("response.done", "failed"),
        ("response.done", "incomplete"),
        ("response.done", "cancelled"),
    ] {
        let mut event = completed_event("resp_size", "msg_size", "");
        event["type"] = json!(event_type);
        event["response"]["status"] = json!(status);
        event["response"]["error"] =
            json!({"code":"context_length_exceeded", "message":"structured terminal detail"});
        cases.push((event, status != "cancelled"));
    }
    for reason in ["context_length_exceeded", "max_output_tokens"] {
        let mut event = completed_event("resp_incomplete", "msg_incomplete", "");
        event["type"] = json!("response.incomplete");
        event["response"]["status"] = json!("incomplete");
        event["response"]["incomplete_details"] = json!({"reason":reason});
        cases.push((event, reason == "context_length_exceeded"));
    }
    cases
}

#[tokio::test]
async fn structured_size_events_are_typed_and_never_reconnect_websocket() {
    for (event, expected) in size_event_cases() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let label = event.to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            receive_json(&mut socket).await;
            send_json(&mut socket, event).await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(40), listener.accept())
                    .await
                    .is_err()
            );
        });
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut openai = test_session_with_url(&url, socket, None);
        let mut state = AttemptState::default();
        let parts = RequestParts {
            prompt: Message::user("summary"),
            history: Vec::new(),
            instructions: String::new(),
            allowed_tool_names: Some(Vec::new()),
        };
        let error = run_turn_request_with_recovery(
            parts.request(),
            &mut openai,
            &mut state,
            &discard_updates(),
            &fast_recovery_policy(2),
        )
        .await
        .unwrap_err()
        .context("outer wrapper");
        assert_eq!(
            error.chain().any(|cause| cause.is::<ModelInputTooLarge>()),
            expected,
            "{label}: {error:#}"
        );
        assert!(!is_websocket_disconnect(&error), "{label}: {error:#}");
        assert!(state.result.is_empty());
        assert!(openai.ws.continuation.is_none());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn structured_http_and_sse_size_errors_preserve_status_details_and_request_id_without_retry()
{
    let mut cases = size_event_cases()
        .into_iter()
        .map(|(event, expected)| {
            (
                "200 OK",
                "text/event-stream",
                sse_data(event.to_string()),
                expected,
            )
        })
        .collect::<Vec<_>>();
    for (status, code, expected) in [
        ("400 Bad Request", "context_length_exceeded", true),
        ("500 Internal Server Error", "context_too_large", true),
        ("400 Bad Request", "invalid_request_error", false),
        ("401 Unauthorized", "invalid_api_key", false),
        ("429 Too Many Requests", "rate_limit_exceeded", false),
        ("400 Bad Request", "max_output_tokens", false),
    ] {
        cases.push((
            status,
            "application/json",
            json!({"error":{"code":code,"message":"context too large diagnostic"}}).to_string(),
            expected,
        ));
    }
    cases.push((
        "413 Payload Too Large",
        "text/plain",
        "body exceeds capacity".into(),
        true,
    ));
    cases.push((
        "400 Bad Request",
        "text/plain",
        "context_length_exceeded is just prose".into(),
        false,
    ));
    for (status, content_type, body, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let label = body.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_json(&mut stream).await;
            send_http_response_with_raw_request_id(
                &mut stream,
                status,
                content_type,
                Some(b"req_size_test"),
                body,
            )
            .await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(40), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut openai = connect_http_test_provider(url, ToolServer::new().run()).await;
        let mut state = AttemptState::default();
        let parts = RequestParts {
            prompt: Message::user("summary"),
            history: Vec::new(),
            instructions: String::new(),
            allowed_tool_names: Some(Vec::new()),
        };
        let error = run_turn_request_with_recovery(
            parts.request(),
            &mut openai,
            &mut state,
            &discard_updates(),
            &fast_recovery_policy(2),
        )
        .await
        .unwrap_err()
        .context("outer wrapper");
        assert_eq!(
            error.chain().any(|cause| cause.is::<ModelInputTooLarge>()),
            expected,
            "{label}: {error:#}"
        );
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("req_size_test"), "{diagnostic}");
        if status != "200 OK" {
            assert!(diagnostic.contains(&status[..3]), "{diagnostic}");
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<rig_core::completion::CompletionError>()),
                "original completion error remains in the source chain"
            );
        }
        assert!(state.result.is_empty());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn count_endpoint_size_failures_are_not_completion_retry_signals() {
    for status in ["413 Payload Too Large", "400 Bad Request"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/input_tokens", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_json(&mut stream).await;
            send_http_response(
                &mut stream,
                status,
                "application/json",
                json!({"error":{"code":"context_too_large", "message":"count-only failure"}})
                    .to_string(),
            )
            .await;
        });
        let mut openai = test_session_with_pump(
            "ws://127.0.0.1:9/v1/responses",
            OpenAiWebSocketSession::disconnected(OpenAiWebSocketTerminalCategory::HttpFallback),
            None,
        );
        openai.input_token_count_url = Some(reqwest::Url::parse(&url).unwrap());
        let parts = RequestParts {
            prompt: Message::user("count"),
            history: Vec::new(),
            instructions: String::new(),
            allowed_tool_names: Some(Vec::new()),
        };
        let error = openai
            .count_input_tokens(parts.request())
            .await
            .unwrap_err();
        assert!(!error.chain().any(|cause| cause.is::<ModelInputTooLarge>()));
        assert!(format!("{error:#}").contains("context_too_large"));
        server.await.unwrap();
    }
}

fn profile() -> ModelProfileRef {
    ModelProfileRef::new("openai", "summary-test")
}
fn canonical_call(id: &str, provider: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::from_dual_wire(
        id.to_string(),
        provider.to_string(),
        ToolFunction::new("command".to_string(), json!({"command":"rtk pwd"})),
    ))
}
fn canonical_result(id: &str, provider: &str) -> UserContent {
    let AssistantContent::ToolCall(call) = canonical_call(id, provider) else {
        unreachable!()
    };
    UserContent::ToolResult(ToolResult {
        call: call.id,
        provider: call.provider,
        name: call.function.name,
        content: vec![ToolResultContent::text("result 雪🚀".repeat(1000))],
    })
}
fn native_call(id: &str, handle: &str) -> Value {
    json!({"type":"function_call", "id":id, "call_id":handle, "name":"command", "arguments":"{}", "status":"completed", "opaque_extra":{"signature":"unaltered"}})
}
fn mixed_source() -> Vec<OwnedModelRequestItem> {
    vec![
        OwnedModelRequestItem::message(Message::user("oldest indivisible")),
        OwnedModelRequestItem::message(Message::Assistant { id: Some("canonical-batch".into()), content: vec![canonical_call("a", "shared-a"), canonical_call("b", "shared-b")] }),
        OwnedModelRequestItem::message(Message::User { content: vec![canonical_result("a", "shared-a"), canonical_result("b", "shared-b")] }),
        OwnedModelRequestItem::replay_backed(ProviderReplay::openai_responses(profile(), vec![native_call("native-c", "c"), native_call("native-d", "d")])).unwrap(),
        OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(profile(), vec![json!({"type":"function_call_output", "call_id":"c", "output":"unchanged native result", "extra":[1,2,3]}), json!({"type":"function_call_output", "call_id":"d", "output":{"text":"batched result"}})])).unwrap(),
        OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(profile(), vec![json!({"type":"web_search_call", "id":"self-contained-search", "status":"completed", "action":{"type":"search", "query":"rust"}}), json!({"type":"reasoning", "id":"opaque-reasoning", "summary":[], "encrypted_content":"private cipher"})])).unwrap(),
        OwnedModelRequestItem::message(Message::user("長い Unicode tail 🚀 e\u{301}".repeat(2000))),
        OwnedModelRequestItem::message(zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Text("multimodal tail".into()), zevria_content::PromptBlock::Image(zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap())]).unwrap().to_message()),
    ]
}

#[test]
fn replay_safe_boundaries_have_real_projection_parity_and_verbatim_mixed_tails() {
    let source = mixed_source();
    let original = source.clone();
    let borrowed = source
        .iter()
        .map(OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    let safe = replay_safe_prefixes(&borrowed).unwrap();
    assert_eq!(
        safe,
        [true, true, false, true, false, true, true, true, true]
    );
    assert!(zevria_responses::replay::project(&borrowed, &profile()).is_ok());
    for k in 1..=source.len() {
        let replacement = summary_tail_history(&source, k, "summary").unwrap();
        assert_eq!(&replacement[1..], &original[k..]);
        let replacement = replacement
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        if safe[k] {
            assert!(zevria_responses::replay::preflight(&borrowed[..k], &profile()).is_ok());
            assert!(zevria_responses::replay::preflight(&replacement, &profile()).is_ok());
        } else {
            assert!(
                zevria_responses::replay::project(&replacement, &profile()).is_err(),
                "unsafe cut {k} must orphan a result"
            );
        }
    }
    assert_eq!(source, original);
}

#[test]
fn reused_handles_and_malformed_correlations_match_real_projection() {
    let mut source = vec![
        OwnedModelRequestItem::message(Message::Assistant {
            id: None,
            content: vec![canonical_call("old", "reused")],
        }),
        OwnedModelRequestItem::message(Message::User {
            content: vec![canonical_result("old", "reused")],
        }),
        OwnedModelRequestItem::message(Message::Assistant {
            id: None,
            content: vec![canonical_call("new", "reused")],
        }),
        OwnedModelRequestItem::message(Message::User {
            content: vec![canonical_result("new", "reused")],
        }),
    ];
    let analyze = |source: &[OwnedModelRequestItem]| {
        let borrowed = source
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        (
            replay_safe_prefixes(&borrowed),
            zevria_responses::replay::project(&borrowed, &profile()),
        )
    };
    assert_eq!(
        analyze(&source).0.unwrap(),
        [true, false, true, false, true]
    );
    assert!(analyze(&source).1.is_ok());
    source[3] = OwnedModelRequestItem::message(Message::User {
        content: vec![canonical_result("old", "reused")],
    });
    let (boundary, projection) = analyze(&source);
    assert!(
        boundary.is_err() && projection.is_err(),
        "old canonical/new provider handle is ambiguous"
    );
    source.remove(1);
    let (boundary, projection) = analyze(&source);
    assert!(
        boundary.is_err() && projection.is_err(),
        "outstanding reused handle is ambiguous"
    );
    let orphan = vec![OwnedModelRequestItem::message(Message::User {
        content: vec![UserContent::ToolResult(ToolResult {
            call: ToolCallId::new("missing").unwrap(),
            provider: None,
            name: "command".into(),
            content: vec![ToolResultContent::text("orphan")],
        })],
    })];
    let (boundary, projection) = analyze(&orphan);
    assert!(boundary.is_err() && projection.is_err());
}

#[test]
fn summary_checkpoint_with_opaque_tail_roundtrips_and_stays_profile_bound() {
    let source = mixed_source();
    let replacement = summary_tail_history(&source, 1, "durable summary").unwrap();
    let checkpoint = zevria_model::CompactionCheckpoint::new(
        zevria_model::CompactionTrigger::Manual,
        zevria_model::CompactionBackend::LocalSummary,
        replacement.clone(),
        vec![],
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    writer
        .append(&TranscriptItem::Message(Message::user("original user")))
        .unwrap();
    writer
        .append(&TranscriptItem::Compaction(checkpoint.clone()))
        .unwrap();
    writer
        .append(&TranscriptItem::Message(Message::user(
            "appended after checkpoint",
        )))
        .unwrap();
    let loaded = transcript::load(writer.path()).unwrap();
    assert_eq!(loaded[1], TranscriptItem::Compaction(checkpoint));
    let projection = transcript::model_input(&loaded);
    let owned = projection
        .iter()
        .map(|item| item.to_owned_item().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(&owned[..replacement.len()], &replacement);
    assert_eq!(
        owned.last().unwrap(),
        &OwnedModelRequestItem::message(Message::user("appended after checkpoint"))
    );
    assert!(matches!(
        zevria_responses::replay::preflight(&projection, &profile()).unwrap(),
        zevria_model::models::ReplayPreflight::Compatible(_)
    ));
    assert!(
        matches!(zevria_responses::replay::preflight(&projection, &ModelProfileRef::new("openai", "foreign")).unwrap(), zevria_model::models::ReplayPreflight::ConversionRequired { sources } if sources == vec![profile()])
    );
}
