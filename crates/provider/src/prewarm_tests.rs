//! Opt-in handshakes are idle work, never completion recovery attempts.
use super::*;
use std::time::Duration;

fn profile(url: String, model: &str, enabled: bool, websocket: bool) -> ResolvedModelProfile {
    let mut profile = resolved_profile(
        "test-provider",
        model,
        url,
        "test-key",
        websocket,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        272_000,
    );
    let network = &mut profile.responses_endpoint_mut().network;
    network.prewarm = enabled;
    network.websocket_keepalive_seconds = 0;
    profile
}

async fn provider(url: String) -> OpenAiProvider {
    OpenAiProvider::unconnected(
        &profile(url, "gpt-test", true, true),
        ReasoningEffort::Medium,
        "prewarm instructions",
        "prewarm-session",
        "prewarm-cache",
    )
    .await
    .unwrap()
}

fn parts() -> RequestParts {
    RequestParts {
        tools: zevria_model::ToolDeclarations::default(),
        forbid_tool_calls: false,
        prompt: Message::user("question"),
        history: vec![],
        instructions: "prewarm instructions".into(),
        allowed_tool_names: None,
    }
}

async fn complete_once(provider: &mut OpenAiProvider) {
    provider.recovery = fast_recovery_policy(1);
    let request = parts();
    let response = provider
        .complete(request.request(), discard_updates())
        .await
        .unwrap();
    assert_eq!(response.message(), &Message::assistant("done"));
}

#[tokio::test]
async fn router_prewarm_is_opt_in_role_local_and_sends_no_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let other_url = format!("ws://{}", other.local_addr().unwrap());
    let mut router = ProviderRouter::from_routes(
        [
            (
                ModelRole::Build,
                profile(url, "build", true, true),
                ReasoningEffort::Medium,
            ),
            (
                ModelRole::Plan,
                profile(other_url, "plan", true, true),
                ReasoningEffort::Medium,
            ),
        ],
        "prewarm instructions",
        "prewarm-session",
    )
    .unwrap();
    assert_eq!(router.initialized_profile_count(), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    let (quiet, checked_quiet) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), socket.next())
                .await
                .is_err()
        );
        quiet.send(()).unwrap();
        let request = receive_json(&mut socket).await;
        assert_eq!(request["model"], "build");
        assert_eq!(request["parallel_tool_calls"], true);
        send_json(&mut socket, completed_event("prewarmed", "answer", "done")).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    });
    router.prewarm(ModelRole::Build).await;
    assert_eq!(router.initialized_profile_count(), 1);
    router.prewarm(ModelRole::Build).await; // Healthy sockets are not replaced.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), other.accept())
            .await
            .is_err()
    );
    checked_quiet.await.unwrap();
    let request = parts();
    assert_eq!(
        router
            .complete(request.request(), discard_updates())
            .await
            .unwrap()
            .message(),
        &Message::assistant("done")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn opt_out_and_http_only_profiles_do_not_connect_or_initialize_slots() {
    for (enabled, websocket) in [(false, true), (true, false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = profile(
            format!("http://{}", listener.local_addr().unwrap()),
            "gpt-test",
            enabled,
            websocket,
        );
        let mut router = ProviderRouter::from_routes(
            [(ModelRole::Build, profile, ReasoningEffort::Medium)],
            "instructions",
            "session",
        )
        .unwrap();
        router.prewarm(ModelRole::Build).await;
        assert_eq!(router.initialized_profile_count(), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn failed_prewarm_leaves_the_first_completion_a_fresh_one_attempt_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut refused, _) = listener.accept().await.unwrap();
        let _ = receive_http_headers(&mut refused).await;
        refused.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        drop(refused);
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        let _ = receive_json(&mut socket).await;
        send_json(&mut socket, completed_event("fresh", "answer", "done")).await;
    });
    let mut provider = provider(url).await;
    provider.prewarm(ModelRole::Build).await;
    assert_eq!(provider.ws.socket_generation, 0);
    assert_eq!(
        provider.ws.session.terminal_status(),
        Some(OpenAiWebSocketTerminalCategory::NotConnected)
    );
    complete_once(&mut provider).await;
    assert_eq!(provider.ws.socket_generation, 0);
    assert_eq!(continuation_response_id(&provider), Some("fresh"));
    server.await.unwrap();
}

#[tokio::test]
async fn dropping_a_prewarm_handshake_installs_nothing_and_the_first_request_can_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (observed, started) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut pending, _) = listener.accept().await.unwrap();
        let _ = receive_http_headers(&mut pending).await;
        observed.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pending.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut socket = accept_async(listener.accept().await.unwrap().0)
            .await
            .unwrap();
        let _ = receive_json(&mut socket).await;
        send_json(&mut socket, completed_event("after-drop", "answer", "done")).await;
    });
    let mut provider = provider(url).await;
    tokio::select! {
        () = provider.prewarm(ModelRole::Build) => panic!("handshake must remain pending"),
        result = started => result.unwrap(),
    }
    assert_eq!(provider.ws.socket_generation, 0);
    assert_eq!(
        provider.ws.session.terminal_status(),
        Some(OpenAiWebSocketTerminalCategory::NotConnected)
    );
    assert!(provider.ws.continuation.is_none());
    complete_once(&mut provider).await;
    server.await.unwrap();
}

#[tokio::test]
async fn prewarm_upgrade_required_selects_sticky_http_without_spending_a_completion_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut refused, _) = listener.accept().await.unwrap();
        let _ = receive_http_headers(&mut refused).await;
        refused
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        drop(refused);
        let (mut http, _) = listener.accept().await.unwrap();
        let _ = receive_http_json(&mut http).await;
        send_http_response(
            &mut http,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("http", "answer", "done").to_string()),
        )
        .await;
    });
    let mut provider = provider(url).await;
    provider.prewarm(ModelRole::Build).await;
    assert_eq!(provider.transport, crate::connection::OpenAiTransport::Http);
    assert_eq!(
        provider.http_fallback,
        Some(crate::connection::HttpFallback::Sticky(
            "websocket_upgrade_required"
        ))
    );
    provider.prewarm(ModelRole::Build).await;
    complete_once(&mut provider).await;
    server.await.unwrap();
}
