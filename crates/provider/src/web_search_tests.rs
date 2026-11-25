//! Loopback-only regression coverage of isolated search and root continuation.
use super::*;
use tokio_util::sync::CancellationToken;
use zevria_foundation::{ReasoningLevel, WebSearchDepth};
use zevria_session_api::{WebSearchExecutor, WebSearchProgress, WebSearchRequest};

pub(super) fn target(url: String) -> ResolvedModelProfile {
    let mut target = resolved_profile(
        "search-provider",
        "search-model",
        url,
        "test-key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        32_000,
    );
    target.reasoning_levels = vec![
        ReasoningLevel::None,
        ReasoningLevel::Low,
        ReasoningLevel::High,
    ];
    let ProfileProtocol::Responses { endpoint, .. } = &mut target.protocol else {
        unreachable!()
    };
    let endpoint = Arc::make_mut(endpoint);
    endpoint.session_id_header = Some("x-search-session".into());
    endpoint.network.max_attempts = 1;
    target
}

pub(super) fn executor(target: ResolvedModelProfile) -> OpenAiWebSearchExecutor {
    executor_with_options(
        target,
        ReasoningLevel::High,
        WebSearchConfig {
            max_tool_calls: Some(8),
            search_context_size: SearchContextSize::Medium,
            filters: Some(WebSearchFilters {
                allowed_domains: Some(vec!["example.com".into()]),
                blocked_domains: Some(vec!["stale.example.com".into()]),
            }),
            ..Default::default()
        },
    )
}

fn executor_with_options(
    target: ResolvedModelProfile,
    level: ReasoningLevel,
    options: WebSearchConfig,
) -> OpenAiWebSearchExecutor {
    OpenAiWebSearchExecutor::new(
        ResolvedWebSearchConfig::new(target, level, options).unwrap(),
        "test-session",
    )
    .unwrap()
}

fn request(depth: WebSearchDepth) -> WebSearchRequest {
    WebSearchRequest {
        query: "Search for a version".into(),
        context: Some("Use stable releases".into()),
        depth,
        allowed_domains: Some(vec!["docs.example.com".into()]),
    }
}

fn item(id: &str, status: &str, action: Value) -> Value {
    json!({"type":"web_search_call","id":id,"status":status,"action":action})
}

fn search_events() -> Vec<Value> {
    let query = item(
        "q",
        "completed",
        json!({"type":"search","queries":["version"]}),
    );
    let page = item(
        "p",
        "completed",
        json!({"type":"open_page","url":"https://docs.example.com/version"}),
    );
    let find = item(
        "f",
        "completed",
        json!({"type":"find_in_page","url":"https://docs.example.com/version","pattern":"Version"}),
    );
    let answer = json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[
        {"type":"output_text","text":"Version 2.","annotations":[
            {"type":"url_citation","start_index":0,"end_index":10,"title":"Version guide","url":"https://docs.example.com/version"},
            {"type":"url_citation","start_index":0,"end_index":10,"title":"Duplicate","url":"https://docs.example.com/version"}
        ]}
    ]});
    let mut terminal = completed_output_event(
        "search-response",
        vec![query.clone(), page.clone(), find.clone(), answer],
    );
    terminal["response"]["usage"] = json!({"input_tokens":6170,"input_tokens_details":{"cached_tokens":1024},"output_tokens":30,"total_tokens":6200});
    let step = |index, mut item: Value| {
        item["status"] = json!("in_progress");
        json!({"type":"response.output_item.added", "output_index":index, "item":item})
    };
    vec![
        json!({"type":"response.web_search_call.searching","item_id":"q","output_index":0}),
        json!({"type":"response.web_search_call.searching","item_id":"q","output_index":0}),
        json!({"type":"response.output_item.done","output_index":0,"item":query}),
        step(1, page),
        step(2, find),
        terminal,
    ]
}

pub(super) async fn send_search_response(socket: &mut TcpStream) {
    send_search_events(socket, &search_events()).await;
}

async fn send_search_events(socket: &mut TcpStream, events: &[Value]) {
    let events = events
        .iter()
        .map(|event| sse_data(event.to_string()))
        .collect::<String>();
    send_http_response(socket, "200 OK", "text/event-stream", events).await;
}

