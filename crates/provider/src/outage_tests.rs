//! Real transport outages and cache-prefix invariants, using only loopback IO.
use super::*;
use crate::connection::HttpFallback;
use std::time::Duration;
use zevria_model::ModelInputTooLarge;
use zevria_session_api::event::NetworkStatus;

fn parts() -> RequestParts {
    RequestParts {
        tools: zevria_model::ToolDeclarations::default(),
        forbid_tool_calls: false,
        prompt: Message::user("recover this request"),
        history: vec![],
        instructions: "Immutable cache prefix".into(),
        allowed_tool_names: None,
    }
}

fn retries(events: &mut SessionEventReceiver) -> Vec<(usize, Duration, Duration, String)> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|update| match update {
            SessionUpdate::Lifecycle(SessionEvent::TurnRetrying {
                attempt,
                retry_after,
                retrying_for,
                error,
                ..
            }) => Some((attempt, retry_after, retrying_for, error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn six_unavailable_attempts_then_success_replay_byte_identical_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut original = None;
        for index in 0..7 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = receive_http_request(&mut socket).await;
            if let Some(original) = &original {
                assert_eq!(&wire, original, "headers and body bytes are immutable");
            } else {
                original = Some(wire);
            }
            if index < 6 {
                send_http_response(
                    &mut socket,
                    "503 Service Unavailable",
                    "application/json",
                    "{}",
                )
                .await;
            } else {
                send_http_response(
                    &mut socket,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("recovered", "answer", "done").to_string()),
                )
                .await;
            }
        }
    });
    let mut provider = connect_http_test_provider(url).await;
    let (sender, mut events) = session_event_channel(128);
    let policy = RecoveryPolicy {
        backoff_cap: Duration::from_secs(1),
        ..fast_recovery_policy(30)
    };
    run_turn_request_with_recovery(
        parts().request(),
        &mut provider,
        &mut AttemptState::default(),
        &ProgressReporter::new(sender),
        &policy,
    )
    .await
    .unwrap();
    let retries = retries(&mut events);
    assert_eq!(
        retries.iter().map(|event| event.0).collect::<Vec<_>>(),
        vec![2, 3, 4, 5, 6, 7]
    );
    assert!(retries.windows(2).all(|pair| pair[0].1 <= pair[1].1));
    assert!(
        retries
            .iter()
            .all(|event| event.3.starts_with("server_error:"))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn slow_headers_succeed_after_deadline_escalation_and_warn_while_waiting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut workers = vec![];
        let mut original = None;
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = receive_http_request(&mut socket).await;
            if let Some(original) = &original {
                assert_eq!(original, &wire);
            } else {
                original = Some(wire);
            }
            workers.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let body = sse_data(completed_event("slow", "answer", "done").to_string());
                // Earlier clients legitimately timed out and closed their sockets.
                let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
    });
    let mut provider = connect_http_test_provider(url).await;
    let policy = RecoveryPolicy {
        send_timeout: Duration::from_millis(60),
        stall_warning: Duration::from_millis(35),
        response_idle_timeout: Duration::from_secs(1),
        ..fast_recovery_policy(5)
    };
    let (sender, mut events) = session_event_channel(128);
    run_turn_request_with_recovery(
        parts().request(),
        &mut provider,
        &mut AttemptState::default(),
        &ProgressReporter::new(sender),
        &policy,
    )
    .await
    .unwrap();
    let updates = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    let attempts = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { attempt, error, .. }) => {
                assert!(error.starts_with("request_timeout:"));
                Some(*attempt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts, vec![2, 3]);
    assert!(updates.iter().any(|u| matches!(
        u,
        SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
            attempt: 3,
            status: NetworkStatus::Quiet {
                will_retry: true,
                ..
            },
            ..
        })
    )));
    server.await.unwrap();
}

