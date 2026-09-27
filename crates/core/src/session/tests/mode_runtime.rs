use super::*;

#[tokio::test]
async fn busy_mode_requests_are_rejected_not_queued_and_idle_selection_allocates_no_turn() {
    let (_directory, transcript) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let mut engine = engine(provider, transcript).with_mode_management();
    engine
        .conversation
        .replace_session_mode(SessionMode::Build)
        .unwrap();
    let path = engine.conversation.path().to_path_buf();
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(engine, [submit("active work")], events, receiver);
    let call = run.call(&mut calls).await;
    assert_eq!(call.turn.id, TurnId::new(1));
    for mode in SessionMode::ALL {
        let request_id = format!("busy-{}", mode.name());
        run.send(SessionCommand::Manage(ManagementCommand::SetMode {
            request_id: request_id.clone(),
            mode,
        }));
        let result = run.until(|event| matches!(event, SessionEvent::ModeResult { request_id: id, .. } if id == &request_id)).await;
        assert!(
            matches!(result, SessionEvent::ModeResult { result: ModeSelectionResult::Rejected { code, .. }, .. } if code == "busy")
        );
        assert_eq!(
            zevria_transcript::transcript::session_mode(
                &zevria_transcript::transcript::load(&path).unwrap()
            )
            .unwrap(),
            Some(SessionMode::Build)
        );
    }
    answer(call, "done");
    run.completed(TurnId::new(1)).await;
    run.idle_fence("idle").await;
    assert!(calls.try_recv().is_err());
    run.send(SessionCommand::Manage(ManagementCommand::SetMode {
        request_id: "accepted".into(),
        mode: SessionMode::Plan,
    }));
    let result = run.until(|event| matches!(event, SessionEvent::ModeResult { request_id, .. } if request_id == "accepted")).await;
    assert!(matches!(
        result,
        SessionEvent::ModeResult {
            result: ModeSelectionResult::Accepted {
                mode: SessionMode::Plan,
                changed: true
            },
            ..
        }
    ));
    assert_eq!(
        zevria_transcript::transcript::session_mode(
            &zevria_transcript::transcript::load(&path).unwrap()
        )
        .unwrap(),
        Some(SessionMode::Plan)
    );
    let lifecycle = run.shutdown().await;
    assert_eq!(
        lifecycle
            .iter()
            .filter(|event| matches!(event, SessionEvent::TurnStarted { .. }))
            .count(),
        1
    );
}
