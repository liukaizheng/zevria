//! Overlays behavior and presentation tests.
use super::*;

#[test]
fn live_worker_chrome_tracks_idle_popup_busy_and_frozen_composer_availability() {
    use zevria_workflow::WorkerControlId;
    use zevria_workflow::WorkerControlTarget;
    use zevria_workflow::WorkerInput;
    use zevria_workflow::WorkerPromptKind;
    use zevria_workflow::WorkerReviewEvent;
    use zevria_workflow::WorkerReviewState;

    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "worker".into(),
        label: "Worker".into(),
        safe_mode: "read-only".into(),
    };
    let target = WorkerControlTarget {
        turn_id: TEST_TURN_ID,
        run_id: EnsembleRunId::new(),
        worker_id: descriptor.id.clone(),
    };
    let mut state = WorkerReviewState::new(descriptor);
    let title = "ACP · Worker · read-only · idle";
    let mut app = App::acp_inspect(title).with_model_profiles(configured_profiles());
    app.bind_worker_review(target.clone(), Box::new(state.clone()));
    app.update_pane_metadata(
        title,
        Some(crate::status::ExternalContextUsage {
            used: 1_200,
            size: 8_000,
        }),
    );
    let normal = format!("{}{}", rendered_text(&mut app, 180, 20), context_help(&app));
    for hint in [
        "i input",
        "j/↓ scroll down",
        "v select",
        "Ctrl+O root",
        "Ctrl+C clear/cancel worker",
        "d diagnostics",
        "context 1.2k/8.0k",
        title,
    ] {
        assert!(normal.contains(hint), "missing {hint}: {normal}");
    }
    for misleading in [
        "c confirm",
        "Ctrl+Y confirm",
        "inspect only",
        "Shift+Tab",
        "/implement",
        "cancel turn",
        "provider-build",
        "provider-plan",
    ] {
        assert!(!normal.contains(misleading), "{normal}");
    }
    for width in [80, 36] {
        let compact = rendered_text(&mut app, width, 20);
        for hint in ["i input", "v select", "? help"] {
            assert!(compact.contains(hint), "missing {hint}: {compact}");
        }
    }
    assert!(!cursor_visible_after_render(&mut app, 100, 20));

    enter_insert(&mut app);
    let wide_insert = rendered_text(&mut app, 180, 20);
    assert!(wide_insert.contains("Ctrl+Z undo"));
    assert!(wide_insert.contains("Ctrl+Y redo"));
    assert!(!wide_insert.contains("Ctrl+Y confirm"));
    let insert = rendered_text(&mut app, 100, 20);
    for hint in [
        "Ctrl+Enter send",
        "Ctrl+Z undo",
        "Ctrl+Y redo",
        "Esc normal",
    ] {
        assert!(insert.contains(hint), "{insert}");
    }
    assert!(!insert.contains("Shift+Tab"));
    assert!(cursor_visible_after_render(&mut app, 100, 20));
    app.handle_event(Event::Paste("/re".into()));
    let popup = rendered_text(&mut app, 100, 20);
    for hint in [
        "Enter accept/run",
        "Tab complete",
        "Ctrl+Enter send",
        "Esc cancel",
        "command",
        "Retry this worker",
    ] {
        assert!(popup.contains(hint), "{popup}");
    }
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    // Busy work leaves the draft, completion and cursor available.
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: WorkerInput {
                generation: 1,
                request_id: WorkerControlId::new(),
                kind: WorkerPromptKind::Initial,
                text: "initial".into(),
            },
        })
        .unwrap();
    app.bind_worker_review(target.clone(), Box::new(state.clone()));
    let busy = rendered_text(&mut app, 100, 20);
    assert!(busy.contains("busy"));
    assert!(!busy.contains("locked"));
    assert!(busy.contains("Retry this worker"));
    assert!(app.command_menu_active());
    assert_eq!(app.input_cursor(), 3);
    assert!(cursor_visible_after_render(&mut app, 100, 20));
    // Leave completion/editor deliberately before using transcript shortcuts.
    app.set_focus_for_test(crate::app::FocusState::Normal);
    app.handle_event(key(KeyCode::Char('d')));
    assert!(context_help(&app).contains("d hide diagnostics"));
    app.seed_history_entry(history_message(Message::assistant(
        "selectable worker discussion",
    )));
    rendered_text(&mut app, 180, 20);
    let scroll = app.view_scroll();
    assert!(!app.can_submit_work());
    press_v(&mut app);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert!(!app.interaction().selection_reveal());
    assert!(!app.view_follow());
    assert_eq!(app.input(), "/re");
    let selected = rendered_text(&mut app, 180, 20);
    assert_eq!(app.view_scroll(), scroll);
    assert!(selected.contains("y copy"));
    assert!(selected.contains("Enter blocks"));
    assert!(!selected.contains("Ctrl+E edit"));
    assert!(!cursor_visible_after_render(&mut app, 100, 20));
    app.handle_event(key(KeyCode::Esc));

    state
        .apply(&WorkerReviewEvent::Dispatched {
            generation: 1,
            attempt: 1,
        })
        .unwrap();
    state
        .apply(&WorkerReviewEvent::Settled {
            generation: 1,
            failure: None,
            connected: true,
            evidence: Box::new(state.evidence.clone()),
        })
        .unwrap();
    app.bind_worker_review(target, Box::new(state));
    assert!(!rendered_text(&mut app, 100, 20).contains("busy · locked"));
    assert!(!cursor_visible_after_render(&mut app, 100, 20));
    enter_insert(&mut app);
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    app.freeze_worker();
    let frozen = rendered_text(&mut app, 360, 20);
    assert!(frozen.contains("inspect only"), "{frozen}");
    assert!(!app.can_submit_work());
    assert!(frozen.contains("Ctrl+O root"));
    assert!(!frozen.contains("Ctrl+Y confirm"));
    assert!(!frozen.contains("Ctrl+Z undo"));
    assert!(!frozen.contains("Ctrl+Y redo"));
    assert!(!frozen.contains("/re"));
    assert_eq!(
        app.input(),
        "/re",
        "freezing hides, but does not discard, the draft"
    );
    assert!(!cursor_visible_after_render(&mut app, 100, 20));
}

#[test]
fn defaults_to_build_and_backtab_toggles_only_while_idle() {
    let mut app = App::new();
    app.set_input_for_test("preserved draft", "preserved draft".len());

    assert_eq!(app.next_mode(), SessionMode::Build);
    assert_eq!(app.in_flight_mode(), None);
    assert_eq!(app.plan_state(), PlanWorkflowState::Idle);

    toggle_mode_with_ack(&mut app);
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert_eq!(app.input(), "preserved draft");
    toggle_mode_with_ack(&mut app);
    assert_eq!(app.next_mode(), SessionMode::Build);

    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    assert_eq!(app.handle_event(key(KeyCode::BackTab)), None);
    assert_eq!(app.next_mode(), SessionMode::Build);
    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TEST_TURN_ID,
        error: "rejected".to_string(),
    });

    app.select_for_test(cursor(0, 0));
    assert_eq!(app.handle_event(key(KeyCode::BackTab)), None);
    assert_eq!(app.next_mode(), SessionMode::Build);
    app.select_for_test(None);

    app.set_mode_for_test(SessionMode::Plan);
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    });
    assert_eq!(app.handle_event(key(KeyCode::BackTab)), None);
    assert_eq!(app.next_mode(), SessionMode::Plan);
}

#[test]
fn turn_completed_alone_never_opens_plan_approval() {
    let mut app = App::new();
    toggle_mode_with_ack(&mut app);
    enter_insert(&mut app);
    for ch in "plan this".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "plan this".into(),
            mode: SessionMode::Plan,
        })
    );
    assert!(app.is_busy());
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Plan));

    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("plan this"),
            mode: SessionMode::Plan,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("the plan"),
        },
    );
    assert!(!app.is_busy());
    assert_eq!(app.in_flight_mode(), None);
    assert_eq!(app.plan_state(), PlanWorkflowState::Idle);
    assert_eq!(app.next_mode(), SessionMode::Plan);
}

#[test]
fn plan_approval_keeps_conversation_navigation_available() {
    let mut app = App::new();
    app.set_view_for_test(25, false);
    let artifact = test_plan_artifact();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready { artifact },
    });

    for (key_code, expected_scroll) in [(KeyCode::PageUp, 24), (KeyCode::PageDown, 25)] {
        assert_eq!(app.handle_event(key(key_code)), None);
        assert_eq!(app.view_scroll(), expected_scroll);
        assert!(!app.view_follow());
        assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));
    }

    // ↑/↓ move the option highlight instead of scrolling.
    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.plan_choice(), PlanChoice::Revise);
    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.plan_choice(), PlanChoice::ImplementFresh);
    assert_eq!(app.view_scroll(), 25);

    assert_eq!(app.handle_event(key(KeyCode::Home)), None);
    assert_eq!(app.view_scroll(), 0);
    assert!(!app.view_follow());
    assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));

    assert_eq!(app.handle_event(key(KeyCode::End)), None);
    assert!(app.view_follow());
    assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));
}

#[test]
fn short_plan_choice_area_reveals_each_clamped_navigation_target() {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: test_plan_artifact(),
        },
    });

    let first = rendered_text(&mut app, 100, 4);
    assert!(first.contains("3. Revise the plan"));
    assert!(first.contains('█'));

    app.handle_event(key(KeyCode::Up));
    let second = rendered_text(&mut app, 100, 4);
    assert!(second.contains("2. Clear context, then implement the plan"));

    app.handle_event(key(KeyCode::Up));
    let third = rendered_text(&mut app, 100, 4);
    assert!(third.contains("1. Implement the plan"));

    app.handle_event(key(KeyCode::Up));
    let wrapped = rendered_text(&mut app, 100, 4);
    assert_eq!(app.plan_choice(), PlanChoice::Implement);
    assert!(wrapped.contains("1. Implement the plan"));

    let mut terminal = Terminal::new(TestBackend::new(100, 11)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("render plan");
    let buffer = terminal.backend().buffer();
    let thumbs = (5..10)
        .filter(|&y| buffer[(98, y)].symbol() == "█")
        .map(|y| (98, y))
        .collect::<Vec<_>>();
    assert!(
        thumbs.is_empty(),
        "the fitting Plan viewport should not draw a thumb: {thumbs:?}"
    );
}

#[test]
fn plan_revision_is_a_versioned_engine_decision_and_failures_do_not_open_approval() {
    for dismiss in [KeyCode::Char('n'), KeyCode::Esc] {
        let mut app = App::new();
        app.set_focus_for_test(crate::app::FocusState::Insert);
        let artifact = test_plan_artifact();
        let expected = artifact.version;
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            },
        });

        assert_eq!(app.handle_event(key(dismiss)), None);
        assert!(!app.is_busy());
        if dismiss == KeyCode::Esc {
            app.handle_event(key(KeyCode::Char('p')));
        }
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::ResolvePlan {
                expected,
                decision: PlanDecision::Revise,
            })
        );
        assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Planning {
                id: expected.id,
                previous: Some(artifact),
            },
        });
        assert!(matches!(
            app.plan_state(),
            PlanWorkflowState::Planning { .. }
        ));
        assert_eq!(app.next_mode(), SessionMode::Plan);
        assert!(!app.is_busy());
        assert_eq!(app.interaction().is_insert(), dismiss != KeyCode::Esc);
    }

    let mut failed = App::new();
    failed.set_mode_for_test(SessionMode::Plan);
    assert!(failed.begin_operation_for_test(OperationKind::Submit, SessionMode::Plan));
    apply_turn_event(
        &mut failed,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "planning failed".to_string(),
        },
    );
    assert!(!matches!(
        failed.plan_state(),
        PlanWorkflowState::Ready { .. }
    ));
    assert_eq!(failed.in_flight_mode(), None);
    assert_eq!(failed.next_mode(), SessionMode::Plan);

    let mut build = App::new();
    assert!(build.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    apply_turn_event(
        &mut build,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("built"),
        },
    );
    assert!(!matches!(
        build.plan_state(),
        PlanWorkflowState::Ready { .. }
    ));
}

#[test]
fn plan_approval_actions_carry_the_displayed_version() {
    let mut app = App::new();
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready { artifact },
    });
    assert_eq!(app.plan_choice(), PlanChoice::Revise);

    assert_eq!(app.handle_event(key(KeyCode::Char('2'))), None);
    assert_eq!(app.plan_choice(), PlanChoice::ImplementFresh);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementFresh,
        })
    );
    assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));
}

