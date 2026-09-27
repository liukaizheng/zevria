use super::*;
use crate::app::{ComposerChrome, ConversationTail};
use zevria_session_api::ModeSelectionResult;

fn select_command(app: &mut App, text: &str) -> (String, SessionMode) {
    enter_insert(app);
    app.set_input_for_test(text, text.len());
    let Some(UiAction::SetMode { request_id, mode }) = app.handle_event(ctrl_enter()) else {
        panic!("mode command must use management, not a turn");
    };
    (request_id, mode)
}

fn accepted(app: &mut App, request_id: String, mode: SessionMode, changed: bool) {
    app.reduce_without_effects(SessionEvent::ModeResult {
        request_id,
        result: ModeSelectionResult::Accepted { mode, changed },
    });
}

#[test]
fn orchestration_completion_retains_draft_and_recall_preserves_explicit_behavior() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("/orch", 5);
    assert!(app.handle_event(key(KeyCode::Enter)).is_none());
    assert_eq!(app.input(), "/orchestrate ");
    assert!(!app.is_busy());
    assert!(app.handle_event(ctrl_enter()).is_none());
    assert_eq!(app.input(), "/orchestrate ");
    let draft = "/orchestrate implement independent parts";
    app.set_input_for_test(draft, draft.len());
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Orchestrate,
            mode: SessionMode::Build,
            ..
        })
    ));
    apply_turn_event(
        &mut app,
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "concurrency limit is one".into(),
        },
    );
    assert_eq!(app.input(), draft);
    let request =
        zevria_foundation::RequestMetadata::new(zevria_foundation::RequestBehavior::Orchestrate);
    app.restore(vec![
        TranscriptItem::RequestPrompt {
            message: Message::user("implement independent parts"),
            request: request.clone(),
        },
        TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(request)),
    ]);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    assert_eq!(app.input(), draft);
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            replacement: TranscriptEditReplacement::Message {
                behavior: zevria_foundation::RequestBehavior::Orchestrate,
                ..
            },
            ..
        }))
    ));
}

#[test]
fn literal_recall_and_removed_orchestration_prefix_are_standard_requests() {
    use zevria_foundation::{RequestBehavior, RequestMetadata};
    for literal in ["/orchestrate literal data", "$orchestrate literal data"] {
        let mut app = App::new();
        let request = RequestMetadata::new(RequestBehavior::Standard);
        app.restore(vec![
            TranscriptItem::RequestPrompt {
                message: Message::user(literal),
                request: request.clone(),
            },
            TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(
                request,
            )),
        ]);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        assert_eq!(app.input(), format!(" {literal}"));
        assert!(
            matches!(app.handle_event(ctrl_enter()), Some(UiAction::EditTranscript(TranscriptEdit { replacement: TranscriptEditReplacement::Message { behavior: RequestBehavior::Standard, text, .. }, .. })) if text.text_projection() == literal)
        );
    }
    let mut app = App::new();
    let request = RequestMetadata::new(RequestBehavior::Orchestrate);
    app.restore(vec![
        TranscriptItem::RequestPrompt {
            message: Message::user("independent work"),
            request: request.clone(),
        },
        TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(request)),
    ]);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    assert_eq!(app.input(), "/orchestrate independent work");
    app.set_input_for_test("independent work", 16);
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            replacement: TranscriptEditReplacement::Message {
                behavior: RequestBehavior::Standard,
                ..
            },
            ..
        }))
    ));
}

