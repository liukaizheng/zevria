use super::*;

#[path = "worker_abandon.rs"]
mod abandonment;

#[path = "worker_baseline.rs"]
mod baseline;

#[path = "worker_review_recovery.rs"]
mod recovery;

#[path = "direct_plan.rs"]
mod direct_plan;

struct InteractiveLauncher {
    limit: usize,
    late_updates: usize,
    abandoned: Arc<Mutex<Vec<AgentRunId>>>,
    unavailable: bool,
}
impl EnsembleLauncher for InteractiveLauncher {
    fn workers(
        &self,
        _: EnsembleWorkflow,
    ) -> anyhow::Result<Vec<zevria_workflow::AgentRunDescriptor>> {
        Ok((0..2)
            .map(|i| zevria_workflow::AgentRunDescriptor {
                id: AgentRunId::new(),
                agent: format!("worker-{i}"),
                label: format!("Worker {i}"),
                safe_mode: "read-only".into(),
            })
            .collect())
    }
    fn max_synthesis_bytes_per_agent(&self) -> usize {
        self.limit
    }
    fn launch<'a>(
        &'a self,
        _: EnsembleLaunchRequest,
        _: SessionEventSender,
        _: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async { anyhow::bail!("interactive only") })
    }
    fn finalize_review<'a>(
        &'a self,
        _: EnsembleLaunchRequest,
        outcomes: Vec<AgentRunOutcome>,
        _: SessionEventSender,
        _: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async move { Ok(outcomes) })
    }
    fn start_review(
        &self,
        _: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        _: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        let cancellation = turn.cancellation().child_token();
        let (tx, rx) = mpsc::channel(32);
        let mut commands = HashMap::new();
        for state in states {
            if state.abandoned {
                continue;
            }
            let abandoned = self.abandoned.clone();
            let late_updates = self.late_updates;
            let worker_id = state.descriptor.id.clone();
            let (command_tx, mut command_rx) = mpsc::unbounded_channel();
            if !self.unavailable || state.descriptor.agent != "worker-1" {
                commands.insert(worker_id.clone(), command_tx);
            }
            let tx = tx.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                let send = |event| {
                    let tx = tx.clone();
                    let worker_id = worker_id.clone();
                    async move {
                        tx.send(WorkerActorUpdate { worker_id, event })
                            .await
                            .unwrap();
                    }
                };
                if let Some(initial) = state.pending.front().cloned() {
                    send(WorkerReviewEvent::Dispatched {
                        generation: initial.generation,
                        attempt: 1,
                    })
                    .await;
                    send(WorkerReviewEvent::Published {
                        generation: initial.generation,
                        plan: zevria_workflow::AgentStructuredPlan {
                            plan_id: None,
                            markdown: Some("# Proposal\n".into()),
                            entries: vec![],
                        },
                        replay: false,
                    })
                    .await;
                    send(WorkerReviewEvent::Settled {
                        generation: initial.generation,
                        failure: None,
                        connected: true,
                        evidence: Box::new(state.evidence.clone()),
                    })
                    .await;
                }
                for _ in 0..late_updates {
                    send(WorkerReviewEvent::Connection {
                        connected: true,
                        diagnostic: None,
                    })
                    .await;
                }
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => break,
                        command = command_rx.recv() => match command {
                            Some(WorkerActorCommand::Finish { acknowledgement, .. }) => { let _ = acknowledgement.send(Ok(())); break; }
                            Some(WorkerActorCommand::Abandon { outcome, .. }) => {
                                assert!(outcome.is_sanitized_abandonment());
                                abandoned.lock().unwrap().push(worker_id.clone());
                                let _ = tx.send(WorkerActorUpdate { worker_id: worker_id.clone(), event: WorkerReviewEvent::Fatal { error: "late excluded fatal".into() } }).await;
                                break;
                            }
                            Some(WorkerActorCommand::Prompt(input)) => {
                                send(WorkerReviewEvent::Dispatched { generation: input.generation, attempt: 2 }).await;
                                if input.text != "prose only".into() {
                                    send(WorkerReviewEvent::Published { generation: input.generation, plan: zevria_workflow::AgentStructuredPlan { plan_id: None, markdown: Some("# Revised proposal\n".into()), entries: vec![] }, replay: false }).await;
                                }
                                send(WorkerReviewEvent::Settled { generation: input.generation, failure: None, connected: true, evidence: Box::new(state.evidence.clone()) }).await;
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
            });
        }
        Ok(EnsembleReviewExecution {
            commands,
            updates: rx,
            cancellation,
        })
    }
}

