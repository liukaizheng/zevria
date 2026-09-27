use crate::*;
#[tokio::test]
async fn retry_reporter_forwards_delay_and_silent_reporters_suppress_progress() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn = TurnContext::new(TurnId::new(7), SessionMode::Build, CancellationToken::new());
    let policy = CompactionPolicy::default();
    let reporter = ProgressReporter::for_turn(
        sender.clone(),
        turn.clone(),
        ModelRole::Build,
        policy.for_role(ModelRole::Build),
    );
    let retry_after = std::time::Duration::from_millis(500);
    reporter.stream_updated(Message::assistant("stale preview"));
    reporter.retrying(2, 5, retry_after, "offline".into()).await;
    assert_eq!(
        receiver.try_recv(),
        Ok(SessionUpdate::Lifecycle(SessionEvent::TurnRetrying {
            turn_id: turn.id,
            attempt: 2,
            max_attempts: 5,
            retry_after,
            error: "offline".into(),
        }))
    );
    assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));

    let silent = ProgressReporter::silent_for_turn(
        sender,
        turn,
        ModelRole::Build,
        policy.for_role(ModelRole::Build),
    );
    silent
        .retrying(1, 5, retry_after, "hidden summary retry".into())
        .await;
    silent.stream_updated(Message::assistant("hidden summary"));
    silent.stream_cleared();
    silent.usage_updated(TokenUsage::default()).await;
    assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
}

#[test]
fn streaming_updates_coalesce_while_lifecycle_events_stay_bounded() {
    let (events, mut receiver) = session_event_channel(1);
    let turn_id = TurnId::new(9);
    events
        .try_send(SessionEvent::TurnStarted {
            turn_id,
            message: Message::user("question"),
            mode: SessionMode::Build,
        })
        .expect("the sole lifecycle slot is available");

    for index in 0..1_000 {
        events
            .try_send(SessionEvent::AssistantStreamUpdated {
                turn_id,
                snapshot: (Message::assistant(format!("draft {index}"))).into(),
            })
            .expect("stream updates replace the watch value instead of queueing");
    }

    assert!(matches!(
        events.try_send(SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id,
            message: Message::assistant("done"),
        }),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    events.stream_updated(turn_id, Message::assistant("latest after full"));

    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionUpdate::Lifecycle(SessionEvent::TurnStarted { .. }))
    ));
    let batch = match receiver.try_recv().expect("latest stream batch") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert_eq!(
        batch.root,
        Some(SessionStreamState {
            attempt: None,
            // TurnStarted is 1, the original 1,000 streams are 2..=1,001,
            // the failed try_send consumes nothing, and this is 1,002.
            revision: 1_002,
            turn_id,
            message: Some(Message::assistant("latest after full")),
        })
    );
}

#[test]
fn slow_consumers_receive_only_the_latest_large_indexed_answer_and_collector_revision() {
    let (events, mut receiver) = session_event_channel(1);
    let progress = ProgressReporter::new(events);
    let mut attempt = zevria_content::WebSearchAttemptRecord::new(
        zevria_foundation::ModelProfileRef::new("test", "test"),
    );
    attempt
        .presentation
        .push(zevria_content::AssistantPresentationPart {
            source: zevria_content::AssistantSourceAddress {
                output_index: 0,
                part: zevria_content::AssistantPartIdentity::Content(0),
                item_id: Some("answer".into()),
            },
            content: zevria_content::AssistantPresentationContent::Answer {
                text: String::new(),
            },
        });
    let mut answer = String::new();
    for _ in 0..1000 {
        answer.push_str("🦀 Readable provisional answer, with an unfinished **Markdown span.\n");
        attempt.presentation[0].content = zevria_content::AssistantPresentationContent::Answer {
            text: answer.clone(),
        };
        attempt.touch();
        progress.stream_snapshot(zevria_content::AssistantStreamSnapshot {
            message: Some(Message::assistant(&answer)),
            attempt: Some(attempt.clone()),
        });
    }
    let SessionUpdate::Streams(batch) = receiver.try_recv().unwrap() else {
        panic!("latest preview")
    };
    let root = batch.root.unwrap();
    assert_eq!(root.attempt.as_ref(), Some(&attempt));
    assert_eq!(
        zevria_content::assistant_plain_text(root.message.as_ref().unwrap()),
        answer
    );
    assert!(
        matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "no queued token frames"
    );
    attempt.finish(zevria_content::WebSearchAttemptOutcome::Interrupted);
    assert_eq!(
        progress.drain_web_search(zevria_content::WebSearchAttemptOutcome::Interrupted),
        [attempt]
    );
}

