use super::*;
use zevria_content::HostedActivity;
use zevria_content::HostedActivityStatus as Status;
use zevria_content::ResponseAttempt;
use zevria_content::ResponseAttemptOutcome as Outcome;

#[path = "recoverable_batches.rs"]
mod recoverable_batches;

fn recovery(attempt: &ResponseAttempt) -> TranscriptItem {
    TranscriptItem::IncompleteResponse(
        zevria_transcript::IncompleteResponse::from_attempt(attempt)
            .unwrap()
            .unwrap(),
    )
}

fn attempt() -> ResponseAttempt {
    let mut attempt = ResponseAttempt::new(test_profile());
    attempt.hosted_activity = vec![
        HostedActivity {
            kind: zevria_content::HostedActivityKind::WebSearch,
            item_id: Some("done".into()),
            output_index: 0,
            status: Status::Completed,
            action: Some(json!({"type":"search","query":"Rust"})),
        },
        HostedActivity {
            kind: zevria_content::HostedActivityKind::WebSearch,
            item_id: Some("open".into()),
            output_index: 1,
            status: Status::Searching,
            action: Some(json!({"type":"open_page","url":"https://example.com"})),
        },
    ];
    attempt
        .presentation
        .push(zevria_content::AssistantPresentationPart {
            source: zevria_content::AssistantSourceAddress {
                output_index: 2,
                part: zevria_content::AssistantPartIdentity::Content(0),
                item_id: Some("answer".into()),
            },
            content: zevria_content::AssistantPresentationContent::Answer {
                text: "observed partial answer".into(),
            },
        });
    attempt
}

#[test]
fn activity_round_trip_is_model_and_capacity_inert() {
    let mut attempt = attempt();
    attempt.close(Outcome::Interrupted);
    let record = recovery(&attempt);
    let json = serde_json::to_string(&record).unwrap();
    let restored: TranscriptItem = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, record);
    assert!(restored.message().is_none());
    assert!(restored.model_request_item().is_none());
    assert!(restored.provider_replay().is_none());
    let before = vec![TranscriptItem::Message(Message::user("hello")), record];
    let items = vec![before[0].clone(), restored];
    // The complete model projection, not merely its length or token estimate,
    // stays identical across persistence and when display metadata is omitted.
    // Restoration therefore cannot change any provider-cacheable input prefix.
    assert_eq!(
        zevria_transcript::transcript::model_input(&before),
        zevria_transcript::transcript::model_input(&items),
    );
    assert_eq!(
        zevria_transcript::transcript::model_input(&items),
        zevria_transcript::transcript::model_input(&items[..1]),
    );
    assert_eq!(zevria_transcript::transcript::model_input(&items).len(), 1);
    assert_eq!(
        estimate_model_input(zevria_transcript::transcript::model_input(&items)),
        estimate_model_input(zevria_transcript::transcript::model_input(&items[..1]))
    );
    assert_eq!(attempt.hosted_activity[0].status, Status::Completed);
    assert_eq!(attempt.hosted_activity[1].status, Status::Searching);
    assert_eq!(
        attempt.action_display_status(&attempt.hosted_activity[1]),
        "completion unconfirmed"
    );
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["zevria_incomplete_response"]["version"], 1);
    for version in [2, 99] {
        value["zevria_incomplete_response"]["version"] = json!(version);
        assert!(serde_json::from_value::<TranscriptItem>(value.clone()).is_err());
    }
}

#[tokio::test]
async fn collector_clones_keep_observations_when_delivery_is_cancelled() {
    let (events, mut receiver) = session_event_channel(1);
    let progress = ProgressReporter::new(events.clone());
    events
        .send(SessionEvent::StreamCleared {
            turn_id: TurnId::new(0),
        })
        .await
        .unwrap();
    events
        .send(SessionEvent::TurnRecovered {
            turn_id: TurnId::new(0),
        })
        .await
        .unwrap();
    let observed = attempt();
    let expected = observed.id.clone();
    let clone = progress.clone();
    {
        let update = clone.attempt_updated(observed.clone());
        tokio::pin!(update);
        assert!(matches!(
            futures_util::poll!(&mut update),
            std::task::Poll::Pending
        ));
        // Dropping the blocked delivery is the same boundary as dropping a
        // cancelled provider future. Collection already happened.
    }
    let records = progress.drain_attempts();
    assert_eq!(records, vec![observed]);
    assert_eq!(records[0].id, expected);
    assert!(clone.drain_attempts().is_empty());
    assert!(receiver.try_recv().is_ok());
}

