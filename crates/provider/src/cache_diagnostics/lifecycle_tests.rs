//! Removable integration tests. Permanent fixtures make no diagnostic assertions.
use super::*;
use crate::cache_diagnostics::{dispatch, inspect};

#[path = "partial_tests.rs"]
mod partial;

fn isolated(name: &str) -> bool {
    isolate_log_test(&format!("tests::cache_diagnostic_lifecycle::{name}"))
}

fn request(input: Vec<ModelRequestItem<'_>>) -> ModelRequest<'_> {
    ModelRequest {
        instructions: test_instructions(),
        input,
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    }
}
fn diagnostic_lines(logs: &str) -> Vec<&str> {
    logs.lines()
        .filter(|line| line.contains("diagnostic_event="))
        .collect()
}
fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_once(&format!("{name}="))
        .unwrap()
        .1
        .split_whitespace()
        .next()
        .unwrap()
}

fn capture() -> (CapturedLogs, tracing::Dispatch) {
    let logs = CapturedLogs::default();
    let subscriber = tracing::Dispatch::new(captured_log_subscriber(logs.clone()));
    (logs, subscriber)
}

#[tokio::test]
async fn instructions_change_only_for_same_tool_workflow_switch_not_activation() {
    if isolated("instructions_change_only_for_same_tool_workflow_switch_not_activation") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let mut provider = connect_http_test_provider("http://127.0.0.1:9/responses".into(), ToolServer::new().run()).await;
        let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
        let mut set = test_instruction_set("Application");
        let orchestrated = zevria_foundation::RequestMetadata::new(zevria_foundation::RequestBehavior::Orchestrate);
        for index in 0..5 {
            if index == 2 {
                history.push(zevria_model::OwnedModelRequestItem::RequestInstruction(zevria_instructions::RequestDirective::correction(orchestrated.clone())));
            } else {
                history.push(zevria_model::OwnedModelRequestItem::message(Message::user(format!("prompt {index}"))));
                let request = if index == 1 { orchestrated.clone() } else { zevria_foundation::RequestMetadata::new(zevria_foundation::RequestBehavior::Standard) };
                history.push(zevria_model::OwnedModelRequestItem::RequestInstruction(zevria_instructions::RequestDirective::boundary(request)));
            }
            if index == 3 {
                history.push(zevria_model::OwnedModelRequestItem::DeveloperInstruction(test_skill("Pinned body").clone()));
            }
            if index == 4 {
                set.workflow.scope = "plan".into();
                set.workflow.instructions = "Plan policy".into();
            }
            let instructions = set.render();
            let request = ModelRequest { instructions: &instructions, input: history.iter().map(zevria_model::OwnedModelRequestItem::as_borrowed).collect(), model_role: ModelRole::Build, allowed_tool_names: None };
            let prepared = crate::turn::prepare_turn_request(&request, &mut provider).await.unwrap();
            let prepared = dispatch(prepared, &mut provider);
            let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(provider.profile.clone(), vec![json!({"type":"message", "id":format!("reply-{index}"), "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"done"}]})])).unwrap();
            crate::cache_diagnostics::finish(&prepared, &mut provider, Some(&response));
            history.push(response.into_record().into_model_request_item());
        }
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let prepared = diagnostic_lines(&text)
        .into_iter()
        .filter(|line| line.contains("diagnostic_event=\"prepared\""))
        .collect::<Vec<_>>();
    assert_eq!(prepared.len(), 5, "{text}");
    for line in &prepared[1..4] {
        assert!(line.contains("instructions_changed=Some(false)"), "{line}");
        assert!(line.contains("prefix_status=\"exact_extension\""), "{line}");
    }
    assert!(
        prepared[4].contains("instructions_changed=Some(true)"),
        "{text}"
    );
    assert!(prepared[4].contains("tools_changed=Some(false)"));
    assert!(prepared[4].contains("remaining_properties_changed=Some(false)"));
}

#[tokio::test]
async fn reasoning_switch_changes_only_properties_with_exact_prefix_extension() {
    if isolated("reasoning_switch_changes_only_properties_with_exact_prefix_extension") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut key = Value::Null;
            for index in 0..3 {
                let request = receive_json(&mut socket).await;
                if index == 0 {
                    key = request["prompt_cache_key"].clone();
                }
                assert_eq!(request["prompt_cache_key"], key);
                assert_eq!(
                    request["reasoning"]["effort"],
                    if index == 0 { "medium" } else { "high" }
                );
                if index == 2 {
                    assert_eq!(request["previous_response_id"], "reason-1");
                } else {
                    assert!(request.get("previous_response_id").is_none());
                }
                send_json(
                    &mut socket,
                    completed_event(
                        &format!("reason-{index}"),
                        &format!("msg-{index}"),
                        "answer",
                    ),
                )
                .await;
            }
        });
        let profile = resolved_profile(
            "p",
            "m",
            url,
            "key",
            true,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            100_000,
        );
        let mut router = ResponsesRouter::from_routes(
            [(
                ModelRole::Build,
                profile,
                zevria_foundation::ReasoningLevel::Medium,
            )],
            "application",
            ToolServer::new().run(),
            "reasoning-root",
        )
        .unwrap();
        let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
        for index in 0..3 {
            if index == 1 {
                let mut selection = router.model_selection(ModelRole::Build).unwrap();
                selection.reasoning_level = ReasoningEffort::High;
                router
                    .prepare_model_update(ModelRole::Build, &selection)
                    .unwrap();
                router.install_model_update(ModelRole::Build, &selection);
            }
            history.push(zevria_model::OwnedModelRequestItem::message(Message::user(
                format!("question {index}"),
            )));
            let response = router
                .complete(
                    request(
                        history
                            .iter()
                            .map(zevria_model::OwnedModelRequestItem::as_borrowed)
                            .collect(),
                    ),
                    discard_updates(),
                )
                .await
                .unwrap();
            history.push(response.into_record().into_model_request_item());
        }
        server.await.unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let prepared = lines
        .iter()
        .filter(|line| line.contains("diagnostic_event=\"prepared\""))
        .collect::<Vec<_>>();
    assert_eq!(prepared.len(), 3, "{text}");
    for line in &prepared[1..] {
        assert!(line.contains("instructions_changed=Some(false)"), "{line}");
        assert!(line.contains("tools_changed=Some(false)"), "{line}");
        assert!(line.contains("prefix_status=\"exact_extension\""), "{line}");
    }
    assert!(prepared[1].contains("remaining_properties_changed=Some(true)"));
    assert!(prepared[2].contains("remaining_properties_changed=Some(false)"));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("request_properties_changed")
                && line.contains("request_mode=\"full\"")),
        "{text}"
    );
    let transmitted = lines
        .iter()
        .filter(|line| line.contains("diagnostic_event=\"transmission\""))
        .collect::<Vec<_>>();
    assert_eq!(transmitted.len(), 3, "{text}");
    for line in &transmitted[1..] {
        assert_eq!(
            field(transmitted[0], "transmitted_cache_key_fingerprint"),
            field(line, "transmitted_cache_key_fingerprint")
        );
    }
}

