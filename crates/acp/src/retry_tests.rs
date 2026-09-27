//! Exercise the live retry consumer, including stream boundaries, over ACP.

use super::*;
use agent_client_protocol::schema::v1::{ContentBlock, SessionNotification};
use agent_client_protocol::{Agent, Channel, Client};
use rig_core::message::Message;
use std::time::Duration;

struct NoopLifecycle;

impl SessionRuntimeLifecycle for NoopLifecycle {
    fn shutdown(
        self: Box<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn live_retry_diagnostics_reset_streams_without_changing_their_channel() {
    let delays = [
        Duration::ZERO,
        Duration::from_millis(500),
        Duration::from_millis(1500),
        Duration::from_secs(4),
    ];
    let (client_transport, server_transport) = Channel::duplex();
    let (client_done, client_received) = oneshot::channel();
    let (server_done, server_received) = oneshot::channel();
    let done = Arc::new(Mutex::new(Some((client_done, server_done))));
    let updates = Arc::new(Mutex::new(Vec::new()));
    let log = updates.clone();
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                let mut updates = log.lock().unwrap();
                updates.push(notification.update);
                if updates.len() == delays.len() * 3 {
                    let (client_done, server_done) = done.lock().unwrap().take().unwrap();
                    let _ = client_done.send(());
                    let _ = server_done.send(());
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_transport, async move |_connection| {
            client_received.await.unwrap();
            Ok(())
        });
    let server = Agent
        .builder()
        .connect_with(server_transport, async move |connection| {
            let (commands, _receiver) = mpsc::unbounded_channel();
            let permit = Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap();
            let live = LiveSession::new(
                "retry-test".into(),
                PathBuf::from("/retry-test"),
                commands,
                Box::new(NoopLifecycle),
                permit,
                SessionMode::Build,
                PlanWorkflowState::Idle,
                connection,
                ClientState::default(),
                ExecutionProfile::Interactive,
            );
            for (index, delay) in delays.into_iter().enumerate() {
                let turn_id = TurnId::new(index as u64 + 1);
                live.handle_event(SessionEvent::TurnStarted {
                    turn_id,
                    message: Message::user("prompt"),
                    mode: SessionMode::Build,
                })?;
                live.handle_event(SessionEvent::AssistantStreamUpdated {
                    turn_id,
                    snapshot: (Message::assistant("prefix")).into(),
                })?;
                live.handle_event(SessionEvent::TurnRetrying {
                    turn_id,
                    attempt: 2,
                    max_attempts: 5,
                    retry_after: delay,
                    error: "offline".into(),
                })?;
                live.handle_event(SessionEvent::AssistantStreamUpdated {
                    turn_id,
                    snapshot: (Message::assistant("prefix resumed")).into(),
                })?;
            }
            server_received.await.unwrap();
            Ok(())
        });
    let (client, server) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(client, server)
    })
    .await
    .unwrap();
    client.unwrap();
    server.unwrap();
    let updates = updates.lock().unwrap();
    for (index, expected) in [
        "Provider retry 2/5: offline",
        "Provider retry 2/5 (next attempt in 1s): offline",
        "Provider retry 2/5 (next attempt in 2s): offline",
        "Provider retry 2/5 (next attempt in 4s): offline",
    ]
    .into_iter()
    .enumerate()
    {
        let [
            AcpSessionUpdate::AgentMessageChunk(first),
            AcpSessionUpdate::AgentThoughtChunk(notice),
            AcpSessionUpdate::AgentMessageChunk(resumed),
        ] = &updates[index * 3..index * 3 + 3]
        else {
            panic!("message/diagnostic/message ordering")
        };
        let ContentBlock::Text(text) = &notice.content else {
            panic!("text diagnostic")
        };
        assert_eq!(text.text, expected);
        assert_eq!(
            notice.message_id.as_ref().unwrap().0.as_ref(),
            format!("zevria-turn-{}-status-retry", index + 1)
        );
        let ContentBlock::Text(text) = &resumed.content else {
            panic!("resumed text")
        };
        assert_eq!(
            text.text, "prefix resumed",
            "retry reset must resend the shared prefix"
        );
        assert_ne!(
            first.message_id, resumed.message_id,
            "retry resets the stream segment"
        );
    }
}
