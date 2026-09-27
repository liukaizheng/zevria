use super::*;
use zevria_foundation::ReasoningLevel as Level;

#[tokio::test]
async fn reasoning_switch_keeps_socket_prefix_and_cache_key_then_resumes_continuation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
    let mut a = profile("A", &url);
    a.endpoint.supports_websockets = true;
    let cache_key = crate::router::profile_cache_key("reasoning-root", &a);
    let server = tokio::spawn(async move {
        // All three requests must arrive on this one socket, with no reconnect.
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let first = receive_json(&mut socket).await;
        assert_eq!(first["reasoning"]["effort"], "medium");
        assert_eq!(first["prompt_cache_key"], cache_key);
        send_json(&mut socket, completed_event("reason-1", "msg-1", "first")).await;
        let second = receive_json(&mut socket).await;
        assert_eq!(second["reasoning"]["effort"], "high");
        assert!(second.get("previous_response_id").is_none());
        for field in [
            "instructions",
            "tools",
            "input",
            "prompt_cache_key",
            "include",
        ] {
            assert_eq!(
                first[field], second[field],
                "reasoning switch changed {field}"
            );
        }
        send_json(&mut socket, completed_event("reason-2", "msg-2", "second")).await;
        let third = receive_json(&mut socket).await;
        assert_eq!(third["reasoning"]["effort"], "high");
        assert_eq!(third["previous_response_id"], "reason-2");
        assert_eq!(third["input"].as_array().unwrap().len(), 1);
        assert_eq!(first["prompt_cache_key"], third["prompt_cache_key"]);
        assert_eq!(first["instructions"], third["instructions"]);
        assert_eq!(first["tools"], third["tools"]);
        send_json(&mut socket, completed_event("reason-3", "msg-3", "third")).await;
    });
    let mut router = ResponsesRouter::from_routes(
        [(
            ModelRole::Build,
            a.clone(),
            zevria_foundation::ReasoningLevel::Medium,
        )],
        "unchanged application",
        policy_tools(),
        "reasoning-root",
    )
    .unwrap();
    let mut history = vec![OwnedModelRequestItem::message(Message::user(
        "identical prompt",
    ))];
    for index in 0..3 {
        if index == 1 {
            let previous = router
                .continuation_response_id(&a.profile)
                .unwrap()
                .to_string();
            assert_eq!(
                router
                    .prepare_model_update(
                        ModelRole::Build,
                        &ModelSelection::new(a.profile.clone(), Level::High)
                    )
                    .unwrap(),
                a.context_policy()
            );
            router.install_model_update(
                ModelRole::Build,
                &ModelSelection::new(a.profile.clone(), Level::High),
            );
            assert_eq!(
                router.continuation_response_id(&a.profile),
                Some(previous.as_str()),
                "install must not reset continuation"
            );
        }
        let response = router
            .complete(
                ModelRequest {
                    instructions: test_instructions(),
                    input: history
                        .iter()
                        .map(OwnedModelRequestItem::as_borrowed)
                        .collect(),
                    model_role: ModelRole::Build,
                    allowed_tool_names: Some(&["command".into()]),
                },
                discard_updates(),
            )
            .await
            .unwrap();
        if index == 1 {
            history.push(response.into_record().into_model_request_item());
            history.push(OwnedModelRequestItem::message(Message::user("next prompt")));
        }
    }
    assert_eq!(router.initialized_profile_count(), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn shared_slot_uses_role_levels_and_explicit_maintenance_uses_requested_levels() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let mut a = profile("A", &url);
    a.endpoint.input_token_count.enabled = true;
    a.endpoint.compaction.url = Some(format!("{url}/compact"));
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for index in 0..7 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            bodies.push(request.body);
            if request
                .headers
                .starts_with("POST /v1/responses/input_tokens")
            {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    "{\"input_tokens\":42}",
                )
                .await;
            } else if request.headers.starts_with("POST /v1/responses/compact") {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    json!({"output":[{"type":"compaction","encrypted_content":"opaque"}]})
                        .to_string(),
                )
                .await;
            } else {
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(
                        completed_event(
                            &format!("resp-{index}"),
                            &format!("msg-{index}"),
                            "answer",
                        )
                        .to_string(),
                    ),
                )
                .await;
            }
        }
        bodies
    });
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (ModelRole::Plan, a.clone(), Level::Low),
            (
                ModelRole::Review,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        policy_tools(),
        "shared",
    )
    .unwrap();
    router.install_model_update(
        ModelRole::Build,
        &ModelSelection::new(a.profile.clone(), Level::High),
    );
    let prompt = Message::user("same input");
    let request = |role| ModelRequest {
        instructions: test_instructions(),
        input: vec![ModelRequestItem::message(&prompt)],
        model_role: role,
        allowed_tool_names: Some(&[]),
    };
    for role in [ModelRole::Build, ModelRole::Plan, ModelRole::Review] {
        router
            .complete(request(role), discard_updates())
            .await
            .unwrap();
    }
    router
        .count_input_tokens(request(ModelRole::Build))
        .await
        .unwrap();
    router
        .count_profile(
            &ModelSelection::new(a.profile.clone(), Level::Low),
            request(ModelRole::Build),
        )
        .await
        .unwrap();
    router
        .complete_profile(
            &ModelSelection::new(a.profile.clone(), Level::High),
            request(ModelRole::Build),
            discard_updates(),
        )
        .await
        .unwrap();
    router.compact(request(ModelRole::Plan)).await.unwrap();
    assert_eq!(router.initialized_profile_count(), 1);
    assert_eq!(
        router
            .model_selection(ModelRole::Build)
            .unwrap()
            .reasoning_level,
        Level::High
    );
    assert_eq!(
        router
            .model_selection(ModelRole::Plan)
            .unwrap()
            .reasoning_level,
        Level::Low
    );
    let bodies = server.await.unwrap();
    assert_eq!(
        bodies
            .iter()
            .map(|body| body["reasoning"]["effort"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["high", "low", "medium", "high", "low", "high", "low"]
    );
    for body in &bodies[1..] {
        assert_eq!(body["instructions"], bodies[0]["instructions"]);
        assert_eq!(body["input"], bodies[0]["input"]);
    }
    let target = ModelSelection::new(a.profile.clone(), Level::Medium);
    router
        .prepare_model_update(ModelRole::Build, &target)
        .unwrap();
    router.install_model_update(ModelRole::Build, &target);
    assert_eq!(
        router
            .model_selection(ModelRole::Build)
            .unwrap()
            .reasoning_level,
        Level::Medium
    );
    assert_eq!(
        router
            .model_selection(ModelRole::Plan)
            .unwrap()
            .reasoning_level,
        Level::Low
    );
}

#[tokio::test]
async fn converting_history_for_the_same_selected_model_uses_captured_explicit_reasoning() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = profile(
        "A",
        &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
    );
    let b = profile("B", "http://127.0.0.1:1/v1/responses");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert_eq!(request.body["model"], "A");
        assert_eq!(
            request.body["reasoning"]["effort"], "high",
            "source maintenance uses captured pre-switch role level"
        );
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("summary", "msg-summary", "portable summary").to_string()),
        )
        .await;
    });
    let (_directory, mut engine) = engine(
        a.clone(),
        b.clone(),
        opaque_history(&a),
        Arc::new(Settings::default()),
    );
    let target = ModelSelection::new(b.profile.clone(), Level::High);
    engine
        .provider_mut()
        .install_model_update(ModelRole::Plan, &target);
    let mut items = engine.conversation().items().to_vec();
    let TranscriptItem::SessionModels(models) = &mut items[0] else {
        panic!("header")
    };
    *models = models
        .with_selection(SessionMode::Plan, target.clone())
        .unwrap();
    engine = engine.with_transcript_items(items).unwrap();
    let Result::ConfirmationRequired(preview) = manage(
        &mut engine,
        Request::Select {
            scope: Scope::SessionOnly,
            mode: SessionMode::Plan,
            target,
            revision: String::new(),
        },
    )
    .await
    else {
        panic!("opaque source requires conversion")
    };
    assert!(matches!(
        manage(&mut engine, Request::Confirm { preview }).await,
        Result::Changed {
            reasoning_level: Level::High,
            unchanged: false,
            ..
        }
    ));
    assert_eq!(
        engine
            .provider()
            .model_selection(ModelRole::Plan)
            .unwrap()
            .reasoning_level,
        Level::High
    );
    assert_eq!(
        engine
            .conversation()
            .session_models()
            .unwrap()
            .reasoning_for_mode(SessionMode::Plan),
        Level::High
    );
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_validation_is_local_and_respects_capabilities_and_roles() {
    let mut a = profile("A", "http://127.0.0.1:1/v1/responses");
    a.reasoning_levels = vec![Level::Low, Level::Medium];
    let router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Review,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        ToolServer::new().run(),
        "root",
    )
    .unwrap();
    assert!(
        router
            .prepare_model_update(
                ModelRole::Build,
                &ModelSelection::new(a.profile.clone(), Level::High)
            )
            .is_err()
    );
    assert!(
        router
            .prepare_model_update(
                ModelRole::Review,
                &ModelSelection::new(a.profile.clone(), Level::Low)
            )
            .is_err()
    );
    assert!(
        router
            .prepare_model_update(
                ModelRole::Plan,
                &ModelSelection::new(a.profile.clone(), Level::Low)
            )
            .is_err()
    );
    assert!(router.model_selection(ModelRole::Plan).is_none());
    assert_eq!(router.initialized_profile_count(), 0);
}

