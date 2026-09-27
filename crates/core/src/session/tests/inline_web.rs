use super::*;
use zevria_content::AssistantPartIdentity;
use zevria_content::AssistantPresentationContent;
use zevria_content::AssistantPresentationPart;
use zevria_content::AssistantSourceAddress;
use zevria_content::WebSearchAttemptOutcome;
use zevria_content::WebSearchAttemptRecord;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

fn readable_attempt() -> WebSearchAttemptRecord {
    let mut attempt = WebSearchAttemptRecord::new(test_profile());
    attempt.presentation.push(AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Summary(0),
            item_id: Some("reasoning-item".into()),
        },
        content: AssistantPresentationContent::Reasoning {
            text: "retained before retry\nsecond line".into(),
        },
    });
    attempt.touch();
    attempt
}

fn completed_linked_attempt() -> (WebSearchAttemptRecord, TranscriptItem) {
    let replay = ProviderReplay::openai_responses(
        test_profile(),
        vec![
            json!({"type":"reasoning", "id":"reasoning-item", "summary":[{"type":"summary_text", "text":"retained before retry\nsecond line"}]}),
            json!({"type":"message", "id":"answer", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"canonical answer", "annotations":[]}]}),
        ],
    );
    let mut attempt = readable_attempt();
    attempt.reconcile_native_presentation(&replay.items);
    attempt.finish(WebSearchAttemptOutcome::Completed);
    let record = TranscriptItem::provider_message(replay)
        .unwrap()
        .with_display_attempt(Some(attempt.id.clone()))
        .unwrap();
    (attempt, record)
}

#[tokio::test]
async fn record_completed_compacts_only_when_the_link_commits() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (attempt, record) = completed_linked_attempt();
    let checkpoint = TranscriptItem::WebSearchAttempt(attempt.clone());
    engine
        .record_completed_items(vec![checkpoint.clone()])
        .unwrap();
    assert_eq!(
        zevria_transcript::transcript::load(engine.conversation.path()).unwrap(),
        [checkpoint]
    );
    let mut expected = engine.conversation.items().to_vec();
    expected.push(record.clone());
    engine.record_completed(record).unwrap();
    assert!(
        matches!(&engine.conversation.items()[0], TranscriptItem::WebSearchAttempt(saved) if saved.presentation_elided && saved.presentation.is_empty() && saved.revision == attempt.revision)
    );
    assert_eq!(
        zevria_transcript::reconstruct_transcript(engine.conversation.items()),
        expected
    );
    assert_eq!(
        zevria_transcript::transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn deferred_final_item_compacts_with_the_workflow_terminal_record() {
    deferred_link_commit_case(false).await;
}

#[tokio::test]
async fn failed_deferred_link_rewrite_reports_degraded_and_keeps_memory_truthful() {
    deferred_link_commit_case(true).await;
}

async fn deferred_link_commit_case(fail_write: bool) {
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let start = ensemble_start_fixture("linked-search", EnsembleWorkflow::Review, "review");
    let (attempt, record) = completed_linked_attempt();
    engine
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                run_id: start.run_id.clone(),
                synthesis_input: Message::user("durable review evidence"),
                agents: vec![AgentRunSummary {
                    descriptor: start.agents[0].clone(),
                    status: AgentRunStatus::Completed,
                    partial: false,
                    failure: None,
                    has_report: true,
                    has_plan_proof: false,
                    confirmation: None,
                    decision_ids: Vec::new(),
                    unavailable_decisions: Vec::new(),
                }],
            }),
        ])
        .unwrap();
    engine
        .record_completed_items(vec![TranscriptItem::WebSearchAttempt(attempt.clone())])
        .unwrap();
    let checkpoint_bytes = std::fs::read(&path).unwrap();
    assert!(
        matches!(&zevria_transcript::transcript::load(&path).unwrap()[2], TranscriptItem::WebSearchAttempt(saved) if saved == &attempt && !saved.presentation_elided)
    );
    let mut expected = engine.conversation.items().to_vec();
    expected.push(record.clone());
    expected.push(TranscriptItem::Ensemble(EnsembleRecord::Completed {
        run_id: start.run_id.clone(),
    }));

    // Only transcript replacement is blocked; the append handle still works
    // and the full checkpoint must remain intact in the saved file.
    let mut blocker = fail_write
        .then(|| TranscriptRewriteBlocker::new(&path).expect("block transcript replacement"));
    let accepted = AcceptedTurn::new(AcceptedKind::Ensemble {
        run_id: start.run_id.clone(),
        workflow: EnsembleWorkflow::Review,
        resumed: false,
    });
    let output = ModelTurnOutput {
        display_attempt_id: Some(attempt.id.clone()),
        message: record.message().cloned(),
        final_item: Some(record),
        usage: None,
        submission_gate: PlanSubmissionGate::inert(None),
    };
    let turn = TurnContext::new(TurnId::new(7), SessionMode::Build, CancellationToken::new());
    let (events, mut receiver) = session_event_channel(16);
    engine
        .finalize_turn(
            &accepted,
            output,
            FinalResponsePersistence::Deferred,
            &events,
            &turn,
        )
        .await
        .unwrap();
    assert_eq!(
        engine.conversation.persistence_error().is_some(),
        fail_write
    );
    assert!(
        matches!(&engine.conversation.items()[2], TranscriptItem::WebSearchAttempt(saved) if saved.presentation_elided && saved.presentation.is_empty() && saved.revision == attempt.revision)
    );
    assert_eq!(
        zevria_transcript::reconstruct_transcript(engine.conversation.items()),
        expected
    );
    SessionReplayError::validate(engine.conversation.items()).unwrap();
    let updates = collect_events(&mut receiver).await;
    assert_eq!(
        updates.iter().any(|event| matches!(
            event,
            SessionEvent::PersistenceChanged { error: Some(_), .. }
        )),
        fail_write
    );
    assert!(updates.iter().any(|event| matches!(event, SessionEvent::TurnCompleted { display_attempt_id: Some(id), .. } if id == &attempt.id)));
    if let Some(blocker) = &mut blocker {
        assert_eq!(
            std::fs::read(blocker.backup_path()).unwrap(),
            checkpoint_bytes
        );
        blocker.restore().expect("restore transcript filename");
        assert!(engine.conversation.ensure_durable().unwrap());
        assert!(engine.conversation.persistence_error().is_none());
    }
    assert_eq!(
        zevria_transcript::transcript::load(&path).unwrap(),
        engine.conversation.items()
    );
}

