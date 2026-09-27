//! Permanent opt-in request compatibility tests, run with the feature on/off.
use super::*;

#[tokio::test]
async fn explicit_comparison_survives_http_serialization_unchanged() {
    let options = json!({"comparison_response_id":"resp_synthetic_baseline", "mode":"implicit", "ttl":"30m", "future":{"retain":true}});
    let request = capture_http_request_with_options(
        ResponsesCompatibilityConfig::default(),
        BTreeMap::from([("prompt_cache_options".into(), options.clone())]),
        policy_tools(),
    )
    .await;
    assert_eq!(request.body["prompt_cache_options"], options);
    assert!(request.body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn comparison_id_changes_preserve_socket_continuation_but_cache_mode_does_not() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut first_input = Vec::new();
        let mut first_output = Vec::new();
        for index in 0..3 {
            let request = receive_json(&mut socket).await;
            assert_eq!(
                request["prompt_cache_options"]["comparison_response_id"],
                format!("resp_explicit_baseline_{index}")
            );
            assert_eq!(
                request["prompt_cache_options"]["mode"],
                if index == 2 { "explicit" } else { "implicit" }
            );
            assert_eq!(request["prompt_cache_options"]["ttl"], "30m");
            if index == 1 {
                assert_eq!(request["previous_response_id"], "resp_synthetic_0");
                assert_eq!(request["input"].as_array().unwrap().len(), 1);
            } else {
                assert!(request.get("previous_response_id").is_none());
            }
            let event = completed_event(
                &format!("resp_synthetic_{index}"),
                &format!("msg_synthetic_{index}"),
                "synthetic answer",
            );
            if index == 0 {
                first_input = request["input"].as_array().unwrap().clone();
                first_output = event["response"]["output"].as_array().unwrap().clone();
            }
            if index == 2 {
                let prefix: Vec<_> = first_input.iter().chain(&first_output).cloned().collect();
                assert_eq!(
                    &request["input"].as_array().unwrap()[..prefix.len()],
                    prefix.as_slice()
                );
            }
            send_json(&mut socket, event).await;
        }
    });
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, None);
    let mut history = Vec::<zevria_model::OwnedModelRequestItem>::new();
    for index in 0..3 {
        provider.additional_params.insert("prompt_cache_options".into(), json!({"comparison_response_id":format!("resp_explicit_baseline_{index}"), "mode":if index == 2 {"explicit"} else {"implicit"}, "ttl":"30m"}));
        history.push(zevria_model::OwnedModelRequestItem::message(Message::user(
            format!("synthetic prompt {index}"),
        )));
        let response = provider
            .complete(
                ModelRequest {
                    instructions: test_instructions(),
                    input: history
                        .iter()
                        .map(zevria_model::OwnedModelRequestItem::as_borrowed)
                        .collect(),
                    model_role: ModelRole::Build,
                    allowed_tool_names: None,
                },
                discard_updates(),
            )
            .await
            .unwrap();
        history.push(response.into_record().into_model_request_item());
    }
    server.await.unwrap();
}
