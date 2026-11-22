use super::*;
use crate::timeline::{BlockKind, Step, TimelineRow, tools::ToolDetail};
use gpui_kit::{AppContext as _, Focusable as _};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Reasoning, ToolCall, ToolFunction, ToolName,
};
use zevria_transcript::transcript::TranscriptItem;

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
        window.input("untouched draft", cx);
        window.press("left", cx);
        assert_eq!(view.read(cx).composer.read(cx).cursor(), 14);
    })
    .unwrap();
    render(window, cx);
    assert!(top(&view, cx).0 >= 6);
    press(window, Shortcut::PREVIOUS_TURN.0, cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    // A second exact-boundary press skips the current turn start.
    press(window, Shortcut::PREVIOUS_TURN.0, cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, Shortcut::NEXT_TURN.0, cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    press(window, Shortcut::NEXT_TURN.0, cx);
    assert_eq!(top(&view, cx), (6, px(0.)), "no wrap at the last turn");
    scroll_row(&view, 5, cx);
    render(window, cx);
    press(window, Shortcut::PREVIOUS_TURN.0, cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, Shortcut::JUMP_TO_BEGINNING.0, cx);
    assert_eq!(top(&view, cx), (0, px(0.)));
    press(window, Shortcut::PREVIOUS_TURN.0, cx);
    assert_eq!(top(&view, cx), (0, px(0.)));
    // Both presses happen before layout: logical top updates immediately.
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::NEXT_TURN.0, cx);
        window.press(Shortcut::NEXT_TURN.0, cx);
    })
    .unwrap();
    render(window, cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    assert!(
        !transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail())
    );
    press(window, Shortcut::COLLAPSE_ALL.0, cx);
    press(window, Shortcut::PREVIOUS_TURN.0, cx);
    assert_eq!(top(&view, cx), (3, px(0.)));
    press(window, Shortcut::NEXT_TURN.0, cx);
    assert_eq!(top(&view, cx), (6, px(0.)));
    press(window, Shortcut::JUMP_TO_LATEST.0, cx);
    assert!(transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail()));
    cx.update_window(window.into(), |_, window, cx| {
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
        assert_eq!(
            view.read(cx).composer.read(cx).value().as_str(),
            "untouched draft"
        );
        assert_eq!(view.read(cx).composer.read(cx).cursor(), 14);
    })
    .unwrap();
}

fn rows(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<TimelineRow> {
    transcript(view, cx).read_with(cx, |view, cx| view.test_data(cx))
}

fn command() -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
        ToolCall::new(
            CallId::from_wire("cmd"),
            ToolFunction::new(
                ToolName::new("command").unwrap(),
                serde_json::json!({"command":"git status --short"}),
            ),
        ),
    )]))
}

