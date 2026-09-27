use super::*;

#[test]
fn ensemble_abandonment_is_terminal_in_every_unsealed_phase() {
    let mut phases = vec![state(), proposal()];
    let mut queued = state();
    accept(&mut queued, "initial");
    phases.push(queued.clone());
    dispatch(&mut queued);
    accept(&mut queued, "discard queued feedback");
    phases.push(queued);
    let mut blocked = proposal();
    accept(&mut blocked, "failed feedback");
    dispatch(&mut blocked);
    settle(&mut blocked, Some("provider failure sentinel"));
    phases.push(blocked);
    let mut confirmed = proposal();
    confirmed
        .apply(&WorkerReviewEvent::Confirmed {
            receipt: receipt(&confirmed),
        })
        .unwrap();
    phases.push(confirmed);
    for mut state in phases {
        let before = state.clone();
        let event = WorkerReviewEvent::Abandoned {
            request_id: WorkerControlId::new(),
        };
        state.apply(&event).unwrap();
        assert!(state.abandoned);
        assert!(state.status().is_terminal());
        assert_eq!(state.status(), AgentRunStatus::Abandoned);
        assert!(state.quiescent());
        assert!(state.cancellable_generation().is_none());
        assert!(state.eligible_snapshot().is_none());
        assert!(state.confirmed_plan().is_none());
        assert!(state.confirmation.is_none());
        assert!(!state.connected);
        assert_eq!(state.evidence, before.evidence);
        assert_eq!(state.retained, before.retained);
        assert_eq!(state.candidate, before.candidate);
        assert!(state.outcome().is_sanitized_abandonment());
        let frozen = state.clone();
        let transitions = vec![
            event.clone(),
            WorkerReviewEvent::Sealed,
            WorkerReviewEvent::Withdrawn {
                request_id: WorkerControlId::new(),
            },
            WorkerReviewEvent::CancelRequested { generation: 1 },
            WorkerReviewEvent::Connection {
                connected: true,
                diagnostic: None,
            },
            WorkerReviewEvent::Fatal {
                error: "late fatal".into(),
            },
            WorkerReviewEvent::PayloadChecked { error: None },
            WorkerReviewEvent::Removed {
                plan_id: "provider-plan".into(),
            },
            WorkerReviewEvent::Interrupted { generation: 1 },
            WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: state.accepted_generation + 1,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::UserFeedback,
                    text: "late feedback".into(),
                },
            },
        ];
        for transition in transitions {
            assert!(state.apply(&transition).is_err());
            assert_eq!(state, frozen);
        }
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkerReviewState>(&json).unwrap(),
            state
        );
        assert_eq!(
            serde_json::from_str::<WorkerReviewEvent>(&serde_json::to_string(&event).unwrap())
                .unwrap(),
            event
        );
    }
    let mut old = serde_json::to_value(state()).unwrap();
    old.as_object_mut().unwrap().remove("abandoned");
    assert!(
        !serde_json::from_value::<WorkerReviewState>(old)
            .unwrap()
            .abandoned
    );
    for request_id in ["".to_string(), " ".into(), "a".repeat(129)] {
        assert!(
            state()
                .apply(&WorkerReviewEvent::Abandoned {
                    request_id: WorkerControlId(request_id)
                })
                .is_err()
        );
    }
}

#[test]
fn ensemble_root_abandonment_requires_an_exact_accepted_control_and_never_seals_empty() {
    use crate::{EnsembleRecord, EnsembleStart, EnsembleWorkflow, transcript::TranscriptItem};
    let state = state();
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan".into(),
        agents: vec![state.descriptor.clone()],
    };
    let request_id = WorkerControlId::new();
    let event = WorkerReviewEvent::Abandoned {
        request_id: request_id.clone(),
    };
    let result = WorkerControlResult {
        control: WorkerControl {
            request_id,
            target: WorkerControlTarget {
                turn_id: TurnId::new(1),
                run_id: start.run_id.clone(),
                worker_id: state.descriptor.id.clone(),
            },
            action: WorkerControlAction::Abandon,
        },
        accepted: true,
        detail: "last worker abandoned; ensemble cancelled".into(),
    };
    let prefix = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: ENSEMBLE_REVIEW_VERSION,
        }),
    ];
    for accepted in [None, Some(false), Some(true)] {
        let mut items = prefix.clone();
        items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
            event: Box::new(event.clone()),
            result: accepted.map(|accepted| WorkerControlResult {
                accepted,
                ..result.clone()
            }),
        }));
        assert_eq!(
            crate::validate_ensemble_review_history(&items).is_ok(),
            accepted == Some(true)
        );
    }
    let mut abandoned = state;
    abandoned.apply(&event).unwrap();
    let record = EnsembleRecord::WorkersConfirmed {
        run_id: start.run_id,
        final_confirmation: result,
        outcomes: vec![abandoned.outcome()],
    };
    assert_eq!(
        serde_json::from_str::<EnsembleRecord>(&serde_json::to_string(&record).unwrap()).unwrap(),
        record
    );
    let mut items = prefix;
    items.push(TranscriptItem::Ensemble(record));
    assert!(crate::validate_ensemble_review_history(&items).is_err());
}