#[test]
fn mode_commands_wait_for_correlated_acknowledgements_without_a_turn_or_history() {
    for (command, mode) in [("/build", SessionMode::Build), ("/plan", SessionMode::Plan)] {
        let mut app = configured_app();
        let (request_id, requested) = select_command(&mut app, command);
        assert_eq!(requested, mode);
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(app.mode_selection_pending());
        assert!(app.is_busy());
        assert!(app.input().is_empty());
        assert_eq!(app.active_turn_id(), None);
        assert!(app.history().is_empty());
        let parts = app.render_parts();
        assert!(matches!(parts.tail, ConversationTail::None));
        assert_eq!(parts.chrome, ComposerChrome::ModePending);
        assert!(!parts.composer_locked);
        accepted(&mut app, request_id, mode, mode != SessionMode::Build);
        assert_eq!(app.next_mode(), mode);
        assert!(!app.is_busy());
        assert!(!app.mode_selection_pending());
        assert_eq!(app.plan_state(), PlanWorkflowState::Idle);
        assert!(app.history().is_empty());
        assert!(app.interaction().is_insert());
    }
}

#[test]
fn pending_mode_selection_locks_input_management_and_unrelated_turn_events() {
    let mut app = configured_app();
    let (id, _) = select_command(&mut app, "/plan");
    for event in [
        key(KeyCode::Char('x')),
        key(KeyCode::BackTab),
        ctrl_enter(),
        Event::Paste("must not insert".into()),
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    ] {
        assert_eq!(app.handle_event(event), None);
    }
    assert!(app.begin_model_management().is_none());
    assert!(app.input().is_empty());
    for event in [
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            mode: SessionMode::Plan,
            message: Message::user("stale turn"),
        },
        SessionEvent::CompactionStarted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::Manual,
        },
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "stale rejection".into(),
        },
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    ] {
        app.reduce_without_effects(event);
        assert!(app.mode_selection_pending());
        assert_eq!(app.active_turn_id(), None);
        assert!(app.history().is_empty());
    }
    accepted(&mut app, id, SessionMode::Plan, true);
    assert!(!app.is_busy());
}

#[test]
fn wrong_request_wrong_mode_and_duplicate_acknowledgements_are_inert() {
    let mut app = App::new();
    let (id, mode) = select_command(&mut app, "/plan");
    accepted(&mut app, "previous-session".into(), mode, true);
    accepted(&mut app, id.clone(), SessionMode::Build, true);
    app.reduce_without_effects(SessionEvent::ModeResult {
        request_id: "previous-session".into(),
        result: ModeSelectionResult::Rejected {
            code: "stale".into(),
            message: "not visible".into(),
        },
    });
    assert!(app.mode_selection_pending());
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert!(app.history().is_empty());
    accepted(&mut app, id.clone(), mode, true);
    app.reduce_without_effects(SessionEvent::ModeResult {
        request_id: id.clone(),
        result: ModeSelectionResult::Rejected {
            code: "duplicate".into(),
            message: "not visible".into(),
        },
    });
    accepted(&mut app, id, mode, true);
    assert_eq!(app.next_mode(), mode);
    assert!(app.history().is_empty());
    assert!(!app.is_busy());
    let (id, _) = select_command(&mut app, "/plan");
    accepted(&mut app, id, mode, false);
    assert_eq!(app.next_mode(), mode);
    assert!(
        app.history().is_empty(),
        "idempotent selection is not a prompt or notice"
    );
}

#[test]
fn selection_error_restores_exact_command_draft_and_never_overwrites_newer_input() {
    for newer in [None, Some("new draft")] {
        let mut app = App::new();
        app.apply_selected_mode(SessionMode::Plan);
        let draft = "/plan \n\t";
        let (id, _) = select_command(&mut app, draft);
        if let Some(newer) = newer {
            app.set_input_for_test(newer, 3);
        }
        app.reduce_without_effects(SessionEvent::ModeResult {
            request_id: id,
            result: ModeSelectionResult::Rejected {
                code: "save_failed".into(),
                message: "Selection was not persisted".into(),
            },
        });
        assert_eq!(app.next_mode(), SessionMode::Plan);
        assert!(!app.is_busy());
        assert_eq!(app.input(), newer.unwrap_or(draft));
        assert_eq!(
            app.input_cursor(),
            if newer.is_some() { 3 } else { draft.len() }
        );
        assert!(app.interaction().is_insert());
        assert!(
            matches!(app.history(), [HistoryEntry::Error(error)] if error.contains("save_failed") && error.contains("not persisted"))
        );
    }
}