fn command_row(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> TimelineRow {
    rows(view, cx)
        .into_iter()
        .find(|row| matches!(row.data.kind, BlockKind::Step(Step::Tool(_))))
        .unwrap()
}

#[gpui_kit::test]
fn running_command_collapses_to_status_and_copies_original_input_and_output(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(20);
    event(
        &controller,
        E::TurnStarted {
            turn_id: id,
            mode: SessionMode::Build,
            message: Message::user("Check the worktree"),
        },
        cx,
    );
    event(
        &controller,
        E::ModelCallStarted {
            turn_id: id,
            call: 1,
        },
        cx,
    );
    event(
        &controller,
        E::Intermediate {
            turn_id: id,
            message: command(),
            attempt_id: None,
        },
        cx,
    );
    render(window, cx);
    let running = command_row(&view, cx);
    assert!(running.expanded);
    let payload = "exit_code: 101\nduration: 1.8s\n<stdout>\n M Cargo.toml\n</stdout>\n<stderr>\npermission denied\n</stderr>";
    event(
        &controller,
        E::ToolResults {
            turn_id: id,
            metadata: vec![],
            message: Message::tool_result(
                CallId::from_wire("cmd"),
                ToolName::new("command").unwrap(),
                payload,
            ),
        },
        cx,
    );
    render(window, cx);
    let finished = command_row(&view, cx);
    assert_eq!(running.data.key, finished.data.key);
    assert!(!finished.expanded);
    let BlockKind::Step(Step::Tool(tool)) = &finished.data.kind else {
        unreachable!()
    };
    let ToolDetail::Command {
        duration,
        exit,
        lines,
        ..
    } = &tool.detail
    else {
        unreachable!()
    };
    assert_eq!(
        (duration.as_deref(), exit.as_deref(), *lines),
        (Some("1.8s"), Some("exit 101"), Some(2))
    );
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("command-stdout").is_none());
        assert_eq!(
            window.find("toggle-details").label(),
            Some("Expand details")
        );
        window
            .within(finished.data.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert_eq!(
            window.find("toggle-details").label(),
            Some("Collapse details")
        );
        assert!(window.try_find("command-stdout").is_some());
        assert!(window.try_find("command-stderr").is_some());
        assert!(window.try_find("command-input").is_none());
        window.press(Shortcut::COPY_DISCLOSURE.0, cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
            Some(finished.data.primary_copy().as_str())
        );
        let mut row = window.within(finished.data.key.clone());
        row.hover("block-disclosure", cx);
        row.click("copy-output", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
            Some(payload)
        );
    })
    .unwrap();
    assert!(command_row(&view, cx).expanded);
    event(
        &controller,
        E::ModelCallStarted {
            turn_id: id,
            call: 2,
        },
        cx,
    );
    event(
        &controller,
        E::AssistantStreamUpdated {
            turn_id: id,
            snapshot: zevria_content::AssistantStreamSnapshot {
                message: Some(Message::assistant("I'll report the result.")),
                attempt: None,
            },
        },
        cx,
    );
    assert!(command_row(&view, cx).expanded);
    let child = zevria_foundation::SubtaskId::new("child");
    controller.update(cx, |c, cx| {
        c.model.children.push(crate::session::ChildPane {
            id: child.clone(),
            title: "Review".into(),
            workspace: None,
            kind: zevria_foundation::SubtaskKind::Explore,
            status: Some(zevria_foundation::SubtaskStatus::Completed),
            model: Default::default(),
        });
        c.select_pane(Some(child.clone()), cx);
    });
    render(window, cx);
    assert!(rows(&view, cx).is_empty());
    controller.update(cx, |c, cx| c.select_pane(None, cx));
    render(window, cx);
    assert!(command_row(&view, cx).expanded);
    press(window, Shortcut::COLLAPSE_ALL.0, cx);
    assert!(!command_row(&view, cx).expanded);
    press(window, Shortcut::EXPAND_ALL.0, cx);
    assert!(command_row(&view, cx).expanded);
    restore(&controller, vec![TranscriptItem::Message(command())], cx);
    assert!(
        !command_row(&view, cx).expanded,
        "replacement clears disclosure overrides"
    );
}

#[gpui_kit::test]
fn detail_shortcuts_change_only_thoughts_and_the_focused_step(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Check the worktree")),
            TranscriptItem::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("Read status")),
            ]))),
            TranscriptItem::Message(command()),
            TranscriptItem::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("Review status output")),
            ]))),
            TranscriptItem::Message(Message::assistant("Worktree checked.")),
        ],
        cx,
    );
    render(window, cx);
    assert!(
        rows(&view, cx)
            .iter()
            .filter(|r| r.data.is_disclosure())
            .all(|r| !r.expanded)
    );
    let thought_states = |cx: &TestAppContext| {
        rows(&view, cx)
            .into_iter()
            .filter(|r| r.data.thought().is_some())
            .map(|r| r.expanded)
            .collect::<Vec<_>>()
    };
    press(window, Shortcut::EXPAND_THINKING.0, cx);
    assert_eq!(thought_states(cx), [true, true]);
    let command = command_row(&view, cx);
    assert!(!command.expanded);
    let key = command.data.key.clone();
    cx.update_window(window.into(), |_, window, cx| {
        window.focus(&view.read(cx).transcript.read(cx).test_focus(&key, cx), cx);
        window.press("space", cx);
    })
    .unwrap();
    assert!(command_row(&view, cx).expanded);
    press(window, Shortcut::COLLAPSE_THINKING.0, cx);
    assert!(command_row(&view, cx).expanded);
    assert_eq!(thought_states(cx), [false, false]);
    press(window, Shortcut::FOLD.0, cx);
    assert!(!command_row(&view, cx).expanded);
    press(window, Shortcut::UNFOLD.0, cx);
    assert!(command_row(&view, cx).expanded);
    dispatch(window, actions::ToggleFold, cx);
    assert!(!command_row(&view, cx).expanded);
    assert!(
        rows(&view, cx)
            .iter()
            .filter(|r| !r.data.is_disclosure())
            .all(|r| r.expanded)
    );
}
