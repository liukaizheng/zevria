use super::*;
use crate::ui::blocks::{BlockData, BlockKind, Gutter, Speaker};
use gpui_kit::{AppContext as _, Focusable as _, MenuItem};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Reasoning, Text, ToolCall, ToolFunction, ToolName,
};
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanRecord;

fn thinking(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
        Reasoning::new(text),
    )]))
}
fn answer(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::Reasoning(Reasoning::new("Checked the result")),
        AssistantContent::Text(Text::new(text)),
    ]))
}
fn command() -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
        ToolCall::new(
            CallId::from_wire("turn-command"),
            ToolFunction::new(
                ToolName::new("command").unwrap(),
                serde_json::json!({"command":"rtk cargo test --release"}),
            ),
        ),
    )]))
}
fn restored_items() -> Vec<TranscriptItem> {
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: "Preserve reading position".into(),
        markdown: "Keep prompts and answers visible.".into(),
        source_turn_id: TurnId::new(1),
    };
    vec![
        TranscriptItem::Message(Message::user("First prompt")),
        TranscriptItem::Message(Message::assistant("I'll trace the source.")),
        TranscriptItem::Message(thinking("Read the supplied notes")),
        TranscriptItem::Message(command()),
        TranscriptItem::Message(answer("First final answer")),
        TranscriptItem::Message(Message::user("Second prompt")),
        TranscriptItem::Message(Message::assistant("Prepare the plan.")),
        TranscriptItem::Plan(PlanRecord::Ready { artifact }),
        TranscriptItem::Message(thinking("Check the plan")),
        TranscriptItem::Message(Message::assistant("Second final answer")),
        TranscriptItem::Message(Message::user("Third prompt")),
        TranscriptItem::Message(Message::assistant("Check the stop path.")),
        TranscriptItem::Message(thinking("Keep the partial result")),
        TranscriptItem::Message(Message::assistant("Third partial answer")),
        TranscriptItem::Error {
            error: zevria_presentation::TURN_CANCELLED.into(),
        },
    ]
}
fn restore(
    controller: &Entity<SessionController>,
    items: Vec<TranscriptItem>,
    cx: &mut TestAppContext,
) {
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(items);
        cx.notify();
    });
    cx.run_until_parked();
}
fn render(window: Window, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}
fn press(window: Window, key: &str, cx: &mut TestAppContext) {
    cx.update_window(window.into(), |_, window, cx| window.press(key, cx))
        .unwrap();
    render(window, cx);
}
fn blocks(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    view.read_with(cx, |view, cx| view.transcript.read(cx).test_data())
}
fn summaries(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    blocks(view, cx)
        .into_iter()
        .filter(|b| matches!(b.kind, BlockKind::Steps(_)))
        .collect()
}
fn transcript(
    view: &Entity<DesktopWindow>,
    cx: &TestAppContext,
) -> Entity<super::super::transcript::TranscriptView> {
    view.read_with(cx, |v, _| v.transcript.clone())
}
fn top(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> (usize, gpui_kit::Pixels) {
    transcript(view, cx).read_with(cx, |view, cx| {
        let top = view.scroller().read(cx).logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    })
}
fn focus_composer(window: Window, view: &Entity<DesktopWindow>, cx: &mut TestAppContext) {
    cx.update_window(window.into(), |_, window, cx| {
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.focus(window, cx))
        });
    })
    .unwrap();
}
fn focus_block(window: Window, view: &Entity<DesktopWindow>, key: &str, cx: &mut TestAppContext) {
    cx.update_window(window.into(), |_, window, cx| {
        let focus = view.read(cx).transcript.read(cx).test_focus(key, cx);
        window.focus(&focus, cx);
    })
    .unwrap();
}
fn scroll_row(view: &Entity<DesktopWindow>, row: usize, cx: &mut TestAppContext) {
    transcript(view, cx).update(cx, |view, cx| {
        view.scroller()
            .update(cx, |list, cx| list.scroll_to_item(row, cx));
    });
}