#[test]
fn ensemble_abandoned_evidence_never_reenters_synthesis_or_recovery() {
    let mut excluded = proposal();
    excluded.descriptor.label = "EXCLUDED_LABEL".into();
    excluded.evidence.descriptor = excluded.descriptor.clone();
    excluded.evidence.report = "EXCLUDED_PROSE".into();
    excluded.evidence.failure = Some("EXCLUDED_PROVIDER_FAILURE".into());
    excluded.retained.as_mut().unwrap().plan.markdown = Some("EXCLUDED_MARKDOWN".into());
    let request_id = crate::QuestionRequestId::generate();
    let decision_id = crate::AgentUserDecisionId::from_question(&request_id, "excluded");
    excluded.evidence.decision_ids.push(decision_id.clone());
    excluded
        .evidence
        .user_decisions
        .push(crate::AgentUserDecisionBatch {
            request_id: request_id.clone(),
            answers: vec![crate::AgentUserDecisionAnswer {
                decision_id: decision_id.clone(),
                question_id: "excluded".into(),
                header: "EXCLUDED_HEADER".into(),
                question: "EXCLUDED_QUESTION".into(),
                answer: crate::AgentUserDecisionValue::String {
                    value: "EXCLUDED_ANSWER".into(),
                },
            }],
        });
    let unavailable = crate::AgentUnavailableDecision::normalized_payload_too_large(request_id, 2);
    excluded
        .evidence
        .unavailable_decisions
        .push(unavailable.clone());
    let archived = excluded.evidence.clone();
    excluded
        .apply(&WorkerReviewEvent::Abandoned {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    let outcome = excluded.outcome();
    assert!(outcome.is_sanitized_abandonment());
    let mut survivor = proposal();
    survivor
        .apply(&WorkerReviewEvent::Confirmed {
            receipt: receipt(&survivor),
        })
        .unwrap();
    let input = crate::build_synthesis_input(
        crate::EnsembleWorkflow::Plan,
        "plan",
        &[outcome.clone(), survivor.outcome()],
        usize::MAX,
    )
    .unwrap();
    assert!(!input.contains("EXCLUDED"));
    assert!(!input.contains(decision_id.as_str()));
    assert!(!input.contains(unavailable.id.as_str()));
    assert!(input.contains("# Exact plan"));
    assert!(
        crate::build_synthesis_input(
            crate::EnsembleWorkflow::Plan,
            "plan",
            std::slice::from_ref(&outcome),
            usize::MAX
        )
        .is_err()
    );
    let mut malformed = outcome.clone();
    malformed.report = "EXCLUDED".into();
    assert!(!malformed.is_sanitized_abandonment());
    assert!(
        crate::build_synthesis_input(
            crate::EnsembleWorkflow::Plan,
            "plan",
            &[malformed, survivor.outcome()],
            usize::MAX
        )
        .is_err()
    );
    let mut projection = crate::AgentRunProjection::default();
    projection.apply(&crate::AgentRunTranscriptRecord::Outcome { outcome: archived });
    projection.apply(&crate::AgentRunTranscriptRecord::Outcome {
        outcome: outcome.clone(),
    });
    assert!(!projection.decision_ids.is_empty());
    assert_eq!(projection.recoverable_outcome(), Some(outcome.clone()));
    let mut summary = outcome.summary();
    summary.decision_ids = projection.decision_ids;
    assert!(
        crate::ReportReconciliationCatalog::from_summaries(&[summary])
            .unwrap()
            .decision_ids
            .is_empty()
    );
}
