use super::*;

#[tokio::test]
async fn root_and_subtask_fences_are_independent_and_relays_get_parent_sequences() {
    let (sender, mut receiver) = session_event_channel(8);
    let turn_id = TurnId::new(15);
    let first_child = SubtaskId::new("first-child");
    let second_child = SubtaskId::new("second-child");

    sender.stream_updated(turn_id, Message::assistant("root preview"));
    sender.set_subtask_stream(
        first_child.clone(),
        SessionStreamState {
            attempt: None,
            revision: 700,
            turn_id,
            message: Some(Message::assistant("stale first child")),
        },
    );
    sender.set_subtask_stream(
        second_child.clone(),
        SessionStreamState {
            attempt: None,
            revision: 900,
            turn_id,
            message: Some(Message::assistant("second child")),
        },
    );
    let child_commit = SessionEvent::SubtaskSession {
        id: first_child.clone(),
        event: Box::new(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id,
            message: Message::assistant("first child commit"),
        }),
    };
    sender
        .send(child_commit.clone())
        .await
        .expect("child lifecycle queues");

    assert_eq!(
        receiver.try_recv(),
        Ok(SessionUpdate::Lifecycle(child_commit))
    );
    let batch = match receiver.try_recv().expect("independent targets remain") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert_eq!(
        batch.root.and_then(|state| state.message),
        Some(Message::assistant("root preview"))
    );
    assert!(!batch.subtasks.contains_key(&first_child));
    assert_eq!(
        batch.subtasks.get(&second_child),
        Some(&SessionStreamState {
            attempt: None,
            // The supplied child-local revision 900 is replaced by the
            // third publication on this parent channel.
            revision: 3,
            turn_id,
            message: Some(Message::assistant("second child")),
        })
    );

    let root_commit = SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id,
        message: Message::assistant("root commit"),
    };
    sender
        .send(root_commit.clone())
        .await
        .expect("root lifecycle queues");
    sender.set_subtask_stream(
        first_child.clone(),
        SessionStreamState {
            attempt: None,
            revision: 1,
            turn_id,
            message: Some(Message::assistant("fresh first child")),
        },
    );
    assert_eq!(
        receiver.try_recv(),
        Ok(SessionUpdate::Lifecycle(root_commit))
    );
    let batch = match receiver.try_recv().expect("child survives root fence") {
        SessionUpdate::Streams(batch) => batch,
        update => panic!("expected streams, got {update:?}"),
    };
    assert_eq!(
        batch
            .subtasks
            .get(&first_child)
            .and_then(|state| state.message.as_ref()),
        Some(&Message::assistant("fresh first child"))
    );
}

#[tokio::test]
async fn failed_children_surface_as_correlated_tool_errors() {
    let (events, mut receiver) = session_event_channel(1024);
    let channels = subtask_channels("root-session", events.clone());
    let mut requests_rx = channels.requests;
    tokio::spawn(async move {
        let request = requests_rx.recv().await.expect("launch request");
        let _ = request.outcome.send(SubtaskOutcome::Failed {
            error: "websocket refused".to_string(),
        });
    });

    let tools = ToolServer::new()
        .tool(LaunchTestTool {
            launcher: channels.launcher.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![launch_call("call-a", "doomed explore task")],
        }),
        Ok(Message::assistant("noted the failure")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "explore it".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    let tool_results = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .expect("tool results event");
    assert_eq!(tool_results.1[0].outcome, ToolCallOutcome::Error);
    assert_eq!(
        tool_results.1[0]
            .subtasks()
            .first()
            .and_then(|entry| entry.launch.as_ref())
            .map(|metadata| metadata.title.as_str()),
        Some("doomed explore task")
    );
    let serialized = serde_json::to_string(tool_results.0).expect("message json");
    assert!(serialized.contains("status: error"));
    assert!(serialized.contains("websocket refused"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    // The model sees the failure in the continuation and can recover.
    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests.len(), 2);
}
