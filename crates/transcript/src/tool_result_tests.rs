use super::*;
use rig_core::message::ToolResultContent;
use serde_json::json;
use zevria_foundation::{
    FileChange, FileChangeOutput, QuestionTerminalDisposition, SubtaskId, SubtaskKind,
    SubtaskLaunchMetadata, ToolCallOutcome, ToolResultDetail,
};

fn mixed_results() -> TranscriptItem {
    let cases = [
        ("ordinary", "command", ToolCallOutcome::Success, None),
        (
            "files",
            "edit",
            ToolCallOutcome::Partial,
            Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
                path: "PRIVATE_PATH".into(),
                change: FileChange::Add {
                    content: "PRIVATE_CONTENT".into(),
                },
            }])),
        ),
        (
            "launch",
            "launch_subtasks",
            ToolCallOutcome::Cancelled,
            Some(ToolResultDetail::Subtasks(vec![
                zevria_foundation::SubtaskEntryMetadata {
                    index: 0,
                    status: zevria_foundation::SubtaskStatus::Completed,
                    launch: Some(SubtaskLaunchMetadata {
                        id: SubtaskId::new("PRIVATE_CHILD_ID"),
                        title: "PRIVATE_TITLE".into(),
                        kind: SubtaskKind::Build,
                        workspace: Some("PRIVATE_WORKSPACE".into()),
                    }),
                },
            ])),
        ),
        (
            "question",
            "question",
            ToolCallOutcome::Error,
            Some(ToolResultDetail::QuestionDisposition(
                QuestionTerminalDisposition::InvalidFrontendResponse,
            )),
        ),
    ];
    TranscriptItem::ToolResults {
        message: Message::User {
            content: cases
                .iter()
                .map(|(id, name, _, _)| {
                    UserContent::tool_result_with_call_id(
                        *id,
                        format!("provider-{id}"),
                        *name,
                        vec![ToolResultContent::text("plain result")],
                    )
                })
                .collect(),
        },
        metadata: cases
            .into_iter()
            .map(|(id, name, outcome, detail)| ToolResultMetadata {
                diagnostic: Some(format!("PRIVATE_DIAGNOSTIC_{id}")),
                id: format!("provider-{id}"),
                call_id: Some(format!("provider-{id}")),
                tool_name: name.into(),
                outcome,
                detail,
            })
            .collect(),
        skill_applications: Vec::new(),
    }
}

#[test]
fn mixed_details_round_trip_in_the_existing_sidecar_without_entering_model_input() {
    let original = mixed_results();
    let wire = serde_json::to_string(&original).unwrap();
    assert_eq!(wire.lines().count(), 1);
    let json: serde_json::Value = serde_json::from_str(&wire).unwrap();
    let metadata = json[TOOL_RESULT_METADATA_KEY].as_array().unwrap();
    assert_eq!(metadata.len(), 4);
    assert_eq!(metadata[1]["diagnostic"], "PRIVATE_DIAGNOSTIC_files");
    assert!(metadata[0].get("detail").is_none());
    assert_eq!(metadata[1]["detail"]["type"], "file_changes");
    assert_eq!(metadata[2]["detail"]["type"], "subtasks");
    assert_eq!(metadata[3]["detail"]["type"], "question_disposition");
    assert!(json.get(SUBTASK_RESULTS_KEY).is_none());

    let restored: TranscriptItem = serde_json::from_str(&wire).unwrap();
    assert_eq!(restored, original);
    let TranscriptItem::ToolResults {
        message: Message::User { content },
        metadata,
        ..
    } = &restored
    else {
        panic!("correlated tool results")
    };
    assert_eq!(content.len(), metadata.len());
    for (result, metadata) in content.iter().zip(metadata) {
        let UserContent::ToolResult(result) = result else {
            panic!("tool result")
        };
        assert_eq!(metadata.id, result.call.as_str());
        assert_eq!(
            metadata.call_id.as_deref(),
            result
                .provider
                .as_ref()
                .map(|provider| provider.call_id.as_str())
        );
        assert_eq!(metadata.tool_name, result.name);
    }
    let plain = original.message().unwrap();
    assert_eq!(restored.message(), Some(plain));
    assert_eq!(
        restored.model_request_item(),
        Some(ModelRequestItem::message(plain))
    );
    // A bare Rig reader still ignores the sidecar envelope, not its model text.
    assert_eq!(serde_json::from_str::<Message>(&wire).unwrap(), *plain);
    let items = vec![restored];
    assert_eq!(model_history(&items), vec![plain.clone()]);
    assert_eq!(model_input(&items), vec![ModelRequestItem::message(plain)]);
    let model_wire = serde_json::to_string(&model_history(&items)).unwrap();
    for hidden in [
        "PRIVATE_",
        "invalid_frontend_response",
        TOOL_RESULT_METADATA_KEY,
    ] {
        assert!(!model_wire.contains(hidden), "leaked {hidden}");
    }
}

#[test]
fn invalid_details_and_retired_fields_reject_the_sidecar_instead_of_falling_back_to_messages() {
    let base = serde_json::to_value(mixed_results()).unwrap();
    for (key, value) in [
        ("detail", json!({"type": "future_variant", "data": {}})),
        (
            "fileChanges",
            base[TOOL_RESULT_METADATA_KEY][1]["detail"]["data"].clone(),
        ),
        (
            "subtask",
            base[TOOL_RESULT_METADATA_KEY][2]["detail"]["data"].clone(),
        ),
        ("questionDisposition", json!("dismissed")),
    ] {
        // Cover both obsolete-only records and obsolete fields mixed with a
        // new payload. Neither may silently lose restore metadata.
        for keep_detail in [false, true] {
            let mut wire = base.clone();
            let entry = wire[TOOL_RESULT_METADATA_KEY][1].as_object_mut().unwrap();
            if !keep_detail {
                entry.remove("detail");
            }
            entry.insert(key.into(), value.clone());
            assert!(serde_json::from_value::<TranscriptItem>(wire.clone()).is_err());
            assert!(serde_json::from_value::<Message>(wire).is_ok());
        }
    }
}