#[tokio::test]
async fn high_low_high_mock_usage_keeps_matching_prefix_after_reconnect() {
    if isolated("high_low_high_mock_usage_keeps_matching_prefix_after_reconnect") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    incident_prefix_flow(true, [121472, 121472, 3968, 122112])
        .with_subscriber(subscriber)
        .await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let prepared: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("diagnostic_event=\"prepared\""))
        .collect();
    assert_eq!(prepared.len(), 4, "{text}");
    assert!(prepared[0].contains("prefix_status=\"unknown\""));
    for line in &prepared[1..] {
        assert!(line.contains("prefix_status=\"exact_extension\""), "{line}");
        assert!(line.contains("instructions_changed=Some(false)"));
        assert!(line.contains("tools_changed=Some(false)"));
        assert!(line.contains("remaining_properties_changed=Some(false)"));
    }
    let low = lines
        .iter()
        .find(|line| line.contains("cached_tokens=Some(3968)"))
        .unwrap();
    assert_eq!(
        field(low, "local_request_id"),
        field(prepared[2], "local_request_id")
    );
    let transmission = lines
        .iter()
        .find(|line| {
            line.contains("diagnostic_event=\"transmission\"")
                && field(line, "local_request_id") == field(low, "local_request_id")
        })
        .unwrap();
    assert!(transmission.contains("request_mode=\"full\""));
    assert!(transmission.contains("socket_generation=1"));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("diagnostic_event=\"socket_recovery\"")
                && line.contains("observation_source=\"pump\""))
    );
    let diagnostics = lines.join("\n");
    for secret in [
        "SYNTHETIC_OPAQUE_REASONING",
        "first ordinary prompt",
        "second ordinary prompt",
        "final assistant response",
        "synthetic reasoning",
        "{  }",
        "test-session",
        "test-key",
        "counted",
    ] {
        assert!(
            !diagnostics.contains(secret),
            "leaked {secret}: {diagnostics}"
        );
    }
    assert!(lines.iter().all(|line| line.len() < 2400));
}

