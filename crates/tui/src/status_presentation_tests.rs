//! Status decorations and readable copies must not alter transcript payloads.
use super::*;
use crate::app::{FoldKey, FoldState};
use crate::layout::ConversationCache;
use crate::presentation::{
    AcpToolPresentation, BlockVisibility, ChecklistItem, ChecklistStatus, ConversationEntry,
    NativeToolState, PresentationBlock, PresentationBlockId, PresentedChecklist, PresentedPlan,
    PresentedPlanContent, PresentedTool, WebActivityPresentation,
};
use crate::status_icon::StatusIcon;

fn block(kind: PresentationBlockKind) -> PresentationBlock {
    PresentationBlock {
        id: PresentationBlockId(0),
        revision: 0,
        role: Some(PresentationRole::Assistant),
        prompt_group: None,
        prompt: None,
        visibility: BlockVisibility::Always,
        kind,
    }
}

fn native(
    name: &str,
    arguments: serde_json::Value,
    status: ToolCallStatus,
    outcome: Option<ToolCallOutcome>,
) -> PresentationBlock {
    let AssistantContent::ToolCall(call) = tool_call("status-test", None, name, arguments) else {
        unreachable!()
    };
    let mut state = NativeToolState::new(status, &call.function.arguments);
    if let Some(outcome) = outcome {
        state.metadata = Some(file_metadata(
            "status-test",
            None,
            name,
            outcome,
            Vec::new(),
        ));
        state.result = Some(test_tool_result(
            "status-test",
            name,
            "exact result\nsecond line",
        ));
    }
    block(PresentationBlockKind::Tool(PresentedTool::Native {
        call: Box::new(call),
        state: Box::new(state),
    }))
}

fn render(
    block: &PresentationBlock,
    width: u16,
    selected: bool,
    folded: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let mut lines = Vec::new();
    crate::layout::prepare::render_conversation_block(
        block,
        &mut lines,
        crate::layout::prepare::ConversationBlockContext {
            width,
            header_role: None,
            header: None,
            separator_before: false,
            item_gap_before: false,
            selected,
            folded,
            reasoning_heading: false,
            appearance: crate::presentation::TranscriptAppearance::Native,
        },
    );
    lines
}

fn assert_icon(lines: &[ratatui::text::Line<'_>], icon: StatusIcon) {
    assert!(
        lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.trim() == icon.glyph(0) && span.style.fg == Some(icon.color())
        }),
        "missing {icon:?} with semantic foreground: {lines:?}"
    );
}

fn checklist() -> PresentationBlock {
    block(PresentationBlockKind::Plan(PresentedPlan {
        plan_id: None,
        content: PresentedPlanContent::Checklist(PresentedChecklist {
            label: "plan".into(),
            items: vec![
                ChecklistItem {
                    text: "waiting".into(),
                    priority: None,
                    status: ChecklistStatus::Pending,
                },
                ChecklistItem {
                    text: "working".into(),
                    priority: Some("high".into()),
                    status: ChecklistStatus::InProgress,
                },
                ChecklistItem {
                    text: "finished".into(),
                    priority: Some("low".into()),
                    status: ChecklistStatus::Completed,
                },
                ChecklistItem {
                    text: "future status".into(),
                    priority: None,
                    status: ChecklistStatus::Unknown,
                },
            ],
        }),
    }))
}

fn task_arguments() -> serde_json::Value {
    json!({"explanation": "Keep the full explanation, including every detail.", "tasks": [
        {"step": format!("Full pending text {}", "界 é 👩‍💻 ".repeat(12).trim_end()), "status": "pending"},
        {"step": "working", "status": "in_progress"},
        {"step": "finished", "status": "completed"}
    ]})
}