struct ObservingProvider {
    exit: Outcome,
    dropped: Arc<AtomicBool>,
}
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ModelProvider for ObservingProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let _guard = DropFlag(self.dropped.clone());
            let mut first = attempt();
            first.close(Outcome::Failed);
            progress.attempt_updated(first).await;
            let second = attempt();
            progress.collect_attempt(second.clone());
            match self.exit {
                Outcome::Completed => Ok(ModelResponse::from_replay(ProviderReplay::openai_responses(test_profile(), vec![json!({
                    "type":"message","id":"committed","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]
                })])).unwrap().with_attempt_id(Some(second.id)).unwrap()),
                Outcome::Failed => Err(anyhow::anyhow!("provider rejected request")),
                Outcome::Interrupted => {
                    progress.turn().cancellation().cancel();
                    std::future::pending().await
                }
                Outcome::InProgress => unreachable!(),
            }
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn every_model_exit_flushes_retry_trail_before_terminal_publication() {
    for exit in [Outcome::Completed, Outcome::Failed, Outcome::Interrupted] {
        let dropped = Arc::new(AtomicBool::new(false));
        let provider = ObservingProvider {
            exit,
            dropped: dropped.clone(),
        };
        let (_directory, writer) = test_transcript();
        let path = writer.path().to_path_buf();
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let (events, mut receiver) = session_event_channel(256);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "search".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        let items = zevria_transcript::transcript::load(&path).unwrap();
        let attempts = items
            .iter()
            .filter_map(|item| {
                if let TranscriptItem::IncompleteResponse(attempt) = item {
                    Some(attempt.to_attempt())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            attempts.len(),
            if exit == Outcome::Completed { 1 } else { 2 }
        );
        assert_eq!(attempts[0].outcome, Outcome::Failed);
        if exit != Outcome::Completed {
            assert_eq!(attempts[1].outcome, exit);
            assert_eq!(attempts[1].hosted_activity[0].status, Status::Completed);
        }
        assert!(attempts.iter().all(|attempt| matches!(&attempt.presentation[0].content,
            zevria_content::AssistantPresentationContent::Answer { text } if text == "observed partial answer")));
        assert!(
            items
                .iter()
                .filter_map(TranscriptItem::message)
                .all(|message| !zevria_content::assistant_plain_text(message)
                    .contains("observed partial answer")),
            "partial display evidence is not a canonical assistant message"
        );
        let lifecycle = collect_events(&mut receiver).await;
        let final_activity = lifecycle
            .iter()
            .rposition(|event| matches!(event, SessionEvent::ResponseAttemptUpdated { .. }))
            .unwrap();
        let terminal = lifecycle
            .iter()
            .position(|event| {
                matches!(
                    event,
                    SessionEvent::TurnCompleted { .. }
                        | SessionEvent::TurnFailed { .. }
                        | SessionEvent::TurnCancelled { .. }
                )
            })
            .unwrap();
        assert!(final_activity < terminal);
        assert_eq!(
            engine
                .conversation
                .model_input()
                .iter()
                .filter(|item| item.replay_ref().is_some())
                .count(),
            usize::from(exit == Outcome::Completed)
        );
    }
}

struct SearchBeforeToolProvider {
    called: bool,
    dropped: Arc<AtomicBool>,
}
impl ModelProvider for SearchBeforeToolProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            if self.called {
                return Ok(model_response(Message::assistant("done")));
            }
            self.called = true;
            let _guard = DropFlag(self.dropped.clone());
            let attempt = attempt();
            progress.collect_attempt(attempt.clone());
            Ok(ModelResponse::from_replay(ProviderReplay::openai_responses(test_profile(), vec![
                json!({"type":"web_search_call","id":"search","status":"completed","action":{"type":"search","query":"Rust"}}),
                json!({"type":"function_call","id":"local","call_id":"local","status":"completed","name":"verify_search_durable","arguments":"{}"}),
            ])).unwrap().with_attempt_id(Some(attempt.id)).unwrap())
        })
    }
    fn reset(&mut self) {}
}
struct VerifySearchDurable {
    path: PathBuf,
    dropped: Arc<AtomicBool>,
    executed: Arc<AtomicBool>,
}
impl Tool for VerifySearchDurable {
    const NAME: &'static str = "verify_search_durable";
    type Args = serde_json::Value;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Assert the pre-effect durability boundary".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object","properties":{}})
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        assert!(self.dropped.load(Ordering::SeqCst));
        let items = zevria_transcript::transcript::load(&self.path).unwrap();
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, TranscriptItem::IncompleteResponse(_)))
        );
        assert!(items.iter().enumerate().any(|(index, item)| {
            zevria_transcript::restored_attempt(item, index)
                .is_some_and(|attempt| !attempt.hosted_activity.is_empty())
        }));
        self.executed.store(true, Ordering::SeqCst);
        Ok("verified".into())
    }
}
#[tokio::test]
async fn observed_activity_is_on_disk_before_a_local_tool_can_execute() {
    let (_directory, writer) = test_transcript();
    let dropped = Arc::new(AtomicBool::new(false));
    let executed = Arc::new(AtomicBool::new(false));
    let tools = ToolServer::new()
        .tool(VerifySearchDurable {
            path: writer.path().to_path_buf(),
            dropped: dropped.clone(),
            executed: executed.clone(),
        })
        .run();
    let provider = SearchBeforeToolProvider {
        called: false,
        dropped,
    };
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, _receiver) = session_event_channel(256);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "search then tool".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(executed.load(Ordering::SeqCst));
}

