use super::*;
use zevria_foundation::RequestBehavior;

fn busy() -> App {
    let mut app = app_with_skills();
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    app
}
fn enqueue(app: &mut App, text: &str) {
    enter_insert(app);
    app.handle_event(Event::Paste(text.into()));
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert!(app.input().is_empty());
    assert!(app.interaction().is_normal());
}
fn complete(app: &mut App, turn_id: TurnId) {
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        turn_id,
        message: Message::assistant("done"),
        attempt_id: None,
    });
}
fn idle(app: &mut App) {
    app.reduce_without_effects(SessionEvent::EngineIdle);
}
fn pause_label(app: &App) -> Option<&'static str> {
    app.queue_for_test().paused.map(|pause| pause.label())
}

#[test]
fn busy_prompt_orchestration_skill_and_image_only_drafts_are_queued_with_captured_mode() {
    for (text, mode) in [
        ("follow up", SessionMode::Build),
        ("/orchestrate x", SessionMode::Plan),
        ("$review x", SessionMode::Build),
    ] {
        let mut app = busy();
        app.set_mode_for_test(mode);
        enqueue(&mut app, text);
        let view = app.queue_for_test();
        assert_eq!(view.len, 1);
        assert_eq!(view.rows[0].mode, mode);
        assert_eq!(view.rows[0].preview, text);
        assert!(app.view_follow());
        assert!(app.history().is_empty());
    }
    let mut app = busy();
    enter_insert(&mut app);
    let Some(UiAction::ReadClipboard { generation, cursor }) = app.handle_event(ctrl('v')) else {
        panic!("paste")
    };
    app.clipboard_completed(
        generation,
        cursor,
        crate::clipboard::ClipboardResult::Image(
            zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
        ),
    );
    let expected = app.prompt_for_test();
    assert!(!expected.is_blank());
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert_eq!(app.queue_for_test().len, 1);
    assert!(app.input().is_empty());
    complete(&mut app, TEST_TURN_ID);
    idle(&mut app);
    assert!(
        matches!(app.dispatch_next_queued(), Some(UiAction::Submit { text, .. }) if text == expected)
    );
}

#[test]
fn pending_work_also_queues_but_management_workers_and_readonly_panes_do_not() {
    let mut app = App::new();
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    enqueue(&mut app, "pending follow up");
    assert_eq!(app.queue_for_test().len, 1);
    let mut management = App::new();
    assert!(
        management.begin_operation_for_test(OperationKind::ModelManagement, SessionMode::Build)
    );
    enter_insert(&mut management);
    management.handle_event(Event::Paste("not queued".into()));
    assert_eq!(management.handle_event(ctrl_enter()), None);
    assert_eq!(management.input(), "not queued");
    assert_eq!(management.queue_for_test().len, 0);
    for mut inspect in [App::subtask_inspect("child"), App::acp_inspect("external")] {
        inspect.set_input_for_test("not queued", 10);
        assert_eq!(
            inspect.capabilities().queue_work,
            Err(crate::input::DisabledReason::ReadOnly)
        );
        assert_eq!(inspect.handle_event(ctrl_enter()), None);
        assert_eq!(inspect.queue_for_test().len, 0);
    }
}

#[test]
fn busy_nonqueueable_inputs_are_inert_but_classification_errors_keep_draft_and_notice() {
    for text in [
        "/build",
        "/plan",
        "/compact",
        "/new",
        "/resume",
        "/model",
        "/implement",
        "/implement-fresh",
        "/ensemble-plan x",
        "/ensemble-review x",
    ] {
        let mut app = busy();
        enter_insert(&mut app);
        app.handle_event(Event::Paste(text.into()));
        assert_eq!(app.handle_event(ctrl_enter()), None, "{text}");
        assert_eq!(app.input(), text);
        assert!(app.interaction().is_insert());
        assert_eq!(app.queue_for_test().len, 0);
        assert!(app.history().is_empty(), "{text}: {:?}", app.history());
    }
    for text in ["/unknown", "/orchestrate", "$unknown x"] {
        let mut app = busy();
        enter_insert(&mut app);
        app.handle_event(Event::Paste(text.into()));
        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert_eq!(app.input(), text);
        assert_eq!(app.queue_for_test().len, 0);
        assert_eq!(app.history().len(), 1);
    }
}