#[tokio::test]
async fn model_call_markers_are_lossless_and_fence_root_and_forwarded_previews() {
    let (sender, mut receiver) = session_event_channel(4);
    let turn_id = TurnId::new(3);
    let child_id = SubtaskId::generate();
    let child_call = SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::ModelCallStarted {
            turn_id: TurnId::new(1),
            call: 2,
        }),
    };
    let root_call = SessionEvent::ModelCallStarted { turn_id, call: 1 };
    sender.send(root_call.clone()).await.unwrap();
    sender.stream_updated(turn_id, Message::assistant("root preview"));
    sender.send(child_call.clone()).await.unwrap();
    sender.set_subtask_stream(
        child_id.clone(),
        SessionStreamState {
            turn_id: TurnId::new(1),
            revision: 1,
            message: Some(Message::assistant("child preview")),
            attempt: None,
        },
    );
    assert_eq!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(root_call))
    );
    assert_eq!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(child_call))
    );
    let Some(SessionUpdate::Streams(batch)) = receiver.recv().await else {
        panic!("stream batch")
    };
    assert_eq!(batch.root.unwrap().turn_id, turn_id);
    assert_eq!(batch.subtasks.len(), 1);
    assert_eq!(batch.subtasks[&child_id].turn_id, TurnId::new(1));
}

#[tokio::test]
async fn agent_previews_coalesce_and_lossless_updates_create_boundaries() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(10);
    let ensemble_run_id = EnsembleRunId::new();
    let agent_run_id = AgentRunId::new();
    for text in ["hello ", "world"] {
        sender
            .send(SessionEvent::AgentRunUpdated {
                turn_id,
                ensemble_run_id: ensemble_run_id.clone(),
                agent_run_id: agent_run_id.clone(),
                event: AgentRunEvent::AgentMessage {
                    text: text.to_string(),
                    message_id: Some("message-1".to_string()),
                },
            })
            .await
            .expect("preview publishes");
    }
    let batch = match receiver.recv().await.expect("coalesced preview") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert!(matches!(
        &batch.agent_runs.first().expect("one worker preview").event,
        AgentRunEvent::AgentMessage { text, .. } if text == "hello world"
    ));

    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Running,
                detail: None,
            },
        })
        .await
        .expect("lossless status publishes");
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
            event: AgentRunEvent::Status { .. },
            ..
        }))
    ));
    assert_eq!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty),
        "the boundary must not republish the preceding preview"
    );

    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id,
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::AgentMessage {
                text: "fresh".to_string(),
                message_id: Some("message-1".to_string()),
            },
        })
        .await
        .expect("new segment publishes");
    let batch = match receiver.recv().await.expect("fresh preview") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert!(matches!(
        &batch.agent_runs.first().expect("one worker preview").event,
        AgentRunEvent::AgentMessage { text, .. } if text == "fresh"
    ));
}

#[tokio::test]
async fn protocol_diagnostics_do_not_split_agent_message_previews() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(11);
    let ensemble_run_id = EnsembleRunId::new();
    let agent_run_id = AgentRunId::new();

    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::AgentMessage {
                text: "I".to_string(),
                message_id: Some("message-1".to_string()),
            },
        })
        .await
        .expect("first token publishes");
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch { agent_runs, .. }))
            if matches!(
                &agent_runs.first().expect("one worker preview").event,
                AgentRunEvent::AgentMessage { text, .. } if text == "I"
            )
    ));

    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::Protocol {
                direction: crate::AgentProtocolDirection::AgentToClient,
                json: "token packet".to_string(),
            },
        })
        .await
        .expect("protocol diagnostic publishes");
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
            event: AgentRunEvent::Protocol { .. },
            ..
        }))
    ));

    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id,
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::AgentMessage {
                text: "’ll inspect".to_string(),
                message_id: Some("message-1".to_string()),
            },
        })
        .await
        .expect("second token publishes");
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch { agent_runs, .. }))
            if matches!(
                &agent_runs.first().expect("one worker preview").event,
                AgentRunEvent::AgentMessage { text, .. } if text == "I’ll inspect"
            )
    ));
}