struct BackpressuredProvider {
    ready: Option<tokio::sync::oneshot::Sender<ProgressReporter>>,
    dropped: Arc<AtomicBool>,
}
impl ModelProvider for BackpressuredProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let _guard = DropFlag(self.dropped.clone());
            // Fill the lossless queue, then prove the activity send is pending.
            progress
                .events()
                .send(SessionEvent::TurnRecovered {
                    turn_id: progress.turn().id,
                })
                .await
                .unwrap();
            let update = progress.attempt_updated(attempt());
            tokio::pin!(update);
            assert!(matches!(
                futures_util::poll!(&mut update),
                std::task::Poll::Pending
            ));
            self.ready
                .take()
                .unwrap()
                .send(progress.clone())
                .ok()
                .unwrap();
            update.await;
            std::future::pending().await
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn cancellation_while_provider_is_blocked_on_delivery_flushes_once() {
    let (ready, mut ready_rx) = tokio::sync::oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = BackpressuredProvider {
        ready: Some(ready),
        dropped: dropped.clone(),
    };
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
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
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "search".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        engine
    });
    let progress = loop {
        tokio::select! {
            biased;
            progress = &mut ready_rx => break progress.unwrap(),
            event = receiver.recv() => { assert!(event.is_some()); }
        }
    };
    progress.turn().cancellation().cancel();
    let engine = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                engine = &mut run => break engine.unwrap(),
                _ = receiver.recv() => {},
            }
        }
    })
    .await
    .unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    let items = zevria_transcript::transcript::load(&path).unwrap();
    let attempts = items
        .iter()
        .filter_map(|item| {
            if let TranscriptItem::IncompleteResponse(attempt) = item {
                Some(attempt.to_attempt())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Outcome::Interrupted);
    assert!(
        matches!(&attempts[0].presentation[0].content, zevria_content::AssistantPresentationContent::Answer { text } if text == "observed partial answer")
    );
    assert!(
        items
            .iter()
            .filter_map(TranscriptItem::message)
            .all(|message| !zevria_content::assistant_plain_text(message)
                .contains("observed partial answer"))
    );
    assert_eq!(attempts[0].hosted_activity[0].status, Status::Completed);
    assert_eq!(attempts[0].hosted_activity[1].status, Status::Searching);
    assert_eq!(
        attempts[0].action_display_status(&attempts[0].hosted_activity[1]),
        "completion unconfirmed"
    );
    assert!(progress.drain_attempts().is_empty());
    assert!(engine.conversation.persistence_error().is_none());
}

#[tokio::test]
async fn independent_hosted_updates_do_not_fence_root_or_child_previews() {
    let (events, mut receiver) = session_event_channel(32);
    let turn_id = TurnId::new(3);
    events.stream_updated(turn_id, Message::assistant("existing answer"));
    events
        .send(SessionEvent::ResponseAttemptUpdated {
            turn_id,
            attempt: attempt(),
        })
        .await
        .unwrap();
    let first = receiver.try_recv().unwrap();
    assert!(matches!(
        first,
        SessionUpdate::Lifecycle(SessionEvent::ResponseAttemptUpdated { .. })
    ));
    assert!(
        matches!(receiver.try_recv().unwrap(),SessionUpdate::Streams(batch) if batch.root.is_some())
    );
    events.stream_updated(turn_id, Message::assistant("existing answer continued"));
    assert!(
        matches!(receiver.try_recv().unwrap(),SessionUpdate::Streams(batch) if batch.root.is_some())
    );
    let child = zevria_foundation::SubtaskId::new("child");
    events.set_subtask_stream(
        child.clone(),
        SessionStreamState {
            attempt: None,
            revision: 0,
            turn_id,
            message: Some(Message::assistant("child answer")),
        },
    );
    events
        .send(SessionEvent::SubtaskSession {
            id: child.clone(),
            event: Box::new(SessionEvent::ResponseAttemptUpdated {
                turn_id,
                attempt: attempt(),
            }),
        })
        .await
        .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionUpdate::Lifecycle(SessionEvent::SubtaskSession { .. })
    ));
    assert!(
        matches!(receiver.try_recv().unwrap(),SessionUpdate::Streams(batch) if batch.subtasks.contains_key(&child))
    );
}

