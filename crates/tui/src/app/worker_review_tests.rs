use super::*;
use ratatui::{Terminal, backend::TestBackend};
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentStructuredPlan;

fn worker() -> App {
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "worker".into(),
        label: "Worker".into(),
        safe_mode: "read-only".into(),
    };
    let mut state = WorkerReviewState::new(descriptor.clone());
    let input = WorkerInput {
        generation: 1,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::Initial,
        text: "initial".into(),
    };
    state
        .apply(&WorkerReviewEvent::InputAccepted { input })
        .unwrap();
    state
        .apply(&WorkerReviewEvent::Dispatched {
            generation: 1,
            attempt: 1,
        })
        .unwrap();
    state
        .apply(&WorkerReviewEvent::Published {
            generation: 1,
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: Some("# Plan".into()),
                entries: vec![],
            },
            replay: false,
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
    let mut app = App::acp_inspect("Worker");
    app.bind_worker_review(
        WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: EnsembleRunId::new(),
            worker_id: descriptor.id,
        },
        Box::new(state),
    );
    assert!(app.interaction.is_normal());
    assert!(!app.composer_editable());
    assert!(
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(app.composer_editable());
    app
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(ratatui::crossterm::event::KeyEvent::new(code, modifiers))
}
#[test]
fn worker_insert_redo_never_confirms_even_when_empty_or_in_completion() {
    let mut app = worker();
    for character in ['y', 'Y'] {
        assert_eq!(
            app.handle_event(key(KeyCode::Char(character), KeyModifiers::CONTROL)),
            None
        );
        assert!(app.worker.pending.is_none());
    }
    app.handle_event(Event::Paste("/con".into()));
    assert!(app.command_menu_active());
    app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert!(app.composer.is_empty());
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.composer.text(), "/con");
    assert!(app.command_menu_active());
    assert!(app.worker.pending.is_none());
    app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.interaction.is_normal());
    assert!(matches!(
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        Some(UiAction::WorkerControl(WorkerControl {
            action: WorkerControlAction::Confirm { .. },
            ..
        }))
    ));
}

#[test]
fn worker_clear_undo_invalidates_an_ack_even_when_visible_content_matches() {
    let mut app = worker();
    app.handle_event(Event::Paste("/confirm".into()));
    let draft = app.composer.snapshot();
    let Some(UiAction::WorkerControl(control)) = app.submit_worker() else {
        panic!("confirmation command")
    };
    assert!(app.composer_editable());
    app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(app.composer.text(), "/confirm");
    assert!(!app.composer.matches_draft(&draft));
    app.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "accepted old draft".into(),
    });
    assert_eq!(app.composer.text(), "/confirm");
    app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert!(app.composer.is_empty());
}

#[test]
fn worker_draft_history_survives_rejection_but_not_acceptance_and_freeze_blocks_replay() {
    for accepted in [false, true] {
        let mut app = worker();
        app.handle_event(Event::Paste("feedback".into()));
        let Some(UiAction::WorkerControl(control)) = app.submit_worker() else {
            panic!("feedback")
        };
        assert!(app.interaction.is_normal());
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.composer_editable());
        let pending = app.composer.snapshot();
        app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(app.composer.text(), "feedback");
        assert!(!app.composer.matches_draft(&pending));
        app.worker_control_result(&WorkerControlResult {
            control,
            accepted,
            detail: "settled".into(),
        });
        assert_eq!(app.composer.text(), "feedback");
        assert!(app.interaction.is_insert());
        assert!(app.composer.undo());
        assert!(app.composer.is_empty());
        assert!(app.composer.redo());
        app.freeze_worker();
        let frozen = app.composer.snapshot();
        for character in ['z', 'y'] {
            assert_eq!(
                app.handle_event(key(KeyCode::Char(character), KeyModifiers::CONTROL)),
                None
            );
            assert!(app.composer.matches_draft(&frozen));
        }
    }
}

