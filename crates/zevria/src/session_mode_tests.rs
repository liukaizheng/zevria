use super::*;
use zevria_acp::{ExecutionProfile, SessionRuntimeFactory as _, SessionStart, StartSessionRequest};
use zevria_session_api::ManagementCommand;
use zevria_session_api::ModeSelectionResult;
use zevria_workflow::PlanRecord;
use zevria_workflow::PlanWorkflowState;

async fn select_mode(
    commands: &mpsc::UnboundedSender<SessionCommand>,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    changed: bool,
) {
    let request_id = format!("select-{mode}");
    commands
        .send(SessionCommand::Manage(ManagementCommand::SetMode {
            request_id: request_id.clone(),
            mode,
        }))
        .unwrap();
    let update = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        update,
        SessionUpdate::Lifecycle(SessionEvent::ModeResult {
            request_id,
            result: ModeSelectionResult::Accepted { mode, changed },
        }),
        "selection must acknowledge management without turn, workflow, or provider events"
    );
    assert!(events.try_recv().is_err());
}

fn assert_tui_restoration_uses_selected_mode(restoration: &runtime::SessionRestoration) {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use zevria_tui::{App, UiAction};

    let mut app = App::new();
    app.restore(restoration.transcript_items.clone());
    app.restore_plan_state(restoration.plan_state.clone());
    app.apply_selected_mode(restoration.selected_mode);
    // Observe the restored composer's next action without dispatching a turn.
    assert_eq!(
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE
        ))),
        None
    );
    assert_eq!(
        app.handle_event(Event::Paste("continue the restored session".into())),
        None
    );
    assert!(matches!(
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))),
        Some(UiAction::Submit { mode, .. }) if mode == restoration.selected_mode
    ));
}