#[test]
fn fifo_dispatch_waits_for_barriers_accepts_detached_drafts_and_preserves_new_composer() {
    let mut app = busy();
    enqueue(&mut app, "/orchestrate first");
    enqueue(&mut app, "$review second");
    complete(&mut app, TEST_TURN_ID);
    assert_eq!(app.dispatch_next_queued(), None);
    enter_insert(&mut app);
    app.handle_event(Event::Paste("newer draft".into()));
    app.handle_event(key(KeyCode::Left));
    app.set_view_for_test(3, false);
    let draft = app.draft_for_test();
    idle(&mut app);
    assert_eq!(
        app.dispatch_next_queued(),
        Some(UiAction::Submit {
            text: "first".into(),
            mode: SessionMode::Build,
            behavior: RequestBehavior::Orchestrate,
        })
    );
    assert_eq!(app.draft_for_test(), draft);
    assert!(app.interaction().is_insert());
    assert_eq!((app.view_scroll(), app.view_follow()), (3, false));
    assert_eq!(app.dispatch_next_queued(), None);
    // A stale barrier cannot release locally pending work.
    idle(&mut app);
    assert!(!app.session.is_quiescent());
    start_empty_turn(&mut app, TurnId::new(800), SessionMode::Build);
    complete(&mut app, TurnId::new(800));
    assert_eq!(app.dispatch_next_queued(), None);
    idle(&mut app);
    assert_eq!(
        app.dispatch_next_queued(),
        Some(UiAction::InvokeSkill {
            name: SkillName::parse("review").unwrap(),
            args: "second".into(),
            mode: SessionMode::Build,
        })
    );
    assert_eq!(app.queue_for_test().len, 0);
    assert_eq!(app.draft_for_test(), draft);
    start_empty_turn(&mut app, TurnId::new(801), SessionMode::Build);
    app.reduce_without_effects(SessionEvent::TurnFailed {
        turn_id: TurnId::new(801),
        error: "accepted failure".into(),
    });
    assert_eq!(app.draft_for_test(), draft);
    assert!(
        !context_help(&app).contains("recover draft"),
        "accepted drafts are not duplicated"
    );
}

#[test]
fn plan_completion_ready_idle_race_pauses_until_explicit_send_even_after_revise() {
    for insert in [false, true] {
        let mut app = busy();
        app.set_mode_for_test(SessionMode::Plan);
        enqueue(&mut app, "plan follow up");
        complete(&mut app, TEST_TURN_ID);
        assert_eq!(app.dispatch_next_queued(), None);
        let artifact = test_plan_artifact();
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            },
        });
        idle(&mut app);
        assert_eq!(pause_label(&app), Some("Plan decision required"));
        assert!(
            !context_help(&app).contains("clear queue"),
            "review Ctrl+C only hides the dialog"
        );
        assert!(!context_help(&app).contains("send queued"));
        assert_eq!(app.dispatch_next_queued(), None);
        assert!(matches!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::ResolvePlan {
                decision: PlanDecision::Revise,
                ..
            })
        ));
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact),
                draft: None,
            },
        });
        idle(&mut app);
        assert_eq!(app.dispatch_next_queued(), None);
        if insert {
            enter_insert(&mut app);
        }
        let event = if insert {
            ctrl_enter()
        } else {
            key(KeyCode::Enter)
        };
        assert!(matches!(
            app.handle_event(event),
            Some(UiAction::Submit {
                mode: SessionMode::Plan,
                ..
            })
        ));
        assert_eq!(pause_label(&app), None);
        assert!(app.interaction().is_normal());
    }
}

#[test]
fn explicit_send_also_drains_a_running_queue_without_any_idle_barrier() {
    for insert in [false, true] {
        let mut app = busy();
        enqueue(&mut app, "not stranded");
        complete(&mut app, TEST_TURN_ID);
        assert_eq!(app.dispatch_next_queued(), None);
        assert!(context_help(&app).contains("send queued"));
        if insert {
            enter_insert(&mut app);
        }
        let event = if insert {
            ctrl_enter()
        } else {
            key(KeyCode::Enter)
        };
        assert!(
            matches!(app.handle_event(event), Some(UiAction::Submit { text, .. }) if text == "not stranded".into())
        );
    }
}

