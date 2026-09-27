use super::*;
use zevria_foundation::TurnId;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::WorkerConfirmationReceipt;
use zevria_workflow::WorkerControl;
use zevria_workflow::WorkerControlAction;
use zevria_workflow::WorkerControlId;
use zevria_workflow::WorkerControlResult;
use zevria_workflow::WorkerControlTarget;
use zevria_workflow::WorkerReviewEvent;
use zevria_workflow::WorkerReviewState;

fn fixture() -> (EnsembleStart, Vec<TranscriptItem>, Vec<WorkerReviewState>) {
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan".into(),
        agents: (0..3)
            .map(|i| AgentRunDescriptor {
                id: AgentRunId::new(),
                agent: format!("worker-{i}"),
                label: format!("Worker {i}"),
                safe_mode: "read-only".into(),
            })
            .collect(),
    };
    let mut items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: 1,
        }),
    ];
    let mut states = Vec::new();
    for descriptor in &start.agents {
        let mut state = WorkerReviewState::new(descriptor.clone());
        for event in [
            WorkerReviewEvent::InputAccepted {
                input: zevria_workflow::WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: zevria_workflow::WorkerPromptKind::Initial,
                    text: start.prompt.clone(),
                },
            },
            WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            },
            WorkerReviewEvent::Published {
                generation: 1,
                plan: zevria_workflow::AgentStructuredPlan {
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
                evidence: Box::new(state.evidence.clone()),
            },
        ] {
            state.apply(&event).unwrap();
            items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: descriptor.id.clone(),
                event: Box::new(event),
                result: None,
            }));
        }
        states.push(state);
    }
    (start, items, states)
}

fn mark(
    start: &EnsembleStart,
    states: &mut [WorkerReviewState],
    items: &mut Vec<TranscriptItem>,
    index: usize,
) -> WorkerConfirmationReceipt {
    let receipt = WorkerConfirmationReceipt {
        request_id: WorkerControlId::new(),
        target: WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: start.run_id.clone(),
            worker_id: states[index].descriptor.id.clone(),
        },
        revision: states[index].eligible_snapshot().unwrap().revision.clone(),
    };
    let event = WorkerReviewEvent::BaselineMarked {
        receipt: receipt.clone(),
    };
    zevria_workflow::apply_worker_review_event(states, &receipt.target.worker_id, &event).unwrap();
    items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: receipt.target.worker_id.clone(),
        event: Box::new(event),
        result: Some(WorkerControlResult {
            control: WorkerControl {
                request_id: receipt.request_id.clone(),
                target: receipt.target.clone(),
                action: WorkerControlAction::Baseline {
                    expected_revision: receipt.revision.clone(),
                },
            },
            accepted: true,
            detail: "baseline".into(),
        }),
    }));
    receipt
}

fn publish(views: &mut SessionViews, start: &EnsembleStart, state: &WorkerReviewState) {
    views.apply(SessionEvent::WorkerReviewUpdated {
        target: WorkerControlTarget {
            turn_id: TurnId::new(1),
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
        },
        state: Box::new(state.clone()),
    });
}

#[test]
fn late_outcomes_and_reports_cannot_revive_a_revoked_review_or_freeze_a_new_round() {
    let (start, mut items, mut states) = fixture();
    mark(&start, &mut states, &mut items, 0);
    let stale = states[0].outcome();
    let worker_id = states[0].descriptor.id.clone();
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    publish(&mut views, &start, &states[0]);
    let event = WorkerReviewEvent::Withdrawn {
        request_id: WorkerControlId::new(),
    };
    states[0].apply(&event).unwrap();
    items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: worker_id.clone(),
        event: Box::new(event),
        result: None,
    }));
    publish(&mut views, &start, &states[0]);
    views.apply(SessionEvent::AgentRunFinished {
        turn_id: TurnId::new(1),
        ensemble_run_id: start.run_id.clone(),
        outcome: stale.clone(),
    });
    views.apply(SessionEvent::EnsembleReportsReady {
        turn_id: TurnId::new(1),
        run_id: start.run_id.clone(),
        agents: vec![stale.summary()],
    });
    assert_eq!(views.root.ensemble_baseline(&start.run_id), None);
    assert_eq!(
        views.root.reviewed_worker_status(&start.run_id, &worker_id),
        Some(states[0].status())
    );
    assert!(
        views.agents[0].app.capabilities().edit_draft.is_ok(),
        "stale outcome cannot freeze a still-live review"
    );
    assert!(!views.agents[0].baseline);
    items.push(TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
        run_id: start.run_id.clone(),
        synthesis_input: rig_core::message::Message::user("stale lower-authority summary"),
        agents: vec![stale.summary()],
    }));
    let mut restored = App::new();
    restored.restore(items);
    assert_eq!(restored.ensemble_baseline(&start.run_id), None);
    assert_eq!(
        restored.reviewed_worker_status(&start.run_id, &worker_id),
        Some(states[0].status())
    );
}