#[test]
fn every_native_header_path_has_one_diamond_and_semantic_outcome() {
    for (name, arguments) in [
        ("generic", json!({"argument": "value"})),
        ("command", json!({"command": "echo first\necho last"})),
        ("read", json!({"file_path": "src/a.rs"})),
        ("write", json!({"file_path": "src/a.rs", "content": "body"})),
        ("edit", json!({"file_path": "src/a.rs", "replacements": []})),
        ("delete", json!({"file_path": "src/a.rs"})),
        ("skill", json!({"skill": "test-skill"})),
        ("task", task_arguments()),
        (
            "question",
            json!({"questions": [{"id": "q", "header": "Scope"}]}),
        ),
        (
            "submit_plan",
            json!({"title": "Title", "markdown": "# Title"}),
        ),
        (
            "reconcile_reports",
            serde_json::to_value(test_reconciliation()).unwrap(),
        ),
    ] {
        for (state, outcome, icon) in [
            (ToolCallStatus::Executing, None, StatusIcon::Running),
            (ToolCallStatus::Interrupted, None, StatusIcon::Interrupted),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Success),
                if name == "task" {
                    StatusIcon::Updated
                } else {
                    StatusIcon::Done
                },
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Error),
                StatusIcon::Failed,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Denied),
                StatusIcon::Denied,
            ),
            (
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Cancelled),
                StatusIcon::Interrupted,
            ),
        ] {
            let block = native(name, arguments.clone(), state, outcome);
            let before = block.primary_copy();
            let lines = render(&block, 160, false, false);
            assert_eq!(
                lines
                    .iter()
                    .map(line_text)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .matches('◆')
                    .count(),
                1,
                "{name}"
            );
            assert!(lines[0].to_string().starts_with("◆ "));
            assert_icon(&lines[..if name == "command" { 2 } else { 1 }], icon);
            assert_eq!(block.primary_copy(), before);
            assert!(!before.contains('◆'));
            for folded in [false, true] {
                let selected = render(&block, 40, true, folded);
                assert!(
                    selected
                        .iter()
                        .flat_map(|line| &line.spans)
                        .all(|span| span.style.fg == Some(ZEVRIA_DARK.surfaces.canvas))
                );
            }
        }
    }
}

#[test]
fn lists_and_subtasks_share_icons_in_rendering_and_primary_copy() {
    let list = checklist();
    let expected = "○ waiting\n◐ working (high)\n✓ finished (low)\n? future status";
    assert_eq!(list.primary_copy(), expected);
    assert_eq!(list.readable_list_copy().as_deref(), Some(expected));
    let lines = render(&list, 80, false, false);
    for icon in [
        StatusIcon::Pending,
        StatusIcon::Running,
        StatusIcon::Done,
        StatusIcon::Unknown,
    ] {
        assert_icon(&lines, icon);
    }
    assert!(
        !lines.iter().any(|line| line.to_string().contains('◆')),
        "a checklist is not a tool header"
    );
    for (status, icon) in [
        (SubtaskStatus::Starting, StatusIcon::Running),
        (SubtaskStatus::Running, StatusIcon::Running),
        (SubtaskStatus::Completed, StatusIcon::Done),
        (SubtaskStatus::Failed, StatusIcon::Failed),
        (SubtaskStatus::Cancelled, StatusIcon::Interrupted),
    ] {
        let mut descriptor = child_descriptor("child", "full title");
        descriptor.status = status;
        descriptor.workspace = Some("pages/private".into());
        let before = serde_json::to_value(&descriptor).unwrap();
        let child = block(PresentationBlockKind::Subtask {
            parent: PresentationBlockId(1),
            entry_index: 0,
            descriptor,
        });
        let lines = render(&child, 80, false, false);
        assert_eq!(
            lines[0].to_string(),
            format!("◆ explore · full title · pages/private {}", icon.glyph(0))
        );
        assert_icon(&lines, icon);
        assert_eq!(
            child.primary_copy(),
            format!("explore · full title {}", icon.glyph(0))
        );
        assert!(child.readable_list_copy().is_none());
        let PresentationBlockKind::Subtask { descriptor, .. } = &child.kind else {
            unreachable!()
        };
        assert_eq!(serde_json::to_value(descriptor).unwrap(), before);
    }
}