#[test]
fn disconnect_before_acknowledgement_restores_draft_and_ignores_a_late_result() {
    let mut app = App::new();
    let (id, mode) = select_command(&mut app, "/plan");
    app.mode_selection_disconnected();
    assert_eq!(app.input(), "/plan");
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert!(!app.is_busy());
    let history_len = app.history().len();
    app.mode_selection_disconnected();
    accepted(&mut app, id, mode, true);
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert_eq!(app.history().len(), history_len);
}

#[test]
fn restoring_a_session_invalidates_pending_mode_requests_without_reusing_ids() {
    let mut app = App::new();
    let (old_id, _) = select_command(&mut app, "/plan");
    app.restore(vec![TranscriptItem::SessionMode(SessionMode::Plan)]);
    app.restore_plan_state(PlanWorkflowState::Idle);
    app.apply_selected_mode(SessionMode::Plan);
    let (id, mode) = select_command(&mut app, "/build");
    assert_ne!(old_id, id);
    accepted(&mut app, old_id, SessionMode::Plan, true);
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert!(app.mode_selection_pending());
    accepted(&mut app, id, mode, true);
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert!(app.history().is_empty());
}

#[test]
fn shortcut_uses_the_same_ack_path_preserving_unicode_draft_caret_and_focus() {
    for (from, to) in [
        (SessionMode::Build, SessionMode::Plan),
        (SessionMode::Plan, SessionMode::Build),
    ] {
        for success in [true, false] {
            let mut app = App::new();
            app.apply_selected_mode(from);
            enter_insert(&mut app);
            let draft = "é draft\nnext line";
            app.set_input_for_test(draft, "é".len());
            let Some(UiAction::SetMode { request_id, mode }) =
                app.handle_event(key(KeyCode::BackTab))
            else {
                panic!("mode management shortcut");
            };
            assert_eq!(mode, to);
            assert_eq!(app.next_mode(), from);
            assert_eq!(app.input(), draft);
            assert_eq!(app.input_cursor(), 2);
            assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
            assert!(app.handle_event(ctrl_enter()).is_none());
            app.handle_event(key(KeyCode::Char('x')));
            assert_eq!(app.input(), "éx draft\nnext line");
            app.handle_event(ctrl('z'));
            assert_eq!(app.input(), draft);
            app.reduce_without_effects(SessionEvent::ModeResult {
                request_id,
                result: if success {
                    ModeSelectionResult::Accepted {
                        mode,
                        changed: true,
                    }
                } else {
                    ModeSelectionResult::Rejected {
                        code: "busy".into(),
                        message: "Try again".into(),
                    }
                },
            });
            assert_eq!(app.next_mode(), if success { to } else { from });
            assert_eq!(app.input(), draft);
            assert_eq!(app.input_cursor(), 2);
            assert!(app.interaction().is_insert());
        }
    }
}

#[test]
fn mode_completion_enter_selects_but_tab_only_completes() {
    for (prefix, full, mode) in [
        ("/bui", "/build ", SessionMode::Build),
        ("/pla", "/plan ", SessionMode::Plan),
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        app.set_input_for_test(prefix, prefix.len());
        assert!(app.handle_event(key(KeyCode::Tab)).is_none());
        assert_eq!(app.input(), full);
        assert!(!app.is_busy());
        app.set_input_for_test(prefix, prefix.len());
        let Some(UiAction::SetMode {
            request_id,
            mode: selected,
        }) = app.handle_event(key(KeyCode::Enter))
        else {
            panic!("Enter should activate {prefix}");
        };
        assert_eq!(selected, mode);
        assert!(app.input().is_empty());
        assert!(app.mode_selection_pending());
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(app.history().is_empty());

        app.set_input_for_test(prefix, prefix.len());
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(app.input(), full);
        assert_eq!(app.handle_event(ctrl_enter()), None);
        accepted(&mut app, request_id, mode, true);
        assert_eq!(app.next_mode(), mode);
        assert!(!app.mode_selection_pending());
        assert_eq!(
            app.input(),
            full,
            "acknowledgement preserves the newer draft"
        );
    }
}

