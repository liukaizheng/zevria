use super::*;
use ratatui::crossterm::event::KeyEvent;
use zevria_foundation::ModelRole;
use zevria_foundation::SessionMode;
use zevria_session_api::ManagementCommand;
use zevria_session_api::ModeSelectionResult;

fn request(views: &mut SessionViews, command: &str) -> (String, SessionMode) {
    views
        .root
        .set_focus_for_test(crate::app::FocusState::Insert);
    views.root.set_input_for_test(command, command.len());
    let Some(UiAction::SetMode { request_id, mode }) = views.handle_event(Event::Key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
    )) else {
        panic!("root mode command");
    };
    (request_id, mode)
}

#[test]
fn mode_actions_use_management_transport_and_acknowledge_only_the_root() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.seed_child_creation_order(&[TranscriptItem::SessionMode(SessionMode::Plan)]);
    assert!(views.children.is_empty());
    assert!(views.agents.is_empty());
    let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let (id, mode) = request(&mut views, "/plan");
    views.send_mode_selection(&commands, id.clone(), mode);
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode: SessionMode::Plan }) if request_id == id)
    );
    assert!(receiver.try_recv().is_err());
    let child = SubtaskId::new("child");
    let index = views.ensure_child(&child);
    views.set_visible(Some(VisiblePane::Subtask(
        views.children[index].app.surface().id.pane,
    )));
    views.apply(SessionEvent::SubtaskSession {
        id: child.clone(),
        event: Box::new(SessionEvent::ModeResult {
            request_id: id.clone(),
            result: ModeSelectionResult::Accepted {
                mode,
                changed: true,
            },
        }),
    });
    assert!(views.root.mode_selection_pending());
    assert_eq!(views.root.next_mode(), SessionMode::Build);
    views.apply(SessionEvent::ModeResult {
        request_id: id,
        result: ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    });
    assert!(!views.root.is_busy());
    assert_eq!(views.root.next_mode(), SessionMode::Plan);
    assert_eq!(views.child(&child).unwrap().next_mode(), SessionMode::Build);
    views.apply(SessionEvent::ModeChanged {
        mode: SessionMode::Plan,
    });
    assert_eq!(views.root.next_mode(), SessionMode::Plan);
    assert_eq!(views.child(&child).unwrap().next_mode(), SessionMode::Build);
}

#[test]
fn closed_mode_transport_unlocks_the_root_and_restores_the_command_draft() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    drop(receiver);
    let draft = "/plan \n";
    let (id, mode) = request(&mut views, draft);
    views.send_mode_selection(&commands, id, mode);
    assert_eq!(views.root.next_mode(), SessionMode::Build);
    assert_eq!(views.root.input(), draft);
    assert!(!views.root.is_busy());
    assert!(!views.root.mode_selection_pending());
    assert!(
        matches!(views.root.history(), [crate::app::HistoryEntry::Error(error)] if error.contains("engine_stopped"))
    );
}

#[test]
fn pending_shortcut_preserves_the_root_draft_and_cannot_cancel_root_from_a_child() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views
        .root
        .set_focus_for_test(crate::app::FocusState::Insert);
    let draft = "é draft\nnext line";
    views.root.set_input_for_test(draft, 2);
    let Some(UiAction::SetMode { request_id, mode }) = views.handle_event(Event::Key(
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    )) else {
        panic!("mode management shortcut");
    };
    let child = views.ensure_child(&SubtaskId::new("inspect-child"));
    for pane in [
        Some(VisiblePane::Subtask(
            views.children[child].app.surface().id.pane,
        )),
        None,
    ] {
        views.set_visible(pane);
        assert!(
            views
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )))
                .is_none()
        );
        assert_eq!(views.root.input(), if pane.is_some() { draft } else { "" });
        assert_eq!(
            views.root.input_cursor(),
            if pane.is_some() { 2 } else { 0 }
        );
        assert!(views.root.mode_selection_pending());
        assert_eq!(views.root.active_turn_id(), None);
    }
    views.apply(SessionEvent::ModeResult {
        request_id,
        result: ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    });
    assert!(!views.root.is_busy());
    assert_eq!(views.root.next_mode(), SessionMode::Plan);
    assert_eq!(views.root.input(), "");
    assert_eq!(views.root.input_cursor(), 0);
}