#[test]
fn worker_composers_are_independent_multiline_and_scoped() {
    let mut first = worker();
    let second = worker();
    for character in "feedback".chars() {
        first.handle_event(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    first.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    first.handle_event(Event::Paste("more".into()));
    assert_eq!(first.composer.text(), "feedback\nmore");
    assert!(second.composer.text().is_empty());
    let Some(UiAction::WorkerControl(control)) =
        first.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("worker control")
    };
    assert!(
        matches!(&control.action, WorkerControlAction::SendFeedback { text } if text == &zevria_content::UserPrompt::from_text("feedback\nmore"))
    );
    assert_eq!(&control.target, &first.worker.bound.as_ref().unwrap().0);
    assert_ne!(&control.target, &second.worker.bound.as_ref().unwrap().0);
    assert!(first.worker_busy());
    assert!(first.interaction.is_normal());
    assert!(second.interaction.is_insert());
    assert!(!second.worker_busy());
    assert_eq!(
        first.composer.text(),
        "feedback\nmore",
        "draft is retained until durable acknowledgement"
    );
    first.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "accepted".into(),
    });
    assert!(first.composer.text().is_empty());
    assert!(first.interaction.is_normal());
}
#[test]
fn worker_feedback_returns_to_navigation_and_explicit_reentry_preserves_newer_drafts() {
    for accepted in [false, true] {
        let mut app = worker();
        for index in 0..12 {
            app.conversation.push_message(
                Message::user(format!("message {index}")),
                ToolCallStatus::Finished,
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(terminal.backend().cursor_visible());
        let control = feedback(&mut app, "submitted");
        assert!(app.interaction.is_normal());
        assert!(app.worker.pending.is_some());
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(!terminal.backend().cursor_visible());
        let bottom = app.view.scroll();
        assert!(bottom > 0);
        app.handle_event(key(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.view.scroll(), bottom - 1);
        app.handle_event(key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view.scroll(), bottom);
        assert_eq!(app.composer.text(), "submitted");

        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.composer_editable());
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(terminal.backend().cursor_visible());
        app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        app.handle_event(Event::Paste("newer draft".into()));
        app.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        let newer = app.composer.snapshot();
        assert!(app.worker_busy());
        assert_eq!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL)),
            None
        );
        assert!(app.interaction.is_insert());
        assert_eq!(app.composer.snapshot(), newer);
        app.worker_control_result(&WorkerControlResult {
            control: control.clone(),
            accepted,
            detail: "settled".into(),
        });
        assert!(app.interaction.is_insert());
        assert_eq!(app.composer.snapshot(), newer);
        assert_eq!(app.drafts.has_recovery(), !accepted);
        if accepted {
            let (target, mut state) = app.worker.bound.clone().unwrap();
            accepted_input(&mut state, &control);
            app.bind_worker_review(target.clone(), state.clone());
            assert!(app.interaction.is_insert());
            assert_eq!(app.composer.snapshot(), newer);
            settle(&mut state);
            app.bind_worker_review(target, state);
        }
        assert!(!app.worker_busy());
        assert!(app.interaction.is_insert());
        assert_eq!(app.composer.snapshot(), newer);
        app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.composer.is_empty());
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(app.composer.text(), "newer draft");
    }
}

#[test]
fn worker_image_only_feedback_returns_to_normal_and_clears_only_on_acknowledgement() {
    let mut app = worker();
    app.composer
        .attach_image(zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap())
        .unwrap();
    let draft = app.composer.snapshot();
    let prompt = app.composer.prompt();
    let Some(UiAction::WorkerControl(control)) =
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("image-only feedback")
    };
    assert_eq!(
        control.action,
        WorkerControlAction::SendFeedback { text: prompt }
    );
    assert!(app.interaction.is_normal());
    assert!(app.composer.matches_draft(&draft));
    app.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "accepted".into(),
    });
    assert!(app.composer.is_empty());
    assert!(app.interaction.is_normal());
}

#[test]
fn worker_acknowledgement_distinguishes_cursor_motion_from_pending_paste() {
    for paste_pending in [false, true] {
        let mut app = worker();
        app.handle_event(Event::Paste("submitted".into()));
        let Some(UiAction::WorkerControl(control)) =
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
        else {
            panic!("feedback")
        };
        assert!(app.interaction.is_normal());
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(app.composer_editable());
        let generation = app.composer.generation();
        app.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.composer.cursor(), "submitted".len() - 1);
        assert_eq!(app.composer.generation(), generation);
        if paste_pending {
            assert!(matches!(
                app.handle_event(key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
                Some(UiAction::ReadClipboard { .. })
            ));
            assert!(app.composer.is_paste_pending());
        }
        app.worker_control_result(&WorkerControlResult {
            control,
            accepted: true,
            detail: "accepted".into(),
        });
        assert_eq!(
            app.composer.text(),
            if paste_pending { "submitted" } else { "" }
        );
        assert_eq!(app.composer.is_paste_pending(), paste_pending);
        assert!(app.interaction.is_insert());
    }
}

#[test]
fn worker_confirmation_help_uses_exact_eligibility_and_focus() {
    let mut app = worker();
    assert!(matches!(
        app.render_parts().chrome,
        ComposerChrome::Worker {
            can_confirm: false,
            ..
        }
    ));
    app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(
        app.render_parts().chrome,
        ComposerChrome::Worker {
            can_confirm: true,
            ..
        }
    ));
    app.composer.insert_text("unsent");
    assert!(matches!(
        app.render_parts().chrome,
        ComposerChrome::Worker {
            can_confirm: false,
            ..
        }
    ));
    assert!(!app.render_parts().composer_locked);
}

