use super::*;
use crate::{SubtaskId, SubtaskKind, SubtaskLaunchMetadata, SubtaskStatus};
use serde_json::json;

fn metadata(detail: Option<ToolResultDetail>) -> ToolResultMetadata {
    ToolResultMetadata {
        diagnostic: None,
        id: "result-id".into(),
        call_id: Some("provider-call-id".into()),
        tool_name: "fixture".into(),
        outcome: ToolCallOutcome::Success,
        detail,
    }
}

fn changes() -> Vec<FileChangeOutput> {
    vec![FileChangeOutput {
        path: "src/new.rs".into(),
        change: FileChange::Add {
            content: "metadata-only content".into(),
        },
    }]
}

fn subtask() -> Vec<SubtaskEntryMetadata> {
    vec![SubtaskEntryMetadata {
        index: 0,
        status: SubtaskStatus::Completed,
        launch: Some(SubtaskLaunchMetadata {
            id: SubtaskId::new("child-id"),
            title: "Build a page".into(),
            kind: SubtaskKind::Build,
            workspace: Some("pages/one".into()),
        }),
    }]
}

#[test]
fn ordinary_metadata_omits_detail_and_accepts_explicit_null() {
    let metadata = metadata(None);
    let mut wire = serde_json::to_value(&metadata).unwrap();
    assert_eq!(
        wire,
        json!({
            "id": "result-id",
            "callId": "provider-call-id",
            "toolName": "fixture",
            "outcome": "success",
        })
    );
    assert_eq!(
        serde_json::from_value::<ToolResultMetadata>(wire.clone()).unwrap(),
        metadata
    );
    wire["detail"] = serde_json::Value::Null;
    assert_eq!(
        serde_json::from_value::<ToolResultMetadata>(wire).unwrap(),
        metadata
    );
    assert!(metadata.file_changes().is_empty());
    assert!(metadata.subtasks().is_empty());
    assert!(metadata.question_disposition().is_none());
}

#[test]
fn every_detail_round_trips_with_tagged_data_independently_of_outcome() {
    let changes = changes();
    let subtask = subtask();
    let mut cases = vec![
        (None, None),
        (
            Some(ToolResultDetail::FileChanges(changes.clone())),
            Some(json!({"type": "file_changes", "data": changes})),
        ),
        (
            Some(ToolResultDetail::FileChanges(Vec::new())),
            Some(json!({"type": "file_changes", "data": []})),
        ),
        (
            Some(ToolResultDetail::Subtasks(subtask.clone())),
            Some(json!({"type": "subtasks", "data": subtask})),
        ),
    ];
    for (disposition, value) in [
        (QuestionTerminalDisposition::Answered, "answered"),
        (QuestionTerminalDisposition::Dismissed, "dismissed"),
        (QuestionTerminalDisposition::Unavailable, "unavailable"),
        (
            QuestionTerminalDisposition::InvalidFrontendResponse,
            "invalid_frontend_response",
        ),
    ] {
        cases.push((
            Some(ToolResultDetail::QuestionDisposition(disposition)),
            Some(json!({"type": "question_disposition", "data": value})),
        ));
    }
    for (detail, expected) in cases {
        for outcome in [
            ToolCallOutcome::Success,
            ToolCallOutcome::Error,
            ToolCallOutcome::Skipped,
            ToolCallOutcome::Denied,
            ToolCallOutcome::Cancelled,
            ToolCallOutcome::Partial,
        ] {
            let mut metadata = metadata(detail.clone());
            metadata.outcome = outcome;
            let wire = serde_json::to_value(&metadata).unwrap();
            assert_eq!(wire.get("detail"), expected.as_ref());
            let restored: ToolResultMetadata = serde_json::from_value(wire).unwrap();
            assert_eq!(restored, metadata);
            assert_eq!(
                restored.file_changes(),
                match &detail {
                    Some(ToolResultDetail::FileChanges(changes)) => changes.as_slice(),
                    _ => &[],
                }
            );
            assert_eq!(
                restored.subtasks(),
                match &detail {
                    Some(ToolResultDetail::Subtasks(subtask)) => subtask.as_slice(),
                    _ => &[],
                }
            );
            assert_eq!(
                restored.question_disposition(),
                match detail {
                    Some(ToolResultDetail::QuestionDisposition(disposition)) => Some(disposition),
                    _ => None,
                }
            );
        }
    }
}

#[test]
fn unknown_variants_and_obsolete_payload_fields_are_rejected() {
    let base = serde_json::to_value(metadata(None)).unwrap();
    for detail in [
        json!({"type": "future_detail", "data": {}}),
        json!({"type": "file_changes"}),
        json!({"type": "question_disposition", "data": "future_disposition"}),
    ] {
        let mut wire = base.clone();
        wire["detail"] = detail;
        assert!(serde_json::from_value::<ToolResultMetadata>(wire).is_err());
    }
    // Reject old fields even when empty or paired with a valid new detail.
    // Otherwise restoration could silently discard identity or workflow data.
    for detail in [None, Some(ToolResultDetail::Subtasks(subtask()))] {
        for (field, value) in [
            ("fileChanges", json!(changes())),
            ("fileChanges", json!([])),
            ("subtask", json!(subtask())),
            ("subtask", serde_json::Value::Null),
            ("questionDisposition", json!("dismissed")),
            ("questionDisposition", serde_json::Value::Null),
            ("futureField", json!(true)),
        ] {
            let mut wire = serde_json::to_value(metadata(detail.clone())).unwrap();
            wire[field] = value;
            assert!(
                serde_json::from_value::<ToolResultMetadata>(wire).is_err(),
                "accepted {field}"
            );
        }
    }
}