#[test]
fn escaped_plan_approval_reopens_with_p_and_keeps_revision_drafts_explicit() {
    let mut app = App::new();
    app.set_focus_for_test(crate::app::FocusState::Insert);
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert!(matches!(app.plan_state(), PlanWorkflowState::Ready { .. }));
    assert!(!app.is_busy());
    app.handle_event(key(KeyCode::Char('p')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::Revise,
        })
    );
    assert!(!app.interaction().is_insert());
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Planning {
            id: expected.id,
            previous: Some(artifact),
        },
    });

    app.set_input_for_test(
        "unfinished revision draft",
        "unfinished revision draft".len(),
    );
    assert_eq!(app.handle_event(key(KeyCode::Char('p'))), None);
    assert!(app.plan_recovery_open());
    let recovered = rendered_text(&mut app, 120, 11);
    assert!(recovered.contains("Submitted plan"));
    assert!(recovered.contains(&expected.to_string()));
    assert!(recovered.contains("3. Continue revising the plan"));
    assert!(recovered.contains("Esc close"));

    assert_eq!(app.handle_event(key(KeyCode::Char('3'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(!app.plan_recovery_open());
    assert_eq!(app.input(), "unfinished revision draft");

    assert_eq!(app.handle_event(key(KeyCode::Char('p'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert!(!app.plan_recovery_open());
    assert!(matches!(
        app.plan_state(),
        PlanWorkflowState::Planning { .. }
    ));

    assert_eq!(app.handle_event(key(KeyCode::Char('p'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('2'))), None);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementFresh,
        })
    );
    assert!(
        app.plan_recovery_open(),
        "the pending decision remains reviewable until the engine settles it"
    );
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(
        app.input().is_empty(),
        "unfinished revision prose is not approved"
    );
}

#[test]
fn plan_review_can_reopen_while_busy_but_cannot_dispatch_another_decision() {
    let artifact = test_plan_artifact();
    let state = PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact),
    };

    let mut insert = App::new();
    insert.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: state.clone(),
    });
    enter_insert(&mut insert);
    assert_eq!(insert.handle_event(key(KeyCode::Char('p'))), None);
    assert!(!insert.plan_recovery_open());
    assert_eq!(insert.input(), "p");

    let mut busy = App::new();
    busy.reduce_without_effects(SessionEvent::PlanStateChanged { state });
    assert!(busy.begin_operation_for_test(OperationKind::Submit, SessionMode::Plan));
    assert_eq!(busy.handle_event(key(KeyCode::Char('p'))), None);
    assert!(busy.plan_recovery_open());
    busy.handle_event(key(KeyCode::Char('1')));
    assert_eq!(busy.handle_event(key(KeyCode::Enter)), None);
    assert!(busy.is_busy());

    let mut empty = App::new();
    assert_eq!(empty.handle_event(key(KeyCode::Char('p'))), None);
    assert!(!empty.plan_recovery_open());
}

#[test]
fn revision_state_reopens_input_and_partial_completion_stays_planning() {
    let mut app = App::new();
    let artifact = test_plan_artifact();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact),
        },
    });
    assert_eq!(app.next_mode(), SessionMode::Plan);
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("revise"),
            mode: SessionMode::Plan,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("I need more information."),
        },
    );
    assert!(matches!(
        app.plan_state(),
        PlanWorkflowState::Planning { .. }
    ));
    assert!(!app.is_busy());
}

#[test]
fn resumed_ready_plan_is_deduplicated_and_copyable() {
    let artifact = test_plan_artifact();
    let state = PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    };
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Plan(PlanRecord::Started {
            id: artifact.version.id,
        }),
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
    ]);
    app.restore_plan_state(state);

    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert_eq!(
        app.history()
            .iter()
            .filter(|entry| matches!(entry, HistoryEntry::PlanArtifact(_)))
            .count(),
        1,
        "the transcript row and restored snapshot must not duplicate the artifact"
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown.clone(),
        })
    );
    app.set_view_for_test(0, false);
    let rendered = rendered_text(&mut app, 120, 30);
    assert!(rendered.contains("Plan artifact · revision 1"));
    assert!(rendered.contains(&artifact.title));
    assert!(rendered.contains("Plan ready"));
    assert!(!app.is_busy());
    assert_eq!(app.active_turn_id(), None);

    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact.clone()),
        },
    });
    rendered_text(&mut app, 120, 30);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown.clone(),
        })
    );
    let expected = artifact.version;
    app.restore_plan_state(PlanWorkflowState::Ready { artifact });
    assert_eq!(
        app.selection(),
        None,
        "Ready clears the retained-row selection"
    );
    app.handle_event(key(KeyCode::Char('1')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementCurrent,
        })
    );
    assert!(app.is_busy());
}

#[test]
fn plan_restoration_matrix_seeds_ui_without_turns_and_accepts_live_transitions() {
    let artifact = test_plan_artifact();
    for state in [
        PlanWorkflowState::Idle,
        PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: None,
        },
        PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact.clone()),
        },
        PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
        PlanWorkflowState::Resolved {
            artifact: artifact.clone(),
            resolution: PlanResolution::ImplementedCurrent,
        },
    ] {
        let mut items = Vec::new();
        if !matches!(state, PlanWorkflowState::Idle) {
            items.push(TranscriptItem::Plan(PlanRecord::Started {
                id: artifact.version.id,
            }));
        }
        if state.artifact().is_some() {
            items.push(TranscriptItem::Plan(PlanRecord::Ready {
                artifact: artifact.clone(),
            }));
        }
        match &state {
            PlanWorkflowState::Planning {
                previous: Some(_), ..
            } => items.push(TranscriptItem::Plan(PlanRecord::RevisionRequested {
                artifact: artifact.clone(),
            })),
            PlanWorkflowState::Resolved { resolution, .. } => {
                items.push(TranscriptItem::Plan(PlanRecord::Resolved {
                    id: artifact.version.id,
                    artifact: Some(artifact.clone()),
                    resolution: *resolution,
                }))
            }
            _ => {}
        }
        let mut app = App::new();
        app.restore_plan_state(PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        });
        app.restore(items);
        assert_eq!(
            app.plan_state(),
            PlanWorkflowState::Idle,
            "transcript restore resets workflow first"
        );
        app.restore_plan_state(state.clone());
        assert_eq!(app.plan_state(), state);
        assert_eq!(
            app.next_mode(),
            if matches!(state, PlanWorkflowState::Ready { .. }) {
                SessionMode::Plan
            } else {
                SessionMode::Build
            }
        );
        assert!(!app.is_busy(), "restoration must not queue an operation");
        assert_eq!(app.active_turn_id(), None);
        assert_eq!(
            app.history()
                .iter()
                .filter(|entry| matches!(entry, HistoryEntry::PlanArtifact(_)))
                .count(),
            usize::from(state.artifact().is_some())
        );
        assert_eq!(
            rendered_text(&mut app, 120, 30).contains("Plan ready"),
            matches!(state, PlanWorkflowState::Ready { .. })
        );
        assert!(!app.plan_recovery_open());
        if matches!(
            state,
            PlanWorkflowState::Planning {
                previous: Some(_),
                ..
            }
        ) {
            assert!(app.open_plan_recovery_for_test());
            assert!(app.plan_recovery_open());
            assert_eq!(
                app.handle_event(key(KeyCode::Char('y'))),
                Some(UiAction::Copy {
                    text: artifact.markdown.clone()
                })
            );
        }

        // Live Ready replaces any restored or locally opened dialog, then the
        // correlated revision transition settles its pending decision normally.
        let mut revised = artifact.clone();
        revised.version.revision += 1;
        revised.markdown.push_str("\nNew live revision.\n");
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: revised.clone(),
            },
        });
        assert!(rendered_text(&mut app, 120, 30).contains("Plan ready"));
        assert!(!app.plan_recovery_open());
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::ResolvePlan {
                expected: revised.version,
                decision: PlanDecision::Revise
            })
        );
        assert!(app.is_busy());
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Planning {
                id: revised.version.id,
                previous: Some(revised.clone()),
            },
        });
        assert!(!app.is_busy());
        assert!(!rendered_text(&mut app, 120, 30).contains("Plan ready"));
        assert_eq!(app.next_mode(), SessionMode::Plan);
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Resolved {
                artifact: revised,
                resolution: PlanResolution::ImplementedFresh,
            },
        });
        assert_eq!(
            app.next_mode(),
            SessionMode::Plan,
            "artifact snapshots retain explicit selection"
        );
        app.reduce_without_effects(SessionEvent::ModeChanged {
            mode: SessionMode::Build,
        });
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(!app.plan_recovery_open());
        assert_eq!(app.active_turn_id(), None);
    }
}

#[test]
fn direct_plan_snapshot_preserves_composer_lock_and_settles_matching_operations() {
    let mut app = App::new();
    app.set_focus_for_test(crate::app::FocusState::Insert);
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    app.restore_plan_state(PlanWorkflowState::Idle);
    assert!(app.is_busy());
    assert!(
        app.interaction().is_insert(),
        "busy work does not revoke draft editing"
    );

    let artifact = test_plan_artifact();
    let mut app = App::new();
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected: artifact.version,
            decision: PlanDecision::Revise
        })
    );
    assert!(app.is_busy());
    app.restore_plan_state(PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact),
    });
    assert!(!app.is_busy());
    assert_eq!(app.active_turn_id(), None);
}

#[test]
fn handoff_is_a_semantic_selectable_row_instead_of_user_prose() {
    let artifact = test_plan_artifact();
    let handoff = PlanHandoff::new(artifact.clone(), "source-session");
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Resolved {
            artifact: artifact.clone(),
            resolution: PlanResolution::ImplementedCurrent,
        },
    });
    app.reduce_without_effects(SessionEvent::PlanHandoffStarted {
        turn_id: TEST_TURN_ID,
        handoff: handoff.clone(),
    });
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("Implemented."),
        },
    );

    assert!(matches!(
        app.history().first(),
        Some(HistoryEntry::PlanHandoff(live, _)) if live == &handoff
    ));
    assert!(
        !app.history()
            .iter()
            .any(|entry| conversation_has_role(entry, PresentationRole::User))
    );
    app.set_view_for_test(0, false);
    let rendered = rendered_text(&mut app, 120, 30);
    assert!(rendered.contains("Approved Plan handoff"));
    assert!(rendered.contains("source source-session"));
    assert!(
        !rendered.contains("Implemented."),
        "newer content is below the pane"
    );

    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown.clone(),
        })
    );

    let mut restored = App::new();
    restored.restore(vec![TranscriptItem::Plan(PlanRecord::Handoff {
        handoff: handoff.clone(),
    })]);
    assert!(matches!(
        restored.history(),
        [HistoryEntry::PlanHandoff(saved, _)] if saved == &handoff
    ));
    rendered_text(&mut restored, 120, 30);
    double_escape(&mut restored);
    assert_eq!(
        restored.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown,
        })
    );
}

#[test]
fn streaming_handoff_preserves_top_after_home_and_gg() {
    for navigation in [vec![KeyCode::Home], vec![KeyCode::Char('g'); 2]] {
        for render_before_update in [false, true] {
            for assistant_snapshot in [false, true] {
                let mut app = streaming_handoff_app(false);
                assert!(app.interaction().is_normal());
                for &code in &navigation {
                    assert_eq!(app.handle_event(key(code)), None);
                }
                assert_eq!(app.view_scroll(), 0);
                assert!(!app.view_follow());
                assert_eq!(app.selection(), None);
                if render_before_update {
                    assert!(rendered_text(&mut app, 80, 18).contains("Approved Plan handoff"));
                    assert_eq!(app.view_scroll(), 0);
                }

                for event in [
                    handoff_stream_update(10, assistant_snapshot),
                    handoff_stream_update(12, assistant_snapshot),
                    SessionEvent::ToolResults {
                        turn_id: TEST_TURN_ID,
                        message: tool_result_message(
                            "handoff-tool",
                            None,
                            "command",
                            "implemented",
                        ),
                        metadata: vec![],
                    },
                    handoff_stream_update(14, assistant_snapshot),
                    SessionEvent::TurnCompleted {
                        turn_id: TEST_TURN_ID,
                        display_attempt_id: None,
                        message: Message::assistant("Implementation complete."),
                    },
                ] {
                    app.reduce_without_effects(event);
                    let rendered = rendered_text(&mut app, 80, 18);
                    assert_eq!(
                        app.view_scroll(),
                        0,
                        "navigation={navigation:?}, render_before_update={render_before_update}, \
                         assistant_snapshot={assistant_snapshot}"
                    );
                    assert!(!app.view_follow());
                    assert_eq!(app.selection(), None);
                    assert!(rendered.contains("Approved Plan handoff"));
                    assert!(rendered.contains("Durable approval workflow"));
                }
                assert_eq!(tool_status(&app, 1, 1), ToolCallStatus::Finished);
                assert!(!app.is_busy());
                assert!(app.streaming().is_none());

                // Selection must still enter on the visible approved plan,
                // not on the later implementation content.
                let HistoryEntry::PlanHandoff(handoff, _) = &app.history()[0] else {
                    panic!("approved handoff");
                };
                let markdown = handoff.artifact.markdown.clone();
                double_escape(&mut app);
                assert_eq!(app.selection(), cursor(0, 0));
                assert_eq!(
                    app.handle_event(key(KeyCode::Char('y'))),
                    Some(UiAction::Copy { text: markdown })
                );
                assert_eq!(app.view_scroll(), 0);
                assert!(!app.view_follow());
            }
        }
    }
}

#[test]
fn streaming_handoff_preserves_manual_rows_until_end_or_g_resumes_follow() {
    for with_compaction in [false, true] {
        for render_before_update in [false, true] {
            for bottom_key in [KeyCode::End, KeyCode::Char('G')] {
                let mut app = streaming_handoff_app(with_compaction);
                let (top, visible_text) = if with_compaction {
                    (app.view_cache().entries()[0].extent(), "Context compacted")
                } else {
                    (3, "Own approval in the engine.")
                };
                // Use real upward navigation from the live tail to a row inside
                // the handoff or the divider before later committed content.
                for _ in top..app.view_scroll() {
                    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
                }
                assert_eq!(app.view_scroll(), top);
                assert!(!app.view_follow());
                if render_before_update {
                    assert!(rendered_text(&mut app, 80, 18).contains(visible_text));
                    assert_eq!(app.view_scroll(), top);
                }
                for rows in [10, 12, 14] {
                    app.reduce_without_effects(handoff_stream_update(rows, true));
                    let rendered = rendered_text(&mut app, 80, 18);
                    assert_eq!(
                        app.view_scroll(),
                        top,
                        "with_compaction={with_compaction}, render_before_update={render_before_update}"
                    );
                    assert!(!app.view_follow());
                    assert!(rendered.contains(visible_text));
                }

                assert_eq!(app.handle_event(key(bottom_key)), None);
                rendered_text(&mut app, 80, 18);
                assert!(app.view_follow());
                let bottom = app.view_scroll();
                assert!(bottom > top);
                app.reduce_without_effects(handoff_stream_update(30, true));
                let rendered = rendered_text(&mut app, 80, 18);
                assert!(app.view_follow());
                assert!(app.view_scroll() > bottom);
                assert!(rendered.contains("Implementation stream row."));
            }
        }
    }
}