#[test]
fn readable_native_lists_preserve_operation_evidence_full_source_and_raw_payloads() {
    let mut reconciliation = test_reconciliation();
    reconciliation.disagreements[0].id = format!("untruncated-{}", "界e\u{301}".repeat(120));
    for (name, arguments) in [
        ("task", task_arguments()),
        (
            "reconcile_reports",
            serde_json::to_value(&reconciliation).unwrap(),
        ),
    ] {
        for encoded in [false, true] {
            let arguments = if encoded {
                json!(arguments.to_string())
            } else {
                arguments.clone()
            };
            for (state, outcome, icon) in [
                (ToolCallStatus::Executing, None, StatusIcon::Running),
                (ToolCallStatus::Interrupted, None, StatusIcon::Interrupted),
                (ToolCallStatus::Finished, None, StatusIcon::Unknown),
                (
                    ToolCallStatus::Finished,
                    Some(ToolCallOutcome::Success),
                    if name == "task" {
                        StatusIcon::Updated
                    } else {
                        StatusIcon::Done
                    },
                ),
                (
                    ToolCallStatus::Finished,
                    Some(ToolCallOutcome::Error),
                    StatusIcon::Failed,
                ),
                (
                    ToolCallStatus::Finished,
                    Some(ToolCallOutcome::Denied),
                    StatusIcon::Denied,
                ),
                (
                    ToolCallStatus::Finished,
                    Some(ToolCallOutcome::Cancelled),
                    StatusIcon::Interrupted,
                ),
            ] {
                let block = native(name, arguments.clone(), state, outcome);
                let (call, native_state) = block.native_tool().unwrap();
                let before = (
                    serde_json::to_value(call).unwrap(),
                    serde_json::to_value(&native_state.metadata).unwrap(),
                    block.secondary_copy(),
                );
                let readable = block.readable_list_copy().unwrap();
                assert!(
                    readable.lines().next().unwrap().ends_with(icon.glyph(0)),
                    "{readable}"
                );
                assert!(!readable.contains('◆'));
                if name == "task" {
                    let args = task_arguments();
                    assert!(readable.contains(args["explanation"].as_str().unwrap()));
                    assert!(
                        readable
                            .contains(&format!("○ {}", args["tasks"][0]["step"].as_str().unwrap()))
                    );
                    assert!(readable.contains("◐ working\n✓ finished"));
                    let rows = render(&block, 80, false, false);
                    assert_icon(&rows, StatusIcon::Pending);
                    assert_icon(&rows, StatusIcon::Running);
                    assert_icon(&rows, StatusIcon::Done);
                    let done = rows
                        .iter()
                        .find(|line| line.to_string() == "✓ finished")
                        .unwrap();
                    assert_eq!(done.spans[1].style.fg, Some(ZEVRIA_DARK.text.muted));
                } else {
                    assert!(readable.contains(&format!(
                        "⚖ {} · factual",
                        reconciliation.disagreements[0].id
                    )));
                    assert!(readable.contains(&format!(
                        "✓ {} · recorded · applied",
                        reconciliation.decisions[0].decision_id
                    )));
                    assert!(readable.contains(&format!(
                        "– {} · recorded · objectively inapplicable",
                        reconciliation.decisions[1].decision_id
                    )));
                    assert!(readable.contains(&format!(
                        "○ {} · unavailable · root question required",
                        reconciliation.unavailable_decisions[0].unavailable_decision_id
                    )));
                    assert!(readable.contains(&format!(
                        "– {} · unavailable · objectively inapplicable",
                        reconciliation.unavailable_decisions[1].unavailable_decision_id
                    )));
                    for text in [
                        "SUMMARY_SENTINEL",
                        "FIRST_POSITION_SENTINEL",
                        "EVIDENCE_SENTINEL",
                        "EXPLANATION_SENTINEL",
                        "UNAVAILABLE_REASON_SENTINEL",
                    ] {
                        assert!(readable.contains(text), "{text}: {readable}");
                    }
                    let positions = [
                        "⚖ untruncated-",
                        "⚖ requirement",
                        "⚖ recorded",
                        "⚖ question",
                        " · recorded · applied",
                        " · recorded · objectively",
                        " · unavailable · root",
                        " · unavailable · objectively",
                    ]
                    .map(|text| readable.find(text).unwrap());
                    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
                    assert_icon(&render(&block, 80, false, false), StatusIcon::Dismissed);
                }
                for width in [1, 12, 80] {
                    render(&block, width, true, true);
                }
                assert_eq!(
                    block.primary_copy(),
                    arguments
                        .as_str()
                        .map_or_else(|| arguments.to_string(), str::to_owned)
                );
                assert_eq!(block.readable_list_copy().unwrap(), readable);
                assert_eq!(
                    (
                        serde_json::to_value(call).unwrap(),
                        serde_json::to_value(&native_state.metadata).unwrap(),
                        block.secondary_copy()
                    ),
                    before
                );
            }
        }
    }
    // Neither half of the completion evidence is sufficient on its own.
    for name in ["task", "reconcile_reports"] {
        let args = if name == "task" {
            task_arguments()
        } else {
            serde_json::to_value(test_reconciliation()).unwrap()
        };
        for missing_result in [false, true] {
            let mut block = native(
                name,
                args.clone(),
                ToolCallStatus::Finished,
                Some(ToolCallOutcome::Success),
            );
            let (_, state) = block.native_tool_mut().unwrap();
            if missing_result {
                state.result = None;
            } else {
                state.metadata = None;
            }
            assert!(
                block
                    .readable_list_copy()
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .ends_with('?')
            );
            assert_icon(&render(&block, 80, false, true), StatusIcon::Unknown);
        }
    }
}