#[gpui_kit::test]
fn final_outputs_keep_prompts_answers_documents_errors_and_restore_exact_blocks(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    let original = blocks(&view, cx);
    focus_composer(window, &view, cx);
    press(window, "shift-cmd-r", cx);
    let data = blocks(&view, cx);
    let runs = summaries(&view, cx);
    assert_eq!(
        runs.len(),
        4,
        "the Plan artifact splits the second turn's steps"
    );
    assert!(runs.iter().all(|b| !b.expanded));
    for text in [
        "First prompt",
        "Second prompt",
        "Third prompt",
        "First final answer",
        "Second final answer",
        "Third partial answer",
        "Stopped",
    ] {
        assert!(data.iter().any(|b| b.body == text), "missing {text}");
    }
    assert!(
        data.iter()
            .any(|b| matches!(b.kind, BlockKind::Document { .. }))
    );
    assert!(!data.iter().any(|b| b.body == "Read the supplied notes"));
    assert!(
        data.iter().any(|b| b.body == "Checked the result"),
        "the final call's Thinking header stays visible"
    );
    cx.update_window(window.into(), |_, window, cx| {
        let header = window.within(runs[0].key.clone()).find("steps-disclosure");
        assert_eq!(header.role(), Some(gpui_kit::Role::Button));
        assert_eq!(header.expanded(), Some(false));
        assert_eq!(header.label(), Some("3 steps"));
        assert_eq!(header.description(), Some("1 tool call, 1 thought, 1 message"));
        assert!(header.bounds().size.height >= px(28.));
        let checked = actions::menus(view.read(cx).transcript.read(cx).final_outputs()).into_iter().flat_map(|m| m.items).find(|item| matches!(item, MenuItem::Action { name, .. } if name == "Show final outputs only")).unwrap();
        assert!(checked.is_checked());
        assert_eq!(window.find("final-outputs").label(), Some("Show final outputs only ⇧⌘R"));
    }).unwrap();
    press(window, "shift-cmd-r", cx);
    assert_eq!(
        blocks(&view, cx).iter().map(|b| &b.key).collect::<Vec<_>>(),
        original.iter().map(|b| &b.key).collect::<Vec<_>>()
    );
    assert_eq!(
        blocks(&view, cx)
            .iter()
            .map(|b| b.expanded)
            .collect::<Vec<_>>(),
        original.iter().map(|b| b.expanded).collect::<Vec<_>>()
    );
}

