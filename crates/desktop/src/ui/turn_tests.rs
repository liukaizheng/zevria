use super::*;
use crate::ui::blocks::{BlockData, BlockKind};
use gpui_kit::{AppContext as _, Focusable as _, MenuItem, Role, size};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Reasoning, Text, ToolCall, ToolFunction, ToolName,
};
use zevria_presentation::fold::FoldKey;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanRecord;

fn thinking(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
        Reasoning::new(text),
    )]))
}
fn answer(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::Reasoning(Reasoning::new("Checked the result\n\nDetailed evidence")),
        AssistantContent::Reasoning(Reasoning::new("Checked the logs\n\nDetailed trace")),
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
        TranscriptItem::Message(answer("Second final answer\n\nFull answer detail")),
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
        .filter(|b| matches!(b.kind, BlockKind::RangeSummary))
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
fn focus(
    window: Window,
    view: &Entity<DesktopWindow>,
    block: &BlockData,
    target: FoldKey,
    cx: &mut TestAppContext,
) {
    cx.update_window(window.into(), |_, window, cx| {
        let focus = view
            .read(cx)
            .transcript
            .read(cx)
            .test_focus(&block.key, target, cx);
        window.focus(&focus, cx);
    })
    .unwrap();
}
fn dispatch<A: gpui_kit::Action>(window: Window, action: A, cx: &mut TestAppContext) {
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_action(Box::new(action), cx)
    })
    .unwrap();
    render(window, cx);
}
fn scroll_row(view: &Entity<DesktopWindow>, row: usize, cx: &mut TestAppContext) {
    transcript(view, cx).update(cx, |view, cx| {
        view.scroller()
            .update(cx, |list, cx| list.scroll_to_item(row, cx));
    });
}