async fn next_snapshot(
    receiver: &mut SessionEventReceiver,
    predicate: impl Fn(&WorkerReviewState) -> bool,
) -> (WorkerControlTarget, Box<WorkerReviewState>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(SessionUpdate::Lifecycle(SessionEvent::WorkerReviewUpdated {
                target,
                state,
            })) = receiver.recv().await
                && predicate(&state)
            {
                return (target, state);
            }
        }
    })
    .await
    .expect("review snapshot")
}
fn confirm(target: WorkerControlTarget, state: &WorkerReviewState) -> WorkerControl {
    WorkerControl {
        request_id: WorkerControlId::new(),
        target,
        action: WorkerControlAction::Confirm {
            expected_revision: state.eligible_snapshot().unwrap().revision.clone(),
        },
    }
}

#[tokio::test]
async fn image_synthesis_capacity_rejects_confirmation_before_a_durable_seal() {
    let provider =
        ScriptedProvider::new([]).with_input_counts([Ok(InputTokenCount::Exact(u64::MAX))]);
    let calls = provider.input_count_calls.clone();
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
        limit: 16_384,
        late_updates: 0,
        abandoned: Default::default(),
        unavailable: false,
    });
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[4, 3, 2, 255]).unwrap();
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(image)])
            .unwrap(),
        agents: launcher.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
        ])
        .unwrap();
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(128);
    let cancellation = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, cancellation.clone());
    let runner = tokio::spawn(async move {
        engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await
    });
    let (target, state) =
        next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
    let control = confirm(target, &state);
    router.route(control.clone()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(SessionUpdate::Lifecycle(SessionEvent::WorkerControlResult { result })) =
                receiver.recv().await
                && result.control.request_id == control.request_id
            {
                break result;
            }
        }
    })
    .await
    .unwrap();
    assert!(!result.accepted);
    assert!(
        result.detail.contains("input-token limit"),
        "{}",
        result.detail
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cancellation.cancel();
    assert!(matches!(runner.await.unwrap(), Err(Failure::Cancelled)));
    let items = zevria_transcript::transcript::load(&path).unwrap();
    assert!(!items.iter().any(|item| matches!(
        item,
        TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { .. })
    )));
    assert!(
        zevria_transcript::project_worker_reviews(&items)
            .unwrap()
            .values()
            .flatten()
            .all(|state| state.confirmation.is_none())
    );
}

#[tokio::test]
async fn ensemble_interactive_barrier_requires_every_exact_explicit_receipt() {
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
        limit: 16_384,
        late_updates: 0,
        abandoned: Default::default(),
        unavailable: false,
    });
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan it".into(),
        agents: launcher.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
        ])
        .unwrap();
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(128);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, CancellationToken::new());
    let runner = tokio::spawn(async move {
        let result = engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await;
        (engine, result)
    });
    let first = next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
    let second = next_snapshot(&mut receiver, |state| {
        state.eligible_snapshot().is_some() && state.descriptor.id != first.0.worker_id
    })
    .await;
    assert!(requests.lock().unwrap().is_empty());
    assert!(!runner.is_finished());
    let first_confirmation = confirm(first.0.clone(), &first.1);
    router.route(first_confirmation.clone()).unwrap();
    next_snapshot(&mut receiver, |state| {
        state.status() == AgentRunStatus::Confirmed
    })
    .await;
    assert!(!runner.is_finished());
    // A duplicate accepted request returns the original result, not another receipt.
    router.route(first_confirmation).unwrap();
    let feedback = WorkerControl {
        request_id: WorkerControlId::new(),
        target: first.0.clone(),
        action: WorkerControlAction::SendFeedback {
            text: "revise".into(),
        },
    };
    router.route(feedback.clone()).unwrap();
    let revised = next_snapshot(&mut receiver, |state| {
        state.descriptor.id == first.0.worker_id
            && state.accepted_generation == 2
            && state.eligible_snapshot().is_some()
    })
    .await;
    assert!(revised.1.confirmation.is_none());
    // Deduplicated accepted feedback must not allocate generation 3.
    router.route(feedback).unwrap();
    router.route(confirm(second.0.clone(), &second.1)).unwrap();
    next_snapshot(&mut receiver, |state| {
        state.descriptor.id == second.0.worker_id && state.status() == AgentRunStatus::Confirmed
    })
    .await;
    assert!(!runner.is_finished());
    router.route(confirm(revised.0, &revised.1)).unwrap();
    let (engine, outcomes) = tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
    let outcomes = outcomes.unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.confirmation.is_some())
    );
    assert!(requests.lock().unwrap().is_empty());
    let items = zevria_transcript::transcript::load(&path).unwrap();
    assert_eq!(items, engine.conversation.items());
    zevria_transcript::validate_ensemble_review_history(&items).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(
                item,
                TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { .. })
            ))
            .count(),
        1
    );
    assert!(router.route(confirm(first.0, &first.1)).is_err());
}

