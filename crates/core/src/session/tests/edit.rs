use super::*;

#[tokio::test]
async fn submit_edit_replaces_the_message_and_rewrites_the_transcript() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("second answer")),
        Ok(Message::assistant("revised answer")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            prompt_message_edit(0, "first revised", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    // The target and everything after it are gone; the revision runs as
    // the newest turn with a fresh request history.
    assert_eq!(
        engine.history(),
        vec![
            Message::user("first revised"),
            Message::assistant("revised answer")
        ]
    );
    {
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].prompt, Message::user("first revised"));
        assert!(requests[2].history.is_empty());
    }
    // The persisted transcript drops the edited turn's items too.
    let loaded = zevria_transcript::transcript::load(engine.conversation().path())
        .expect("transcript loads");
    assert_eq!(
        conversation_records(&loaded),
        vec![
            TranscriptItem::Message(Message::user("first revised")),
            TranscriptItem::Message(Message::assistant("revised answer")),
        ]
    );
    // The live TurnStarted carried the revision as the newest turn.
    let started = collect_events(&mut receiver)
        .await
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStarted { message, .. } => Some(message),
            _ => None,
        })
        .next_back()
        .expect("a turn started");
    assert_eq!(started, Message::user("first revised"));
}

#[tokio::test]
async fn submit_edit_out_of_range_fails_without_touching_history_or_transcript() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("answer"))]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let before = engine.history().to_vec();
    let before_items = engine.conversation.items().to_vec();
    engine
        .handle_command(
            // The session holds exactly one prompt, numbered zero.
            prompt_message_edit(1, "revised", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.history(), before);
    assert_eq!(engine.conversation.items(), before_items);
    let loaded = zevria_transcript::transcript::load(engine.conversation().path())
        .expect("transcript loads");
    assert_eq!(
        conversation_records(&loaded),
        vec![
            TranscriptItem::Message(Message::user("question")),
            TranscriptItem::Message(Message::assistant("answer")),
        ],
        "a stale edit address must not append through the old tail"
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| { matches!(event, SessionEvent::TurnRejected { .. }) })
    );
}

#[tokio::test]
async fn submit_edit_on_a_resumed_session_rewrites_the_reopened_transcript() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("new answer"))]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (directory, mut transcript) = test_transcript();
    let prior = vec![
        TranscriptItem::Message(Message::user("old question")),
        TranscriptItem::provider_message(ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "message",
                "id": "msg_old",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "old answer"}]
            })],
        ))
        .expect("native replay should derive a message"),
    ];
    for item in &prior {
        transcript.append(item).expect("append should succeed");
    }
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_fixture(prior)
    .unwrap();
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            prompt_message_edit(0, "edited question", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(
        engine.history(),
        vec![
            Message::user("edited question"),
            Message::assistant("new answer")
        ]
    );
    {
        let requests = requests.lock().expect("requests lock");
        assert!(requests[0].history.is_empty());
    }
    // The resumed session's file on disk is truncated to the edit point.
    let reloaded = zevria_transcript::transcript::load(
        &directory
            .path()
            .join(format!("{}.jsonl", engine.conversation().session_id())),
    )
    .expect("transcript reloads");
    assert_eq!(
        conversation_records(&reloaded),
        vec![
            TranscriptItem::Message(Message::user("edited question")),
            TranscriptItem::Message(Message::assistant("new answer")),
        ]
    );
}

#[tokio::test]
async fn submit_edit_addresses_the_numbered_prompt_not_the_last_matching_text() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("middle answer")),
        Ok(Message::assistant("last answer")),
        Ok(Message::assistant("revised answer")),
    ]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, _receiver) = session_event_channel(1024);
    // Repeating a prompt verbatim is ordinary ("continue", "yes"), so the
    // ordinal — not the text — has to decide which turn is replaced.
    for text in ["continue", "middle", "continue"] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }

    engine
        .handle_command(
            prompt_message_edit(0, "continue please", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    // The *first* "continue" is replaced, taking the later duplicate and
    // everything between them with it.
    assert_eq!(
        engine.history(),
        vec![
            Message::user("continue please"),
            Message::assistant("revised answer")
        ]
    );
    let loaded = zevria_transcript::transcript::load(engine.conversation().path())
        .expect("transcript loads");
    assert_eq!(
        conversation_records(&loaded),
        vec![
            TranscriptItem::Message(Message::user("continue please")),
            TranscriptItem::Message(Message::assistant("revised answer")),
        ]
    );
}