#[test]
fn cancellation_pauses_at_request_time_and_idle_cancel_clears_before_quitting() {
    let mut app = busy();
    enqueue(&mut app, "wait for consent");
    assert_eq!(
        app.handle_event(ctrl('c')),
        Some(UiAction::CancelTurn {
            turn_id: Some(TEST_TURN_ID)
        })
    );
    assert_eq!(pause_label(&app), Some("cancellation requested"));
    enqueue(&mut app, "also waits");
    app.reduce_without_effects(SessionEvent::TurnCancelled {
        turn_id: TEST_TURN_ID,
    });
    idle(&mut app);
    assert_eq!(app.dispatch_next_queued(), None);
    assert!(context_help(&app).contains("clear queue"));
    assert_eq!(app.handle_event(ctrl('c')), None);
    assert_eq!(app.queue_for_test().len, 0);
    assert!(rendered_text(&mut app, 100, 24).contains("Cleared 2 queued follow-ups."));
    assert_eq!(app.handle_event(ctrl('c')), Some(UiAction::Quit));

    let mut after = busy();
    assert!(matches!(
        after.handle_event(ctrl('c')),
        Some(UiAction::CancelTurn { .. })
    ));
    enqueue(&mut after, "queued after cancellation");
    assert_eq!(pause_label(&after), None);
    after.reduce_without_effects(SessionEvent::TurnCancelled {
        turn_id: TEST_TURN_ID,
    });
    idle(&mut after);
    assert!(matches!(
        after.dispatch_next_queued(),
        Some(UiAction::Submit { .. })
    ));
}

#[test]
fn focused_cancel_clears_draft_without_cancelling_or_clearing_queue() {
    let mut app = busy();
    enqueue(&mut app, "queued");
    enter_insert(&mut app);
    app.handle_event(Event::Paste("focused draft".into()));
    assert_eq!(app.handle_event(ctrl('c')), None);
    assert!(app.input().is_empty());
    assert_eq!(app.queue_for_test().len, 1);
    assert_eq!(pause_label(&app), None);
}

#[test]
fn failed_and_rejected_direct_or_queued_work_pause_the_remaining_queue_and_recover_drafts() {
    for queued in [false, true] {
        for newer in [false, true] {
            for failed in [false, true] {
                let mut app = if queued { busy() } else { app_with_skills() };
                if queued {
                    enqueue(&mut app, "attempt");
                    enqueue(&mut app, "remaining");
                    complete(&mut app, TEST_TURN_ID);
                    idle(&mut app);
                    assert!(app.dispatch_next_queued().is_some());
                } else {
                    enter_insert(&mut app);
                    app.handle_event(Event::Paste("attempt".into()));
                    assert!(app.handle_event(ctrl_enter()).is_some());
                    enqueue(&mut app, "remaining");
                }
                if newer {
                    enter_insert(&mut app);
                    app.handle_event(Event::Paste("newer".into()));
                }
                let event = if failed {
                    SessionEvent::TurnFailed {
                        turn_id: TurnId::new(802),
                        error: "failed".into(),
                    }
                } else {
                    SessionEvent::TurnRejected {
                        turn_id: TurnId::new(802),
                        error: "rejected".into(),
                    }
                };
                app.reduce_without_effects(event);
                idle(&mut app);
                assert_eq!(
                    pause_label(&app),
                    Some(if failed {
                        "turn failed"
                    } else {
                        "turn rejected"
                    })
                );
                assert_eq!(app.dispatch_next_queued(), None);
                let retained = newer || !queued;
                assert_eq!(
                    app.input(),
                    if newer {
                        "newer"
                    } else if retained {
                        ""
                    } else {
                        "attempt"
                    }
                );
                if retained {
                    if newer {
                        app.handle_event(ctrl('c'));
                        app.handle_event(key(KeyCode::Esc));
                    }
                    app.handle_event(key(KeyCode::Char('r')));
                    assert_eq!(app.input(), "attempt");
                }
            }
        }
    }
}

#[test]
fn a_direct_prompt_like_send_while_paused_runs_ahead_and_resumes_followups() {
    for text in [
        "direct",
        "/orchestrate direct",
        "$review direct",
        "/ensemble-review direct",
    ] {
        let mut app = busy();
        enqueue(&mut app, "queued");
        app.handle_event(ctrl('c'));
        app.reduce_without_effects(SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        });
        enter_insert(&mut app);
        app.handle_event(Event::Paste(text.into()));
        let action = app.handle_event(ctrl_enter());
        assert!(
            matches!(
                action,
                Some(
                    UiAction::Submit { .. }
                        | UiAction::InvokeSkill { .. }
                        | UiAction::RunEnsemble { .. }
                )
            ),
            "{text}: {action:?}"
        );
        assert_eq!(pause_label(&app), None);
        assert_eq!(app.queue_for_test().len, 1);
        if !text.starts_with("/ensemble") {
            start_empty_turn(&mut app, TurnId::new(803), SessionMode::Build);
            complete(&mut app, TurnId::new(803));
            idle(&mut app);
            assert!(
                matches!(app.dispatch_next_queued(), Some(UiAction::Submit { text, .. }) if text == "queued".into())
            );
        }
    }
}