#[tokio::test]
async fn baseline_survives_reconnect_fallback_retry_and_maintenance_but_not_reset() {
    if isolated("baseline_survives_reconnect_fallback_retry_and_maintenance_but_not_reset") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            receive_json(&mut socket).await;
            send_json(&mut socket, completed_event("resp_initial", "msg_initial", "initial answer")).await;
            let (stream, _) = listener.accept().await.unwrap();
            let replacement = accept_async(stream).await.unwrap();
            drop((socket, replacement));
            for index in 0..6 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let captured = receive_http_json(&mut stream).await;
                if index < 2 {
                    assert_eq!(captured.body["prompt_cache_options"], json!({"mode":"implicit"}));
                } else {
                    assert_eq!(captured.body["prompt_cache_options"]["comparison_response_id"], "resp_operator_baseline");
                }
                match index {
                    0 => {
                        assert!(!captured.body.as_object().unwrap().contains_key("prompt_cache_key"));
                        send_http_response(&mut stream, "200 OK", "application/json", r#"{"input_tokens":42}"#).await;
                    }
                    1 => send_http_response(&mut stream, "200 OK", "application/json", r#"{"output":[{"type":"compaction","encrypted_content":"synthetic_compaction"}]}"#).await,
                    2 => send_http_response_with_raw_request_id(&mut stream, "500 Internal Server Error", "application/json", Some(b"req_retry"), "{}").await,
                    3 => send_http_response_with_raw_request_id(&mut stream, "200 OK", "text/event-stream", Some(b"req_success"), sse_data(completed_event_with_usage("resp_http", "msg_http", "http answer").to_string())).await,
                    4 => send_http_response(&mut stream, "200 OK", "text/event-stream", sse_data(completed_output_event("resp_invalid", vec![json!({"type":"UNSUPPORTED_NATIVE"})]).to_string())).await,
                    5 => send_http_response(&mut stream, "200 OK", "text/event-stream", sse_data(output_text_delta("partial never completed", 0).to_string())).await,
                    _ => unreachable!(),
                }
            }
        });
        let url = format!("ws://{address}/v1/responses");
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut provider = test_session_with_url(&url, socket, None);
        let directory = tempfile::tempdir().unwrap();
        let transcript_path = directory.path().join("session.jsonl");
        let context = crate::CacheDiagnosticContext::new(&transcript_path, "test-session");
        crate::cache_diagnostics::configure(&mut provider, &context);
        let saved = || {
            #[cfg(unix)]
            {
                let file = std::fs::read_dir(directory.path().join(".cache-diagnostics")).unwrap().next().unwrap().unwrap().path();
                std::fs::read(file).unwrap()
            }
            #[cfg(not(unix))]
            { Vec::<u8>::new() }
        };
        let prompt = Message::user("initial prompt");
        let first = provider.complete(request(vec![ModelRequestItem::message(&prompt)]), discard_updates()).await.unwrap();
        let before = inspect(&provider);
        let before_disk = saved();
        provider.additional_params.insert("prompt_cache_options".into(), json!({"comparison_response_id":"resp_operator_baseline", "mode":"implicit"}));
        assert!(before.0.is_some());
        assert_eq!(before.1, 2);
        provider.ws.reconnect().await.unwrap();
        assert_eq!(inspect(&provider), before);
        assert!(provider.ws.continuation.is_none());
        provider.switch_to_http("diagnostic_test");
        assert_eq!(inspect(&provider), before);
        let next = Message::user("next prompt");
        let replay = first.record().provider_replay().unwrap();
        let owned = zevria_model::OwnedModelRequestItem::replay_backed(replay.clone()).unwrap();
        let next_request = || request(vec![ModelRequestItem::message(&prompt), owned.as_borrowed(), ModelRequestItem::message(&next)]);
        let prepared = crate::turn::prepare_turn_request(&next_request(), &mut provider).await.unwrap();
        assert!(prepared.cache_diagnostics.is_none());
        assert_eq!(inspect(&provider), before);
        provider.input_token_count_url = Some(format!("http://{address}/input_tokens").parse().unwrap());
        assert_eq!(provider.count_input_tokens(next_request()).await.unwrap(), InputTokenCount::Exact(42));
        provider.compaction_url = Some(format!("http://{address}/compact").parse().unwrap());
        let no_tools = Vec::new();
        let maintenance = zevria_instructions::InstructionSet::maintenance("", []).render();
        let mut compact = next_request();
        compact.instructions = &maintenance;
        compact.allowed_tool_names = Some(&no_tools);
        assert!(matches!(provider.compact(compact).await.unwrap(), CompactResult::Replacement(_)));
        assert_eq!(inspect(&provider), before, "maintenance cannot dispatch or advance diagnostic state");
        assert_eq!(saved(), before_disk, "maintenance cannot replace persistent state");
        let policy = RecoveryPolicy { max_attempts: 2, backoff_base: std::time::Duration::ZERO, backoff_cap: std::time::Duration::ZERO, ..Default::default() };
        let mut state = AttemptState::default();
        run_turn_request_with_recovery(next_request(), &mut provider, &mut state, &discard_updates(), &policy).await.unwrap();
        let after = inspect(&provider);
        let after_disk = saved();
        #[cfg(unix)]
        assert_ne!(after_disk, before_disk);
        assert_ne!(after.0, before.0);
        assert_eq!(after.1, 4);
        let no_retry = RecoveryPolicy { max_attempts: 1, ..policy };
        for _ in 0..2 {
            assert!(run_turn_request_with_recovery(next_request(), &mut provider, &mut state, &discard_updates(), &no_retry).await.is_err());
            assert_eq!(inspect(&provider).0, after.0, "invalid/partial attempts cannot promote baseline");
        }
        provider.cancel();
        assert_eq!(saved(), after_disk, "failed/partial/cancelled attempts cannot replace disk baseline");
        assert_eq!(inspect(&provider).0, after.0);
        provider.reset();
        assert_eq!(inspect(&provider), (None, 0, 1, None));
        let prepared = crate::turn::prepare_turn_request(&next_request(), &mut provider).await.unwrap();
        let _ = dispatch(prepared, &mut provider);
        let mut other = connect_http_test_provider(format!("http://{address}/responses"), ToolServer::new().run()).await;
        crate::cache_diagnostics::configure(&mut other, &crate::CacheDiagnosticContext::new(&transcript_path, "test-session"));
        assert_eq!(inspect(&other), (None, 0, 0, None), "reset tombstone cannot be resurrected on a fresh provider");
        server.await.unwrap();
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let prepared: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"prepared\""))
        .collect();
    assert_eq!(prepared.len(), 5, "{text}");
    assert!(prepared[1].contains("prefix_status=\"exact_extension\""));
    let retries: Vec<_> = lines
        .iter()
        .filter(|l| {
            l.contains("diagnostic_event=\"transmission\"")
                && field(l, "local_request_id") == field(prepared[1], "local_request_id")
        })
        .collect();
    assert_eq!(
        retries.len(),
        2,
        "one snapshot must serve both HTTP attempts: {text}"
    );
    assert!(prepared[4].contains("missing_baseline_reason=\"engine_reset\""));
    assert!(prepared[4].contains("prefix_status=\"unknown\""));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("upstream_request_id=Some(\"req_success\")"))
    );
}