#[tokio::test]
async fn http_status_hints_quota_and_overrides_share_the_recovery_policy() {
    for (status, headers, body, should_retry) in [
        (
            "429 Too Many Requests",
            "Retry-After-Ms: 20\r\n",
            json!({"error":{"type":"rate_limit_exceeded"}}),
            true,
        ),
        ("408 Request Timeout", "", json!({}), true),
        (
            "503 Service Unavailable",
            "X-Should-Retry: false\r\n",
            json!({}),
            false,
        ),
        (
            "400 Bad Request",
            "X-Should-Retry: true\r\n",
            json!({}),
            true,
        ),
        (
            "429 Too Many Requests",
            "X-Should-Retry: true\r\n",
            json!({"error":{"code":"insufficient_quota"}}),
            false,
        ),
        (
            "429 Too Many Requests",
            "",
            json!({"error":{"type":"insufficient_quota"}}),
            false,
        ),
        (
            "503 Service Unavailable",
            "X-Should-Retry: true\r\n",
            json!({"error":{"code":"context_too_large"}}),
            false,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let original = receive_http_request(&mut socket).await;
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            if should_retry {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert_eq!(original, receive_http_request(&mut socket).await);
                send_http_response(
                    &mut socket,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("r", "m", "done").to_string()),
                )
                .await;
            } else {
                assert!(
                    tokio::time::timeout(Duration::from_millis(60), listener.accept())
                        .await
                        .is_err()
                );
            }
        });
        let mut provider = connect_http_test_provider(url).await;
        let (sender, mut events) = session_event_channel(64);
        let result = run_turn_request_with_recovery(
            parts().request(),
            &mut provider,
            &mut AttemptState::default(),
            &ProgressReporter::new(sender),
            &fast_recovery_policy(3),
        )
        .await;
        assert_eq!(result.is_ok(), should_retry, "{status}: {result:?}");
        let retries = retries(&mut events);
        assert_eq!(retries.len(), usize::from(should_retry));
        if headers.contains("Retry-After-Ms") {
            assert_eq!(retries[0].1, Duration::from_millis(20));
        }
        if !should_retry && headers.contains("X-Should-Retry: true") && status.starts_with("503") {
            assert!(result.unwrap_err().is::<ModelInputTooLarge>());
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn transient_sse_errors_and_failed_responses_replay_full_input() {
    let mut failed = completed_event("failed", "discard", "");
    failed["type"] = json!("response.failed");
    failed["response"]["status"] = json!("failed");
    failed["response"]["error"] = json!({"code":"server_error","message":"provider unavailable"});
    for event in [
        json!({"type":"error","error":{"code":"server_error","message":"unavailable"}}),
        json!({"type":"error","error":{"code":"rate_limit_exceeded","message":"slow down"}}),
        json!({"type":"error","error":{"code":"relay_error","message":"websocket: close 1006 (abnormal closure): unexpected EOF"}}),
        failed,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let original = receive_http_request(&mut socket).await;
            send_http_response(
                &mut socket,
                "200 OK",
                "text/event-stream",
                sse_data(event.to_string()),
            )
            .await;
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(original, receive_http_request(&mut socket).await);
            send_http_response(
                &mut socket,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("recovered", "m", "done").to_string()),
            )
            .await;
        });
        let mut provider = connect_http_test_provider(url).await;
        run_turn_request_with_recovery(
            parts().request(),
            &mut provider,
            &mut AttemptState::default(),
            &discard_updates(),
            &fast_recovery_policy(3),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn transient_websocket_errors_and_failed_responses_invalidate_continuation() {
    let mut failed = completed_event("failed", "discard", "");
    failed["type"] = json!("response.failed");
    failed["response"]["status"] = json!("failed");
    failed["response"]["error"] = json!({"code":"server_error","message":"provider unavailable"});
    for event in [
        failed,
        json!({"type":"error","error":{"code":"server_error","message":"unavailable"}}),
        json!({"type":"error","error":{"code":"rate_limit_exceeded","message":"slow down"}}),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let original = receive_json(&mut socket).await;
            send_json(&mut socket, event).await;
            let retried = receive_json(&mut socket).await;
            assert_eq!(original, retried);
            assert!(retried.get("previous_response_id").is_none());
            send_json(&mut socket, completed_event("recovered", "m", "done")).await;
        });
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut provider = test_session_with_url(&url, socket, None);
        run_turn_request_with_recovery(
            parts().request(),
            &mut provider,
            &mut AttemptState::default(),
            &discard_updates(),
            &fast_recovery_policy(3),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_connectivity_display_keeps_socket_cause_and_actionable_settings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/responses?secret=not-for-errors",
        listener.local_addr().unwrap()
    );
    drop(listener);
    let mut provider = connect_http_test_provider(url).await;
    provider.recovery = RecoveryPolicy {
        recovery_window: Duration::from_millis(25),
        ..fast_recovery_policy(30)
    };
    let error = provider
        .complete(parts().request(), discard_updates())
        .await
        .unwrap_err();
    let display = error.to_string();
    for expected in [
        "attempts in",
        "recovery stopped:",
        "recovery window",
        "[request_error]",
        "network or proxy",
        "recovery_window_seconds",
    ] {
        assert!(display.contains(expected), "{display}");
    }
    assert!(
        display.to_ascii_lowercase().contains("connection refused"),
        "{display}"
    );
    assert!(!display.contains("not-for-errors"));
    assert!(error.chain().any(|cause| cause.is::<reqwest::Error>()));
}