#[test]
fn readable_y_is_block_only_independent_of_yank_and_fold_chords_in_every_pane() {
    for mut app in [
        App::new(),
        App::subtask_inspect("child"),
        acp_transcript_app().0,
    ] {
        let task = native(
            "task",
            task_arguments(),
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Success),
        );
        let primary = task.primary_copy();
        let secondary = task.secondary_copy().unwrap();
        let readable = task.readable_list_copy().unwrap();
        app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![task],
        }));
        let index = app.history().len() - 1;
        app.select_for_test(cursor(index, 0));
        assert!(context_help(&app).contains("Y list"));
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                app.handle_event(key(KeyCode::Char('y'))),
                Some(UiAction::Copy {
                    text: primary.clone()
                })
            );
            assert_eq!(
                app.handle_event(modified_key(KeyCode::Char('Y'), modifiers)),
                Some(UiAction::Copy {
                    text: readable.clone()
                })
            );
            assert_eq!(
                app.handle_event(key(KeyCode::Char('y'))),
                Some(UiAction::Copy {
                    text: primary.clone()
                })
            );
            assert_eq!(
                app.handle_event(key(KeyCode::Char('y'))),
                Some(UiAction::Copy {
                    text: secondary.clone()
                })
            );
            app.handle_event(key(KeyCode::Char('z')));
            assert_eq!(
                app.handle_event(modified_key(KeyCode::Char('Y'), modifiers)),
                Some(UiAction::Copy {
                    text: readable.clone()
                })
            );
            assert!(!app.interaction().pending_z());
        }
        app.handle_event(key(KeyCode::Char('z')));
        app.handle_event(key(KeyCode::Char('c')));
        rendered_text(&mut app, 40, 15);
        assert_eq!(
            app.handle_event(key(KeyCode::Char('Y'))),
            Some(UiAction::Copy {
                text: readable.clone()
            })
        );
        assert_eq!(
            app.handle_event(modified_key(KeyCode::Char('Y'), KeyModifiers::CONTROL)),
            None
        );
        app.select_message_for_test(cursor(index, 0));
        assert_eq!(app.handle_event(key(KeyCode::Char('Y'))), None);
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy { text: primary })
        );
        app.select_for_test(cursor(index, 0));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('Y'))),
            Some(UiAction::Copy { text: readable })
        );
    }
}

#[test]
fn readable_y_fallbacks_and_message_copy_preserve_primary_contracts() {
    let mut blocks = vec![checklist()];
    for name in ["task", "reconcile_reports"] {
        for args in [json!("{malformed"), json!({"invalid": "arguments"})] {
            let block = native(name, args, ToolCallStatus::Executing, None);
            assert_eq!(block.readable_list_copy(), Some(block.primary_copy()));
            blocks.push(block);
        }
    }
    blocks.push(native(
        "command",
        json!({"command": "echo untouched"}),
        ToolCallStatus::Executing,
        None,
    ));
    blocks.push(native(
        "submit_plan",
        json!({"title": "Title", "markdown": "# Title\nOriginal Markdown"}),
        ToolCallStatus::Executing,
        None,
    ));
    let mut child = child_descriptor("c", "copy title");
    child.status = SubtaskStatus::Cancelled;
    blocks.push(block(PresentationBlockKind::Subtask {
        parent: PresentationBlockId(20),
        entry_index: 0,
        descriptor: child,
    }));
    for (id, block) in blocks.iter_mut().enumerate() {
        block.id = PresentationBlockId(id as u64);
    }
    let expected_message = blocks
        .iter()
        .map(PresentationBlock::primary_copy)
        .collect::<Vec<_>>()
        .join("\n\n");
    let copies = blocks
        .iter()
        .map(PresentationBlock::readable_list_copy)
        .collect::<Vec<_>>();
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks,
    }));
    for (index, copy) in copies.into_iter().enumerate() {
        app.select_for_test(cursor(0, index));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('Y'))),
            copy.map(|text| UiAction::Copy { text })
        );
    }
    app.select_message_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: expected_message
        })
    );
    assert_eq!(app.handle_event(key(KeyCode::Char('Y'))), None);
}