#[test]
fn resumed_fresh_resolution_returns_to_build_and_exposes_typed_retry() {
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Plan(PlanRecord::Started {
            id: artifact.version.id,
        }),
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::Resolved {
            id: artifact.version.id,
            artifact: Some(artifact.clone()),
            resolution: PlanResolution::ImplementedFresh,
        }),
    ]);
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Resolved {
            artifact,
            resolution: PlanResolution::ImplementedFresh,
        },
    });

    assert_eq!(app.next_mode(), SessionMode::Build);
    assert!(rendered_text(&mut app, 120, 12).contains("Fresh Plan handoff pending"));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementFresh,
        })
    );
}

#[test]
fn mode_and_approval_states_render_distinct_titles_bodies_and_cursor_visibility() {
    let mut app = App::new();
    enter_insert(&mut app);
    let build = rendered_text(&mut app, 100, 20);
    assert!(build.contains("Build"));
    assert!(build.contains("Shift+Tab Plan"));
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    toggle_mode_with_ack(&mut app);
    let plan = rendered_text(&mut app, 100, 20);
    assert!(plan.contains("Plan"));
    assert!(plan.contains("Shift+Tab Build"));
    assert!(plan.contains("Ctrl+Enter send"));
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Plan));
    let planning = rendered_text(&mut app, 100, 20);
    assert!(planning.contains("Plan · Waiting"));
    assert!(planning.contains("Ctrl+Z undo"));
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TEST_TURN_ID,
        error: "rejected".to_string(),
    });
    let artifact = test_plan_artifact();
    let version = artifact.version;
    app.restore_plan_state(PlanWorkflowState::Ready { artifact });
    app.set_input_for_test("hidden draft", "hidden draft".len());
    let approval = rendered_text(&mut app, 100, 20);
    assert!(approval.contains("Plan ready"));
    assert!(approval.contains("j next"));
    assert!(approval.contains("1. Implement the plan"));
    assert!(approval.contains("2. Clear context, then implement the plan"));
    assert!(approval.contains("3. Revise the plan"));
    assert!(!approval.contains("hidden draft"));
    assert!(!cursor_visible_after_render(&mut app, 100, 20));

    let located = rendered_text(&mut app, 120, 20);
    assert!(located.contains("Plan ready"));
    assert!(located.contains(&version.to_string()));
}

#[test]
fn token_usage_renders_in_the_footer_and_survives_failures() {
    let mut app = App::new();
    let stats = "last in 12.3k · cached 10.2k (83%) · out 1.4k · total 13.7k";
    assert!(
        !rendered_text(&mut app, 120, 8).contains("cached"),
        "the footer stays empty until the first completed response"
    );

    apply_turn_event(
        &mut app,
        SessionEvent::UsageUpdated {
            turn_id: TEST_TURN_ID,
            usage: TokenUsage {
                input_tokens: 12_300,
                cached_tokens: 10_200,
                output_tokens: 1_400,
                total_tokens: 13_700,
            },
            profile: zevria_foundation::ModelProfileRef::new("test", "model"),
            model_role: zevria_foundation::ModelRole::Build,
            input_token_limit: 100_000,
            context_window_tokens: 100_000,
        },
    );
    assert!(rendered_text(&mut app, 120, 8).contains(stats));

    // The accounting describes a response the server completed, so a failed
    // turn afterwards keeps it visible.
    apply_turn_event(
        &mut app,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "boom".to_string(),
        },
    );
    assert!(rendered_text(&mut app, 120, 8).contains(stats));

    // Plan approval selects the independent Plan telemetry slot rather than
    // reusing the latest Build response.
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    });
    assert!(!rendered_text(&mut app, 120, 8).contains(stats));
}

#[test]
fn projected_and_last_response_usage_render_separately_and_survive_failure() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::UsageUpdated {
            turn_id: TEST_TURN_ID,
            usage: TokenUsage {
                input_tokens: 12_300,
                cached_tokens: 10_200,
                output_tokens: 1_400,
                total_tokens: 13_700,
            },
            profile: zevria_foundation::ModelProfileRef::new("test", "model"),
            model_role: zevria_foundation::ModelRole::Build,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ContextUsageUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: zevria_model::ContextTokenSnapshot {
                profile: zevria_foundation::ModelProfileRef::new("test", "model"),
                model_role: zevria_foundation::ModelRole::Build,
                projected_input_tokens: 17_800,
                source: zevria_model::ContextTokenSource::UsagePlusDelta,
                automatic_trigger: 90_000,
                input_token_limit: 100_000,
                context_window_tokens: 128_000,
            },
        },
    );
    let summary = "next 17.8k/100.0k usage+delta · last total 13.7k";
    // Leave room for the active engine-local turn suffix as well as telemetry.
    assert!(rendered_text(&mut app, 100, 8).contains(summary));

    apply_turn_event(
        &mut app,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "later failure".to_string(),
        },
    );
    assert!(rendered_text(&mut app, 80, 8).contains(summary));
}

#[test]
fn configured_profile_fallback_and_role_slots_are_independent() {
    let mut app = configured_app();
    let build = app.status_for_test();
    assert_eq!(
        build.profile,
        Some(ModelProfileRef::new("provider-build", "build-model"))
    );
    assert!(build.response.is_none());
    assert!(build.context.is_none());

    toggle_mode_with_ack(&mut app);
    let plan = app.status_for_test();
    assert_eq!(plan.primary, "Plan · Normal");
    assert_eq!(
        plan.profile,
        Some(ModelProfileRef::new("provider-plan", "plan-model"))
    );
    assert!(plan.response.is_none());

    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Plan);
    app.reduce_without_effects(SessionEvent::UsageUpdated {
        turn_id: TEST_TURN_ID,
        usage: TokenUsage {
            input_tokens: 4_000,
            cached_tokens: 1_000,
            output_tokens: 500,
            total_tokens: 4_500,
        },
        profile: ModelProfileRef::new("runtime-plan", "plan-v2"),
        model_role: ModelRole::Plan,
        input_token_limit: 90_000,
        context_window_tokens: 120_000,
    });
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
        message: Message::assistant("planned"),
    });
    let authoritative_plan = app.status_for_test();
    assert_eq!(
        authoritative_plan.profile,
        Some(ModelProfileRef::new("runtime-plan", "plan-v2"))
    );
    assert_eq!(
        authoritative_plan
            .response
            .expect("Plan response")
            .total_tokens,
        4_500
    );

    toggle_mode_with_ack(&mut app);
    let untouched_build = app.status_for_test();
    assert_eq!(untouched_build.primary, "Build · Normal");
    assert_eq!(
        untouched_build.profile,
        Some(ModelProfileRef::new("provider-build", "build-model"))
    );
    assert!(untouched_build.response.is_none());

    let build_turn = TurnId::new(2);
    start_empty_turn(&mut app, build_turn, SessionMode::Build);
    app.reduce_without_effects(SessionEvent::UsageUpdated {
        turn_id: build_turn,
        usage: TokenUsage {
            input_tokens: 8_000,
            cached_tokens: 4_000,
            output_tokens: 1_000,
            total_tokens: 9_000,
        },
        profile: ModelProfileRef::new("runtime-build", "build-v2"),
        model_role: ModelRole::Build,
        input_token_limit: 100_000,
        context_window_tokens: 128_000,
    });
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: build_turn,
        message: Message::assistant("built"),
    });
    toggle_mode_with_ack(&mut app);
    let retained_plan = app.status_for_test();
    assert_eq!(
        retained_plan.profile,
        Some(ModelProfileRef::new("runtime-plan", "plan-v2"))
    );
    assert_eq!(
        retained_plan
            .response
            .expect("retained Plan response")
            .total_tokens,
        4_500
    );
}

#[test]
fn authoritative_profile_changes_never_mix_old_context_or_stale_turns() {
    let mut app = configured_app();
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    let first_profile = ModelProfileRef::new("first", "build-a");
    app.reduce_without_effects(SessionEvent::ContextUsageUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: ContextTokenSnapshot {
            profile: first_profile,
            model_role: ModelRole::Build,
            projected_input_tokens: 20_000,
            source: ContextTokenSource::Exact,
            automatic_trigger: 80_000,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        },
    });

    let second_profile = ModelProfileRef::new("second", "build-b");
    app.reduce_without_effects(SessionEvent::UsageUpdated {
        turn_id: TEST_TURN_ID,
        usage: TokenUsage {
            input_tokens: 10_000,
            cached_tokens: 5_000,
            output_tokens: 1_000,
            total_tokens: 11_000,
        },
        profile: second_profile.clone(),
        model_role: ModelRole::Build,
        input_token_limit: 90_000,
        context_window_tokens: 120_000,
    });
    let replaced = app.status_for_test();
    assert_eq!(replaced.profile, Some(second_profile.clone()));
    assert!(replaced.response.is_some());
    assert!(
        replaced.context.is_none(),
        "old-profile context was cleared"
    );

    app.reduce_without_effects(SessionEvent::ContextUsageUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: ContextTokenSnapshot {
            profile: second_profile.clone(),
            model_role: ModelRole::Build,
            projected_input_tokens: 24_000,
            source: ContextTokenSource::UsagePlusDelta,
            automatic_trigger: 72_000,
            input_token_limit: 90_000,
            context_window_tokens: 120_000,
        },
    });
    let coherent = app.status_for_test();
    assert_eq!(
        coherent.context.expect("matching context").profile,
        second_profile
    );
    assert!(coherent.response.is_some());

    app.reduce_without_effects(SessionEvent::UsageUpdated {
        turn_id: TurnId::new(999),
        usage: TokenUsage::default(),
        profile: ModelProfileRef::new("stale", "stale-model"),
        model_role: ModelRole::Build,
        input_token_limit: 1,
        context_window_tokens: 1,
    });
    let after_stale = app.status_for_test();
    assert_eq!(
        after_stale.profile,
        Some(ModelProfileRef::new("second", "build-b"))
    );
    assert_eq!(
        after_stale
            .context
            .expect("context survives stale event")
            .projected_input_tokens,
        24_000
    );
}

#[test]
fn plan_review_and_transcript_edit_ensembles_use_exact_model_roles() {
    for (workflow, role, primary, profile) in [
        (
            EnsembleWorkflow::Plan,
            ModelRole::Plan,
            "Plan · Waiting",
            ModelProfileRef::new("provider-plan", "plan-model"),
        ),
        (
            EnsembleWorkflow::Review,
            ModelRole::Review,
            "Review · Waiting",
            ModelProfileRef::new("provider-review", "review-model"),
        ),
    ] {
        let mut app = configured_app();
        enter_insert(&mut app);
        let input = format!("{} inspect", workflow.slash_command());
        app.set_input_for_test(&input, input.len());
        assert!(matches!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::RunEnsemble {
                workflow: actual,
                ..
            }) if actual == workflow
        ));
        assert_eq!(app.in_flight_role(), Some(role));
        let status = app.status_for_test();
        assert_eq!(status.primary, primary);
        assert_eq!(status.profile, Some(profile));
    }

    let mut edit = configured_app();
    edit.seed_history_entry(history_message(Message::user("ordinary prompt")));
    edit.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut edit), None);
    edit.set_input_for_test(
        "/ensemble-review revised review",
        "/ensemble-review revised review".len(),
    );
    assert!(matches!(
        edit.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            replacement: TranscriptEditReplacement::Ensemble {
                workflow: EnsembleWorkflow::Review,
                ..
            },
            ..
        }))
    ));
    assert_eq!(edit.in_flight_role(), Some(ModelRole::Review));
    let status = edit.status_for_test();
    assert_eq!(status.primary, "Review · Waiting");
    assert_eq!(
        status.profile,
        Some(ModelProfileRef::new("provider-review", "review-model"))
    );

    let mut authoritative = configured_app();
    authoritative.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: editable_ensemble_start(
            "authoritative-review-role",
            EnsembleWorkflow::Review,
            "review",
        ),
        resumed: false,
    });
    assert_eq!(authoritative.in_flight_role(), Some(ModelRole::Review));
    assert_eq!(
        authoritative.status_for_test().profile,
        Some(ModelProfileRef::new("provider-review", "review-model"))
    );
}

#[test]
fn semantic_status_precedence_covers_focus_activity_plan_and_warnings() {
    let mut app = App::new();
    assert_eq!(app.status_for_test().primary, "Build · Normal");
    enter_insert(&mut app);
    assert_eq!(app.status_for_test().primary, "Build · Insert");
    app.handle_event(key(KeyCode::Char('/')));
    assert_eq!(app.status_for_test().primary, "Build · Command");

    let mut selecting = App::new();
    selecting.seed_history_entry(history_message(Message::user("select me")));
    selecting.select_for_test(cursor(0, 0));
    assert_eq!(selecting.status_for_test().primary, "Build · Select");
    assert_eq!(ctrl_e(&mut selecting), None);
    assert_eq!(selecting.status_for_test().primary, "Build · Recall");

    let mut waiting = App::new();
    start_empty_turn(&mut waiting, TEST_TURN_ID, SessionMode::Build);
    assert_eq!(waiting.status_for_test().primary, "Build · Waiting");
    waiting.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("stream")).into(),
    });
    assert_eq!(waiting.status_for_test().primary, "Build · Streaming");
    waiting.reduce_without_effects(SessionEvent::TurnRetrying {
        turn_id: TEST_TURN_ID,
        attempt: 2,
        max_attempts: 4,
        retry_after: std::time::Duration::from_millis(500),
        error: "offline".to_string(),
    });
    assert_eq!(
        waiting.status_for_test().primary,
        "Build · Reconnecting 2/4"
    );

    let mut compacting = App::new();
    compacting.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id: TEST_TURN_ID,
        trigger: CompactionTrigger::Manual,
    });
    assert_eq!(compacting.status_for_test().primary, "Build · Compacting");

    let mut ready = App::new();
    ready.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: test_plan_artifact(),
        },
    });
    assert_eq!(ready.status_for_test().primary, "Plan ready");
    assert!(matches!(
        ready.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan { .. })
    ));
    assert_eq!(ready.status_for_test().primary, "Plan ready");

    let artifact = test_plan_artifact();
    let mut submitted = App::new();
    submitted.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact),
        },
    });
    assert!(submitted.open_plan_recovery_for_test());
    assert_eq!(submitted.status_for_test().primary, "Submitted plan");
    submitted.handle_event(key(KeyCode::Char('1')));
    assert!(matches!(
        submitted.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan { .. })
    ));
    assert_eq!(submitted.status_for_test().primary, "Submitted plan");

    let mut persistence = App::new();
    persistence.reduce_without_effects(SessionEvent::PersistenceChanged {
        path: PathBuf::from("/tmp/session.jsonl"),
        error: Some("disk full".to_string()),
    });
    let warning = persistence.status_for_test();
    assert_eq!(warning.primary, "Persistence degraded");
    assert_eq!(warning.tone, crate::status::StatusTone::Warning);
    assert_eq!(warning.detail.as_deref(), Some("Build · Normal"));

    let artifact = test_plan_artifact();
    let mut fresh = App::new();
    fresh.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Resolved {
            artifact,
            resolution: PlanResolution::ImplementedFresh,
        },
    });
    let warning = fresh.status_for_test();
    assert_eq!(warning.primary, "Fresh Plan handoff pending");
    assert_eq!(warning.tone, crate::status::StatusTone::Warning);
}