#[tokio::test]
async fn fresh_root_headers_and_selected_only_restoration_are_frontend_authoritative() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let fresh = fixture
        .start(runtime::SessionStart::New {
            inherited_models: None,
        })
        .await;
    assert_eq!(fresh.restoration().selected_mode, SessionMode::Build);
    assert!(matches!(
        fresh.restoration().transcript_items.as_slice(),
        [
            TranscriptItem::SessionModels(_),
            TranscriptItem::SessionMode(SessionMode::Build)
        ]
    ));
    let headers = fresh.restoration().transcript_items.clone();
    let fresh_path = fresh.restoration().transcript_path.clone();
    let fresh_bytes = std::fs::read(&fresh_path).unwrap();
    fresh.shutdown().await.unwrap();
    assert_eq!(std::fs::read(&fresh_path).unwrap(), fresh_bytes);
    assert!(
        !transcript::is_abandoned_root(&fresh_path),
        "the canonical Build selection survives even an immediate fresh-root close"
    );

    let factory = crate::acp_host::AcpHostFactory::new(Arc::new(load(&fixture.path).unwrap()));
    for (id, selected_mode) in [
        ("selected-build", SessionMode::Build),
        ("selected-plan", SessionMode::Plan),
    ] {
        let mut items = headers.clone();
        items[1] = TranscriptItem::SessionMode(selected_mode);
        let mut writer = TranscriptWriter::create_with_id(
            &transcript::sessions_dir(fixture.directory.path()),
            id,
        )
        .unwrap();
        writer.rewrite(&items).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        assert!(!transcript::is_abandoned_root(&path));
        for _ in 0..2 {
            let mut resumed = fixture
                .start(runtime::SessionStart::Resume(path.clone()))
                .await;
            assert_eq!(resumed.restoration().selected_mode, selected_mode);
            assert_eq!(resumed.restoration().plan_state, PlanWorkflowState::Idle);
            assert_eq!(resumed.restoration().transcript_items, items);
            assert!(resumed.take_event_receiver().unwrap().try_recv().is_err());
            resumed.shutdown().await.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }

        let mut restored = factory
            .start(StartSessionRequest {
                workspace: fixture.directory.path().to_path_buf(),
                start: SessionStart::Existing {
                    session_id: id.into(),
                },
            })
            .await
            .unwrap();
        assert_eq!(restored.selected_mode, selected_mode);
        assert_eq!(restored.plan_state, PlanWorkflowState::Idle);
        assert!(restored.events.try_recv().is_err());
        restored.lifecycle.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn production_mode_selection_survives_immediate_close_across_tui_and_acp() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let config_before = std::fs::read(&fixture.path).unwrap();
    let factory = crate::acp_host::AcpHostFactory::new(Arc::new(load(&fixture.path).unwrap()));
    for selected_mode in [SessionMode::Build, SessionMode::Plan] {
        let mut tui = fixture
            .start(runtime::SessionStart::New {
                inherited_models: None,
            })
            .await;
        let id = tui.restoration().session_id.clone();
        let path = tui.restoration().transcript_path.clone();
        let mut expected = tui.restoration().transcript_items.clone();
        let original = std::fs::read(&path).unwrap();
        let mut events = tui.take_event_receiver().unwrap();
        assert!(events.try_recv().is_err());
        select_mode(
            &tui.command_sender(),
            &mut events,
            SessionMode::Build,
            false,
        )
        .await;
        assert_eq!(std::fs::read(&path).unwrap(), original);
        select_mode(&tui.command_sender(), &mut events, SessionMode::Plan, true).await;
        select_mode(
            &tui.command_sender(),
            &mut events,
            selected_mode,
            selected_mode != SessionMode::Plan,
        )
        .await;
        // In particular, Plan -> Build returns to the exact canonical
        // fresh header, which must remain discoverable on immediate shutdown.
        tui.shutdown().await.unwrap();
        assert!(events.try_recv().is_err());
        expected[1] = TranscriptItem::SessionMode(selected_mode);
        assert_eq!(transcript::load(&path).unwrap(), expected);
        assert!(!transcript::is_abandoned_root(&path));
        assert!(!path.with_extension("jsonl.lock").exists());
        let selected_bytes = std::fs::read(&path).unwrap();
        if selected_mode == SessionMode::Build {
            assert_eq!(selected_bytes, original);
        }
        assert!(server.requests.try_recv().is_err());
        assert!(
            factory
                .list(fixture.directory.path().to_path_buf())
                .await
                .unwrap()
                .iter()
                .any(|session| session.id == id)
        );

        let mut acp = factory
            .start(StartSessionRequest {
                workspace: fixture.directory.path().to_path_buf(),
                start: SessionStart::Existing {
                    session_id: id.clone(),
                },
            })
            .await
            .unwrap();
        assert_eq!(acp.session_id, id);
        assert_eq!(acp.selected_mode, selected_mode);
        assert_eq!(acp.plan_state, PlanWorkflowState::Idle);
        assert_eq!(acp.transcript_items, expected);
        assert!(acp.events.try_recv().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), selected_bytes);
        // Drive the same real management path from an ACP-owned runtime, then
        // close directly after acknowledgement and restore through the TUI.
        let intermediate = if selected_mode == SessionMode::Plan {
            SessionMode::Build
        } else {
            SessionMode::Plan
        };
        select_mode(&acp.commands, &mut acp.events, intermediate, true).await;
        select_mode(&acp.commands, &mut acp.events, selected_mode, true).await;
        acp.lifecycle.shutdown().await.unwrap();
        assert!(acp.events.try_recv().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), selected_bytes);
        assert!(!path.with_extension("jsonl.lock").exists());
        assert!(server.requests.try_recv().is_err());

        let mut resumed_tui = fixture
            .start(runtime::SessionStart::Resume(path.clone()))
            .await;
        assert_eq!(resumed_tui.restoration().session_id, id);
        assert_eq!(resumed_tui.restoration().selected_mode, selected_mode);
        assert_eq!(
            resumed_tui.restoration().plan_state,
            PlanWorkflowState::Idle
        );
        assert_eq!(resumed_tui.restoration().transcript_items, expected);
        assert!(
            resumed_tui
                .take_event_receiver()
                .unwrap()
                .try_recv()
                .is_err()
        );
        assert_tui_restoration_uses_selected_mode(resumed_tui.restoration());
        resumed_tui.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), selected_bytes);
        assert!(!path.with_extension("jsonl.lock").exists());
        assert_eq!(std::fs::read(&fixture.path).unwrap(), config_before);
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn production_mode_selection_does_not_allocate_turns_or_call_the_provider() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let mut root = fixture
        .start(runtime::SessionStart::New {
            inherited_models: None,
        })
        .await;
    let path = root.restoration().transcript_path.clone();
    let before = std::fs::read(&path).unwrap();
    let mut events = root.take_event_receiver().unwrap();
    for mode in [SessionMode::Plan, SessionMode::Build] {
        select_mode(&root.command_sender(), &mut events, mode, true).await;
        assert!(server.requests.try_recv().is_err());
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // The first actual prompt must still own turn 1 in this same engine, not
    // merely in a new runtime that would reset an accidentally spent turn ID.
    root.command_sender()
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first real prompt after mode management".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    loop {
        match event(&mut events).await {
            SessionEvent::TurnStarted { turn_id, mode, .. } => {
                assert_eq!(turn_id, zevria_foundation::TurnId::new(1));
                assert_eq!(mode, SessionMode::Build);
                break;
            }
            SessionEvent::TurnFailed { error, .. } | SessionEvent::TurnRejected { error, .. } => {
                panic!("first real prompt failed: {error}");
            }
            _ => {}
        }
    }
    completed(
        &mut events,
        &ModelProfileRef::new("p", "old"),
        SessionMode::Build,
        100000,
    )
    .await;
    server.expect("p", "old").await;
    root.shutdown().await.unwrap();
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn roots_without_selected_mode_metadata_use_the_engine_plan_fallback() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let fresh = fixture
        .start(runtime::SessionStart::New {
            inherited_models: None,
        })
        .await;
    let headers = fresh
        .restoration()
        .transcript_items
        .iter()
        .filter(|item| !matches!(item, TranscriptItem::SessionMode(_)))
        .cloned()
        .collect::<Vec<_>>();
    fresh.shutdown().await.unwrap();
    let artifact = plan_handoff("source").artifact;
    for (id, records, state, selected_mode) in [
        (
            "legacy-idle",
            vec![],
            PlanWorkflowState::Idle,
            SessionMode::Build,
        ),
        (
            "legacy-planning",
            vec![PlanRecord::Started {
                id: artifact.version.id,
            }],
            PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: None,
            },
            SessionMode::Plan,
        ),
        (
            "legacy-ready",
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
            ],
            PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            },
            SessionMode::Plan,
        ),
    ] {
        let mut items = headers.clone();
        items.push(TranscriptItem::Message(rig_core::message::Message::user(
            "saved conversation",
        )));
        items.extend(records.into_iter().map(TranscriptItem::Plan));
        let mut writer = TranscriptWriter::create_with_id(
            &transcript::sessions_dir(fixture.directory.path()),
            id,
        )
        .unwrap();
        writer.rewrite(&items).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        let restored = fixture
            .start(runtime::SessionStart::Resume(path.clone()))
            .await;
        assert_eq!(restored.restoration().selected_mode, selected_mode);
        assert_eq!(restored.restoration().plan_state, state);
        assert_eq!(restored.restoration().transcript_items, items);
        restored.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn worker_profile_rejects_persisted_orchestrate_without_provider_or_transcript_mutation() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let fresh = fixture
        .start(runtime::SessionStart::New {
            inherited_models: None,
        })
        .await;
    let mut items = fresh.restoration().transcript_items.clone();
    items[1] = TranscriptItem::SessionMode(SessionMode::Build);
    fresh.shutdown().await.unwrap();
    let workspace = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let directory = runtime::sessions_dir(&workspace, ExecutionProfile::EnsembleWorker);
    let mut writer = TranscriptWriter::create_with_id(&directory, "invalid-worker").unwrap();
    writer.rewrite(&items).unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);
    let legacy = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"selected\":\"build\"", "\"selected\":\"orchestrate\"");
    std::fs::write(&path, legacy).unwrap();
    let before = std::fs::read(&path).unwrap();
    let factory = crate::acp_host::AcpHostFactory::with_profile(
        Arc::new(load(&fixture.path).unwrap()),
        ExecutionProfile::EnsembleWorker,
    );
    let error = match factory
        .start(StartSessionRequest {
            workspace,
            start: SessionStart::Existing {
                session_id: "invalid-worker".into(),
            },
        })
        .await
    {
        Ok(_) => panic!("worker profile must reject Orchestrate"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("legacy Orchestrate mode is no longer supported"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(zevria_app::test_support::RootSessionLease::acquire(&path).is_ok());
    assert!(server.requests.try_recv().is_err());
}
