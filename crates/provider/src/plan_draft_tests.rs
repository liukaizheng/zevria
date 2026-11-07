use super::*;
use zevria_foundation::{ToolContextRuntimeExt, TurnId};
use zevria_workflow::{
    PlanCandidate, PlanDraft, PlanId, PlanRecord, PlanWorkflowState, apply_plan_record,
};

#[tokio::test]
async fn real_plan_tools_keep_strict_schemas_and_cache_prefix_over_http_and_websocket() {
    for websocket in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let markdown = "# Durable iterative plan authoring\n\n## Goal\nOriginal.\n\n## Decisions\nRust.\n\n## Implementation\nBuild.\n\n## Validation\nTest.\n\n## Risks\nNone.";
        let calls = vec![
            function_call(
                "fc-create",
                "create",
                "create_plan",
                json!({"markdown":markdown,"replace":false}),
            ),
            function_call(
                "fc-update",
                "update",
                "update_plan",
                json!({"edits":[{"type":"replace_section","heading":"## Goal","content":"Updated."}]}),
            ),
            function_call("fc-read", "read", "read_plan", json!({})),
            function_call("fc-submit", "submit", "submit_plan", json!({})),
        ];
        let outputs = calls.clone();
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
                    completed_output_event(&format!("plan-{index}"), vec![output])
                } else {
                    completed_event("plan-final", "msg-final", "Submitted.")
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
        let tools = ToolServer::new()
            .tool(zevria_tools::CreatePlanTool::new(131_072))
            .tool(zevria_tools::UpdatePlanTool::new(131_072))
            .tool(zevria_tools::ReadPlanTool)
            .tool(zevria_tools::SubmitPlanTool::new(131_072))
            .run();
        let mut provider = if websocket {
            let url = format!("ws://{address}/v1/responses");
            let (socket, _) = connect_async(&url).await.unwrap();
            let mut provider = test_session_with_url(&url, socket, None);
            provider.tools = tools.clone();
            provider
        } else {
            connect_http_test_provider_with_options(
                format!("http://{address}/v1/responses"),
                tools.clone(),
                ResponsesCompatibilityConfig::default(),
                BTreeMap::new(),
                "plan-cache",
            )
            .await
        };
        let mut state = AttemptState::default();
        let mut planning = apply_plan_record(
            PlanWorkflowState::Idle,
            &PlanRecord::Started { id: PlanId::new() },
        )
        .unwrap();
        for native in &calls {
            run_turn(
                "continue planning",
                &mut provider,
                &mut state,
                &discard_updates(),
            )
            .await
            .unwrap();
            let message = attempt_message(&state);
            let Message::Assistant(assistant) = message else {
                panic!("assistant")
            };
            let AssistantContent::ToolCall(call) = &assistant.content[0] else {
                panic!("tool call")
            };
            assert_eq!(
                call.function.name.as_str(),
                native["name"].as_str().unwrap()
            );
            let mut context = ToolContext::new();
            context.insert_scope(planning.draft_scope(TurnId::new(1)).unwrap());
            let result = tools
                .snapshot()
                .get(call.function.name.as_str())
                .unwrap()
                .execute(call.function.raw_arguments(), &mut context)
                .await;
            assert!(result.is_success(), "{}", result.output().render());
            if let Some(draft) = context.result::<PlanDraft>().unwrap() {
                planning = apply_plan_record(planning, &PlanRecord::DraftSaved { draft }).unwrap();
            }
            if call.function.name == "submit_plan" {
                let candidate = context.result::<PlanCandidate>().unwrap().unwrap();
                assert_eq!(
                    candidate.markdown(),
                    planning.working_copy().unwrap().markdown()
                );
                assert!(candidate.markdown().contains("Updated."));
            }
            state.history.push(Message::tool_result(
                call.id.clone(),
                call.function.name.clone(),
                result.output().render(),
            ));
            state.history_replays.push(None);
        }
        run_turn("finish", &mut provider, &mut state, &discard_updates())
            .await
            .unwrap();
        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 5);
        for (index, request) in requests.iter().enumerate() {
            for key in ["instructions", "tools", "prompt_cache_key"] {
                assert_eq!(
                    request[key], requests[0][key],
                    "{key} changed during draft edits"
                );
            }
            let definitions = request["tools"].as_array().unwrap();
            assert_eq!(
                definitions
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["create_plan", "update_plan", "read_plan", "submit_plan"]
            );
            for definition in definitions {
                assert_eq!(definition["strict"], true);
                assert_eq!(definition["parameters"]["additionalProperties"], false);
            }
            assert_eq!(
                definitions[0]["parameters"]["required"],
                json!(["markdown", "replace"])
            );
            let edits = &definitions[1]["parameters"]["properties"]["edits"];
            assert_eq!(edits["minItems"], 1);
            assert_eq!(edits["maxItems"], 32);
            assert_eq!(edits["items"]["$ref"], "#/$defs/PlanEdit");
            let union = definitions[1]["parameters"]["$defs"]["PlanEdit"]["anyOf"]
                .as_array()
                .unwrap();
            assert_eq!(union.len(), 2);
            for variant in union {
                assert_eq!(variant["additionalProperties"], false);
            }
            assert_eq!(definitions[3]["parameters"]["properties"], json!({}));
            assert_eq!(definitions[3]["parameters"]["required"], json!([]));
            if websocket && index > 0 {
                assert!(
                    request.get("previous_response_id").is_some(),
                    "WebSocket continuation must remain active across draft edits"
                );
            }
            if index > 0 && request.get("previous_response_id").is_some() {
                assert_eq!(
                    request["previous_response_id"],
                    format!("plan-{}", index - 1)
                );
                let input = request["input"].as_array().unwrap();
                assert_eq!(
                    input.len(),
                    2,
                    "continuations send only the result and new prompt"
                );
                assert!(!input.iter().any(|item| item["type"] == "function_call"));
            }
        }
    }
}