#[test]
fn mode_mismatch_pauses_and_only_explicit_send_uses_the_captured_mode() {
    let mut app = busy();
    enqueue(&mut app, "captured Build");
    complete(&mut app, TEST_TURN_ID);
    app.set_mode_for_test(SessionMode::Plan);
    idle(&mut app);
    assert_eq!(app.dispatch_next_queued(), None);
    assert_eq!(pause_label(&app), Some("mode changed"));
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert!(matches!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::Submit {
            mode: SessionMode::Build,
            ..
        })
    ));
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Build));
}

#[test]
fn edit_queued_restores_newest_images_cursor_and_undo_history_without_reordering_the_rest() {
    let mut app = busy();
    enqueue(&mut app, "oldest");
    enter_insert(&mut app);
    app.handle_event(Event::Paste("newest draft".into()));
    app.handle_event(key(KeyCode::Left));
    let Some(UiAction::ReadClipboard { generation, cursor }) = app.handle_event(ctrl('v')) else {
        panic!("paste")
    };
    app.clipboard_completed(
        generation,
        cursor,
        crate::clipboard::ClipboardResult::Image(
            zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
        ),
    );
    app.handle_event(key(KeyCode::Left));
    let expected_prompt = app.prompt_for_test();
    let expected_cursor = app.input_cursor();
    let mut expected_editor = crate::composer::ComposerState::default();
    expected_editor.restore(app.draft_for_test());
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert!(context_help(&app).contains("edit queued"));
    app.handle_event(key(KeyCode::Char('e')));
    assert!(app.interaction().is_insert());
    assert_eq!(app.prompt_for_test(), expected_prompt);
    assert_eq!(app.input_cursor(), expected_cursor);
    assert_eq!(app.queue_for_test().rows[0].preview, "oldest");
    assert_eq!(app.queue_for_test().len, 1);
    assert!(!context_help(&app).contains("edit queued"));
    expected_editor.undo();
    app.handle_event(ctrl('z'));
    assert_eq!(app.prompt_for_test(), expected_editor.prompt());
    assert_eq!(app.input_cursor(), expected_editor.cursor());
}

#[test]
fn recall_replacements_are_not_queued_and_edit_queued_is_unavailable_during_recall() {
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(Message::user("original"))]);
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    enqueue(&mut app, "waiting");
    complete(&mut app, TEST_TURN_ID);
    app.select_for_test(cursor(0, 0));
    app.handle_event(ctrl('e'));
    assert!(app.is_recalling());
    assert!(!context_help(&app).contains("edit queued"));
    app.set_input_for_test("replacement", 11);
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(_))
    ));
    assert_eq!(app.queue_for_test().len, 1);
}

#[test]
fn session_replacement_and_fresh_plan_choices_and_retries_refuse_to_drop_queued_work() {
    for command in ["/new", "/resume", "/implement-fresh"] {
        let mut app = busy();
        enqueue(&mut app, "keep me");
        complete(&mut app, TEST_TURN_ID);
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: test_plan_artifact(),
            },
        });
        app.handle_event(key(KeyCode::Esc));
        enter_insert(&mut app);
        app.handle_event(Event::Paste(command.into()));
        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert_eq!(app.input(), command);
        assert_eq!(app.queue_for_test().len, 1);
        let screen = rendered_text(&mut app, 120, 30);
        assert!(
            screen.contains("queued follow-ups before"),
            "{command}: {screen}"
        );
    }
    let mut app = busy();
    enqueue(&mut app, "keep me");
    complete(&mut app, TEST_TURN_ID);
    let artifact = test_plan_artifact();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });
    app.handle_event(key(KeyCode::Char('2')));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(!app.is_busy());
    app.handle_event(key(KeyCode::Esc));
    app.restore_plan_state(PlanWorkflowState::Resolved {
        artifact,
        resolution: PlanResolution::ImplementedFresh,
    });
    for event in [key(KeyCode::Enter), key(KeyCode::Char('r'))] {
        assert_eq!(app.handle_event(event), None);
        assert!(!app.is_busy());
        assert_eq!(app.queue_for_test().len, 1);
    }
}

