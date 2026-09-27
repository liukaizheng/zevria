use super::*;
use crate::EnsembleWorkflow;

fn mark(state: &mut WorkerReviewState) -> WorkerConfirmationReceipt {
    let mut marking = receipt(state);
    if let Some(original) = &state.confirmation {
        marking.target.run_id = original.target.run_id.clone();
        marking.target.turn_id = TurnId::new(original.target.turn_id.get() + 1);
    }
    state
        .apply(&WorkerReviewEvent::BaselineMarked {
            receipt: marking.clone(),
        })
        .unwrap();
    marking
}

#[test]
fn marking_confirms_and_preserves_independent_receipt_identities() {
    let mut state = proposal();
    let marking = mark(&mut state);
    assert_eq!(state.confirmation.as_ref(), Some(&marking));
    assert_eq!(state.baseline.as_ref(), Some(&marking));
    assert_eq!(
        state.outcome().confirmation.unwrap().baseline,
        Some(marking.clone())
    );
    state
        .apply(&WorkerReviewEvent::BaselineCleared {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    assert_eq!(state.confirmation.as_ref(), Some(&marking));
    let later = mark(&mut state);
    assert_ne!(later.target.turn_id, marking.target.turn_id);
    assert_eq!(state.confirmation.as_ref(), Some(&marking));
    assert_eq!(
        state.confirmed_plan().unwrap().baseline,
        Some(later.clone())
    );
    let frozen = state.outcome();
    state
        .apply(&WorkerReviewEvent::Connection {
            connected: false,
            diagnostic: Some("offline".into()),
        })
        .unwrap();
    assert_eq!(state.outcome(), frozen);
    state.apply(&WorkerReviewEvent::Sealed).unwrap();
    assert_eq!(state.confirmed_plan().unwrap().baseline, Some(later));
    assert!(
        state
            .apply(&WorkerReviewEvent::BaselineCleared {
                request_id: WorkerControlId::new()
            })
            .is_err()
    );
}

#[test]
fn run_transfer_is_atomic_and_never_withdraws_old_confirmation() {
    let mut states = vec![proposal(), proposal(), proposal()];
    let first = mark(&mut states[0]);
    let mut second = receipt(&states[1]);
    second.target.run_id = first.target.run_id.clone();
    let before = states.clone();
    let mut stale = second.clone();
    stale.revision.revision += 1;
    let id = states[1].descriptor.id.clone();
    assert!(
        apply_worker_review_event(
            &mut states,
            &id,
            &WorkerReviewEvent::BaselineMarked { receipt: stale }
        )
        .is_err()
    );
    assert_eq!(states, before);
    let affected = apply_worker_review_event(
        &mut states,
        &id,
        &WorkerReviewEvent::BaselineMarked {
            receipt: second.clone(),
        },
    )
    .unwrap();
    assert_eq!(affected, vec![id.clone(), states[0].descriptor.id.clone()]);
    assert_eq!(states[0].confirmation, Some(first));
    assert!(states[0].baseline.is_none());
    assert_eq!(states[1].confirmation, Some(second.clone()));
    assert_eq!(states[1].baseline, Some(second));
    assert!(states[2].confirmation.is_none());
    let before = states.clone();
    let repeated = receipt(&states[1]);
    assert!(
        apply_worker_review_event(
            &mut states,
            &id,
            &WorkerReviewEvent::BaselineMarked { receipt: repeated }
        )
        .is_err()
    );
    assert_eq!(states, before);
    let id = states[0].descriptor.id.clone();
    assert!(
        apply_worker_review_event(
            &mut states,
            &id,
            &WorkerReviewEvent::BaselineCleared {
                request_id: WorkerControlId::new()
            }
        )
        .is_err()
    );
    assert_eq!(states, before);
}

#[test]
fn every_confirmation_invalidation_also_clears_baseline_without_automatic_restoration() {
    for kind in ["feedback", "remove", "payload", "withdraw", "abandon"] {
        let mut state = proposal();
        mark(&mut state);
        match kind {
            "feedback" => {
                accept(&mut state, "change it");
                dispatch(&mut state);
                settle(&mut state, Some("failed feedback"));
                assert!(state.eligible_snapshot().is_some());
            }
            "remove" => state
                .apply(&WorkerReviewEvent::Removed {
                    plan_id: "provider-plan".into(),
                })
                .unwrap(),
            "payload" => state
                .apply(&WorkerReviewEvent::PayloadChecked {
                    error: Some("oversize".into()),
                })
                .unwrap(),
            "withdraw" => state
                .apply(&WorkerReviewEvent::Withdrawn {
                    request_id: WorkerControlId::new(),
                })
                .unwrap(),
            "abandon" => state
                .apply(&WorkerReviewEvent::Abandoned {
                    request_id: WorkerControlId::new(),
                })
                .unwrap(),
            _ => unreachable!(),
        }
        assert!(state.confirmation.is_none(), "{kind}");
        assert!(state.baseline.is_none(), "{kind}");
        assert!(state.outcome().confirmation.is_none(), "{kind}");
    }
}

#[test]
fn baseline_validation_and_optional_v2_serialization_are_exact() {
    let mut state = proposal();
    let unmarked = serde_json::to_value(&state).unwrap();
    assert!(unmarked.get("baseline").is_none());
    assert_eq!(
        serde_json::from_value::<WorkerReviewState>(unmarked).unwrap(),
        state
    );
    let original = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt: original })
        .unwrap();
    let no_baseline = serde_json::to_value(state.confirmed_plan().unwrap()).unwrap();
    assert!(no_baseline.get("baseline").is_none());
    assert!(
        serde_json::from_value::<ConfirmedWorkerPlan>(no_baseline)
            .unwrap()
            .baseline
            .is_none()
    );
    let marking = mark(&mut state);
    let confirmed = state.confirmed_plan().unwrap();
    assert!(confirmed.validate(&state.descriptor.id));
    for kind in [
        "request",
        "long_request",
        "run",
        "worker",
        "revision",
        "digest",
    ] {
        let mut bad = confirmed.clone();
        let receipt = bad.baseline.as_mut().unwrap();
        match kind {
            "request" => receipt.request_id.0.clear(),
            "long_request" => receipt.request_id.0 = "x".repeat(129),
            "run" => receipt.target.run_id = EnsembleRunId::new(),
            "worker" => receipt.target.worker_id = AgentRunId::new(),
            "revision" => receipt.revision.revision += 1,
            "digest" => receipt.revision.digest.push('x'),
            _ => unreachable!(),
        }
        assert!(!bad.validate(&state.descriptor.id), "{kind}");
    }
    for event in [
        WorkerReviewEvent::BaselineMarked { receipt: marking },
        WorkerReviewEvent::BaselineCleared {
            request_id: WorkerControlId::new(),
        },
    ] {
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkerReviewEvent>(&json).unwrap(),
            event
        );
    }
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(
        serde_json::from_str::<WorkerReviewState>(&json).unwrap(),
        state
    );
    state.confirmation = None;
    assert!(state.confirmed_plan().is_none());
    assert_eq!(ENSEMBLE_REVIEW_VERSION, 1);
    assert_eq!(crate::AGENT_RUN_TRANSCRIPT_VERSION, 1);
}

