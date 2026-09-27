//! Explicit confirmation fixtures for synthesis tests. Lifecycle tests do not
//! use this harness: a proposal-only launcher never confirms on the user's behalf.
use super::*;

pub(super) fn start_stub_review(
    states: Vec<WorkerReviewState>,
    reports: &[Option<String>],
    proof: bool,
    choices: &[Vec<AgentUserDecisionBatch>],
    turn: TurnContext,
) -> EnsembleReviewExecution {
    let (tx, rx) = mpsc::channel(32);
    let cancellation = turn.cancellation().child_token();
    let mut commands = HashMap::new();
    for ((mut state, report), choices) in states.into_iter().zip(reports).zip(choices) {
        let (actor, mut inputs) = mpsc::unbounded_channel();
        commands.insert(state.descriptor.id.clone(), actor);
        let tx = tx.clone();
        let cancellation = cancellation.clone();
        let has_report = report.is_some();
        state.evidence.user_decisions = choices.clone();
        state.evidence.decision_ids = choices
            .iter()
            .flat_map(AgentUserDecisionBatch::decision_ids)
            .cloned()
            .collect();
        tokio::spawn(async move {
            let input = state.pending.front().unwrap().clone();
            let send = |event| {
                let tx = tx.clone();
                let worker_id = state.descriptor.id.clone();
                async move {
                    tx.send(WorkerActorUpdate { worker_id, event })
                        .await
                        .unwrap();
                }
            };
            send(WorkerReviewEvent::Dispatched {
                generation: input.generation,
                attempt: 1,
            })
            .await;
            if has_report && proof {
                send(WorkerReviewEvent::Published {
                    generation: input.generation,
                    plan: zevria_workflow::AgentStructuredPlan {
                        plan_id: Some("stub-plan".into()),
                        markdown: Some("# Stub implementation plan".into()),
                        entries: vec![],
                    },
                    replay: false,
                })
                .await;
            }
            send(WorkerReviewEvent::Settled {
                generation: input.generation,
                failure: (!has_report).then(|| "scripted failure".into()),
                connected: has_report,
                evidence: Box::new(state.evidence.clone()),
            })
            .await;
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    command = inputs.recv() => match command {
                        Some(WorkerActorCommand::Finish { acknowledgement, .. }) => { let _ = acknowledgement.send(Ok(())); break; }
                        None => break,
                        Some(_) => {}
                    }
                }
            }
        });
    }
    EnsembleReviewExecution {
        commands,
        updates: rx,
        cancellation,
    }
}

pub(super) async fn explicitly_confirm_proposals<P: ModelProvider>(
    engine: &mut SessionEngine<P>,
    command: SessionCommand,
    events: &SessionEventSender,
) -> Result<(), SessionReplayError> {
    explicitly_review_proposals(engine, command, events, false).await
}

pub(super) async fn explicitly_review_proposals<P: ModelProvider>(
    engine: &mut SessionEngine<P>,
    command: SessionCommand,
    events: &SessionEventSender,
    baseline: bool,
) -> Result<(), SessionReplayError> {
    let router = engine.capabilities.worker_controls.clone();
    let (tx, mut rx) = session_event_channel(128);
    let run = engine.handle_command(command, &tx);
    tokio::pin!(run);
    let mut confirmed = std::collections::HashSet::new();
    let result = loop {
        tokio::select! {
            biased;
            update = rx.recv() => {
                if let Some(SessionUpdate::Lifecycle(event)) = update {
                    if let SessionEvent::WorkerReviewUpdated { target, state } = &event
                        && let Some(snapshot) = state.eligible_snapshot()
                        && state.confirmation.is_none()
                        && confirmed.insert(snapshot.revision.digest.clone() + state.descriptor.id.as_str()) {
                        router.route(WorkerControl { request_id: WorkerControlId::new(), target: target.clone(), action: if baseline { WorkerControlAction::Baseline { expected_revision: snapshot.revision.clone() } } else { WorkerControlAction::Confirm { expected_revision: snapshot.revision.clone() } } }).unwrap();
                    }
                    events.send(event).await.unwrap();
                }
            }
            result = &mut run => break result,
        }
    };
    while let Ok(update) = rx.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            events.send(event).await.unwrap();
        }
    }
    result
}