#[test]
fn mode_controls_retain_invalid_drafts_and_cannot_replace_recalled_messages() {
    for command in ["/build", "/plan", "/plan"] {
        let mut app = App::new();
        enter_insert(&mut app);
        let invalid = format!("{command} extra");
        app.set_input_for_test(&invalid, invalid.len());
        assert!(app.handle_event(ctrl_enter()).is_none());
        assert_eq!(app.input(), invalid);
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(!app.mode_selection_pending());

        let mut recall = App::new();
        recall.seed_history_entry(history_message(Message::user("original")));
        recall.select_for_test(cursor(0, 0));
        ctrl_e(&mut recall);
        recall.set_input_for_test(command, command.len());
        assert!(recall.handle_event(ctrl_enter()).is_none());
        assert!(recall.handle_event(key(KeyCode::BackTab)).is_none());
        assert!(recall.is_recalling());
        assert_eq!(recall.input(), command);
        assert!(!recall.is_busy());
    }
}

#[test]
fn artifact_snapshots_retain_selection_and_only_ready_is_modal() {
    let artifact = test_plan_artifact();
    for mode in SessionMode::ALL {
        for snapshot in [
            PlanWorkflowState::Idle,
            PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: None,
            },
            PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact.clone()),
            },
            PlanWorkflowState::Published {
                artifact: artifact.clone(),
            },
            PlanWorkflowState::Resolved {
                artifact: artifact.clone(),
                resolution: PlanResolution::ImplementedCurrent,
            },
        ] {
            let mut app = App::new();
            app.apply_selected_mode(mode);
            app.reduce_without_effects(SessionEvent::PlanStateChanged {
                state: snapshot.clone(),
            });
            assert_eq!(app.next_mode(), mode);
            assert_eq!(app.plan_state(), snapshot);
            assert!(app.render_parts().plan_dialog.is_none());
            app.apply_selected_mode(mode);
            assert_eq!(app.next_mode(), mode);
        }
        let mut app = App::new();
        app.apply_selected_mode(mode);
        app.restore_plan_state(PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        });
        app.apply_selected_mode(mode);
        assert_eq!(app.next_mode(), SessionMode::Plan);
        assert!(app.render_parts().plan_dialog.is_some());
        assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
    }
}

#[test]
fn ready_snapshot_racing_a_mode_result_stays_modal_and_cannot_be_approved_by_selection() {
    let mut app = App::new();
    let (id, mode) = select_command(&mut app, "/plan");
    let snapshot = PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    };
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: snapshot.clone(),
    });
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert!(app.mode_selection_pending());
    let text = rendered_text(&mut app, 160, 20);
    assert!(text.contains("selecting mode · input locked"), "{text}");
    assert!(!text.contains("Enter confirm"));
    assert!(!text.contains("Ctrl+C cancel"));
    assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
    assert!(app.handle_event(ctrl_enter()).is_none());
    assert!(app.begin_model_management().is_none());
    accepted(&mut app, id.clone(), mode, true);
    assert!(!app.is_busy());
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert_eq!(app.plan_state(), snapshot);
    assert!(app.render_parts().plan_dialog.is_some());
    assert!(!app.render_parts().composer_locked);
    assert!(!app.render_parts().editor_active);
    assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
    accepted(&mut app, id, mode, true);
    assert_eq!(app.next_mode(), SessionMode::Plan);
    assert_eq!(app.plan_state(), snapshot);
}