#[tokio::test]
async fn large_history_logs_are_bounded_and_profiles_do_not_share_baselines() {
    if isolated("large_history_logs_are_bounded_and_profiles_do_not_share_baselines") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let mut provider = connect_http_test_provider("http://127.0.0.1:9/responses".into(), ToolServer::new().run()).await;
        let messages = (0..1000).map(|_| Message::user("PRIVATE_PROMPT_ARGUMENT_CONTENT".repeat(40))).collect::<Vec<_>>();
        for count in [1, 1000] {
            let request = request(messages[..count].iter().map(ModelRequestItem::message).collect());
            let prepared = crate::turn::prepare_turn_request(&request, &mut provider).await.unwrap();
            let wire_before = serde_json::to_vec(&prepared.full_input).unwrap();
            let prepared = dispatch(prepared, &mut provider);
            assert_eq!(serde_json::to_vec(&prepared.full_input).unwrap(), wire_before, "diagnostic canonicalization cannot rewrite wire values");
            let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(provider.profile.clone(), vec![
                json!({"type":"message", "id":"msg_bounded", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"PRIVATE_OUTPUT"}]}),
            ])).unwrap();
            crate::cache_diagnostics::finish(&prepared, &mut provider, Some(&response));
        }
        let old = inspect(&provider);
        assert!(old.0.is_some());
        provider.profile = zevria_foundation::ModelProfileRef::new("another-profile", "another-model");
        let next = request(vec![ModelRequestItem::message(&messages[0])]);
        let prepared = crate::turn::prepare_turn_request(&next, &mut provider).await.unwrap();
        let _ = dispatch(prepared, &mut provider);
        assert_eq!(inspect(&provider).0, None);
        assert_eq!(inspect(&provider).2, old.2 + 1);
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|line| line.len() < 2400));
    assert!(!lines.join("\n").contains("PRIVATE_"));
    assert!(lines.iter().any(
        |line| line.contains("missing_baseline_reason=\"profile_changed\"")
            && line.contains("prefix_status=\"unknown\"")
    ));
}

