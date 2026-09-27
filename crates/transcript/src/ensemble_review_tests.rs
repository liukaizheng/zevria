use super::*;
use crate::{
    AgentRunDescriptor, AgentRunId, AgentRunOutcome, AgentRunStatus, AgentStructuredPlan,
    EnsembleRunId, TurnId,
};

#[path = "ensemble_abandon_tests.rs"]
mod abandonment;

#[path = "ensemble_baseline_tests.rs"]
mod baseline;

#[path = "ensemble_direct_plan_tests.rs"]
mod direct_plan;

fn state() -> WorkerReviewState {
    WorkerReviewState::new(AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "worker".into(),
        label: "Worker".into(),
        safe_mode: "read-only".into(),
    })
}
fn accept(state: &mut WorkerReviewState, text: &str) -> WorkerInput {
    let input = WorkerInput {
        generation: state.accepted_generation + 1,
        request_id: WorkerControlId::new(),
        kind: if state.accepted_generation == 0 {
            WorkerPromptKind::Initial
        } else {
            WorkerPromptKind::UserFeedback
        },
        text: text.into(),
    };
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        })
        .unwrap();
    input
}
fn dispatch(state: &mut WorkerReviewState) {
    state
        .apply(&WorkerReviewEvent::Dispatched {
            generation: state.pending.front().unwrap().generation,
            attempt: state.attempt + 1,
        })
        .unwrap();
}
fn publish(state: &mut WorkerReviewState, markdown: &str, replay: bool) {
    state
        .apply(&WorkerReviewEvent::Published {
            generation: state.active.as_ref().unwrap().generation,
            plan: AgentStructuredPlan {
                plan_id: Some("provider-plan".into()),
                markdown: Some(markdown.into()),
                entries: Vec::new(),
            },
            replay,
        })
        .unwrap();
}
fn settle(state: &mut WorkerReviewState, error: Option<&str>) {
    state
        .apply(&WorkerReviewEvent::Settled {
            generation: state.active.as_ref().unwrap().generation,
            failure: error.map(str::to_string),
            connected: error.is_none(),
            evidence: Box::new(state.evidence.clone()),
        })
        .unwrap();
}
fn proposal() -> WorkerReviewState {
    let mut state = state();
    accept(&mut state, "initial");
    dispatch(&mut state);
    publish(&mut state, "# Exact plan\n", false);
    settle(&mut state, None);
    state
}
fn receipt(state: &WorkerReviewState) -> WorkerConfirmationReceipt {
    WorkerConfirmationReceipt {
        request_id: WorkerControlId::new(),
        target: WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: EnsembleRunId::new(),
            worker_id: state.descriptor.id.clone(),
        },
        revision: state.eligible_snapshot().unwrap().revision.clone(),
    }
}
#[test]
fn failed_image_retry_preserves_bytes_and_only_success_becomes_evidence() {
    let mut state = proposal();
    let image = crate::PromptImage::from_rgba(1, 1, &[4, 3, 2, 255]).unwrap();
    let feedback = WorkerInput {
        generation: 2,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: crate::UserPrompt::new(vec![crate::PromptBlock::Image(image)]).unwrap(),
    };
    state
        .apply(&WorkerReviewEvent::ImageCapability { supported: true })
        .unwrap();
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: feedback.clone(),
        })
        .unwrap();
    dispatch(&mut state);
    settle(&mut state, Some("agent changed image capability"));
    assert_eq!(state.failed_image_input.as_ref(), Some(&feedback));
    assert_eq!(state.image_capability, None);
    assert!(state.incorporated_images.is_empty());
    let recovered: WorkerReviewState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(recovered, state);
    let mut retry = WorkerInput {
        generation: 3,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::RecoveryContinuation,
        text: "continue".into(),
    };
    assert!(
        state
            .apply(&WorkerReviewEvent::InputAccepted {
                input: retry.clone()
            })
            .is_err(),
        "allocated session must not allow a text-only downgrade"
    );
    retry.text = feedback.text.clone();
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: retry.clone(),
        })
        .unwrap();
    assert_eq!(
        crate::ensemble::potential_synthesis_images(&"initial".into(), &[state.clone()], None)
            .unwrap()
            .images()
            .count(),
        1
    );
    dispatch(&mut state);
    publish(&mut state, "# Image incorporated by retry", false);
    settle(&mut state, None);
    assert!(state.failed_image_input.is_none());
    assert_eq!(state.incorporated_images, vec![retry]);
    assert_eq!(
        crate::ensemble::potential_synthesis_images(&"initial".into(), &[state], None)
            .unwrap()
            .images()
            .count(),
        1
    );
}

