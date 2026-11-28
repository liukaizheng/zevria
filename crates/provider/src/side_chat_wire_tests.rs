//! Side Chats share the parent's Responses affinity, not its continuation.
use super::*;
use std::time::Duration;
use zevria_foundation::{RequestBehavior, RequestMetadata};
use zevria_instructions::RequestDirective;

#[tokio::test]
async fn side_chat_reuses_responses_affinity_on_a_separate_websocket() {
    const ROOT: &str = "parent-session";
    const SIDE: &str = "side-session";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = resolved_profile(
        "gateway",
        "gpt-test",
        format!("ws://{}/v1/responses", listener.local_addr().unwrap()),
        "test-key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    profile.responses_endpoint_mut().session_id_header = Some("X-Session-ID".into());
    let expected_key = crate::router::profile_cache_key(ROOT, &profile);
    let side_key = crate::router::profile_cache_key(SIDE, &profile);
    assert_ne!(expected_key, side_key);

    let server = async {
        let accept = || async {
            let (stream, _) = listener.accept().await.unwrap();
            accept_hdr_async(
                stream,
                |request: &ServerRequest, response: ServerResponse| {
                    assert_eq!(request.headers().get("x-session-id").unwrap(), ROOT);
                    Ok(response)
                },
            )
            .await
            .unwrap()
        };
        let mut parent = accept().await;
        let first = receive_json(&mut parent).await;
        assert_eq!(first["prompt_cache_key"], expected_key);
        assert!(first.get("previous_response_id").is_none());
        let answer = completed_event("resp_parent_n", "msg_parent_n", "parent answer");
        let mut prefix = first["input"].as_array().unwrap().clone();
        prefix.extend(
            answer["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        send_json(&mut parent, answer).await;

        // Accept a second socket while keeping the parent's socket open.
        let mut side = accept().await;
        let fork = receive_json(&mut side).await;
        assert_eq!(
            wire_request_properties(&fork),
            wire_request_properties(&first)
        );
        assert!(fork.get("previous_response_id").is_none());
        let input = fork["input"].as_array().unwrap();
        assert!(
            input.starts_with(&prefix),
            "the inherited wire prefix changed"
        );
        assert!(
            input[prefix.len()..]
                .iter()
                .any(|item| item.to_string().contains("side question"))
        );
        send_json(
            &mut side,
            completed_event("resp_side", "msg_side", "side answer"),
        )
        .await;

        let next = receive_json(&mut parent).await;
        assert_eq!(
            wire_request_properties(&next),
            wire_request_properties(&first)
        );
        assert_eq!(next["previous_response_id"], "resp_parent_n");
        let input = next["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "Turn N+1 should send only the delta");
        assert!(input[0].to_string().contains("parent follow-up"));
        send_json(
            &mut parent,
            completed_event("resp_parent_next", "msg_parent_next", "done"),
        )
        .await;
    };
    let client = async {
        let router = |id| {
            ProviderRouter::from_routes(
                [(ModelRole::Build, profile.clone(), ReasoningEffort::Medium)],
                "",
                id,
            )
            .unwrap()
        };
        let mut parent = router(ROOT);
        let initial = RequestParts {
            tools: zevria_model::ToolDeclarations::default(),
            forbid_tool_calls: false,
            prompt: Message::user("parent question"),
            history: Vec::new(),
            instructions: "Stable parent instructions".into(),
            allowed_tool_names: None,
        };
        let answer = parent
            .complete(initial.request(), discard_updates())
            .await
            .unwrap();
        let mut side = router(SIDE).with_prompt_cache_session_id(ROOT);
        let side_prompt = Message::user("side question");
        let boundary = RequestDirective::boundary(RequestMetadata::new(RequestBehavior::SideChat));
        let inherited = [
            ModelRequestItem::message(&initial.prompt),
            answer.record().model_request_item(),
        ];
        let mut fork = initial.request();
        fork.input = inherited
            .into_iter()
            .chain([
                ModelRequestItem::RequestInstruction(&boundary),
                ModelRequestItem::message(&side_prompt),
            ])
            .collect();
        side.complete(fork, discard_updates()).await.unwrap();

        let follow_up = Message::user("parent follow-up");
        let mut next = initial.request();
        next.input = inherited
            .into_iter()
            .chain([ModelRequestItem::message(&follow_up)])
            .collect();
        parent.complete(next, discard_updates()).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, client)
    })
    .await
    .expect("both WebSocket conversations should complete");
}