#[test]
fn worker_rejection_preserves_draft_and_shortcuts_never_discard_it() {
    let mut app = worker();
    app.handle_event(Event::Paste("draft".into()));
    assert!(
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert_eq!(app.composer.text(), "draft");
    let Some(UiAction::WorkerControl(control)) =
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("feedback")
    };
    assert!(app.interaction.is_normal());
    app.worker_control_result(&WorkerControlResult::rejected(control, "stale target"));
    assert_eq!(app.composer.text(), "draft");
    assert!(app.interaction.is_normal());
    app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert!(app.composer_editable());
    assert!(
        app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(app.composer.text().is_empty());
    assert_eq!(
        app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        None,
        "an idle worker cannot cancel root or quit"
    );
}

#[test]
fn worker_confirmation_binds_revision_and_normal_c_does_not_intercept_inserted_prose() {
    let mut app = worker();
    app.handle_event(key(KeyCode::Char('c'), KeyModifiers::NONE));
    assert_eq!(app.composer.text(), "c");
    app.composer.clear();
    app.interaction.enter_normal();
    let revision = app
        .worker
        .bound
        .as_ref()
        .unwrap()
        .1
        .eligible_snapshot()
        .unwrap()
        .revision
        .clone();
    assert!(
        matches!(app.handle_event(key(KeyCode::Char('c'), KeyModifiers::NONE)), Some(UiAction::WorkerControl(WorkerControl { action: WorkerControlAction::Confirm { expected_revision }, .. })) if expected_revision == revision)
    );
}
#[test]
fn worker_root_commands_are_rejected_and_literal_escape_is_available() {
    let mut app = worker();
    app.handle_event(Event::Paste("/ensemble-plan hidden dispatch".into()));
    assert!(
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
            .is_none()
    );
    assert_eq!(app.composer.text(), "/ensemble-plan hidden dispatch");
    assert!(app.interaction.is_insert());
    app.composer
        .replace("//implement is syntax to discuss".into(), 31);
    assert!(
        matches!(app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL)), Some(UiAction::WorkerControl(WorkerControl { action: WorkerControlAction::SendFeedback { text }, .. })) if text == zevria_content::UserPrompt::from_text("/implement is syntax to discuss"))
    );
}
#[test]
fn worker_cancel_remains_available_before_feedback_ack_and_freezing_preserves_draft() {
    let mut app = worker();
    app.handle_event(Event::Paste("retained draft".into()));
    let Some(UiAction::WorkerControl(feedback)) =
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("feedback")
    };
    assert!(matches!(
        app.worker_action(WorkerControlAction::CancelPrompt),
        Some(UiAction::WorkerControl(WorkerControl {
            action: WorkerControlAction::CancelPrompt,
            ..
        }))
    ));
    let mut stale = feedback.clone();
    stale.target.turn_id = TurnId::new(99);
    app.worker_control_result(&WorkerControlResult {
        control: stale,
        accepted: true,
        detail: "stale reply".into(),
    });
    assert_eq!(app.composer.text(), "retained draft");
    assert!(app.worker.pending.is_some());
    let Some(UiAction::WorkerControl(abandon)) = app.worker_action(WorkerControlAction::Abandon)
    else {
        panic!("urgent abandonment")
    };
    assert!(!app.worker.urgent.is_empty());
    app.freeze_worker();
    for control in [feedback, abandon] {
        app.worker_control_result(&WorkerControlResult {
            control,
            accepted: true,
            detail: "late acknowledgement after terminal freeze".into(),
        });
    }
    assert_eq!(app.composer.text(), "retained draft");
    assert!(app.worker.pending.is_none());
    assert!(app.worker.handoff.is_none());
    assert!(app.worker.urgent.is_empty());
    assert!(
        app.worker_action(WorkerControlAction::CancelPrompt)
            .is_none()
    );
}

#[test]
fn worker_abandon_is_urgent_and_only_clears_its_matching_unchanged_draft() {
    for changed in [false, true] {
        let mut app = worker();
        app.handle_event(Event::Paste("feedback".into()));
        let Some(UiAction::WorkerControl(feedback)) =
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
        else {
            panic!("feedback")
        };
        assert!(app.interaction.is_normal());
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        app.composer.replace("/ab".into(), 3);
        // Enter activation retains the urgent control's independent eligibility.
        let Some(UiAction::WorkerControl(abandon)) =
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("urgent abandonment")
        };
        assert_eq!(abandon.action, WorkerControlAction::Abandon);
        assert_eq!(app.worker.pending.as_ref().unwrap().0, feedback);
        app.worker_control_result(&WorkerControlResult {
            control: feedback,
            accepted: true,
            detail: "queued".into(),
        });
        assert_eq!(app.composer.text(), "/abandon ");
        if changed {
            app.composer.replace("unrelated draft".into(), 15);
        }
        let (target, mut state) = app.worker.bound.clone().unwrap();
        state
            .apply(&WorkerReviewEvent::Abandoned {
                request_id: abandon.request_id.clone(),
            })
            .unwrap();
        app.bind_worker_review(target, state);
        assert!(!app.pane.is_worker());
        app.worker_control_result(&WorkerControlResult {
            control: abandon,
            accepted: true,
            detail: "excluded".into(),
        });
        assert_eq!(
            app.composer.text(),
            if changed { "unrelated draft" } else { "" }
        );
        assert!(app.worker_action(WorkerControlAction::Retry).is_none());
        assert!(app.worker_action(WorkerControlAction::Abandon).is_none());
    }
}

#[test]
fn worker_abandon_rejection_and_late_acceptance_after_freeze_preserve_drafts() {
    let mut app = worker();
    app.composer.replace("/abandon".into(), 8);
    let Some(UiAction::WorkerControl(control)) = app.submit_worker() else {
        panic!("abandon")
    };
    app.worker_control_result(&WorkerControlResult::rejected(control.clone(), "sealed"));
    assert_eq!(app.composer.text(), "/abandon");
    app.freeze_worker();
    app.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "late".into(),
    });
    assert_eq!(app.composer.text(), "/abandon");
}

