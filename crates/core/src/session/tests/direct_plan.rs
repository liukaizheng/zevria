use super::*;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

struct DirectLauncher {
    stub: StubEnsembleLauncher,
    markdown: String,
    limit: usize,
    finalizations: Arc<AtomicUsize>,
}
impl DirectLauncher {
    fn new(markdown: &str) -> Self {
        Self {
            stub: StubEnsembleLauncher::successful(["report prose is NOT the plan"]),
            markdown: markdown.into(),
            limit: 1024 * 1024,
            finalizations: Default::default(),
        }
    }
}
impl EnsembleLauncher for DirectLauncher {
    fn workers(
        &self,
        workflow: EnsembleWorkflow,
    ) -> anyhow::Result<Vec<zevria_workflow::AgentRunDescriptor>> {
        self.stub.workers(workflow)
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
        Box::pin(async { panic!("direct Plan must use review, not ACP redispatch") })
    }
    fn finalize_review<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        outcomes: Vec<AgentRunOutcome>,
        _: SessionEventSender,
        _: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        assert!(request.resume);
        self.finalizations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(outcomes) })
    }
    fn start_review(
        &self,
        request: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        let mut execution = self.stub.start_review(request, states, events, turn)?;
        let (tx, rx) = mpsc::channel(32);
        let markdown = self.markdown.clone();
        let commands = execution.commands.clone();
        let cancellation = execution.cancellation.clone();
        tokio::spawn(async move {
            while let Some(mut update) = execution.updates.recv().await {
                if let WorkerReviewEvent::Published { plan, .. } = &mut update.event {
                    plan.markdown = Some(markdown.clone());
                }
                if tx.send(update).await.is_err() {
                    break;
                }
            }
        });
        Ok(EnsembleReviewExecution {
            commands,
            updates: rx,
            cancellation,
        })
    }
}

fn run_command(prompt: zevria_content::UserPrompt) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::RunEnsemble {
        workflow: EnsembleWorkflow::Plan,
        prompt,
    })
}

fn recovery(items: &[TranscriptItem]) -> EnsembleRecovery {
    latest_ensemble_recovery(items.iter().filter_map(|item| match item {
        TranscriptItem::Ensemble(record) => Some(record),
        _ => None,
    }))
    .unwrap()
}

fn assert_one_publication(items: &[TranscriptItem], markdown: &str) -> PlanArtifact {
    zevria_transcript::validate_session_replay(items).unwrap();
    let publications = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Plan(PlanRecord::Published {
                artifact,
                provenance:
                    PlanPublicationProvenance::ConfirmedWorker {
                        run_id,
                        worker_id,
                        revision,
                    },
            }) => Some((artifact, run_id, worker_id, revision)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(publications.len(), 1);
    let (artifact, run_id, worker_id, revision) = publications[0];
    assert_eq!(artifact.markdown.as_bytes(), markdown.as_bytes());
    assert_eq!(artifact.version.revision, 1);
    let frozen = items
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
                run_id: sealed,
                outcomes,
                ..
            }) if sealed == run_id => Some(&outcomes[0]),
            _ => None,
        })
        .unwrap();
    assert_eq!(worker_id, &frozen.descriptor.id);
    assert_eq!(
        revision,
        &frozen.confirmation.as_ref().unwrap().snapshot.revision
    );
    assert!(!items.iter().any(|item| matches!(
        item,
        TranscriptItem::Plan(PlanRecord::Ready { .. } | PlanRecord::Handoff { .. })
            | TranscriptItem::ProviderMessage(_)
            | TranscriptItem::ToolResults { .. }
    )));
    artifact.clone()
}