#[test]
fn portable_citation_estimates_include_expanded_destinations() {
    let replay = zevria_model::ProviderReplay::openai_responses(
        test_profile(),
        vec![
            json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"X","annotations":[{"type":"url_citation","title":"A source title","url":format!("https://example.com/{}","long-path".repeat(30)),"start_index":0,"end_index":1}]}]}),
        ],
    );
    let message = zevria_model::ReplayMessage::new(replay).unwrap();
    let item = ModelRequestItem::replay_backed(&message);
    let native = estimate_model_request_item_for_profile(item, &test_profile()).unwrap();
    let portable =
        estimate_model_request_item_for_profile(item, &ModelProfileRef::new("other", "model"))
            .unwrap();
    assert!(portable.payload_tokens > native.payload_tokens);
}

mod recovery_lifecycle {
    use super::*;
    use zevria_content::AssistantPartIdentity;
    use zevria_content::AssistantPresentationContent;
    use zevria_content::AssistantPresentationPart;
    use zevria_content::AssistantSourceAddress;
    use zevria_content::ResponseAttempt;
    use zevria_content::ResponseAttemptOutcome;
    use zevria_transcript::test_support::TranscriptRewriteBlocker;

    fn readable_attempt() -> ResponseAttempt {
        let mut attempt = ResponseAttempt::new(test_profile());
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

    fn completed_attempt() -> (ResponseAttempt, TranscriptItem) {
        let replay = ProviderReplay::openai_responses(
            test_profile(),
            vec![
                json!({"type":"reasoning", "id":"reasoning-item", "summary":[{"type":"summary_text", "text":"retained before retry\nsecond line"}]}),
                json!({"type":"message", "id":"answer", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"canonical answer", "annotations":[]}]}),
            ],
        );
        let mut attempt = readable_attempt();
        let (presentation, hosted) =
            zevria_content::response::responses_native_display(&replay.items);
        attempt.complete_with(presentation, hosted);
        let record = TranscriptItem::provider_message(replay).unwrap();
        (attempt, record)
    }

    #[tokio::test]
    async fn record_completed_is_an_append_and_never_changes_earlier_recovery() {
        let (_directory, writer) = test_transcript();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([]),
            ToolServer::new().run(),
            test_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let mut abandoned = readable_attempt();
        abandoned.close(ResponseAttemptOutcome::Failed);
        let recovery = super::recovery(&abandoned);
        engine.record_completed(recovery.clone()).unwrap();
        let before = std::fs::read(engine.conversation.path()).unwrap();
        let (_, record) = completed_attempt();
        engine.record_completed(record.clone()).unwrap();
        assert!(
            std::fs::read(engine.conversation.path())
                .unwrap()
                .starts_with(&before)
        );
        assert_eq!(
            zevria_transcript::load(engine.conversation.path()).unwrap(),
            [recovery, record]
        );
    }

    #[tokio::test]
    async fn deferred_final_item_commits_with_the_workflow_terminal_record() {
        deferred_commit_case(false).await;
    }

    #[tokio::test]
    async fn failed_deferred_rewrite_reports_degraded_and_keeps_memory_truthful() {
        deferred_commit_case(true).await;
    }

    async fn deferred_commit_case(fail_write: bool) {
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
        let (attempt, record) = completed_attempt();
        engine
            .commit(vec![
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
        let checkpoint_bytes = std::fs::read(&path).unwrap();
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
            attempt_id: Some(attempt.id.clone()),
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
        assert_eq!(engine.conversation.items(), expected);
        SessionReplayError::validate(engine.conversation.items()).unwrap();
        let updates = collect_events(&mut receiver).await;
        assert_eq!(
            updates.iter().any(|event| matches!(
                event,
                SessionEvent::PersistenceChanged { error: Some(_), .. }
            )),
            fail_write
        );
        assert!(updates.iter().any(|event| matches!(event, SessionEvent::TurnCompleted { attempt_id: Some(id), .. } if id == &attempt.id)));
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
        // Outlive cancellation of complete() and core's subsequent checkpoint drain.
        blocker: Option<TranscriptRewriteBlocker>,
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
                    })
                    .is_ok()
                {}
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
                attempt.close(ResponseAttemptOutcome::Failed);
                let checkpoint = progress.checkpoint_failed_attempt(attempt.clone()).await;
                assert_eq!(
                    request.owned_messages(),
                    original,
                    "display checkpoints cannot mutate in-flight input"
                );
                if checkpoint.is_ok() {
                    let saved = zevria_transcript::transcript::load(&self.path).unwrap();
                    let expected = zevria_transcript::IncompleteResponse::from_attempt(&attempt)
                        .unwrap()
                        .unwrap();
                    assert!(saved.iter().any(|item| matches!(item, TranscriptItem::IncompleteResponse(saved) if saved == &expected)));
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
        checkpoint_case(false).await;
    }

    #[tokio::test]
    async fn checkpoint_failure_is_acknowledged_as_failure_and_retained_once_in_memory() {
        checkpoint_case(true).await;
    }

    async fn checkpoint_case(fail_write: bool) {
        let (_directory, writer) = test_transcript();
        let path = writer.path().to_path_buf();
        let (entered, mut entered_rx) = tokio::sync::oneshot::channel();
        let (verified, verified_rx) = tokio::sync::oneshot::channel();
        let provider = CheckpointProvider {
            path: path.clone(),
            blocker: None,
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
        engine
            .commit(TranscriptItem::Message(Message::user("request")))
            .unwrap();
        let policy = test_policies().policy(SessionMode::Build).clone();
        engine.reconcile_before_dispatch(&policy).unwrap();
        if fail_write {
            engine.provider.blocker = Some(TranscriptRewriteBlocker::new(&path).unwrap());
            assert!(
                engine
                    .record_completed(vec![TranscriptItem::Error {
                        error: "already completed".into()
                    }])
                    .is_err()
            );
        }
        let (events, mut receiver) = session_event_channel(1);
        let mut run = tokio::spawn(async move {
            let turn =
                TurnContext::new(TurnId::new(7), SessionMode::Build, CancellationToken::new());
            let policy = test_policies().policy(SessionMode::Build).clone();
            let progress = ProgressReporter::for_turn(
                events,
                turn,
                policy.model_role,
                engine.context_policy(policy.model_role),
            );
            let accepted = AcceptedTurn::new(AcceptedKind::Prompt {
                mode: SessionMode::Build,
            });
            let result = engine
                .run_model_tool_loop(
                    &accepted,
                    &policy,
                    progress,
                    PlanSubmissionGate::inert(None),
                    FinalResponsePersistence::Immediate,
                )
                .await;
            assert!(matches!(result, Err(Failure::Cancelled)));
            engine
        });
        loop {
            tokio::select! { biased; _ = &mut entered_rx => break, update = receiver.recv() => { assert!(update.is_some()); } }
        }
        let (progress, durable) =
            tokio::time::timeout(std::time::Duration::from_secs(3), verified_rx)
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
            .filter(|item| matches!(item, TranscriptItem::IncompleteResponse(_)))
            .count();
        let expected_revisions = 1;
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
                    .filter(|item| matches!(item, TranscriptItem::IncompleteResponse(_)))
                    .count(),
                expected_revisions
            );
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
        progress.collect_attempt(stale);
        let collected = progress.drain_attempts();
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
                progress.collect_attempt(ResponseAttempt::new(test_profile()));
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
                .any(|item| matches!(item, TranscriptItem::IncompleteResponse(_)))
        );
        assert!(
            !collect_events(&mut receiver)
                .await
                .iter()
                .any(|event| matches!(event, SessionEvent::ResponseAttemptUpdated { .. }))
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
}

struct FinalProvider {
    cancel: bool,
    block_frontend: bool,
    ready: Option<tokio::sync::oneshot::Sender<ProgressReporter>>,
}
impl ModelProvider for FinalProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let mut observed = attempt();
            // Also exercise the completed-but-uncommitted collector case.
            if self.cancel {
                observed.close(Outcome::Completed);
            }
            progress.stream_snapshot(zevria_content::AssistantStreamSnapshot {
                message: None,
                attempt: Some(observed.clone()),
            });
            if self.block_frontend {
                while progress
                    .events()
                    .try_send(SessionEvent::TurnRecovered {
                        turn_id: TurnId::new(999),
                    })
                    .is_ok()
                {}
            }
            if let Some(ready) = self.ready.take() {
                ready.send(progress.clone()).ok().unwrap();
            }
            let response = ModelResponse::from_replay(ProviderReplay::openai_responses(test_profile(), vec![
                json!({"type":"reasoning","id":"reasoning","summary":[{"type":"summary_text","text":"committed reasoning"}]}),
                json!({"type":"message","id":"answer","role":"assistant","status":"completed","content":[{"type":"output_text","text":"committed answer","annotations":[]}]}),
            ])).unwrap().with_attempt_id(Some(observed.id)).unwrap().with_usage(Some(TokenUsage {
                input_tokens: 10, cached_tokens: 0, output_tokens: 2, total_tokens: 12,
            }));
            if self.cancel {
                progress.turn().cancellation().cancel();
            }
            Ok(response)
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn success_is_finalized_from_the_record_and_live_only_activity_is_not_persisted() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        FinalProvider {
            cancel: false,
            block_frontend: false,
            ready: None,
        },
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "respond".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let items = zevria_transcript::load(engine.conversation.path()).unwrap();
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, TranscriptItem::IncompleteResponse(_)))
    );
    let (index, record) = items
        .iter()
        .enumerate()
        .find(|(_, item)| item.provider_replay().is_some())
        .unwrap();
    let restored = zevria_transcript::restored_attempt(record, index).unwrap();
    let updates = collect_events(&mut receiver).await;
    let live = updates
        .iter()
        .find_map(|event| match event {
            SessionEvent::ResponseAttemptUpdated { attempt, .. }
                if attempt.outcome == Outcome::Completed =>
            {
                Some(attempt)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(live.presentation, restored.presentation);
    assert_eq!(live.hosted_activity, restored.hosted_activity);
    assert!(
        live.hosted_activity.is_empty(),
        "live-only search is dropped, not merged"
    );
    assert!(!serde_json::to_string(record).unwrap().contains(&live.id));
    let attempt = updates.iter().position(|event| matches!(event, SessionEvent::ResponseAttemptUpdated { attempt, .. } if attempt.id == live.id)).unwrap();
    let usage = updates
        .iter()
        .position(|event| matches!(event, SessionEvent::UsageUpdated { .. }))
        .unwrap();
    let terminal = updates.iter().position(|event| matches!(event, SessionEvent::TurnCompleted { attempt_id: Some(id), .. } if id == &live.id)).unwrap();
    assert!(attempt < usage && usage < terminal);
    assert_eq!(
        updates
            .iter()
            .filter(|event| matches!(event, SessionEvent::UsageUpdated { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn completed_response_that_loses_the_cancellation_race_is_interrupted_recovery_only() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        FinalProvider {
            cancel: true,
            block_frontend: false,
            ready: None,
        },
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "respond".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let items = zevria_transcript::load(engine.conversation.path()).unwrap();
    assert!(items.iter().all(|item| item.provider_replay().is_none()));
    let recovery = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::IncompleteResponse(record) => Some(record),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(recovery.len(), 1);
    assert_eq!(
        recovery[0].outcome,
        zevria_transcript::IncompleteResponseOutcome::Interrupted
    );
    assert_eq!(recovery[0].presentation, attempt().presentation);
    assert!(
        items
            .iter()
            .filter_map(TranscriptItem::message)
            .all(|message| !zevria_content::assistant_plain_text(message)
                .contains("committed answer"))
    );
    let updates = collect_events(&mut receiver).await;
    let closure = updates.iter().position(|event| matches!(event, SessionEvent::ResponseAttemptUpdated { attempt, .. } if attempt.outcome == Outcome::Interrupted)).unwrap();
    let terminal = updates
        .iter()
        .position(|event| matches!(event, SessionEvent::TurnCancelled { .. }))
        .unwrap();
    assert!(closure < terminal);
    assert!(
        !updates
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn committed_response_with_usage_is_durable_before_a_blocked_frontend_publication() {
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
    let (ready, mut ready_rx) = tokio::sync::oneshot::channel();
    let mut engine = SessionEngine::new(
        FinalProvider {
            cancel: false,
            block_frontend: true,
            ready: Some(ready),
        },
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(1);
    let mut run = tokio::spawn(async move {
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "respond".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        engine
    });
    loop {
        tokio::select! { biased; ready = &mut ready_rx => { ready.unwrap(); break; }, update = receiver.recv() => assert!(update.is_some()), }
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let items = zevria_transcript::load(&path).unwrap();
            if items.iter().any(|item| item.provider_replay().is_some()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !run.is_finished(),
        "frontend remains blocked after durable response commit"
    );
    let engine = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop { tokio::select! { engine = &mut run => break engine.unwrap(), _ = receiver.recv() => {}, } }
    }).await.unwrap();
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::IncompleteResponse(_)))
    );
}

#[tokio::test]
async fn deferred_ensemble_final_publishes_attempt_before_turn_completed_without_sidecars() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        FinalProvider {
            cancel: false,
            block_frontend: false,
            ready: None,
        },
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let start = ensemble_start_fixture("response-attempt", EnsembleWorkflow::Review, "review");
    engine
        .commit(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                run_id: start.run_id.clone(),
                synthesis_input: Message::user("evidence"),
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
    let accepted = AcceptedTurn::new(AcceptedKind::Ensemble {
        run_id: start.run_id,
        workflow: EnsembleWorkflow::Review,
        resumed: false,
    });
    let turn = TurnContext::new(TurnId::new(7), SessionMode::Build, CancellationToken::new());
    let policy = test_policies().policy(SessionMode::Build).clone();
    let (events, mut receiver) = session_event_channel(128);
    let progress = ProgressReporter::for_turn(
        events.clone(),
        turn.clone(),
        policy.model_role,
        engine.context_policy(policy.model_role),
    );
    let output = engine
        .run_model_tool_loop(
            &accepted,
            &policy,
            progress,
            PlanSubmissionGate::inert(None),
            FinalResponsePersistence::Deferred,
        )
        .await
        .unwrap();
    assert!(output.final_item.is_some());
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(|item| item.provider_replay().is_none())
    );
    let mut updates = collect_events(&mut receiver).await;
    assert!(
        !updates
            .iter()
            .any(|event| matches!(event, SessionEvent::UsageUpdated { .. }))
    );
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
    updates.extend(collect_events(&mut receiver).await);
    let display = updates.iter().position(|event| matches!(event, SessionEvent::ResponseAttemptUpdated { attempt, .. } if attempt.outcome == Outcome::Completed)).unwrap();
    let terminal = updates
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::TurnCompleted {
                    attempt_id: Some(_),
                    ..
                }
            )
        })
        .unwrap();
    let usage = updates
        .iter()
        .position(|event| matches!(event, SessionEvent::UsageUpdated { .. }))
        .unwrap();
    assert!(display < usage && usage < terminal);
    let items = zevria_transcript::load(engine.conversation.path()).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|item| item.provider_replay().is_some())
            .count(),
        1
    );
    assert!(
        items
            .iter()
            .all(|item| !matches!(item, TranscriptItem::IncompleteResponse(_)))
    );
}