#[test]
fn worker_oversize_proposal_is_not_confirmable() {
    let mut app = worker();
    app.worker
        .bound
        .as_mut()
        .unwrap()
        .1
        .apply(&WorkerReviewEvent::PayloadChecked {
            error: Some("mandatory payload requires 20000 bytes; limit 16384".into()),
        })
        .unwrap();
    assert!(
        app.handle_event(key(KeyCode::Char('y'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(app.worker.bound.as_ref().unwrap().1.retained.is_some());
}

fn bind_event(app: &mut App, event: WorkerReviewEvent) {
    let (target, mut state) = app.worker.bound.clone().unwrap();
    state.apply(&event).unwrap();
    app.bind_worker_review(target, state);
}

fn feedback(app: &mut App, draft: &str) -> WorkerControl {
    app.handle_event(Event::Paste(draft.into()));
    let Some(UiAction::WorkerControl(control)) =
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("feedback submission")
    };
    control
}

fn accepted_input(state: &mut WorkerReviewState, control: &WorkerControl) {
    let (kind, text) = match &control.action {
        WorkerControlAction::SendFeedback { text } => {
            (WorkerPromptKind::UserFeedback, text.clone())
        }
        WorkerControlAction::Retry => (WorkerPromptKind::RecoveryContinuation, "retry".into()),
        _ => panic!("prompt control"),
    };
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: WorkerInput {
                generation: state.accepted_generation + 1,
                request_id: control.request_id.clone(),
                kind,
                text,
            },
        })
        .unwrap();
}

fn settle(state: &mut WorkerReviewState) {
    let generation = state.accepted_generation;
    if state.active.is_none() && !state.pending.is_empty() {
        state
            .apply(&WorkerReviewEvent::Dispatched {
                generation,
                attempt: 1,
            })
            .unwrap();
    }
    state
        .apply(&WorkerReviewEvent::Settled {
            generation,
            failure: None,
            connected: true,
            evidence: Box::new(state.evidence.clone()),
        })
        .unwrap();
}

fn confirm_fixture(app: &mut App) -> WorkerPlanRevision {
    let (target, state) = app.worker.bound.as_ref().unwrap();
    let revision = state.eligible_snapshot().unwrap().revision.clone();
    bind_event(
        app,
        WorkerReviewEvent::Confirmed {
            receipt: WorkerConfirmationReceipt {
                request_id: WorkerControlId::new(),
                target: target.clone(),
                revision: revision.clone(),
            },
        },
    );
    revision
}

#[test]
fn worker_busy_snapshots_allow_editing_but_block_duplicate_work() {
    for phase in ["queued", "dispatched", "recovering", "unsettled"] {
        let mut app = worker();
        app.composer.replace("/re retained\ndraft".into(), 3);
        let cursor = app.composer.cursor();
        let (target, mut state) = app.worker.bound.clone().unwrap();
        let control = WorkerControl {
            request_id: WorkerControlId::new(),
            target: target.clone(),
            action: WorkerControlAction::Retry,
        };
        accepted_input(&mut state, &control);
        if phase != "queued" {
            state
                .apply(&WorkerReviewEvent::Dispatched {
                    generation: 2,
                    attempt: 1,
                })
                .unwrap();
        }
        if phase == "recovering" {
            state
                .apply(&WorkerReviewEvent::Recovering {
                    generation: 2,
                    attempt: 2,
                })
                .unwrap();
        }
        let mut settled = state.clone();
        settle(&mut settled);
        if phase == "unsettled" {
            // Even an empty active/queued projection is not idle until its
            // accepted generation has settled.
            state.active = None;
        }
        app.bind_worker_review(target.clone(), state);
        assert!(app.worker_busy(), "{phase}");
        assert!(app.interaction.is_insert(), "{phase}");
        assert!(app.command_menu_active());
        assert!(
            !app.is_busy(),
            "worker activity is independent of root activity"
        );
        assert!(!app.render_parts().composer_locked);
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                .is_none()
        );
        app.handle_event(Event::Paste("newer".into()));
        assert_ne!(app.composer.text(), "/re retained\ndraft");
        app.handle_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.composer.text(), "/re retained\ndraft");
        app.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
        assert_ne!(app.composer.cursor(), cursor);
        app.bind_worker_review(target, settled);
        assert!(!app.worker_busy());
        assert!(app.interaction.is_insert());
        assert!(app.composer_editable());
    }
}