#[gpui_kit::test]
fn summary_disclosures_are_independent_keep_one_label_and_preserve_row_gap(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    let original = blocks(&view, cx);
    let tool = original
        .iter()
        .find(|b| matches!(b.kind, BlockKind::Tool { .. }))
        .unwrap()
        .key
        .clone();
    let final_thought = original
        .iter()
        .find(|b| b.body == "Checked the result")
        .unwrap()
        .key
        .clone();
    let ordinary_gap = cx
        .update_window(window.into(), |_, window, _| {
            window.find(final_thought.clone()).bounds().top() - window.find(tool).bounds().bottom()
        })
        .unwrap();
    press(window, "shift-cmd-r", cx);
    let first = summaries(&view, cx)[0].key.clone();
    cx.update_window(window.into(), |_, window, cx| {
        let gap = window.find(final_thought).bounds().top()
            - window.find(first.clone()).bounds().bottom();
        assert_eq!(gap, ordinary_gap);
        assert_eq!(gap, px(28.));
        window.within(first.clone()).click("steps-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    assert!(summaries(&view, cx)[0].expanded);
    assert!(summaries(&view, cx)[1..].iter().all(|b| !b.expanded));
    let data = blocks(&view, cx);
    let first_run = data
        .iter()
        .skip_while(|b| b.key != first)
        .take_while(|b| b.body != "Checked the result");
    assert_eq!(
        first_run
            .filter(|b| b.gutter == Gutter::Label(Speaker::Zevria))
            .count(),
        1
    );
    press(window, "enter", cx);
    assert!(!summaries(&view, cx)[0].expanded);
    press(window, "space", cx);
    assert!(summaries(&view, cx)[0].expanded);
    press(window, "alt-cmd-left", cx);
    assert!(!summaries(&view, cx)[0].expanded);
    press(window, "alt-cmd-right", cx);
    assert!(summaries(&view, cx)[0].expanded);
    press(window, "shift-cmd-r", cx);
    press(window, "shift-cmd-r", cx);
    assert!(
        summaries(&view, cx).iter().all(|b| !b.expanded),
        "mode toggles retire run choices"
    );
}

#[gpui_kit::test]
fn collapse_rescues_focus_and_the_live_top_anchor_to_the_summary(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Prompt")),
            TranscriptItem::Message(thinking("Focus this step")),
            TranscriptItem::Message(Message::assistant("Intermediate paragraph.\n\n".repeat(40))),
            TranscriptItem::Message(Message::assistant("Final response.\n\n".repeat(70))),
        ],
        cx,
    );
    scroll_row(&view, 1, cx);
    render(window, cx);
    let thought = blocks(&view, cx)
        .into_iter()
        .find(|b| b.body == "Focus this step")
        .unwrap()
        .key;
    focus_block(window, &view, &thought, cx);
    scroll_row(&view, 2, cx);
    render(window, cx);
    assert_eq!(top(&view, cx), (2, px(0.)));
    // Invoke the menu's operation directly: the focused virtualized step is offscreen.
    cx.update_window(window.into(), |_, window, cx| {
        view.read(cx).transcript.clone().update(cx, |view, cx| {
            view.set_final_outputs(true, window, cx);
        });
    })
    .unwrap();
    render(window, cx);
    let summary = summaries(&view, cx)[0].key.clone();
    assert_eq!(top(&view, cx), (1, px(0.)));
    cx.update_window(window.into(), |_, window, cx| {
        let transcript = view.read(cx).transcript.read(cx);
        assert!(transcript.test_focus(&summary, cx).is_focused(window));
        assert!(!transcript.test_focus(&thought, cx).is_focused(window));
    })
    .unwrap();
    press(window, "alt-cmd-right", cx);
    scroll_row(&view, 2, cx);
    render(window, cx);
    focus_block(window, &view, &summary, cx);
    cx.update_window(window.into(), |_, window, cx| {
        view.read(cx).transcript.clone().update(cx, |view, cx| {
            view.disclose(false, false, false, window, cx)
        });
    })
    .unwrap();
    render(window, cx);
    assert_eq!(top(&view, cx), (1, px(0.)));
}

#[gpui_kit::test]
fn step_folds_preserve_tool_thinking_intent_and_composer_targets_visible_thinking(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    let original = blocks(&view, cx);
    let tool = original
        .iter()
        .find(|b| matches!(b.kind, BlockKind::Tool { .. }))
        .unwrap()
        .key
        .clone();
    let hidden = original
        .iter()
        .find(|b| b.body == "Read the supplied notes")
        .unwrap()
        .key
        .clone();
    focus_block(window, &view, &tool, cx);
    press(window, "alt-cmd-right", cx);
    focus_block(window, &view, &hidden, cx);
    press(window, "alt-cmd-right", cx);
    focus_composer(window, &view, cx);
    press(window, "shift-cmd-r", cx);
    press(window, "alt-cmd-right", cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .find(|b| b.body == "Checked the result")
            .unwrap()
            .expanded
    );
    press(window, "shift-cmd-r", cx);
    let data = blocks(&view, cx);
    assert!(data.iter().find(|b| b.key == tool).unwrap().expanded);
    assert!(data.iter().find(|b| b.key == hidden).unwrap().expanded);
    assert!(
        !data
            .iter()
            .find(|b| b.body == "Keep the partial result")
            .unwrap()
            .expanded,
        "composer must not expand hidden thinking"
    );
    press(window, "shift-cmd-r", cx);
    press(window, "alt-shift-cmd-right", cx);
    press(window, "shift-cmd-r", cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Reasoning))
            .all(|b| b.expanded),
        "expand-all still covers hidden thinking"
    );
}