#[tokio::test]
async fn direct_plan_preserves_loose_markdown_projection_and_explicit_handoffs_without_root_calls()
{
    for markdown in [
        "Do this.  ",
        "# Go",
        "\r\n  # Small\r\n\r\n- Do this.  \r\n",
        "\n\tNo heading or sections.\r\n- Exact bytes  ",
    ] {
        for baseline in [false, true] {
            for decision in [PlanDecision::ImplementCurrent, PlanDecision::ImplementFresh] {
                let provider = ScriptedProvider::new([Ok(Message::assistant(
                    "Explicitly authorized implementation.",
                ))])
                .with_input_counts([Ok(InputTokenCount::Exact(u64::MAX))]);
                let requests = provider.requests.clone();
                let counts = provider.input_count_calls.clone();
                let (_directory, transcript) = test_transcript();
                let path = transcript.path().to_path_buf();
                let workspace = tempfile::tempdir().unwrap();
                let plans = workspace.path().join("plans");
                let launcher = Arc::new(DirectLauncher::new(markdown));
                let mut engine = SessionEngine::new(
                    provider,
                    ToolServer::new().run(),
                    test_policies(),
                    transcript,
                    Arc::new(SkillCatalog::default()),
                )
                .unwrap()
                .with_ensemble_launcher(launcher)
                .with_plans_dir(plans.clone());
                let (events, mut receiver) = session_event_channel(512);
                let prompt = zevria_content::UserPrompt::new(vec![
                    zevria_content::PromptBlock::Text("Plan this image".into()),
                    zevria_content::PromptBlock::Image(
                        zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
                    ),
                ])
                .unwrap();
                explicitly_review_proposals(&mut engine, run_command(prompt), &events, baseline)
                    .await
                    .unwrap();
                assert!(requests.lock().unwrap().is_empty());
                assert_eq!(
                    counts.load(Ordering::SeqCst),
                    0,
                    "direct image evidence must not request root token admission"
                );
                let items = zevria_transcript::transcript::load(&path).unwrap();
                let artifact = assert_one_publication(&items, markdown);
                assert!(
                    matches!(engine.plan_state().unwrap(), PlanWorkflowState::Published { artifact: current } if current == &artifact)
                );
                let projection = plans.join(engine.conversation.session_id()).join(format!(
                    "{}-{}.md",
                    artifact.version.id,
                    slug_words(&artifact.title).unwrap()
                ));
                assert_eq!(std::fs::read(&projection).unwrap(), markdown.as_bytes());
                std::fs::write(&projection, "Manual edits must never change the handoff.").unwrap();
                let emitted = collect_events(&mut receiver).await;
                assert!(!emitted.iter().any(|event| matches!(
                    event,
                    SessionEvent::ModelCallStarted { .. }
                        | SessionEvent::Intermediate { .. }
                        | SessionEvent::ToolResults { .. }
                        | SessionEvent::PlanStateChanged {
                            state: PlanWorkflowState::Ready { .. }
                        }
                )));
                assert!(
                    emitted
                        .iter()
                        .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
                );
                let reports = items
                    .iter()
                    .find_map(|item| match item {
                        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                            synthesis_input,
                            agents,
                            ..
                        }) => Some((synthesis_input, agents)),
                        _ => None,
                    })
                    .unwrap();
                assert!(zevria_content::prompt::message_has_images(reports.0));
                assert_eq!(
                    reports.1[0]
                        .confirmation
                        .as_ref()
                        .unwrap()
                        .baseline
                        .is_some(),
                    baseline
                );
                // Publication is not implementation consent. Even the later command is version checked.
                engine
                    .handle_command(
                        SessionCommand::Turn(TurnCommand::ResolvePlan {
                            expected: PlanVersion {
                                revision: 2,
                                ..artifact.version
                            },
                            decision,
                        }),
                        &events,
                    )
                    .await
                    .unwrap();
                assert!(matches!(
                    engine.plan_state().unwrap(),
                    PlanWorkflowState::Published { .. }
                ));
                assert!(requests.lock().unwrap().is_empty());
                // Do not let a deliberately impossible count influence the separately authorized Build turn.
                engine.provider.input_counts.clear();
                engine
                    .handle_command(
                        SessionCommand::Turn(TurnCommand::ResolvePlan {
                            expected: artifact.version,
                            decision,
                        }),
                        &events,
                    )
                    .await
                    .unwrap();
                let emitted = collect_events(&mut receiver).await;
                let handoff = emitted
                    .iter()
                    .find_map(|event| match event {
                        SessionEvent::FreshPlanHandoffRequested { handoff }
                        | SessionEvent::PlanHandoffStarted { handoff, .. } => Some(handoff),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(handoff.artifact, artifact);
                assert_eq!(
                    handoff.prompt,
                    PlanHandoff::new(artifact, engine.conversation.session_id()).prompt
                );
                assert!(handoff.is_canonical());
                assert_eq!(
                    requests.lock().unwrap().len(),
                    usize::from(decision == PlanDecision::ImplementCurrent)
                );
                if decision == PlanDecision::ImplementFresh {
                    let provider =
                        ScriptedProvider::new([Ok(Message::assistant("Fresh implementation."))]);
                    let requests = provider.requests.clone();
                    let (_fresh_directory, transcript) = test_transcript();
                    let mut fresh = SessionEngine::new(
                        provider,
                        ToolServer::new().run(),
                        test_policies(),
                        transcript,
                        Arc::new(SkillCatalog::default()),
                    )
                    .unwrap();
                    fresh
                        .handle_command(
                            SessionCommand::Turn(TurnCommand::StartFromPlan {
                                handoff: handoff.clone(),
                            }),
                            &events,
                        )
                        .await
                        .unwrap();
                    assert_eq!(requests.lock().unwrap()[0].prompt, handoff.prompt);
                    assert!(fresh.conversation.items().iter().any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { handoff: saved }) if saved == handoff)));
                }
            }
        }
    }
}