struct CheckpointProvider {
    path: PathBuf,
    fail_write: bool,
    // Outlive cancellation of complete() and core's subsequent checkpoint drain.
    blocker: Option<TranscriptRewriteBlocker>,
    live: bool,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    verified: Option<tokio::sync::oneshot::Sender<(ProgressReporter, bool)>>,
}
impl ModelProvider for CheckpointProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let original = request.owned_messages();
            self.entered.take().unwrap().send(()).unwrap();
            // Occupy every frontend lifecycle slot. The checkpoint receiver
            // must still be serviced by core without draining this channel.
            while progress
                .events()
                .try_send(SessionEvent::TurnRecovered {
                    turn_id: TurnId::new(999),
                    display_attempt_id: None,
                })
                .is_ok()
            {}
            if self.fail_write {
                self.blocker = Some(
                    TranscriptRewriteBlocker::new(&self.path)
                        .expect("block checkpoint transcript replacement"),
                );
            }
            let mut attempt = readable_attempt();
            attempt.presentation.push(AssistantPresentationPart {
                source: AssistantSourceAddress {
                    output_index: 1,
                    part: AssistantPartIdentity::Content(0),
                    item_id: Some("answer".into()),
                },
                content: AssistantPresentationContent::Answer {
                    text: "provisional answer before citation finality".into(),
                },
            });
            attempt.touch();
            if !self.live {
                attempt.finish(WebSearchAttemptOutcome::Failed);
            }
            let checkpoint = progress.checkpoint_web_search(attempt.clone()).await;
            assert_eq!(
                request.owned_messages(),
                original,
                "display checkpoints cannot mutate in-flight input"
            );
            if checkpoint.is_ok() {
                let saved = zevria_transcript::transcript::load(&self.path).unwrap();
                let mut expected = attempt.clone();
                expected.finish(WebSearchAttemptOutcome::Interrupted);
                assert!(saved.iter().any(|item| matches!(item, TranscriptItem::WebSearchAttempt(saved) if saved == &expected)));
                let bytes = std::fs::read(&self.path).unwrap();
                let restored = zevria_transcript::reconstruct_transcript(&saved);
                assert!(restored.iter().any(|item| matches!(item, TranscriptItem::WebSearchAttempt(display) if display.presentation == attempt.presentation && display.outcome == if self.live { WebSearchAttemptOutcome::Interrupted } else { WebSearchAttemptOutcome::Failed })));
                assert_eq!(std::fs::read(&self.path).unwrap(), bytes);
                assert!(
                    !saved
                        .iter()
                        .any(|item| matches!(item.message(), Some(Message::Assistant { .. }))),
                    "a display checkpoint cannot commit a canonical answer"
                );
            }
            self.verified
                .take()
                .unwrap()
                .send((progress.clone(), checkpoint.is_ok()))
                .ok()
                .unwrap();
            // Simulate cancellation after acknowledgement but before the next
            // request/clear. The final drain must not persist this revision twice.
            std::future::pending().await
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn checkpoint_ack_is_durable_and_independent_of_frontend_backpressure() {
    checkpoint_case(false, false).await;
}