#[gpui_kit::test]
fn summary_counts_and_file_totals_use_rendered_blocks(cx: &mut TestAppContext) {
    use zevria_foundation::{
        FileChange, FileChangeOutput, ToolCallOutcome, ToolResultDetail, ToolResultMetadata,
    };
    let (window, view, controller, _) = setup(cx);
    let edit = Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
        ToolCall::new(
            CallId::from_wire("edit"),
            ToolFunction::new(
                ToolName::new("edit").unwrap(),
                serde_json::json!({"file_path":"test.rs"}),
            ),
        ),
    )]));
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Change the file")),
            TranscriptItem::Message(edit),
            TranscriptItem::ToolResults {
                message: Message::tool_result(
                    CallId::from_wire("edit"),
                    ToolName::new("edit").unwrap(),
                    "Changed",
                ),
                metadata: vec![ToolResultMetadata {
                    id: "edit".into(),
                    call_id: None,
                    tool_name: "edit".into(),
                    outcome: ToolCallOutcome::Success,
                    diagnostic: None,
                    detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
                        path: "test.rs".into(),
                        change: FileChange::Update {
                            unified_diff:
                                "--- a/test.rs\n+++ b/test.rs\n@@ -1 +1,2 @@\n-old\n+new\n+test\n"
                                    .into(),
                            move_path: None,
                        },
                    }])),
                }],
                skill_applications: vec![],
            },
            TranscriptItem::Message(Message::assistant("Updated")),
        ],
        cx,
    );
    press(window, "shift-cmd-r", cx);
    let summary = summaries(&view, cx).remove(0);
    let BlockKind::Steps(counts) = summary.kind else {
        unreachable!()
    };
    assert_eq!((counts.steps(), counts.added, counts.removed), (1, 2, 1));
    assert_eq!(counts.title(), "1 step");
    assert_eq!(counts.description(), "1 tool call");
}

fn start(controller: &Entity<SessionController>, id: TurnId, cx: &mut TestAppContext) {
    event(
        controller,
        E::TurnStarted {
            turn_id: id,
            mode: SessionMode::Build,
            message: Message::user("Live prompt"),
        },
        cx,
    );
    event(
        controller,
        E::ModelCallStarted {
            turn_id: id,
            call: 1,
        },
        cx,
    );
}
fn intermediate(
    controller: &Entity<SessionController>,
    id: TurnId,
    message: Message,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::Intermediate {
            turn_id: id,
            message,
            attempt_id: None,
        },
        cx,
    );
}
#[gpui_kit::test]
fn live_step_choices_survive_commits_but_not_the_next_turn(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    press(window, "shift-cmd-r", cx);
    let id = TurnId::new(201);
    start(&controller, id, cx);
    intermediate(&controller, id, Message::assistant("Trace"), cx);
    intermediate(&controller, id, thinking("Check"), cx);
    render(window, cx);
    let key = summaries(&view, cx)[0].key.clone();
    assert!(summaries(&view, cx)[0].expanded);
    focus_block(window, &view, &key, cx);
    press(window, "alt-cmd-left", cx);
    intermediate(&controller, id, command(), cx);
    assert!(!summaries(&view, cx)[0].expanded);
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: Message::assistant("Done"),
            attempt_id: None,
        },
        cx,
    );
    assert!(!summaries(&view, cx)[0].expanded);
    let next = TurnId::new(202);
    start(&controller, next, cx);
    intermediate(&controller, next, Message::assistant("New trace"), cx);
    intermediate(&controller, next, thinking("New check"), cx);
    let summaries = summaries(&view, cx);
    assert!(summaries.last().unwrap().expanded);
    assert_ne!(summaries.last().unwrap().key, key);
}

#[gpui_kit::test]
fn untouched_live_runs_wait_for_tail_following_before_folding(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    press(window, "shift-cmd-r", cx);
    let id = TurnId::new(203);
    start(&controller, id, cx);
    intermediate(
        &controller,
        id,
        Message::assistant("Progress.\n\n".repeat(40)),
        cx,
    );
    intermediate(&controller, id, thinking(&"Evidence.\n\n".repeat(40)), cx);
    intermediate(
        &controller,
        id,
        Message::assistant("Current response.\n\n".repeat(40)),
        cx,
    );
    render(window, cx);
    scroll_row(&view, 1, cx);
    render(window, cx);
    let before = top(&view, cx);
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: Message::assistant("Final response.\n\n".repeat(40)),
            attempt_id: None,
        },
        cx,
    );
    render(window, cx);
    assert_eq!(top(&view, cx), before);
    assert!(summaries(&view, cx)[0].expanded);
    focus_composer(window, &view, cx);
    press(window, "alt-shift-cmd-down", cx);
    assert!(!summaries(&view, cx)[0].expanded);
    assert!(transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail()));
}

