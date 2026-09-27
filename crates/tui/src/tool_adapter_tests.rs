use super::*;
use crate::presentation::{NativeToolState, PresentedToolStatus, native_tool_status};
use crate::status_icon::StatusIcon;

#[test]
fn raw_result_prose_never_supplies_native_outcome_evidence() {
    let mut state = NativeToolState::new(ToolCallStatus::Finished, &json!({}));
    for output in [
        "status: error\nerror: misleading",
        "success",
        "status: denied\nreason: prose",
    ] {
        state.result = Some(test_tool_result("call", "command", output));
        assert_eq!(native_tool_status(&state), PresentedToolStatus::Unknown);
        assert_eq!(native_tool_status(&state).icon(), StatusIcon::Unknown);
    }
    for (outcome, expected) in [
        (ToolCallOutcome::Success, PresentedToolStatus::Completed),
        (ToolCallOutcome::Error, PresentedToolStatus::Failed),
        (ToolCallOutcome::Skipped, PresentedToolStatus::Failed),
        (ToolCallOutcome::Partial, PresentedToolStatus::Failed),
        (ToolCallOutcome::Denied, PresentedToolStatus::Denied),
        (ToolCallOutcome::Cancelled, PresentedToolStatus::Interrupted),
    ] {
        state.metadata = Some(file_metadata("call", None, "command", outcome, vec![]));
        assert_eq!(native_tool_status(&state), expected);
    }
}

#[test]
fn arguments_decode_once_but_copy_keeps_the_original_representation() {
    for raw in [
        json!({"command":"echo 界"}),
        json!("{ \"command\": \"echo 界\" }"),
        json!("{malformed"),
    ] {
        let message = assistant_message(vec![tool_call("call", None, "command", raw.clone())]);
        let expected_wire = serde_json::to_string(&message).unwrap();
        let entry = history_message(message.clone());
        let HistoryEntry::Conversation(entry) = entry else {
            unreachable!()
        };
        let block = &entry.blocks[0];
        let (_, state) = block.native_tool().unwrap();
        if raw == json!("{malformed") {
            assert!(state.arguments.is_none());
        } else {
            assert_eq!(state.arguments.as_ref().unwrap()["command"], "echo 界");
        }
        assert_eq!(
            block.primary_copy(),
            raw.as_str().map_or_else(|| raw.to_string(), str::to_string)
        );
        assert_eq!(serde_json::to_string(&message).unwrap(), expected_wire);
    }
}

#[test]
fn acp_result_sidecar_survives_replay_without_changing_raw_output() {
    let events = vec![
        AgentRunEvent::ToolCall {
            id: "typed".into(),
            title: "Command".into(),
            kind: "execute".into(),
            status: "failed".into(),
            content: vec!["RAW model envelope".into()],
            locations: vec![],
            raw_input: Some(json!({"command":"echo"})),
            raw_output: Some(json!("RAW model envelope")),
        },
        AgentRunEvent::ToolResultMetadata {
            metadata: Box::new(
                file_metadata("typed", None, "command", ToolCallOutcome::Cancelled, vec![])
                    .with_diagnostic("local cancellation"),
            ),
        },
    ];
    let restored: Vec<AgentRunEvent> =
        serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
    for events in [events, restored] {
        let (mut app, mut reducer) = acp_transcript_app();
        for event in events {
            apply_agent_event(&mut app, &mut reducer, event);
        }
        let output = rendered_text(&mut app, 120, 20);
        assert!(output.contains("Command ◼"), "{output}");
        assert!(output.contains("local cancellation"));
        assert!(!output.contains("RAW model envelope"));
        let tool = app
            .history()
            .iter()
            .find_map(|entry| {
                let HistoryEntry::Conversation(entry) = entry else {
                    return None;
                };
                entry
                    .blocks
                    .iter()
                    .find(|block| matches!(block.kind, PresentationBlockKind::Tool(_)))
            })
            .unwrap();
        assert_eq!(tool.secondary_copy().as_deref(), Some("RAW model envelope"));
    }
}
