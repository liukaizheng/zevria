use super::*;
use zevria_foundation::{
    SubtaskDescriptor, SubtaskEntryMetadata, SubtaskId, SubtaskKind, SubtaskLaunchMetadata,
    SubtaskStatus, ToolResultDetail,
};
use zevria_transcript::TranscriptItem;

fn child(index: usize) -> SubtaskDescriptor {
    SubtaskDescriptor {
        id: SubtaskId::new(format!("child-{index}")),
        parent_session_id: "root".into(),
        title: "Duplicate title".into(),
        kind: SubtaskKind::Explore,
        workspace: None,
        status: SubtaskStatus::Starting,
    }
}
fn display(
    updates: &[AcpSessionUpdate],
) -> HashMap<String, (String, ToolCallStatus, Option<Value>)> {
    let mut result = HashMap::new();
    for update in updates {
        match update {
            AcpSessionUpdate::ToolCall(call) => {
                result.insert(
                    call.tool_call_id.to_string(),
                    (call.title.clone(), call.status, call.raw_input.clone()),
                );
            }
            AcpSessionUpdate::ToolCallUpdate(update) => {
                if let Some(entry) = result.get_mut(&update.tool_call_id.to_string()) {
                    if let Some(title) = &update.fields.title {
                        entry.0 = title.clone();
                    }
                    if let Some(status) = update.fields.status {
                        entry.1 = status;
                    }
                    if let Some(input) = &update.fields.raw_input {
                        entry.2 = Some(input.clone());
                    }
                }
            }
            _ => {}
        }
    }
    result
}

#[test]
fn duplicate_children_have_distinct_lifecycles_and_parent_finishes_only_at_batch_result() {
    let call = Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall::from_dual_wire(
            "fc-outer",
            "outer",
            rig_core::message::ToolFunction::new(
                "launch_subtasks".into(),
                json!({"tasks":[
                    {"title":"Duplicate title","prompt":"first","type":"explore"},
                    {"title":"Duplicate title","prompt":"second","type":"explore"}
                ]}),
            ),
        ))],
    };
    let mut known = KnownTools::new();
    let workspace = Path::new("/startup");
    let mut updates = project_tool_calls(&call, workspace, &mut known);
    let mut projection = SubtaskProjection::default();
    assert!(
        projection
            .status(&child(1).id, SubtaskStatus::Running)
            .is_none()
    );
    updates.push(projection.launch("outer", 1, child(1)));
    updates.push(projection.launch("outer", 0, child(0)));
    updates.push(
        projection
            .status(&child(1).id, SubtaskStatus::Completed)
            .unwrap(),
    );
    let midway = display(&updates);
    assert_eq!(midway["outer"].1, ToolCallStatus::InProgress);
    assert_eq!(midway["zevria-subtask-child-0"].1, ToolCallStatus::Pending);
    assert_eq!(
        midway["zevria-subtask-child-1"].1,
        ToolCallStatus::Completed
    );
    updates.push(
        projection
            .status(&child(1).id, SubtaskStatus::Running)
            .unwrap(),
    );
    updates.push(projection.launch("outer", 1, child(1)));
    assert_eq!(
        display(&updates)["zevria-subtask-child-1"].1,
        ToolCallStatus::Completed
    );
    let metadata = ToolResultMetadata {
        diagnostic: None,
        id: "outer".into(),
        call_id: Some("outer".into()),
        tool_name: "launch_subtasks".into(),
        outcome: ToolCallOutcome::Error,
        detail: Some(ToolResultDetail::Subtasks(
            (0..2)
                .map(|index| {
                    let child = child(index);
                    SubtaskEntryMetadata {
                        index,
                        status: if index == 0 {
                            SubtaskStatus::Failed
                        } else {
                            SubtaskStatus::Completed
                        },
                        launch: Some(SubtaskLaunchMetadata {
                            id: child.id,
                            title: child.title,
                            kind: child.kind,
                            workspace: child.workspace,
                        }),
                    }
                })
                .collect(),
        )),
    };
    let result = Message::tool_result("outer", "launch_subtasks", "one failure and one report");
    updates.extend(projection.results(std::slice::from_ref(&metadata)));
    assert_eq!(display(&updates)["outer"].1, ToolCallStatus::InProgress);
    updates.extend(project_tool_results(
        &result,
        std::slice::from_ref(&metadata),
        workspace,
        &known,
    ));
    let final_display = display(&updates);
    assert_eq!(final_display.len(), 3);
    assert_eq!(final_display["outer"].1, ToolCallStatus::Failed);
    assert_eq!(
        final_display["zevria-subtask-child-0"].1,
        ToolCallStatus::Failed
    );
    assert_eq!(
        final_display["zevria-subtask-child-1"].1,
        ToolCallStatus::Completed
    );
    for index in 0..2 {
        let input = final_display[&format!("zevria-subtask-child-{index}")]
            .2
            .as_ref()
            .unwrap();
        assert_eq!(input["parent_call_id"], "outer");
        assert_eq!(input["entry_index"], index);
        assert!(!input.to_string().contains("report"));
    }
    let items = [
        TranscriptItem::Message(call),
        TranscriptItem::ToolResults {
            message: result,
            metadata: vec![metadata],
            skill_applications: vec![],
        },
    ];
    let items: Vec<TranscriptItem> =
        serde_json::from_str(&serde_json::to_string(&items).unwrap()).unwrap();
    let replay = crate::replay::replay_transcript(&items, workspace);
    assert_eq!(
        display(&replay),
        final_display,
        "replay preserves both identities and each status without relaunching"
    );
}