#[tokio::test]
async fn executor_uses_isolated_sse_and_reports_coalesced_actions_and_exact_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    // The target prefers WebSocket, but the executor always posts HTTP/SSE.
    let executor = executor(target(format!(
        "ws://{}/responses",
        listener.local_addr().unwrap()
    )));
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for (size, cap, effort) in [
            ("low", 3, "low"),
            ("medium", 8, "high"),
            ("high", 8, "high"),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let req = receive_http_json(&mut socket).await;
            let headers = req.headers.to_ascii_lowercase();
            assert!(headers.starts_with("post /responses "));
            assert!(headers.contains("accept: text/event-stream"));
            assert!(headers.contains("x-search-session: test-session"));
            let body = req.body;
            assert_eq!(body["model"], "search-model");
            assert_eq!(body["stream"], true);
            assert_eq!(body["store"], false);
            assert_eq!(body["parallel_tool_calls"], true);
            assert_eq!(body["tools"].as_array().unwrap().len(), 1);
            assert_eq!(body["tools"][0]["type"], "web_search");
            assert_eq!(body["tools"][0]["search_context_size"], size);
            assert_eq!(body["max_tool_calls"], cap);
            assert_eq!(body["reasoning"], json!({"effort":effort}));
            assert!(body.get("previous_response_id").is_none() && body.get("include").is_none());
            assert_eq!(
                body["tools"][0]["filters"],
                json!({"allowed_domains":["docs.example.com"],"blocked_domains":["stale.example.com"]})
            );
            assert_eq!(body["input"].as_array().unwrap().len(), 1);
            let text = body["input"][0]["content"][0]["text"].as_str().unwrap();
            let rest = text
                .strip_prefix("Question:\nSearch for a version\n\nContext:\nUse stable releases")
                .unwrap();
            if size == "low" {
                assert!(rest.contains("one to three focused searches"));
            } else {
                assert!(rest.is_empty());
            }
            assert!(body["instructions"].as_str().unwrap().len() < 900);
            bodies.push(body);
            send_search_response(&mut socket).await;
        }
        assert!(
            bodies
                .windows(2)
                .all(|pair| pair[0]["instructions"] == pair[1]["instructions"]
                    && pair[0]["prompt_cache_key"] == pair[1]["prompt_cache_key"])
        );
    });
    for depth in [
        WebSearchDepth::Quick,
        WebSearchDepth::Standard,
        WebSearchDepth::Deep,
    ] {
        let (sender, mut receiver) = session_event_channel(16);
        let turn_id = zevria_foundation::TurnId::new(17);
        let progress = WebSearchProgress::new(turn_id, "call-search".into(), sender);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            executor.search(request(depth), Some(progress), CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            out.answer
                .contains("[Version guide](https://docs.example.com/version)"),
            "{}",
            out.answer
        );
        assert_eq!(out.detail.depth, depth);
        assert_eq!(out.detail.sources.len(), 1);
        assert_eq!(out.detail.input_tokens, Some(6170));
        assert_eq!(out.detail.cached_tokens, Some(1024));
        assert_eq!(out.detail.output_tokens, Some(30));
        assert_eq!(out.detail.total_tokens, Some(6200));
        assert_eq!(
            out.detail
                .actions
                .iter()
                .map(|a| a.kind.as_str())
                .collect::<Vec<_>>(),
            ["search", "open_page", "find_in_page"]
        );
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            let SessionUpdate::Lifecycle(SessionEvent::Turn(
                zevria_session_api::TurnEvent::WebSearchProgress {
                    turn_id: turn,
                    call_id,
                    activity,
                },
            )) = update
            else {
                panic!("unexpected update: {update:?}");
            };
            assert_eq!(turn, turn_id);
            assert_eq!(call_id, "call-search");
            updates.push(activity);
        }
        assert_eq!(
            updates.len(),
            5,
            "duplicate searching frames do not repeat the snapshot"
        );
        assert_eq!(updates[2].last().unwrap().action_kind(), Some("open_page"));
        assert_eq!(
            updates[3].last().unwrap().action_kind(),
            Some("find_in_page")
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn default_quick_omits_unsupported_tool_cap_and_keeps_resolved_gateway_parameters() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = service_tiers::tier_profile(
        format!("http://{}/responses", listener.local_addr().unwrap()),
        false,
        Some("fast"),
        ModelServiceTier::Set(ServiceTier::Default),
    );
    let ProfileProtocol::Responses { endpoint, .. } = &mut profile.protocol else {
        unreachable!()
    };
    Arc::make_mut(endpoint).additional_params.extend([
        ("tool_choice".into(), json!("none")),
        ("parallel_tool_calls".into(), json!(false)),
        ("max_tool_calls".into(), json!(8)),
        ("conversation".into(), json!("conv-root")),
        ("prompt".into(), json!({"id":"pmpt_root"})),
        ("max_output_tokens".into(), json!(4000)),
    ]);
    let executor =
        executor_with_options(profile, ReasoningLevel::Medium, WebSearchConfig::default());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let body = receive_http_json(&mut socket).await.body;
        assert_eq!(body["service_tier"], "default", "model override wins");
        assert_eq!(body["gateway_routing"], "unchanged");
        assert_eq!(body["max_output_tokens"], 4000);
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["tools"][0]["search_context_size"], "low");
        for key in [
            "max_tool_calls",
            "conversation",
            "prompt",
            "previous_response_id",
        ] {
            assert!(body.get(key).is_none(), "unexpected {key}: {body}");
        }
        send_search_response(&mut socket).await;
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        executor.search(
            request(WebSearchDepth::Quick),
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn incomplete_search_keeps_readable_cited_output_but_rejects_missing_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let executor = executor(target(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    )));
    let server = tokio::spawn(async move {
        // Exercise terminal output and a gateway's response.done without an
        // output array (where the completed items are the available ledger).
        let mut terminal = search_events().pop().unwrap();
        terminal["type"] = json!("response.incomplete");
        terminal["response"]["status"] = json!("incomplete");
        terminal["response"]["incomplete_details"] = json!({"reason":"max_output_tokens"});
        terminal["response"]["output"][3]["status"] = json!("incomplete");
        terminal["response"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("total_tokens");
        let mut done = terminal.clone();
        done["type"] = json!("response.done");
        let output = done["response"]
            .as_object_mut()
            .unwrap()
            .remove("output")
            .unwrap();
        let mut streamed: Vec<_> = output.as_array().unwrap().iter().enumerate().map(|(index, item)| {
            json!({"type":"response.output_item.done", "output_index":index, "item":item})
        }).collect();
        streamed.push(done);
        let mut empty = terminal.clone();
        empty["response"]["output"][3]["content"] = json!([
            {"type":"output_text", "text":"  ", "annotations":[]}
        ]);
        for events in [vec![terminal], streamed, vec![empty]] {
            let (mut socket, _) = listener.accept().await.unwrap();
            receive_http_json(&mut socket).await;
            send_search_events(&mut socket, &events).await;
        }
    });
    let search = || {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            executor.search(
                request(WebSearchDepth::Quick),
                None,
                CancellationToken::new(),
            ),
        )
    };
    for _ in 0..2 {
        let result = search().await.unwrap().unwrap();
        assert!(
            result
                .answer
                .contains("[Version guide](https://docs.example.com/version)")
        );
        assert!(
            result
                .answer
                .ends_with("\n\n[Web search response ended early: max_output_tokens.]")
        );
        assert_eq!(result.detail.sources.len(), 1);
        assert_eq!(result.detail.total_tokens, Some(6200));
    }
    let failure = search().await.unwrap().unwrap_err();
    assert!(
        failure.to_string().contains("max_output_tokens"),
        "{failure:#}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn shared_executor_bounds_concurrency_and_cancels_queued_searches() {
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let executor = Arc::new(executor(target(format!(
            "http://{}/responses",
            listener.local_addr().unwrap()
        ))));
        let start = || {
            let executor = executor.clone();
            tokio::spawn(async move {
                executor
                    .search(
                        request(WebSearchDepth::Quick),
                        None,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            })
        };
        let running = (0..6).map(|_| start()).collect::<Vec<_>>();
        let mut streams = Vec::new();
        for _ in 0..6 {
            let (mut socket, _) = listener.accept().await.unwrap();
            receive_http_json(&mut socket).await;
            streams.push(socket);
        }

        let cancellation = CancellationToken::new();
        let mut queued =
            executor.search(request(WebSearchDepth::Quick), None, cancellation.clone());
        tokio::select! {
            _ = &mut queued => panic!("a queued search must wait for capacity"),
            socket = listener.accept() => panic!("search exceeded shared capacity: {socket:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        cancellation.cancel();
        assert!(queued.await.unwrap_err().to_string().contains("cancelled"));

        let next = start();
        send_search_response(&mut streams.pop().unwrap()).await;
        let (mut next_socket, _) = listener.accept().await.unwrap();
        receive_http_json(&mut next_socket).await;
        send_search_response(&mut next_socket).await;
        next.await.unwrap();
        for mut socket in streams {
            send_search_response(&mut socket).await;
        }
        for search in running {
            search.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_interrupts_a_live_search_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let executor = executor(target(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    )));
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        receive_http_json(&mut socket).await;
        let first = sse_data(
            json!({"type":"response.web_search_call.searching","item_id":"q","output_index":0})
                .to_string(),
        );
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{first}", first.len() + 10_000).as_bytes()).await.unwrap();
        // The incomplete body stays pending until the client drops the stream.
        let mut bytes = [0; 16];
        let close = socket.read(&mut bytes).await;
        assert!(close.as_ref().is_ok_and(|&n| n == 0) || close.is_err());
    });
    let (sender, mut receiver) = session_event_channel(8);
    let token = CancellationToken::new();
    let progress =
        WebSearchProgress::new(zevria_foundation::TurnId::new(1), "cancel".into(), sender);
    let search = executor.search(
        request(WebSearchDepth::Quick),
        Some(progress),
        token.clone(),
    );
    tokio::pin!(search);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            _ = &mut search => panic!("search ended before cancellation"),
            event = receiver.recv() => assert!(matches!(event, Some(SessionUpdate::Lifecycle(SessionEvent::Turn(zevria_session_api::TurnEvent::WebSearchProgress {..}))))),
        }
        token.cancel();
        assert!(search.await.unwrap_err().to_string().contains("cancelled"));
        server.await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn credential_refresh_reuses_same_isolated_body_and_shares_source_cache() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = target(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let helper = crate::credential::test_support::KeyHelper::rotating();
    let ProfileProtocol::Responses { endpoint, .. } = &mut profile.protocol else {
        unreachable!()
    };
    Arc::make_mut(endpoint).api_key = helper.source.clone();
    let executor = executor(profile);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let first = receive_http_json(&mut socket).await;
        assert!(
            first
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer key-1")
        );
        send_http_response(&mut socket, "401 Unauthorized", "application/json", "{}").await;
        let (mut socket, _) = listener.accept().await.unwrap();
        let refreshed = receive_http_json(&mut socket).await;
        assert!(
            refreshed
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer key-2")
        );
        assert_eq!(first.body, refreshed.body);
        send_search_response(&mut socket).await;
    });
    assert_eq!(
        helper
            .source
            .current("search-provider")
            .await
            .unwrap()
            .expose(),
        "key-1"
    );
    executor
        .search(
            request(WebSearchDepth::Quick),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        helper
            .source
            .current("search-provider")
            .await
            .unwrap()
            .expose(),
        "key-2"
    );
    assert_eq!(helper.runs(), 2);
}

