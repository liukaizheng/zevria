use super::*;

#[test]
fn capacity_assessment_boundaries_and_authority_are_shared() {
    for (tokens, band) in [
        (79, CapacityBand::Fits),
        (80, CapacityBand::AtTrigger),
        (100, CapacityBand::AtTrigger),
        (101, CapacityBand::OverLimit),
    ] {
        let assessment = CapacityAssessment::from_candidates(
            CapacityCandidates {
                payload: tokens,
                conservative: tokens,
                usage: None,
                exact: None,
            },
            80,
            100,
        );
        assert_eq!(assessment.tokens, tokens);
        assert_eq!(assessment.band, band);
        assert_eq!(assessment.needs_exact(), band != CapacityBand::Fits);
        assert!(!assessment.measurements_disagree);
    }
    for (usage, exact, tokens, source) in [
        (None, None, 90, ContextTokenSource::ConservativeEstimate),
        (Some(70), None, 70, ContextTokenSource::UsagePlusDelta),
        (Some(70), Some(60), 60, ContextTokenSource::Exact),
    ] {
        let assessment = CapacityAssessment::from_candidates(
            CapacityCandidates {
                payload: 60,
                conservative: 90,
                usage,
                exact,
            },
            80,
            100,
        );
        assert_eq!((assessment.tokens, assessment.source), (tokens, source));
        assert!(assessment.measurements_disagree);
        assert_eq!(assessment.needs_exact(), exact.is_none());
    }
    let assessment = CapacityAssessment::from_candidates(
        CapacityCandidates {
            payload: 20,
            conservative: 30,
            usage: Some(101),
            exact: None,
        },
        80,
        100,
    );
    assert_eq!(assessment.band, CapacityBand::OverLimit);
    assert!(assessment.measurements_disagree && assessment.needs_exact());
}

#[tokio::test]
async fn blocked_lifecycle_is_sequenced_only_after_capacity_is_reserved() {
    let (sender, mut receiver) = session_event_channel(1);
    let turn_id = TurnId::new(17);
    sender
        .try_send(SessionEvent::TurnStarted {
            turn_id,
            message: Message::user("question"),
            mode: SessionMode::Build,
        })
        .expect("queue starts available");

    let blocked_sender = sender.clone();
    let blocked = tokio::spawn(async move {
        blocked_sender
            .send(SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id,
                message: Message::assistant("commit"),
            })
            .await
            .expect("capacity eventually opens");
    });
    tokio::task::yield_now().await;
    sender.stream_updated(turn_id, Message::assistant("preview while blocked"));

    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::TurnStarted { .. }))
    ));
    blocked.await.expect("blocked publisher completes");
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::Intermediate { .. }))
    ));
    assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
}

#[tokio::test]
async fn provider_failure_after_submit_plan_leaves_workflow_planning() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Do not commit without a final response.");
    let provider = ScriptedProvider::new([
        Ok(submit_plan_call(title, &markdown)),
        Err(anyhow::anyhow!(
            "scripted failure before the final Plan response"
        )),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan it".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning { .. }
    ));
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(|item| { !matches!(item, TranscriptItem::Plan(PlanRecord::Ready { .. })) })
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| {
        matches!(event, SessionEvent::TurnFailed { error, .. }
            if error.contains("scripted failure before the final Plan response"))
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    assert_eq!(
        zevria_transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn model_proposed_skill_uses_exact_capacity_instead_of_legacy_tracker_totals() {
    let skills = Arc::new(
        SkillCatalog::new([zevria_instructions::skill::SkillDefinition::new(
            SkillName::parse("review").expect("name"),
            "Review instructions",
            "x".repeat(600),
            zevria_instructions::skill::SkillSource::Programmatic("test".to_string()),
        )
        .expect("definition")])
        .expect("registry"),
    );
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("activate-review".to_string()),
            content: vec![named_tool_call(
                "activate-review-call",
                SKILL_TOOL_NAME,
                json!({"skill": "review"}),
            )],
        }),
        Ok(Message::assistant("activated successfully")),
    ])
    .with_input_counts([
        Ok(InputTokenCount::Exact(100)),
        Ok(InputTokenCount::Exact(200)),
    ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    // Leave headroom for the deterministic command capability text while the
    // legacy tracker still reports near-full usage. Exact counts must win.
    let mut engine = SessionEngine::new(provider, tools, test_policies(), transcript, skills)
        .expect("engine")
        .with_compaction_policy(test_compaction_policy(3_000, 90, 0));
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.report_provider_usage(&policy, 2_900).unwrap();
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "load review".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::ToolResults { metadata, .. }
                    if metadata[0].outcome == ToolCallOutcome::Success
            ))
    );
}

#[tokio::test]
async fn provider_failure_resets_provider_and_next_prompt_recovers() {
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![tool_call("call", "once")],
        }),
        Err(anyhow::anyhow!("scripted continuation failure")),
        Ok(Message::assistant("recovered")),
    ]);
    let requests = provider.requests.clone();
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: tool_calls.clone(),
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
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(collect_events(&mut receiver).await.iter().any(
        |event| matches!(event, SessionEvent::TurnFailed { error, .. }
                if error.contains("scripted continuation failure"))
    ));
    assert_eq!(engine.provider().resets, 1);
    assert_eq!(requests.lock().unwrap().len(), 2);
    let completed_work = engine.history();
    assert_eq!(completed_work.len(), 3);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(collect_events(&mut receiver).await.iter().any(
        |event| matches!(event, SessionEvent::TurnCompleted { message, .. }
                if message == &Message::assistant("recovered"))
    ));
    assert_eq!(*tool_calls.lock().unwrap(), ["once"]);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].history, completed_work);
    assert_eq!(requests[2].prompt, Message::user("second"));
}

