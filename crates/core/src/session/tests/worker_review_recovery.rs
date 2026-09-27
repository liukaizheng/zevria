//! Recovery seam: real journals and root replay, with a synthetic actor that
//! records delivery and then stops. No provider or subprocess work is needed.
use super::*;
use anyhow::Context as _;
use zevria_transcript::{
    AgentRunTranscriptHeader, AgentRunTranscriptRecord, AgentRunTranscriptWriter,
};
use zevria_workflow::{AgentRunDescriptor, AgentRunEvent, AgentStructuredPlan};

struct RecoveryLauncher {
    path: PathBuf,
    starts: Arc<AtomicUsize>,
    delivered: Arc<Mutex<Vec<WorkerInput>>>,
    restored: Arc<Mutex<Vec<WorkerReviewState>>>,
}
impl EnsembleLauncher for RecoveryLauncher {
    fn workers(&self, _: EnsembleWorkflow) -> anyhow::Result<Vec<AgentRunDescriptor>> {
        unreachable!("restoring an existing worker")
    }
    fn max_synthesis_bytes_per_agent(&self) -> usize {
        16_384
    }
    fn launch<'a>(
        &'a self,
        _: EnsembleLaunchRequest,
        _: SessionEventSender,
        _: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        unreachable!("interactive review only")
    }
    fn recover_review(
        &self,
        _: &EnsembleStart,
        history: &[(AgentRunId, WorkerReviewEvent)],
    ) -> anyhow::Result<Vec<WorkerActorUpdate>> {
        let projection = zevria_transcript::load_agent_run_projection(&self.path)
            .context("worker journal preflight")?;
        let journal = projection.review.unwrap();
        let worker_id = journal.state.descriptor.id.clone();
        let root = history
            .iter()
            .filter(|(id, _)| id == &worker_id)
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();
        Ok(journal
            .reconcile(&root)
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|event| WorkerActorUpdate {
                worker_id: worker_id.clone(),
                event,
            })
            .collect())
    }
    fn start_review(
        &self,
        _: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        _: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.restored.lock().unwrap().extend(states.clone());
        let mut writer = AgentRunTranscriptWriter::append_to(self.path.clone())?;
        let previous = zevria_transcript::load_agent_run_projection(&self.path)?
            .review
            .unwrap();
        let (tx, rx) = mpsc::channel(32);
        for state in states {
            // Mirror the same root/actor boundary used by the real supervisor.
            if let Some(active) = &previous.state.active
                && state.active.is_none()
                && state.settled_generation >= active.generation
            {
                writer.append(&AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::Interrupted {
                            generation: active.generation,
                        }),
                    },
                })?;
            }
            for input in &state.pending {
                self.delivered.lock().unwrap().push(input.clone());
                let dispatched = WorkerReviewEvent::Dispatched {
                    generation: input.generation,
                    attempt: state.attempt + 1,
                };
                for event in [
                    AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::InputAccepted {
                            input: input.clone(),
                        }),
                    },
                    AgentRunEvent::Review {
                        event: Box::new(dispatched.clone()),
                    },
                    AgentRunEvent::Prompt {
                        text: input.text.display_projection(),
                        continuation: false,
                        repair: None,
                    },
                ] {
                    writer.append(&AgentRunTranscriptRecord::Event { event })?;
                }
                tx.try_send(WorkerActorUpdate {
                    worker_id: state.descriptor.id.clone(),
                    event: dispatched,
                })
                .unwrap();
            }
            tx.try_send(WorkerActorUpdate {
                worker_id: state.descriptor.id,
                event: WorkerReviewEvent::Fatal {
                    error: "synthetic runtime interruption".into(),
                },
            })
            .unwrap();
        }
        Ok(EnsembleReviewExecution {
            commands: HashMap::new(),
            updates: rx,
            cancellation: turn.cancellation().child_token(),
        })
    }
}