#[test]
fn delayed_question_cannot_open_a_response_path_during_mode_management() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    let (id, mode) = request(&mut views, "/plan");
    views.apply(SessionEvent::QuestionAsked {
        turn_id: zevria_foundation::TurnId::new(99),
        request: zevria_foundation::QuestionRequest {
            id: zevria_foundation::QuestionRequestId::new("stale-question"),
            source_label: None,
            dismissible: true,
            questions: vec![zevria_foundation::QuestionPrompt {
                id: "stale-prompt".into(),
                header: "Stale".into(),
                question: "Must not capture input".into(),
                options: vec![],
                kind: zevria_foundation::QuestionPromptKind::Text {
                    min_length: None,
                    max_length: None,
                },
                required: true,
                default: None,
            }],
        },
    });
    assert!(views.overlays.question().is_none());
    assert!(views.root.mode_selection_pending());
    assert!(views.root.history().is_empty());
    views.apply(SessionEvent::ModeResult {
        request_id: id,
        result: ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    });
    assert!(views.overlays.question().is_none());
    assert_eq!(views.root.next_mode(), SessionMode::Plan);
    assert!(!views.root.is_busy());
}

#[test]
fn draining_engine_shutdown_settles_unacknowledged_selection_but_keeps_an_accepted_result() {
    for acknowledged in [false, true] {
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        let draft = "/plan \n\t";
        let (id, mode) = request(&mut views, draft);
        let (events, mut updates) = zevria_session_api::session_event_channel(8);
        if acknowledged {
            events
                .try_send(SessionEvent::ModeResult {
                    request_id: id.clone(),
                    result: ModeSelectionResult::Accepted {
                        mode,
                        changed: true,
                    },
                })
                .unwrap();
        }
        drop(events);
        let mut engine_done = false;
        assert!(
            drain_update_burst(&mut views, &mut updates, &mut engine_done)
                .unwrap()
                .is_none()
        );
        assert!(engine_done);
        assert!(!views.root.is_busy());
        assert!(!views.root.mode_selection_pending());
        let selected = if acknowledged {
            SessionMode::Plan
        } else {
            SessionMode::Build
        };
        assert_eq!(views.root.next_mode(), selected);
        assert_eq!(views.root.input(), if acknowledged { "" } else { draft });
        let errors = views.root.history().len();
        assert_eq!(errors, usize::from(!acknowledged));
        views.root.mode_selection_disconnected();
        views.apply(SessionEvent::ModeResult {
            request_id: id,
            result: ModeSelectionResult::Accepted {
                mode,
                changed: true,
            },
        });
        assert_eq!(views.root.next_mode(), selected);
        assert_eq!(views.root.history().len(), errors);
    }
}

#[test]
fn pending_mode_selection_blocks_model_picker_cancellation_and_repeated_switches() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.root.apply_selected_mode(SessionMode::Plan);
    let (id, mode) = request(&mut views, "/build");
    views.open_model_picker(ModelSelectionScope::SessionOnly);
    assert!(!views.overlays.models.is_open());
    assert!(views.overlays.models.commands.is_empty());
    for key in [
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        assert!(views.handle_event(Event::Key(key)).is_none());
    }
    assert!(views.root.mode_selection_pending());
    views.apply(SessionEvent::ModeResult {
        request_id: id,
        result: ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    });
    views.open_model_picker(ModelSelectionScope::SessionOnly);
    let Some(SessionCommand::Manage(ManagementCommand::Models {
        request_id,
        request:
            zevria_model::models::ModelManagementRequest::List {
                mode: captured,
                scope,
            },
    })) = views.overlays.models.commands.pop_front()
    else {
        panic!("model selection after acknowledged mode");
    };
    assert_eq!(captured, SessionMode::Build);
    let context = zevria_foundation::ModelContextPolicy {
        profile: zevria_foundation::ModelProfileRef::new("shared", "build"),
        context_window_tokens: 10_000,
        input_token_limit: 9_000,
        retained_user_tokens: 100,
    };
    views.apply(SessionEvent::ModelsResult {
        request_id: request_id.clone(),
        result: zevria_model::models::ModelManagementResult::Catalog {
            mode: captured,
            scope,
            current: zevria_model::models::ModelSelection::new(
                context.profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            profiles: vec![zevria_model::models::ModelCandidate {
                context: context.clone(),
                reasoning_levels: vec![zevria_foundation::ReasoningLevel::Medium],
            }],
            revision: "old".into(),
        },
    });
    views.overlays.models.handle_key(KeyCode::Enter);
    views.overlays.models.handle_key(KeyCode::Enter);
    views.apply(SessionEvent::ModelsResult {
        request_id,
        result: zevria_model::models::ModelManagementResult::Changed {
            role: ModelRole::Build,
            scope,
            context: context.clone(),
            snapshot: None,
            reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            revision: "new".into(),
            unchanged: false,
        },
    });
    assert_eq!(views.root.status_for_test().profile, Some(context.profile));
    assert_eq!(views.root.next_mode(), SessionMode::Build);
    assert!(!views.root.is_busy());
}