#[gpui_kit::test]
fn view_actions_are_one_shot_and_keep_the_latest_answer_expanded(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(
        &controller,
        restored_items().into_iter().take(10).collect(),
        cx,
    );
    render(window, cx);
    let original = blocks(&view, cx);
    assert!(original.iter().all(|b| b.expanded));
    focus_composer(window, &view, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("A draft to keep", cx)
    })
    .unwrap();
    press(window, "alt-cmd-left", cx);
    assert!(
        blocks(&view, cx).iter().all(|b| b.expanded),
        "no local target in the composer"
    );
    press(window, "shift-cmd-r", cx);
    let runs = summaries(&view, cx);
    assert_eq!(
        runs.iter().map(|b| b.body.as_str()).collect::<Vec<_>>(),
        ["3 earlier messages", "3 earlier messages"]
    );
    let data = blocks(&view, cx);
    for index in [0, 5] {
        assert!(
            data.iter()
                .any(|b| b.row == index && b.message.is_some_and(|m| m.folded))
        );
    }
    for index in [4, 9] {
        assert!(
            data.iter()
                .any(|b| b.row == index && b.message.is_some_and(|m| !m.folded))
        );
        let thoughts = data
            .iter()
            .filter(|b| b.row == index && matches!(b.kind, BlockKind::Reasoning))
            .collect::<Vec<_>>();
        assert_eq!(thoughts.len(), 2);
        assert!(thoughts.iter().all(|b| b.expanded == (index == 4)));
    }
    assert!(
        data.iter()
            .any(|b| b.body.contains("Full answer detail") && b.expanded)
    );
    cx.update_window(window.into(), |_, window, cx| {
        let button = window.find("fold-earlier-content");
        assert_eq!(button.label(), Some("Fold earlier content"));
        let names: Vec<_> = actions::menus()
            .into_iter()
            .flat_map(|m| m.items)
            .filter_map(|item| match item {
                MenuItem::Action { ref name, .. }
                    if [
                        "Toggle fold",
                        "Fold",
                        "Unfold",
                        "Fold earlier content",
                        "Fold older turns",
                        "Unfold all",
                    ]
                    .contains(&name.as_ref()) =>
                {
                    assert!(!item.is_checked());
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(names.len(), 6);
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
        assert_eq!(
            view.read(cx).composer.read(cx).value().as_str(),
            "A draft to keep"
        );
    })
    .unwrap();
    press(window, "alt-shift-cmd-r", cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .any(|b| b.row == 4 && b.message.is_some_and(|m| m.folded))
    );
    dispatch(window, actions::FoldEarlierContent, cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .any(|b| b.row == 4 && b.message.is_some_and(|m| m.folded)),
        "earlier-content must not undo older-turn folds"
    );
    press(window, "alt-cmd-r", cx);
    assert!(summaries(&view, cx).is_empty());
    assert_eq!(
        blocks(&view, cx)
            .iter()
            .map(|b| (&b.key, b.expanded, b.message))
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(|b| (&b.key, b.expanded, b.message))
            .collect::<Vec<_>>()
    );
}

#[gpui_kit::test]
fn row_controls_keep_scopes_distinct_and_copy_the_original_payload(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(
        &controller,
        restored_items().into_iter().take(5).collect(),
        cx,
    );
    render(window, cx);
    let original = blocks(&view, cx);
    let last = original.iter().find(|b| b.row == 4).unwrap();
    let block_key = last.target.unwrap();
    let message_key = last.message.unwrap().key;
    cx.update_window(window.into(), |_, window, cx| {
        let mut row = window.within(last.key.clone());
        let message = row.find("message-disclosure");
        let block = row.find("block-disclosure");
        for control in [&message, &block] {
            assert_eq!(control.role(), Some(Role::Button));
            assert_eq!(control.expanded(), Some(true));
            assert!(control.bounds().size.height >= px(28.));
        }
        assert_eq!(message.label(), Some("Zevria message"));
        assert_eq!(block.label(), Some("Thinking"));
        assert_eq!(block.description(), Some("Checked the result"));
        assert!(message.bounds().right() < block.bounds().left());
        row.click("block-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    assert!(
        !blocks(&view, cx)
            .iter()
            .find(|b| b.target == Some(block_key))
            .unwrap()
            .expanded
    );
    focus(window, &view, last, message_key, cx);
    dispatch(window, actions::ToggleFold, cx);
    assert_eq!(blocks(&view, cx).iter().filter(|b| b.row == 4).count(), 1);
    let copied = controller.read_with(cx, |c, _| {
        c.model
            .root
            .conversation
            .selected_message_text(4, false)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, _| {
        assert_eq!(
            window
                .within(last.key.clone())
                .find("message-disclosure")
                .expanded(),
            Some(false)
        );
    })
    .unwrap();
    focus(window, &view, last, message_key, cx);
    dispatch(window, gpui_kit::component::input::Copy, cx);
    cx.update_window(window.into(), |_, _, cx| {
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(copied.as_str())
        );
    })
    .unwrap();
    press(window, "alt-cmd-right", cx);
    assert_eq!(blocks(&view, cx).iter().filter(|b| b.row == 4).count(), 3);
    assert!(
        !blocks(&view, cx)
            .iter()
            .find(|b| b.target == Some(block_key))
            .unwrap()
            .expanded
    );
    focus(window, &view, last, block_key, cx);
    press(window, "space", cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .find(|b| b.target == Some(block_key))
            .unwrap()
            .expanded
    );
    press(window, "alt-cmd-left", cx);
    dispatch(window, gpui_kit::component::input::Copy, cx);
    cx.update_window(window.into(), |_, _, cx| {
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(last.primary.as_str())
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn range_restores_the_covered_anchor_and_focus_and_keeps_nested_folds(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Prompt")),
            TranscriptItem::Message(thinking("Earlier thought\n\nMore evidence")),
            TranscriptItem::Message(Message::assistant("Intermediate paragraph.\n\n".repeat(40))),
            TranscriptItem::Message(Message::assistant("Final response.\n\n".repeat(70))),
        ],
        cx,
    );
    scroll_row(&view, 2, cx);
    render(window, cx);
    let prior = blocks(&view, cx).into_iter().find(|b| b.row == 2).unwrap();
    let block_key = prior.target.unwrap();
    let identity =
        transcript(&view, cx).read_with(cx, |view, cx| view.test_identity(&prior.key, cx));
    focus(window, &view, &prior, block_key, cx);
    press(window, "alt-cmd-left", cx);
    assert_eq!(top(&view, cx), (2, px(0.)));
    press(window, "shift-cmd-r", cx);
    assert_eq!(top(&view, cx), (1, px(0.)));
    let range = summaries(&view, cx).remove(0);
    cx.update_window(window.into(), |_, window, cx| {
        let summary = window.within(range.key.clone()).find("range-disclosure");
        assert_eq!(summary.label(), Some("2 earlier messages"));
        assert_eq!(summary.role(), Some(Role::Button));
        assert_eq!(summary.expanded(), Some(false));
        assert_eq!(summary.bounds().size.height, px(28.));
        let view = view.read(cx).transcript.read(cx);
        assert!(
            view.test_focus(&range.key, range.target.unwrap(), cx)
                .is_focused(window)
        );
        assert!(window.try_find(prior.key.clone()).is_none());
    })
    .unwrap();
    dispatch(window, gpui_kit::component::input::Copy, cx);
    cx.update_window(window.into(), |_, _, cx| {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(text.contains("More evidence") && text.contains(&prior.primary));
    })
    .unwrap();
    press(window, "enter", cx);
    assert_eq!(top(&view, cx), (2, px(0.)));
    assert!(summaries(&view, cx).is_empty());
    assert!(
        !blocks(&view, cx)
            .iter()
            .find(|b| b.row == 2)
            .unwrap()
            .expanded
    );
    cx.update_window(window.into(), |_, window, cx| {
        let view = view.read(cx).transcript.read(cx);
        assert_eq!(view.test_identity(&prior.key, cx), identity);
        assert!(
            view.test_focus(&prior.key, block_key, cx)
                .is_focused(window)
        );
    })
    .unwrap();
    press(window, "alt-cmd-r", cx);
    assert!(blocks(&view, cx).iter().all(|b| b.expanded));
}

#[gpui_kit::test]
fn folded_prose_uses_actual_wrapping_and_retains_the_text_view_on_resize(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 860.);
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Fits on one line")),
            TranscriptItem::Message(Message::assistant(
                "Keep this line in full when the conversation has enough room.",
            )),
        ],
        cx,
    );
    render(window, cx);
    let original = blocks(&view, cx);
    let prompt = &original[0];
    let answer = &original[1];
    let height = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, _| {
            window.find(prompt.key.clone()).bounds().size.height
        })
        .unwrap()
    };
    let before = height(cx);
    focus(window, &view, prompt, prompt.message.unwrap().key, cx);
    press(window, "alt-cmd-left", cx);
    assert_eq!(
        height(cx),
        before,
        "a one-line message fold must not add height"
    );
    let transcript = transcript(&view, cx);
    let identity = transcript.read_with(cx, |v, cx| v.test_identity(&answer.key, cx));
    let bounds = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, _| {
            window.find(answer.key.clone()).bounds()
        })
        .unwrap()
    };
    let wide_expanded = bounds(cx);
    focus(window, &view, answer, answer.target.unwrap(), cx);
    press(window, "alt-cmd-left", cx);
    let clamped =
        |cx: &TestAppContext| transcript.read_with(cx, |v, cx| v.test_clamped(&answer.key, cx));
    assert!(
        !clamped(cx),
        "wide expanded {wide_expanded:?}, wide folded {:?}, short prompt clamped {}",
        bounds(cx),
        transcript.read_with(cx, |v, cx| v.test_clamped(&prompt.key, cx))
    );
    cx.simulate_window_resize(window.into(), size(px(720.), px(860.)));
    render(window, cx);
    press(window, "ctrl-cmd-s", cx); // The sidebar further narrows the compact transcript.
    assert!(clamped(cx));
    cx.simulate_window_resize(window.into(), size(px(1240.), px(860.)));
    render(window, cx);
    assert!(!clamped(cx));
    assert_eq!(
        transcript.read_with(cx, |v, cx| v.test_identity(&answer.key, cx)),
        identity
    );
}

