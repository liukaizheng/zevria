#[tokio::test]
async fn native_feedback_recovers_from_missing_payload_and_retains_last_success() {
    let fixture = NativeFixture::new("retry");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "native plan".into(),
        agents: fixture.supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    let mut state = WorkerReviewState::new(start.agents[0].clone());
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: WorkerInput {
                generation: 1,
                request_id: WorkerControlId::new(),
                kind: WorkerPromptKind::Initial,
                text: start.prompt.clone(),
            },
        })
        .unwrap();
    let (events, mut receiver) = session_event_channel(256);
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let mut execution = fixture
        .supervisor
        .start_review(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            vec![state.clone()],
            events,
            TurnContext::new(TurnId::new(92), SessionMode::Plan, CancellationToken::new()),
        )
        .unwrap();
    let mut retained = None;
    for generation in 1..=3 {
        if generation > 1 {
            let input = WorkerInput {
                generation,
                request_id: WorkerControlId::new(),
                kind: WorkerPromptKind::UserFeedback,
                text: "revise the proposal".into(),
            };
            state
                .apply(&WorkerReviewEvent::InputAccepted {
                    input: input.clone(),
                })
                .unwrap();
            execution.commands[&state.descriptor.id]
                .send(WorkerActorCommand::Prompt(input))
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.settled_generation < generation {
                let update = execution.updates.recv().await.unwrap();
                state.apply(&update.event).unwrap();
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!("retry generation {generation}: native round did not settle: {error}; {state:?}")
        });
        assert!(
            state.confirmation.is_none(),
            "capturing is not confirmation"
        );
        match generation {
            1 => {
                assert!(state.eligible_snapshot().is_none());
                assert!(
                    state
                        .diagnostic
                        .as_deref()
                        .is_some_and(|message| message.contains("no eligible completed artifact")),
                    "retry generation {generation}: {state:?}"
                );
            }
            2 => {
                assert!(state.evidence.failure.is_none(), "{state:?}");
                retained = state.eligible_snapshot().cloned();
                assert!(retained.is_some());
            }
            3 => {
                assert_eq!(state.eligible_snapshot(), retained.as_ref());
                assert!(
                    state
                        .diagnostic
                        .as_deref()
                        .is_some_and(|message| message.contains("no eligible completed artifact")),
                    "retry generation {generation}: {state:?}"
                );
            }
            _ => unreachable!(),
        }
    }
    let records = load_agent_run(&agent_run_path(
        &fixture.logs,
        &start.run_id,
        &state.descriptor.id,
    ))
    .unwrap();
    assert_eq!(
        recorded_prompts(&records).len(),
        3,
        "one prompt per accepted input; no repair envelope"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::SessionAllocated { .. }
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::NativePlanCaptured { .. }
                }
            ))
            .count(),
        1
    );
    let projection = AgentRunProjection::from_records(&records);
    assert_eq!(
        projection.review.unwrap().state.eligible_snapshot(),
        retained.as_ref()
    );
    execution.cancellation.cancel();
    for sender in execution.commands.values() {
        sender.closed().await;
    }
    drain.abort();
}

#[tokio::test]
async fn native_cancellation_after_rejection_never_approves_or_captures() {
    let fixture = NativeFixture::new("timeout");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "cancel native settlement".into(),
        agents: fixture.supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    let cancellation = CancellationToken::new();
    let turn = TurnContext::new(TurnId::new(93), SessionMode::Plan, cancellation.clone());
    let (events, mut receiver) = session_event_channel(256);
    let cancel = tokio::spawn(async move {
        while let Some(update) = receiver.recv().await {
            if matches!(update, zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated { event: AgentRunEvent::Permission { ref decision, .. }, .. }) if decision == "reject_once")
            {
                cancellation.cancel();
                return;
            }
        }
        panic!("native rejection was not delivered");
    });
    let outcomes = fixture
        .supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            turn,
        )
        .await
        .unwrap();
    let records = load_agent_run(&agent_run_path(
        &fixture.logs,
        &start.run_id,
        &outcomes[0].descriptor.id,
    ))
    .unwrap();
    cancel.await.unwrap_or_else(|error| {
        panic!("cancellation after rejection: {error}; {}", fixture_diagnostics(&outcomes[0], &records))
    });
    assert_eq!(
        outcomes[0].status,
        AgentRunStatus::Cancelled,
        "cancellation after rejection: {}",
        fixture_diagnostics(&outcomes[0], &records)
    );
    assert!(outcomes[0].confirmation.is_none());
    assert!(!records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::NativePlanCaptured { .. }
        }
    )));
    assert_eq!(recorded_prompts(&records).len(), 1);
}