#[test]
fn uppercase_y_does_not_take_insert_redo_or_dialog_ownership() {
    let mut app = App::new();
    enter_insert(&mut app);
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
        None
    );
    assert_eq!(app.input(), "Y");
    app.handle_event(ctrl('z'));
    assert_eq!(app.input(), "");
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('Y'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input(), "Y");
    let artifact = test_plan_artifact();
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    // Existing plan-dialog binding is lowercase y (including Ctrl+y).
    assert_eq!(app.handle_event(key(KeyCode::Char('Y'))), None);
    assert_eq!(
        app.handle_event(ctrl('y')),
        Some(UiAction::Copy {
            text: artifact.markdown
        })
    );
}

#[test]
fn hosted_headers_share_structured_rendering_without_changing_copy_bytes() {
    for (detail, expected) in [
        (
            Some("Searching: one query".to_string()),
            "Searching: one query ✓ · ✗",
        ),
        (None, "Web actions · 3 · 2 ✓ · 1 ✗"),
    ] {
        let activity = WebActivityPresentation {
            detail,
            members: vec![1, 2, 3],
            outcomes: vec![(StatusIcon::Done, 2), (StatusIcon::Failed, 1)],
        };
        let block = block(PresentationBlockKind::WebActivity(activity));
        assert_eq!(block.primary_copy(), expected);
        let lines = render(&block, 80, false, false);
        assert_eq!(lines[0].to_string(), format!("◆ {expected}"));
        assert_icon(&lines, StatusIcon::Done);
        assert_icon(&lines, StatusIcon::Failed);
    }
    let hosted = block(PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(
        AcpToolPresentation {
            metadata: None,
            id: "hosted-search:test:0".into(),
            title: "Web".into(),
            kind: "search".into(),
            status: "failed".into(),
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(
                json!({"origin": "provider_hosted_web_search", "action": {"type": "search", "query": "query"}}),
            ),
            raw_output: None,
        },
    ))));
    let copy = hosted.primary_copy();
    let lines = render(&hosted, 80, false, false);
    assert_eq!(lines[0].to_string(), format!("◆ {copy}"));
    assert_icon(&lines, StatusIcon::Failed);
    assert!(!copy.contains('◆'));
}

#[test]
fn multiline_hosted_details_keep_line_breaks_one_prefix_and_final_outcome() {
    let tool = AcpToolPresentation {
        metadata: None,
        id: "hosted-search:test:0".into(),
        title: "Web".into(),
        kind: "search".into(),
        status: "completed".into(),
        content: Vec::new(),
        locations: Vec::new(),
        raw_output: None,
        raw_input: Some(
            json!({"origin": "provider_hosted_web_search", "action": {"type": "search", "query": "first\n\nsecond\n"}}),
        ),
    };
    for block in [
        block(PresentationBlockKind::WebActivity(
            tool.hosted_activity().unwrap(),
        )),
        block(PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(
            tool,
        )))),
    ] {
        let copy = "Web search: first\n\nsecond\n ✓";
        assert_eq!(block.primary_copy(), copy);
        let lines = render(&block, 80, false, false);
        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            ["◆ Web search: first", "", "second", " ✓"]
        );
        assert_icon(&lines[3..], StatusIcon::Done);
        for width in [1, 5, 12, 40, 80] {
            let folded = render(&block, width, false, true);
            assert_eq!(crate::layout::prepare::wrapped_height(&folded, width), 1);
            assert_icon(&folded, StatusIcon::Done);
        }
        assert_eq!(block.primary_copy(), copy);
    }
}

