use super::*;
use zevria_session_api::{JobEvent, PlanEvent, QuestionEvent, TurnEvent};

#[test]
fn replaced_projection_retires_old_selection_anchors_even_when_ids_are_reused() {
    let message = || {
        assistant_message(vec![
            AssistantContent::text("first"),
            AssistantContent::text("second"),
        ])
    };
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(message())]);
    app.select_for_test(cursor(0, 1));
    let old = app.presentation_selection_anchor();
    assert!(old.is_some());
    app.restore(vec![TranscriptItem::Message(message())]);
    assert_eq!(app.selection(), None);
    app.select_for_test(cursor(0, 0));
    app.restore_presentation_selection(old);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_ne!(
        app.presentation_selection_anchor().unwrap().epoch,
        old.unwrap().epoch
    );
}

#[test]
fn hidden_ready_revises_feedback_and_identical_snapshots_preserve_dismissal() {
    let mut app = App::new();
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    let state = PlanWorkflowState::Ready { artifact };
    app.reduce_without_effects(SessionEvent::Plan(PlanEvent::StateChanged {
        state: Box::new(state.clone()),
    }));
    app.handle_event(key(KeyCode::Esc));
    assert!(app.render_parts().plan_dialog.is_none());
    app.reduce_without_effects(SessionEvent::Plan(PlanEvent::StateChanged {
        state: Box::new(state),
    }));
    assert!(app.render_parts().plan_dialog.is_none());
    enter_insert(&mut app);
    app.handle_event(Event::Paste("next draft".into()));
    assert_eq!(app.capabilities().submit_work, Ok(()));
    for capability in [
        app.capabilities().edit_transcript,
        app.capabilities().manage_session,
        app.capabilities().queue_work,
    ] {
        assert_eq!(
            capability,
            Err(crate::input::DisabledReason::PlanDecisionRequired)
        );
    }
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::RevisePlanWithMessage {
                expected,
                text: "next draft".into(),
            }
        )))
    );
    assert!(app.interaction().is_normal());
    assert!(app.is_busy());
    assert!(app.input().is_empty());
    assert!(app.render_parts().plan_dialog.is_none());
}

#[test]
fn pending_plan_can_hide_reopen_and_never_dispatch_twice() {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::Plan(PlanEvent::StateChanged {
        state: Box::new(PlanWorkflowState::Ready {
            artifact: test_plan_artifact(),
        }),
    }));
    app.handle_event(key(KeyCode::Char('1')));
    assert!(matches!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::ResolvePlan {
                decision: PlanDecision::ImplementCurrent,
                ..
            }
        )))
    ));
    app.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.render_parts().plan_dialog.is_none());
    assert!(app.is_busy());
    app.handle_event(key(KeyCode::Char('p')));
    assert!(app.render_parts().plan_dialog.unwrap().decision_pending);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
}

#[test]
fn rejection_preserves_newer_root_draft_and_exposes_explicit_recovery() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.handle_event(Event::Paste("submitted".into()));
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit { .. }
        )))
    ));
    assert!(app.interaction().is_normal());
    enter_insert(&mut app);
    app.handle_event(Event::Paste("newer".into()));
    app.reduce_without_effects(SessionEvent::Turn(TurnEvent::Rejected {
        turn_id: TEST_TURN_ID,
        error: "rejected".into(),
    }));
    assert_eq!(app.input(), "newer");
    assert!(app.interaction().is_insert());
    assert!(rendered_text(&mut app, 100, 24).contains("Rejected draft saved"));
    app.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(key(KeyCode::Esc));
    assert!(rendered_text(&mut app, 100, 24).contains("r recover draft"));
    app.handle_event(key(KeyCode::Char('z')));
    assert_eq!(app.handle_event(key(KeyCode::Char('r'))), None);
    assert!(
        app.input().is_empty(),
        "an unsupported zr chord must not activate recovery"
    );
    app.handle_event(key(KeyCode::Char('r')));
    assert_eq!(app.input(), "submitted");
    assert!(app.interaction().is_insert());
}

