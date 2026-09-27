use super::*;
use crate::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunDescriptor, AgentRunId, AgentStructuredPlan,
    EnsembleRunId, EnsembleWorkflow, WorkerControlId, WorkerPromptKind,
};

fn journal() -> WorkerReviewJournal {
    WorkerReviewJournal::new(&AgentRunTranscriptHeader {
        version: AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "initial".into(),
        descriptor: AgentRunDescriptor {
            id: AgentRunId::new(),
            agent: "test".into(),
            label: "Test".into(),
            safe_mode: "plan".into(),
        },
    })
}
fn record(journal: &mut WorkerReviewJournal, event: AgentRunEvent) {
    journal
        .apply(&AgentRunTranscriptRecord::Event { event })
        .unwrap();
}
fn review(journal: &mut WorkerReviewJournal, event: WorkerReviewEvent) {
    record(
        journal,
        AgentRunEvent::Review {
            event: Box::new(event),
        },
    );
}
fn input(generation: u64) -> WorkerInput {
    WorkerInput {
        generation,
        request_id: WorkerControlId::new(),
        kind: if generation == 1 {
            WorkerPromptKind::Initial
        } else {
            WorkerPromptKind::UserFeedback
        },
        text: format!("input {generation}").into(),
    }
}
fn prompt(journal: &mut WorkerReviewJournal, input: &WorkerInput) {
    review(
        journal,
        WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        },
    );
    review(
        journal,
        WorkerReviewEvent::Dispatched {
            generation: input.generation,
            attempt: 1,
        },
    );
    record(
        journal,
        AgentRunEvent::Prompt {
            text: input.text.display_projection(),
            continuation: input.generation > 1,
            repair: None,
        },
    );
}
fn publish(journal: &mut WorkerReviewJournal, markdown: &str) {
    record(
        journal,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("same-provider-id".into()),
                markdown: Some(markdown.into()),
                entries: vec![],
            },
        },
    );
}
fn settle(journal: &mut WorkerReviewJournal, failure: Option<String>) {
    review(
        journal,
        WorkerReviewEvent::Settled {
            generation: journal.state.active.as_ref().unwrap().generation,
            failure,
            connected: true,
            evidence: Box::new(journal.state.evidence.clone()),
        },
    );
}

#[test]
fn ensemble_abandonment_requires_root_authority_but_not_a_worker_mirror() {
    let mut journal = journal();
    let event = WorkerReviewEvent::Abandoned {
        request_id: WorkerControlId::new(),
    };
    let mut outcome = journal.state.outcome();
    outcome.sanitize_abandonment();
    assert!(
        journal
            .apply(&AgentRunTranscriptRecord::Outcome {
                outcome: outcome.clone()
            })
            .is_err()
    );
    assert!(
        journal
            .reconcile(std::slice::from_ref(&event))
            .unwrap()
            .is_empty()
    );
    review(&mut journal, event.clone());
    journal
        .apply(&AgentRunTranscriptRecord::Outcome { outcome })
        .unwrap();
    assert!(journal.reconcile(&[]).is_err());
    assert!(journal.reconcile(&[event]).unwrap().is_empty());
}

#[test]
fn ensemble_abandoned_root_does_not_promote_a_valid_late_execution_suffix() {
    let mut journal = journal();
    prompt(&mut journal, &input(1));
    publish(&mut journal, "# Late excluded publication");
    settle(&mut journal, None);
    let root = vec![WorkerReviewEvent::Abandoned {
        request_id: WorkerControlId::new(),
    }];
    assert!(journal.reconcile(&root).unwrap().is_empty());
    assert!(journal.state.retained.is_some());
}