#[test]
fn typed_folds_preserve_outcomes_geometry_selection_and_copy_across_cache_paths() {
    let long = "very-long-界e\u{301}👩‍💻-".repeat(25);
    let mut child = child_descriptor("child", &long);
    child.status = SubtaskStatus::Cancelled;
    let blocks = vec![
        native(
            "command",
            json!({"command": format!("echo {long}\nprintf final-line")}),
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Error),
        ),
        native(
            "edit",
            json!({"file_path": long}),
            ToolCallStatus::Interrupted,
            None,
        ),
        native(
            "task",
            task_arguments(),
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Success),
        ),
        block(PresentationBlockKind::Subtask {
            parent: PresentationBlockId(9),
            entry_index: 0,
            descriptor: child,
        }),
        block(PresentationBlockKind::WebActivity(
            WebActivityPresentation {
                members: (0..8).collect(),
                detail: None,
                outcomes: vec![
                    (StatusIcon::Done, 100),
                    (StatusIcon::Dismissed, 200),
                    (StatusIcon::Updated, 300),
                    (StatusIcon::Pending, 400),
                    (StatusIcon::Running, 500),
                    (StatusIcon::Interrupted, 600),
                    (StatusIcon::Denied, 700),
                    (StatusIcon::Failed, 800),
                ],
            },
        )),
    ];
    for (block, icon) in blocks.into_iter().zip([
        StatusIcon::Failed,
        StatusIcon::Interrupted,
        StatusIcon::Updated,
        StatusIcon::Interrupted,
        StatusIcon::Failed,
    ]) {
        let primary = block.primary_copy();
        let secondary = block.secondary_copy();
        let readable = block.readable_list_copy();
        for message in [false, true] {
            let mut folds = FoldState::default();
            folds.fold(if message {
                FoldKey::Message { history_index: 0 }
            } else {
                FoldKey::Block {
                    history_index: 0,
                    id: block.id,
                }
            });
            for width in [0, 1, 2, 5, 12, 40, 80] {
                let history = [HistoryEntry::Conversation(ConversationEntry {
                    header: None,
                    blocks: vec![block.clone()],
                })];
                let mut cache = ConversationCache::default();
                // Populate an expanded cache before folding a whole message.
                cache.refresh(&history, None, width, false, &FoldState::default());
                cache.refresh(&history, None, width, false, &folds);
                let entry = &cache.entries()[0];
                if width == 0 {
                    assert_eq!(entry.items[0].1.len(), 0);
                    continue;
                }
                assert_eq!(entry.items[0].1.len(), 1, "width {width}");
                let summary = entry.lines.last().unwrap();
                assert!(
                    summary.width() <= usize::from(width),
                    "{width}: {summary:?}"
                );
                assert_icon(std::slice::from_ref(summary), icon);
                assert!(!summary.to_string().contains('\n'));
                if width < 5 {
                    assert!(!summary.to_string().contains('◆'));
                }
                if width == 80 {
                    assert!(summary.to_string().starts_with("▸ ◆ "));
                }
                let rebuilds = cache.block_rebuilds;
                cache.refresh(&history, None, width, false, &folds);
                assert_eq!(cache.block_rebuilds, rebuilds);
                let selection = ActiveSelection {
                    selection: Selection {
                        history_index: 0,
                        content_index: 0,
                    },
                    scope: if message {
                        SelectionScope::Message
                    } else {
                        SelectionScope::Block
                    },
                };
                cache.refresh(&history, Some(selection), width, false, &folds);
                let entry = &cache.entries()[0];
                assert_eq!(entry.selection, Some(entry.items[0].1));
                assert!(
                    entry
                        .lines
                        .last()
                        .unwrap()
                        .spans
                        .iter()
                        .all(|span| span.style.fg == Some(ZEVRIA_DARK.surfaces.canvas))
                );
            }
        }
        assert_eq!(block.primary_copy(), primary);
        assert_eq!(block.secondary_copy(), secondary);
        assert_eq!(block.readable_list_copy(), readable);
    }
}

