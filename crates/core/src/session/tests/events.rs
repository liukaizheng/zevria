use super::*;

#[tokio::test]
async fn simple_turn_has_authoritative_event_order() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("hello"))]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "hi".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        events[0],
        SessionEvent::TurnStarted {
            mode: SessionMode::Build,
            ..
        }
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextUsageUpdated { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
    let calls = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ModelCallStarted { turn_id, call } => Some((*turn_id, *call)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, [(TurnId::new(1), 1)]);
    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests[0].prompt, Message::user("hi"));
    assert_eq!(requests[0].model_role, ModelRole::Build);
    assert_eq!(
        requests[0].instructions,
        engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
    );
    assert_eq!(requests[0].allowed_tool_names, None);
    assert_eq!(engine.history().len(), 2);
}

#[tokio::test]
async fn loop_calls_count_dispatches_not_tools_and_restart_for_each_turn() {
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![tool_call("first", "one"), tool_call("second", "two")],
        }),
        Ok(Message::assistant("tools done")),
        Ok(Message::assistant("next answer")),
    ]);
    let requests = provider.requests.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: calls.clone(),
        })
        .run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (sender, mut receiver) = session_event_channel(64);
    for text in ["use both tools", "next question"] {
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &sender,
            )
            .await
            .unwrap();
    }
    let events = collect_events(&mut receiver).await;
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStarted { turn_id, .. } => Some((*turn_id, "start", 0)),
            SessionEvent::ModelCallStarted { turn_id, call } => Some((*turn_id, "call", *call)),
            SessionEvent::Intermediate { turn_id, .. } => Some((*turn_id, "intermediate", 0)),
            SessionEvent::ToolResults { turn_id, .. } => Some((*turn_id, "tools", 0)),
            SessionEvent::TurnCompleted { turn_id, .. } => Some((*turn_id, "complete", 0)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let first = TurnId::new(1);
    let second = TurnId::new(2);
    assert_eq!(
        lifecycle,
        [
            (first, "start", 0),
            (first, "call", 1),
            (first, "intermediate", 0),
            (first, "tools", 0),
            (first, "call", 2),
            (first, "complete", 0),
            (second, "start", 0),
            (second, "call", 1),
            (second, "complete", 0),
        ]
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].prompt, Message::user("use both tools"));
    assert_eq!(requests[2].prompt, Message::user("next question"));
    assert!(requests.iter().all(|request| request.instructions
        == engine.rendered_instructions(engine.policies.policy(SessionMode::Build))));
    assert_eq!(requests[0].input.first(), requests[2].input.first());
}

#[tokio::test]
async fn replay_message_drives_tools_events_history_and_persistence() {
    let tool_replay = ProviderReplay::openai_responses(
        test_profile(),
        vec![json!({
            "type": "function_call",
            "id": "fc_echo",
            "call_id": "call_echo",
            "name": "echo",
            "arguments": "{\"value\":\"from replay\"}",
            "status": "completed"
        })],
    );
    let final_replay = ProviderReplay::openai_responses(
        test_profile(),
        vec![json!({
            "type": "message",
            "id": "msg_final",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "canonical final"}]
        })],
    );
    let provider = ScriptedProvider::with_model_responses([
        ModelResponse::from_replay(tool_replay.clone()),
        ModelResponse::from_replay(final_replay.clone()),
    ]);
    let requests = provider.requests.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: calls.clone(),
        })
        .run();
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "use the replay".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(
        calls.lock().expect("calls lock").as_slice(),
        ["from replay"]
    );
    let canonical_tool_message = tool_replay
        .to_message()
        .expect("tool replay should convert");
    let canonical_final = final_replay
        .to_message()
        .expect("final replay should convert");
    assert_eq!(engine.history()[1], canonical_tool_message);
    assert_eq!(engine.history().last(), Some(&canonical_final));

    let lifecycle = collect_events(&mut receiver).await;
    assert!(lifecycle.iter().any(|event| matches!(
        event,
        SessionEvent::Intermediate { message, .. } if message == &canonical_tool_message
    )));
    assert!(matches!(
        lifecycle.last(),
        Some(SessionEvent::TurnCompleted { message, .. }) if message == &canonical_final
    ));

    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests[1].history[1], canonical_tool_message);
    drop(requests);
    drop(engine);

    let lines = std::fs::read_to_string(&transcript_path)
        .expect("transcript should be readable")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSONL"))
        .collect::<Vec<_>>();
    for index in [1, 3] {
        let record = lines[index]
            .as_object()
            .expect("provider record should be an object");
        assert_eq!(record.len(), 1);
        assert!(record.contains_key("zevria_provider_replay"));
        assert!(!record.contains_key("role"));
    }
    let restored = zevria_transcript::transcript::load(&transcript_path)
        .expect("replay-only transcript should restore");
    assert_eq!(restored[1].message(), Some(&canonical_tool_message));
    assert_eq!(restored[3].message(), Some(&canonical_final));
}