#[tokio::test]
async fn fourth_partial_generation_stops_even_with_a_large_attempt_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut original = None;
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = receive_http_request(&mut socket).await;
            if let Some(original) = &original {
                assert_eq!(original, &wire);
            } else {
                original = Some(wire);
            }
            let partial = sse_data(output_text_delta("billed partial response", 1).to_string());
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{partial}", partial.len()+1000).as_bytes()).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(60), listener.accept())
                .await
                .is_err()
        );
    });
    let mut provider = connect_http_test_provider(url).await;
    let error = run_turn_request_with_recovery(
        parts().request(),
        &mut provider,
        &mut AttemptState::default(),
        &discard_updates(),
        &fast_recovery_policy(30),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("after 4 attempts"));
    assert!(error.to_string().contains("4 interrupted responses"));
    server.await.unwrap();
}

#[tokio::test]
async fn provisional_fallback_reprobes_once_then_can_commit_sticky_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut original = None;
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let body = receive_json(&mut socket).await;
            if let Some(original) = &original {
                assert_eq!(original, &body);
            } else {
                original = Some(body);
            }
            socket.close(None).await.unwrap();
        }
        for status in ["503 Service Unavailable", "200 OK"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = receive_http_json(&mut socket).await.body;
            assert_eq!(body["input"], original.as_ref().unwrap()["input"]);
            let data = if status.starts_with("503") {
                "{}".into()
            } else {
                sse_data(completed_event("http", "m", "outage recovered").to_string())
            };
            send_http_response(&mut socket, status, "text/event-stream", data).await;
        }
        // The next request must upgrade WS, send *full* input, then fall back
        // after just this one failure. A second upgrade here fails the fixture.
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        let reprobe = receive_json(&mut socket).await;
        assert!(reprobe.get("previous_response_id").is_none());
        assert!(reprobe["input"].to_string().contains("outage recovered"));
        assert_eq!(
            reprobe["instructions"],
            original.as_ref().unwrap()["instructions"]
        );
        socket.close(None).await.unwrap();
        for id in ["committed", "sticky"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = receive_http_json(&mut socket).await.body;
            assert!(body.get("previous_response_id").is_none());
            send_http_response(
                &mut socket,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event(id, "m", "done").to_string()),
            )
            .await;
        }
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, None);
    provider.recovery = fast_recovery_policy(10);
    let mut state = AttemptState::default();
    recovery_followup("first", &mut provider, &mut state, &discard_updates())
        .await
        .unwrap();
    assert_eq!(provider.http_fallback, Some(HttpFallback::Provisional));
    let generation = provider.ws.socket_generation;
    recovery_followup("second", &mut provider, &mut state, &discard_updates())
        .await
        .unwrap();
    assert_eq!(provider.ws.socket_generation, generation + 1);
    assert!(matches!(
        provider.http_fallback,
        Some(HttpFallback::Sticky(_))
    ));
    recovery_followup("third", &mut provider, &mut state, &discard_updates())
        .await
        .unwrap();
    assert_eq!(provider.transport, OpenAiTransport::Http);
    server.await.unwrap();
}