#[test]
fn explicit_implementation_mode_change_preserves_ready_modal_until_the_replacing_snapshot() {
    for decision in [PlanDecision::ImplementCurrent, PlanDecision::ImplementFresh] {
        let mut app = App::new();
        let artifact = test_plan_artifact();
        let snapshot = PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        };
        app.restore_plan_state(snapshot.clone());
        assert!(
            app.handle_event(key(KeyCode::Char(
                if decision == PlanDecision::ImplementFresh {
                    '2'
                } else {
                    '1'
                }
            )))
            .is_none()
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::ResolvePlan {
                expected: artifact.version,
                decision,
            })
        );
        // Even workflow traffic cannot override Ready presentation. The engine
        // publishes the replacement Plan snapshot before its selected mode.
        app.reduce_without_effects(SessionEvent::ModeChanged {
            mode: SessionMode::Build,
        });
        assert_eq!(app.next_mode(), SessionMode::Plan);
        assert_eq!(app.plan_state(), snapshot);
        assert!(app.render_parts().plan_dialog.unwrap().decision_pending);
        assert!(app.handle_event(key(KeyCode::Enter)).is_none());
        assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
        assert!(app.begin_model_management().is_none());
        let resolution = match decision {
            PlanDecision::ImplementCurrent => PlanResolution::ImplementedCurrent,
            PlanDecision::ImplementFresh => PlanResolution::ImplementedFresh,
            PlanDecision::Revise => unreachable!(),
        };
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Resolved {
                artifact,
                resolution,
            },
        });
        app.reduce_without_effects(SessionEvent::ModeChanged {
            mode: SessionMode::Build,
        });
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(app.render_parts().plan_dialog.is_none());
    }
}

#[test]
fn explicit_workflow_selection_changes_mode_without_erasing_retained_artifacts() {
    let artifact = test_plan_artifact();
    let mut app = App::new();
    let snapshot = PlanWorkflowState::Published { artifact };
    app.restore_plan_state(snapshot.clone());
    for mode in [SessionMode::Plan, SessionMode::Build, SessionMode::Plan] {
        app.reduce_without_effects(SessionEvent::ModeChanged { mode });
        assert_eq!(app.next_mode(), mode);
        app.reduce_without_effects(SessionEvent::PlanStateChanged {
            state: snapshot.clone(),
        });
        assert_eq!(app.next_mode(), mode);
        assert_eq!(app.plan_state(), snapshot);
        assert!(!app.is_busy());
    }
}

#[test]
fn inspect_panes_and_live_workers_ignore_root_mode_metadata_actions_and_acknowledgements() {
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "worker".into(),
        label: "Worker".into(),
        safe_mode: "read-only".into(),
    };
    let mut worker = App::acp_inspect("live worker");
    worker.bind_worker_review(
        zevria_workflow::WorkerControlTarget {
            turn_id: TEST_TURN_ID,
            run_id: EnsembleRunId::new(),
            worker_id: descriptor.id.clone(),
        },
        Box::new(zevria_workflow::WorkerReviewState::new(descriptor)),
    );
    for mut app in [
        App::subtask_inspect("child"),
        App::acp_inspect("historical worker"),
        worker,
    ] {
        app.restore(vec![TranscriptItem::SessionMode(SessionMode::Plan)]);
        app.apply_selected_mode(SessionMode::Plan);
        app.reduce_without_effects(SessionEvent::ModeChanged {
            mode: SessionMode::Plan,
        });
        accepted(&mut app, "mode-root".into(), SessionMode::Plan, true);
        assert_eq!(app.next_mode(), SessionMode::Build);
        assert!(app.handle_event(key(KeyCode::BackTab)).is_none());
        assert!(!app.is_busy());
        assert!(app.history().is_empty());
    }
}