#[test]
fn format_token_count_switches_units_at_the_boundaries() {
    assert_eq!(format_token_count(0), "0");
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(1_000), "1.0k");
    assert_eq!(format_token_count(12_345), "12.3k");
    assert_eq!(format_token_count(1_000_000), "1.0M");
    assert_eq!(format_token_count(2_460_000), "2.5M");
}

#[test]
fn input_remains_editable_while_a_turn_streams() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    app.set_input_for_test("queued", "queued".len());

    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);

    assert_eq!(app.input(), "queuedx\n");
    assert_eq!(app.handle_event(ctrl_enter()), None);
}

#[test]
fn updates_stream_then_commit_and_error_clear_busy() {
    let mut app = App::new();

    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::assistant("draft")).into(),
        },
    );
    assert!(app.streaming().is_some());

    apply_turn_event(
        &mut app,
        SessionEvent::StreamCleared {
            turn_id: TEST_TURN_ID,
        },
    );
    assert!(app.streaming().is_none());

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("tool call"),
        },
    );
    assert!(app.streaming().is_none());
    assert!(app.is_busy(), "intermediate messages keep input locked");
    assert!(
        app.history()
            .last()
            .is_some_and(|entry| conversation_has_role(entry, PresentationRole::Assistant))
    );

    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("final answer"),
        },
    );
    assert!(app.streaming().is_none());
    assert!(!app.is_busy());
    assert!(
        app.history()
            .last()
            .is_some_and(|entry| conversation_has_role(entry, PresentationRole::Assistant))
    );

    apply_turn_event(
        &mut app,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "network down".to_string(),
        },
    );
    assert!(!app.is_busy());
    match app.history().last() {
        Some(HistoryEntry::Error(content)) => assert!(content.contains("network down")),
        other => panic!("expected an error entry, got {other:?}"),
    }
}

#[test]
fn rounded_lower_surfaces_keep_controls_local_and_shared_hints_composer_only() {
    let mut commands = App::new();
    enter_insert(&mut commands);
    commands.handle_event(key(KeyCode::Char('/')));
    let rows = rendered_rows(&mut commands, 100, 21);
    let command_title = rows
        .iter()
        .find(|row| row.contains("Commands"))
        .expect("command title");
    assert!(command_title.contains('╭'));
    assert!(rows.iter().any(|row| row.contains("Tab complete")));
    assert!(rows.iter().any(|row| row.contains("Esc cancel")));
    assert!((0..100).all(|x| rows[0].chars().nth(x).unwrap_or(' ') == ' '));
    assert!(rows[18].contains("Ctrl+Enter send"));
    assert!(!rows[18].contains("↑/↓ select"));
    let mut terminal = Terminal::new(TestBackend::new(100, 21)).expect("terminal");
    terminal
        .draw(|frame| commands.render(frame))
        .expect("render command selection");
    let buffer = terminal.backend().buffer();
    let command_title_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("Commands")
        })
        .expect("command title row");
    let command_border_x = (0..buffer.area.width)
        .find(|&x| buffer[(x, command_title_y)].symbol() == "╭")
        .expect("command border");
    assert_eq!(
        buffer[(command_border_x, command_title_y)].fg,
        ZEVRIA_DARK.surfaces.border_strong
    );
    assert_eq!(
        buffer[(command_border_x, command_title_y)].bg,
        ZEVRIA_DARK.surfaces.overlay
    );
    assert!(buffer.content().iter().any(|cell| {
        cell.bg == ZEVRIA_DARK.surfaces.selection_background
            && cell.fg == ZEVRIA_DARK.surfaces.selection_foreground
    }));
    let compact_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("/compact")
        })
        .expect("unselected command row");
    assert_eq!(
        buffer[(command_border_x + 2, compact_y)].bg,
        ZEVRIA_DARK.surfaces.overlay
    );

    let mut skills = App::new().with_skills(vec![SkillMeta {
        name: "commit".parse().unwrap(),
        description: "Commit changes".to_string(),
    }]);
    enter_insert(&mut skills);
    skills.handle_event(key(KeyCode::Char('$')));
    let rows = rendered_rows(&mut skills, 100, 21);
    let skill_title = rows
        .iter()
        .find(|row| row.contains("Skills"))
        .expect("skill title");
    assert!(skill_title.contains('╭'));
    assert!(
        rows.iter()
            .any(|row| row.contains("Enter complete · Tab complete"))
    );

    let mut plan = App::new();
    let artifact = test_plan_artifact();
    let version = artifact.version;
    plan.restore_plan_state(PlanWorkflowState::Ready { artifact });
    let rows = rendered_rows(&mut plan, 100, 20);
    assert!(rows[14].contains('╭'));
    assert!(rows[14].contains(&version.to_string()));
    assert!(rows[18].contains("j next"));
    assert!(rows[18].contains('╰'));
    assert!(!rows[..19].concat().contains("Ctrl+Enter send"));
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
    terminal
        .draw(|frame| plan.render(frame))
        .expect("render Plan selection");
    let buffer = terminal.backend().buffer();
    let selected_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("3. Revise the plan")
        })
        .expect("selected Plan choice");
    assert!((0..buffer.area.width).any(|x| buffer[(x, selected_y)].bg == SELECTION_BG));
    assert!(
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, selected_y)].bg == SELECTION_BG)
            .all(|x| buffer[(x, selected_y)].fg == ZEVRIA_DARK.surfaces.selection_foreground)
    );
    let plan_border_x = (0..buffer.area.width)
        .find(|&x| buffer[(x, 14)].symbol() == "╭")
        .expect("Plan border");
    assert_eq!(
        buffer[(plan_border_x, 14)].fg,
        ZEVRIA_DARK.surfaces.border_strong
    );
    assert_eq!(buffer[(plan_border_x, 14)].bg, ZEVRIA_DARK.surfaces.overlay);
}

#[test]
fn picker_and_question_are_rounded_and_confined_above_footer() {
    let mut baseline = test_session_views(App::new());
    let expected_status = rendered_views_status_cells(&mut baseline, 80, 20);

    let mut picker_views = test_session_views(App::new());
    picker_views.open_session_picker(vec![session_summary(
        "rounded-picker",
        Some("rounded picker row"),
    )]);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| picker_views.render(frame))
        .expect("render picker");
    let buffer = terminal.backend().buffer();
    let picker_title_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("Resume session")
        })
        .expect("picker title");
    let picker_left = (0..buffer.area.width)
        .find(|&x| buffer[(x, picker_title_y)].symbol() == "╭")
        .expect("picker left border");
    assert_eq!(
        buffer[(picker_left, picker_title_y)].fg,
        ZEVRIA_DARK.surfaces.border_strong
    );
    assert_eq!(
        buffer[(picker_left, picker_title_y)].bg,
        ZEVRIA_DARK.surfaces.overlay
    );
    let picker_selected_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("rounded picker row")
        })
        .expect("picker selected row");
    assert!(
        (0..buffer.area.width)
            .filter(|&x| {
                buffer[(x, picker_selected_y)].bg == ZEVRIA_DARK.surfaces.selection_background
            })
            .all(|x| {
                buffer[(x, picker_selected_y)].fg == ZEVRIA_DARK.surfaces.selection_foreground
            })
    );
    assert_eq!(bottom_row_cells(buffer), expected_status);

    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    let mut question_views = test_session_views(root);
    let expected_status = rendered_views_status_cells(&mut question_views, 80, 20);
    question_views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("rounded-question"),
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| question_views.render(frame))
        .expect("render question");
    let buffer = terminal.backend().buffer();
    let (question_left, question_top) = (0..buffer.area.height - 1)
        .find_map(|y| {
            (0..buffer.area.width)
                .find(|&x| buffer[(x, y)].symbol() == "╭")
                .map(|x| (x, y))
        })
        .expect("question border");
    assert_eq!(
        buffer[(question_left, question_top)].fg,
        ZEVRIA_DARK.surfaces.border_strong
    );
    assert_eq!(
        buffer[(question_left, question_top)].bg,
        ZEVRIA_DARK.surfaces.overlay
    );
    let focused_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("Focused")
        })
        .expect("selected question option");
    assert!((0..buffer.area.width).any(|x| buffer[(x, focused_y)].bg == SELECTION_BG));
    assert!(
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, focused_y)].bg == SELECTION_BG)
            .all(|x| buffer[(x, focused_y)].fg == ZEVRIA_DARK.surfaces.selection_foreground)
    );
    assert_eq!(bottom_row_cells(buffer), expected_status);
}

#[test]
fn slash_menu_opens_filters_and_completes() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert!(app.command_menu_active());
    let rendered = rendered_text(&mut app, 80, 20);
    assert!(rendered.contains("/resume"));
    assert!(rendered.contains("List previous sessions and resume one"));
    assert!(
        rendered.contains("Build · Command"),
        "footer state switches"
    );
    assert!(rendered.contains("Enter accept/run"));
    assert!(rendered.contains("Tab complete"));

    // A non-matching filter keeps the menu open with an empty-state row.
    for ch in "zz".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    let rendered = rendered_text(&mut app, 80, 20);
    assert!(rendered.contains("no matching command"));

    // Narrowing back to a real prefix lets Tab accept the full name and one
    // separator into the editor.
    assert_eq!(app.handle_event(key(KeyCode::Backspace)), None);
    assert_eq!(app.handle_event(key(KeyCode::Backspace)), None);
    for ch in "re".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(app.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(app.input(), "/resume ");
    assert!(!app.command_menu_active());
    assert!(rendered_text(&mut app, 80, 20).contains("Ctrl+Enter send"));
}

#[test]
fn command_menu_uses_the_caret_and_preserves_trailing_draft() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("hello world", 0);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(app.input(), "/hello world");
    assert_eq!(app.input_cursor(), 1);
    assert!(app.command_menu_active());
    assert!(rendered_text(&mut app, 80, 20).contains("/resume"));

    app.set_menu_selection_for_test(
        app.command_match_position("compact")
            .expect("compact remains a match before the suffix"),
    );
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "/compact hello world");
    assert_eq!(app.input_cursor(), "/compact ".len());
    assert!(!app.command_menu_active());
    assert!(!app.is_busy());

    let mut multiline = App::new();
    enter_insert(&mut multiline);
    multiline.set_input_for_test("first line\nsecond line", 0);
    assert_eq!(multiline.handle_event(key(KeyCode::Char('/'))), None);
    assert!(multiline.command_menu_active());
    assert_eq!(multiline.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(multiline.input(), "/resume first line\nsecond line");
    assert_eq!(multiline.input_cursor(), "/resume ".len());
}