#[test]
fn worker_feedback_and_retry_lock_optimistically_and_rejection_preserves_drafts() {
    for draft in ["feedback", "/retry"] {
        let mut app = worker();
        let control = feedback(&mut app, draft);
        assert!(app.worker_busy());
        assert_eq!(app.interaction.is_insert(), draft == "/retry");
        assert_eq!(app.interaction.is_normal(), draft == "feedback");
        assert_eq!(app.command_menu_active(), draft == "/retry");
        if draft == "feedback" {
            app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        }
        assert!(app.composer_editable());
        let handoff = app.worker.handoff.as_ref().unwrap();
        assert_eq!(handoff.request_id, control.request_id);
        assert_eq!(handoff.target, control.target);
        assert_eq!(handoff.accepted_generation, 2);
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(
            app.worker.handoff.as_ref().unwrap().request_id,
            control.request_id
        );
        app.worker_control_result(&WorkerControlResult::rejected(control.clone(), "rejected"));
        assert!(app.worker.pending.is_none());
        assert!(app.worker.handoff.is_none());
        assert!(!app.worker_busy());
        assert_eq!(app.composer.text(), draft);
        assert!(app.interaction.is_insert());
        // A duplicate acceptance after rejection has no draft-clearing authority.
        app.worker_control_result(&WorkerControlResult {
            control,
            accepted: true,
            detail: "late".into(),
        });
        assert_eq!(app.composer.text(), draft);
    }
}

#[test]
fn worker_submission_handoff_handles_both_event_orders_and_already_settled_updates() {
    for draft in ["feedback", "/retry"] {
        for state_first in [false, true] {
            for already_settled in [false, true] {
                let mut app = worker();
                let control = feedback(&mut app, draft);
                assert_eq!(app.interaction.is_normal(), draft == "feedback");
                if draft == "feedback" {
                    app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
                }
                assert!(app.composer_editable());
                let (target, old_idle) = app.worker.bound.clone().unwrap();
                let mut updated = old_idle.clone();
                accepted_input(&mut updated, &control);
                if already_settled {
                    settle(&mut updated);
                }
                let result = WorkerControlResult {
                    control,
                    accepted: true,
                    detail: "accepted".into(),
                };
                if state_first {
                    app.bind_worker_review(target.clone(), updated.clone());
                    assert!(app.worker.handoff.is_none());
                    assert!(
                        app.worker_busy(),
                        "pending acknowledgement still locks settled state"
                    );
                    assert_eq!(app.composer.text(), draft);
                }
                app.worker_control_result(&result);
                assert!(app.composer.text().is_empty());
                assert!(app.worker.pending.is_none());
                if !state_first {
                    assert!(
                        app.worker_busy(),
                        "acceptance must not expose the old idle snapshot"
                    );
                    app.bind_worker_review(target.clone(), old_idle);
                    assert!(
                        app.worker_busy(),
                        "an older snapshot does not retire the handoff"
                    );
                    assert!(app.interaction.is_insert());
                    // A duplicate rejection cannot undo an accepted handoff.
                    app.worker_control_result(&WorkerControlResult::rejected(
                        result.control.clone(),
                        "duplicate",
                    ));
                    assert!(app.worker.handoff.is_some());
                    app.bind_worker_review(target.clone(), updated.clone());
                }
                assert!(
                    app.worker.handoff.is_none(),
                    "acceptance never recreates a retired marker"
                );
                assert_eq!(app.worker_busy(), !already_settled);
                if !already_settled {
                    settle(&mut updated);
                    app.bind_worker_review(target, updated);
                }
                assert!(!app.worker_busy());
                assert!(app.interaction.is_insert());
                app.handle_event(Event::Paste("unrelated draft".into()));
                app.worker_control_result(&result);
                assert_eq!(app.composer.text(), "unrelated draft");
                assert!(app.composer_editable());
            }
        }
    }
}

#[test]
fn worker_old_results_cannot_settle_a_new_submission_or_override_authoritative_activity() {
    let mut app = worker();
    let first = feedback(&mut app, "same draft");
    let (target, mut state) = app.worker.bound.clone().unwrap();
    accepted_input(&mut state, &first);
    settle(&mut state);
    app.bind_worker_review(target.clone(), state.clone());
    let old_result = WorkerControlResult {
        control: first,
        accepted: true,
        detail: "accepted".into(),
    };
    app.worker_control_result(&old_result);
    assert!(app.interaction.is_normal());
    app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    let second = feedback(&mut app, "same draft");
    assert!(app.interaction.is_normal());
    app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    for accepted in [true, false] {
        app.worker_control_result(&WorkerControlResult {
            accepted,
            ..old_result.clone()
        });
        assert_eq!(app.composer.text(), "same draft");
        assert_eq!(app.worker.pending.as_ref().unwrap().0, second);
        assert_eq!(
            app.worker.handoff.as_ref().unwrap().request_id,
            second.request_id
        );
    }
    // Other accepted work can arrive while this submission is rejected. The
    // local result settles only its request, not authoritative worker activity.
    accepted_input(
        &mut state,
        &WorkerControl {
            request_id: WorkerControlId::new(),
            ..second.clone()
        },
    );
    app.bind_worker_review(target.clone(), state.clone());
    app.worker_control_result(&WorkerControlResult::rejected(second, "rejected"));
    assert!(app.worker.pending.is_none());
    assert!(app.worker.handoff.is_none());
    assert!(app.worker_busy());
    assert_eq!(app.composer.text(), "same draft");
    settle(&mut state);
    app.bind_worker_review(target, state);
    assert!(!app.worker_busy());
    assert!(app.interaction.is_insert());
}

