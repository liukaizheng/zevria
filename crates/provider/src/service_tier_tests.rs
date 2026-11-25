use super::*;

const SESSION_ID: &str = "service-tier-session";

pub(super) fn tier_profile(
    base_url: String,
    supports_websockets: bool,
    provider_tier: Option<&str>,
    model_tier: ModelServiceTier,
) -> ResolvedModelProfile {
    let mut additional_params = BTreeMap::from([("gateway_routing".into(), json!("unchanged"))]);
    if let Some(tier) = provider_tier {
        additional_params.insert("service_tier".into(), json!(tier));
    }
    let provider = ResponsesProviderConfig {
        base_url,
        api_key: ApiKeySource::literal("test-key"),
        supports_websockets,
        network: NetworkConfig::default(),
        session_id_header: None,
        models: BTreeMap::from([(
            "gpt-test".into(),
            ModelConfig {
                image_accounting: Default::default(),
                context_window_tokens: 128_000,
                input_token_limit: None,
                retained_user_tokens: 12_800,
                reasoning_levels: vec![ReasoningEffort::Medium],
                reasoning_summary_level: ReasoningSummaryLevel::Detailed,
                service_tier: model_tier,
            },
        )]),
        compatibility: ResponsesCompatibilityConfig::default(),
        additional_params,
        compaction: RemoteCompactionConfig::default(),
        input_token_count: InputTokenCountConfig::default(),
    };
    let assignment = ModelAssignment {
        provider: "gateway".into(),
        model: "gpt-test".into(),
        reasoning_level: ReasoningEffort::Medium,
    };
    ModelRouting::resolve(
        &BTreeMap::from([("gateway".into(), provider.into())]),
        &ModeAssignments {
            build: assignment.clone(),
            plan: assignment.clone(),
            review: assignment.clone(),
            explore: assignment.clone(),
            builder: assignment,
        },
        90,
    )
    .unwrap()
    .for_role(ModelRole::Build)
    .clone()
}

async fn connect(profile: &ResolvedModelProfile) -> OpenAiProvider {
    let mut openai = OpenAiProvider::connect(
        profile,
        ReasoningEffort::Medium,
        "Service tier test preamble",
        SESSION_ID,
        &crate::router::profile_cache_key(SESSION_ID, profile),
    )
    .await
    .unwrap();
    openai.recovery = fast_recovery_policy(3);
    openai
}

fn parts() -> RequestParts {
    RequestParts {
        tools: declarations(&policy_tools(), None),
        forbid_tool_calls: false,
        prompt: Message::user("Service tier test input"),
        history: Vec::new(),
        instructions: "Service tier test instructions".to_string(),
        allowed_tool_names: None,
    }
}

fn assert_tier(body: &Value, expected: Option<&str>) {
    let body = body.as_object().unwrap();
    if let Some(expected) = expected {
        assert_eq!(body["service_tier"], expected);
    } else {
        assert!(!body.contains_key("service_tier"), "{body:?}");
    }
}

async fn capture_create(
    websocket: bool,
    provider_tier: Option<&str>,
    model_tier: ModelServiceTier,
) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = tier_profile(
        format!("http://{}/v1/responses", listener.local_addr().unwrap()),
        websocket,
        provider_tier,
        model_tier,
    );
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let completed = completed_event("resp_tier", "msg_tier", "done");
        if websocket {
            let mut socket = accept_async(stream).await.unwrap();
            let body = receive_json(&mut socket).await;
            assert_eq!(body["type"], "response.create");
            send_json(&mut socket, completed).await;
            body
        } else {
            let mut stream = stream;
            let request = receive_http_json(&mut stream).await;
            assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
            assert_eq!(request.body["stream"], true);
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed.to_string()),
            )
            .await;
            request.body
        }
    });
    connect(&profile)
        .await
        .complete(parts().request(), discard_updates())
        .await
        .unwrap();
    server.await.unwrap()
}

#[tokio::test]
async fn http_and_websocket_creation_preserve_tier_and_prompt_prefix() {
    use ModelServiceTier::{Inherit, Omit, Set};
    for websocket in [false, true] {
        let baseline = capture_create(websocket, None, Inherit).await;
        assert_tier(&baseline, None);
        assert_eq!(baseline["instructions"], parts().request().instructions);
        assert!(!baseline["tools"].as_array().unwrap().is_empty());
        assert_eq!(
            baseline["input"][0]["content"][0]["text"],
            "Service tier test input"
        );
        assert!(!baseline["prompt_cache_key"].as_str().unwrap().is_empty());

        for (provider_tier, model_tier, expected) in [
            (None, Set(ServiceTier::Fast), Some("fast")),
            (Some("fast"), Inherit, Some("fast")),
            (Some("fast"), Set(ServiceTier::Default), Some("default")),
            (Some("fast"), Omit, None),
        ] {
            let mut body = capture_create(websocket, provider_tier, model_tier).await;
            assert_tier(&body, expected);
            body.as_object_mut().unwrap().remove("service_tier");
            assert_eq!(body, baseline);
        }
    }
}

#[tokio::test]
async fn websocket_followup_retry_and_http_fallback_preserve_service_tier() {
    use ModelServiceTier::{Omit, Set};
    for (model_tier, expected) in [
        (Set(ServiceTier::Fast), Some("fast")),
        (Set(ServiceTier::Default), Some("default")),
        (Omit, None),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = tier_profile(
            format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            true,
            Some("fast"),
            model_tier,
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let initial = receive_json(&mut socket).await;
            assert_tier(&initial, expected);
            send_json(
                &mut socket,
                completed_event("resp_initial", "msg_initial", "first answer"),
            )
            .await;

            let chained = receive_json(&mut socket).await;
            assert_tier(&chained, expected);
            assert_eq!(chained["previous_response_id"], "resp_initial");
            assert_eq!(chained["input"].as_array().unwrap().len(), 1);
            send_json(
                &mut socket,
                json!({"type":"error","error":{
                    "code":"previous_response_not_found","message":"Previous response not found"
                }}),
            )
            .await;

            let mut retried = receive_json(&mut socket).await;
            assert_tier(&retried, expected);
            assert!(retried.get("previous_response_id").is_none());
            assert_eq!(retried["input"].as_array().unwrap().len(), 3);
            send_json(
                &mut socket,
                json!({"type":"error","error":{
                    "code":"server_error","message":"unavailable"
                }}),
            )
            .await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
            assert_tier(&request.body, expected);
            retried.as_object_mut().unwrap().remove("type");
            retried["stream"] = json!(true);
            assert_eq!(request.body, retried);
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("resp_recovered", "msg_recovered", "done").to_string()),
            )
            .await;
        });
        let mut provider = connect(&profile).await;
        let parts = parts();
        let initial = provider
            .complete(parts.request(), discard_updates())
            .await
            .unwrap()
            .into_record();
        let next_prompt = Message::user("Follow-up");
        let mut request = parts.request();
        request.input.push(initial.model_request_item());
        request.input.push(ModelRequestItem::message(&next_prompt));
        provider.complete(request, discard_updates()).await.unwrap();
        server.await.unwrap();
    }
}