#[test]
fn synthesis_flag_and_catalog_use_exact_host_identity_not_labels() {
    let mut a = proposal();
    let mut b = proposal();
    b.descriptor.label = a.descriptor.label.clone();
    b.evidence.descriptor = b.descriptor.clone();
    mark(&mut a);
    let receipt = receipt(&b);
    b.apply(&WorkerReviewEvent::Confirmed { receipt }).unwrap();
    let outcomes = vec![a.outcome(), b.outcome()];
    let evidence =
        crate::build_synthesis_input(EnsembleWorkflow::Plan, "request", &outcomes, usize::MAX)
            .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(evidence.split_once("\n\n").unwrap().1).unwrap();
    assert_eq!(json["reports"][0]["baseline"], true);
    assert!(json["reports"][1].get("baseline").is_none());
    // The marking receipt is durable host metadata, not extra model evidence.
    assert!(json["reports"][0]["confirmation"].get("baseline").is_none());
    let catalog = crate::ReportReconciliationCatalog::from_summaries(
        &outcomes
            .iter()
            .map(AgentRunOutcome::summary)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(
        catalog.baseline.as_ref().unwrap().worker_id,
        a.descriptor.id
    );
    let declaration =
        |worker_id: String, application: &str, classification| crate::ReportReconciliation {
            disagreements: vec![crate::ReportDisagreement {
                id: "organization".into(),
                summary: "Choose viable organization".into(),
                positions: vec![
                    crate::ReportPosition {
                        label: format!("{} ({})", a.descriptor.label, a.descriptor.id),
                        position: "A".into(),
                    },
                    crate::ReportPosition {
                        label: format!("{} ({})", b.descriptor.label, b.descriptor.id),
                        position: "B".into(),
                    },
                ],
                classification,
                resolution: crate::ReportDisagreementResolution::BaselinePrecedence {
                    worker_id,
                    application: application.into(),
                },
            }],
            decisions: vec![],
            unavailable_decisions: vec![],
        };
    use crate::ReportDisagreementClassification::{Factual, PreferenceTradeoff};
    let valid = declaration(
        a.descriptor.id.to_string(),
        "Use A's stated organization",
        PreferenceTradeoff,
    );
    assert_eq!(
        valid.clone().validate(&catalog).unwrap().next_step,
        crate::ReconciliationNextStep::SubmitPlan
    );
    assert!(
        valid
            .validate(&crate::ReportReconciliationCatalog::default())
            .is_err()
    );
    for invalid in [
        declaration(b.descriptor.id.to_string(), "B", PreferenceTradeoff),
        declaration(
            a.descriptor.label.clone(),
            "label is not authority",
            PreferenceTradeoff,
        ),
        declaration(a.descriptor.id.to_string(), "  ", PreferenceTradeoff),
        declaration(a.descriptor.id.to_string(), "fact", Factual),
    ] {
        assert!(invalid.validate(&catalog).is_err());
    }
    mark(&mut b);
    let outcomes = vec![a.outcome(), b.outcome()];
    assert!(
        crate::build_synthesis_input(EnsembleWorkflow::Plan, "request", &outcomes, usize::MAX)
            .is_err()
    );
    assert!(
        crate::ReportReconciliationCatalog::from_summaries(
            &outcomes
                .iter()
                .map(AgentRunOutcome::summary)
                .collect::<Vec<_>>()
        )
        .is_err()
    );
}

#[test]
fn journal_baseline_mirrors_are_audit_only_and_require_exact_retained_publication() {
    let mut state = proposal();
    let receipt = mark(&mut state);
    let header = crate::AgentRunTranscriptHeader {
        version: 1,
        ensemble_run_id: receipt.target.run_id.clone(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "initial".into(),
        descriptor: state.descriptor.clone(),
    };
    let mut journal = crate::WorkerReviewJournal::new(&header);
    // Execution projection has the publication but not root confirmation authority.
    journal.state = state.clone();
    journal.state.confirmation = None;
    journal.state.baseline = None;
    for _ in 0..2 {
        for event in [
            WorkerReviewEvent::Confirmed {
                receipt: receipt.clone(),
            },
            WorkerReviewEvent::BaselineMarked {
                receipt: receipt.clone(),
            },
            WorkerReviewEvent::BaselineCleared {
                request_id: receipt.request_id.clone(),
            },
            WorkerReviewEvent::Sealed,
        ] {
            journal
                .apply(&crate::AgentRunTranscriptRecord::Event {
                    event: crate::AgentRunEvent::Review {
                        event: Box::new(event),
                    },
                })
                .unwrap();
        }
    }
    assert!(journal.events.is_empty());
    assert!(journal.state.confirmation.is_none());
    assert!(journal.state.baseline.is_none());
    let mut forged = receipt;
    forged.revision.digest.push('x');
    assert!(
        journal
            .apply(&crate::AgentRunTranscriptRecord::Event {
                event: crate::AgentRunEvent::Review {
                    event: Box::new(WorkerReviewEvent::BaselineMarked { receipt: forged })
                }
            })
            .is_err()
    );
}

#[test]
fn baseline_payload_size_is_serialized_and_never_truncates_mandatory_evidence() {
    let mut state = proposal();
    let receipt = receipt(&state);
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt })
        .unwrap();
    let unmarked = crate::build_synthesis_input(
        EnsembleWorkflow::Plan,
        "request",
        &[state.outcome()],
        usize::MAX,
    )
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(unmarked.split_once("\n\n").unwrap().1).unwrap();
    let old_len = serde_json::to_vec(&json["reports"][0]).unwrap().len();
    mark(&mut state);
    assert!(
        crate::validate_worker_synthesis_payload(EnsembleWorkflow::Plan, &state.outcome(), old_len)
            .is_err()
    );
    let marked = crate::build_synthesis_input(
        EnsembleWorkflow::Plan,
        "request",
        &[state.outcome()],
        usize::MAX,
    )
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(marked.split_once("\n\n").unwrap().1).unwrap();
    let new_len = serde_json::to_vec(&json["reports"][0]).unwrap().len();
    assert!(new_len > old_len);
    crate::validate_worker_synthesis_payload(EnsembleWorkflow::Plan, &state.outcome(), new_len)
        .unwrap();
    assert!(
        crate::validate_worker_synthesis_payload(
            EnsembleWorkflow::Plan,
            &state.outcome(),
            new_len - 1
        )
        .is_err()
    );
    assert_eq!(
        json["reports"][0]["structuredPlan"]["markdown"],
        "# Exact plan\n"
    );
}