#[test]
fn reports_only_baseline_evidence_has_the_same_live_and_restored_fallback() {
    let (start, mut items, mut states) = fixture();
    mark(&start, &mut states, &mut items, 0);
    let summary = states[0].outcome().summary();
    let records = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: rig_core::message::Message::user("reports"),
            agents: vec![summary.clone()],
        }),
    ];
    let mut live = App::new();
    live.reduce(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    live.reduce(SessionEvent::EnsembleReportsReady {
        turn_id: TurnId::new(1),
        run_id: start.run_id.clone(),
        agents: vec![summary],
    });
    let mut restored = App::new();
    restored.restore(records.clone());
    assert_eq!(
        restored.ensemble_baseline(&start.run_id),
        Some(&states[0].descriptor.id)
    );
    assert_eq!(
        restored.ensemble_baseline(&start.run_id),
        live.ensemble_baseline(&start.run_id)
    );
    let mut views = SessionViews::new(restored, PathBuf::from("."));
    views.seed_child_creation_order(&records);
    assert_eq!(
        views.baseline_workers.get(&start.run_id),
        Some(&states[0].descriptor.id)
    );
}

#[test]
fn live_transfer_uses_one_root_id_for_all_rows_and_panes() {
    let (start, mut items, mut states) = fixture();
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    let first = mark(&start, &mut states, &mut items, 0);
    publish(&mut views, &start, &states[0]);
    assert!(views.agents[0].baseline);
    assert!(
        views.agents[0]
            .app
            .subsession_title()
            .unwrap()
            .contains("baseline")
    );
    mark(&start, &mut states, &mut items, 1);
    // New target arrives before the old target's displacement snapshot.
    publish(&mut views, &start, &states[1]);
    assert!(!views.agents[0].baseline);
    assert!(views.agents[1].baseline);
    assert!(
        !views.agents[0]
            .app
            .subsession_title()
            .unwrap()
            .contains("baseline")
    );
    publish(&mut views, &start, &states[0]);
    assert!(views.agents[1].baseline);
    assert_eq!(states[0].confirmation.as_ref(), Some(&first));
    assert_eq!(
        views.root.ensemble_baseline(&start.run_id),
        Some(&states[1].descriptor.id)
    );
    states[1]
        .apply(&WorkerReviewEvent::BaselineCleared {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    publish(&mut views, &start, &states[1]);
    assert!(views.agents.iter().all(|pane| !pane.baseline));
    assert!(states[1].confirmation.is_some());
    mark(&start, &mut states, &mut items, 0);
    publish(&mut views, &start, &states[0]);
    states[0]
        .apply(&WorkerReviewEvent::Withdrawn {
            request_id: WorkerControlId::new(),
        })
        .unwrap();
    publish(&mut views, &start, &states[0]);
    assert!(views.agents.iter().all(|pane| !pane.baseline));
}

#[test]
fn historical_badges_follow_root_transfer_clear_and_absence_not_sidecar_text() {
    let (start, mut items, mut states) = fixture();
    let stale = mark(&start, &mut states, &mut items, 0);
    let marked_a = items.clone();
    let selected = mark(&start, &mut states, &mut items, 1);
    let transferred = items.clone();
    let mut frozen = transferred.clone();
    let mut frozen_states = states.clone();
    mark(&start, &mut frozen_states, &mut frozen, 2);
    let TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        result: Some(final_confirmation),
        ..
    }) = frozen.pop().unwrap()
    else {
        unreachable!()
    };
    let outcomes = frozen_states
        .iter()
        .map(WorkerReviewState::outcome)
        .collect::<Vec<_>>();
    frozen.push(TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
        run_id: start.run_id.clone(),
        final_confirmation,
        outcomes: outcomes.clone(),
    }));
    let mut reports = frozen.clone();
    reports.push(TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
        run_id: start.run_id.clone(),
        synthesis_input: zevria_workflow::ensemble::build_synthesis_prompt(
            start.workflow,
            &start.prompt,
            &outcomes,
            usize::MAX,
        )
        .unwrap(),
        agents: outcomes
            .iter()
            .map(zevria_workflow::AgentRunOutcome::summary)
            .collect(),
    }));
    let request_id = WorkerControlId::new();
    items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: states[1].descriptor.id.clone(),
        event: Box::new(WorkerReviewEvent::BaselineCleared {
            request_id: request_id.clone(),
        }),
        result: Some(WorkerControlResult {
            control: WorkerControl {
                request_id,
                target: selected.target.clone(),
                action: WorkerControlAction::Unbaseline {
                    expected_revision: selected.revision.clone(),
                },
            },
            accepted: true,
            detail: "confirmation retained".into(),
        }),
    }));
    let root_absent = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: 1,
        }),
    ];
    for (history, selected_index) in [
        (marked_a, Some(0)),
        (transferred, Some(1)),
        (frozen, Some(2)),
        (reports, Some(2)),
        (items, None),
        (root_absent, None),
    ] {
        zevria_transcript::validate_ensemble_review_history(&history).unwrap();
        let mut root = App::new();
        root.restore(history.clone());
        let mut views = SessionViews::new(root, PathBuf::from("."));
        views.seed_child_creation_order(&history);
        for descriptor in &start.agents {
            let mut records = vec![AgentRunTranscriptRecord::Header {
                header: zevria_transcript::AgentRunTranscriptHeader {
                    version: 1,
                    ensemble_run_id: start.run_id.clone(),
                    workflow: start.workflow,
                    prompt: start.prompt.clone(),
                    descriptor: descriptor.clone(),
                },
            }];
            if descriptor.id == stale.target.worker_id {
                records.push(AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::BaselineMarked {
                            receipt: stale.clone(),
                        }),
                    },
                });
            }
            views.restore_agent_run(records);
        }
        for (index, pane) in views.agents.iter().enumerate() {
            assert!(pane.historical);
            assert_eq!(pane.baseline, selected_index == Some(index));
            assert_eq!(
                pane.app.subsession_title().unwrap().contains("baseline"),
                selected_index == Some(index)
            );
        }
        // Historical inspect-only panes deliberately have no live review binding.
        if let Some(index) = selected_index {
            assert_eq!(
                views.root.ensemble_baseline(&start.run_id),
                Some(&start.agents[index].id)
            );
        } else {
            assert!(views.root.ensemble_baseline(&start.run_id).is_none());
        }
    }
}