#[test]
fn completion_acceptance_reuses_whitespace_and_unicode_suffixes() {
    let mut whitespace = App::new();
    enter_insert(&mut whitespace);
    whitespace.set_input_for_test(" hello", 0);
    assert_eq!(whitespace.handle_event(key(KeyCode::Char('/'))), None);
    whitespace.set_menu_selection_for_test(
        whitespace
            .command_match_position("compact")
            .expect("compact remains a match before a whitespace suffix"),
    );
    assert_eq!(whitespace.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(whitespace.input(), "/compact hello");
    assert_eq!(whitespace.input_cursor(), "/compact ".len());

    let mut unicode = App::new();
    enter_insert(&mut unicode);
    unicode.set_input_for_test("e\u{301}cho", 0);
    assert_eq!(unicode.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(unicode.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(unicode.input(), "/resume e\u{301}cho");
    assert_eq!(unicode.input_cursor(), "/resume ".len());
}

#[test]
fn skill_menu_filters_from_the_caret_before_existing_text() {
    let mut app = app_with_skills();
    enter_insert(&mut app);
    app.set_input_for_test("the auth module", 0);
    assert_eq!(app.handle_event(key(KeyCode::Char('$'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('r'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('e'))), None);
    assert_eq!(app.input(), "$rethe auth module");
    assert!(app.command_menu_active());
    assert_eq!(app.command_match_position("review"), Some(0));

    assert_eq!(app.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(app.input(), "$review the auth module");
    assert_eq!(app.input_cursor(), "$review ".len());
}

#[test]
fn command_menu_closes_after_whitespace_before_the_caret() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("/compact hello", "/compact ".len());
    assert!(!app.command_menu_active());
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "/compact \nhello");
}

#[test]
fn escape_cancels_only_the_active_completion_prefix() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("hello world", 0);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert!(app.command_menu_active());

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(app.input(), "hello world");
    assert_eq!(app.input_cursor(), 0);
    assert!(app.interaction().is_insert());
    assert!(!app.command_menu_active());
    assert_eq!(app.selection(), None);
}

#[test]
fn tab_and_ctrl_i_accept_every_palette_row_without_submitting() {
    for acceptance in [key(KeyCode::Tab), ctrl('i')] {
        for spec in COMMANDS {
            let mut app = App::new();
            enter_insert(&mut app);
            for character in format!("/{}", spec.name).chars() {
                assert_eq!(app.handle_event(key(KeyCode::Char(character))), None);
            }
            app.set_menu_selection_for_test(
                app.command_match_position(spec.name)
                    .expect("typed built-in remains a palette row"),
            );

            assert_eq!(app.handle_event(acceptance.clone()), None);
            assert_eq!(app.input(), format!("/{} ", spec.name));
            assert_eq!(app.input_cursor(), app.input().len());
            assert!(app.interaction().is_insert());
            assert_eq!(app.command_menu_selection(), 0);
            assert!(!app.command_menu_active());
            assert!(!app.is_busy());
            assert_eq!(app.in_flight_mode(), None);
            assert!(cursor_visible_after_render(&mut app, 100, 10));
        }

        for skill in ["commit", "review"] {
            let mut app = app_with_skills();
            enter_insert(&mut app);
            for character in format!("${skill}").chars() {
                assert_eq!(app.handle_event(key(KeyCode::Char(character))), None);
            }
            app.set_menu_selection_for_test(usize::MAX);

            assert_eq!(app.handle_event(acceptance.clone()), None);
            assert_eq!(app.input(), format!("${skill} "));
            assert_eq!(app.input_cursor(), app.input().len());
            assert!(app.interaction().is_insert());
            assert_eq!(app.command_menu_selection(), 0);
            assert!(!app.command_menu_active());
            assert!(!app.is_busy());
            assert_eq!(app.in_flight_mode(), None);
        }
    }
}

#[test]
fn model_commands_use_management_not_prompts_and_cannot_execute_from_recall() {
    for (text, command) in [
        ("/model", SlashCommand::Model),
        ("/model-session", SlashCommand::ModelSession),
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        for ch in text.chars() {
            app.handle_event(key(KeyCode::Char(ch)));
        }
        assert_eq!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::RunCommand(command))
        );
        assert!(app.input().is_empty());
        assert!(app.history().is_empty());
        assert!(!app.is_busy(), "only the runtime gate starts management");

        let mut recall = App::new();
        recall.seed_history_entry(history_message(Message::user("original")));
        recall.select_for_test(cursor(0, 0));
        ctrl_e(&mut recall);
        recall.set_input_for_test(text, text.len());
        assert_eq!(recall.handle_event(ctrl_enter()), None);
        assert!(recall.is_recalling());
        assert!(!recall.is_busy());
        assert_eq!(recall.begin_model_management(), None);
        assert_eq!(recall.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(recall.input(), format!("{text} "));
        assert!(recall.is_recalling());
        assert!(!recall.is_busy());

        let mut inspect = App::subtask_inspect("test");
        inspect.set_focus_for_test(crate::app::FocusState::Insert);
        inspect.set_input_for_test(text, text.len());
        assert_eq!(inspect.handle_event(ctrl_enter()), None);
        assert!(!inspect.command_menu_active());
        assert_eq!(inspect.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(inspect.input(), text);
        assert_eq!(inspect.begin_model_management(), None);
    }
}

#[test]
fn model_management_gate_requires_idle_root_without_plan_dialog_or_recall() {
    let mut app = App::new();
    app.set_mode_for_test(SessionMode::Plan);
    assert_eq!(app.begin_model_management(), Some(SessionMode::Plan));
    assert!(app.is_busy());
    assert_eq!(app.begin_model_management(), None);
    app.finish_model_management();
    assert!(!app.is_busy());
    app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build);
    assert_eq!(app.begin_model_management(), None);
    let mut approval = App::new();
    approval.restore_plan_state(PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    });
    assert_eq!(approval.begin_model_management(), None);
    let mut recovery = App::new();
    let artifact = test_plan_artifact();
    recovery.restore_plan_state(PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact),
    });
    assert!(recovery.open_plan_recovery_for_test());
    assert_eq!(recovery.begin_model_management(), None);
}

#[test]
fn completion_enter_executes_parameterless_commands_through_submission() {
    for (text, command) in [
        ("/res", SlashCommand::Resume),
        ("/new", SlashCommand::New),
        ("/ski", SlashCommand::Skills),
        ("/mod", SlashCommand::Model),
        ("/model-s", SlashCommand::ModelSession),
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        for character in text.chars() {
            assert_eq!(app.handle_event(key(KeyCode::Char(character))), None);
        }
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::RunCommand(command))
        );
        assert!(app.input().is_empty());
        assert!(!app.command_menu_active());
        assert!(app.history().is_empty());
        assert!(!app.is_busy());
        assert_eq!(app.in_flight_mode(), None);
        assert_eq!(app.active_turn_id(), None);
    }

    let mut compact = App::new();
    enter_insert(&mut compact);
    for character in "/comp".chars() {
        assert_eq!(compact.handle_event(key(KeyCode::Char(character))), None);
    }
    assert_eq!(
        compact.handle_event(key(KeyCode::Enter)),
        Some(UiAction::Compact {
            mode: SessionMode::Build,
        })
    );
    assert!(compact.input().is_empty());
    assert!(compact.is_busy());

    for command in ["/implement", "/implement-fresh"] {
        let mut app = App::new();
        enter_insert(&mut app);
        for character in command.chars() {
            assert_eq!(app.handle_event(key(KeyCode::Char(character))), None);
        }
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(app.input(), format!("{command} "));
        assert_eq!(app.history().len(), 1);
        assert!(!app.is_busy());
        assert!(rendered_text(&mut app, 100, 12).contains("No submitted plan"));
    }

    let mut ensemble = App::new();
    enter_insert(&mut ensemble);
    for character in "/ensemble-plan".chars() {
        assert_eq!(ensemble.handle_event(key(KeyCode::Char(character))), None);
    }
    assert_eq!(ensemble.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(ensemble.input(), "/ensemble-plan ");
    assert!(!ensemble.is_busy());

    let mut skill = app_with_skills();
    enter_insert(&mut skill);
    for character in "$commit".chars() {
        assert_eq!(skill.handle_event(key(KeyCode::Char(character))), None);
    }
    assert_eq!(skill.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(skill.input(), "$commit ");
    assert!(!skill.is_busy());
}

#[test]
fn completion_enter_executes_the_default_arrow_selected_or_clamped_row() {
    let mut default = App::new();
    enter_insert(&mut default);
    default.handle_event(Event::Paste("/".into()));
    assert_eq!(default.command_menu_selection(), 0);
    assert_eq!(
        default.handle_event(key(KeyCode::Enter)),
        Some(UiAction::RunCommand(SlashCommand::Resume))
    );

    for (name, command) in [
        ("new", SlashCommand::New),
        ("model", SlashCommand::Model),
        ("model-session", SlashCommand::ModelSession),
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        app.handle_event(Event::Paste("/".into()));
        let row = app.command_match_position(name).unwrap();
        for _ in 0..row {
            assert_eq!(app.handle_event(key(KeyCode::Down)), None);
        }
        assert_eq!(app.handle_event(key(KeyCode::Up)), None);
        assert_eq!(app.handle_event(key(KeyCode::Down)), None);
        assert_eq!(app.command_menu_selection(), row);
        assert_eq!(app.input(), "/");
        assert!(app.history().is_empty());
        assert!(!app.is_busy());
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::RunCommand(command))
        );
        assert!(app.input().is_empty());
    }

    let mut clamped = App::new();
    enter_insert(&mut clamped);
    clamped.set_input_for_test("/mod", 4);
    clamped.set_menu_selection_for_test(usize::MAX);
    assert_eq!(
        clamped.handle_event(key(KeyCode::Enter)),
        Some(UiAction::RunCommand(SlashCommand::ModelSession))
    );
}

#[test]
fn completion_enter_keeps_ensembles_and_skills_even_with_prompt_suffixes() {
    for name in ["/ensemble-plan", "/ensemble-review", "$commit", "$review"] {
        for suffix in ["", " prompt", "雪", "\u{2003}雪", "\nfirst\nsecond"] {
            let mut app = app_with_skills();
            enter_insert(&mut app);
            app.set_input_for_test(format!("{name}{suffix}"), name.len());
            assert!(app.command_menu_active());
            assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
            let separator = if suffix.starts_with(char::is_whitespace) {
                ""
            } else {
                " "
            };
            assert_eq!(app.input(), format!("{name}{separator}{suffix}"));
            assert!(!app.command_menu_active());
            assert!(!app.is_busy());
            assert!(
                app.history().is_empty(),
                "completion is not a failed submission"
            );
        }
    }
}

#[test]
fn completion_enter_preserves_invalid_builtin_suffixes_without_submitting() {
    for (prefix, name) in [("/bui", "/build"), ("/ne", "/new")] {
        for suffix in [" argument", "雪", "\u{2003}雪", "\nfirst\nsecond"] {
            let mut app = App::new();
            enter_insert(&mut app);
            app.set_input_for_test(format!("{prefix}{suffix}"), prefix.len());
            assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
            let separator = if suffix.starts_with(char::is_whitespace) {
                ""
            } else {
                " "
            };
            assert_eq!(app.input(), format!("{name}{separator}{suffix}"));
            assert!(!app.is_busy());
            assert!(!app.mode_selection_pending());
            assert!(app.history().is_empty());
        }
    }
    // Blank suffixes are still valid whole-draft builtins.
    let mut blank = App::new();
    enter_insert(&mut blank);
    blank.set_input_for_test("/ne\u{2003}\n ", 3);
    assert_eq!(
        blank.handle_event(key(KeyCode::Enter)),
        Some(UiAction::RunCommand(SlashCommand::New))
    );
}

#[test]
fn completion_enter_ignores_repeat_release_and_modified_events() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("/new", 4);
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            assert_eq!(
                app.handle_event(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    modifiers,
                    kind,
                ))),
                None
            );
            assert_eq!(app.input(), "/new");
        }
    }
    for modifiers in [
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ] {
        assert_eq!(
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Enter, modifiers))),
            None
        );
        assert_eq!(app.input(), "/new");
    }
    assert!(app.history().is_empty());
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::RunCommand(SlashCommand::New))
    );
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "\n");
}

#[test]
fn completion_hints_distinguish_command_activation_from_skill_completion_at_narrow_widths() {
    for width in [40, 60, 100, 180] {
        for skill in [false, true] {
            let mut app = app_with_skills();
            enter_insert(&mut app);
            app.handle_event(Event::Paste(if skill { "$" } else { "/" }.into()));
            let rows = rendered_rows(&mut app, width, 21);
            let popup = rows
                .iter()
                .find(|row| row.contains('╰') && row.contains("Enter"))
                .expect("completion popup footer");
            let footer = &rows[18];
            if skill {
                assert!(popup.contains("Enter complete · Tab complete"), "{popup}");
                assert!(footer.contains("Enter complete"), "{footer}");
                assert!(!popup.contains("run") && !footer.contains("run"));
            } else {
                assert!(popup.contains("Enter accept/run"), "{popup}");
                assert!(popup.contains("Tab complete"), "{popup}");
                assert!(footer.contains("Enter accept/run"), "{footer}");
            }
            assert!(
                footer.contains("Tab complete") || footer.contains("Tab/Ctrl-I complete"),
                "{footer}"
            );
        }
    }
}

#[test]
fn new_command_is_fresh_only_and_takes_no_arguments() {
    let mut invalid = App::new();
    enter_insert(&mut invalid);
    for character in "/new later".chars() {
        assert_eq!(invalid.handle_event(key(KeyCode::Char(character))), None);
    }
    assert_eq!(invalid.handle_event(ctrl_enter()), None);
    assert_eq!(invalid.input(), "/new later");
    assert_eq!(invalid.input_cursor(), "/new later".len());
    assert!(!invalid.is_busy());
    assert_eq!(invalid.in_flight_mode(), None);
    assert_eq!(invalid.history().len(), 1);
    assert!(matches!(
        invalid.history().last(),
        Some(HistoryEntry::Error(error)) if error == "Unknown command: /new later"
    ));

    let mut recall = App::new();
    recall.seed_history_entry(history_message(Message::user("original prompt")));
    recall.seed_history_entry(history_message(Message::assistant("original response")));
    recall.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut recall), None);
    recall.set_input_for_test("/new", "/new".len());

    assert!(recall.command_menu_active());
    assert_eq!(recall.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(recall.input(), "/new ");
    assert!(!recall.command_menu_active());
    assert!(recall.is_recalling());
    assert_eq!(
        recall.recalled_edit_target(),
        Some(&TranscriptEditTarget::PromptOrdinal(0))
    );
    assert!(!recall.is_awaiting_edit_acceptance());
    assert!(!recall.is_busy());
    assert_eq!(recall.in_flight_mode(), None);
    assert_eq!(recall.history().len(), 3);
    assert!(matches!(
        recall.history().last(),
        Some(HistoryEntry::Error(error))
            if error == "Built-in commands cannot replace transcript items; cancel the recall to run the command."
    ));
    let rendered = rendered_text(&mut recall, 100, 16);
    assert!(rendered.contains("Built-in commands cannot replace transcript items"));
    assert!(rendered.contains("original prompt"));
    assert!(rendered.contains("original response"));
}

#[test]
fn recall_palette_completes_with_enter_or_tab_and_escape_restores_saved_composer() {
    for acceptance_key in [KeyCode::Enter, KeyCode::Tab] {
        let mut app = app_with_skills();
        app.seed_history_entry(history_message(Message::user("ordinary prompt")));
        app.set_input_for_test("saved draft", "saved".len());
        app.set_focus_for_test(crate::app::FocusState::Insert);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        app.set_input_for_test("$re", "$re".len());

        assert!(app.command_menu_active());
        assert_eq!(app.handle_event(key(acceptance_key)), None);
        assert_eq!(app.input(), "$review ");
        assert!(app.is_recalling());
        assert!(!app.is_busy());
    }

    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("ordinary prompt")));
    app.set_input_for_test("saved draft", "saved".len());
    app.set_focus_for_test(crate::app::FocusState::Normal);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.set_input_for_test("/res", "/res".len());
    assert!(app.command_menu_active());

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(app.input(), "saved draft");
    assert_eq!(app.input_cursor(), "saved".len());
    assert!(!app.interaction().is_insert());
    assert!(!app.is_recalling());
}