#[tokio::test]
async fn unread_thought_survives_protocol_and_message_preview_burst() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(12);
    let ensemble_run_id = EnsembleRunId::new();
    let agent_run_id = AgentRunId::new();
    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::Thought {
                text: "private analysis".to_string(),
                message_id: Some("thought-1".to_string()),
            },
        })
        .await
        .expect("thought preview");
    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: agent_run_id.clone(),
            event: AgentRunEvent::Protocol {
                direction: crate::AgentProtocolDirection::AgentToClient,
                json: "interleaved packet".to_string(),
            },
        })
        .await
        .expect("protocol lifecycle");
    sender
        .send(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id,
            agent_run_id,
            event: AgentRunEvent::AgentMessage {
                text: "public report".to_string(),
                message_id: Some("message-1".to_string()),
            },
        })
        .await
        .expect("message preview");

    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch { agent_runs, .. }))
            if matches!(
                agent_runs.as_slice(),
                [AgentRunStreamState {
                    event: AgentRunEvent::Thought { text, .. },
                    ..
                }] if text == "private analysis"
            )
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
            event: AgentRunEvent::Protocol { .. },
            ..
        }))
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch { agent_runs, .. }))
            if matches!(
                agent_runs.as_slice(),
                [AgentRunStreamState {
                    event: AgentRunEvent::AgentMessage { text, .. },
                    ..
                }] if text == "public report"
            )
    ));
}

#[tokio::test]
async fn unread_root_lifecycle_events_fence_older_streams() {
    let turn_id = TurnId::new(12);
    let fencing_events = [
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id,
            message: Message::assistant("committed tool call"),
        },
        SessionEvent::ToolResults {
            turn_id,
            message: Message::user("tool result"),
            metadata: Vec::new(),
        },
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id,
            message: Message::assistant("done"),
        },
        SessionEvent::TurnFailed {
            turn_id,
            error: "failed".to_string(),
        },
        SessionEvent::TurnCancelled { turn_id },
        SessionEvent::TurnRetrying {
            turn_id,
            attempt: 1,
            max_attempts: 3,
            retry_after: std::time::Duration::from_millis(500),
            error: "disconnected".to_string(),
        },
    ];

    for event in fencing_events {
        let (sender, mut receiver) = session_event_channel(8);
        sender.stream_updated(turn_id, Message::assistant("stale preview"));
        sender.send(event.clone()).await.expect("lifecycle queues");

        assert_eq!(receiver.try_recv(), Ok(SessionUpdate::Lifecycle(event)));
        assert_eq!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
            "an unread lifecycle event must discard its older preview"
        );
    }
}

#[tokio::test]
async fn a_consumed_preview_precedes_and_is_then_cleared_by_lifecycle() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(13);
    sender.stream_updated(turn_id, Message::assistant("visible draft"));

    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch {
            root: Some(SessionStreamState {
                message: Some(Message::Assistant { .. }),
                ..
            }),
            ..
        }))
    ));

    let committed = SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id,
        message: Message::assistant("committed"),
    };
    sender
        .send(committed.clone())
        .await
        .expect("lifecycle queues");
    assert_eq!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(committed))
    );
}

#[tokio::test]
async fn a_post_lifecycle_preview_survives_while_the_older_preview_is_discarded() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(14);
    sender.stream_updated(turn_id, Message::assistant("old preview"));
    let committed = SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id,
        message: Message::assistant("tool call"),
    };
    sender
        .send(committed.clone())
        .await
        .expect("lifecycle queues");
    sender.stream_updated(turn_id, Message::assistant("new preview"));

    assert_eq!(receiver.try_recv(), Ok(SessionUpdate::Lifecycle(committed)));
    let batch = match receiver.try_recv().expect("new preview remains") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert_eq!(
        batch.root.map(|state| state.message),
        Some(Some(Message::assistant("new preview")))
    );
    assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
}

#[tokio::test]
async fn child_forwarding_through_the_parent_fences_an_unread_child_preview() {
    let (child_sender, mut child_receiver) = session_event_channel(8);
    let (parent_sender, mut parent_receiver) = session_event_channel(8);
    let child_id = SubtaskId::new("forwarded-child");
    let turn_id = TurnId::new(16);

    child_sender.stream_updated(turn_id, Message::assistant("child preview"));
    let child_stream = match child_receiver.recv().await.expect("child stream") {
        SessionUpdate::Streams(batch) => batch.root.expect("root child state"),
        update => panic!("expected child streams, got {update:?}"),
    };
    parent_sender.set_subtask_stream(child_id.clone(), child_stream);

    let child_commit = SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id,
        message: Message::assistant("child commit"),
    };
    child_sender
        .send(child_commit.clone())
        .await
        .expect("child lifecycle queues");
    let forwarded = match child_receiver.recv().await.expect("child lifecycle") {
        SessionUpdate::Lifecycle(event) => SessionEvent::SubtaskSession {
            id: child_id,
            event: Box::new(event),
        },
        update => panic!("expected child lifecycle, got {update:?}"),
    };
    parent_sender
        .send(forwarded.clone())
        .await
        .expect("parent lifecycle queues");

    assert_eq!(
        parent_receiver.try_recv(),
        Ok(SessionUpdate::Lifecycle(forwarded))
    );
    assert_eq!(
        parent_receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
}