#[test]
fn full_queue_keeps_the_twenty_first_draft_and_still_reports_classification_errors() {
    let mut app = busy();
    for index in 0..20 {
        enqueue(&mut app, &format!("follow up {index}"));
    }
    enter_insert(&mut app);
    app.handle_event(Event::Paste("twenty-first".into()));
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert_eq!(app.input(), "twenty-first");
    assert_eq!(app.queue_for_test().len, 20);
    assert!(app.interaction().is_insert());
    assert!(rendered_text(&mut app, 120, 30).contains("queue is full"));
    app.set_input_for_test("/unknown", 8);
    app.handle_event(ctrl_enter());
    assert!(rendered_text(&mut app, 120, 30).contains("Unknown command: /unknown"));
}

#[test]
fn running_queue_preserves_fifo_in_the_idle_window_before_the_barrier() {
    let mut app = busy();
    enqueue(&mut app, "first");
    complete(&mut app, TEST_TURN_ID);
    enqueue(&mut app, "second");
    assert_eq!(app.queue_for_test().len, 2);
    assert!(!app.is_busy());
    idle(&mut app);
    assert!(
        matches!(app.dispatch_next_queued(), Some(UiAction::Submit { text, .. }) if text == "first".into())
    );
}

#[test]
fn mode_changes_compaction_and_transcript_edits_do_not_resume_a_paused_queue() {
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(Message::user("original"))]);
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    enqueue(&mut app, "paused follow up");
    app.handle_event(ctrl('c'));
    app.reduce_without_effects(SessionEvent::TurnRecovered {
        turn_id: TEST_TURN_ID,
    });
    idle(&mut app);
    toggle_mode_with_ack(&mut app);
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert_eq!(pause_label(&app), Some("cancellation requested"));
    enter_insert(&mut app);
    app.handle_event(Event::Paste("/compact".into()));
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Compact { .. })
    ));
    app.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id: TurnId::new(812),
        trigger: CompactionTrigger::Manual,
    });
    app.reduce_without_effects(SessionEvent::CompactionCompleted {
        turn_id: TurnId::new(812),
        trigger: CompactionTrigger::Manual,
        backend: CompactionBackend::LocalSummary,
    });
    idle(&mut app);
    assert_eq!(pause_label(&app), Some("cancellation requested"));
    assert_eq!(app.dispatch_next_queued(), None);
    app.handle_event(key(KeyCode::Esc));
    app.select_for_test(cursor(0, 0));
    app.handle_event(ctrl('e'));
    assert!(app.is_recalling());
    app.set_input_for_test("replacement", 11);
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(_))
    ));
    start_empty_turn(&mut app, TurnId::new(813), SessionMode::Plan);
    complete(&mut app, TurnId::new(813));
    idle(&mut app);
    assert_eq!(pause_label(&app), Some("cancellation requested"));
    assert_eq!(app.dispatch_next_queued(), None);
}

#[test]
fn edit_queued_does_not_overwrite_a_nonempty_normal_composer() {
    let mut app = busy();
    enqueue(&mut app, "keep queued");
    app.set_input_for_test("new draft", 4);
    assert!(!context_help(&app).contains("edit queued"));
    app.handle_event(key(KeyCode::Char('e')));
    assert_eq!(app.input(), "new draft");
    assert_eq!(app.input_cursor(), 4);
    assert_eq!(app.queue_for_test().len, 1);
    assert!(app.interaction().is_normal());
}

#[test]
fn cancelled_unacknowledged_queued_drafts_use_staged_recovery_without_loss() {
    for newer in [false, true] {
        let mut app = busy();
        enqueue(&mut app, "cancelled queued draft");
        enqueue(&mut app, "remaining");
        complete(&mut app, TEST_TURN_ID);
        idle(&mut app);
        assert!(app.dispatch_next_queued().is_some());
        assert_eq!(
            app.handle_event(ctrl('c')),
            Some(UiAction::CancelTurn { turn_id: None })
        );
        if newer {
            enter_insert(&mut app);
            app.handle_event(Event::Paste("newer".into()));
        }
        app.reduce_without_effects(SessionEvent::TurnCancelled {
            turn_id: TurnId::new(814),
        });
        idle(&mut app);
        assert_eq!(app.dispatch_next_queued(), None);
        assert_eq!(pause_label(&app), Some("cancellation requested"));
        assert_eq!(
            app.input(),
            if newer {
                "newer"
            } else {
                "cancelled queued draft"
            }
        );
        if newer {
            app.handle_event(ctrl('c'));
            app.handle_event(key(KeyCode::Esc));
            app.handle_event(key(KeyCode::Char('r')));
            assert_eq!(app.input(), "cancelled queued draft");
        }
        assert_eq!(app.queue_for_test().len, 1);
    }
}

