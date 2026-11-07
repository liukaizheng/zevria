use super::*;
use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, PlanDraftDetail, READ_PLAN_TOOL_NAME, UPDATE_PLAN_TOOL_NAME,
};
use zevria_workflow::PlanDraft;

fn detail(title: &str, change: Option<FileChange>) -> ToolResultDetail {
    ToolResultDetail::PlanDraft(PlanDraftDetail {
        title: title.into(),
        target_revision: 1,
        sequence: Some(1),
        bytes: 100,
        change,
        projection: Some(PathBuf::from(".zevria/plans/session/plan-first-title.md")),
    })
}
#[test]
fn draft_rows_restore_exactly_copy_markdown_and_never_show_successful_read_output() {
    let artifact = test_plan_artifact();
    let cases = [
        (
            CREATE_PLAN_TOOL_NAME,
            json!({"markdown":artifact.markdown,"replace":false}),
            Some(detail(&artifact.title, None)),
        ),
        (
            UPDATE_PLAN_TOOL_NAME,
            json!({"edits":[{"type":"replace","old_string":"Before","new_string":"After","replace_all":false}]}),
            Some(detail(
                &artifact.title,
                Some(FileChange::Update {
                    unified_diff: "--- original\n+++ modified\n@@ -1 +1 @@\n-Before\n+After\n"
                        .into(),
                    move_path: None,
                }),
            )),
        ),
        (READ_PLAN_TOOL_NAME, json!({}), None),
        (
            SUBMIT_PLAN_TOOL_NAME,
            json!({}),
            Some(detail(&artifact.title, None)),
        ),
    ];
    for (name, args, detail) in cases {
        let id = format!("{name}-draft-test");
        let call = assistant_message(vec![tool_call(&id, None, name, args.clone())]);
        let result = tool_result_message(&id, None, name, "HIDDEN_SUCCESS_BODY");
        let mut meta = file_metadata(&id, None, name, ToolCallOutcome::Success, Vec::new());
        meta.detail = detail;
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                turn_id: TEST_TURN_ID,
                message: call.clone(),
                attempt_id: None,
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: result.clone(),
                metadata: vec![meta.clone()],
            },
        );
        app.set_view_for_test(0, false);
        let live = rendered_text(&mut app, 160, 80);
        assert!(!live.contains("HIDDEN_SUCCESS_BODY"));
        match name {
            CREATE_PLAN_TOOL_NAME => {
                assert!(live.contains("create_plan · Durable approval workflow"));
                assert!(live.contains("Own approval in the engine."));
                assert!(live.contains(".zevria/plans/session/plan-first-title.md"));
                app.select_for_test(cursor(0, 0));
                assert_eq!(
                    app.handle_event(key(KeyCode::Char('y'))),
                    Some(UiAction::Copy {
                        text: artifact.markdown.clone()
                    })
                );
            }
            UPDATE_PLAN_TOOL_NAME => {
                assert!(live.contains("update_plan · 1 edits"));
                assert!(live.contains("Before") && live.contains("After"));
            }
            READ_PLAN_TOOL_NAME => {
                assert!(!live.contains("read_plan"));
            }
            SUBMIT_PLAN_TOOL_NAME => {
                assert!(live.contains("submit_plan · Durable approval workflow"));
            }
            _ => unreachable!(),
        }
        let mut restored = App::new();
        restored.restore(vec![
            TranscriptItem::Message(call),
            TranscriptItem::ToolResults {
                message: result,
                metadata: vec![meta],
                skill_applications: Vec::new(),
            },
        ]);
        restored.set_view_for_test(0, false);
        rendered_text(&mut restored, 160, 80);
        assert_eq!(
            laid_out_transcript_text(&restored),
            laid_out_transcript_text(&app)
        );
    }
}
#[test]
fn read_and_edit_failures_always_show_diagnostics() {
    for name in [
        CREATE_PLAN_TOOL_NAME,
        UPDATE_PLAN_TOOL_NAME,
        READ_PLAN_TOOL_NAME,
        SUBMIT_PLAN_TOOL_NAME,
    ] {
        let call = assistant_message(vec![tool_call("failed-plan", None, name, json!({}))]);
        let metadata = file_metadata(
            "failed-plan",
            None,
            name,
            ToolCallOutcome::Error,
            Vec::new(),
        )
        .with_diagnostic("Exact draft diagnostic; call read_plan.");
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                turn_id: TEST_TURN_ID,
                message: call,
                attempt_id: None,
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message("failed-plan", None, name, "opaque result"),
                metadata: vec![metadata],
            },
        );
        assert!(
            rendered_text(&mut app, 160, 40).contains("Exact draft diagnostic; call read_plan."),
            "missing diagnostic for {name}"
        );
    }
}
#[test]
fn draft_only_snapshots_do_not_change_mode_or_close_recovery_and_warn_about_retained_revision() {
    let artifact = test_plan_artifact();
    let mut app = App::new();
    app.restore_plan_state(PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact.clone()),
        draft: None,
    });
    app.handle_event(key(KeyCode::Char('p')));
    assert!(app.plan_recovery_open());
    let before = app.next_mode();
    let draft = PlanDraft {
        target: PlanVersion {
            revision: 2,
            ..artifact.version
        },
        sequence: 3,
        markdown: artifact
            .markdown
            .replace("Build it.", "Unsubmitted change."),
        source_turn_id: TEST_TURN_ID,
    };
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact.clone()),
            draft: Some(draft.clone()),
        },
    });
    assert_eq!(app.next_mode(), before);
    assert!(app.plan_recovery_open());
    assert!(
        rendered_text(&mut app, 160, 35).contains("Unsubmitted draft changes are not included.")
    );
    app.handle_event(key(KeyCode::Char('1')));
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResolvePlan {
            expected: artifact.version,
            decision: PlanDecision::ImplementCurrent
        })
    );
    let mut warning = App::new();
    warning.reduce_without_effects(SessionEvent::PlanProjectionWarning {
        version: draft.target,
        draft: Some(3),
        path: PathBuf::from("mirror.md"),
        error: "read-only directory".into(),
    });
    assert!(rendered_text(&mut warning, 160, 35).contains("draft 3 remains available"));
}