#[tokio::test]
async fn preflight_observes_pump_failure_without_dequeuing_or_inventing_idle_death_time() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = accept_async(stream).await.unwrap();
        drop(socket); // read error, no close handshake
    });
    let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let mut provider = test_session(socket, None);
    provider.ws.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(742);
    let terminal = wait_for_terminal(&provider.ws.session).await;
    assert_eq!(terminal, OpenAiWebSocketTerminalCategory::ReadError);
    let observed = provider.ws.session.cache_diagnostics.observation().unwrap();
    assert_eq!(observed.category, terminal);
    assert!(observed.at.unwrap().elapsed() < provider.ws.idle_for());
    assert!(observed.error_class.is_some());
    provider
        .ws
        .session
        .terminate(OpenAiWebSocketTerminalCategory::LocalCancellation);
    assert_eq!(
        provider.ws.session.terminal_status(),
        Some(terminal),
        "observed failure precedence stays unchanged"
    );
    assert_eq!(
        provider
            .ws
            .session
            .cache_diagnostics
            .observation()
            .unwrap()
            .at,
        observed.at
    );
    assert!(matches!(
        provider.ws.session.next().await,
        Some(OpenAiWebSocketInbound::Error(_))
    ));
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn persisted_resume_matches_46_items_and_reports_upstream_zero_not_replay_failure() {
    if isolated("persisted_resume_matches_46_items_and_reports_upstream_zero_not_replay_failure") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    let requests = resume_wire::flow().with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let summaries = lines
        .iter()
        .filter(|line| line.contains("diagnostic_event=\"comparison_summary\""))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 4, "{text}");
    let resumed = summaries[3];
    for expected in [
        "baseline_source=\"persisted\"",
        "matched_item_count=46",
        "input_count=47",
        "previous_input_count=Some(44)",
        "previous_output_count=Some(2)",
        "prefix_status=\"exact_extension\"",
        "request_properties_changed=Some(false)",
        "routing_metadata_changed=Some(false)",
        "connection_changed=Some(true)",
        "request_mode=\"full\"",
        "full_replay_reason=\"fresh_runtime_no_continuation\"",
        "preparation_to_wire_consistent=Some(true)",
        "previous_cached_tokens=Some(14848)",
        "raw_cached=Some(Present(0))",
        "provider_reported_miss_with_unchanged_local_prefix=false",
        "reported_input_category=Some(ZeroCached)",
        "provider_comparison_conclusive=false",
        "unresolved_beyond_client_boundary=true",
        "wire_projection_mismatch=false",
        "local_input_changed=Some(false)",
    ] {
        assert!(resumed.contains(expected), "missing {expected}: {resumed}");
    }
    assert_ne!(
        field(summaries[2], "runtime_id"),
        field(resumed, "runtime_id")
    );
    assert!(resumed.contains(field(summaries[2], "runtime_id")));
    let transmissions = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"transmission\""))
        .collect::<Vec<_>>();
    assert_eq!(transmissions.len(), requests.len());
    for (line, payload) in transmissions.iter().zip(requests) {
        use sha2::Digest;
        assert!(
            line.contains(&crate::lowercase_hex(&sha2::Sha256::digest(
                payload.as_bytes()
            ))),
            "wire hash must cover exact server-captured text: {line}"
        );
        assert!(line.contains(&format!("wire_bytes=Some({})", payload.len())));
        assert!(line.contains("preparation_to_wire_consistent=Some(true)"));
    }
    assert!(lines.iter().all(|l| l.len() < 3000));
    assert!(!lines.join("\n").contains("PRIVATE_"));
    assert!(!lines.join("\n").contains("fixed-resume-session"));
}