#[test]
fn ensemble_cross_log_dispatch_publication_and_settlement_crash_boundaries() {
    let mut journal = journal();
    let input = input(1);
    prompt(&mut journal, &input);
    publish(&mut journal, "# Exact proposal\n");
    settle(&mut journal, None);
    for captured in 0..=journal.events.len() {
        let mut root = vec![WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        }];
        root.extend_from_slice(&journal.events[..captured]);
        let mut state = WorkerReviewState::new(journal.state.descriptor.clone());
        for event in &root {
            state.apply(event).unwrap();
        }
        let missing = journal.reconcile(&root).unwrap();
        for event in &missing {
            state.apply(event).unwrap();
        }
        assert_eq!(state, journal.state);
        assert!(
            state.pending.is_empty(),
            "durably dispatched text must not be sent again"
        );
        assert!(state.eligible_snapshot().is_some());
        assert!(state.confirmation.is_none(), "recovery is not approval");
    }
}

#[test]
fn ensemble_cross_log_dispatched_but_unsettled_feedback_uses_failed_fallback() {
    let mut journal = journal();
    let first = input(1);
    prompt(&mut journal, &first);
    publish(&mut journal, "# Retained");
    settle(&mut journal, None);
    let second = input(2);
    let mut root = vec![WorkerReviewEvent::InputAccepted { input: first }];
    root.extend(journal.events.clone());
    root.push(WorkerReviewEvent::InputAccepted {
        input: second.clone(),
    });
    prompt(&mut journal, &second);
    publish(&mut journal, "# Unfinished feedback draft");
    let mut state = WorkerReviewState::new(journal.state.descriptor.clone());
    for event in root.iter().chain(journal.reconcile(&root).unwrap().iter()) {
        state.apply(event).unwrap();
    }
    assert_eq!(state.active.as_ref().unwrap().generation, 2);
    state
        .apply(&WorkerReviewEvent::Interrupted { generation: 2 })
        .unwrap();
    assert!(state.pending.is_empty());
    assert_eq!(
        state.eligible_snapshot().unwrap().plan.markdown.as_deref(),
        Some("# Retained")
    );
    assert!(state.confirmation.is_none());
    assert!(
        state
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not incorporated")
    );
}

#[test]
fn ensemble_late_removal_cannot_rewrite_a_root_sealed_snapshot() {
    let mut journal = journal();
    prompt(&mut journal, &input(1));
    publish(&mut journal, "# Exact final snapshot");
    settle(&mut journal, None);
    let mut root = journal.state.clone();
    let receipt = crate::WorkerConfirmationReceipt {
        request_id: WorkerControlId::new(),
        target: crate::WorkerControlTarget {
            turn_id: crate::TurnId::new(1),
            run_id: journal.run_id.clone(),
            worker_id: root.descriptor.id.clone(),
        },
        revision: root.eligible_snapshot().unwrap().revision.clone(),
    };
    root.apply(&WorkerReviewEvent::Confirmed {
        receipt: receipt.clone(),
    })
    .unwrap();
    let frozen = root.outcome();
    record(
        &mut journal,
        AgentRunEvent::PlanRemoved {
            plan_id: "same-provider-id".into(),
        },
    );
    assert!(journal.state.eligible_snapshot().is_none());
    review(&mut journal, WorkerReviewEvent::Confirmed { receipt });
    journal
        .apply(&AgentRunTranscriptRecord::Outcome {
            outcome: frozen.clone(),
        })
        .unwrap();
    assert_eq!(
        frozen.plan.as_ref().unwrap().markdown.as_deref(),
        Some("# Exact final snapshot")
    );
    // Without a root seal the same evidence still revokes confirmation.
    root.apply(&WorkerReviewEvent::Removed {
        plan_id: "same-provider-id".into(),
    })
    .unwrap();
    assert!(root.confirmed_plan().is_none());
}