#[test]
fn synthesis_images_follow_confirmed_incorporation_not_raw_or_abandoned_inputs() {
    fn image(value: u8) -> crate::PromptImage {
        crate::PromptImage::from_rgba(1, 1, &[value, 2, 3, 255]).unwrap()
    }
    fn feedback(state: &mut WorkerReviewState, value: u8, failure: bool) {
        let input = WorkerInput {
            generation: state.accepted_generation + 1,
            request_id: WorkerControlId::new(),
            kind: WorkerPromptKind::UserFeedback,
            text: crate::UserPrompt::new(vec![crate::PromptBlock::Image(image(value))]).unwrap(),
        };
        state
            .apply(&WorkerReviewEvent::InputAccepted { input })
            .unwrap();
        dispatch(state);
        publish(state, "# Image-informed proposal", false);
        settle(state, failure.then_some("not incorporated"));
    }
    let mut participating = proposal();
    feedback(&mut participating, 10, false);
    feedback(&mut participating, 20, true);
    let confirmation = receipt(&participating);
    participating
        .apply(&WorkerReviewEvent::Confirmed {
            receipt: confirmation,
        })
        .unwrap();
    let mut abandoned = proposal();
    feedback(&mut abandoned, 30, false);
    abandoned
        .apply(&WorkerReviewEvent::Abandoned {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    let original = crate::UserPrompt::new(vec![
        crate::PromptBlock::Image(image(1)),
        crate::PromptBlock::Text(" twice ".into()),
        crate::PromptBlock::Image(image(1)),
    ])
    .unwrap();
    let outcomes = vec![participating.outcome(), abandoned.outcome()];
    let states = vec![participating, abandoned];
    let message = crate::ensemble::build_synthesis_prompt_with_feedback(
        crate::EnsembleWorkflow::Plan,
        &original,
        &outcomes,
        &states,
        usize::MAX,
    )
    .unwrap();
    let prompt = crate::UserPrompt::from_message(&message).unwrap();
    let images = prompt.images().cloned().collect::<Vec<_>>();
    assert_eq!(images, vec![image(1), image(1), image(10)]);
    assert!(prompt.text_projection().contains("generation 2"));
    assert!(!prompt.text_projection().contains(&image(10).base64()));
    let overflow = crate::UserPrompt::new(vec![crate::PromptBlock::Image(image(2)); 8]).unwrap();
    assert!(
        crate::ensemble::validate_synthesis_image_budget(&original, &states, Some(&overflow))
            .is_err()
    );
}

#[test]
fn ensemble_proposal_and_prompt_end_are_not_confirmation() {
    let state = proposal();
    assert_eq!(state.status(), AgentRunStatus::AwaitingConfirmation);
    assert!(!state.status().is_terminal());
    assert!(state.confirmed_plan().is_none());
    assert!(state.outcome().confirmation.is_none());
}
#[test]
fn ensemble_confirmation_is_revocable_and_revision_bound() {
    let mut state = proposal();
    let receipt = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed {
            receipt: receipt.clone(),
        })
        .unwrap();
    assert_eq!(state.status(), AgentRunStatus::Confirmed);
    assert!(!state.status().is_terminal());
    assert!(
        state
            .apply(&WorkerReviewEvent::Confirmed {
                receipt: receipt.clone()
            })
            .is_err()
    );
    accept(&mut state, "change it");
    assert!(state.confirmation.is_none());
    assert!(
        state
            .apply(&WorkerReviewEvent::Confirmed { receipt })
            .is_err()
    );
}
#[test]
fn ensemble_queued_feedback_blocks_confirmation_without_overlap() {
    let mut state = proposal();
    accept(&mut state, "first");
    dispatch(&mut state);
    accept(&mut state, "second");
    let before = state.clone();
    assert!(
        state
            .apply(&WorkerReviewEvent::Dispatched {
                generation: 3,
                attempt: 2
            })
            .is_err()
    );
    assert_eq!(state, before);
    publish(&mut state, "# First revision", false);
    settle(&mut state, None);
    assert!(state.eligible_snapshot().is_none());
    dispatch(&mut state);
    publish(&mut state, "# Second revision", false);
    settle(&mut state, None);
    assert_eq!(state.eligible_snapshot().unwrap().revision.generation, 3);
}
#[test]
fn ensemble_successful_prose_requires_fresh_publication_and_replay_does_not_qualify() {
    let mut state = proposal();
    accept(&mut state, "discuss");
    dispatch(&mut state);
    settle(&mut state, None);
    assert_eq!(state.status(), AgentRunStatus::AwaitingFeedback);
    accept(&mut state, "republish");
    dispatch(&mut state);
    publish(&mut state, "# Exact plan\n", true);
    settle(&mut state, None);
    assert!(state.eligible_snapshot().is_none());
    accept(&mut state, "intentionally republish");
    dispatch(&mut state);
    publish(&mut state, "# Exact plan\n", false);
    settle(&mut state, None);
    let snapshot = state.eligible_snapshot().unwrap();
    assert_eq!(snapshot.revision.generation, 4);
    assert_eq!(snapshot.revision.revision, 2);
    assert_eq!(snapshot.plan.markdown.as_deref(), Some("# Exact plan\n"));
}
#[test]
fn ensemble_failed_followup_restores_only_preceding_eligible_snapshot_without_confirmation() {
    let mut state = proposal();
    let old = state.retained.clone();
    let receipt = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt })
        .unwrap();
    accept(&mut state, "feedback");
    dispatch(&mut state);
    publish(&mut state, "# Failed round draft", false);
    settle(&mut state, Some("provider timeout"));
    assert_eq!(state.retained, old);
    assert_eq!(state.eligible_snapshot(), old.as_ref());
    assert!(state.confirmation.is_none());
    assert!(!state.connected);
    assert!(
        state
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not incorporated")
    );
}
#[test]
fn ensemble_failure_never_erases_prior_successful_prose_freshness_requirement() {
    let mut state = proposal();
    accept(&mut state, "discuss");
    dispatch(&mut state);
    settle(&mut state, None);
    accept(&mut state, "revise");
    dispatch(&mut state);
    publish(&mut state, "# Partial failed revision", false);
    settle(&mut state, Some("cancelled"));
    assert_eq!(state.publication_floor, 2);
    assert!(state.eligible_snapshot().is_none());
}
#[test]
fn ensemble_failed_followup_cannot_bypass_later_queued_feedback() {
    let mut state = proposal();
    accept(&mut state, "failing");
    dispatch(&mut state);
    accept(&mut state, "queued");
    settle(&mut state, Some("crash"));
    assert!(state.eligible_snapshot().is_none());
    dispatch(&mut state);
    settle(&mut state, Some("another failure"));
    assert!(state.eligible_snapshot().is_some());
}
#[test]
fn ensemble_matching_removal_does_not_resurrect_historical_plan() {
    let mut state = proposal();
    state
        .apply(&WorkerReviewEvent::Removed {
            plan_id: "unrelated".into(),
        })
        .unwrap();
    assert!(state.eligible_snapshot().is_some());
    state
        .apply(&WorkerReviewEvent::Removed {
            plan_id: "provider-plan".into(),
        })
        .unwrap();
    assert!(state.eligible_snapshot().is_none());
    accept(&mut state, "retry");
    dispatch(&mut state);
    settle(&mut state, Some("crash"));
    assert!(state.eligible_snapshot().is_none());
}
#[test]
fn ensemble_sealed_workers_reject_all_later_review_actions() {
    let mut state = proposal();
    assert!(state.apply(&WorkerReviewEvent::Sealed).is_err());
    let receipt = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt })
        .unwrap();
    state.apply(&WorkerReviewEvent::Sealed).unwrap();
    assert_eq!(state.status(), AgentRunStatus::Completed);
    assert!(state.confirmed_plan().is_some());
    assert!(
        state.outcome().confirmation.is_some(),
        "sealed projections retain exact consent"
    );
    let before = state.clone();
    assert!(
        state
            .apply(&WorkerReviewEvent::Withdrawn {
                request_id: WorkerControlId::new()
            })
            .is_err()
    );
    assert_eq!(before, state);
}
#[test]
fn ensemble_review_event_serialization_and_replay_agree() {
    let mut live = state();
    let mut replay = live.clone();
    let input = WorkerInput {
        generation: 1,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::Initial,
        text: "private discussion".into(),
    };
    let events = vec![
        WorkerReviewEvent::InputAccepted { input },
        WorkerReviewEvent::Dispatched {
            generation: 1,
            attempt: 1,
        },
        WorkerReviewEvent::Published {
            generation: 1,
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: Some("# Plan".into()),
                entries: vec![],
            },
            replay: false,
        },
        WorkerReviewEvent::Settled {
            generation: 1,
            failure: None,
            connected: true,
            evidence: Box::new(live.evidence.clone()),
        },
    ];
    for event in events {
        let encoded = serde_json::to_string(&event).unwrap();
        live.apply(&event).unwrap();
        replay
            .apply(&serde_json::from_str(&encoded).unwrap())
            .unwrap();
        assert_eq!(live, replay);
    }
}
#[test]
fn ensemble_cancellation_intent_survives_replay_and_cannot_cancel_the_next_input() {
    for interrupted in [false, true] {
        let mut state = proposal();
        let retained = state.retained.clone();
        let receipt = receipt(&state);
        state
            .apply(&WorkerReviewEvent::Confirmed { receipt })
            .unwrap();
        accept(&mut state, "cancel this feedback");
        let cancel = WorkerReviewEvent::CancelRequested { generation: 2 };
        state
            .apply(&serde_json::from_str(&serde_json::to_string(&cancel).unwrap()).unwrap())
            .unwrap();
        assert_eq!(state.cancel_requested, Some(2));
        assert!(state.confirmation.is_none());
        let mut restored: WorkerReviewState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        accept(&mut restored, "keep this successor");
        assert_eq!(restored.cancel_requested, Some(2));
        dispatch(&mut restored);
        assert_eq!(restored.cancel_requested, Some(2));
        if interrupted {
            restored
                .apply(&WorkerReviewEvent::Interrupted { generation: 2 })
                .unwrap();
        } else {
            settle(&mut restored, Some("cancelled before dispatch"));
        }
        assert_eq!(restored.cancel_requested, None);
        assert_eq!(restored.retained, retained);
        assert!(restored.confirmation.is_none());
        assert!(
            restored.eligible_snapshot().is_none(),
            "cancellation cannot bypass queued feedback"
        );
        let before = restored.clone();
        assert!(
            restored.apply(&cancel).is_err(),
            "stale cancellation cannot affect generation 3"
        );
        assert_eq!(restored, before);
        dispatch(&mut restored);
        publish(&mut restored, "# Successor", false);
        settle(&mut restored, None);
        assert_eq!(restored.eligible_snapshot().unwrap().revision.generation, 3);
        assert!(
            restored
                .apply(&WorkerReviewEvent::CancelRequested { generation: 3 })
                .is_err()
        );
    }
}

