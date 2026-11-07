use super::*;
use std::time::Duration;

#[tokio::test]
async fn retry_hints_for_unavailable_and_overloaded_use_upcoming_attempts_and_identical_wire() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut original = None;
        for (status, hint) in [
            ("503 Service Unavailable", "0.01"),
            ("529 Overloaded", "0.02"),
            ("200 OK", "0"),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let wire = receive_http_request(&mut stream).await;
            if let Some(original) = &original {
                assert_eq!(original, &wire);
            } else {
                original = Some(wire);
            }
            let body = if status == "200 OK" {
                answer()
            } else {
                json!({"error":{"type":"api_error","message":"unavailable"}}).to_string()
            };
            stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nRetry-After: {hint}\r\nRequest-Id: request_hint\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let mut provider = runtime(&profile(url));
    let (sender, mut events) = session_event_channel(64);
    provider
        .complete(
            request(&Message::user("retry unavailable")),
            ProgressReporter::new(sender),
        )
        .await
        .unwrap();
    let retries = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|update| match update {
            SessionUpdate::Lifecycle(SessionEvent::TurnRetrying {
                attempt,
                retry_after,
                retrying_for,
                error,
                ..
            }) => {
                assert!(error.contains("request_hint"));
                assert!(retrying_for < Duration::from_secs(300));
                Some((attempt, retry_after))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retries,
        vec![
            (2, Duration::from_millis(10)),
            (3, Duration::from_millis(20))
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn closed_port_exhaustion_has_the_shared_display_and_original_os_cause() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/messages?secret=hidden",
        listener.local_addr().unwrap()
    );
    drop(listener);
    let mut provider = runtime(&profile(url));
    provider.recovery.max_attempts = 2;
    let (sender, mut events) = session_event_channel(64);
    let error = provider
        .complete(
            request(&Message::user("offline")),
            ProgressReporter::new(sender),
        )
        .await
        .unwrap_err();
    let display = error.to_string();
    for expected in [
        "Model request failed after 2 attempts in",
        "absolute attempt cap",
        "[transport]",
        "network or proxy",
        "recovery_window_seconds",
    ] {
        assert!(display.contains(expected), "{display}");
    }
    assert!(
        display.to_ascii_lowercase().contains("connection refused"),
        "{display}"
    );
    assert!(!display.contains("secret=hidden"));
    assert!(error.chain().any(|cause| cause.is::<reqwest::Error>()));
    let retry = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|update| match update {
            SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { attempt, error, .. }) => {
                Some((attempt, error))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(retry.0, 2);
    assert!(retry.1.starts_with("transport:"));
    assert!(retry.1.to_ascii_lowercase().contains("connection refused"));
}