pub(super) fn with_explicit_fixture_confirmations(
    items: Vec<TranscriptItem>,
) -> Vec<TranscriptItem> {
    let start = items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Ensemble(EnsembleRecord::Started { start })
                if start.workflow == EnsembleWorkflow::Plan =>
            {
                Some(start.clone())
            }
            _ => None,
        })
        .unwrap();
    let mut extra = vec![TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
        run_id: start.run_id.clone(),
        version: ENSEMBLE_REVIEW_VERSION,
    })];
    let mut states = Vec::new();
    let mut final_result = None;
    for (index, descriptor) in start.agents.iter().enumerate() {
        let mut state = WorkerReviewState::new(descriptor.clone());
        let mut push = |event: WorkerReviewEvent, result| {
            state.apply(&event).unwrap();
            extra.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: descriptor.id.clone(),
                event: Box::new(event),
                result,
            }));
        };
        push(
            WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::Initial,
                    text: start.prompt.clone(),
                },
            },
            None,
        );
        push(
            WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            },
            None,
        );
        push(
            WorkerReviewEvent::Published {
                generation: 1,
                plan: zevria_workflow::AgentStructuredPlan {
                    plan_id: None,
                    markdown: Some("# Explicitly confirmed fixture plan".into()),
                    entries: vec![],
                },
                replay: false,
            },
            None,
        );
        drop(push);
        let event = WorkerReviewEvent::Settled {
            generation: 1,
            failure: None,
            connected: true,
            evidence: Box::new(state.evidence.clone()),
        };
        state.apply(&event).unwrap();
        extra.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: descriptor.id.clone(),
            event: Box::new(event),
            result: None,
        }));
        let receipt = WorkerConfirmationReceipt {
            request_id: WorkerControlId::new(),
            target: WorkerControlTarget {
                turn_id: TurnId::new(1),
                run_id: start.run_id.clone(),
                worker_id: descriptor.id.clone(),
            },
            revision: state.eligible_snapshot().unwrap().revision.clone(),
        };
        let result = WorkerControlResult {
            control: WorkerControl {
                request_id: receipt.request_id.clone(),
                target: receipt.target.clone(),
                action: WorkerControlAction::Confirm {
                    expected_revision: receipt.revision.clone(),
                },
            },
            accepted: true,
            detail: "Explicit fixture user confirmation".into(),
        };
        let event = WorkerReviewEvent::Confirmed { receipt };
        state.apply(&event).unwrap();
        if index + 1 < start.agents.len() {
            extra.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                run_id: start.run_id.clone(),
                worker_id: descriptor.id.clone(),
                event: Box::new(event),
                result: Some(result),
            }));
        } else {
            final_result = Some(result);
        }
        states.push(state);
    }
    let outcomes = states
        .iter()
        .map(WorkerReviewState::outcome)
        .collect::<Vec<_>>();
    extra.push(TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
        run_id: start.run_id.clone(),
        final_confirmation: final_result.unwrap(),
        outcomes: outcomes.clone(),
    }));
    let reports = EnsembleRecord::ReportsReady {
        run_id: start.run_id.clone(),
        synthesis_input: zevria_workflow::ensemble::build_synthesis_prompt(
            EnsembleWorkflow::Plan,
            &start.prompt,
            &outcomes,
            usize::MAX,
        )
        .unwrap(),
        agents: outcomes.iter().map(AgentRunOutcome::summary).collect(),
    };
    let mut output = Vec::new();
    for item in items {
        match item {
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. }) => {
                output.append(&mut extra);
                output.push(TranscriptItem::Ensemble(reports.clone()));
            }
            other => output.push(other),
        }
    }
    output
}