#[tokio::test]
async fn late_done_has_prior_owner_and_missing_cached_counter_is_not_a_raw_zero() {
    if isolated("late_done_has_prior_owner_and_missing_cached_counter_is_not_a_raw_zero") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            receive_json(&mut socket).await;
            let mut completed =
                completed_event_with_usage("resp_first", "msg_first", "PRIVATE_ANSWER");
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"] = json!(14848);
            completed["response"]["prompt_cache_diagnostics"] = json!({"type":"cache_hit"});
            send_json(&mut socket, completed.clone()).await;
            receive_json(&mut socket).await;
            completed["type"] = json!("response.done");
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"] = json!(0);
            completed["response"]["prompt_cache_diagnostics"] =
                json!({"type":"cache_miss", "reason":"input_changed", "cache_missed_tokens":14848});
            send_json(&mut socket, completed).await;
            let mut done = completed_event_with_usage("resp_second", "msg_second", "PRIVATE_NEXT");
            done["type"] = json!("response.done");
            done["response"]["usage"]
                .as_object_mut()
                .unwrap()
                .remove("input_tokens_details");
            send_json(&mut socket, done).await;
        });
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut provider = test_session_with_url(&url, socket, None);
        let (events, mut updates) = session_event_channel(64);
        let progress = ProgressReporter::new(events);
        let first = Message::user("first");
        let response = provider
            .complete(
                request(vec![ModelRequestItem::message(&first)]),
                progress.clone(),
            )
            .await
            .unwrap();
        let replay = response.into_record().into_model_request_item();
        let next = Message::user("next");
        provider
            .complete(
                request(vec![
                    ModelRequestItem::message(&first),
                    replay.as_borrowed(),
                    ModelRequestItem::message(&next),
                ]),
                progress,
            )
            .await
            .unwrap();
        let mut usage_count = 0;
        while let Ok(update) = updates.try_recv() {
            if matches!(
                update,
                SessionUpdate::Lifecycle(SessionEvent::UsageUpdated { .. })
            ) {
                usage_count += 1;
            }
        }
        assert_eq!(usage_count, 2, "late done cannot publish usage twice");
        server.await.unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let prepared = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"prepared\""))
        .collect::<Vec<_>>();
    let raw = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"raw_terminal\""))
        .collect::<Vec<_>>();
    assert_eq!(raw.len(), 3, "{text}");
    assert!(
        raw[1].contains("duplicate_terminal=true")
            && raw[1].contains("duplicate_counters_differ=Some(true)"),
        "{text}"
    );
    assert_eq!(
        field(raw[1], "local_request_id"),
        format!("Some({})", field(prepared[0], "local_request_id"))
    );
    assert_eq!(
        field(raw[2], "local_request_id"),
        format!("Some({})", field(prepared[1], "local_request_id"))
    );
    assert!(raw[2].contains("raw_cached=Missing"));
    let comparisons = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"provider_comparison\""))
        .collect::<Vec<_>>();
    assert_eq!(comparisons.len(), 3);
    assert!(comparisons[0].contains("outcome=CacheHit"));
    assert!(
        comparisons[1].contains("outcome=CacheMiss")
            && comparisons[1].contains("duplicate_comparison_differs=Some(true)")
    );
    assert!(comparisons[2].contains("outcome=Absent"));
    let summaries = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"comparison_summary\""))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 2);
    assert!(summaries[0].contains("provider_comparison_conclusive=true"));
    assert!(summaries[1].contains("provider_comparison_conclusive=false"));
    assert!(summaries[1].contains("usage_projection_difference=Some(true)"));
    assert!(summaries[1].contains("provider_reported_miss_with_unchanged_local_prefix=false"));
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("baseline_promoted=true"))
            .count(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn http_wire_privacy_and_diagnostic_write_failures_do_not_change_completion() {
    if isolated("http_wire_privacy_and_diagnostic_write_failures_do_not_change_completion") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    let bodies = async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_request(&mut stream).await;
                assert!(request.contains("PRIVATE_CREDENTIAL"));
                assert!(request.contains("PRIVATE_URL_TOKEN"));
                assert!(request.contains("PRIVATE_SESSION_VALUE"));
                bodies.push(request.split_once("\r\n\r\n").unwrap().1.to_owned());
                let mut event = completed_event_with_usage(&format!("unsafe\nPRIVATE_RESPONSE_{index}"), &format!("msg_{index}"), "PRIVATE_OUTPUT");
                event["response"]["instructions"] = json!("PRIVATE_INSTRUCTIONS");
                event["response"]["tools"] = json!([]);
                event["response"]["prompt_cache_key"] = json!("PRIVATE_ECHO_CACHE_KEY");
                event["response"]["gateway"] = json!({"cookie":"PRIVATE_COOKIE", "authorization":"PRIVATE_AUTH", "backend":"PRIVATE_BACKEND"});
                event["response"]["model"] = json!("unsafe?PRIVATE_RETURNED_MODEL");
                event["response"]["prompt_cache_diagnostics"] = json!({"type":"cache_miss", "reason":"PRIVATE_UNKNOWN_REASON", "cache_missed_tokens":17, "comparison_reusable_tokens":null, "opaque":"PRIVATE_DIAGNOSTIC"});
                send_http_response_with_raw_request_id(&mut stream, "200 OK", "text/event-stream", Some(b"unsafe?PRIVATE_REQUEST_ID"), sse_data(event.to_string())).await;
            }
            bodies
        });
        let mut profile = resolved_profile("test-provider", "gpt-test", format!("http://PRIVATE_USER:PRIVATE_PASSWORD@{address}/responses?token=PRIVATE_URL_TOKEN#PRIVATE_FRAGMENT"), "PRIVATE_CREDENTIAL", false, ReasoningSummaryLevel::Detailed, ResponsesCompatibilityConfig::default(), BTreeMap::new(), RemoteCompactionConfig::default(), 100_000);
        profile.endpoint.session_id_header = Some("X-Session-ID".into());
        let mut provider = OpenAiProvider::connect(&profile, zevria_foundation::ReasoningLevel::Medium, "", ToolServer::new().run(), "PRIVATE_SESSION_VALUE", "PRIVATE_CACHE_KEY").await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let context = crate::CacheDiagnosticContext::new(&directory.path().join("session.jsonl"), "PRIVATE_SESSION_VALUE");
        crate::cache_diagnostics::configure(&mut provider, &context);
        let prompt = Message::user("PRIVATE_PROMPT");
        provider.complete(request(vec![ModelRequestItem::message(&prompt)]), discard_updates()).await.unwrap();
        let diagnostic_dir = directory.path().join(".cache-diagnostics");
        let file = std::fs::read_dir(&diagnostic_dir).unwrap().next().unwrap().unwrap().path();
        let persisted = std::fs::read_to_string(file).unwrap();
        assert!(!persisted.contains("PRIVATE_"), "{persisted}");
        std::fs::rename(&diagnostic_dir, directory.path().join("previous-snapshot")).unwrap();
        std::fs::write(&diagnostic_dir, "blocked storage").unwrap();
        let prior = inspect(&provider).0;
        provider.complete(request(vec![ModelRequestItem::message(&prompt)]), discard_updates()).await.unwrap();
        assert_ne!(inspect(&provider).0, prior, "an I/O failure cannot block a valid in-memory completion");
        server.await.unwrap()
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    assert!(!text.contains("PRIVATE_"), "{text}");
    let lines = diagnostic_lines(&text);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("persistence_error=Some(\"storage_not_directory_or_symlink\")"))
    );
    let transmissions = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"transmission\""))
        .collect::<Vec<_>>();
    assert_eq!(transmissions.len(), 2);
    for (line, body) in transmissions.into_iter().zip(bodies) {
        use sha2::Digest;
        assert!(line.contains(&crate::lowercase_hex(&sha2::Sha256::digest(
            body.as_bytes()
        ))));
        assert!(line.contains("preparation_to_wire_consistent=Some(true)"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn wire_and_routing_changes_are_explicit_and_reset_covers_lazy_profiles() {
    if isolated("wire_and_routing_changes_are_explicit_and_reset_covers_lazy_profiles") {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let directory = tempfile::tempdir().unwrap();
        let context = crate::CacheDiagnosticContext::new(&directory.path().join("session.jsonl"), "test-session");
        let mut profile = resolved_profile("test-provider", "gpt-test", "http://127.0.0.1:9/responses".into(), "test-key", false, ReasoningSummaryLevel::Detailed, ResponsesCompatibilityConfig::default(), BTreeMap::new(), RemoteCompactionConfig::default(), 100_000);
        profile.endpoint.session_id_header = Some("X-Session-ID".into());
        profile.endpoint.input_token_count.enabled = false;
        let tools = ToolServer::new().run();
        let mut provider = OpenAiProvider::connect(&profile, zevria_foundation::ReasoningLevel::Medium, "", tools.clone(), "test-session", "cache-key").await.unwrap();
        crate::cache_diagnostics::configure(&mut provider, &context);
        let prompt = Message::user("first prompt");
        let prepared = crate::turn::prepare_turn_request(&request(vec![ModelRequestItem::message(&prompt)]), &mut provider).await.unwrap();
        let prepared = dispatch(prepared, &mut provider);
        let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(provider.profile.clone(), vec![json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":"reply"}]})])).unwrap();
        crate::cache_diagnostics::finish(&prepared, &mut provider, Some(&response));
        drop(provider);
        // Load into a new diagnostic state before comparing a mutated wire body.
        let mut provider = OpenAiProvider::connect(&profile, zevria_foundation::ReasoningLevel::Medium, "", tools.clone(), "changed-routing-value", "cache-key").await.unwrap();
        crate::cache_diagnostics::configure(&mut provider, &context);
        assert!(provider.ws.continuation.is_none());
        let reply = response.into_record().into_model_request_item();
        let next = Message::user("ok");
        let prepared = crate::turn::prepare_turn_request(&request(vec![ModelRequestItem::message(&prompt), reply.as_borrowed(), ModelRequestItem::message(&next)]), &mut provider).await.unwrap();
        let prepared = dispatch(prepared, &mut provider);
        let mut body = prepared.request_properties.clone();
        body["stream"] = json!(true);
        body["input"] = json!(prepared.full_input);
        body["input"][0] = json!({"role":"user","content":"WIRE_ONLY_MUTATION"});
        crate::cache_diagnostics::transmission(&prepared, &mut provider, "full", 3, None, Some(crate::cache_diagnostics::FullReason::Http), Some(&serde_json::to_vec(&body).unwrap()));
        let mut event = completed_event_with_usage("resp_changed", "msg_changed", "reply");
        event["response"]["usage"]["input_tokens_details"]["cached_tokens"] = json!(0);
        crate::cache_diagnostics::observe_raw(&mut provider, &event.to_string(), None, None);
        crate::cache_diagnostics::terminal(&mut provider, OpenAiTransport::Http, "completed", Some("resp_changed"), None, Some(TokenUsage::default()), None);
        let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(provider.profile.clone(), event["response"]["output"].as_array().unwrap().clone())).unwrap();
        crate::cache_diagnostics::finish(&prepared, &mut provider, Some(&response));
        // A later faithful wire must not hide the previous known wire mismatch.
        let second_reply = response.into_record().into_model_request_item();
        let third_prompt = Message::user("after mismatch");
        let prepared = crate::turn::prepare_turn_request(&request(vec![ModelRequestItem::message(&prompt), reply.as_borrowed(), ModelRequestItem::message(&next), second_reply.as_borrowed(), ModelRequestItem::message(&third_prompt)]), &mut provider).await.unwrap();
        let prepared = dispatch(prepared, &mut provider);
        let mut body = prepared.request_properties.clone();
        body["stream"] = json!(true);
        body["input"] = json!(prepared.full_input);
        crate::cache_diagnostics::transmission(&prepared, &mut provider, "full", 5, None, Some(crate::cache_diagnostics::FullReason::Http), Some(&serde_json::to_vec(&body).unwrap()));
        event["response"]["id"] = json!("resp_following");
        crate::cache_diagnostics::observe_raw(&mut provider, &event.to_string(), None, None);
        let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(provider.profile.clone(), event["response"]["output"].as_array().unwrap().clone())).unwrap();
        crate::cache_diagnostics::finish(&prepared, &mut provider, Some(&response));
        let mut router = ResponsesRouter::from_routes([(ModelRole::Build, profile, zevria_foundation::ReasoningLevel::Medium)], "", tools, "test-session").unwrap().with_cache_diagnostics(context.clone());
        assert_eq!(router.initialized_profile_count(), 0);
        router.reset();
        assert_eq!(router.initialized_profile_count(), 0);
        let mut other = connect_http_test_provider("http://127.0.0.1:9/responses".into(), ToolServer::new().run()).await;
        crate::cache_diagnostics::configure(&mut other, &context);
        assert_eq!(inspect(&other).0, None);
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let summaries = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"comparison_summary\""))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 3);
    let summary = summaries[1];
    for expected in [
        "baseline_source=\"persisted\"",
        "prefix_status=\"exact_extension\"",
        "routing_metadata_changed=Some(true)",
        "wire_projection_mismatch=true",
        "provider_reported_miss_with_unchanged_local_prefix=false",
    ] {
        assert!(summary.contains(expected), "{summary}");
    }
    for expected in [
        "preparation_to_wire_consistent=Some(true)",
        "previous_preparation_to_wire_consistent=Some(false)",
        "wire_projection_mismatch=true",
        "provider_reported_miss_with_unchanged_local_prefix=false",
        "unresolved_beyond_client_boundary=false",
    ] {
        assert!(summaries[2].contains(expected), "{}", summaries[2]);
    }
    assert!(!lines.join("\n").contains("WIRE_ONLY_MUTATION"));
}