#[tokio::test]
async fn finite_tool_continuations_complete_with_sequential_calls_and_preserved_results() {
    const CONTINUATIONS: usize = 6;
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: tool_calls.clone(),
        })
        .run();
    let provider = ScriptedProvider::new(
        (0..CONTINUATIONS)
            .map(|index| {
                Ok(Message::Assistant {
                    id: None,
                    content: vec![tool_call(
                        &format!("call-{index}"),
                        &format!("value-{index}"),
                    )],
                })
            })
            .chain([Ok(Message::assistant("all continuations completed"))]),
    );
    let requests = provider.requests.clone();
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
                text: "continue until finished".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
    )));
    let calls = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ModelCallStarted { call, .. } => Some(*call),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, (1..=CONTINUATIONS + 1).collect::<Vec<_>>());
    assert_eq!(
        *tool_calls.lock().unwrap(),
        (0..CONTINUATIONS)
            .map(|index| format!("value-{index}"))
            .collect::<Vec<_>>()
    );
    let results = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResults { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), CONTINUATIONS);
    let history = engine.history();
    assert_eq!(history.len(), 2 * CONTINUATIONS + 2);
    assert_eq!(
        history.last(),
        Some(&Message::assistant("all continuations completed"))
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), CONTINUATIONS + 1);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.history, history[..2 * index]);
        assert_eq!(request.prompt, history[2 * index]);
        assert_eq!(request.instructions, requests[0].instructions);
        assert_eq!(request.allowed_tool_names, requests[0].allowed_tool_names);
        if index > 0 {
            assert_eq!(request.prompt, results[index - 1]);
            let Message::User { content } = &request.prompt else {
                panic!("expected tool results")
            };
            assert!(
                matches!(content.as_slice(), [UserContent::ToolResult(result)]
                if result.call.as_str() == format!("call-{}", index - 1)
                    && result.name == "echo"
                    && result.content == vec![ToolResultContent::text(format!("value-{}", index - 1))])
            );
        }
    }
    assert_eq!(engine.provider().resets, 0);
    assert!(engine.provider().responses.is_empty());
    assert_eq!(
        zevria_transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn ensemble_review_capacity_uses_review_profile_not_build_profile() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("still too large"))]);
    let requests = provider.requests.clone();
    let launcher = Arc::new(StubEnsembleLauncher::successful(["evidence ".repeat(200)]));
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
        ("build-large", 10_000, 0),
        ("plan", 10_000, 0),
        ("review-small", 100, 0),
        ("explore", 10_000, 0),
        ("builder", 11_000, 0),
    ))
    .with_ensemble_launcher(launcher);
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::RunEnsemble {
                workflow: EnsembleWorkflow::Review,
                prompt: "review it".into(),
            }),
            &events,
        )
        .await
        .unwrap();

    // The Review mode's fixed instructions alone exceed its tiny limit.
    // No compaction call can rescue non-compactable overhead.
    assert!(requests.lock().expect("requests").is_empty());
    let events = collect_events(&mut receiver).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::ModelCallStarted { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnFailed { error, .. }
            if error.contains("prepared review request") && error.contains("review-small")
    )));
}

#[tokio::test]
async fn exact_count_wait_observes_turn_cancellation() {
    let mut provider = PendingInputCountProvider;
    let prompt = Message::user("count this");
    let cancellation = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(41), SessionMode::Build, cancellation.clone());
    let cancel = tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancellation.cancel();
    });
    let mut input_count = TurnInputCountState::Empty;
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        count_input_tokens_for_turn(
            &mut provider,
            &mut input_count,
            ModelRequest {
                instructions: "test instructions",
                input: vec![ModelRequestItem::message(&prompt)],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            &turn,
        ),
    )
    .await
    .expect("cancellation should interrupt a pending exact count");
    cancel.await.expect("canceller");
    assert!(result.is_err());
    assert!(turn.is_cancelled());
    assert_eq!(input_count, TurnInputCountState::Empty);
}

#[tokio::test]
async fn transient_exact_count_failure_is_attempted_once_per_turn() {
    let mut provider = ScriptedProvider::new([]).with_input_counts([
        Err(anyhow::anyhow!("temporary count failure")),
        Ok(InputTokenCount::Exact(42)),
    ]);
    let calls = provider.input_count_calls.clone();
    let prompt = Message::user("count this");
    let mut input_count = TurnInputCountState::Empty;
    let first_turn = TurnContext::new(
        TurnId::new(51),
        SessionMode::Build,
        CancellationToken::new(),
    );

    let first = count_input_tokens_for_turn(
        &mut provider,
        &mut input_count,
        ModelRequest {
            instructions: "test instructions",
            input: vec![ModelRequestItem::message(&prompt)],
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        },
        &first_turn,
    )
    .await;
    assert!(first.is_err());
    assert_eq!(
        count_input_tokens_for_turn(
            &mut provider,
            &mut input_count,
            ModelRequest {
                instructions: "test instructions",
                input: vec![ModelRequestItem::message(&prompt)],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            &first_turn,
        )
        .await
        .expect("same-turn attempt is suppressed"),
        InputTokenCount::Unsupported
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let next_turn = TurnContext::new(
        TurnId::new(52),
        SessionMode::Build,
        CancellationToken::new(),
    );
    assert_eq!(
        count_input_tokens_for_turn(
            &mut provider,
            &mut input_count,
            ModelRequest {
                instructions: "test instructions",
                input: vec![ModelRequestItem::message(&prompt)],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            &next_turn,
        )
        .await
        .expect("later turns retry transient failures"),
        InputTokenCount::Exact(42)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(input_count, TurnInputCountState::Empty);
}
