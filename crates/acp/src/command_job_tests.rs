use super::*;
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobNotice, CommandJobNoticeKind, CommandJobSnapshot,
    CommandJobStatus,
};
fn notice(id: &str, kind: CommandJobNoticeKind) -> CommandJobNotice {
    CommandJobNotice::new(
        kind,
        vec![CommandJobSnapshot {
            id: CommandJobId::new(id),
            call_id: None,
            command: "test background".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Exited {
                code: Some(1),
                success: false,
                signal: None,
            },
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            stdout_bytes: 10,
            stderr_bytes: 0,
        }],
        vec![],
        "Command job update: private stream output".into(),
    )
}
fn chunk_text(update: &AcpSessionUpdate) -> &str {
    let AcpSessionUpdate::AgentThoughtChunk(chunk) = update else {
        panic!("expected thought")
    };
    let ContentBlock::Text(text) = &chunk.content else {
        panic!("expected text")
    };
    &text.text
}
fn chunk_id(update: &AcpSessionUpdate) -> &str {
    let AcpSessionUpdate::AgentThoughtChunk(chunk) = update else {
        panic!("expected thought")
    };
    chunk.message_id.as_ref().unwrap().0.as_ref()
}
#[tokio::test]
async fn acp_ignores_offers_and_projects_live_and_replayed_notices_as_thought_chunks() {
    let workspace = tempfile::tempdir().unwrap();
    let factory = FakeFactory::new();
    let sender = factory.event_sender.clone();
    let commands = factory.commands.clone();
    let log = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let updates = log.clone();
    let checked = log.clone();
    let (client_transport, server_transport) = Channel::duplex();
    let root = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            root,
            Arc::new(factory),
            server_transport,
        )
        .await
    });
    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _| {
                updates.lock().unwrap().push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                let sender = sender.lock().unwrap().clone().unwrap();
                sender
                    .send(SessionEvent::JobContinuationChanged {
                        mode: Some(SessionMode::Build),
                    })
                    .await
                    .unwrap();
                sender
                    .send(SessionEvent::CommandJobsReported {
                        turn_id: TurnId::new(1),
                        notice: notice("job-live", CommandJobNoticeKind::Update),
                    })
                    .await
                    .unwrap();
                sender
                    .send(SessionEvent::JobContinuationStarted {
                        turn_id: TurnId::new(2),
                        mode: SessionMode::Build,
                        notice: notice("job-defensive", CommandJobNoticeKind::Continuation),
                    })
                    .await
                    .unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if checked.lock().unwrap().len() >= 2 {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(
                    checked.lock().unwrap().len(),
                    2,
                    "advertisements must produce no ACP update"
                );
                connection
                    .send_request(CloseSessionRequest::new(session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            }
        })
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    let notifications = log.lock().unwrap();
    for update in notifications.iter() {
        assert!(matches!(
            update.update,
            AcpSessionUpdate::AgentThoughtChunk(_)
        ));
        assert!(!chunk_text(&update.update).contains("private stream output"));
        assert!(chunk_text(&update.update).contains("exited 1"));
    }
    assert_ne!(
        chunk_id(&notifications[0].update),
        chunk_id(&notifications[1].update)
    );
    assert!(
        !commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| matches!(command, SessionCommand::Turn(_)))
    );
    let replay = crate::replay::replay_transcript(
        &[TranscriptItem::CommandJobNotice(notice(
            "job-live",
            CommandJobNoticeKind::Update,
        ))],
        workspace.path(),
    );
    assert_eq!(replay.len(), 1);
    assert!(matches!(replay[0], AcpSessionUpdate::AgentThoughtChunk(_)));
    assert_eq!(chunk_text(&replay[0]), chunk_text(&notifications[0].update));
}
