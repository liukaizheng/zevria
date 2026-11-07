use super::*;
use zevria_foundation::{PLAN_AUTHORING_TOOL_NAMES, PlanDraftDetail, ToolResultDetail};

#[test]
fn all_plan_authoring_tools_are_thinking_and_create_uses_the_derived_title() {
    for name in PLAN_AUTHORING_TOOL_NAMES {
        assert_eq!(tool_kind(name, &json!({})), ToolKind::Think);
    }
    assert_eq!(
        tool_title(
            "create_plan",
            &json!({"markdown":"# Durable draft authoring workflow\n\n## Goal\nDraft."})
        ),
        "create_plan: Durable draft authoring workflow"
    );
    assert_eq!(
        tool_title("create_plan", &json!({"markdown":"invalid"})),
        "create_plan"
    );
}
#[test]
fn draft_projection_path_is_a_display_only_tool_location() {
    let result = Message::tool_result(
        rig_core::message::CallId::from_wire("save"),
        rig_core::message::ToolName::new("update_plan").unwrap(),
        "Plan draft 2 saved.",
    );
    let metadata = ToolResultMetadata {
        id: "save".into(),
        call_id: None,
        tool_name: "update_plan".into(),
        outcome: ToolCallOutcome::Success,
        diagnostic: None,
        detail: Some(ToolResultDetail::PlanDraft(PlanDraftDetail {
            title: "Durable draft authoring workflow".into(),
            target_revision: 1,
            sequence: Some(2),
            bytes: 100,
            change: None,
            projection: Some(PathBuf::from(
                "/workspace/.zevria/plans/session/first-title.md",
            )),
        })),
    };
    assert!(metadata.file_changes().is_empty());
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("first-title.md")
    );
    let updates = project_tool_results(
        &result,
        &[metadata],
        Path::new("/workspace"),
        &KnownTools::default(),
    );
    let wire = serde_json::to_value(&updates[0]).unwrap();
    assert_eq!(
        wire["locations"][0]["path"],
        "/workspace/.zevria/plans/session/first-title.md"
    );
    assert!(
        wire["rawOutput"]
            .to_string()
            .contains("Plan draft 2 saved.")
    );
}