#[test]
fn worker_stale_results_changed_drafts_and_target_rebinding_are_isolated() {
    let mut app = worker();
    let control = feedback(&mut app, "feedback");
    for mismatch in ["request", "turn", "run", "worker", "action"] {
        let mut stale = control.clone();
        match mismatch {
            "request" => stale.request_id = WorkerControlId::new(),
            "turn" => stale.target.turn_id = TurnId::new(99),
            "run" => stale.target.run_id = EnsembleRunId::new(),
            "worker" => stale.target.worker_id = AgentRunId::new(),
            _ => stale.action = WorkerControlAction::Retry,
        }
        for accepted in [false, true] {
            app.worker_control_result(&WorkerControlResult {
                control: stale.clone(),
                accepted,
                detail: "stale".into(),
            });
            assert_eq!(app.composer.text(), "feedback");
            assert_eq!(app.worker.pending.as_ref().unwrap().0, control);
            assert_eq!(
                app.worker.handoff.as_ref().unwrap().request_id,
                control.request_id
            );
        }
    }
    assert!(app.interaction.is_normal());
    app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    app.composer.replace("changed draft".into(), 4);
    app.worker_control_result(&WorkerControlResult {
        control: control.clone(),
        accepted: true,
        detail: "accepted".into(),
    });
    assert_eq!(app.composer.text(), "changed draft");
    assert_eq!(app.composer.cursor(), 4);
    assert!(app.interaction.is_insert());
    assert!(app.worker_busy());
    let (target, state) = worker().worker.bound.unwrap();
    app.bind_worker_review(target, state);
    assert!(
        !app.worker_busy(),
        "a foreign binding cannot inherit the handoff"
    );
    assert!(app.worker.handoff.is_none());
    app.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "duplicate".into(),
    });
    assert_eq!(app.composer.text(), "changed draft");

    // Rebinding while acknowledgement itself is pending also retires the old
    // request without letting its later result discard identical new text.
    if !app.interaction.is_insert() {
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    }
    app.composer.clear();
    let control = feedback(&mut app, "same text");
    let (target, state) = worker().worker.bound.unwrap();
    app.bind_worker_review(target, state);
    assert!(!app.worker_busy());
    app.worker_control_result(&WorkerControlResult {
        control,
        accepted: true,
        detail: "obsolete".into(),
    });
    assert_eq!(app.composer.text(), "same text");
}

#[test]
fn worker_terminal_bindings_retire_handoff_but_only_freeze_retires_pending_ack() {
    for terminal in ["abandoned", "sealed", "freeze"] {
        let mut app = worker();
        let control = feedback(&mut app, "retained");
        if terminal == "freeze" {
            app.freeze_worker();
            assert!(app.worker.pending.is_none());
        } else {
            let (target, mut state) = app.worker.bound.clone().unwrap();
            if terminal == "abandoned" {
                state
                    .apply(&WorkerReviewEvent::Abandoned {
                        request_id: WorkerControlId::new(),
                    })
                    .unwrap();
            } else {
                confirm_fixture(&mut app);
                state = app.worker.bound.as_ref().unwrap().1.clone();
                state.apply(&WorkerReviewEvent::Sealed).unwrap();
            }
            app.bind_worker_review(target, state);
            assert!(app.worker.pending.is_some());
        }
        assert!(app.worker.handoff.is_none());
        assert!(!app.pane.can_compose());
        assert!(!app.worker_busy());
        assert_eq!(app.composer.text(), "retained");
        app.worker_control_result(&WorkerControlResult {
            control,
            accepted: true,
            detail: "accepted".into(),
        });
        assert!(app.worker.pending.is_none());
        assert_eq!(
            app.composer.text(),
            if terminal == "freeze" { "retained" } else { "" }
        );
    }
}

#[test]
fn baseline_on_confirmed_worker_is_nonurgent_and_rejection_retains_draft() {
    let mut app = worker();
    let revision = confirm_fixture(&mut app);
    let original = app.worker.bound.as_ref().unwrap().1.confirmation.clone();
    app.composer.replace("/baseline".into(), 9);
    let Some(UiAction::WorkerControl(control)) =
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("mark confirmed worker");
    };
    assert_eq!(
        control.action,
        WorkerControlAction::Baseline {
            expected_revision: revision
        }
    );
    assert!(app.worker.handoff.is_none());
    assert!(app.worker.pending.is_some());
    assert!(
        app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
            .is_none()
    );
    app.worker_control_result(&WorkerControlResult::rejected(
        control,
        "displayed revision is stale",
    ));
    assert_eq!(app.composer.text(), "/baseline");
    assert_eq!(app.worker.bound.as_ref().unwrap().1.confirmation, original);
}

