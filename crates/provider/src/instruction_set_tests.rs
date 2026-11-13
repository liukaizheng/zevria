//! Permanent continuation tests for the request-owned instruction set.
use super::*;
use zevria_foundation::{ModelRole, TurnPolicy};
use zevria_instructions::{DirectiveContent, DirectivePolicy, InstructionSet, SkillSnapshot};
use zevria_model::ModelRequestItem;

#[tokio::test]
async fn same_tool_mode_switch_changes_request_properties_but_activation_is_incremental() {
    let mut provider =
        crate::tests::connect_http_test_provider("http://127.0.0.1:9/responses".into()).await;
    let mut set = InstructionSet {
        application: "Application".into(),
        system: vec![],
        catalog: None,
        workflow: DirectivePolicy::new(
            "build",
            &TurnPolicy::new("Build policy", None, ModelRole::Build, true),
        ),
    };
    let build = set.render();
    let prompt = Message::user("first");
    let first_request = ModelRequest {
        input_identity: None,
        tools: &[],
        forbid_tool_calls: false,
        instructions: &build,
        input: vec![ModelRequestItem::message(&prompt)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let first = prepare_turn_request(&first_request, &mut provider)
        .await
        .unwrap();
    let output = vec![
        serde_json::json!({"type":"message", "id":"reply", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"answer"}]}),
    ];
    let replay = zevria_model::ReplayMessage::new(ProviderReplay::openai_responses(
        provider.profile.clone(),
        output.clone(),
    ))
    .unwrap();
    let response = zevria_model::ModelResponse::from_replay(ProviderReplay::openai_responses(
        provider.profile.clone(),
        output,
    ))
    .unwrap();
    provider.ws.continuation = Some(
        ContinuationState::after_response(
            &first,
            provider.ws.socket_generation,
            "first-response".into(),
            &response,
        )
        .unwrap(),
    );
    let body = DirectiveContent::skill(
        &SkillSnapshot::new("review".parse().unwrap(), "Review", "Pinned body").unwrap(),
    );
    let next = Message::user("apply review");
    let input = vec![
        ModelRequestItem::message(&prompt),
        ModelRequestItem::replay_backed(&replay),
        ModelRequestItem::message(&next),
        ModelRequestItem::DeveloperInstruction(&body),
    ];
    let build_request = ModelRequest {
        input_identity: None,
        tools: &[],
        forbid_tool_calls: false,
        instructions: &build,
        input,
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let activated = prepare_turn_request(&build_request, &mut provider)
        .await
        .unwrap();
    assert_eq!(first.request_properties, activated.request_properties);
    let transmission = select_transmission(&activated, &mut provider);
    assert!(matches!(transmission.mode, RequestMode::Incremental));
    assert_eq!(
        transmission.previous_response_id.as_deref(),
        Some("first-response")
    );
    assert_eq!(transmission.input.len(), 2);
    assert_eq!(
        transmission.input.as_ptr(),
        activated.full_input[activated.full_input.len() - 2..].as_ptr(),
        "incremental transmission borrows the prepared suffix"
    );
    assert!(
        transmission.input[1]
            .to_string()
            .contains("Skill directive:")
    );

    set.workflow.scope = "plan".into();
    set.workflow.instructions = "Plan policy".into();
    let plan = set.render();
    let switched = prepare_turn_request(
        &ModelRequest {
            input_identity: None,
            instructions: &plan,
            ..build_request
        },
        &mut provider,
    )
    .await
    .unwrap();
    assert_eq!(activated.full_input, switched.full_input);
    let mut properties = activated.request_properties.clone();
    properties["instructions"] = plan.into();
    assert_eq!(
        properties, switched.request_properties,
        "instructions are the only request-property difference"
    );
    let transmission = select_transmission(&switched, &mut provider);
    assert!(matches!(transmission.mode, RequestMode::Full));
    assert!(transmission.previous_response_id.is_none());
    assert_eq!(transmission.input, switched.full_input.as_slice());
    assert_eq!(
        transmission.input.as_ptr(),
        switched.full_input.as_ptr(),
        "full transmission borrows the prepared input"
    );
    assert!(
        provider.ws.continuation.is_none(),
        "request_properties_changed clears the previous chain"
    );
}
