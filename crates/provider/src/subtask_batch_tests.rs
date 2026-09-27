use super::*;

#[tokio::test]
async fn one_batch_call_preserves_arrays_strict_schema_replay_and_cache_prefix_on_http_and_websocket()
 {
    for websocket in [false, true] {
        for parallel in [None, Some(false)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let batches: Vec<_> = [2, 1].into_iter().enumerate().map(|(batch, count)| {
                let tasks: Vec<_> = (0..count).map(|index| json!({
                    "title":"Duplicate title", "prompt":format!("BATCH_{batch}_ENTRY_{index}"), "type":"explore", "workspace":null
                })).collect();
                function_call(&format!("fc-{batch}"), &format!("outer-{batch}"), "launch_subtasks", json!({"tasks":tasks}))
            }).collect();
            let outputs = batches.clone();
            let server = tokio::spawn(async move {
                let mut socket = if websocket {
                    Some(
                        accept_async(listener.accept().await.unwrap().0)
                            .await
                            .unwrap(),
                    )
                } else {
                    None
                };
                let mut requests = Vec::new();
                for (index, output) in outputs.into_iter().map(Some).chain([None]).enumerate() {
                    let response = if let Some(output) = output {
                        completed_output_event(&format!("resp-{index}"), vec![output])
                    } else {
                        completed_event("resp-final", "msg-final", "done")
                    };
                    if let Some(socket) = socket.as_mut() {
                        requests.push(receive_json(socket).await);
                        send_json(socket, response).await;
                    } else {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        requests.push(receive_http_json(&mut stream).await.body);
                        send_http_response(
                            &mut stream,
                            "200 OK",
                            "text/event-stream",
                            sse_data(response.to_string()),
                        )
                        .await;
                    }
                }
                requests
            });
            let root = tempfile::tempdir().unwrap();
            let (events, _) = session_event_channel(32);
            let channels = zevria_session_api::subtask_channels("root", events);
            let tools = ToolServer::new()
                .tool(zevria_tools::LaunchSubtasksTool::new(
                    channels.launcher,
                    root.path().to_path_buf(),
                ))
                .run();
            let params = parallel
                .map(|parallel| BTreeMap::from([("parallel_tool_calls".into(), json!(parallel))]))
                .unwrap_or_default();
            let mut openai = if websocket {
                let url = format!("ws://{address}/v1/responses");
                let (socket, _) = connect_async(&url).await.unwrap();
                let mut provider = test_session_with_url(&url, socket, None);
                provider.tools = tools;
                provider.additional_params = params;
                provider
            } else {
                connect_http_test_provider_with_options(
                    format!("http://{address}/v1/responses"),
                    tools,
                    ResponsesCompatibilityConfig::default(),
                    params,
                    "batch-cache",
                )
                .await
            };
            let mut state = AttemptState::default();
            for (index, native) in batches.iter().enumerate() {
                run_turn(
                    "delegate ready work",
                    &mut openai,
                    &mut state,
                    &discard_updates(),
                )
                .await
                .unwrap();
                let message = attempt_message(&state);
                let Message::Assistant { content, .. } = message else {
                    panic!("assistant call")
                };
                assert_eq!(content.len(), 1, "exactly one provider function call");
                let AssistantContent::ToolCall(call) = &content[0] else {
                    panic!("call")
                };
                assert_eq!(call.function.name, "launch_subtasks");
                assert_eq!(
                    call.function.arguments,
                    serde_json::from_str::<Value>(native["arguments"].as_str().unwrap()).unwrap()
                );
                assert_eq!(
                    state
                        .history_replays
                        .last()
                        .and_then(Option::as_ref)
                        .unwrap()
                        .replay()
                        .items,
                    vec![native.clone()]
                );
                state.history.push(Message::tool_result(
                    format!("outer-{index}"),
                    "launch_subtasks",
                    "all outcomes",
                ));
                state.history_replays.push(None);
            }
            // Reconstruct a complete native request after an ordinary continuation
            // so replay must carry BOTH full arrays, not presentation sidecars.
            openai.ws.continuation = None;
            run_turn("finish", &mut openai, &mut state, &discard_updates())
                .await
                .unwrap();
            let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            for request in &requests {
                assert_eq!(
                    request.get("parallel_tool_calls").cloned(),
                    parallel.map(|value| json!(value))
                );
                assert_eq!(request["instructions"], requests[0]["instructions"]);
                assert_eq!(
                    request["tools"], requests[0]["tools"],
                    "batch size/state cannot change ordered static definitions"
                );
                assert_eq!(request["prompt_cache_key"], requests[0]["prompt_cache_key"]);
                let tools = request["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0]["name"], "launch_subtasks");
                assert_eq!(tools[0]["strict"], true);
                let schema = &tools[0]["parameters"];
                assert_eq!(schema["required"], json!(["tasks"]));
                assert_eq!(schema["additionalProperties"], false);
                assert_eq!(schema["properties"]["tasks"]["minItems"], 1);
                let entry = &schema["properties"]["tasks"]["items"];
                assert_eq!(entry["additionalProperties"], false);
                assert!(
                    entry["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("workspace"))
                );
            }
            let input = requests[2]["input"].as_array().unwrap();
            let calls: Vec<_> = input
                .iter()
                .filter(|item| item["type"] == "function_call")
                .cloned()
                .collect();
            assert_eq!(calls, batches);
            let results: Vec<_> = input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .collect();
            assert_eq!(results.len(), 2, "one outer result per batch");
        }
    }
}