#[tokio::test]
async fn checkpoint_failure_is_acknowledged_as_failure_and_retained_once_in_memory() {
    checkpoint_case(true, false).await;
}

#[tokio::test]
async fn checkpointed_live_answer_is_recoverable_before_completion_and_after_cancellation() {
    checkpoint_case(false, true).await;
}

async fn checkpoint_case(fail_write: bool, live: bool) {
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
    let (entered, mut entered_rx) = tokio::sync::oneshot::channel();
    let (verified, verified_rx) = tokio::sync::oneshot::channel();
    let provider = CheckpointProvider {
        path: path.clone(),
        fail_write,
        blocker: None,
        live,
        entered: Some(entered),
        verified: Some(verified),
    };
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(1);
    let mut run = tokio::spawn(async move {
        let _ = engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "request".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await;
        engine
    });
    loop {
        tokio::select! { biased; _ = &mut entered_rx => break, update = receiver.recv() => { assert!(update.is_some()); } }
    }
    let (progress, durable) = tokio::time::timeout(std::time::Duration::from_secs(3), verified_rx)
        .await
        .expect("ack must not wait for frontend capacity")
        .unwrap();
    assert_eq!(durable, !fail_write);
    progress.turn().cancellation().cancel();
    let mut degraded = false;
    let engine = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            tokio::select! {
                engine = &mut run => break engine.unwrap(),
                update = receiver.recv() => { degraded |= matches!(update, Some(SessionUpdate::Lifecycle(SessionEvent::PersistenceChanged { error: Some(_), .. }))); }
            }
        }
    }).await.unwrap();
    let attempts = engine
        .conversation
        .items()
        .iter()
        .filter(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
        .count();
    let expected_revisions = if live { 2 } else { 1 };
    assert_eq!(attempts, expected_revisions);
    assert_eq!(
        engine.conversation.persistence_error().is_some(),
        fail_write
    );
    assert_eq!(degraded, fail_write);
    assert_eq!(engine.provider.blocker.is_some(), fail_write);
    if let Some(blocker) = &engine.provider.blocker {
        assert!(
            path.is_dir(),
            "cancelling the provider must not restore the transcript"
        );
        assert!(blocker.backup_path().is_file());
    }
    if !fail_write {
        assert_eq!(
            zevria_transcript::transcript::load(&path)
                .unwrap()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
                .count(),
            expected_revisions
        );
    }
}

