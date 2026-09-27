use super::*;

fn history(workers: usize, abandon_second: bool) -> Vec<TranscriptItem> {
    let mut states = (0..workers).map(|_| state()).collect::<Vec<_>>();
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan it".into(),
        agents: states
            .iter()
            .map(|state| state.descriptor.clone())
            .collect(),
    };
    let id = PlanId::new();
    let markdown = "\r\n  # Go\r\n- Do this.  ";
    let mut items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: ENSEMBLE_REVIEW_VERSION,
        }),
        TranscriptItem::Plan(PlanRecord::Started { id }),
    ];
    for state in &mut states {
        let events = [
            WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::Initial,
                    text: start.prompt.clone(),
                },
            },
            WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            },
            WorkerReviewEvent::Published {
                generation: 1,
                plan: AgentStructuredPlan {
                    plan_id: None,
                    markdown: Some(markdown.into()),
                    entries: vec![],
                },
                replay: false,
            },
        ];
        for event in events {
            state.apply(&event).unwrap();
            items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: state.descriptor.id.clone(),
                event: Box::new(event),
                result: None,
            }));
        }
        let event = WorkerReviewEvent::Settled {
            generation: 1,
            failure: None,
            connected: true,
            evidence: Box::new(state.evidence.clone()),
        };
        state.apply(&event).unwrap();
        items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
            event: Box::new(event),
            result: None,
        }));
    }
    let mut last_result = None;
    for (index, state) in states.iter_mut().enumerate() {
        let control = WorkerControl {
            request_id: WorkerControlId::new(),
            target: WorkerControlTarget {
                turn_id: TurnId::new(1),
                run_id: start.run_id.clone(),
                worker_id: state.descriptor.id.clone(),
            },
            action: if abandon_second && index == 1 {
                WorkerControlAction::Abandon
            } else {
                WorkerControlAction::Confirm {
                    expected_revision: state.eligible_snapshot().unwrap().revision.clone(),
                }
            },
        };
        let event = control.sealing_event().unwrap();
        let result = WorkerControlResult {
            control,
            accepted: true,
            detail: "explicit user control".into(),
        };
        state.apply(&event).unwrap();
        if index + 1 == workers {
            last_result = Some(result);
        } else {
            items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: state.descriptor.id.clone(),
                event: Box::new(event),
                result: Some(result),
            }));
        }
    }
    let outcomes = states
        .iter()
        .map(WorkerReviewState::outcome)
        .collect::<Vec<_>>();
    items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
        run_id: start.run_id.clone(),
        final_confirmation: last_result.unwrap(),
        outcomes: outcomes.clone(),
    }));
    items.push(TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
        run_id: start.run_id.clone(),
        synthesis_input: ensemble::build_synthesis_prompt_with_feedback(
            start.workflow,
            &start.prompt,
            &outcomes,
            &states,
            usize::MAX,
        )
        .unwrap(),
        agents: outcomes.iter().map(AgentRunOutcome::summary).collect(),
    }));
    let confirmed = outcomes[0].confirmation.as_ref().unwrap();
    items.push(TranscriptItem::Plan(PlanRecord::Published {
        artifact: DirectWorkerPlan::validate(markdown.into())
            .unwrap()
            .into_artifact(PlanVersion { id, revision: 1 }, TurnId::new(1)),
        provenance: PlanPublicationProvenance::ConfirmedWorker {
            run_id: start.run_id,
            worker_id: outcomes[0].descriptor.id.clone(),
            revision: confirmed.snapshot.revision.clone(),
        },
    }));
    items
}

#[test]
fn direct_publication_replays_exact_frozen_markdown_as_published() {
    let mut items = history(1, false);
    validate_session_replay(&items).unwrap();
    let serialized = serde_json::to_string(&items).unwrap();
    let restored: Vec<TranscriptItem> = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored, items);
    validate_session_replay(&restored).unwrap();
    let run_id = match &items[0] {
        TranscriptItem::Ensemble(EnsembleRecord::Started { start }) => start.run_id.clone(),
        _ => unreachable!(),
    };
    items.push(TranscriptItem::Ensemble(EnsembleRecord::Completed {
        run_id,
    }));
    validate_session_replay(&items).unwrap();
}

#[test]
fn direct_publication_rejects_forged_source_revision_content_and_metadata() {
    for mutation in 0..8 {
        let mut items = history(1, false);
        let TranscriptItem::Plan(PlanRecord::Published {
            artifact,
            provenance,
        }) = items.last_mut().unwrap()
        else {
            unreachable!()
        };
        let PlanPublicationProvenance::ConfirmedWorker {
            run_id,
            worker_id,
            revision,
        } = provenance
        else {
            unreachable!()
        };
        match mutation {
            0 => *run_id = EnsembleRunId::new(),
            1 => *worker_id = AgentRunId::new(),
            2 => revision.worker_id = AgentRunId::new(),
            3 => revision.revision += 1,
            4 => revision.digest.push('x'),
            5 => artifact.markdown.push('\n'),
            6 => artifact.title = "Forged metadata".into(),
            7 => *provenance = PlanPublicationProvenance::Synthesized,
            _ => unreachable!(),
        }
        assert!(
            validate_session_replay(&items).is_err(),
            "mutation {mutation}"
        );
    }
    for abandoned in [false, true] {
        assert!(
            validate_ensemble_review_history(&history(2, abandoned))
                .unwrap_err()
                .contains("original single-worker")
        );
    }
}

#[test]
fn direct_publication_requires_valid_seal_and_reports_in_order_once() {
    let good = history(1, false);
    for mutation in 0..11 {
        let mut items = good.clone();
        let seal_index = items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { .. })
                )
            })
            .unwrap();
        let reports_index = seal_index + 1;
        match mutation {
            0 => {
                items.remove(seal_index);
            }
            1 => {
                items.remove(reports_index);
            }
            2 => {
                items.swap(reports_index, reports_index + 1);
            }
            3 => {
                items.push(items.last().unwrap().clone());
            }
            _ => {
                let TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
                    outcomes,
                    final_confirmation,
                    ..
                }) = &mut items[seal_index]
                else {
                    unreachable!()
                };
                match mutation {
                    4 => outcomes[0].confirmation = None,
                    5 => outcomes[0].status = AgentRunStatus::Failed,
                    6 => outcomes[0].partial = true,
                    7 => outcomes[0].plan = None,
                    8 => outcomes[0].status = AgentRunStatus::Abandoned,
                    9 => final_confirmation.accepted = false,
                    10 => {
                        outcomes[0]
                            .confirmation
                            .as_mut()
                            .unwrap()
                            .snapshot
                            .plan
                            .markdown = Some("changed".into())
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert!(
            validate_ensemble_review_history(&items).is_err(),
            "mutation {mutation}"
        );
    }
}
