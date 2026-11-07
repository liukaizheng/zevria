use super::*;

#[tokio::test]
async fn idle_barrier_follows_completion_and_ready_plan_publication() {
    let title = "Idle barrier ordering";
    let markdown = valid_plan_markdown(title, "Publish before admitting follow-ups.");
    let provider = ScriptedProvider::new([
        Ok(create_and_submit_plan_call(title, &markdown)),
        Ok(Message::assistant("Ready.")),
    ]);
    let tools = ToolServer::new()
        .tool(CreatePlanStubTool)
        .tool(UpdatePlanStubTool)
        .tool(ReadPlanStubTool)
        .tool(SubmitPlanStubTool)
        .run();
    let (_directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_plans_dir(_directory.path().join("plans"))
    .with_idle_notifications();
    let (events, receiver) = session_event_channel(1);
    let mut run = RunningSession::start(
        engine,
        [SessionCommand::Turn(TurnCommand::Submit {
            text: "make a plan".into(),
            mode: SessionMode::Plan,
            behavior: zevria_foundation::RequestBehavior::Standard,
        })],
        events,
        receiver,
    );
    run.until(|event| matches!(event, SessionEvent::EngineIdle))
        .await;
    let events = run.shutdown().await;
    let completed = events
        .iter()
        .position(|e| matches!(e, SessionEvent::TurnCompleted { .. }))
        .unwrap();
    let ready = events
        .iter()
        .position(|e| {
            matches!(e, SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready { artifact },
    } if artifact.markdown.trim() == markdown.trim() && artifact.source_turn_id == TurnId::new(1))
        })
        .unwrap_or_else(|| panic!("missing Ready: {events:#?}"));
    let idle = events
        .iter()
        .position(|e| matches!(e, SessionEvent::EngineIdle))
        .unwrap();
    assert!(completed < ready && ready < idle);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::EngineIdle))
            .count(),
        1
    );
}

#[tokio::test]
async fn queued_turns_publish_only_one_idle_barrier_after_the_final_turn() {
    let (_directory, transcript) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let (events, receiver) = session_event_channel(1);
    let mut run = RunningSession::start(
        engine(provider, transcript).with_idle_notifications(),
        [submit("first")],
        events,
        receiver,
    );
    let first = run.call(&mut calls).await;
    run.send(submit("second"));
    run.busy_fence("second-is-queued").await;
    answer(first, "first done");
    let second = run.call(&mut calls).await;
    assert_call(&second, 2, "second");
    assert!(
        !run.lifecycle
            .iter()
            .any(|e| matches!(e, SessionEvent::EngineIdle))
    );
    answer(second, "second done");
    run.until(|event| matches!(event, SessionEvent::EngineIdle))
        .await;
    let events = run.shutdown().await;
    assert_turns_do_not_overlap(&events, &[1, 2]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::EngineIdle))
            .count(),
        1
    );
    let completed = events
        .iter()
        .rposition(|e| matches!(e, SessionEvent::TurnCompleted { .. }))
        .unwrap();
    let idle = events
        .iter()
        .position(|e| matches!(e, SessionEvent::EngineIdle))
        .unwrap();
    assert!(completed < idle);
}

#[tokio::test]
async fn idle_control_management_queries_and_shutdown_publish_no_barrier() {
    let (_directory, transcript) = test_transcript();
    let (events, receiver) = session_event_channel(1);
    let mut run = RunningSession::start(
        engine(ScriptedProvider::new([]), transcript)
            .with_mode_management()
            .with_idle_notifications(),
        [],
        events,
        receiver,
    );
    run.send(SessionCommand::Control(ControlCommand::CancelTurn {
        turn_id: None,
    }));
    run.send(SessionCommand::Manage(ManagementCommand::SetMode {
        request_id: "mode".into(),
        mode: SessionMode::Plan,
    }));
    run.until(|event| matches!(event, SessionEvent::ModeResult { .. }))
        .await;
    run.send(SessionCommand::Manage(ManagementCommand::Models {
        request_id: "models".into(),
        request: ModelManagementRequest::List {
            scope: Scope::SessionAndDefault,
            mode: SessionMode::Plan,
        },
    }));
    run.until(|event| matches!(event, SessionEvent::ModelsResult { .. }))
        .await;
    run.idle_fence("query").await;
    let events = run.shutdown().await;
    assert!(!events.iter().any(|e| matches!(
        e,
        SessionEvent::EngineIdle | SessionEvent::TurnStarted { .. }
    )));
}

#[tokio::test]
async fn ordinary_engines_do_not_publish_idle_notifications() {
    let (_directory, transcript) = test_transcript();
    let (events, receiver) = session_event_channel(1);
    let mut run = RunningSession::start(
        engine(
            ScriptedProvider::new([Ok(Message::assistant("done"))]),
            transcript,
        ),
        [submit("first")],
        events,
        receiver,
    );
    run.completed(TurnId::new(1)).await;
    let events = run.shutdown().await;
    assert!(!events.iter().any(|e| matches!(e, SessionEvent::EngineIdle)));
}