#[test]
fn explicit_assistant_binding_round_trips_without_entering_model_input() {
    let attempt = readable_attempt();
    let record = TranscriptItem::Message(Message::assistant("canonical answer"))
        .with_display_attempt(Some(attempt.id.clone()))
        .unwrap();
    let restored: TranscriptItem =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    assert_eq!(restored.display_attempt_id(), Some(attempt.id.as_str()));
    assert_eq!(restored.message(), record.message());
    assert_eq!(
        restored.model_request_item().unwrap().message_ref(),
        record.message()
    );
    for value in [
        json!({"role":"user","content":"bad","zevria_display_attempt":"id"}),
        json!({"zevria_web_search_attempt":attempt,"zevria_display_attempt":"id"}),
    ] {
        assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
    }
}

#[test]
fn display_snapshots_are_revisioned_independently_of_channel_publications() {
    let (events, mut receiver) = session_event_channel(1);
    let progress = ProgressReporter::new(events);
    let mut attempt = readable_attempt();
    let stale = attempt.clone();
    attempt.presentation[0].content = AssistantPresentationContent::Reasoning {
        text: "latest".into(),
    };
    attempt.touch();
    progress.stream_snapshot(zevria_content::AssistantStreamSnapshot {
        message: None,
        attempt: Some(attempt.clone()),
    });
    progress.collect_web_search(stale);
    let collected = progress.drain_web_search(WebSearchAttemptOutcome::Interrupted);
    assert_eq!(collected[0].presentation, attempt.presentation);
    let SessionUpdate::Streams(batch) = receiver.try_recv().unwrap() else {
        panic!("activity-only stream")
    };
    let state = batch.root.unwrap();
    assert!(state.message.is_none());
    assert_eq!(state.attempt.unwrap().presentation, attempt.presentation);
    progress.stream_cleared();
    let SessionUpdate::Streams(batch) = receiver.try_recv().unwrap() else {
        panic!("clear stream")
    };
    let state = batch.root.unwrap();
    assert!(state.message.is_none() && state.attempt.is_none());
}

struct EmptyAttemptProvider;
impl ModelProvider for EmptyAttemptProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            progress.collect_web_search(WebSearchAttemptRecord::new(test_profile()));
            Err(anyhow::anyhow!("failed before readable evidence"))
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn attempts_without_observed_display_are_not_persisted_or_published() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        EmptyAttemptProvider,
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(64);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "request".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(
        !engine
            .conversation
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
    );
    assert!(
        !collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::WebSearchUpdated { .. }))
    );
}

/// Opt-in memory/latency probe. Run the test executable under an OS RSS profiler
/// with ZEVRIA_SNAPSHOT_COPIES=0 and =1 to isolate the added request snapshot.
#[test]
#[ignore = "opt-in owned-request memory probe"]
fn owned_request_snapshot_memory_probe() {
    let copies = std::env::var("ZEVRIA_SNAPSHOT_COPIES")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(1);
    assert!(copies <= 8);
    let text = "readable content".repeat(512); // 8 KiB
    let mut history = Vec::new();
    for index in 0..256 {
        history.push(TranscriptItem::Message(Message::user(text.clone())));
        history.push(TranscriptItem::provider_message(zevria_model::ProviderReplay::openai_responses(
            test_profile(), vec![
                json!({"type":"reasoning","id":format!("r-{index}"),"summary":[{"type":"summary_text","text":"check evidence"}],"encrypted_content":text}),
                json!({"type":"message","id":format!("m-{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}),
            ],
        )).unwrap());
    }
    let input = history
        .iter()
        .filter_map(TranscriptItem::model_request_item)
        .collect::<Vec<_>>();
    let wire_bytes: usize = input
        .iter()
        .map(|item| {
            if let Some(replay) = item.replay_ref() {
                serde_json::to_vec(replay).unwrap().len()
            } else {
                serde_json::to_vec(item.message_ref().unwrap())
                    .unwrap()
                    .len()
            }
        })
        .sum();
    let started = std::time::Instant::now();
    let snapshots = (0..copies)
        .map(|_| {
            input
                .iter()
                .copied()
                .map(ModelRequestItem::to_owned_item)
                .collect::<anyhow::Result<Vec<_>>>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    eprintln!(
        "snapshot probe: items={}, native/message serialized bytes={wire_bytes}, copies={copies}, clone_elapsed={:?}",
        input.len(),
        started.elapsed()
    );
    std::hint::black_box((&history, &input, &snapshots));
}