// 0: root accepted only; 1: worker acceptance mirrored; 2: dispatched but not
// settled, with dispatch not yet published back to the root at the crash.
fn recovery_fixture(
    boundary: u8,
) -> (
    tempfile::TempDir,
    SessionEngine<ScriptedProvider>,
    EnsembleStart,
    PathBuf,
) {
    let (directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "test".into(),
        label: "Test".into(),
        safe_mode: "read-only".into(),
    };
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "initial proposal".into(),
        agents: vec![descriptor.clone()],
    };
    let path = directory.path().join("worker.jsonl");
    let header = AgentRunTranscriptHeader {
        version: zevria_transcript::AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: start.run_id.clone(),
        workflow: start.workflow,
        descriptor: descriptor.clone(),
        prompt: start.prompt.clone(),
    };
    let mut writer = AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap();
    let mut journal = zevria_transcript::WorkerReviewJournal::new(&header);
    let initial = WorkerReviewEvent::InputAccepted {
        input: WorkerInput {
            generation: 1,
            request_id: WorkerControlId::new(),
            kind: WorkerPromptKind::Initial,
            text: start.prompt.clone(),
        },
    };
    for event in [
        AgentRunEvent::Review {
            event: Box::new(initial.clone()),
        },
        AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            }),
        },
        AgentRunEvent::Prompt {
            text: start.prompt.display_projection(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("retained".into()),
                markdown: Some("# Retained proposal".into()),
                entries: vec![],
            },
        },
    ] {
        let record = AgentRunTranscriptRecord::Event { event };
        writer.append(&record).unwrap();
        journal.apply(&record).unwrap();
    }
    let mut evidence = journal.state.evidence.clone();
    evidence.plan = journal
        .state
        .candidate
        .as_ref()
        .map(|snapshot| snapshot.plan.clone());
    let record = AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::Settled {
                generation: 1,
                failure: None,
                connected: true,
                evidence: Box::new(evidence),
            }),
        },
    };
    writer.append(&record).unwrap();
    journal.apply(&record).unwrap();
    let mut root = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: ENSEMBLE_REVIEW_VERSION,
        }),
    ];
    for event in std::iter::once(initial).chain(journal.events.clone()) {
        root.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: descriptor.id.clone(),
            event: Box::new(event),
            result: None,
        }));
    }
    let input = WorkerInput {
        generation: 2,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: "accepted feedback".into(),
    };
    let accepted = WorkerReviewEvent::InputAccepted {
        input: input.clone(),
    };
    root.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: descriptor.id.clone(),
        event: Box::new(accepted.clone()),
        result: Some(WorkerControlResult {
            control: WorkerControl {
                request_id: input.request_id.clone(),
                target: WorkerControlTarget {
                    turn_id: TurnId::new(1),
                    run_id: start.run_id.clone(),
                    worker_id: descriptor.id,
                },
                action: WorkerControlAction::SendFeedback {
                    text: input.text.clone(),
                },
            },
            accepted: true,
            detail: "accepted".into(),
        }),
    }));
    engine.record_required_items(root).unwrap();
    if boundary >= 1 {
        writer
            .append(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Review {
                    event: Box::new(accepted),
                },
            })
            .unwrap();
    }
    if boundary >= 2 {
        for event in [
            AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::Dispatched {
                    generation: 2,
                    attempt: 2,
                }),
            },
            AgentRunEvent::Prompt {
                text: input.text.display_projection(),
                continuation: false,
                repair: None,
            },
        ] {
            writer
                .append(&AgentRunTranscriptRecord::Event { event })
                .unwrap();
        }
    }
    let mut attempt = zevria_content::WebSearchAttemptRecord::new(test_profile());
    attempt.activity.push(zevria_content::WebSearchActivity {
        item_id: None,
        output_index: 1,
        status: zevria_content::WebSearchStatus::Completed,
        action: None,
    });
    attempt
        .terminal
        .insert(1, zevria_content::WebSearchStatus::Completed);
    writer
        .append(&AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::ResponseDisplay {
                display: Box::new(zevria_content::web_search::ResponseDisplay {
                    version: 1,
                    attempt,
                    bindings: vec![],
                }),
            },
        })
        .unwrap();
    (directory, engine, start, path)
}