#[test]
fn worker_all_seven_popup_commands_run_highlighted_entries_with_exact_targets() {
    for (row, prefix, name) in [
        (0, "/co", "confirm"),
        (1, "/un", "unconfirm"),
        (2, "/ba", "baseline"),
        (3, "/unb", "unbaseline"),
        (4, "/re", "retry"),
        (5, "/ca", "cancel"),
        (6, "/ab", "abandon"),
    ] {
        for arrow_selected in [false, true] {
            let mut app = worker();
            let revision = if matches!(name, "unconfirm" | "unbaseline") {
                confirm_fixture(&mut app)
            } else {
                app.worker
                    .bound
                    .as_ref()
                    .unwrap()
                    .1
                    .eligible_snapshot()
                    .unwrap()
                    .revision
                    .clone()
            };
            if name == "unbaseline" {
                let (target, state) = app.worker.bound.as_ref().unwrap();
                let receipt = state.confirmation.as_ref().unwrap().clone();
                let _ = target;
                bind_event(&mut app, WorkerReviewEvent::BaselineMarked { receipt });
            }
            if name == "cancel" {
                let (target, mut state) = app.worker.bound.clone().unwrap();
                accepted_input(
                    &mut state,
                    &WorkerControl {
                        request_id: WorkerControlId::new(),
                        target: target.clone(),
                        action: WorkerControlAction::Retry,
                    },
                );
                app.bind_worker_review(target, state);
            }
            app.handle_event(Event::Paste(
                if arrow_selected { "/" } else { prefix }.into(),
            ));
            if arrow_selected {
                for _ in 0..row {
                    app.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
                }
                if row > 0 {
                    app.handle_event(key(KeyCode::Up, KeyModifiers::NONE));
                    app.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
                }
            }
            assert!(
                app.worker.pending.is_none() && app.worker.urgent.is_empty(),
                "highlighting cannot execute a control"
            );
            let Some(UiAction::WorkerControl(control)) =
                app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("popup {name}")
            };
            assert_eq!(control.target, app.worker.bound.as_ref().unwrap().0);
            assert_eq!(
                control.action,
                match name {
                    "confirm" => WorkerControlAction::Confirm {
                        expected_revision: revision
                    },
                    "unconfirm" => WorkerControlAction::Unconfirm {
                        expected_revision: revision
                    },
                    "baseline" => WorkerControlAction::Baseline {
                        expected_revision: revision
                    },
                    "unbaseline" => WorkerControlAction::Unbaseline {
                        expected_revision: revision
                    },
                    "retry" => WorkerControlAction::Retry,
                    "cancel" => WorkerControlAction::CancelPrompt,
                    _ => WorkerControlAction::Abandon,
                }
            );
            assert_eq!(app.composer.text(), format!("/{name} "));
            assert_eq!(
                app.worker_busy(),
                matches!(name, "retry" | "cancel"),
                "non-prompt controls do not acquire activity locks"
            );
            assert!(
                app.worker
                    .pending
                    .iter()
                    .chain(&app.worker.urgent)
                    .any(|(pending, _)| pending == &control)
            );
            // Move only the caret back into completion; no duplicate control
            // is emitted while the original request is pending.
            app.handle_event(key(KeyCode::Left, KeyModifiers::NONE));
            assert!(app.command_menu_active());
            assert!(
                app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                    .is_none()
            );
            assert!(
                app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                    .is_none()
            );
            assert_eq!(app.composer.text(), format!("/{name} "));
            assert!(
                app.worker
                    .pending
                    .iter()
                    .chain(&app.worker.urgent)
                    .any(|(pending, _)| pending == &control)
            );
            // Dispatch still waits for the coordinator's correlated outcome.
            app.worker_control_result(&WorkerControlResult::rejected(
                control,
                "coordinator rejected the control",
            ));
            assert!(app.worker.pending.is_none() && app.worker.urgent.is_empty());
            assert_eq!(app.composer.text(), format!("/{name} "));
        }
    }
}

#[test]
fn worker_completion_preserves_suffixes_tab_only_completes_and_no_match_enter_is_inert() {
    for suffix in ["", " argument", "雪", "\u{2003}雪", "\nmore\ntext"] {
        for (code, modifiers) in [
            (KeyCode::Tab, KeyModifiers::NONE),
            (KeyCode::Char('i'), KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::NONE),
        ] {
            if suffix.is_empty() && code == KeyCode::Enter {
                continue;
            }
            let mut app = worker();
            app.composer.replace(format!("/re{suffix}"), 3);
            assert!(app.command_menu_active());
            assert!(app.handle_event(key(code, modifiers)).is_none());
            let space = if suffix.starts_with(char::is_whitespace) {
                ""
            } else {
                " "
            };
            assert_eq!(app.composer.text(), format!("/retry{space}{suffix}"));
            assert_eq!(
                app.composer.cursor(),
                6 + if space.is_empty() {
                    suffix.chars().next().unwrap().len_utf8()
                } else {
                    1
                }
            );
            assert!(app.worker.pending.is_none());
            assert!(!app.worker_busy());
        }
    }
    for draft in ["/no-match", "//x"] {
        let mut app = worker();
        app.handle_event(Event::Paste(draft.into()));
        assert!(app.command_menu_active());
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.composer.text(), draft);
        if draft == "//x" {
            assert!(
                matches!(app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL)), Some(UiAction::WorkerControl(WorkerControl { action: WorkerControlAction::SendFeedback { text }, .. })) if text == zevria_content::UserPrompt::from_text("/x"))
            );
        }
    }
    let mut app = worker();
    app.handle_event(Event::Paste("/re".into()));
    app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.command_menu_active());
    assert!(app.interaction.is_insert());
    app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.composer.text(), "\n");
    app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.interaction.is_normal());
}