#[test]
fn exact_marking_and_clearing_audit_mirrors_deduplicate_on_finalization() {
    let (start, mut items, mut states) = fixture();
    let receipt = mark(&start, &mut states, &mut items, 0);
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    let index = views.ensure_agent(&start.run_id, &start.agents[0]);
    let pane = &mut views.agents[index];
    for _ in 0..2 {
        pane.apply_transcript_event(AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::BaselineMarked {
                receipt: receipt.clone(),
            }),
        });
        pane.reconcile_outcome_evidence(
            "",
            states[0].outcome().plan.as_ref(),
            states[0].outcome().confirmation.as_deref(),
        );
    }
    let text = format!("{:?}", pane.app.history());
    assert_eq!(text.matches("Baseline marked").count(), 1);
    assert_eq!(text.matches("Confirmed proposal revision").count(), 1);
    assert!(text.contains(&receipt.revision.digest));
    let notices = pane
        .app
        .history()
        .iter()
        .filter_map(|entry| match entry {
            crate::app::HistoryEntry::Conversation(entry) => Some(&entry.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match &block.kind {
            crate::presentation::PresentationBlockKind::Diagnostic(notice)
                if block.visibility == crate::presentation::BlockVisibility::Always =>
            {
                Some(notice)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices.len(),
        3,
        "confirmation, baseline and freeze are separate audit outcomes"
    );
    assert!(
        notices
            .iter()
            .all(|notice| notice.tone == crate::presentation::DiagnosticTone::Success)
    );
    // Audit messages alone cannot put a badge on the pane.
    assert!(!pane.baseline);
}
