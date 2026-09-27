use super::*;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use zevria_foundation::{TOOL_RESULT_META_KEY, ToolCallOutcome, ToolResultMetadata};

#[test]
fn typed_result_metadata_is_correlated_optional_and_separate_from_raw_output() {
    let metadata = ToolResultMetadata {
        id: "call".into(),
        call_id: None,
        tool_name: "command".into(),
        outcome: ToolCallOutcome::Cancelled,
        diagnostic: Some("display diagnostic".into()),
        detail: None,
    };
    for valid in [true, false] {
        let mut wire = serde_json::to_value(&metadata).unwrap();
        if !valid {
            wire["id"] = serde_json::json!("foreign");
        }
        let update = ToolCallUpdate::new(
            "call",
            ToolCallUpdateFields::new().raw_output(serde_json::json!("original bytes")),
        )
        .meta(serde_json::Map::from_iter([(
            TOOL_RESULT_META_KEY.into(),
            wire,
        )]));
        let events = normalize_update(AcpSessionUpdate::ToolCallUpdate(update));
        assert!(
            matches!(&events[0], AgentRunEvent::ToolCallUpdate { raw_output: Some(value), .. } if value == "original bytes")
        );
        assert_eq!(events.len(), if valid { 2 } else { 1 });
        if valid {
            assert!(
                matches!(&events[1], AgentRunEvent::ToolResultMetadata { metadata: actual } if **actual == metadata)
            );
            let restored: Vec<AgentRunEvent> =
                serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
            assert_eq!(restored, events);
        }
    }
}

#[test]
fn malformed_tool_sidecars_do_not_guess_an_outcome_or_replace_standard_updates() {
    for wire in [
        serde_json::Value::Null,
        serde_json::json!({"id":"call", "outcome":"invented"}),
    ] {
        let update = ToolCallUpdate::new("call", ToolCallUpdateFields::new()).meta(
            serde_json::Map::from_iter([(TOOL_RESULT_META_KEY.into(), wire)]),
        );
        assert!(matches!(
            normalize_update(AcpSessionUpdate::ToolCallUpdate(update)).as_slice(),
            [AgentRunEvent::ToolCallUpdate { .. }]
        ));
    }
}
