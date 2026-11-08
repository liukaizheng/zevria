use super::*;
use zevria_model::models::{ModelSelection, SessionModels};

#[tokio::test]
async fn single_messages_router_uses_the_core_local_summary_path() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = profile(format!(
        "http://{}/v1/messages",
        listener.local_addr().unwrap()
    ));
    profile.retained_user_tokens = 0;
    if let ProfileProtocol::Anthropic { endpoint, .. } = &mut profile.protocol {
        endpoint.input_token_count.enabled = false;
    }
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert!(request.body.get("tools").is_none());
        send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
        request
    });
    let tools = ToolServer::new().run();
    let router = ProviderRouter::from_routes(
        [(
            ModelRole::Build,
            profile.clone(),
            zevria_foundation::ReasoningLevel::High,
        )],
        "stable",
        "root",
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let path = writer.path().to_path_buf();
    let selection = ModelSelection::new(
        profile.profile.clone(),
        zevria_foundation::ReasoningLevel::High,
    );
    let history = vec![
        TranscriptItem::SessionModels(SessionModels::new(selection.clone(), selection).unwrap()),
        TranscriptItem::Message(Message::user("old prompt")),
        TranscriptItem::Message(Message::assistant("old answer")),
    ];
    let mut engine = SessionEngine::new(
        router,
        tools,
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_transcript_items(history)
    .unwrap()
    .with_compaction_policy(
        zevria_model::CompactionPolicy::new(
            Default::default(),
            std::array::from_fn(|_| profile.context_policy()),
        )
        .unwrap(),
    );
    let (events, mut receiver) = session_event_channel(256);
    engine
        .handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let mut failed = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(SessionEvent::TurnFailed { error, .. }) = update {
            failed.push(error);
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
    drop(engine);
    let loaded = transcript::load(&path).unwrap();
    assert!(loaded.iter().any(|item|matches!(item,TranscriptItem::Compaction(checkpoint) if checkpoint.backend==zevria_model::CompactionBackend::LocalSummary)));
    server.await.unwrap();
}