#[gpui_kit::test]
fn tool_diff_totals_survive_fold_and_unfold(cx: &mut TestAppContext) {
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
    render(window, cx);
    let check_totals = |cx: &TestAppContext| {
        let tool = blocks(&view, cx)
            .into_iter()
            .find(|b| matches!(b.kind, BlockKind::Tool { .. }))
            .unwrap();
        let BlockKind::Tool { meta, .. } = &tool.kind else {
            unreachable!()
        };
        assert_eq!((meta.added, meta.removed), (2, 1));
        tool
    };
    let tool = check_totals(cx);
    focus(window, &view, &tool, tool.target.unwrap(), cx);
    press(window, "alt-cmd-left", cx);
    assert!(!check_totals(cx).expanded);
    press(window, "alt-cmd-right", cx);
    assert!(check_totals(cx).expanded);
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
fn acp_history_uses_stable_block_folds_and_no_native_header_is_required(cx: &mut TestAppContext) {
    use zevria_presentation::agents::AgentTranscriptReducer;
    use zevria_workflow::AgentRunEvent;

    let (window, view, controller, _) = setup(cx);
    let mut reducer = AgentTranscriptReducer::default();
    let answer = |text: &str| AgentRunEvent::AgentMessage {
        text: text.into(),
        message_id: Some("answer".into()),
    };
    controller.update(cx, |c, cx| {
        let conversation = &mut c.model.root.conversation;
        for event in [
            AgentRunEvent::UserMessage {
                text: "Check the test".into(),
                message_id: Some("prompt".into()),
            },
            AgentRunEvent::Thought {
                text: "Read the log\n\nThe second line is evidence".into(),
                message_id: Some("thought".into()),
            },
            answer("The test passed."),
        ] {
            reducer.apply_event(conversation, event);
        }
        cx.notify();
    });
    render(window, cx);
    dispatch(window, actions::FoldEarlierContent, cx);
    let data = blocks(&view, cx);
    let thought = data
        .iter()
        .find(|b| matches!(b.kind, BlockKind::Reasoning))
        .unwrap();
    assert!(matches!(thought.target, Some(FoldKey::Block { .. })));
    assert!(!thought.expanded);
    assert!(
        data.iter()
            .any(|b| b.body == "The test passed." && b.expanded)
    );
    assert!(data.iter().all(|b| b.message.is_none_or(|m| !m.folded)));
    assert!(summaries(&view, cx).is_empty());
    controller.update(cx, |c, cx| {
        reducer.apply_event(&mut c.model.root.conversation, answer(" No failures."));
        cx.notify();
    });
    render(window, cx);
    assert!(
        blocks(&view, cx)
            .iter()
            .any(|b| b.target == thought.target && !b.expanded)
    );
    focus(window, &view, thought, thought.target.unwrap(), cx);
    press(window, "alt-cmd-right", cx);
    assert!(blocks(&view, cx).iter().all(|b| b.expanded));
}

#[gpui_kit::test]
fn ensemble_prompt_and_worker_are_separate_item_targets(cx: &mut TestAppContext) {
    use zevria_presentation::blocks::{DisplayTurn, NativeHeader};
    use zevria_workflow::{
        AgentRunDescriptor, AgentRunId, AgentRunStatus, EnsembleRunId, EnsembleStart,
        EnsembleWorkflow,
    };

    let (window, view, controller, _) = setup(cx);
    controller.update(cx, |c, cx| {
        let conversation = &mut c.model.root.conversation;
        let run_id = EnsembleRunId::from_string("review");
        let worker_id = AgentRunId::from_string("reviewer");
        conversation.add_ensemble(
            EnsembleStart {
                run_id: run_id.clone(),
                workflow: EnsembleWorkflow::Review,
                prompt: "Check the run".into(),
                agents: vec![AgentRunDescriptor {
                    id: worker_id.clone(),
                    agent: "codex".into(),
                    label: "Reviewer".into(),
                    safe_mode: "read-only".into(),
                }],
            },
            AgentRunStatus::Queued,
            Some(NativeHeader::Prompt(DisplayTurn(1))),
        );
        conversation.update_ensemble_worker(
            &run_id,
            &worker_id,
            AgentRunStatus::Failed,
            Some("Worker output was not available".into()),
        );
        cx.notify();
    });
    render(window, cx);
    let data = blocks(&view, cx);
    assert_eq!(data.len(), 2);
    assert!(data[1].body.contains("Worker output was not available"));
    for (content_index, block) in data.iter().enumerate() {
        assert_eq!(
            block.target,
            Some(FoldKey::Item {
                history_index: 0,
                content_index
            })
        );
    }
    focus(window, &view, &data[1], data[1].target.unwrap(), cx);
    press(window, "alt-cmd-left", cx);
    let visible = blocks(&view, cx);
    assert!(visible[0].expanded && !visible[1].expanded);
    let transcript = transcript(&view, cx);
    assert!(transcript.read_with(cx, |v, cx| v.test_clamped(&visible[1].key, cx)));
    press(window, "cmd-c", cx);
    cx.update_window(window.into(), |_, _, cx| {
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(data[1].body.as_str())
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn unfold_all_is_pane_local_and_replacement_retires_old_folds(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 1240., 1800.);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    dispatch(window, actions::FoldEarlierContent, cx);
    let root = summaries(&view, cx)[0].key.clone();
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
    assert!(summaries(&view, cx).is_empty());
    dispatch(window, actions::FoldOlderTurns, cx);
    assert!(!summaries(&view, cx).is_empty());
    dispatch(window, actions::UnfoldAll, cx);
    assert!(summaries(&view, cx).is_empty());
    controller.update(cx, |c, cx| {
        c.model.selected_child = None;
        cx.notify();
    });
    render(window, cx);
    assert_eq!(summaries(&view, cx)[0].key, root);
    restore(&controller, restored_items(), cx);
    render(window, cx);
    assert!(summaries(&view, cx).is_empty());
    assert!(
        blocks(&view, cx)
            .iter()
            .all(|b| b.expanded && b.message.is_none_or(|m| !m.folded))
    );
}
