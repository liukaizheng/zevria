//! Short-interval loopback coverage; never waits for the production default.
use super::*;
use std::time::Duration;

#[tokio::test]
async fn parked_socket_sends_keepalives_without_publishing_pongs_or_requiring_them() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (observed, ready) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        for _ in 0..3 {
            let message = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let WebSocketMessage::Ping(payload) = message else {
                panic!("expected client keepalive")
            };
            socket.send(WebSocketMessage::Pong(payload)).await.unwrap();
        }
        observed.send(()).unwrap();
        // Stop polling (and answering) here: missing pongs are not fatal in 1.2.
        let _ = released.await;
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut session = OpenAiWebSocketSession::new(socket, Duration::from_millis(15));
    ready.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(75), session.next())
            .await
            .is_err()
    );
    assert_eq!(session.terminal_status(), None);
    release.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn zero_interval_disables_client_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(75), socket.next())
                .await
                .is_err()
        );
        socket
            .send(WebSocketMessage::text("ordinary event"))
            .await
            .unwrap();
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut session = OpenAiWebSocketSession::new(socket, Duration::ZERO);
    let event = tokio::time::timeout(Duration::from_secs(1), session.next())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, OpenAiWebSocketInbound::Message(WebSocketMessage::Text(text)) if text == "ordinary event")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn client_keepalive_pongs_never_extend_the_response_watchdog() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        let _ = receive_json(&mut socket).await;
        let mut pings = 0;
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(message)) = socket.next().await {
                if let WebSocketMessage::Ping(payload) = message {
                    pings += 1;
                    if socket.send(WebSocketMessage::Pong(payload)).await.is_err() {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        pings
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_pump(
        &url,
        OpenAiWebSocketSession::new(socket, Duration::from_millis(10)),
        None,
    );
    let parts = RequestParts {
        tools: zevria_model::ToolDeclarations::default(),
        forbid_tool_calls: false,
        prompt: Message::user("no model progress"),
        history: vec![],
        instructions: "stable".into(),
        allowed_tool_names: None,
    };
    let policy = RecoveryPolicy {
        response_idle_timeout: Duration::from_millis(150),
        stall_warning: Duration::from_millis(25),
        ..fast_recovery_policy(1)
    };
    let result = run_turn_request_with_recovery(
        parts.request(),
        &mut provider,
        &mut AttemptState::default(),
        &discard_updates(),
        &policy,
    )
    .await;
    assert!(result.is_err(), "pongs are not model progress");
    assert_eq!(
        provider.ws.session.terminal_status(),
        Some(OpenAiWebSocketTerminalCategory::ResponseIdleTimeout)
    );
    assert!(
        server.await.unwrap() >= 2,
        "keepalives must have occurred during the response"
    );
}
