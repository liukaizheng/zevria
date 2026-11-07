use super::*;
use rig_agent::tool::{Tool, ToolContext};
use zevria_foundation::{ToolContextRuntimeExt, ToolResultDetail, TurnId};
use zevria_workflow::{
    PlanCandidate, PlanDraft, PlanEdit, PlanId, PlanRecord, PlanWorkflowState, apply_plan_record,
};

fn markdown() -> String {
    "# Durable plan authoring workflow\n\n## Goal\nShip.\n\n## Decisions\nRust.\n\n## Implementation\nBuild.\n\n## Validation\nTest.\n\n## Risks\nNone.".into()
}
fn scoped(state: &PlanWorkflowState) -> ToolContext {
    let mut context = ToolContext::new();
    context.insert_scope(state.draft_scope(TurnId::new(1)).unwrap());
    context
}
fn initial() -> PlanWorkflowState {
    apply_plan_record(
        PlanWorkflowState::Idle,
        &PlanRecord::Started { id: PlanId::new() },
    )
    .unwrap()
}
#[tokio::test]
async fn lifecycle_and_scope_contract() {
    let create = CreatePlanTool::new(usize::MAX);
    let update = UpdatePlanTool::new(usize::MAX);
    let submit = SubmitPlanTool::new(usize::MAX);
    let args = CreatePlanArgs {
        markdown: markdown(),
        replace: false,
    };
    let edits = UpdatePlanArgs {
        edits: vec![PlanEdit::Replace {
            old_string: "Ship.".into(),
            new_string: "Ship drafts.".into(),
            replace_all: false,
        }],
    };
    let mut unscoped = ToolContext::new();
    assert!(create.call(&mut unscoped, args.clone()).await.is_err());
    assert!(update.call(&mut unscoped, edits.clone()).await.is_err());
    assert!(
        ReadPlanTool
            .call(&mut unscoped, ReadPlanArgs {})
            .await
            .is_err()
    );
    assert!(submit.call(&mut unscoped, SubmitPlanArgs {}).await.is_err());
    let state = initial();
    let mut context = scoped(&state);
    assert!(update.call(&mut context, edits.clone()).await.is_err());
    let message = create.call(&mut context, args.clone()).await.unwrap();
    assert_eq!(
        message,
        format!(
            "Plan draft 1 for revision 1 saved ({} bytes). Submission check: ready to submit.",
            markdown().len()
        )
    );
    let draft = context.result::<PlanDraft>().unwrap().unwrap();
    assert_eq!(draft.markdown, markdown());
    assert_eq!(
        context
            .result::<ToolResultDetail>()
            .unwrap()
            .unwrap()
            .file_changes(),
        &[]
    );
    let state = apply_plan_record(state, &PlanRecord::DraftSaved { draft }).unwrap();
    let mut context = scoped(&state);
    assert!(create.call(&mut context, args.clone()).await.is_err());
    assert!(
        create
            .call(
                &mut context,
                CreatePlanArgs {
                    replace: true,
                    ..args
                }
            )
            .await
            .is_ok()
    );
    let mut context = scoped(&state);
    assert_eq!(
        ReadPlanTool
            .call(&mut context, ReadPlanArgs {})
            .await
            .unwrap(),
        format!(
            "Plan draft 1 for revision 1 ({} bytes).\n{}",
            markdown().len(),
            markdown()
        )
    );
    let result = update.call(&mut context, edits).await.unwrap();
    assert!(result.starts_with("Plan draft 2 for revision 1 saved"));
    let detail = context.result::<ToolResultDetail>().unwrap().unwrap();
    let detail = detail.plan_draft().unwrap();
    assert_eq!(detail.sequence, Some(2));
    assert!(
        matches!(&detail.change, Some(zevria_foundation::FileChange::Update { unified_diff, .. }) if unified_diff.contains("+Ship drafts."))
    );
    let draft = context.result::<PlanDraft>().unwrap().unwrap();
    let state = apply_plan_record(
        state,
        &PlanRecord::DraftSaved {
            draft: draft.clone(),
        },
    )
    .unwrap();
    let mut context = scoped(&state);
    submit.call(&mut context, SubmitPlanArgs {}).await.unwrap();
    assert_eq!(
        context
            .result::<PlanCandidate>()
            .unwrap()
            .unwrap()
            .markdown(),
        draft.markdown
    );
}
#[tokio::test]
async fn incomplete_drafts_and_submission_checks() {
    let mut context = scoped(&initial());
    let text = "# Durable plan authoring workflow\n\n## Risks\n".to_owned();
    let message = CreatePlanTool::new(1000)
        .call(
            &mut context,
            CreatePlanArgs {
                markdown: text,
                replace: false,
            },
        )
        .await
        .unwrap();
    assert!(message.contains("Goal") && message.contains("Risks"));
    let draft = context.result::<PlanDraft>().unwrap().unwrap();
    let mut context = scoped(
        &apply_plan_record(
            initial_with_id(draft.target.id),
            &PlanRecord::DraftSaved { draft },
        )
        .unwrap(),
    );
    let error = SubmitPlanTool::new(1000)
        .call(&mut context, SubmitPlanArgs {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("update_plan"));
    assert!(context.result::<PlanCandidate>().unwrap().is_none());
    let mut context = scoped(&initial());
    assert!(
        CreatePlanTool::new(20)
            .call(
                &mut context,
                CreatePlanArgs {
                    markdown: markdown(),
                    replace: false
                }
            )
            .await
            .is_err()
    );
}
fn initial_with_id(id: PlanId) -> PlanWorkflowState {
    apply_plan_record(PlanWorkflowState::Idle, &PlanRecord::Started { id }).unwrap()
}
#[test]
fn strict_schemas_and_static_descriptions() {
    let create = CreatePlanTool::new(1234);
    let update = UpdatePlanTool::new(1234);
    let read = ReadPlanTool;
    let submit = SubmitPlanTool::new(1234);
    for schema in [
        create.parameters(),
        update.parameters(),
        read.parameters(),
        submit.parameters(),
    ] {
        assert_eq!(schema["additionalProperties"], false);
    }
    assert_eq!(
        create.parameters()["required"],
        serde_json::json!(["markdown", "replace"])
    );
    assert_eq!(
        update.parameters()["required"],
        serde_json::json!(["edits"])
    );
    assert_eq!(update.parameters()["properties"]["edits"]["minItems"], 1);
    assert_eq!(update.parameters()["properties"]["edits"]["maxItems"], 32);
    assert!(
        submit.parameters()["properties"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    for value in [
        serde_json::json!({"markdown":"legacy"}),
        serde_json::json!(null),
        serde_json::json!("text"),
    ] {
        assert!(
            serde_json::from_value::<SubmitPlanArgs>(value)
                .unwrap_err()
                .to_string()
                .contains("submit_plan submits the stored draft; change it with update_plan first")
        );
    }
    assert_eq!(
        create.description(),
        "Save the initial Plan draft in engine storage; it persists across turns, restarts and context compaction and is not submitted. `markdown` must begin with exactly `# <title>` (one line, 3–8 words); other sections may be incomplete; limit 1234 bytes. Fails when a draft or a submitted plan under revision already exists unless `replace` is true; prefer update_plan for changes."
    );
    assert_eq!(
        update.description(),
        "Revise the stored Plan working copy with 1–32 atomic, ordered edits. `replace` uses literal old_string/new_string matching: exactly one match unless replace_all is true; old_string must not be empty. `replace_section` replaces the body under one exact ATX heading line of level 2 or deeper, outside fenced code, up to the next heading of the same or higher level; the heading line is kept. Any failure changes nothing; identical results are errors. The result must begin with exactly `# <title>` (one line, 3–8 words) and fit 1234 bytes; other sections may be incomplete. Call read_plan after a match failure to get the exact stored text."
    );
    assert_eq!(
        read.description(),
        "Return the exact stored Plan draft (or the submitted plan under revision) with its draft number."
    );
    assert!(
        submit
            .description()
            .starts_with("Submit the stored Plan draft unchanged; takes no arguments.")
    );
    assert!(submit.description().contains("1234 bytes"));
    let schema = update.parameters();
    let variants = schema["$defs"]["PlanEdit"]["oneOf"].as_array().unwrap();
    assert_eq!(variants.len(), 2);
    for variant in variants {
        assert_eq!(variant["additionalProperties"], false);
    }
}
