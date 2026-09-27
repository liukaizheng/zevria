//! Generated synthetic content only. Mock usage is not provider-cache evidence.
use super::*;

fn synthetic_output(turn: &Value, index: usize) -> Vec<Value> {
    let mut items = Vec::new();
    // Distinct items and opaque fields catch lossy rebuilding or reordering.
    for n in 0..turn["reasoning_items"].as_u64().unwrap() {
        items.push(json!({"type":"reasoning", "id":format!("rs_synthetic_{index}_{n}"), "summary":[], "encrypted_content":format!("PRIVATE_SYNTHETIC_OPAQUE_{index}_{n}"), "future":{"order":[n, null, true]}}));
    }
    for n in 0..turn["search_actions"].as_u64().unwrap() {
        let action = match n % 3 {
            0 => json!({"type":"search", "queries":["PRIVATE_SYNTHETIC_QUERY"]}),
            1 => json!({"type":"open_page", "url":"https://example.invalid/PRIVATE_SYNTHETIC_URL"}),
            _ => {
                json!({"type":"find_in_page", "url":"https://example.invalid/PRIVATE_SYNTHETIC_URL", "pattern":"PRIVATE_SYNTHETIC_PATTERN"})
            }
        };
        items.push(json!({"type":"web_search_call", "id":format!("ws_synthetic_{index}_{n}"), "status":"completed", "action":action, "future":{"opaque":"PRIVATE_SYNTHETIC_NATIVE"}}));
    }
    items.push(json!({"type":"message", "id":format!("msg_synthetic_{index}"), "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"Synthetic answer", "annotations":[]}]}));
    items
}

#[tokio::test]
async fn synthetic_seven_turn_partial_reuse_remains_unexplained_across_incremental_and_reconnect() {
    if isolate_log_test(
        "tests::cache_diagnostic_lifecycle::partial::synthetic_seven_turn_partial_reuse_remains_unexplained_across_incremental_and_reconnect",
    ) {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let fixture: Value = serde_json::from_str(include_str!("seven_turns.json")).unwrap();
        assert_eq!(fixture["synthetic"], true);
        let turns = fixture["turns"].as_array().unwrap().clone();
        let server_turns = turns.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut logical = Vec::new();
            let mut properties = None;
            for (index, turn) in server_turns.iter().enumerate() {
                if turn["reconnect"] == true {
                    let (stream, _) = listener.accept().await.unwrap();
                    socket = accept_async(stream).await.unwrap();
                }
                let wire = receive_json(&mut socket).await;
                let input = wire["input"].as_array().unwrap();
                if let Some(properties) = &properties { assert_eq!(&wire_request_properties(&wire), properties); }
                else { properties = Some(wire_request_properties(&wire)); }
                if matches!(index, 1 | 6) {
                    assert_eq!(wire["previous_response_id"], format!("resp_synthetic_{}", index - 1));
                    assert_eq!(input.len(), 1);
                    logical.extend(input.clone());
                } else {
                    assert!(wire.get("previous_response_id").is_none());
                    assert_eq!(&input[..logical.len()], logical.as_slice(), "exact native prefix including opaque fields");
                    logical = input.clone();
                }
                assert_eq!(logical.len(), turn["input_items"].as_u64().unwrap() as usize);
                let output = synthetic_output(turn, index);
                logical.extend(output.clone());
                let mut event = completed_output_event(&format!("resp_synthetic_{index}"), output);
                event["response"]["usage"] = json!({"input_tokens":turn["input_tokens"], "input_tokens_details":{"cached_tokens":turn["cached_tokens"], "cache_write_tokens":0}, "output_tokens":10, "output_tokens_details":{"reasoning_tokens":0}, "total_tokens":turn["input_tokens"].as_u64().unwrap()+10});
                send_json(&mut socket, event).await;
            }
        });
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut provider = test_session_with_url(&url, socket, None);
        provider.web_search = serde_json::from_value(json!({"enabled":true,"external_web_access":false})).unwrap();
        provider.responses_parameters = Some(json!({"reasoning":{"effort":"high", "summary":"detailed"}}));
        let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
        for (index, turn) in turns.iter().enumerate() {
            if turn["reconnect"] == true { provider.ws.reconnect().await.unwrap(); }
            history.push(zevria_model::OwnedModelRequestItem::message(Message::user(format!("Synthetic prompt {index}"))));
            let response = provider.complete(request(history.iter().map(zevria_model::OwnedModelRequestItem::as_borrowed).collect()), discard_updates()).await.unwrap();
            assert_eq!(response.record().provider_replay().unwrap().items, synthetic_output(turn, index));
            history.push(response.into_record().into_model_request_item());
        }
        server.await.unwrap();
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    assert!(!lines.join("\n").contains("PRIVATE_"));
    assert!(lines.iter().all(|l| l.len() < 3000), "bounded records");
    let summaries = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"comparison_summary\""))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 7);
    for (index, line) in summaries.iter().enumerate() {
        assert!(
            line.contains("reported_input_category=Some(Partial)"),
            "{line}"
        );
        assert!(line.contains("partial_reuse_unexplained=true"), "{line}");
        assert!(
            line.contains("provider_reported_miss_with_unchanged_local_prefix=false"),
            "{line}"
        );
        assert!(
            line.contains("usage_projection_difference=Some(false)"),
            "{line}"
        );
        if index > 0 {
            assert!(line.contains("prefix_status=\"exact_extension\""), "{line}");
            assert!(
                line.contains("request_properties_changed=Some(false)"),
                "{line}"
            );
            assert!(
                line.contains("unresolved_beyond_client_boundary=true"),
                "{line}"
            );
            assert!(
                line.contains(if matches!(index, 1 | 6) {
                    "connection_changed=Some(false)"
                } else {
                    "connection_changed=Some(true)"
                }),
                "{line}"
            );
        }
    }
    let native = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"native_context\""))
        .collect::<Vec<_>>();
    assert_eq!(native.len(), 7);
    assert!(native[6].contains("historical_native=NativeCounts { search: 13, open_page: 11, find_in_page: 9, other_search: 0, reasoning: 87 }"), "{}", native[6]);
    assert!(native[6].contains("current_native=NativeCounts { search: 0, open_page: 0, find_in_page: 0, other_search: 0, reasoning: 2 }"));
}