#[test]
fn ensemble_recovery_preserves_host_decisions_captured_before_interrupted_settlement() {
    let mut journal = journal();
    let initial = input(1);
    prompt(&mut journal, &initial);
    let mut root = vec![WorkerReviewEvent::InputAccepted { input: initial }];
    root.extend(journal.events.clone());
    let request_id = crate::QuestionRequestId::new("durable-choice");
    let batch = crate::AgentUserDecisionBatch {
        request_id: request_id.clone(),
        answers: vec![crate::AgentUserDecisionAnswer {
            decision_id: crate::AgentUserDecisionId::from_question(&request_id, "scope"),
            question_id: "scope".into(),
            header: "Scope".into(),
            question: "Choose scope".into(),
            answer: crate::AgentUserDecisionValue::String {
                value: "Focused".into(),
            },
        }],
    };
    record(
        &mut journal,
        AgentRunEvent::Elicitation {
            field_count: 1,
            outcome: crate::AgentElicitationOutcome::Accepted,
            decision: Some(batch.clone()),
            decision_unavailable: None,
        },
    );
    let mut state = WorkerReviewState::new(journal.state.descriptor.clone());
    for event in root.iter().chain(journal.reconcile(&root).unwrap().iter()) {
        state.apply(event).unwrap();
    }
    state
        .apply(&WorkerReviewEvent::Interrupted { generation: 1 })
        .unwrap();
    assert_eq!(state.evidence.user_decisions, vec![batch]);
    assert_eq!(state.evidence.decision_ids.len(), 1);
    assert!(state.confirmation.is_none());
}

#[test]
fn ensemble_worker_mirrors_are_idempotent_but_conflicting_or_unaccepted_input_fails() {
    let mut journal = journal();
    let accepted = input(1);
    prompt(&mut journal, &accepted);
    let before = journal.state.clone();
    review(
        &mut journal,
        WorkerReviewEvent::InputAccepted {
            input: accepted.clone(),
        },
    );
    assert_eq!(journal.state, before);
    assert!(journal.reconcile(&[]).is_err());
    let mut conflicting = accepted;
    conflicting.text = "not the accepted text".into();
    assert!(
        journal
            .apply(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Review {
                    event: Box::new(WorkerReviewEvent::InputAccepted {
                        input: conflicting.clone()
                    }),
                }
            })
            .is_err()
    );
    assert!(
        journal
            .reconcile(&[WorkerReviewEvent::InputAccepted { input: conflicting }])
            .is_err()
    );
}

#[test]
fn ensemble_late_cancellation_mirror_does_not_rewrite_worker_settlement() {
    let mut journal = journal();
    prompt(&mut journal, &input(1));
    publish(&mut journal, "# Settled before cancellation arrived");
    settle(&mut journal, None);
    let before = journal.clone();
    review(
        &mut journal,
        WorkerReviewEvent::CancelRequested { generation: 1 },
    );
    assert_eq!(
        journal, before,
        "host cancellation mirrors are not worker execution or eligibility authority"
    );
    for generation in [0, 2] {
        assert!(
            journal
                .apply(&AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::CancelRequested { generation }),
                    }
                })
                .is_err()
        );
        assert_eq!(journal, before);
    }
}

#[test]
fn ensemble_worker_replay_is_not_fresh_but_identical_explicit_republication_is() {
    let mut journal = journal();
    prompt(&mut journal, &input(1));
    publish(&mut journal, "# Same bytes");
    settle(&mut journal, None);
    let revision = journal.state.retained.as_ref().unwrap().revision.clone();
    record(&mut journal, AgentRunEvent::ReplayBoundary);
    publish(&mut journal, "# Same bytes");
    record(
        &mut journal,
        AgentRunEvent::SessionEstablished {
            session_id: "same-session".into(),
            capabilities: serde_json::json!({}),
            safe_mode: "plan".into(),
            recovered: true,
        },
    );
    prompt(&mut journal, &input(2));
    settle(&mut journal, None);
    assert!(journal.state.eligible_snapshot().is_none());
    prompt(&mut journal, &input(3));
    publish(&mut journal, "# Same bytes");
    settle(&mut journal, None);
    let current = &journal.state.eligible_snapshot().unwrap().revision;
    assert_eq!(current.digest, revision.digest);
    assert_eq!(current.revision, revision.revision + 1);
    assert_eq!(current.generation, 3);
}