#[tokio::test]
async fn ensemble_seal_drains_bounded_worker_updates_before_terminal_acknowledgements() {
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
        limit: 16_384,
        late_updates: 200,
        abandoned: Default::default(),
        unavailable: false,
    });
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan it".into(),
        agents: launcher.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
        ])
        .unwrap();
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(128);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, CancellationToken::new());
    let runner = tokio::spawn(async move {
        engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await
    });
    let first = next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
    let second = next_snapshot(&mut receiver, |state| {
        state.eligible_snapshot().is_some() && state.descriptor.id != first.0.worker_id
    })
    .await;
    router.route(confirm(first.0, &first.1)).unwrap();
    router.route(confirm(second.0, &second.1)).unwrap();
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let outcomes = tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .expect("bounded late telemetry must not deadlock the seal")
        .unwrap()
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.confirmation.is_some())
    );
    drain.await.unwrap();
}

#[tokio::test]
async fn ensemble_payload_ineligibility_is_durable_before_confirmation() {
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
        limit: 128,
        late_updates: 0,
        abandoned: Default::default(),
        unavailable: false,
    });
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan it".into(),
        agents: launcher.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
        ])
        .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    let cancellation = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, cancellation.clone());
    let runner = tokio::spawn(async move {
        engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await
    });
    let (_, state) = next_snapshot(&mut receiver, |state| state.synthesis_error.is_some()).await;
    assert!(state.retained.is_some());
    assert!(state.eligible_snapshot().is_none());
    assert_eq!(state.status(), AgentRunStatus::Blocked);
    assert!(state.synthesis_error.as_deref().unwrap().contains("128"));
    cancellation.cancel();
    assert!(matches!(runner.await.unwrap(), Err(Failure::Cancelled)));
    let items = zevria_transcript::transcript::load(&path).unwrap();
    assert!(items.iter().any(|item| matches!(item, TranscriptItem::Ensemble(EnsembleRecord::WorkerReview { event, .. }) if matches!(event.as_ref(), WorkerReviewEvent::PayloadChecked { error: Some(_) }))));
    assert!(!items.iter().any(|item| matches!(
        item,
        TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { .. })
    )));
}

#[tokio::test]
async fn ensemble_root_cancellation_returns_results_for_every_routed_control() {
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
        limit: 16_384,
        late_updates: 0,
        abandoned: Default::default(),
        unavailable: false,
    });
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan it".into(),
        agents: launcher.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
        ])
        .unwrap();
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(128);
    let cancellation = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, cancellation.clone());
    let runner = tokio::spawn(async move {
        engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await
    });
    let (target, _) =
        next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
    let mut pending = std::collections::HashSet::new();
    for n in 0..4 {
        let control = WorkerControl {
            request_id: WorkerControlId::new(),
            target: target.clone(),
            action: WorkerControlAction::SendFeedback {
                text: format!("queued draft {n}").into(),
            },
        };
        pending.insert(control.request_id.clone());
        router.route(control).unwrap();
    }
    cancellation.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !pending.is_empty() {
            if let Some(SessionUpdate::Lifecycle(SessionEvent::WorkerControlResult { result })) =
                receiver.recv().await
            {
                assert!(!result.accepted);
                pending.remove(&result.control.request_id);
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(runner.await.unwrap(), Err(Failure::Cancelled)));
}