#[tokio::test]
async fn resume_exposes_ready_without_startup_events_or_repairing_manual_projection_edits() {
    let (artifact, items) = ready_plan_fixture();
    let provider = ScriptedProvider::new(Vec::<anyhow::Result<Message>>::new());
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    let session_id = transcript.session_id().to_string();
    for item in &items {
        transcript.append(item).expect("seed transcript");
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let plans = workspace.path().join("plans");
    let projection = plans.join(&session_id).join(format!(
        "{}-durable-approval-workflow.md",
        artifact.version.id
    ));
    std::fs::create_dir_all(projection.parent().expect("projection parent"))
        .expect("projection directory");
    std::fs::write(&projection, "manually edited").expect("stale projection");
    let engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap()
    .with_plans_dir(plans);
    assert_eq!(
        engine.plan_state().unwrap(),
        &PlanWorkflowState::Ready { artifact }
    );
    let (events, mut receiver) = session_event_channel(1);
    let (commands, command_rx) = mpsc::unbounded_channel();
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ))
        .expect("shutdown command");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        engine.run(command_rx, events),
    )
    .await
    .expect("startup must not wait for an event consumer")
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(projection).expect("preserved projection"),
        "manually edited"
    );
    assert!(collect_events(&mut receiver).await.is_empty());
}

#[tokio::test]
async fn provider_failures_are_recorded_and_emit_an_authoritative_terminal_event() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let writer = TranscriptWriter::create(directory.path()).expect("transcript writer");
    let path = writer.path().to_path_buf();
    let provider = ScriptedProvider::new([Err(anyhow::anyhow!("provider unavailable"))]);
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "will fail".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        events.first(),
        Some(SessionEvent::TurnStarted { .. })
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::ContextUsageUpdated { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(SessionEvent::TurnFailed { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::ModelCallStarted { call: 1, .. }))
            .count(),
        1
    );
    drop(engine);

    let recorded = zevria_transcript::transcript::load(&path).expect("transcript should load");
    assert!(matches!(
        conversation_records(&recorded).as_slice(),
        [
            TranscriptItem::Message(Message::User { .. }),
            TranscriptItem::Error { error }
        ] if error == "provider unavailable"
    ));
}

#[tokio::test]
async fn usage_events_report_the_active_profile_role_and_window() {
    let usage = TokenUsage {
        input_tokens: 80,
        cached_tokens: 20,
        output_tokens: 10,
        total_tokens: 90,
    };
    let provider = ScriptedProvider::with_model_responses([Ok(model_response(
        Message::assistant("done"),
    )
    .with_usage(Some(usage)))]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(role_compaction_policy(
        90,
        ("build-usage", 12_345, 100),
        ("plan", 9_000, 100),
        ("review", 8_000, 100),
        ("explore", 7_000, 100),
        ("builder", 6_000, 100),
    ));
    let (events, mut receiver) = session_event_channel(16);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "report usage".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::UsageUpdated {
            usage: actual,
            profile,
            model_role: ModelRole::Build,
            context_window_tokens: 12_345,
            ..
        } if *actual == usage
            && profile == &ModelProfileRef::new("test-provider", "build-usage")
    )));
}