#[gpui_kit::test]
fn turn_shortcuts_from_composer_chain_strict_boundaries_without_changing_draft_or_focus(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    let mut items = Vec::new();
    for index in 0..3 {
        items.extend([
            TranscriptItem::Message(Message::user(format!("Prompt {index}"))),
            TranscriptItem::Message(Message::assistant("Intermediate")),
            TranscriptItem::Message(Message::assistant(
                format!("Answer {index}.\n\n").repeat(80),
            )),
        ]);
    }
    restore(&controller, items, cx);
    render(window, cx);
    focus_composer(window, &view, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("untouched draft", cx)
    })
    .unwrap();
    render(window, cx);
    assert!(top(&view, cx).0 >= 6);
    press(window, "alt-cmd-up", cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    // A second exact-boundary press skips the current turn start.
    press(window, "alt-cmd-up", cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, "alt-cmd-down", cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    press(window, "alt-cmd-down", cx);
    assert_eq!(top(&view, cx), (6, px(0.)), "no wrap at the last turn");
    scroll_row(&view, 5, cx);
    render(window, cx);
    press(window, "alt-cmd-up", cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, "alt-shift-cmd-up", cx);
    assert_eq!(top(&view, cx), (0, px(0.)));
    press(window, "alt-cmd-up", cx);
    assert_eq!(top(&view, cx), (0, px(0.)));
    // Both presses happen before layout: logical top updates immediately.
    cx.update_window(window.into(), |_, window, cx| {
        window.press("alt-cmd-down", cx);
        window.press("alt-cmd-down", cx);
    })
    .unwrap();
    render(window, cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    assert!(
        !transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail())
    );
    press(window, "shift-cmd-r", cx);
    press(window, "alt-cmd-up", cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, "alt-cmd-down", cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    press(window, "alt-shift-cmd-down", cx);
    assert!(transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail()));
    cx.update_window(window.into(), |_, window, cx| {
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
        assert_eq!(
            view.read(cx).composer.read(cx).value().as_str(),
            "untouched draft"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn projection_replacement_and_inspection_keep_the_mode_but_retire_run_choices(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(&controller, restored_items(), cx);
    press(window, "shift-cmd-r", cx);
    let root = summaries(&view, cx)[0].key.clone();
    focus_block(window, &view, &root, cx);
    press(window, "alt-cmd-right", cx);
    let id = zevria_foundation::SubtaskId::new("turn-child");
    controller.update(cx, |c, cx| {
        let mut model = zevria_presentation::SessionModel::default();
        model.conversation.restore(restored_items());
        c.model.children.push(crate::session::ChildPane {
            id: id.clone(),
            title: "Trace the reader".into(),
            workspace: None,
            kind: zevria_foundation::SubtaskKind::Explore,
            status: Some(zevria_foundation::SubtaskStatus::Completed),
            model,
        });
        c.model.selected_child = Some(id);
        cx.notify();
    });
    render(window, cx);
    assert!(summaries(&view, cx).iter().all(|b| !b.expanded));
    let child = summaries(&view, cx)[0].key.clone();
    focus_block(window, &view, &child, cx);
    press(window, "alt-cmd-right", cx);
    // Changing mode in the child must retire the parked root's choices too.
    press(window, "shift-cmd-r", cx);
    press(window, "shift-cmd-r", cx);
    controller.update(cx, |c, cx| {
        c.model.selected_child = None;
        cx.notify();
    });
    render(window, cx);
    assert!(!summaries(&view, cx)[0].expanded);
    focus_block(window, &view, &root, cx);
    press(window, "alt-cmd-right", cx);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    assert!(transcript(&view, cx).read_with(cx, |v, _| v.final_outputs()));
    assert!(summaries(&view, cx).iter().all(|b| !b.expanded));
    assert_ne!(summaries(&view, cx)[0].key, root);
}