#[tokio::test]
async fn direct_plan_recovery_at_every_durable_boundary_uses_saved_selection_and_never_redispatches()
 {
    let markdown = "\r\nConfirmed exact bytes.  ";
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_ensemble_launcher(Arc::new(DirectLauncher::new(markdown)));
    let (events, _receiver) = session_event_channel(512);
    explicitly_confirm_proposals(&mut engine, run_command("plan it".into()), &events)
        .await
        .unwrap();
    let complete = engine.conversation.items().to_vec();
    let original = assert_one_publication(&complete, markdown);
    for boundary in ["seal", "reports", "published"] {
        let index = complete
            .iter()
            .position(|item| {
                matches!(
                    (boundary, item),
                    (
                        "seal",
                        TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { .. })
                    ) | (
                        "reports",
                        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
                    ) | (
                        "published",
                        TranscriptItem::Plan(PlanRecord::Published { .. })
                    )
                )
            })
            .unwrap();
        let items = complete[..=index].to_vec();
        let provider = ScriptedProvider::new([]);
        let requests = provider.requests.clone();
        let counts = provider.input_count_calls.clone();
        // Current configuration is deliberately different from the persisted start.
        let mut launcher = DirectLauncher::new("Never reread this mutable plan");
        launcher.stub = StubEnsembleLauncher::successful(["new one", "new two", "new three"]);
        let launches = launcher.stub.launches.clone();
        let queries = launcher.stub.worker_queries.clone();
        let finalizations = launcher.finalizations.clone();
        let (_dir, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let path = transcript.path().to_path_buf();
        let mut restored = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_ensemble_launcher(Arc::new(launcher));
        let turn = TurnContext::new(TurnId::new(7), SessionMode::Plan, CancellationToken::new());
        restored
            .resume_ensemble(recovery(&items), &events, &turn)
            .await
            .unwrap();
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(counts.load(Ordering::SeqCst), 0);
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        assert_eq!(queries.load(Ordering::SeqCst), 0);
        assert_eq!(
            finalizations.load(Ordering::SeqCst),
            usize::from(boundary == "seal")
        );
        let persisted = zevria_transcript::transcript::load(&path).unwrap();
        let artifact = assert_one_publication(&persisted, markdown);
        assert_eq!(artifact.version, original.version);
        if boundary == "published" {
            assert_eq!(artifact, original);
            assert_eq!(
                persisted.len(),
                items.len() + 1,
                "only missing completion is appended"
            );
        }
        assert_eq!(
            persisted
                .iter()
                .filter(|item| matches!(
                    item,
                    TranscriptItem::Ensemble(EnsembleRecord::Completed { .. })
                ))
                .count(),
            1
        );
        assert!(
            latest_ensemble_recovery(persisted.iter().filter_map(|item| match item {
                TranscriptItem::Ensemble(record) => Some(record),
                _ => None,
            }))
            .is_none()
        );
    }
}