struct NativeSyncWriter {
    sync: usize,
    fail_at: Option<usize>,
    gate: Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
impl RunLogWriter for NativeSyncWriter {
    fn append_buffered(&mut self, _record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        Ok(1)
    }
    fn sync(&mut self) -> anyhow::Result<()> {
        self.sync += 1;
        if self.sync == 2
            && let Some((entered, release)) = self.gate.take()
        {
            let _ = entered.send(());
            release.recv_timeout(Duration::from_secs(5))?;
        }
        anyhow::ensure!(
            self.fail_at != Some(self.sync),
            "scripted native sync failure"
        );
        Ok(())
    }
}

fn native_test_log(writer: NativeSyncWriter) -> (RunLog, zevria_session_api::SessionEventReceiver) {
    let (events, receiver) = session_event_channel(32);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(writer, test_publication(events), failure.clone());
    (
        RunLog {
            path: "unused-native-log".into(),
            writer,
            failure,
            evidence: Arc::new(Mutex::new(WorkerEvidenceState::default())),
            review_publication: Arc::new(Mutex::new(None)),
            event_order: Arc::new(tokio::sync::Mutex::new(())),
        },
        receiver,
    )
}

fn native_rejection_event() -> AgentRunEvent {
    AgentRunEvent::Permission {
        tool_kind: Some("switch_mode".into()),
        decision: "reject_once".into(),
        option_id: Some("exact-reject".into()),
    }
}

#[tokio::test]
async fn native_permission_and_capture_sync_failures_cannot_complete_or_publish_plan() {
    for fail_at in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let (_, _, handoff) = claude_handoff_fixture(&directory);
        let _attempt = handoff.start_attempt(CancellationToken::new());
        let source = native_exit(
            &handoff,
            "exit",
            serde_json::json!({"plan":"# Durable only"}),
        );
        handoff.begin_settlement("exit", source).unwrap();
        let guard = NativeCaptureGuard(handoff.clone());
        let (log, mut receiver) = native_test_log(NativeSyncWriter {
            sync: 0,
            fail_at: Some(fail_at),
            gate: None,
        });
        let error = handoff
            .persist_capture(&log, native_rejection_event())
            .await
            .unwrap_err();
        assert!(error.contains(if fail_at == 1 {
            "permission persistence failed"
        } else {
            "capture persistence failed"
        }));
        assert!(!handoff.is_completed());
        assert!(!log.has_plan_proof());
        while let Ok(update) = receiver.try_recv() {
            assert!(!matches!(
                update,
                zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
                    event: AgentRunEvent::NativePlanCaptured { .. },
                    ..
                })
            ));
        }
        drop(guard);
        assert!(handoff.capture_error().is_some());
    }
}

#[tokio::test]
async fn native_capture_waits_for_durability_before_signalling_or_exposing_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (_, _, handoff) = claude_handoff_fixture(&directory);
    let _attempt = handoff.start_attempt(CancellationToken::new());
    let source = native_exit(
        &handoff,
        "exit",
        serde_json::json!({"plan":"# Sync barrier"}),
    );
    handoff.begin_settlement("exit", source).unwrap();
    let (entered, waiting) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (log, mut receiver) = native_test_log(NativeSyncWriter {
        sync: 0,
        fail_at: None,
        gate: Some((entered, gate)),
    });
    let task_handoff = handoff.clone();
    let task_log = log.clone();
    let work = tokio::spawn(async move {
        task_handoff
            .persist_capture(&task_log, native_rejection_event())
            .await
    });
    waiting.await.unwrap();
    assert!(!handoff.is_completed());
    assert!(handoff.is_settling());
    assert!(!log.has_plan_proof());
    while let Ok(update) = receiver.try_recv() {
        assert!(!matches!(
            update,
            zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
                event: AgentRunEvent::NativePlanCaptured { .. },
                ..
            })
        ));
    }
    release.send(()).unwrap();
    work.await.unwrap().unwrap();
    assert!(handoff.is_completed());
    assert!(log.has_plan_proof());
}