#[test]
fn enter_on_builtin_during_recall_keeps_fresh_only_validation() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("ordinary prompt")));
    app.set_input_for_test("saved draft", "saved draft".len());
    app.set_focus_for_test(crate::app::FocusState::Normal);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.set_input_for_test("/compact", "/compact".len());

    assert!(app.command_menu_active());
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "/compact ");
    assert!(app.is_recalling());
    assert!(
        rendered_text(&mut app, 100, 14)
            .contains("Built-in commands cannot replace transcript items")
    );
    assert!(matches!(
        app.history().first(),
        Some(HistoryEntry::Conversation(_))
    ));
    assert!(matches!(app.history().last(), Some(HistoryEntry::Error(_))));
}

#[test]
fn broad_palette_selection_accepts_ensemble_plan_and_empty_filters_are_inert() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    let ensemble_index = COMMANDS
        .iter()
        .position(|spec| spec.name == "ensemble-plan")
        .expect("ensemble-plan command");
    for _ in 0..ensemble_index {
        assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    }
    assert_eq!(app.command_menu_selection(), ensemble_index);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "/ensemble-plan ");
    assert!(!app.command_menu_active());

    for acceptance_key in [KeyCode::Enter, KeyCode::Tab] {
        let mut no_match = App::new();
        enter_insert(&mut no_match);
        for character in "/does-not-exist".chars() {
            no_match.handle_event(key(KeyCode::Char(character)));
        }
        let draft = no_match.input().to_string();
        let cursor = no_match.input_cursor();
        assert!(no_match.command_menu_active());
        assert_eq!(no_match.handle_event(key(acceptance_key)), None);
        assert_eq!(no_match.input(), draft);
        assert_eq!(no_match.input_cursor(), cursor);
        assert!(!no_match.is_busy());
    }
}

#[test]
fn slash_menu_keeps_up_down_selection_while_left_right_move_the_caret() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    let cursor = app.input_cursor();

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.command_menu_selection(), 1);
    assert_eq!(app.input_cursor(), cursor);
    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.command_menu_selection(), 0);
    assert_eq!(app.input_cursor(), cursor);

    assert_eq!(app.handle_event(key(KeyCode::Left)), None);
    assert_eq!(app.input_cursor(), 0);
    assert_eq!(app.handle_event(key(KeyCode::Right)), None);
    assert_eq!(app.input_cursor(), cursor);
}

#[test]
fn ctrl_enter_submits_actual_commands_without_accepting_highlighted_completions() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "/resume".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::RunCommand(SlashCommand::Resume))
    );
    assert!(app.input().is_empty());

    // Even an alternative matching row must not redirect Ctrl+Enter.
    app.set_input_for_test("/model", "/model".len());
    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.command_menu_selection(), 1);
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::RunCommand(SlashCommand::Model))
    );

    let mut partial = App::new();
    enter_insert(&mut partial);
    for ch in "/res".chars() {
        assert_eq!(partial.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(partial.handle_event(ctrl_enter()), None);
    assert_eq!(partial.input(), "/res");
    assert!(matches!(
        partial.history().last(),
        Some(HistoryEntry::Error(error)) if error == "Unknown command: /res"
    ));

    let mut accepted = App::new();
    enter_insert(&mut accepted);
    for ch in "/res".chars() {
        accepted.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(accepted.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(accepted.input(), "/resume ");
    assert_eq!(
        accepted.handle_event(ctrl_enter()),
        Some(UiAction::RunCommand(SlashCommand::Resume))
    );
}

#[test]
fn accepted_builtins_execute_while_skills_and_ensembles_keep_plain_enter() {
    let mut builtin = App::new();
    enter_insert(&mut builtin);
    for ch in "/compact".chars() {
        builtin.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        builtin.handle_event(ctrl_enter()),
        Some(UiAction::Compact {
            mode: SessionMode::Build
        })
    );
    assert!(builtin.input().is_empty());
    assert!(builtin.is_busy());

    let mut skill = app_with_skills();
    enter_insert(&mut skill);
    for ch in "$commit".chars() {
        skill.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(skill.handle_event(key(KeyCode::Tab)), None);
    for ch in "first".chars() {
        skill.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(skill.handle_event(key(KeyCode::Enter)), None);
    assert!(!skill.is_busy());
    for ch in "second".chars() {
        skill.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        skill.handle_event(ctrl_enter()),
        Some(UiAction::InvokeSkill {
            name: "commit".parse().unwrap(),
            args: "first\nsecond".into(),
            mode: SessionMode::Build,
        })
    );

    let mut ensemble = App::new();
    enter_insert(&mut ensemble);
    for ch in "/ensemble-review".chars() {
        ensemble.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(ensemble.handle_event(key(KeyCode::Enter)), None);
    for ch in "first".chars() {
        ensemble.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(ensemble.handle_event(key(KeyCode::Enter)), None);
    assert!(!ensemble.is_busy(), "the first line was not submitted");
    for ch in "second".chars() {
        ensemble.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        ensemble.handle_event(ctrl_enter()),
        Some(UiAction::RunEnsemble {
            workflow: EnsembleWorkflow::Review,
            prompt: "first\nsecond".into(),
        })
    );
}

#[test]
fn plan_decision_locks_before_ack_rejects_duplicates_and_recovers_from_early_failure() {
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready { artifact },
    });

    app.handle_event(key(KeyCode::Char('1')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementCurrent,
        })
    );
    assert!(app.is_busy());
    assert_eq!(app.active_turn_id(), None);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(rendered_text(&mut app, 120, 10).contains("decision pending"));

    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TurnId::new(404),
        error: "decision rejected before acknowledgement".to_string(),
    });
    assert!(!app.is_busy());
    assert!(app.plan_state().artifact().is_some());
    assert!(rendered_text(&mut app, 120, 12).contains("Plan ready"));
    app.handle_event(key(KeyCode::Char('1')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementCurrent,
        })
    );
}

#[test]
fn plan_fresh_snapshot_settles_pending_but_current_handoff_stays_active() {
    let artifact = test_plan_artifact();
    let expected = artifact.version;

    let mut fresh = App::new();
    fresh.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });
    fresh.handle_event(key(KeyCode::Char('2')));
    assert!(matches!(
        fresh.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            decision: PlanDecision::ImplementFresh,
            ..
        })
    ));
    fresh.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Resolved {
            artifact: artifact.clone(),
            resolution: PlanResolution::ImplementedFresh,
        },
    });
    assert!(!fresh.is_busy());

    let mut current = App::new();
    current.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });
    current.handle_event(key(KeyCode::Char('1')));
    assert!(matches!(
        current.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            decision: PlanDecision::ImplementCurrent,
            ..
        })
    ));
    current.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Resolved {
            artifact: artifact.clone(),
            resolution: PlanResolution::ImplementedCurrent,
        },
    });
    assert!(current.is_busy());
    let turn_id = TurnId::new(405);
    current.reduce_without_effects(SessionEvent::PlanHandoffStarted {
        turn_id,
        handoff: PlanHandoff::new(artifact, "source"),
    });
    assert_eq!(current.active_turn_id(), Some(turn_id));
    current.reduce_without_effects(SessionEvent::TurnRecovered {
        turn_id,
        display_attempt_id: None,
    });
    assert!(!current.is_busy());
    assert_eq!(expected, current.plan_state().version().unwrap());
}

#[test]
fn plan_implementation_commands_target_the_retained_submitted_version() {
    let artifact = test_plan_artifact();
    let expected = artifact.version;

    for (command, decision) in [
        ("/implement", PlanDecision::ImplementCurrent),
        ("/implement-fresh", PlanDecision::ImplementFresh),
    ] {
        for activation in [key(KeyCode::Enter), ctrl_enter()] {
            let mut app = App::new();
            app.reduce_without_effects(SessionEvent::ModeChanged {
                mode: SessionMode::Plan,
            });
            app.reduce_without_effects(SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Planning {
                    id: expected.id,
                    previous: Some(artifact.clone()),
                },
            });
            assert!(rendered_text(&mut app, 120, 20).contains("p plan choices"));
            enter_insert(&mut app);
            assert!(rendered_text(&mut app, 120, 20).contains("Ctrl+Enter send"));
            for ch in command.chars() {
                assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
            }
            assert_eq!(
                app.handle_event(activation),
                Some(UiAction::ResolvePlan { expected, decision })
            );
            assert!(app.input().is_empty());
            assert!(
                app.is_busy(),
                "Plan decisions lock before engine acknowledgement"
            );
            app.set_input_for_test(command, command.len());
            assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
            assert_eq!(app.input(), format!("{command} "));
            assert_eq!(app.handle_event(ctrl_enter()), None);
            assert_eq!(app.plan_state().version(), Some(expected));
        }
    }
}

#[test]
fn plan_implementation_commands_fail_locally_without_a_submitted_artifact() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "/implement".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }

    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert_eq!(app.input(), "/implement");
    assert!(!app.is_busy());
    assert!(
        rendered_text(&mut app, 100, 12).contains("No submitted plan is available to implement.")
    );
}

#[test]
fn unknown_fresh_sigils_show_local_errors_without_clearing_the_draft() {
    let mut command = App::new();
    enter_insert(&mut command);
    for ch in "/bogus".chars() {
        assert_eq!(command.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(command.handle_event(ctrl_enter()), None);
    assert_eq!(command.input(), "/bogus");
    assert!(!command.is_busy(), "nothing was submitted to the engine");
    let rendered = rendered_text(&mut command, 80, 20);
    assert!(rendered.contains("Unknown command: /bogus"));

    let mut skill = App::new();
    enter_insert(&mut skill);
    for ch in "$bogus".chars() {
        assert_eq!(skill.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(skill.handle_event(ctrl_enter()), None);
    assert_eq!(skill.input(), "$bogus");
    assert!(!skill.is_busy(), "nothing was submitted to the engine");
    let rendered = rendered_text(&mut skill, 80, 20);
    assert!(rendered.contains("Unknown skill: $bogus"));
}

#[test]
fn skills_complete_in_their_own_dollar_menu() {
    let mut app = app_with_skills();
    enter_insert(&mut app);
    // The `/` menu serves only built-ins; skills live behind `$`.
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    let rendered = rendered_text(&mut app, 80, 20);
    assert!(rendered.contains("/resume"));
    assert!(!rendered.contains("commit"));
    assert_eq!(app.handle_event(key(KeyCode::Backspace)), None);

    assert_eq!(app.handle_event(key(KeyCode::Char('$'))), None);
    assert!(app.command_menu_active());
    let rendered = rendered_text(&mut app, 80, 20);
    assert!(rendered.contains("$commit"));
    assert!(rendered.contains("Commit changes"));
    assert!(rendered.contains("$review"));
    assert!(!rendered.contains("/resume"));

    // Tab accepts the highlighted skill with its `$` sigil and separator.
    for ch in "rev".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(app.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(app.input(), "$review ");
    assert!(!app.command_menu_active());
}

#[test]
fn large_skill_menu_reveals_the_last_bounded_match_in_a_short_popup() {
    let skills = (0..20)
        .map(|index| skill_meta(&format!("skill-{index:02}"), "Catalog entry"))
        .collect();
    let mut app = App::new().with_skills(skills);
    enter_insert(&mut app);
    app.handle_event(key(KeyCode::Char('$')));
    for _ in 0..25 {
        app.handle_event(key(KeyCode::Down));
    }
    assert_eq!(
        app.command_menu_selection(),
        19,
        "menu navigation is bounded"
    );

    let rendered = rendered_text(&mut app, 52, 9);
    assert!(rendered.contains("$skill-19"));
    assert!(
        rendered.contains('█'),
        "overflowing completion menu scrolls"
    );

    for character in "skill-0".chars() {
        app.handle_event(key(KeyCode::Char(character)));
    }
    let filtered = rendered_text(&mut app, 52, 9);
    assert_eq!(app.command_menu_selection(), 0);
    assert!(filtered.contains("$skill-00"));

    let mut fitting = App::new().with_skills(vec![
        skill_meta("alpha", "First"),
        skill_meta("beta", "Second"),
    ]);
    enter_insert(&mut fitting);
    fitting.handle_event(key(KeyCode::Char('$')));
    assert!(!rendered_text(&mut fitting, 80, 20).contains('█'));
}

#[test]
fn typed_skill_invocation_returns_to_normal_and_carries_the_mode() {
    let mut app = app_with_skills();
    enter_insert(&mut app);
    for ch in "$commit ship it".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    let action = app.handle_event(ctrl_enter());

    assert_eq!(
        action,
        Some(UiAction::InvokeSkill {
            name: "commit".parse().unwrap(),
            args: "ship it".into(),
            mode: SessionMode::Build,
        })
    );
    assert!(app.is_busy(), "a skill turn blocks duplicate work");
    assert!(app.interaction().is_normal());
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert!(!cursor_visible_after_render(&mut app, 80, 20));
    assert!(app.input().is_empty());
    assert!(
        app.history().is_empty(),
        "TurnStarted owns the user message"
    );

    // The engine echoes the compact `$name` form back for display.
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("$commit ship it"),
            mode: SessionMode::Build,
        },
    );
    let rendered = rendered_text(&mut app, 80, 20);
    assert!(rendered.contains("$commit ship it"));
}

#[test]
fn plain_enter_allows_multiline_skill_arguments_before_ctrl_enter_runs_them() {
    let mut app = app_with_skills();
    enter_insert(&mut app);
    for ch in "$commit first".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    for ch in "second".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::InvokeSkill {
            name: "commit".parse().unwrap(),
            args: "first\nsecond".into(),
            mode: SessionMode::Build,
        })
    );
}

#[test]
fn plan_mode_skill_invocations_snapshot_the_selected_mode() {
    let mut app = app_with_skills();
    toggle_mode_with_ack(&mut app);
    assert_eq!(app.next_mode(), SessionMode::Plan);
    enter_insert(&mut app);
    for ch in "$review".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::InvokeSkill {
            name: "review".parse().unwrap(),
            args: zevria_content::UserPrompt::default(),
            mode: SessionMode::Plan,
        })
    );
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Plan));
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert!(app.interaction().is_normal());
}