#[test]
fn failures_and_cancellation_preserve_explicit_insert_reentry_and_newer_draft_history() {
    for acknowledged in [false, true] {
        for event in [
            SessionEvent::Turn(TurnEvent::Failed {
                turn_id: TEST_TURN_ID,
                error: "failure".into(),
            }),
            SessionEvent::Turn(TurnEvent::Cancelled {
                turn_id: TEST_TURN_ID,
            }),
        ] {
            let mut app = App::new();
            enter_insert(&mut app);
            app.handle_event(Event::Paste("submitted".into()));
            assert!(matches!(
                app.handle_event(ctrl_enter()),
                Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit { .. }
                )))
            ));
            assert!(app.interaction().is_normal());
            enter_insert(&mut app);
            app.handle_event(Event::Paste("newer".into()));
            app.handle_event(key(KeyCode::Left));
            if acknowledged {
                app.reduce_without_effects(SessionEvent::Turn(TurnEvent::Started {
                    turn_id: TEST_TURN_ID,
                    message: Message::user("submitted"),
                    mode: SessionMode::Build,
                    references: Vec::new(),
                }));
                assert!(app.interaction().is_insert());
            }
            app.reduce_without_effects(event);
            assert!(!app.is_busy());
            assert!(app.interaction().is_insert());
            assert_eq!(app.input(), "newer");
            assert_eq!(app.input_cursor(), 4);
            app.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
            assert!(app.input().is_empty());
            app.handle_event(modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL));
            assert_eq!(app.input(), "newer");
        }
    }
}

#[test]
fn rejected_draft_recovery_wins_over_fresh_plan_retry() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.handle_event(Event::Paste("recover this draft".into()));
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit { .. }
        )))
    ));
    assert!(app.interaction().is_normal());
    enter_insert(&mut app);
    app.handle_event(Event::Paste("newer draft".into()));
    app.reduce_without_effects(SessionEvent::Turn(TurnEvent::Rejected {
        turn_id: TEST_TURN_ID,
        error: "rejected".into(),
    }));
    app.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(key(KeyCode::Esc));
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    app.restore_plan_state(PlanWorkflowState::Resolved {
        artifact,
        resolution: PlanResolution::ImplementedFresh,
    });
    assert_eq!(app.handle_event(key(KeyCode::Char('r'))), None);
    assert_eq!(app.input(), "recover this draft");
    assert!(app.interaction().is_insert());
    app.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::ResolvePlan {
                expected,
                decision: PlanDecision::ImplementFresh,
            }
        )))
    );
}

#[test]
fn modified_activation_keys_do_not_decide_questions_or_change_plan_focus() {
    let mut root = App::new();
    enter_insert(&mut root);
    let mut views = test_session_views(root);
    views.apply(SessionEvent::Turn(TurnEvent::Started {
        turn_id: TEST_TURN_ID,
        message: Message::user("work"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    let request = question_request("exact-activation");
    let request_id = request.id.clone();
    views.apply(SessionEvent::Question(QuestionEvent::Asked {
        turn_id: TEST_TURN_ID,
        request,
    }));
    let before = rendered_views_text(&mut views, 100, 28);
    for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Down, KeyCode::End] {
        assert_eq!(
            views.handle_event(modified_key(code, KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(rendered_views_text(&mut views, 100, 28), before);
    }
    assert_eq!(
        views.handle_event(key(KeyCode::Esc)),
        Some(UiAction::Session(
            zevria_session_api::SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id,
                    response: QuestionResponse::Dismissed,
                }
            )
        ))
    );

    let mut app = App::new();
    enter_insert(&mut app);
    app.reduce_without_effects(SessionEvent::Plan(PlanEvent::StateChanged {
        state: Box::new(PlanWorkflowState::Ready {
            artifact: test_plan_artifact(),
        }),
    }));
    assert!(app.interaction().is_insert());
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Esc, KeyModifiers::SHIFT)),
        None
    );
    assert!(app.render_parts().plan_dialog.is_some());
    assert!(app.interaction().is_insert());
}

#[test]
fn awaiting_transcript_edit_does_not_bind_a_new_draft_to_the_old_target() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("original")));
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.handle_event(Event::Paste(" revised".into()));
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Session(zevria_session_api::SessionCommand::Turn(
            zevria_session_api::TurnCommand::EditTranscript(_)
        )))
    ));
    assert!(app.interaction().is_normal());
    assert_eq!(app.handle_event(key(KeyCode::Char('i'))), None);
    assert!(
        app.interaction().is_normal(),
        "the pending edit still locks re-entry"
    );
    assert_eq!(
        app.capabilities().edit_draft,
        Err(crate::input::DisabledReason::TranscriptEditPending)
    );
    for input in [
        Event::Paste("newer".into()),
        modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL),
        modified_key(KeyCode::Char('v'), KeyModifiers::CONTROL),
    ] {
        assert_eq!(app.handle_event(input), None);
    }
    assert!(app.input().is_empty());
    app.reduce_without_effects(SessionEvent::Turn(TurnEvent::Rejected {
        turn_id: TEST_TURN_ID,
        error: "rejected".into(),
    }));
    assert_eq!(app.input(), "original revised");
    assert!(app.is_recalling());
    assert!(app.interaction().is_normal());
    enter_insert(&mut app);
    app.handle_event(Event::Paste(" again".into()));
    assert_eq!(app.input(), "original revised again");
}