fn launcher(path: PathBuf) -> Arc<RecoveryLauncher> {
    Arc::new(RecoveryLauncher {
        path,
        starts: Default::default(),
        delivered: Default::default(),
        restored: Default::default(),
    })
}

#[tokio::test]
async fn blocked_worker_preflight_preserves_root_and_starts_no_actors_or_recovery_transitions() {
    let (_directory, mut engine, start, path) = recovery_fixture(2);
    let root_path = engine.conversation.path().to_path_buf();
    let original = std::fs::read_to_string(&path).unwrap().replace(
        r#""terminal":{"1":"completed"}"#,
        r#""terminal":{"PRIVATE_KEY":"completed"}"#,
    );
    std::fs::write(&path, &original).unwrap();
    let root_before = std::fs::read(&root_path).unwrap();
    let items_before = engine.conversation.items().to_vec();
    let launcher = launcher(path.clone());
    let (events, _receiver) = session_event_channel(128);
    let turn = TurnContext::new(TurnId::new(2), SessionMode::Plan, CancellationToken::new());
    let result = engine
        .review_plan_workers(launcher.clone(), &start, true, &events, &turn)
        .await;
    let Err(Failure::Fatal(error)) = result else {
        panic!("preflight must be a fatal replay error")
    };
    let detail = error.to_string();
    assert!(detail.contains("preflight is blocked"));
    assert!(detail.contains("decoding failure"));
    assert!(detail.contains(&path.display().to_string()));
    assert!(detail.contains("Preserve the existing root and worker logs"));
    assert!(!detail.contains("PRIVATE_KEY"));
    assert!(!detail.contains("Restart/resume"));
    assert!(!detail.contains("fresh session"));
    assert!(!detail.contains("older binary"));
    assert_eq!(launcher.starts.load(Ordering::SeqCst), 0);
    assert!(launcher.delivered.lock().unwrap().is_empty());
    assert_eq!(engine.conversation.items(), items_before);
    assert_eq!(std::fs::read(&root_path).unwrap(), root_before);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[tokio::test]
async fn recovered_feedback_is_queued_once_and_durable_dispatch_is_never_resent() {
    for boundary in 0..=2 {
        let (_directory, mut engine, start, path) = recovery_fixture(boundary);
        let launcher = launcher(path.clone());
        let (events, _receiver) = session_event_channel(256);
        for turn_id in [2, 3] {
            let turn = TurnContext::new(
                TurnId::new(turn_id),
                SessionMode::Plan,
                CancellationToken::new(),
            );
            let result = engine
                .review_plan_workers(launcher.clone(), &start, true, &events, &turn)
                .await;
            let Err(Failure::Fatal(error)) = result else {
                panic!("synthetic actor interruption")
            };
            assert!(error.to_string().contains("review is durably interrupted"));
        }
        let delivered = launcher.delivered.lock().unwrap();
        assert_eq!(delivered.len(), usize::from(boundary < 2));
        assert!(
            delivered
                .iter()
                .all(|input| input.generation == 2 && input.text == "accepted feedback".into())
        );
        let restored = launcher.restored.lock().unwrap();
        assert_eq!(restored.len(), 2);
        assert!(restored[1].pending.is_empty());
        assert!(restored[1].active.is_none());
        assert_eq!(restored[1].settled_generation, 2);
        assert!(restored[1].eligible_snapshot().is_some());
        assert!(restored.iter().all(|state| state.confirmation.is_none()));
        let records = zevria_transcript::load_agent_run(&path).unwrap();
        assert_eq!(records.iter().filter(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Prompt { text, .. } } if text == "accepted feedback")).count(), 1);
        assert!(!engine.conversation.items().iter().any(|item| matches!(
            item,
            TranscriptItem::Ensemble(
                EnsembleRecord::Failed { .. } | EnsembleRecord::WorkersConfirmed { .. }
            )
        )));
    }
}