#[tokio::test]
async fn opaque_source_must_support_the_captured_pre_switch_level_without_fallback() {
    let mut a = profile("A", "http://127.0.0.1:1/v1/responses");
    a.reasoning_levels = vec![Level::Medium];
    let b = profile("B", &a.endpoint.base_url);
    let settings = Arc::new(Settings::default());
    let (_directory, mut engine) =
        engine(a.clone(), b.clone(), opaque_history(&a), settings.clone());
    let current = ModelSelection::new(b.profile.clone(), Level::High);
    engine
        .provider_mut()
        .install_model_update(ModelRole::Plan, &current);
    let mut items = engine.conversation().items().to_vec();
    let TranscriptItem::SessionModels(models) = &mut items[0] else {
        panic!("header")
    };
    *models = models.with_selection(SessionMode::Plan, current).unwrap();
    engine = engine.with_transcript_items(items).unwrap();
    let before = std::fs::read(engine.conversation().path()).unwrap();
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let result = manage(
            &mut engine,
            Request::Select {
                mode: SessionMode::Plan,
                scope,
                target: ModelSelection::new(b.profile.clone(), Level::Low),
                revision: String::new(),
            },
        )
        .await;
        assert!(
            matches!(result, Result::Rejected { message, .. } if message.contains("select that source with a supported level first"))
        );
        assert_eq!(engine.provider().initialized_profile_count(), 0);
        assert_eq!(std::fs::read(engine.conversation().path()).unwrap(), before);
    }
    assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn all_five_roles_use_explicit_levels_on_one_profile_and_children_remain_independent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for index in 0..5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            bodies.push(receive_http_json(&mut stream).await.body);
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(
                    completed_event(&format!("r-{index}"), &format!("m-{index}"), "done")
                        .to_string(),
                ),
            )
            .await;
        }
        bodies
    });
    let provider: ProviderConfig = serde_json::from_value(json!({
        "base_url": url, "api_key": "test", "supports_websockets": false,
        "models": {"shared": { "context_window_tokens": 100000, "retained_user_tokens": 100,
            "reasoning_levels": ["none", "minimal", "low", "medium", "high", "max"], "reasoning_summary_level": "detailed" }}
    })).unwrap();
    let assignment = |reasoning_level| ModelAssignment {
        provider: "p".into(),
        model: "shared".into(),
        reasoning_level,
    };
    let modes = ModeAssignments {
        build: assignment(Level::Low),
        plan: assignment(Level::High),
        review: assignment(Level::Medium),
        explore: assignment(Level::Minimal),
        builder: assignment(Level::Max),
    };
    let routing =
        ModelRouting::resolve(&BTreeMap::from([("p".into(), provider)]), &modes, 90).unwrap();
    let tools = ToolServer::new().run();
    let mut root = ResponsesRouter::root(&routing, "stable", tools.clone(), "root").unwrap();
    let children = [ModelRole::Explore, ModelRole::Builder].map(|role| {
        ResponsesRouterFactory::new(
            role,
            routing.for_role(role).clone(),
            routing.selection_for_role(role).reasoning_level,
            "stable",
        )
    });
    let prompt = Message::user("identical input");
    let request = |model_role| ModelRequest {
        instructions: test_instructions(),
        input: vec![ModelRequestItem::message(&prompt)],
        model_role,
        allowed_tool_names: Some(&[]),
    };
    for role in [ModelRole::Build, ModelRole::Plan, ModelRole::Review] {
        root.complete(request(role), discard_updates())
            .await
            .unwrap();
    }
    assert_eq!(root.initialized_profile_count(), 1);
    let updated = ModelSelection::new(
        routing.for_role(ModelRole::Build).profile.clone(),
        Level::None,
    );
    root.prepare_model_update(ModelRole::Build, &updated)
        .unwrap();
    root.install_model_update(ModelRole::Build, &updated);
    for (role, factory) in [ModelRole::Explore, ModelRole::Builder]
        .into_iter()
        .zip(children)
    {
        let mut child = factory.create(role.name(), tools.clone()).unwrap();
        assert_eq!(
            child.model_selection(role).unwrap(),
            *routing.selection_for_role(role)
        );
        child
            .complete(request(role), discard_updates())
            .await
            .unwrap();
        assert!(root.model_selection(role).is_none());
    }
    let bodies = server.await.unwrap();
    assert_eq!(
        bodies
            .iter()
            .map(|b| b["reasoning"]["effort"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["low", "high", "medium", "minimal", "max"]
    );
    for body in &bodies[1..3] {
        assert_eq!(body["prompt_cache_key"], bodies[0]["prompt_cache_key"]);
    }
    assert_ne!(bodies[3]["prompt_cache_key"], bodies[0]["prompt_cache_key"]);
    assert_ne!(bodies[4]["prompt_cache_key"], bodies[3]["prompt_cache_key"]);
}
