//! Durable native-child launch correlation for frontend restoration.
use crate::TranscriptItem;
use std::collections::HashMap;

/// Full durable launch correlation; restore is presentation only.
pub fn subtask_launch_metadata(
    items: &[TranscriptItem],
) -> HashMap<String, zevria_foundation::SubtaskLaunchMetadata> {
    let mut launches = HashMap::new();
    for item in items {
        if let TranscriptItem::ToolResults { metadata, .. } = item {
            for entry in metadata {
                for subtask in entry
                    .subtasks()
                    .iter()
                    .filter_map(|entry| entry.launch.as_ref())
                {
                    launches.insert(subtask.id.as_str().to_string(), subtask.clone());
                }
            }
        }
    }
    launches
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::SubtaskId;
    #[test]
    fn full_subtask_launch_metadata_from_the_root_transcript() {
        use rig_core::message::Message;
        use zevria_foundation::SubtaskKind;
        use zevria_foundation::SubtaskLaunchMetadata;
        use zevria_foundation::ToolCallOutcome;
        use zevria_foundation::ToolResultDetail;
        use zevria_foundation::ToolResultMetadata;

        for outcome in [
            ToolCallOutcome::Success,
            ToolCallOutcome::Error,
            ToolCallOutcome::Cancelled,
        ] {
            let items = vec![
                TranscriptItem::Message(Message::user("kick off")),
                TranscriptItem::ToolResults {
                    skill_applications: Vec::new(),
                    message: Message::tool_result("call-1", "launch_subtasks", "status: launched"),
                    metadata: vec![ToolResultMetadata {
                        diagnostic: None,
                        id: "call-1".to_string(),
                        call_id: None,
                        tool_name: "launch_subtasks".to_string(),
                        outcome,
                        detail: Some(ToolResultDetail::Subtasks(vec![
                            zevria_foundation::SubtaskEntryMetadata {
                                index: 0,
                                status: zevria_foundation::SubtaskStatus::Completed,
                                launch: Some(SubtaskLaunchMetadata {
                                    id: SubtaskId::new("child-a"),
                                    title: "map config loading".to_string(),
                                    kind: SubtaskKind::Build,
                                    workspace: Some("landing-pages/book-1".into()),
                                }),
                            },
                        ])),
                    }],
                },
            ];

            let mut items: Vec<TranscriptItem> =
                serde_json::from_value(serde_json::to_value(items).unwrap()).unwrap();
            let launches = subtask_launch_metadata(&items);
            let metadata = launches.get("child-a").unwrap();
            assert_eq!(metadata.title, "map config loading");
            assert_eq!(metadata.kind, SubtaskKind::Build);
            assert_eq!(metadata.workspace.as_deref(), Some("landing-pages/book-1"));
            assert_eq!(launches.len(), 1);

            let TranscriptItem::ToolResults { metadata, .. } = &mut items[1] else {
                unreachable!()
            };
            metadata[0].detail = None;
            assert!(
                subtask_launch_metadata(&items).is_empty(),
                "tool name alone cannot restore a child"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn plural_mixed_terminal_metadata_restores_every_accepted_identity_in_input_order() {
    use zevria_foundation::{
        SubtaskEntryMetadata, SubtaskId, SubtaskKind, SubtaskLaunchMetadata, SubtaskStatus,
        ToolCallOutcome, ToolResultDetail, ToolResultMetadata,
    };
    let entries: Vec<_> = [
        SubtaskStatus::Completed,
        SubtaskStatus::Failed,
        SubtaskStatus::Cancelled,
        SubtaskStatus::Cancelled,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, status)| SubtaskEntryMetadata {
        index,
        status,
        launch: (index < 3).then(|| SubtaskLaunchMetadata {
            id: SubtaskId::new(format!("child-{index}")),
            title: "Duplicate title".into(),
            kind: SubtaskKind::Explore,
            workspace: None,
        }),
    })
    .collect();
    let item = TranscriptItem::ToolResults {
        message: rig_core::message::Message::tool_result(
            "outer",
            "launch_subtasks",
            "entry 0: completed report; entry 1: failed; entry 2/3: cancelled",
        ),
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: "outer".into(),
            call_id: Some("outer".into()),
            tool_name: "launch_subtasks".into(),
            outcome: ToolCallOutcome::Cancelled,
            detail: Some(ToolResultDetail::Subtasks(entries.clone())),
        }],
        skill_applications: vec![],
    };
    let restored: TranscriptItem =
        serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
    let launches = subtask_launch_metadata(std::slice::from_ref(&restored));
    assert_eq!(launches.len(), 3);
    for index in 0..3 {
        assert!(launches.contains_key(&format!("child-{index}")));
    }
    let TranscriptItem::ToolResults {
        metadata, message, ..
    } = restored
    else {
        unreachable!()
    };
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].subtasks(), entries);
    let rig_core::message::Message::User { content } = message else {
        unreachable!()
    };
    assert_eq!(
        content.len(),
        1,
        "one result for the real provider call, not one per child"
    );
}