#[test]
fn launch_notice_folds_keep_missing_counts_statusless_and_hidden_batches_hidden() {
    let mut launch = native(
        "launch_subtasks",
        two_child_launch_arguments(),
        ToolCallStatus::Finished,
        Some(ToolCallOutcome::Error),
    );
    launch
        .native_tool_mut()
        .unwrap()
        .1
        .subtasks
        .insert(0, child_descriptor("first", "first child"));
    let before = launch.primary_copy();
    assert_eq!(
        render(&launch, 80, false, false)[0].to_string(),
        "◆ 1 of 2 subtasks not launched"
    );
    for width in [1, 2, 5, 12, 40, 80] {
        let lines = render(&launch, width, false, true);
        assert_eq!(crate::layout::prepare::wrapped_height(&lines, width), 1);
        assert!(lines[0].width() <= usize::from(width));
        let summary = lines[0].to_string();
        for icon in ["○", "◐", "✓", "✗", "⊘", "◼", "–", "•"] {
            assert!(!summary.contains(icon));
        }
        if width >= 12 {
            assert!(summary.contains("1 of 2"));
        }
    }
    assert_eq!(launch.primary_copy(), before);
    assert!(launch.visible(false));
    launch.native_tool_mut().unwrap().1.status = ToolCallStatus::Executing;
    assert!(!launch.visible(false));
    let (_, state) = launch.native_tool_mut().unwrap();
    state.status = ToolCallStatus::Finished;
    state
        .subtasks
        .insert(1, child_descriptor("second", "second child"));
    assert!(!launch.visible(false));
}

#[test]
fn acp_tool_and_hosted_early_return_folds_retain_typed_status_in_single_block_messages() {
    for hosted in [false, true] {
        let tool = AcpToolPresentation {
            metadata: None,
            id: if hosted {
                "hosted-search:test:0"
            } else {
                "ordinary"
            }
            .into(),
            title: "an unusually long ACP title ".repeat(12),
            kind: "search".into(),
            status: "denied".into(),
            content: vec!["full output".into()],
            locations: Vec::new(),
            raw_input: Some(if hosted {
                json!({"origin":"provider_hosted_web_search", "action":{"type":"search", "query":"long query ".repeat(25)}})
            } else {
                json!("original raw arguments")
            }),
            raw_output: None,
        };
        let block = block(PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(
            tool,
        ))));
        let primary = block.primary_copy();
        let secondary = block.secondary_copy();
        let history = [HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![block.clone()],
        })];
        for message in [false, true] {
            let mut folds = FoldState::default();
            folds.fold(if message {
                FoldKey::Message { history_index: 0 }
            } else {
                FoldKey::Block {
                    history_index: 0,
                    id: block.id,
                }
            });
            for width in [1, 2, 5, 12, 40, 80] {
                let mut cache = ConversationCache::default();
                cache.set_appearance(crate::presentation::TranscriptAppearance::Acp);
                cache.refresh(&history, None, width, false, &FoldState::default());
                cache.refresh(&history, None, width, false, &folds);
                let entry = &cache.entries()[0];
                assert_eq!(entry.items[0].1.len(), 1);
                let summary = entry.lines.last().unwrap();
                assert!(summary.width() <= usize::from(width));
                assert_icon(std::slice::from_ref(summary), StatusIcon::Denied);
            }
        }
        assert_eq!(block.primary_copy(), primary);
        assert_eq!(block.secondary_copy(), secondary);
    }
}

#[test]
fn grouped_folds_prioritize_each_non_success_before_done_and_keep_full_groups_when_possible() {
    for icon in [
        StatusIcon::Failed,
        StatusIcon::Denied,
        StatusIcon::Interrupted,
        StatusIcon::Running,
        StatusIcon::Pending,
        StatusIcon::Updated,
        StatusIcon::Dismissed,
    ] {
        let block = block(PresentationBlockKind::WebActivity(
            WebActivityPresentation {
                detail: None,
                members: vec![1, 2],
                outcomes: vec![(StatusIcon::Done, 999), (icon, 2)],
            },
        ));
        let full = block.primary_copy();
        assert_eq!(render(&block, 1, false, true)[0].to_string(), icon.glyph(0));
        assert_eq!(
            render(&block, 2, false, true)[0].to_string(),
            format!("{}…", icon.glyph(0))
        );
        let fit = render(&block, 20, false, true)[0].to_string();
        assert!(
            fit.contains(&format!("999 ✓ · 2 {}", icon.glyph(0))),
            "{fit}"
        );
        assert_eq!(block.primary_copy(), full);
    }
}