#[test]
fn ensemble_cancellation_history_requires_a_correlated_explicit_host_control() {
    use crate::{EnsembleRecord, EnsembleStart, EnsembleWorkflow, transcript::TranscriptItem};
    let mut state = state();
    let input = accept(&mut state, "initial");
    let run_id = EnsembleRunId::new();
    let base = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: EnsembleStart {
                run_id: run_id.clone(),
                workflow: EnsembleWorkflow::Plan,
                prompt: input.text.clone(),
                agents: vec![state.descriptor.clone()],
            },
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: run_id.clone(),
            version: ENSEMBLE_REVIEW_VERSION,
        }),
        TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: run_id.clone(),
            worker_id: state.descriptor.id.clone(),
            event: Box::new(WorkerReviewEvent::InputAccepted { input }),
            result: None,
        }),
    ];
    let result = WorkerControlResult {
        control: WorkerControl {
            request_id: WorkerControlId::new(),
            target: WorkerControlTarget {
                turn_id: TurnId::new(1),
                run_id: run_id.clone(),
                worker_id: state.descriptor.id.clone(),
            },
            action: WorkerControlAction::CancelPrompt,
        },
        accepted: true,
        detail: "Cancellation requested".into(),
    };
    for (generation, result, valid) in [
        (1, Some(result.clone()), true),
        (1, None, false),
        (2, Some(result.clone()), false),
        (
            1,
            Some(WorkerControlResult {
                control: WorkerControl {
                    action: WorkerControlAction::Retry,
                    ..result.control.clone()
                },
                ..result.clone()
            }),
            false,
        ),
        (
            1,
            Some(WorkerControlResult {
                control: WorkerControl {
                    request_id: WorkerControlId("".into()),
                    ..result.control.clone()
                },
                ..result.clone()
            }),
            false,
        ),
    ] {
        let mut items = base.clone();
        items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: run_id.clone(),
            worker_id: state.descriptor.id.clone(),
            event: Box::new(WorkerReviewEvent::CancelRequested { generation }),
            result,
        }));
        assert_eq!(
            crate::validate_ensemble_review_history(&items).is_ok(),
            valid
        );
    }
    let mut legacy_current_version = base;
    legacy_current_version.push(TranscriptItem::Ensemble(EnsembleRecord::ControlResult {
        run_id,
        result,
    }));
    crate::validate_ensemble_review_history(&legacy_current_version).unwrap();
}

#[test]
fn ensemble_checklist_is_not_a_markdown_publication() {
    let mut state = state();
    accept(&mut state, "plan");
    dispatch(&mut state);
    state
        .apply(&WorkerReviewEvent::Published {
            generation: 1,
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: None,
                entries: vec![crate::AgentPlanEntry {
                    content: "done".into(),
                    priority: "high".into(),
                    status: "completed".into(),
                }],
            },
            replay: false,
        })
        .unwrap();
    settle(&mut state, None);
    assert!(state.eligible_snapshot().is_none());
}
#[test]
fn ensemble_synthesis_excludes_free_form_discussion() {
    let mut state = proposal();
    state.evidence.report = "SECRET EARLIER DISCUSSION".into();
    let receipt = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt })
        .unwrap();
    let input = crate::build_synthesis_input(
        crate::EnsembleWorkflow::Plan,
        "request",
        &[state.outcome()],
        16_384,
    )
    .unwrap();
    assert!(!input.contains("SECRET EARLIER DISCUSSION"));
    assert!(input.contains("# Exact plan"));
    assert!(input.contains("confirmation"));
}