#[tokio::test]
async fn direct_plan_oversize_is_review_error_before_sealing_and_feedback_stays_available() {
    let markdown = "x".repeat(MAX_PLAN_ARTIFACT_BYTES + 1);
    let launcher = Arc::new(DirectLauncher::new(&markdown));
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_ensemble_launcher(launcher);
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(128);
    let cancel = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, cancel.clone());
    let runner = tokio::spawn(async move {
        engine
            .run_ensemble(
                TurnAnchor::Append,
                EnsembleWorkflow::Plan,
                "plan it".into(),
                &events,
                &turn,
            )
            .await
            .unwrap();
        engine
    });
    let (target, state) =
        next_snapshot(&mut receiver, |state| state.synthesis_error.is_some()).await;
    let error = state.synthesis_error.as_ref().unwrap();
    assert!(
        error.contains("131072")
            && error.contains("feedback")
            && error.contains("not be truncated"),
        "{error}"
    );
    assert!(state.eligible_snapshot().is_none());
    let confirm = WorkerControl {
        request_id: WorkerControlId::new(),
        target: target.clone(),
        action: WorkerControlAction::Confirm {
            expected_revision: state.retained.as_ref().unwrap().revision.clone(),
        },
    };
    router.route(confirm.clone()).unwrap();
    let rejected = control_result(&mut receiver, &confirm.request_id).await;
    assert!(!rejected.accepted);
    assert!(rejected.detail.contains("final artifact limit"));
    let feedback = WorkerControl {
        request_id: WorkerControlId::new(),
        target,
        action: WorkerControlAction::SendFeedback {
            text: "Shorten the plan without truncating evidence.".into(),
        },
    };
    router.route(feedback.clone()).unwrap();
    assert!(
        control_result(&mut receiver, &feedback.request_id)
            .await
            .accepted
    );
    cancel.cancel();
    let engine = runner.await.unwrap();
    assert!(requests.lock().unwrap().is_empty());
    assert!(!engine.conversation.items().iter().any(|item| matches!(
        item,
        TranscriptItem::Ensemble(
            EnsembleRecord::WorkersConfirmed { .. } | EnsembleRecord::ReportsReady { .. }
        ) | TranscriptItem::Plan(PlanRecord::Published { .. })
    )));
}