#[test]
fn inspect_only_panes_never_invoke_skills() {
    let mut app = App::subtask_inspect("test").with_skills(vec![SkillMeta {
        name: "commit".parse().unwrap(),
        description: "Commit changes".to_string(),
    }]);
    app.set_focus_for_test(crate::app::FocusState::Insert);
    app.set_input_for_test("$commit ship it", "$commit ship it".len());
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert!(!app.is_busy());
}

#[test]
fn recalled_skill_resubmitted_unchanged_remains_a_typed_skill_edit() {
    let mut app = app_with_skills();
    app.restore(vec![
        TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship it",
            SkillApplication::Activate(
                SkillSnapshot::new(
                    "commit".parse().unwrap(),
                    "Commit changes",
                    "Commit instructions",
                )
                .unwrap(),
            ),
        )),
        TranscriptItem::Message(Message::assistant("committed")),
    ]);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    assert_eq!(app.input(), "$commit ship it");

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            target: TranscriptEditTarget::PromptOrdinal(0),
            replacement: TranscriptEditReplacement::Skill {
                name: "commit".parse().unwrap(),
                args: "ship it".into(),
                mode: SessionMode::Build,
            },
        }))
    );
    assert!(app.interaction().is_normal());
    assert!(app.is_awaiting_edit_acceptance());
    assert!(app.input().is_empty());
}

#[test]
fn restored_skill_activations_are_invisible_and_invocations_show_the_dollar_form_only() {
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship it",
            SkillApplication::Activate(
                SkillSnapshot::new(
                    "commit".parse().unwrap(),
                    "Commit changes",
                    "Commit instructions",
                )
                .unwrap(),
            ),
        )),
        TranscriptItem::Message(Message::assistant("committed")),
    ]);

    let rendered = rendered_text(&mut app, 100, 20);
    assert_eq!(rendered.matches("$commit ship it").count(), 1);
    assert!(
        !rendered.contains("Commit instructions"),
        "activation metadata and engine-owned bodies stay out of rows"
    );
}

#[test]
fn slash_menu_navigation_neither_scrolls_nor_leaves_insert_mode() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert!(
        app.view_follow(),
        "menu navigation must not unpin the conversation"
    );
    assert!(app.interaction().is_insert());
    assert!(app.command_menu_active());
}

#[test]
fn escape_dismisses_the_slash_menu_without_arming_select_mode() {
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(Message::user("earlier"))]);
    enter_insert(&mut app);
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    let now = Instant::now();

    assert_eq!(app.handle_event_at(key(KeyCode::Esc), now), None);
    assert_eq!(app.input(), "");
    assert!(app.interaction().is_insert(), "typing continues");
    assert!(!app.command_menu_active());

    // The dismissal did not count toward double-Esc: the next press is the
    // first of a fresh pair, and only the one after that selects.
    rendered_text(&mut app, 80, 12);
    assert_eq!(
        app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(100)),
        None
    );
    assert_eq!(app.selection(), None);
    assert_eq!(
        app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(200)),
        None
    );
    assert_eq!(app.selection(), cursor(0, 0));
}

#[test]
fn command_menu_stays_inactive_in_normal_mode_and_busy_completion_cannot_submit() {
    let mut app = App::new();
    // Normal mode: neither sigil is typed into the input box at all.
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('$'))), None);
    assert_eq!(app.input(), "");
    assert!(!app.command_menu_active());

    // Busy drafting preserves completion but not work admission.
    enter_insert(&mut app);
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    assert_eq!(app.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('$'))), None);
    assert_eq!(app.input(), "/$");
    assert!(app.command_menu_active());
    assert_eq!(app.handle_event(ctrl_enter()), None);
    for (prefix, full) in [
        ("/ne", "/new "),
        ("/mod", "/model "),
        ("/imp", "/implement "),
        ("/bui", "/build "),
    ] {
        app.set_input_for_test(prefix, prefix.len());
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(app.input(), full);
        assert!(!app.mode_selection_pending());
        assert!(app.history().is_empty());
    }
    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TurnId::new(405),
        error: "submission rejected before acknowledgement".into(),
    });
    assert!(!app.is_busy());
    assert_eq!(app.input(), "/build ");
    app.handle_event(key(KeyCode::Left));
    assert!(app.command_menu_active());
    assert!(matches!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::SetMode {
            mode: SessionMode::Build,
            ..
        })
    ));
}

#[test]
fn session_picker_renders_navigates_and_resumes_the_chosen_session() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(vec![
        session_summary("newest-session", Some("fix the login bug")),
        session_summary("older-session", None),
    ]);

    let rendered = rendered_views_text(&mut views, 90, 24);
    assert!(rendered.contains("Resume session"));
    assert!(rendered.contains("fix the login bug"));
    assert!(rendered.contains("(no messages)"));
    assert!(rendered.contains("newest-s"), "short id column shows");
    assert!(rendered.contains("just now"));

    // The modal captures pane keys: 'i' must not enter insert mode behind it.
    assert_eq!(views.handle_event(key(KeyCode::Char('i'))), None);
    assert!(!views.root().interaction().is_insert());

    // j moves to the older session; its row keeps the migrated highlight.
    assert_eq!(views.handle_event(key(KeyCode::Char('j'))), None);
    {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("terminal");
        terminal
            .draw(|frame| views.render(frame))
            .expect("render selected picker row");
        let buffer = terminal.backend().buffer();
        let row_has_selection = |needle: &str| {
            (0..buffer.area.height).any(|y| {
                let row = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                row.contains(needle)
                    && (0..buffer.area.width).any(|x| buffer[(x, y)].bg == SELECTION_BG)
            })
        };
        assert!(row_has_selection("(no messages)"));
        assert!(!row_has_selection("fix the login bug"));
    }

    // Enter resumes the highlighted session and closes the modal.
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResumeSession {
            path: PathBuf::from("/tmp/sessions/older-session.jsonl")
        })
    );
    let rendered = rendered_views_text(&mut views, 90, 24);
    assert!(!rendered.contains("Resume session"));
}

#[test]
fn long_session_picker_reaches_and_renders_the_final_bounded_row() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(
        (0..20)
            .map(|index| session_summary(&format!("id-{index:02}-final"), Some("preview")))
            .collect(),
    );
    for _ in 0..30 {
        assert_eq!(views.handle_event(key(KeyCode::Char('j'))), None);
    }
    let rendered = rendered_views_text(&mut views, 70, 10);
    assert!(rendered.contains("id-19-fi"));
    assert!(rendered.contains('█'), "overflowing picker has a scrollbar");
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResumeSession {
            path: PathBuf::from("/tmp/sessions/id-19-final.jsonl")
        })
    );
}

#[test]
fn empty_session_picker_is_inert_on_enter_and_escape_restores_the_pane() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(Vec::new());
    let rendered = rendered_views_text(&mut views, 90, 24);
    assert!(rendered.contains("No previous sessions"));
    assert!(!rendered.contains('█'));

    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    let rendered = rendered_views_text(&mut views, 90, 24);
    assert!(
        rendered.contains("No previous sessions"),
        "Enter stays open"
    );

    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Char('i'))), None);
    assert!(
        views.root().interaction().is_insert(),
        "keys reach the pane again after the modal closes"
    );
}

#[test]
fn temporary_input_owners_close_groups_and_preserve_composer_history() {
    for owner in ["pane", "picker", "question"] {
        let mut root = App::new();
        if owner == "question" {
            start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
        }
        enter_insert(&mut root);
        for character in "draft".chars() {
            root.handle_event(key(KeyCode::Char(character)));
        }
        let mut views = test_session_views(root);
        match owner {
            "pane" => {
                views.restore_child(
                    SubtaskId::new("undo-child"),
                    Some(child_launch("undo-child", "inspection")),
                    vec![],
                );
                views.handle_event(ctrl('i'));
            }
            "picker" => views.open_session_picker(vec![]),
            "question" => views.apply(SessionEvent::QuestionAsked {
                turn_id: TEST_TURN_ID,
                request: question_request("undo-question"),
            }),
            _ => unreachable!(),
        }
        for character in ['z', 'y'] {
            views.handle_event(ctrl(character));
            assert_eq!(views.root().input(), "draft", "owner {owner}");
        }
        views.handle_event(if owner == "pane" {
            ctrl('o')
        } else {
            key(KeyCode::Esc)
        });
        views.handle_event(key(KeyCode::Char('!')));
        assert_eq!(views.root().input(), "draft!");
        views.handle_event(ctrl('z'));
        assert_eq!(
            views.root().input(),
            "draft",
            "owner {owner} must end the typing group"
        );
        views.handle_event(ctrl('z'));
        assert_eq!(views.root().input(), "");
        views.handle_event(ctrl('y'));
        assert_eq!(views.root().input(), "draft");
    }
}

#[test]
fn capturing_dialogs_keep_v_from_the_normal_transcript() {
    for question in [false, true] {
        let mut root = App::new();
        root.seed_history_entry(history_message(Message::user("selectable history")));
        root.set_input_for_test("hidden draft", 3);
        if question {
            start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
        }
        let mut views = test_session_views(root);
        if question {
            views.apply(SessionEvent::QuestionAsked {
                turn_id: TEST_TURN_ID,
                request: question_request("selection-question"),
            });
        } else {
            views.open_session_picker(vec![session_summary("some-session", None)]);
        }
        let title = if question {
            "Scope · 1/2"
        } else {
            "Resume session"
        };
        assert!(rendered_views_text(&mut views, 100, 28).contains(title));
        assert!(views.root().rendered_selection_window().is_some());
        let before = (views.root().view_scroll(), views.root().view_follow());
        assert_eq!(views.handle_event(key(KeyCode::Char('v'))), None);
        assert!(views.root().interaction().is_normal());
        assert_eq!(views.root().selection(), None);
        assert_eq!(views.root().input(), "hidden draft");
        assert_eq!(views.root().input_cursor(), 3);
        assert_eq!(
            (views.root().view_scroll(), views.root().view_follow()),
            before
        );
        assert!(rendered_views_text(&mut views, 100, 28).contains(title));
        views.handle_event(key(KeyCode::Esc));
        rendered_views_text(&mut views, 100, 28);
        assert_eq!(views.handle_event(key(KeyCode::Char('v'))), None);
        assert_eq!(views.root().selection(), cursor(0, 0));
        assert_eq!(
            views.root().selection_scope(),
            Some(SelectionScope::Message)
        );
    }
}

#[test]
fn picker_and_question_modals_consume_paste_before_the_hidden_composer() {
    let mut picker_root = App::new();
    picker_root.set_focus_for_test(crate::app::FocusState::Insert);
    picker_root.set_input_for_test("picker draft", "picker draft".len());
    let mut picker_views = test_session_views(picker_root);
    picker_views.open_session_picker(vec![session_summary("some-session", None)]);

    assert_eq!(
        picker_views.handle_event(Event::Paste("\nleaked".to_string())),
        None
    );
    assert_eq!(picker_views.root().input(), "picker draft");
    assert!(rendered_views_text(&mut picker_views, 90, 24).contains("Resume session"));

    let mut question_root = App::new();
    start_empty_turn(&mut question_root, TEST_TURN_ID, SessionMode::Build);
    question_root.set_focus_for_test(crate::app::FocusState::Insert);
    question_root.set_input_for_test("question draft", "question draft".len());
    let mut question_views = test_session_views(question_root);
    question_views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("paste-question"),
    });

    assert_eq!(
        question_views.handle_event(Event::Paste("\nleaked".to_string())),
        None
    );
    assert_eq!(question_views.root().input(), "question draft");
    assert!(rendered_views_text(&mut question_views, 100, 28).contains("Scope · 1/2"));
}

#[test]
fn ctrl_c_dismisses_the_picker_without_quitting() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(vec![session_summary("some-session", None)]);
    assert_eq!(views.handle_event(ctrl('c')), None);
    assert!(!rendered_views_text(&mut views, 100, 20).contains("Resume session"));
}

#[test]
fn ctrl_c_targets_the_active_turn_instead_of_quitting_while_busy() {
    let mut root = App::new();
    start_empty_turn(&mut root, TurnId::new(42), SessionMode::Build);
    let mut views = test_session_views(root);

    assert_eq!(
        views.handle_event(ctrl('c')),
        Some(UiAction::CancelTurn {
            turn_id: Some(TurnId::new(42)),
        })
    );
}

#[test]
fn ctrl_c_clears_a_busy_draft_before_cancelling_the_turn() {
    let mut root = App::new();
    start_empty_turn(&mut root, TurnId::new(42), SessionMode::Build);
    enter_insert(&mut root);
    root.set_input_for_test("draft", "draft".len());
    let mut views = test_session_views(root);

    assert_eq!(views.handle_event(ctrl('c')), None);
    assert!(views.root().input().is_empty());
    assert_eq!(
        views.handle_event(ctrl('c')),
        Some(UiAction::CancelTurn {
            turn_id: Some(TurnId::new(42)),
        })
    );
}

