//! Exercise the live retry consumer, including stream boundaries, over ACP.

use super::*;
use agent_client_protocol::schema::v1::{ContentBlock, SessionNotification};
use agent_client_protocol::{Agent, Channel, Client};
use rig_core::message::Message;
use std::time::Duration;
use zevria_session_api::event::{NetworkStatus, NetworkTransport};

struct NoopLifecycle;

impl SessionRuntimeLifecycle for NoopLifecycle {
    fn shutdown(
        self: Box<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn live_network_diagnostics_are_quiet_until_stall_or_retry_and_preserve_streams() {
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
                if updates.len() == delays.len() * 10 {
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
                let _pending =
                    live.begin_prompt(zevria_content::UserPrompt::from_text("prompt"))?;
                live.handle_event(SessionEvent::TurnStarted {
                    turn_id,
                    message: Message::user("prompt"),
                    mode: SessionMode::Build,
                })?;
                let transport = if index % 2 == 0 {
                    NetworkTransport::WebSocket
                } else {
                    NetworkTransport::Http
                };
                let status = |call, attempt, status| SessionEvent::NetworkStatus {
                    turn_id,
                    call,
                    attempt,
                    max_attempts: 5,
                    transport,
                    status,
                };
                let routine_phases = || {
                    [
                        NetworkStatus::AttemptStarted,
                        NetworkStatus::Connecting,
                        NetworkStatus::AwaitingResponse,
                        NetworkStatus::ProgressResumed,
                    ]
                };
                for phase in routine_phases() {
                    live.handle_event(status(1, 1, phase))?; // no routine diagnostics
                }
                live.handle_event(SessionEvent::AssistantStreamUpdated {
                    turn_id,
                    snapshot: (Message::assistant("prefix")).into(),
                })?;
                let quiet = NetworkStatus::Quiet {
                    idle_for: Duration::from_secs(30),
                    retry_in: Duration::from_secs(150),
                };
                live.handle_event(status(1, 1, quiet.clone()))?;
                live.handle_event(status(1, 1, quiet.clone()))?; // no duplicate warning
                live.handle_event(status(99, 1, quiet.clone()))?; // stale call: no diagnostic or reset
                live.handle_event(SessionEvent::AssistantStreamUpdated {
                    turn_id,
                    snapshot: Message::assistant("prefix continued").into(),
                })?;
                live.handle_event(status(1, 1, NetworkStatus::ProgressResumed))?;
                live.handle_event(SessionEvent::TurnRetrying {
                    call: 1,
                    turn_id,
                    attempt: 2,
                    max_attempts: 5,
                    retry_after: delay,
                    error: "offline".into(),
                })?;
                for phase in routine_phases() {
                    live.handle_event(status(1, 2, phase))?; // recovery details remain visible
                }
                live.handle_event(status(1, 1, quiet.clone()))?; // stale attempt
                live.handle_event(SessionEvent::AssistantStreamUpdated {
                    turn_id,
                    snapshot: (Message::assistant("prefix resumed")).into(),
                })?;
                live.handle_event(SessionEvent::ModelCallStarted { turn_id, call: 2 })?;
                for phase in routine_phases() {
                    live.handle_event(status(2, 1, phase))?; // a new call starts quietly again
                }
                live.handle_turn_terminal(turn_id, false, None)?;
                live.handle_event(status(2, 1, quiet))?; // terminal cleanup
                assert!(live.state.lock().unwrap().network_scope.is_none());
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
            AcpSessionUpdate::AgentThoughtChunk(quiet),
            AcpSessionUpdate::AgentMessageChunk(continued),
            AcpSessionUpdate::AgentThoughtChunk(progress),
            AcpSessionUpdate::AgentThoughtChunk(notice),
            AcpSessionUpdate::AgentThoughtChunk(started),
            AcpSessionUpdate::AgentThoughtChunk(connecting),
            AcpSessionUpdate::AgentThoughtChunk(awaiting),
            AcpSessionUpdate::AgentThoughtChunk(recovered),
            AcpSessionUpdate::AgentMessageChunk(resumed),
        ] = &updates[index * 10..index * 10 + 10]
        else {
            panic!("message/diagnostic/message ordering")
        };
        assert_eq!(
            first.message_id, continued.message_id,
            "quiet warning must not reset the stream segment"
        );
        let ContentBlock::Text(continuation) = &continued.content else {
            panic!("text stream")
        };
        assert_eq!(continuation.text, " continued");
        let ContentBlock::Text(warning) = &quiet.content else {
            panic!("text warning")
        };
        assert!(
            warning
                .text
                .contains("No response progress for 30s · automatic retry in 2m 30s")
        );
        let transport = if index % 2 == 0 { "WebSocket" } else { "Http" };
        for (diagnostic, expected) in [
            (progress, "Response progress resumed".to_string()),
            (started, format!("Starting {transport} attempt 2/5")),
            (
                connecting,
                format!("Connecting via {transport} · attempt 2/5"),
            ),
            (awaiting, "Awaiting response · attempt 2/5".to_string()),
            (recovered, "Response progress resumed".to_string()),
        ] {
            let ContentBlock::Text(text) = &diagnostic.content else {
                panic!("text diagnostic")
            };
            assert_eq!(text.text, expected);
        }
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