#[tokio::test]
async fn two_selected_workers_with_one_abandoned_still_use_all_root_synthesis_gates() {
    let title = "Synthesized surviving worker plan";
    let markdown = valid_plan_markdown(
        title,
        "The original selection, not survivors, controls synthesis.",
    );
    let provider = ScriptedProvider::new([
        Ok(command_call("inspection", "rtk rg -n plan crates/core/src")),
        Ok(reconciliation_call(
            "reconciliation",
            no_disagreement_reconciliation(),
        )),
        Ok(submit_plan_call(title, &markdown)),
        Ok(Message::assistant("Synthesized.")),
    ]);
    let requests = provider.requests.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_directory, transcript) = test_transcript();
    let launcher = Arc::new(InteractiveLauncher {
        limit: 16_384,
        late_updates: 0,
        abandoned: Default::default(),
        unavailable: false,
    });
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new()
            .tool(CommandTestTool {
                calls: calls.clone(),
            })
            .tool(ReconcileReportsStubTool)
            .tool(SubmitPlanStubTool)
            .run(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_ensemble_launcher(launcher);
    let router = engine.capabilities.worker_controls.clone();
    let (events, mut receiver) = session_event_channel(256);
    let runner = tokio::spawn(async move {
        engine
            .handle_command(run_command("plan it".into()), &events)
            .await
            .unwrap();
        engine
    });
    let a = next_snapshot(&mut receiver, |state| state.eligible_snapshot().is_some()).await;
    let b = next_snapshot(&mut receiver, |state| {
        state.descriptor.id != a.0.worker_id && state.eligible_snapshot().is_some()
    })
    .await;
    router
        .route(WorkerControl {
            request_id: WorkerControlId::new(),
            target: b.0,
            action: WorkerControlAction::Abandon,
        })
        .unwrap();
    router.route(confirm(a.0, &a.1)).unwrap();
    let engine = runner.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(engine.conversation.items().iter().any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Published { artifact, provenance: PlanPublicationProvenance::Synthesized }) if artifact.markdown == markdown)));
    zevria_transcript::validate_session_replay(engine.conversation.items()).unwrap();
}