#[test]
fn question_modal_cursor_edits_are_correlated_and_isolated_from_main_composer() {
    // Native choices, native no-option questions, and ACP Text/multi forms all
    // enter the same SessionViews modal path.
    for (kind, source_label) in [
        (QuestionPromptKind::SingleSelect { allow_other: true }, None),
        (
            QuestionPromptKind::Text {
                min_length: Some(1),
                max_length: None,
            },
            None,
        ),
        (
            QuestionPromptKind::Text {
                min_length: Some(1),
                max_length: Some(20),
            },
            Some("ACP worker"),
        ),
        (
            QuestionPromptKind::MultiSelect {
                min_selections: Some(1),
                max_selections: Some(1),
                allow_other: true,
            },
            Some("ACP worker"),
        ),
    ] {
        let is_text = matches!(kind, QuestionPromptKind::Text { .. });
        let multi = matches!(kind, QuestionPromptKind::MultiSelect { .. });
        let mut root = App::new();
        start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
        root.set_focus_for_test(crate::app::FocusState::Insert);
        root.set_input_for_test("hidden draft", 6);
        let mut views = test_session_views(root);
        let mut request = question_request("cursor-question");
        request.questions.truncate(1);
        request.questions[0].kind = kind;
        if is_text {
            request.questions[0].options.clear();
        }
        request.source_label = source_label.map(str::to_string);
        views.apply(SessionEvent::QuestionAsked {
            turn_id: TEST_TURN_ID,
            request,
        });
        if !is_text {
            assert_eq!(views.handle_event(key(KeyCode::End)), None);
            assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
        }
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('c'),
            KeyCode::Left,
            KeyCode::Char('b'),
        ] {
            assert_eq!(views.handle_event(key(code)), None);
        }
        assert!(rendered_views_text(&mut views, 100, 28).contains("ab▌c"));
        for event in [
            modified_key(KeyCode::Left, KeyModifiers::CONTROL),
            key(KeyCode::Right),
            key(KeyCode::Delete),
            modified_key(KeyCode::Char('b'), KeyModifiers::SHIFT),
            key(KeyCode::End),
            key(KeyCode::Backspace),
            key(KeyCode::Char('c')),
            key(KeyCode::Home),
            modified_key(KeyCode::Right, KeyModifiers::CONTROL),
        ] {
            assert_eq!(views.handle_event(event), None);
            assert_eq!(views.root().input(), "hidden draft");
            assert_eq!(views.root().input_cursor(), 6);
        }
        assert!(rendered_views_text(&mut views, 100, 28).contains("abc▌"));
        if multi {
            assert_eq!(
                views.handle_event(key(KeyCode::Enter)),
                None,
                "multi-select Other first returns to choices, without sending an answer"
            );
        }
        assert_eq!(
            views.handle_event(key(KeyCode::Enter)),
            Some(UiAction::AnswerQuestion {
                request_id: QuestionRequestId::new("cursor-question"),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope".into(),
                        answer: Some(if multi {
                            QuestionAnswerValue::Strings(vec!["abc".into()])
                        } else {
                            QuestionAnswerValue::String("abc".into())
                        }),
                    }],
                },
            })
        );
        assert_eq!(views.root().input(), "hidden draft");
        assert_eq!(views.root().input_cursor(), 6);
        assert!(
            !rendered_views_text(&mut views, 100, 28).contains("How broad should this change be?")
        );
    }
}

#[test]
fn question_modal_is_global_collects_the_batch_and_dismisses_with_escape() {
    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    let mut views = test_session_views(root);
    views.restore_child(
        SubtaskId::new("visible-child"),
        Some(child_launch("visible-child", "live inspection")),
        vec![TranscriptItem::Message(Message::user("child context"))],
    );
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(
        views.visible_child_id(),
        Some(&SubtaskId::new("visible-child"))
    );
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("question-1"),
    });

    let rendered = rendered_views_text(&mut views, 100, 28);
    for expected in [
        "Scope · 1/2",
        "How broad should this change be?",
        "Focused",
        "Change only the requested workflow.",
        "Other",
    ] {
        assert!(rendered.contains(expected), "modal missing {expected:?}");
    }
    assert_eq!(
        views.visible_child_id(),
        Some(&SubtaskId::new("visible-child")),
        "the global modal does not disturb pane navigation state"
    );

    // Default the first answer, choose the second answer on the next prompt,
    // and submit one correlated response for the complete batch.
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.handle_event(key(KeyCode::Down)), None);
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::AnswerQuestion {
            request_id: QuestionRequestId::new("question-1"),
            response: QuestionResponse::Answered {
                answers: vec![
                    QuestionAnswer {
                        id: "scope".to_string(),
                        answer: Some(QuestionAnswerValue::String("Focused".to_string())),
                    },
                    QuestionAnswer {
                        id: "tests".to_string(),
                        answer: Some(QuestionAnswerValue::String("Full".to_string())),
                    },
                ],
            },
        })
    );
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Which validation should run?"));

    views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("question-2"),
    });
    assert_eq!(
        views.handle_event(key(KeyCode::Esc)),
        Some(UiAction::AnswerQuestion {
            request_id: QuestionRequestId::new("question-2"),
            response: QuestionResponse::Dismissed,
        })
    );
}

#[test]
fn question_modal_codex_three_questions_use_inline_other_without_note_steps() {
    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    root.set_input_for_test("hidden draft", 6);
    let mut views = test_session_views(root);
    // Host-normalized Codex ACP 1.13.1 shape: six wire properties have already
    // become three required selects. The TUI never sees note IDs or wire tokens.
    let request = QuestionRequest {
        id: QuestionRequestId::new("codex-three"),
        questions: (0..3)
            .map(|index| QuestionPrompt {
                id: format!("question_{index}"),
                header: format!("Scope {index}"),
                question: format!("How broad should change {index} be?"),
                options: vec![
                    QuestionOption {
                        label: "Focused".into(),
                        description: "Keep it narrow.".into(),
                    },
                    QuestionOption {
                        label: "Broad".into(),
                        description: String::new(),
                    },
                ],
                kind: QuestionPromptKind::SingleSelect { allow_other: true },
                required: true,
                default: None,
            })
            .collect(),
        source_label: Some("Codex ACP".into()),
        dismissible: true,
    };
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request,
    });

    for index in 0..3 {
        let rendered = rendered_views_text(&mut views, 100, 28);
        assert!(
            rendered.contains(&format!("Codex ACP · Scope {index} · {}/3", index + 1)),
            "{rendered}"
        );
        assert!(rendered.contains("Other"));
        for absent in [
            "Additional answer or note",
            "None of the above",
            "Skip",
            "/6",
            "4/3",
        ] {
            assert!(
                !rendered.contains(absent),
                "unexpected {absent:?}: {rendered}"
            );
        }
        match index {
            0 => assert_eq!(views.handle_event(key(KeyCode::Enter)), None),
            1 => {
                assert_eq!(views.handle_event(key(KeyCode::End)), None);
                assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
                assert_eq!(
                    views.handle_event(Event::Paste("Custom scope".into())),
                    None
                );
                let editing = rendered_views_text(&mut views, 100, 28);
                assert!(editing.contains("Scope 1 · 2/3"));
                assert!(editing.contains("Custom scope▌"), "{editing}");
                assert!(!editing.contains("Additional answer or note"));
                assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
            }
            2 => {
                assert_eq!(views.handle_event(key(KeyCode::Down)), None);
                assert_eq!(
                    views.handle_event(key(KeyCode::Enter)),
                    Some(UiAction::AnswerQuestion {
                        request_id: QuestionRequestId::new("codex-three"),
                        response: QuestionResponse::Answered {
                            answers: ["Focused", "Custom scope", "Broad"]
                                .into_iter()
                                .enumerate()
                                .map(|(index, answer)| QuestionAnswer {
                                    id: format!("question_{index}"),
                                    answer: Some(QuestionAnswerValue::String(answer.into())),
                                })
                                .collect(),
                        },
                    })
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(views.root().input(), "hidden draft");
        assert_eq!(views.root().input_cursor(), 6);
    }
    let completed = rendered_views_text(&mut views, 100, 28);
    for absent in [
        "Codex ACP",
        "How broad should",
        "Additional answer or note",
        "4/3",
    ] {
        assert!(
            !completed.contains(absent),
            "unexpected fourth step: {completed}"
        );
    }
}

#[test]
fn question_modal_preserves_ctrl_c_and_clears_on_terminal_events() {
    let mut root = App::new();
    start_empty_turn(&mut root, TurnId::new(42), SessionMode::Build);
    let mut views = test_session_views(root);
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TurnId::new(42),
        request: question_request("question-42"),
    });

    assert_eq!(
        views.handle_event(ctrl('c')),
        Some(UiAction::AnswerQuestion {
            request_id: zevria_foundation::QuestionRequestId::new("question-42"),
            response: QuestionResponse::Dismissed,
        })
    );
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
    assert!(views.root().is_busy());
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TurnId::new(42),
        request: question_request("question-43"),
    });

    views.apply(SessionEvent::TurnCancelled {
        turn_id: TurnId::new(42),
    });
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
}

#[test]
fn question_close_matches_both_turn_and_request_before_clearing_modal() {
    let mut root = App::new();
    start_empty_turn(&mut root, TurnId::new(7), SessionMode::Build);
    let mut views = test_session_views(root);
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TurnId::new(7),
        request: question_request("older"),
    });
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TurnId::new(7),
        request: question_request("newer"),
    });
    views.apply(SessionEvent::QuestionClosed {
        turn_id: TurnId::new(7),
        request_id: QuestionRequestId::new("older"),
    });
    assert!(rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));

    views.apply(SessionEvent::QuestionClosed {
        turn_id: TurnId::new(8),
        request_id: QuestionRequestId::new("newer"),
    });
    assert!(rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));

    views.apply(SessionEvent::QuestionClosed {
        turn_id: TurnId::new(7),
        request_id: QuestionRequestId::new("newer"),
    });
    assert!(!rendered_views_text(&mut views, 100, 28).contains("Scope · 1/2"));
}

#[test]
fn tab_accepts_a_slash_command_instead_of_opening_a_child_pane() {
    let mut views = test_session_views(App::new());
    views.restore_child(
        SubtaskId::new("child-9"),
        None,
        vec![TranscriptItem::Message(Message::user("child task"))],
    );

    assert_eq!(views.handle_event(key(KeyCode::Char('i'))), None);
    assert_eq!(views.handle_event(key(KeyCode::Char('/'))), None);
    assert_eq!(views.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(
        views.visible_child_id(),
        None,
        "Tab accepted, not navigated"
    );
    assert_eq!(views.root().input(), "/resume ");
    assert!(views.root().interaction().is_insert());
    assert!(!views.root().command_menu_active());

    // The accepted separator dismisses the palette, so a later Tab retains
    // the existing child-navigation binding.
    assert_eq!(views.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(views.visible_child_id(), Some(&SubtaskId::new("child-9")));
}

#[test]
fn compact_command_carries_the_current_mode() {
    let mut app = App::new();
    app.set_mode_for_test(SessionMode::Plan);
    enter_insert(&mut app);
    app.set_input_for_test("/compact", "/compact".len());

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Compact {
            mode: SessionMode::Plan
        })
    );
}

#[test]
fn ensemble_commands_are_typed_and_reject_empty_prompts_locally() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test(
        "/ensemble-review find races",
        "/ensemble-review find races".len(),
    );
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::RunEnsemble {
            workflow: EnsembleWorkflow::Review,
            prompt: "find races".into(),
        })
    );
    assert!(app.is_busy());
    assert_eq!(app.active_turn_id(), None);
    app.reduce_without_effects(SessionEvent::TurnFailed {
        turn_id: TurnId::new(99),
        error: "ensemble configuration is invalid".to_string(),
    });
    assert!(!app.is_busy());
    assert_eq!(app.in_flight_mode(), None);
    assert!(matches!(
        app.history().last(),
        Some(HistoryEntry::Error(error)) if error.contains("configuration is invalid")
    ));

    for (name, workflow, in_flight_mode) in [
        ("ensemble-plan", EnsembleWorkflow::Plan, SessionMode::Plan),
        (
            "ensemble-review",
            EnsembleWorkflow::Review,
            SessionMode::Build,
        ),
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        for ch in format!("/{name}").chars() {
            app.handle_event(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
        let draft = format!("/{name} ");
        assert_eq!(app.input(), draft);
        let cursor = app.input_cursor();

        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert_eq!(app.input(), draft, "validation retains the full draft");
        assert_eq!(app.input_cursor(), cursor);
        assert!(app.interaction().is_insert());
        assert!(cursor_visible_after_render(&mut app, 100, 12));
        assert!(!app.is_busy());
        assert_eq!(app.in_flight_mode(), None);
        assert!(matches!(
            app.history().last(),
            Some(HistoryEntry::Error(error))
                if error == &format!("/{name} requires a prompt.")
        ));

        assert_eq!(
            app.handle_event(Event::Paste("corrected".to_string())),
            None
        );
        assert_eq!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::RunEnsemble {
                workflow,
                prompt: "corrected".into(),
            })
        );
        assert!(app.input().is_empty());
        assert!(app.is_busy());
        assert!(app.interaction().is_normal());
        assert!(!cursor_visible_after_render(&mut app, 80, 20));
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert_eq!(app.in_flight_mode(), Some(in_flight_mode));
    }
}

#[test]
fn multiline_paste_after_palette_acceptance_survives_ensemble_submission() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "/ensemble-plan".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "/ensemble-plan ");
    assert_eq!(
        app.handle_event(Event::Paste("firstsecond".to_string())),
        None
    );
    for _ in 0..6 {
        app.handle_event(key(KeyCode::Left));
    }
    assert_eq!(app.handle_event(Event::Paste("\n".to_string())), None);
    assert_eq!(app.input(), "/ensemble-plan first\nsecond");
    assert!(!app.is_busy());

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::RunEnsemble {
            workflow: EnsembleWorkflow::Plan,
            prompt: "first\nsecond".into(),
        })
    );
}

#[test]
fn compact_locks_before_ack_rejects_duplicates_and_accepts_early_failure() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("/compact", "/compact".len());

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Compact {
            mode: SessionMode::Build,
        })
    );
    assert!(app.is_busy());
    assert_eq!(app.active_turn_id(), None);
    app.set_input_for_test("/compact", "/compact".len());
    assert_eq!(app.handle_event(ctrl_enter()), None);

    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TurnId::new(88),
        error: "cannot compact yet".to_string(),
    });
    assert!(!app.is_busy());
    assert_eq!(app.active_turn_id(), None);
    assert!(
        matches!(app.history().last(), Some(HistoryEntry::Error(error)) if error == "cannot compact yet")
    );
}