#[tokio::test]
async fn root_continues_same_socket_after_isolated_search_with_only_local_result_in_input() {
    let root = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let search = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root_url = format!("ws://{}/responses", root.local_addr().unwrap());
    let search = (
        format!("http://{}/responses", search.local_addr().unwrap()),
        search,
    );
    let executor = Arc::new(executor_with_options(
        target(search.0),
        ReasoningLevel::High,
        WebSearchConfig::default(),
    ));
    let search_server = tokio::spawn(async move {
        let (mut socket, _) = search.1.accept().await.unwrap();
        let request = receive_http_json(&mut socket).await;
        assert_eq!(request.body["tools"][0]["type"], "web_search");
        assert!(request.body.get("max_tool_calls").is_none());
        send_search_response(&mut socket).await;
        request.body["prompt_cache_key"].clone()
    });
    let root_server = tokio::spawn(async move {
        let (socket, _) = root.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        let first = receive_json(&mut socket).await;
        assert_eq!(first["tools"][0]["type"], "function");
        assert_eq!(first["tools"][0]["name"], "web_search");
        assert!(first.get("previous_response_id").is_none());
        let call = json!({"type":"function_call","id":"search-item","call_id":"call-search","status":"completed","name":"web_search","arguments":"{\"query\":\"Search for a version\"}"});
        send_json(&mut socket, completed_output_event("root-1", vec![call])).await;
        let second = receive_json(&mut socket).await;
        for key in ["tools", "instructions", "prompt_cache_key"] {
            assert_eq!(
                serde_json::to_vec(&first[key]).unwrap(),
                serde_json::to_vec(&second[key]).unwrap(),
                "{key}"
            );
        }
        assert_eq!(second["previous_response_id"], "root-1");
        let input = second["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "{input:?}");
        assert_eq!(input[0]["type"], "function_call_output");
        assert!(
            input[0]["output"]
                .to_string()
                .contains("docs.example.com/version")
        );
        assert!(!input[0]["output"].to_string().contains("6200"));
        send_json(&mut socket, completed_output_event("root-2", vec![json!({"type":"message","id":"final","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Root answer","annotations":[]}]})])).await;
        first["prompt_cache_key"].clone()
    });
    let (socket, _) = connect_async(&root_url).await.unwrap();
    let mut provider = test_session_with_url(&root_url, socket, None);
    provider.input_token_count_url = None;
    let tools = ToolServer::new()
        .tool(zevria_tools::WebSearchTool::new(executor))
        .run();
    let directory = tempfile::tempdir().unwrap();
    let transcript = TranscriptWriter::create(directory.path()).unwrap();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, _receiver) = session_event_channel(256);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "Research the version".into(),
                mode: SessionMode::Build,
            }),
            &events,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(root_server.await.unwrap(), search_server.await.unwrap());
    let items = transcript::load(&path).unwrap();
    assert!(
        items
            .iter()
            .filter_map(TranscriptItem::provider_replay)
            .flat_map(|replay| &replay.items)
            .all(|item| item["type"] != "web_search_call" && item["type"] != "reasoning")
    );
    let details = items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolResults { metadata, .. } => metadata[0].web_search(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].total_tokens, Some(6200));
}