#[tokio::test]
async fn direct_publication_projection_warning_and_retained_persistence_keep_the_exact_plan() {
    let markdown = "No heading.\r\nDo not add a newline.  ";
    let (_directory, transcript) = test_transcript();
    let mut original = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_ensemble_launcher(Arc::new(DirectLauncher::new(markdown)));
    let (events, _receiver) = session_event_channel(512);
    explicitly_confirm_proposals(&mut original, run_command("plan".into()), &events)
        .await
        .unwrap();
    let mut items = original.conversation.items().to_vec();
    let reports = items
        .iter()
        .position(|item| {
            matches!(
                item,
                TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
            )
        })
        .unwrap();
    items.truncate(reports + 1);
    for fail_persistence in [false, true] {
        let (directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let path = transcript.path().to_path_buf();
        let blocker = directory.path().join("blocked-plan-dir");
        std::fs::write(&blocker, "not a directory").unwrap();
        let provider = ScriptedProvider::new([]);
        let requests = provider.requests.clone();
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_plans_dir(blocker);
        let before = std::fs::read(&path).unwrap();
        let mut rewrite_blocker = fail_persistence
            .then(|| TranscriptRewriteBlocker::new(&path).expect("block transcript replacement"));
        let (events, mut receiver) = session_event_channel(128);
        let turn = TurnContext::new(TurnId::new(9), SessionMode::Plan, CancellationToken::new());
        engine
            .resume_ensemble(recovery(&items), &events, &turn)
            .await
            .unwrap();
        let artifact = assert_one_publication(engine.conversation.items(), markdown);
        assert!(
            matches!(engine.plan_state().unwrap(), PlanWorkflowState::Published { artifact: current } if current == &artifact)
        );
        assert_eq!(
            engine.conversation.persistence_error().is_some(),
            fail_persistence
        );
        let emitted = collect_events(&mut receiver).await;
        let completed = emitted
            .iter()
            .position(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
            .unwrap();
        let published = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    SessionEvent::PlanStateChanged {
                        state: PlanWorkflowState::Published { .. }
                    }
                )
            })
            .unwrap();
        let warning = emitted
            .iter()
            .position(|event| matches!(event, SessionEvent::PlanProjectionWarning { .. }))
            .unwrap();
        assert!(completed < published && published < warning);
        assert_eq!(
            emitted.iter().any(|event| matches!(
                event,
                SessionEvent::PersistenceChanged { error: Some(_), .. }
            )),
            fail_persistence
        );
        if let Some(blocker) = &mut rewrite_blocker {
            assert_eq!(std::fs::read(blocker.backup_path()).unwrap(), before);
            // No implementation can be accepted until the retained completion is durable.
            engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::ResolvePlan {
                        expected: artifact.version,
                        decision: PlanDecision::ImplementFresh,
                    }),
                    &events,
                )
                .await
                .unwrap();
            assert!(matches!(
                engine.plan_state().unwrap(),
                PlanWorkflowState::Published { .. }
            ));
            blocker.restore().expect("restore transcript filename");
            assert!(engine.conversation.ensure_durable().unwrap());
        }
        assert_eq!(
            zevria_transcript::transcript::load(&path).unwrap(),
            engine.conversation.items()
        );
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::ResolvePlan {
                    expected: artifact.version,
                    decision: PlanDecision::ImplementFresh,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(collect_events(&mut receiver).await.iter().any(|event| matches!(event, SessionEvent::FreshPlanHandoffRequested { handoff } if handoff.artifact == artifact)));
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn direct_plan_requires_current_explicit_confirmation_not_publication_or_prompt_end() {
    for failure in ["stale", "proofless", "failed", "abandoned"] {
        let mut launcher = DirectLauncher::new("Unconfirmed plan");
        if failure == "proofless" {
            launcher.stub.plan_proof = false;
        }
        if failure == "failed" {
            launcher.stub = StubEnsembleLauncher::all_failed(1);
        }
        let provider = ScriptedProvider::new([]);
        let requests = provider.requests.clone();
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_ensemble_launcher(Arc::new(launcher));
        let router = engine.capabilities.worker_controls.clone();
        let (events, mut receiver) = session_event_channel(128);
        let cancellation = CancellationToken::new();
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, cancellation.clone());
        let runner = tokio::spawn(async move {
            engine
                .run_ensemble(
                    TurnAnchor::Append,
                    EnsembleWorkflow::Plan,
                    "plan".into(),
                    &events,
                    &turn,
                )
                .await
                .unwrap();
            engine
        });
        let (target, state) = next_snapshot(&mut receiver, |state| {
            state.accepted_generation == 1 && state.quiescent()
        })
        .await;
        assert!(
            !runner.is_finished(),
            "prompt end alone is never confirmation"
        );
        let action = if failure == "abandoned" {
            WorkerControlAction::Abandon
        } else {
            let mut revision = state
                .retained
                .as_ref()
                .map(|snapshot| snapshot.revision.clone())
                .unwrap_or(WorkerPlanRevision {
                    worker_id: target.worker_id.clone(),
                    generation: 1,
                    revision: 1,
                    digest: "not proof".into(),
                });
            revision.revision += 1;
            WorkerControlAction::Confirm {
                expected_revision: revision,
            }
        };
        let control = WorkerControl {
            request_id: WorkerControlId::new(),
            target,
            action,
        };
        router.route(control.clone()).unwrap();
        assert_eq!(
            control_result(&mut receiver, &control.request_id)
                .await
                .accepted,
            failure == "abandoned"
        );
        if failure != "abandoned" {
            cancellation.cancel();
        }
        let engine = runner.await.unwrap();
        assert!(requests.lock().unwrap().is_empty());
        assert!(!engine.conversation.items().iter().any(|item| matches!(
            item,
            TranscriptItem::Ensemble(
                EnsembleRecord::WorkersConfirmed { .. } | EnsembleRecord::ReportsReady { .. }
            ) | TranscriptItem::Plan(PlanRecord::Published { .. })
        )));
    }
}

async fn control_result(
    receiver: &mut SessionEventReceiver,
    id: &WorkerControlId,
) -> WorkerControlResult {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(SessionUpdate::Lifecycle(SessionEvent::WorkerControlResult { result })) =
                receiver.recv().await
                && &result.control.request_id == id
            {
                return result;
            }
        }
    })
    .await
    .unwrap()
}
