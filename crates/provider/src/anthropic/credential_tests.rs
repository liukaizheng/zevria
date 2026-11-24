use super::*;
use crate::credential::test_support::{KeyHelper, retry_events};
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

async fn assert_no_request(listener: &TcpListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "unexpected upstream request"
    );
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
        InputTokenCount::Approximate(123)
    );
    assert_eq!(helper.runs(), 2);
    server.await.unwrap();
}

#[tokio::test]
async fn transient_command_helper_failure_retries_before_sending_anthropic_request() {
    let helper = KeyHelper::new(
        "if n == 1:\n    sys.stderr.write('request timed out')\n    sys.exit(1)\nprint('key-2')",
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = command_profile(
        format!("http://{}/messages", listener.local_addr().unwrap()),
        AnthropicAuth::ApiKey,
        helper.source.clone(),
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_request(&mut stream).await;
        assert!(request.contains("x-api-key: key-2"));
        send_http_response(&mut stream, "200 OK", "text/event-stream", answer()).await;
        assert_no_request(&listener).await;
    });
    let mut provider = runtime(&profile);
    provider.recovery.max_attempts = 3;
    let (sender, mut receiver) = session_event_channel(128);
    provider
        .complete(
            request(&Message::user("hello")),
            ProgressReporter::new(sender),
        )
        .await
        .unwrap();
    let retries = retry_events(&mut receiver);
    let [(2, error)] = retries.as_slice() else {
        panic!("expected one retry, got {retries:?}");
    };
    assert!(
        error.starts_with("credential_helper: Anthropic credential helper failed: "),
        "{error}"
    );
    assert_eq!(
        error.matches("providers.anthropic.api_key").count(),
        1,
        "{error}"
    );
    assert!(error.contains("stderr: request timed out"), "{error}");
    assert_eq!(helper.runs(), 2);
    server.await.unwrap();
}

#[tokio::test]
async fn exhausted_command_helper_failure_keeps_the_diagnostic_in_display() {
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
    let (sender, mut receiver) = session_event_channel(128);
    let error = provider
        .complete(
            request(&Message::user("hello")),
            ProgressReporter::new(sender),
        )
        .await
        .unwrap_err()
        .to_string();
    for expected in [
        "after 3 attempts",
        "credential_helper",
        "providers.anthropic.api_key",
        "(exit status: 4)",
        "stderr: refresh your login",
        "The API key helper kept failing; check providers.anthropic.api_key.command",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert!(
        !error.contains("PRIVATE_STDOUT") && !error.contains("helper.py"),
        "{error}"
    );
    assert_eq!(retry_events(&mut receiver).len(), 2);
    assert_eq!(helper.runs(), 3);
    assert_no_request(&listener).await;
}

#[tokio::test]
async fn missing_helper_and_invalid_stdout_are_terminal_before_any_anthropic_request() {
    for (missing, reason, runs) in [
        (true, "could not start helper", 0),
        (
            false,
            "stdout must contain a non-empty single-line HTTP header value",
            1,
        ),
    ] {
        let helper = KeyHelper::new("print('PRIVATE_STDOUT\\nextra line')");
        let source = if missing {
            let mut command = helper.source.as_command().unwrap().clone();
            command.command = "./missing-credential-helper".into();
            ApiKeySource::command(command).with_config_directory(helper.directory.path())
        } else {
            helper.source.clone()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = command_profile(
            format!("http://{}/messages", listener.local_addr().unwrap()),
            AnthropicAuth::Bearer,
            source,
        );
        let mut provider = runtime(&profile);
        provider.recovery.max_attempts = 3;
        let (sender, mut receiver) = session_event_channel(128);
        let error = provider
            .complete(
                request(&Message::user("hello")),
                ProgressReporter::new(sender),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("providers.anthropic.api_key") && error.contains(reason),
            "{error}"
        );
        assert!(
            !error.contains("PRIVATE_STDOUT") && !error.contains("helper.py"),
            "{error}"
        );
        assert!(retry_events(&mut receiver).is_empty());
        assert_eq!(helper.runs(), runs);
        assert_no_request(&listener).await;
    }
}

#[tokio::test]
async fn count_tokens_helper_failure_is_single_shot_with_diagnostic() {
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
    let error = provider
        .count_input_tokens(request(&Message::user("count")))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("providers.anthropic.api_key")
            && error.contains("stderr: refresh your login"),
        "{error}"
    );
    assert!(
        !error.contains("PRIVATE_STDOUT") && !error.contains("helper.py"),
        "{error}"
    );
    assert_eq!(helper.runs(), 1);
    assert_no_request(&listener).await;
}

#[tokio::test]
async fn persistent_401_is_terminal_even_with_refresh_failure_or_retry_hint() {
    for interrupted in [false, true] {
        let helper = if interrupted {
            KeyHelper::fail_once_during_refresh()
        } else {
            KeyHelper::rotating()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile = command_profile(
            format!("http://{}/messages", listener.local_addr().unwrap()),
            AnthropicAuth::ApiKey,
            helper.source.clone(),
        );
        let runs = if interrupted { 3 } else { 2 };
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for run in [1, runs] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_request(&mut stream).await;
                let (headers, body) = request.split_once("\r\n\r\n").unwrap();
                assert!(headers.contains(&format!("x-api-key: key-{run}")));
                bodies.push(body.to_owned());
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nx-should-retry: true\r\nConnection: close\r\n\r\n{}").await.unwrap();
            }
            assert_eq!(bodies[0].as_bytes(), bodies[1].as_bytes());
            assert_no_request(&listener).await;
        });
        let mut provider = runtime(&profile);
        provider.recovery.max_attempts = 3;
        let (sender, mut receiver) = session_event_channel(128);
        let error = provider
            .complete(
                request(&Message::user("hello")),
                ProgressReporter::new(sender),
            )
            .await
            .expect_err("the replacement key's 401 must be terminal")
            .to_string();
        assert!(
            error.contains("providers.anthropic.api_key.command was rejected after a refresh"),
            "{error}"
        );
        let retries = retry_events(&mut receiver);
        if interrupted {
            let [(2, reason)] = retries.as_slice() else {
                panic!("expected one helper retry, got {retries:?}");
            };
            assert!(
                reason.starts_with("credential_helper: ")
                    && reason.contains("login renewal failed"),
                "{reason}"
            );
        } else {
            assert!(retries.is_empty(), "{retries:?}");
        }
        assert_eq!(helper.runs(), runs);
        server.await.unwrap();
    }
}
