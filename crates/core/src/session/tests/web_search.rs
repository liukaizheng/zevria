use super::*;
use zevria_content::WebSearchActivity;
use zevria_content::WebSearchAttemptOutcome as Outcome;
use zevria_content::WebSearchAttemptRecord;
use zevria_content::WebSearchStatus as Status;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

fn attempt() -> WebSearchAttemptRecord {
    let mut attempt = WebSearchAttemptRecord::new(test_profile());
    attempt.activity = vec![
        WebSearchActivity {
            item_id: Some("done".into()),
            output_index: 0,
            status: Status::Completed,
            action: Some(json!({"type":"search","query":"Rust"})),
        },
        WebSearchActivity {
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
    attempt.terminal.insert(0, Status::Completed);
    attempt.finish(Outcome::Interrupted);
    let record = TranscriptItem::WebSearchAttempt(attempt.clone());
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
    assert_eq!(attempt.activity[0].status, Status::Completed);
    assert_eq!(attempt.activity[1].status, Status::Searching);
    assert_eq!(
        attempt.status_label(&attempt.activity[1]),
        "completion unconfirmed"
    );
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["zevria_web_search_attempt"]["version"], 1);
    for version in [2, 99] {
        value["zevria_web_search_attempt"]["version"] = json!(version);
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
            display_attempt_id: None,
            turn_id: TurnId::new(0),
        })
        .await
        .unwrap();
    let mut observed = attempt();
    let expected = observed.id.clone();
    let clone = progress.clone();
    {
        let update = clone.web_search_updated(observed.clone());
        tokio::pin!(update);
        assert!(matches!(
            futures_util::poll!(&mut update),
            std::task::Poll::Pending
        ));
        // Dropping the blocked delivery is the same boundary as dropping a
        // cancelled provider future. Collection already happened.
    }
    observed.finish(Outcome::Interrupted);
    let records = progress.drain_web_search(Outcome::Interrupted);
    assert_eq!(records, vec![observed]);
    assert_eq!(records[0].id, expected);
    assert!(clone.drain_web_search(Outcome::Failed).is_empty());
    assert!(receiver.try_recv().is_ok());
}

struct ObservingProvider {
    exit: Outcome,
    dropped: Arc<AtomicBool>,
    sabotage: Option<PathBuf>,
    // Retain the obstruction after complete() returns or is cancelled.
    blocker: Option<TranscriptRewriteBlocker>,
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
            first.finish(Outcome::Failed);
            progress.web_search_updated(first).await;
            let second = attempt();
            progress.web_search_updated(second).await;
            if let Some(path) = &self.sabotage {
                self.blocker = Some(
                    TranscriptRewriteBlocker::new(path)
                        .expect("block search transcript replacement"),
                );
            }
            match self.exit {
                Outcome::Completed => Ok(model_response(Message::assistant("done"))),
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
            sabotage: None,
            blocker: None,
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
                if let TranscriptItem::WebSearchAttempt(attempt) = item {
                    Some(attempt)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].outcome, Outcome::Failed);
        assert_eq!(attempts[1].outcome, exit);
        assert_eq!(attempts[1].activity[0].status, Status::Completed);
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
        let restored = zevria_transcript::reconstruct_transcript(&items);
        assert_eq!(
            zevria_transcript::transcript::model_input(&items),
            zevria_transcript::transcript::model_input(&restored)
        );
        let lifecycle = collect_events(&mut receiver).await;
        let final_activity = lifecycle
            .iter()
            .rposition(|event| matches!(event, SessionEvent::WebSearchUpdated { .. }))
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
            0
        );
    }
}

#[tokio::test]
async fn degraded_search_write_retains_memory_and_reports_failure() {
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
    let provider = ObservingProvider {
        exit: Outcome::Failed,
        dropped: Arc::new(AtomicBool::new(false)),
        sabotage: Some(path),
        blocker: None,
    };
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
    assert_eq!(
        engine
            .conversation
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
            .count(),
        2
    );
    assert!(engine.conversation.persistence_error().is_some());
    assert!(engine.provider.dropped.load(Ordering::SeqCst));
    let blocker = engine.provider.blocker.as_ref().unwrap();
    assert!(
        engine.conversation.path().is_dir(),
        "returning from the provider must not restore the transcript"
    );
    assert!(blocker.backup_path().is_file());
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::PersistenceChanged { error: Some(_), .. }
    )));
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
            progress.web_search_updated(attempt()).await;
            Ok(model_response(Message::Assistant {
                id: None,
                content: vec![named_tool_call("local", "verify_search_durable", json!({}))],
            }))
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
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::WebSearchAttempt(_)))
                .count(),
            1
        );
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
                    display_attempt_id: None,
                    turn_id: progress.turn().id,
                })
                .await
                .unwrap();
            let update = progress.web_search_updated(attempt());
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
            if let TranscriptItem::WebSearchAttempt(attempt) = item {
                Some(attempt)
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
    assert_eq!(attempts[0].activity[0].status, Status::Completed);
    assert_eq!(attempts[0].activity[1].status, Status::Searching);
    assert_eq!(
        attempts[0].status_label(&attempts[0].activity[1]),
        "completion unconfirmed"
    );
    assert!(progress.drain_web_search(Outcome::Failed).is_empty());
    assert!(engine.conversation.persistence_error().is_none());
}

#[tokio::test]
async fn independent_hosted_updates_do_not_fence_root_or_child_previews() {
    let (events, mut receiver) = session_event_channel(32);
    let turn_id = TurnId::new(3);
    events.stream_updated(turn_id, Message::assistant("existing answer"));
    events
        .send(SessionEvent::WebSearchUpdated {
            turn_id,
            attempt: attempt(),
        })
        .await
        .unwrap();
    let first = receiver.try_recv().unwrap();
    assert!(matches!(
        first,
        SessionUpdate::Lifecycle(SessionEvent::WebSearchUpdated { .. })
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
            event: Box::new(SessionEvent::WebSearchUpdated {
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
