use super::*;

#[tokio::test]
async fn ensemble_abandonment_seals_survivors_in_either_control_order() {
    for (abandon_first, unavailable) in [(true, false), (false, false), (true, true), (false, true)]
    {
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
        let abandoned = Arc::new(Mutex::new(Vec::new()));
        let launcher: Arc<dyn EnsembleLauncher> = Arc::new(InteractiveLauncher {
            limit: 16_384,
            late_updates: 0,
            abandoned: abandoned.clone(),
            unavailable,
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
        let resume_start = start.clone();
        let resume_launcher = launcher.clone();
        let resume_turn = turn.clone();
        let runner = tokio::spawn(async move {
            let result = engine
                .review_plan_workers(launcher, &start, false, &events, &turn)
                .await;
            (engine, result)
        });
        let a = next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
        let b = next_snapshot(&mut receiver, |state| {
            state.descriptor.id != a.0.worker_id && state.eligible_snapshot().is_some()
        })
        .await;
        let (a, b) = if a.1.descriptor.agent == "worker-0" {
            (a, b)
        } else {
            (b, a)
        };
        let abandon = WorkerControl {
            request_id: WorkerControlId::new(),
            target: b.0.clone(),
            action: WorkerControlAction::Abandon,
        };
        if abandon_first {
            router.route(abandon.clone()).unwrap();
            next_snapshot(&mut receiver, |state| state.abandoned).await;
            assert!(!runner.is_finished());
            router.route(abandon.clone()).unwrap();
            // Both duplicate acceptance and rejection retain exact correlation.
            let invalid = WorkerControl {
                action: WorkerControlAction::Retry,
                ..abandon.clone()
            };
            router.route(invalid.clone()).unwrap();
            let new_control = WorkerControl {
                request_id: WorkerControlId::new(),
                ..invalid.clone()
            };
            router.route(new_control.clone()).unwrap();
            let mut duplicate = false;
            let mut rejected = 0;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !duplicate || rejected < 2 {
                    if let Some(SessionUpdate::Lifecycle(SessionEvent::WorkerControlResult {
                        result,
                    })) = receiver.recv().await
                    {
                        if result.control == abandon {
                            assert!(result.accepted);
                            duplicate = true;
                        }
                        if result.control == invalid || result.control == new_control {
                            assert!(!result.accepted);
                            rejected += 1;
                        }
                    }
                }
            })
            .await
            .unwrap();
            router.route(confirm(a.0.clone(), &a.1)).unwrap();
        } else {
            router.route(confirm(a.0.clone(), &a.1)).unwrap();
            next_snapshot(&mut receiver, |state| {
                state.status() == AgentRunStatus::Confirmed
            })
            .await;
            router.route(abandon.clone()).unwrap();
        }
        let (mut engine, result) = tokio::time::timeout(std::time::Duration::from_secs(5), runner)
            .await
            .unwrap()
            .unwrap();
        let outcomes = result.unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .find(|outcome| outcome.descriptor.id == b.0.worker_id)
                .unwrap()
                .is_sanitized_abandonment()
        );
        assert!(
            outcomes
                .iter()
                .find(|outcome| outcome.descriptor.id == a.0.worker_id)
                .unwrap()
                .confirmation
                .is_some()
        );
        zevria_transcript::validate_ensemble_review_history(engine.conversation.items()).unwrap();
        let (events, mut restored) = session_event_channel(128);
        assert_eq!(
            engine
                .review_plan_workers(resume_launcher, &resume_start, true, &events, &resume_turn)
                .await
                .unwrap(),
            outcomes
        );
        let snapshot = next_snapshot(&mut restored, |state| state.abandoned).await;
        assert_eq!(snapshot.0.worker_id, b.0.worker_id);
        if !unavailable {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while abandoned.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(*abandoned.lock().unwrap(), vec![b.0.worker_id]);
        } else {
            assert!(abandoned.lock().unwrap().is_empty());
        }
        let mut corrupt = engine.conversation.items().to_vec();
        let frozen = corrupt
            .iter_mut()
            .find_map(|item| match item {
                TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { outcomes, .. }) => {
                    Some(outcomes)
                }
                _ => None,
            })
            .unwrap();
        frozen
            .iter_mut()
            .find(|outcome| outcome.status == AgentRunStatus::Abandoned)
            .unwrap()
            .decision_ids
            .push(zevria_workflow::AgentUserDecisionId::from_question(
                &zevria_foundation::QuestionRequestId::generate(),
                "resurrected",
            ));
        assert!(zevria_transcript::validate_ensemble_review_history(&corrupt).is_err());
    }
}

#[tokio::test]
async fn ensemble_all_abandoned_cancels_and_recovery_never_starts_workers() {
    for worker_count in [1, 2] {
        all_abandoned(worker_count).await;
    }
}

async fn all_abandoned(worker_count: usize) {
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
        agents: launcher
            .workers(EnsembleWorkflow::Plan)
            .unwrap()
            .into_iter()
            .take(worker_count)
            .collect(),
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
    let turn = TurnContext::new(TurnId::new(2), SessionMode::Plan, CancellationToken::new());
    let retained_turn = turn.clone();
    let retained_start = start.clone();
    let retained_launcher = launcher.clone();
    let runner = tokio::spawn(async move {
        let result = engine
            .review_plan_workers(launcher, &start, false, &events, &turn)
            .await;
        (engine, result)
    });
    next_snapshot(&mut receiver, |_| true).await;
    for worker in &retained_start.agents {
        router
            .route(WorkerControl {
                request_id: WorkerControlId::new(),
                target: WorkerControlTarget {
                    turn_id: retained_turn.id,
                    run_id: retained_start.run_id.clone(),
                    worker_id: worker.id.clone(),
                },
                action: WorkerControlAction::Abandon,
            })
            .unwrap_or_else(|_| panic!("router must be registered after snapshot"));
        next_snapshot(&mut receiver, |state| {
            state.descriptor.id == worker.id && state.abandoned
        })
        .await;
    }
    let (mut engine, result) = runner.await.unwrap();
    assert!(matches!(result, Err(Failure::Cancelled)));
    assert!(!retained_turn.is_cancelled());
    assert!(!engine.conversation.items().iter().any(|item| matches!(
        item,
        TranscriptItem::Ensemble(
            EnsembleRecord::WorkersConfirmed { .. } | EnsembleRecord::ReportsReady { .. }
        )
    )));
    zevria_transcript::validate_ensemble_review_history(engine.conversation.items()).unwrap();
    let (events, _receiver) = session_event_channel(128);
    assert!(matches!(
        engine
            .review_plan_workers(
                retained_launcher,
                &retained_start,
                true,
                &events,
                &retained_turn
            )
            .await,
        Err(Failure::Cancelled)
    ));
}