#[test]
fn normal_cancellation_preserves_an_unfocused_draft() {
    let mut app = App::new();
    app.set_input_for_test("saved draft", 5);
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(UiAction::Session(
            zevria_session_api::SessionCommand::Control(
                zevria_session_api::ControlCommand::CancelTurn { turn_id: None }
            )
        ))
    );
    assert_eq!(app.input(), "saved draft");
    assert_eq!(app.input_cursor(), 5);
}

#[test]
fn stale_questions_cannot_capture_a_new_turn_or_an_idle_pane() {
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::Question(QuestionEvent::Asked {
        turn_id: TEST_TURN_ID,
        request: question_request("before-start"),
    }));
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
    views.apply(SessionEvent::Turn(TurnEvent::Started {
        turn_id: TEST_TURN_ID,
        message: Message::user("work"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    views.apply(SessionEvent::Question(QuestionEvent::Asked {
        turn_id: TurnId::new(9876),
        request: question_request("foreign"),
    }));
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
    views.apply(SessionEvent::Turn(TurnEvent::Cancelled {
        turn_id: TEST_TURN_ID,
    }));
    views.apply(SessionEvent::Question(QuestionEvent::Asked {
        turn_id: TEST_TURN_ID,
        request: question_request("after-finish"),
    }));
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
}

#[test]
fn jobs_overlay_is_global_captures_input_and_dismisses_without_cancelling_busy_root() {
    let mut root = App::new();
    enter_insert(&mut root);
    root.set_input_for_test("saved draft", 5);
    let mut views = SessionViews::new(root, PathBuf::from("/tmp"));
    views.apply(SessionEvent::Turn(TurnEvent::Started {
        turn_id: TEST_TURN_ID,
        message: Message::user("work"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    assert_eq!(
        views.handle_event(modified_key(KeyCode::Char('j'), KeyModifiers::ALT)),
        None
    );
    assert!(views.workspace.root.jobs_inspector().is_open());
    views.apply(SessionEvent::Job(JobEvent::Updated {
        job: Box::new(zevria_foundation::CommandJobSnapshot {
            id: zevria_foundation::CommandJobId::new("job-test01"),
            call_id: None,
            command: "sleep 600".into(),
            mode: zevria_foundation::CommandJobMode::Foreground,
            status: zevria_foundation::CommandJobStatus::Running,
            started_at: std::time::SystemTime::now(),
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        }),
    }));
    let text = rendered_views_text(&mut views, 120, 28);
    assert!(text.contains("Jobs"));
    assert!(text.contains("sleep 600"));
    assert!(!text.contains("job-test01"));
    assert!(text.contains("1 job"));
    assert!(!text.contains("1 jobs"));
    views.handle_event(Event::Paste("ignored overlay paste".into()));
    assert_eq!(views.workspace.root.input(), "saved draft");
    assert_eq!(views.handle_event(key(KeyCode::Char('x'))), None);
    assert!(
        views
            .workspace
            .root
            .jobs_inspector()
            .commands
            .iter()
            .any(|c| matches!(
                c,
                zevria_session_api::SessionCommand::Control(
                    zevria_session_api::ControlCommand::Command(
                        zevria_session_api::CommandControl {
                            action: zevria_session_api::CommandControlAction::Terminate { .. },
                            ..
                        }
                    )
                )
            ))
    );
    assert_eq!(
        views.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        None
    );
    assert!(!views.workspace.root.jobs_inspector().is_open());
    assert!(views.workspace.root.is_busy());
    assert_eq!(views.workspace.root.input(), "saved draft");
}

#[test]
fn selected_running_command_controls_stay_correlated_with_jobs_overlay_closed() {
    use zevria_foundation::{CommandJobId, CommandJobMode, CommandJobSnapshot, CommandJobStatus};
    use zevria_session_api::{
        CommandControlAction as Request, CommandControlResult as Reply, ControlCommand,
        SessionCommand,
    };
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::Turn(TurnEvent::Started {
        turn_id: TEST_TURN_ID,
        message: Message::user("Run a command"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    views.apply(SessionEvent::Turn(TurnEvent::ModelCallStarted {
        turn_id: TEST_TURN_ID,
        call: 1,
    }));
    views.apply(SessionEvent::Turn(TurnEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        message: assistant_message(vec![tool_call(
            "job-call",
            None,
            "command",
            json!({"command": "sleep 60"}),
        )]),
        attempt_id: None,
    }));
    let mut job = CommandJobSnapshot {
        id: CommandJobId::new("job-inline"),
        call_id: Some("job-call".into()),
        command: "sleep 60".into(),
        mode: CommandJobMode::Foreground,
        status: CommandJobStatus::Running,
        started_at: SystemTime::now(),
        finished_at: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
    };
    views.apply(SessionEvent::Job(JobEvent::Updated {
        job: Box::new(job.clone()),
    }));
    let selection = cursor(views.workspace.root.history().len() - 1, 0);
    views.workspace.root.select_message_for_test(selection);
    views.handle_event(key(KeyCode::Char('x')));
    assert!(
        views
            .workspace
            .root
            .jobs_inspector_mut()
            .commands
            .is_empty()
    );
    views.workspace.root.select_for_test(selection);
    let eligible = views.workspace.root.hint_eligibility();
    assert!(!eligible.disabled.contains(&crate::input::Action::Terminate));
    assert!(
        !eligible
            .disabled
            .contains(&crate::input::Action::MoveToBackground)
    );
    views.handle_event(key(KeyCode::Char('m')));
    let Some(SessionCommand::Control(ControlCommand::Command(background))) = views
        .workspace
        .root
        .jobs_inspector_mut()
        .commands
        .pop_front()
    else {
        panic!("expected job background request");
    };
    assert_eq!(
        background.action,
        Request::MoveToBackground { id: job.id.clone() }
    );
    job.mode = CommandJobMode::Background;
    views.apply(SessionEvent::Job(JobEvent::ControlResult {
        request_id: background.request_id,
        result: Box::new(Reply::Accepted(job.clone())),
    }));
    views.apply(SessionEvent::Job(JobEvent::Updated {
        job: Box::new(job.clone()),
    }));
    assert!(
        views
            .workspace
            .root
            .hint_eligibility()
            .disabled
            .contains(&crate::input::Action::MoveToBackground)
    );
    assert!(
        views
            .workspace
            .root
            .jobs_inspector_mut()
            .commands
            .is_empty()
    );
    views.handle_event(key(KeyCode::Char('x')));
    let Some(SessionCommand::Control(ControlCommand::Command(stop))) = views
        .workspace
        .root
        .jobs_inspector_mut()
        .commands
        .pop_front()
    else {
        panic!("expected job stop request");
    };
    assert_eq!(stop.action, Request::Terminate { id: job.id });
    views.apply(SessionEvent::Job(JobEvent::ControlResult {
        request_id: stop.request_id,
        result: Box::new(Reply::Rejected {
            message: "The job just finished.".into(),
        }),
    }));
    assert!(!views.workspace.root.jobs_inspector().is_open());
    assert!(views.workspace.root.is_busy());
    assert!(views.workspace.root.history().iter().any(
        |entry| matches!(entry, HistoryEntry::Error(text) if text == "The job just finished.")
    ));
}

#[test]
fn question_owns_input_above_a_suspended_picker_and_draft() {
    let mut root = App::new();
    enter_insert(&mut root);
    root.set_input_for_test("saved draft", 5);
    let mut views = SessionViews::new(root, PathBuf::from("/tmp"));
    views.apply(SessionEvent::Turn(TurnEvent::Started {
        turn_id: TEST_TURN_ID,
        message: Message::user("work"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    views.open_session_picker(vec![
        session_summary("first", Some("first choice")),
        session_summary("second", Some("second choice")),
    ]);
    views.handle_event(key(KeyCode::Down));
    let request = question_request("capturing");
    let id = request.id.clone();
    views.apply(SessionEvent::Question(QuestionEvent::Asked {
        turn_id: TEST_TURN_ID,
        request,
    }));
    views.handle_event(Event::Paste("never a pane draft".into()));
    assert_eq!(views.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(views.workspace.root.input(), "saved draft");
    assert_eq!(
        views.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(UiAction::Session(
            zevria_session_api::SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id: id,
                    response: QuestionResponse::Dismissed
                }
            )
        ))
    );
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResumeSession {
            path: PathBuf::from("/tmp/sessions/second.jsonl")
        })
    );
    assert_eq!(views.workspace.root.input(), "saved draft");
    assert!(views.workspace.root.is_busy());
}