#[test]
fn session_mode_and_other_private_metadata_do_not_change_rendered_rows_or_recall_ordinals() {
    let prompt = TranscriptItem::Message(Message::user("visible prompt"));
    let mut baseline = App::new();
    baseline.restore(vec![prompt.clone()]);
    let expected = rendered_text(&mut baseline, 100, 20);
    for mode in SessionMode::ALL {
        let mut app = App::new();
        app.restore(vec![
            TranscriptItem::SessionModels(
                zevria_model::models::SessionModels::new(
                    zevria_model::models::ModelSelection::new(
                        ModelProfileRef::new("hidden-provider", "hidden-build"),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    zevria_model::models::ModelSelection::new(
                        ModelProfileRef::new("hidden-provider", "hidden-plan"),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                )
                .unwrap(),
            ),
            TranscriptItem::SessionMode(mode),
            TranscriptItem::Directive(zevria_instructions::DirectiveContent::skill(
                &SkillSnapshot::new(
                    SkillName::parse("hidden-skill").unwrap(),
                    "Hidden skill description",
                    "HIDDEN_SKILL_BODY",
                )
                .unwrap(),
            )),
            prompt.clone(),
        ]);
        assert_eq!(app.history().len(), 1);
        assert_eq!(rendered_text(&mut app, 100, 20), expected);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        assert_eq!(app.input(), "visible prompt");
        assert!(matches!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(0),
                ..
            }))
        ));
    }
}

#[test]
fn narrow_mode_hints_keep_the_correct_complete_shortcut_and_pending_lock() {
    for (mode, target) in [(SessionMode::Build, "Plan"), (SessionMode::Plan, "Build")] {
        for width in [18, 24, 40, 100] {
            for insert in [false, true] {
                let mut app = App::new();
                app.apply_selected_mode(mode);
                if insert {
                    enter_insert(&mut app);
                }
                let text = rendered_text(&mut app, width, 20);
                // Narrow hints omit entire lower-priority bindings, never partial keys.
                if width >= 100 {
                    assert!(text.contains(&format!("Shift+Tab {target}")), "{text}");
                } else if !insert {
                    assert!(text.contains("? help"), "{text}");
                } else {
                    assert!(
                        text.contains("Esc normal") || text.contains("Ctrl+Enter send"),
                        "{text}"
                    );
                }
                let Some(UiAction::SetMode { .. }) = app.handle_event(key(KeyCode::BackTab)) else {
                    panic!("idle mode shortcut");
                };
                let text = rendered_text(&mut app, width, 20);
                if width < 50 {
                    assert!(text.contains("Selecting mode"), "{text}");
                } else {
                    assert!(text.contains("Selecting mode"));
                }
                assert!(!text.contains("Shift+Tab"));
                assert!(!text.contains("Ctrl+C to cancel turn"));
            }
        }
    }
}

#[test]
fn orchestrate_uses_build_role_styling_and_hints_even_on_narrow_terminals() {
    let mut app = configured_app();
    app.apply_selected_mode(SessionMode::Build);
    enter_insert(&mut app);
    app.set_input_for_test("/orchestrate draft", 17);
    let status = app.status_for_test();
    assert_eq!(status.accent, crate::status::StatusAccent::Build);
    assert_eq!(
        status.profile,
        Some(ModelProfileRef::new("provider-build", "build-model"))
    );
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("Build"));
    assert!(rendered.contains("Shift+Tab Plan"));
    assert!(!rendered.contains("inspection commands only"));
    for (width, height) in [
        (1, 1),
        (4, 3),
        (8, 8),
        (14, 12),
        (24, 8),
        (40, 20),
        (100, 20),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        if width >= 40 {
            let caption = buffer
                .content
                .iter()
                .find(|cell| cell.symbol() == "B")
                .expect("Build caption");
            assert_eq!(caption.fg, ZEVRIA_DARK.workflow.build);
        }
    }
    assert_eq!(app.input(), "/orchestrate draft");
    assert_eq!(app.input_cursor(), 17);
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Orchestrate,
            text: "draft".into(),
            mode: SessionMode::Build
        })
    );
    assert_eq!(app.in_flight_role(), Some(ModelRole::Build));
}