#[test]
fn worker_busy_ctrl_c_clears_first_then_cancels_locally_but_slash_submission_is_locked() {
    for draft in [
        "/confirm",
        "/unconfirm",
        "/baseline",
        "/unbaseline",
        "/retry",
        "//literal",
    ] {
        let mut app = worker();
        let control = feedback(&mut app, "pending feedback");
        assert!(app.interaction.is_normal());
        app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        app.composer.replace(draft.into(), draft.len());
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(app.submit_worker().is_none());
        assert_eq!(app.composer.text(), draft);
        assert!(app.interaction.is_insert());
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(
            app.composer.text(),
            if draft == "//literal" {
                draft.into()
            } else {
                format!("{draft} ")
            }
        );
        assert!(
            app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(app.composer.text().is_empty());
        let Some(UiAction::WorkerControl(cancel)) =
            app.handle_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("worker-local cancellation")
        };
        assert_eq!(cancel.target, control.target);
        assert_eq!(cancel.action, WorkerControlAction::CancelPrompt);
        assert_eq!(app.worker.pending.as_ref().unwrap().0, control);
        assert!(app.worker_busy());
    }
}

#[test]
fn worker_popup_enter_preserves_images_and_frozen_drafts() {
    for name in [
        "confirm",
        "unconfirm",
        "baseline",
        "unbaseline",
        "retry",
        "cancel",
        "abandon",
    ] {
        let mut app = worker();
        let command = format!("/{name}");
        app.composer.insert_text(&command);
        app.composer
            .attach_image(zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap())
            .unwrap();
        while app.composer.cursor() > command.len() {
            app.composer.move_left();
        }
        let image = app.composer.prompt().images().next().unwrap().clone();
        assert!(app.command_menu_active());
        assert_eq!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(app.composer.text().starts_with(&format!("{command} ")));
        assert_eq!(
            app.composer.prompt().images().collect::<Vec<_>>(),
            vec![&image]
        );
        assert!(app.worker.pending.is_none() && app.worker.urgent.is_empty());
        assert!(app.history().is_empty());

        app.composer.replace(command.clone(), command.len());
        app.freeze_worker();
        let frozen = app.composer.snapshot();
        assert!(!app.command_menu_active());
        assert_eq!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(app.composer.matches_draft(&frozen));
        assert!(app.worker.pending.is_none() && app.worker.urgent.is_empty());
    }
}

#[test]
fn worker_quiescent_statuses_allow_i_but_inspect_and_root_capabilities_stay_restricted() {
    for status in ["feedback", "confirmation", "confirmed", "blocked"] {
        let mut app = worker();
        if status == "confirmed" {
            confirm_fixture(&mut app);
        }
        let (target, mut state) = app.worker.bound.clone().unwrap();
        if status == "feedback" || status == "blocked" {
            state.retained = None;
            state.connected = status == "feedback";
        }
        app.bind_worker_review(target, state);
        app.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
        if !app.interaction.is_insert() {
            app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        }
        assert!(app.composer_editable(), "{status}");
        let mode = app.session.next_mode();
        app.handle_event(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.session.next_mode(), mode);
        assert!(app.begin_model_management().is_none());
        for forbidden in [
            "/compact",
            "/build",
            "/orchestrate",
            "/plan",
            "/ensemble-plan hidden",
            "$skill",
        ] {
            app.composer.replace(forbidden.into(), forbidden.len());
            assert!(
                app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                    .is_none()
            );
            assert_eq!(app.composer.text(), forbidden);
        }
        app.composer.clear();
        app.conversation.seed_entry(
            HistoryEntry::from_message(Message::user("not replaceable"), ToolCallStatus::Finished)
                .unwrap(),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let now = Instant::now();
        app.handle_event_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
        app.handle_event_at(
            key(KeyCode::Esc, KeyModifiers::NONE),
            now + Duration::from_millis(100),
        );
        assert!(app.interaction.is_selecting());
        assert!(
            app.handle_event(key(KeyCode::Char('e'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(!app.edit.is_recalling());
    }
    for mut app in [
        App::acp_inspect("unbound ACP"),
        App::subtask_inspect("Explore"),
        {
            let mut app = worker();
            app.freeze_worker();
            app
        },
    ] {
        if !app.interaction.is_insert() {
            app.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
        }
        app.handle_event(Event::Paste("forbidden".into()));
        assert!(
            app.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(app.composer.text().is_empty());
        assert!(!app.composer_editable());
    }
}

#[test]
fn published_plan_has_no_approval_dialog_and_keeps_explicit_mode() {
    let mut app = App::new();
    app.apply_selected_mode(SessionMode::Build);
    let artifact = zevria_workflow::PlanArtifact {
        version: PlanVersion {
            id: zevria_workflow::PlanId::new(),
            revision: 1,
        },
        title: "Published plan".into(),
        markdown: "# Published plan".into(),
        source_turn_id: TurnId::new(1),
    };
    app.apply_plan_snapshot(zevria_workflow::PlanWorkflowState::Published {
        artifact: artifact.clone(),
    });
    assert!(app.workflow.dialog().is_none());
    assert_eq!(app.session.next_mode(), SessionMode::Build);
    assert_eq!(app.workflow.submitted_artifact(), Some(&artifact));
}