#[tokio::test]
async fn comparison_only_wire_mutation_is_still_a_wire_mismatch() {
    if isolate_log_test(
        "tests::cache_diagnostic_lifecycle::partial::comparison_only_wire_mutation_is_still_a_wire_mismatch",
    ) {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let mut provider = connect_http_test_provider(
            "http://127.0.0.1:9/responses".into(),
            ToolServer::new().run(),
        )
        .await;
        provider.additional_params.insert(
            "prompt_cache_options".into(),
            json!({"comparison_response_id":"resp_operator"}),
        );
        let prompt = Message::user("Synthetic prompt");
        let prepared = crate::turn::prepare_turn_request(
            &request(vec![ModelRequestItem::message(&prompt)]),
            &mut provider,
        )
        .await
        .unwrap();
        let prepared = dispatch(prepared, &mut provider);
        let mut body = prepared.request_properties.clone();
        body["stream"] = json!(true);
        body["input"] = json!(prepared.full_input);
        body["prompt_cache_options"]["comparison_response_id"] = json!("resp_wire_mutation");
        let bytes = serde_json::to_vec(&body).unwrap();
        crate::cache_diagnostics::transmission(
            &prepared,
            &mut provider,
            "full",
            1,
            None,
            Some(crate::cache_diagnostics::FullReason::Http),
            Some(&bytes),
        );
        use sha2::Digest;
        let exact_hash = crate::lowercase_hex(&sha2::Sha256::digest(&bytes));
        assert!(
            logs.contents()
                .contains(&format!("wire_hash=Some(\"{exact_hash}\")"))
        );
        body["prompt_cache_options"]["comparison_response_id"] = json!("resp_operator");
        let prepared_hash =
            crate::lowercase_hex(&sha2::Sha256::digest(serde_json::to_vec(&body).unwrap()));
        assert_ne!(
            prepared_hash, exact_hash,
            "wire hashing must retain the diagnostic ID"
        );
    }
    .with_subscriber(subscriber)
    .await;
    assert!(
        logs.contents()
            .contains("preparation_to_wire_consistent=Some(false)")
    );
}

