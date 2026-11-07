use super::*;
use crate::credential::test_support::KeyHelper;
use std::time::Duration;

fn command_profile(url: String, auth: AnthropicAuth, source: ApiKeySource) -> ResolvedModelProfile {
    let mut profile = profile(url);
    let ProfileProtocol::Anthropic { endpoint, .. } = &mut profile.protocol else {
        unreachable!()
    };
    endpoint.auth = auth;
    endpoint.api_key = source;
    endpoint.network.max_attempts = 1;
    profile
}

#[tokio::test]
async fn command_credentials_refresh_in_both_auth_modes_with_identical_bodies() {
    for auth in [AnthropicAuth::ApiKey, AnthropicAuth::Bearer] {
        let helper = KeyHelper::rotating();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = command_profile(
            format!("http://{}/messages", listener.local_addr().unwrap()),
            auth,
            helper.source.clone(),
        );
        let server = tokio::spawn(async move {
            let mut requests = vec![];
            for status in ["401 Unauthorized", "200 OK"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(receive_http_request(&mut stream).await);
                send_http_response(&mut stream, status, "text/event-stream", answer()).await;
            }
            let header = if auth == AnthropicAuth::ApiKey {
                "x-api-key: "
            } else {
                "authorization: bearer "
            };
            assert!(
                requests[0]
                    .to_ascii_lowercase()
                    .contains(&format!("{header}key-1"))
            );
            assert!(
                requests[1]
                    .to_ascii_lowercase()
                    .contains(&format!("{header}key-2"))
            );
            let body = |request: &String| request.split_once("\r\n\r\n").unwrap().1.to_owned();
            assert_eq!(body(&requests[0]).as_bytes(), body(&requests[1]).as_bytes());
            assert!(!body(&requests[0]).contains("key-1"));
        });
        let mut provider = runtime(&profile);
        assert_eq!(helper.runs(), 0);
        let (sender, mut receiver) = session_event_channel(128);
        provider
            .complete(
                request(&Message::user("refresh")),
                ProgressReporter::new(sender),
            )
            .await
            .unwrap();
        assert_eq!(helper.runs(), 2);
        while let Ok(event) = receiver.try_recv() {
            assert!(!matches!(
                event,
                SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { .. })
            ));
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn command_count_tokens_refresh_excludes_helper_time_from_deadline() {
    let helper = KeyHelper::new("time.sleep(1.2); print('key-' + str(n))");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = command_profile(
        format!("http://{}/messages", listener.local_addr().unwrap()),
        AnthropicAuth::ApiKey,
        helper.source.clone(),
    );
    let ProfileProtocol::Anthropic { endpoint, .. } = &mut profile.protocol else {
        unreachable!()
    };
    endpoint.input_token_count.request_timeout_seconds = 1;
    let server = tokio::spawn(async move {
        let mut bodies = vec![];
        for (index, status) in ["401 Unauthorized", "200 OK"].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_request(&mut stream).await;
            assert!(request.starts_with("POST /messages/count_tokens "));
            assert!(request.contains(&format!("x-api-key: key-{}", index + 1)));
            bodies.push(request.split_once("\r\n\r\n").unwrap().1.to_owned());
            send_http_response(
                &mut stream,
                status,
                "application/json",
                "{\"input_tokens\":123}",
            )
            .await;
        }
        assert_eq!(bodies[0].as_bytes(), bodies[1].as_bytes());
    });
    let mut provider = runtime(&profile);
    assert_eq!(
        provider
            .count_input_tokens(request(&Message::user("count")))
            .await
            .unwrap(),
        InputTokenCount::Exact(123)
    );
    assert_eq!(helper.runs(), 2);
    server.await.unwrap();
}

#[tokio::test]
async fn command_helper_failure_is_terminal_before_any_anthropic_request() {
    for count in [false, true] {
        let helper = KeyHelper::new(
            "print('PRIVATE_STDOUT'); sys.stderr.write('refresh your login'); sys.exit(4)",
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = command_profile(
            format!("http://{}/messages", listener.local_addr().unwrap()),
            AnthropicAuth::Bearer,
            helper.source.clone(),
        );
        let mut provider = runtime(&profile);
        provider.recovery.max_attempts = 3;
        let error = if count {
            provider
                .count_input_tokens(request(&Message::user("count")))
                .await
                .unwrap_err()
        } else {
            provider
                .complete(request(&Message::user("hello")), discard_updates())
                .await
                .unwrap_err()
        };
        let error = format!("{error:#}");
        assert!(
            error.contains("providers.anthropic.api_key") && error.contains("refresh your login"),
            "{error}"
        );
        assert!(!error.contains("PRIVATE_STDOUT") && !error.contains("helper.py"));
        assert_eq!(helper.runs(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn persistent_401_is_terminal_after_one_command_refresh_even_with_retry_hint() {
    let helper = KeyHelper::rotating();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = command_profile(
        format!("http://{}/messages", listener.local_addr().unwrap()),
        AnthropicAuth::ApiKey,
        helper.source.clone(),
    );
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_request(&mut stream).await;
            stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nx-should-retry: true\r\nConnection: close\r\n\r\n{}").await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let mut provider = runtime(&profile);
    provider.recovery.max_attempts = 3;
    let error = provider
        .complete(request(&Message::user("hello")), discard_updates())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("providers.anthropic.api_key.command was rejected after a refresh")
    );
    assert_eq!(helper.runs(), 2);
    server.await.unwrap();
}