#[test]
fn queue_hints_and_render_tail_are_local_sanitized_and_not_history_or_selection() {
    let mut app = busy();
    enter_insert(&mut app);
    assert!(context_help(&app).contains("Ctrl+Enter queue"));
    app.handle_event(Event::Paste("first\u{1b}[31mred\u{202e}\u{7}\nnext".into()));
    app.handle_event(ctrl_enter());
    for index in 1..7 {
        enqueue(
            &mut app,
            &format!("follow up {index} {}", "long ".repeat(50)),
        );
    }
    app.handle_event(ctrl('c'));
    app.reduce_without_effects(SessionEvent::TurnRecovered {
        turn_id: TEST_TURN_ID,
    });
    for width in [1, 5, 10, 32, 120] {
        let lines = crate::render::queue_status_lines(&app.queue_for_test(), width);
        assert_eq!(lines.len(), 7, "five rows, pause header, hidden count");
        assert!(lines.iter().all(|line| line.width() <= width));
        assert_eq!(
            crate::layout::prepare::wrapped_height(&lines, width as u16),
            lines.len()
        );
    }
    assert!(crate::render::queue_status_lines(&crate::app::QueueView::default(), 80).is_empty());
    for width in [32, 120] {
        let screen = rendered_text(&mut app, width, 35);
        assert!(screen.contains("Queue paused"), "{screen}");
        assert!(screen.contains("○ Build · firstred next"), "{screen}");
        assert!(screen.contains("+2 more queued"));
        assert!(screen.contains("7 queued"));
        assert!(!screen.contains('\u{202e}'));
        assert!(!screen.contains('\u{1b}'));
        assert!(!screen.contains("Assistant"));
    }
    assert!(app.history().is_empty());
    assert!(app.render_parts().turn_starts.is_empty());
    assert!(app.view_cache().entries().is_empty());
    press_v(&mut app);
    assert_eq!(app.selection(), None);
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        None,
        "queue rows cannot be copied"
    );
    assert!(context_help(&app).contains("send queued"));
    assert!(context_help(&app).contains("edit queued"));
    assert!(context_help(&app).contains("clear queue"));
    app.handle_event(ctrl('c'));
    let help = context_help(&app);
    assert!(!help.contains("edit queued"));
    assert!(!help.contains("send queued"));
    assert!(!help.contains("clear queue"));
}

#[test]
fn hidden_root_queue_drains_while_a_child_is_visible_and_restore_clears_it() {
    let mut root = busy();
    enqueue(&mut root, "hidden root follow up");
    let mut views = test_session_views(root);
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("queue-child", "child"),
    });
    views.handle_event(ctrl('i'));
    assert_eq!(
        views.visible_child_id(),
        Some(&SubtaskId::new("queue-child"))
    );
    views.apply(SessionEvent::TurnCompleted {
        turn_id: TEST_TURN_ID,
        message: Message::assistant("done"),
        attempt_id: None,
    });
    assert_eq!(views.dispatch_queued_root_work(), None);
    views.apply(SessionEvent::EngineIdle);
    assert!(
        matches!(views.dispatch_queued_root_work(), Some(UiAction::Submit { text, .. }) if text == "hidden root follow up".into())
    );
    assert!(!rendered_views_text(&mut views, 100, 25).contains("queued"));
    let mut app = busy();
    enqueue(&mut app, "discard only on explicit restore");
    app.restore(Vec::new());
    assert_eq!(app.queue_for_test().len, 0);
    assert!(app.session.is_quiescent());
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    enqueue(&mut app, "restore session");
    app.restore_session(crate::app::RestorationInput {
        items: Vec::new(),
        workflow: PlanWorkflowState::default(),
        selected_mode: SessionMode::Build,
        model_profiles: Vec::new(),
        contexts: Vec::new(),
        reasoning: [zevria_foundation::ReasoningLevel::Medium; ModelRole::COUNT],
        persistence_error: None,
    });
    assert_eq!(app.queue_for_test().len, 0);
    assert!(app.session.is_quiescent());
}