#[tokio::test]
async fn upstream_comparison_explains_only_its_own_scope_not_all_reported_input() {
    if isolate_log_test(
        "tests::cache_diagnostic_lifecycle::partial::upstream_comparison_explains_only_its_own_scope_not_all_reported_input",
    ) {
        return;
    }
    let _lock = CAPTURED_LOG_TEST_LOCK.lock().await;
    let (logs, subscriber) = capture();
    async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (index, diagnostic) in [
                None,
                Some(json!({"type":"cache_hit"})),
                Some(json!({"type":"cache_miss", "reason":"input_changed", "comparison_reusable_tokens":83, "cache_missed_tokens":17})),
                Some(json!({"type":"unavailable"})),
            ].into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                assert_eq!(request.body["prompt_cache_options"]["comparison_response_id"], "resp_operator_baseline");
                let mut event = completed_event(&format!("resp_http_synthetic_{index}"), &format!("msg_http_synthetic_{index}"), "Synthetic answer");
                // Both HTTP terminal variants must be observed before projection.
                if index % 2 == 1 { event["type"] = json!("response.done"); }
                event["response"]["usage"] = json!({"input_tokens":100, "input_tokens_details":{"cached_tokens":25,"cache_write_tokens":0}, "output_tokens":10, "total_tokens":110});
                if let Some(diagnostic) = diagnostic { event["response"]["prompt_cache_diagnostics"] = diagnostic; }
                send_http_response(&mut stream, "200 OK", "text/event-stream", sse_data(event.to_string())).await;
            }
        });
        let mut provider = connect_http_test_provider(url, ToolServer::new().run()).await;
        provider.additional_params.insert("prompt_cache_options".into(), json!({"comparison_response_id":"resp_operator_baseline"}));
        let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
        for index in 0..4 {
            history.push(zevria_model::OwnedModelRequestItem::message(Message::user(format!("Synthetic prompt {index}"))));
            let response = provider.complete(request(history.iter().map(zevria_model::OwnedModelRequestItem::as_borrowed).collect()), discard_updates()).await.unwrap();
            history.push(response.into_record().into_model_request_item());
        }
        server.await.unwrap();
    }.with_subscriber(subscriber).await;
    let text = logs.contents();
    let lines = diagnostic_lines(&text);
    let summaries = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"comparison_summary\""))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 4);
    for (index, line) in summaries.iter().enumerate() {
        assert!(
            line.contains("reported_input_category=Some(Partial)"),
            "{line}"
        );
        assert!(line.contains("raw_cached=Some(Present(25))"), "{line}");
        assert!(
            line.contains(if matches!(index, 1 | 2) {
                "partial_reuse_unexplained=false"
            } else {
                "partial_reuse_unexplained=true"
            }),
            "{line}"
        );
    }
    assert!(summaries[2].contains("provider_reported_miss_with_unchanged_local_prefix=true"));
    assert!(summaries[3].contains("unresolved_beyond_client_boundary=true"));
    let comparisons = lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"provider_comparison\""))
        .collect::<Vec<_>>();
    assert!(comparisons[1].contains("outcome=CacheHit"));
    assert!(
        comparisons[2].contains("comparison_reusable_tokens=Present(83)")
            && comparisons[2].contains("cache_missed_tokens=Present(17)")
    );
    for line in lines
        .iter()
        .filter(|l| l.contains("diagnostic_event=\"reported_input\""))
    {
        assert!(
            line.contains("derived_uncached_input=Some(75)")
                && line.contains("cached_input_fraction=Some(0.25)"),
            "{line}"
        );
    }
}
