use super::*;
use crate::timeline::{BlockKind, Step, TimelineRow, tools::ToolDetail};
use gpui_kit::{AppContext as _, Focusable as _};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Reasoning, ToolCall, ToolFunction, ToolName,
};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};
use zevria_content::{HostedActivity, HostedActivityKind, HostedActivityStatus, ResponseAttempt};
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobSnapshot, CommandJobStatus, ModelProfileRef,
    SubtaskDescriptor, SubtaskId, SubtaskKind, SubtaskStatus,
};
use zevria_presentation::status_icon::StatusIcon;
use zevria_session_api::SessionStreamState;
use zevria_session_api::{JobEvent, SubtaskEvent, TurnEvent};
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

fn tool_call(id: &str, name: &str, args: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::new(
        CallId::from_wire(id),
        ToolFunction::new(ToolName::new(name).unwrap(), args),
    ))
}

const COMMAND_INPUT: &str = "git status --short";
const COMMAND_FAILURE: &str = "exit_code: 101\nduration: 1.8s\n<stdout>\n M Cargo.toml\n</stdout>\n<stderr>\npermission denied\n</stderr>";

fn command() -> Message {
    Message::Assistant(AssistantMessage::new(vec![tool_call(
        "cmd",
        "command",
        json!({"command":COMMAND_INPUT}),
    )]))
}

fn foreground_job(call_id: &str, command: &str) -> CommandJobSnapshot {
    CommandJobSnapshot {
        id: CommandJobId::new(format!("job-{call_id}")),
        call_id: Some(call_id.into()),
        command: command.into(),
        mode: CommandJobMode::Foreground,
        status: CommandJobStatus::Running,
        started_at: SystemTime::now() - Duration::from_secs(1),
        finished_at: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

fn command_row(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> TimelineRow {
    rows(view, cx)
        .into_iter()
        .find(|row| matches!(row.data.kind, BlockKind::Step(Step::Tool(_))))
        .unwrap()
}

const THOUGHT: &str = "Check the worktree first.";
const COMMAND_OUTPUT: &str = "exit_code: 0\nduration: 0.1s\n<stdout>\n M Cargo.toml\n</stdout>";

fn seed_disclosure_rows(controller: &Entity<SessionController>, cx: &mut TestAppContext) {
    let turn_id = TurnId::new(1);
    let mut emit = |update| event(controller, update, cx);
    let command_call = |id, input| tool_call(id, "command", json!({"command":input}));
    emit(E::Turn(TurnEvent::Started {
        turn_id,
        mode: SessionMode::Build,
        message: Message::user("Check the worktree and documentation"),
        references: Vec::new(),
    }));
    emit(E::Turn(TurnEvent::ModelCallStarted { turn_id, call: 1 }));
    emit(E::Turn(TurnEvent::Intermediate {
        turn_id,
        message: Message::Assistant(AssistantMessage::new(vec![
            AssistantContent::Reasoning(Reasoning::new(THOUGHT)),
            command_call("running", "cargo check"),
            command_call("foreground", COMMAND_INPUT),
            command_call("background", "cargo fmt --check"),
            tool_call("launch", "launch_subtasks", json!({})),
        ])),
        attempt_id: None,
    }));
    emit(E::Job(JobEvent::Updated {
        job: Box::new(foreground_job("running", "cargo check")),
    }));
    for (id, name, output) in [
        ("foreground", "command", COMMAND_OUTPUT),
        ("background", "command", "exit_code: 0\nduration: 1s"),
        ("launch", "launch_subtasks", "Started docs review"),
    ] {
        emit(E::Turn(TurnEvent::ToolResults {
            turn_id,
            message: Message::tool_result(
                CallId::from_wire(id),
                ToolName::new(name).unwrap(),
                output,
            ),
            metadata: vec![],
        }));
    }
    let finished_at = SystemTime::now();
    emit(E::Job(JobEvent::Updated {
        job: Box::new(CommandJobSnapshot {
            id: CommandJobId::new("job-format"),
            call_id: Some("background".into()),
            command: "cargo fmt --check".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            },
            started_at: finished_at - Duration::from_secs(1),
            finished_at: Some(finished_at),
            stdout_bytes: 0,
            stderr_bytes: 0,
        }),
    }));
    let subtask_id = SubtaskId::new("docs");
    emit(E::Subtask(SubtaskEvent::Launched {
        turn_id,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: Box::new(SubtaskDescriptor {
            id: subtask_id.clone(),
            parent_session_id: "session".into(),
            title: "Review the docs".into(),
            kind: SubtaskKind::Explore,
            workspace: None,
            status: SubtaskStatus::Starting,
        }),
    }));
    emit(E::Subtask(SubtaskEvent::Status {
        turn_id,
        id: subtask_id,
        status: SubtaskStatus::Completed,
    }));
    let mut attempt = ResponseAttempt::new(ModelProfileRef::new("fixture", "fixture"));
    attempt.hosted_activity.push(HostedActivity {
        kind: HostedActivityKind::WebSearch,
        item_id: Some("web-docs".into()),
        output_index: 0,
        status: HostedActivityStatus::Completed,
        action: Some(json!({"type":"search", "query":"cargo check"})),
    });
    emit(E::Turn(TurnEvent::ResponseAttemptUpdated {
        turn_id,
        attempt: Box::new(attempt),
    }));
}

#[gpui_kit::test]
fn disclosure_tails_align_and_overlay_clicks_do_not_toggle_expansion(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    seed_disclosure_rows(&controller, cx);
    render(window, cx);

    let disclosures = rows(&view, cx)
        .into_iter()
        .filter(|row| row.data.is_disclosure())
        .collect::<Vec<_>>();
    let [thought, running, foreground, background, subtask, web] = disclosures.as_slice() else {
        panic!("expected a thought, three commands, a subtask and a web search");
    };
    assert!(
        thought
            .data
            .thought()
            .is_some_and(|thought| !thought.is_live)
    );
    for (row, status, background) in [
        (running, StatusIcon::Running, false),
        (foreground, StatusIcon::Done, false),
        (background, StatusIcon::Done, true),
    ] {
        let BlockKind::Step(Step::Tool(tool)) = &row.data.kind else {
            panic!("expected a command, got {:?}", row.data.kind);
        };
        assert_eq!(tool.name, "command");
        assert_eq!(
            (tool.status, tool.background),
            (status, background),
            "{}",
            tool.summary
        );
    }
    assert!(matches!(
        &subtask.data.kind,
        BlockKind::Step(Step::Subtask(_))
    ));
    assert!(
        matches!(&web.data.kind, BlockKind::Step(Step::Web(web)) if web.label() == "Web search")
    );

    for width in [1240., 720.] {
        cx.simulate_window_resize(window.into(), size(px(width), px(840.)));
        render(window, cx);
        cx.update_window(window.into(), |_, window, _| {
            let reference = window.within(running.data.key.clone());
            let card = reference.find("timeline-step").bounds();
            let status = reference.find("step-status").bounds();
            let tolerance = px(0.5);
            assert!((status.right() - (card.right() - px(11.))).abs() <= tolerance);
            let stop = reference.find("job-stop").bounds();
            let move_to_background = reference.find("job-background").bounds();
            let hover = reference.find("floating-actions").bounds();
            for button in [move_to_background, stop] {
                assert_eq!(button.size, size(px(20.), px(20.)));
                assert!(card.contains(&button.center()));
            }
            assert!(
                hover.right() <= move_to_background.left()
                    && move_to_background.right() <= stop.left()
                    && stop.right() <= status.left(),
                "overlapping command controls at {width}px: {hover:?}, {move_to_background:?}, {stop:?}, {status:?}"
            );

            let thought = window.within(thought.data.key.clone());
            let left = thought.find("block-disclosure").bounds().left();
            let tail = thought.find("thought-tail").bounds();
            assert!(
                (tail.right() - status.right()).abs() <= tolerance,
                "Thought tail at {width}px"
            );
            assert!(thought.try_find("copy").is_some());

            for (step, expected_actions) in [
                (running, &["copy", "show-in-jobs"][..]),
                (foreground, &["copy", "copy-output"][..]),
                (background, &["copy", "copy-output", "show-in-jobs"][..]),
                (subtask, &["copy", "inspect"][..]),
                (web, &["copy"][..]),
            ] {
                let row = window.within(step.data.key.clone());
                let trigger = row.find("block-disclosure");
                let bounds = row.find("step-status").bounds();
                assert!(
                    (bounds.origin.x - status.origin.x).abs() <= tolerance
                        && (bounds.right() - status.right()).abs() <= tolerance
                        && (trigger.bounds().left() - left).abs() <= tolerance,
                    "misaligned header for {:?} at {width}px: {bounds:?} vs. {status:?}",
                    trigger.label(),
                );
                let actions = ["copy", "copy-output", "inspect", "show-in-jobs"]
                    .into_iter()
                    .filter(|id| row.try_find(*id).is_some())
                    .collect::<Vec<_>>();
                assert_eq!(actions, expected_actions, "{:?}", trigger.label());
            }
        })
        .unwrap();
    }

    assert_overlay_copies(window, thought, &[("copy", THOUGHT)], cx);
    assert_overlay_copies(
        window,
        foreground,
        &[
            ("copy", &foreground.data.primary_copy()),
            ("copy-output", COMMAND_OUTPUT),
        ],
        cx,
    );
}

fn assert_overlay_copies(
    window: Window,
    row: &TimelineRow,
    actions: &[(&'static str, &str)],
    cx: &mut TestAppContext,
) {
    let key = &row.data.key;
    let assert_expanded = |expanded, cx: &mut TestAppContext| {
        render(window, cx);
        cx.update_window(window.into(), |_, window, _| {
            let row = window.within(key.clone());
            assert_eq!(
                row.find("block-disclosure").expanded(),
                Some(expanded),
                "{key}"
            );
        })
        .unwrap();
    };
    assert_expanded(false, cx);
    for expanded in [false, true] {
        for &(id, payload) in actions {
            cx.update_window(window.into(), |_, window, cx| {
                let mut row = window.within(key.clone());
                let header = row.find("block-disclosure").bounds();
                assert!(
                    header.contains(&row.find(id).bounds().center()),
                    "{id} should overlap the trigger"
                );
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(String::new()));
                row.click(id, cx);
                assert_eq!(
                    cx.read_from_clipboard()
                        .and_then(|item| item.text())
                        .as_deref(),
                    Some(payload)
                );
            })
            .unwrap();
            assert_expanded(expanded, cx);
        }
        cx.update_window(window.into(), |_, window, cx| {
            window.within(key.clone()).click("block-disclosure", cx);
        })
        .unwrap();
        assert_expanded(!expanded, cx);
    }
}

fn begin_command_turn(controller: &Entity<SessionController>, cx: &mut TestAppContext) -> TurnId {
    let turn_id = TurnId::new(20);
    event(
        controller,
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("Check the worktree"),
            references: Vec::new(),
        }),
        cx,
    );
    event(
        controller,
        E::Turn(TurnEvent::ModelCallStarted { turn_id, call: 1 }),
        cx,
    );
    turn_id
}

fn finish_command(
    controller: &Entity<SessionController>,
    turn_id: TurnId,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::Turn(TurnEvent::ToolResults {
            turn_id,
            metadata: vec![],
            message: Message::tool_result(
                CallId::from_wire("cmd"),
                ToolName::new("command").unwrap(),
                COMMAND_FAILURE,
            ),
        }),
        cx,
    );
}

#[gpui_kit::test]
fn command_copy_feedback_switches_icons_restarts_and_is_exclusive(cx: &mut TestAppContext) {
    use gpui_kit::component::{IconName, IconNamed, set_icon_resolver};
    use std::{cell::Cell, rc::Rc};

    let (window, view, controller, _) = setup(cx);
    let icons = Rc::new(Cell::new((0, 0)));
    cx.update(|cx| {
        let icons = icons.clone();
        let (copy, check) = (IconName::Copy.path(), IconName::Check.path());
        set_icon_resolver(cx, move |path, _| {
            let (copies, checks) = icons.get();
            icons.set((
                copies + usize::from(path == copy.as_ref()),
                checks + usize::from(path == check.as_ref()),
            ));
            None
        });
    });
    let turn_id = begin_command_turn(&controller, cx);
    event(
        &controller,
        E::Turn(TurnEvent::Intermediate {
            turn_id,
            message: command(),
            attempt_id: None,
        }),
        cx,
    );
    finish_command(&controller, turn_id, cx);
    let key = command_row(&view, cx).data.key.clone();
    let frame = |input: bool, output: bool, cx: &mut TestAppContext| {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            icons.set((0, 0));
            window.render_frame(cx);
            let row = window.within(key.clone());
            let primary = row.find("copy");
            let secondary = row.find("copy-output");
            assert_copy_label(&primary, if input { "Copied input" } else { "Copy input" });
            assert_copy_label(
                &secondary,
                if output {
                    "Copied output"
                } else {
                    "Copy output"
                },
            );
            ([primary.bounds(), secondary.bounds()], icons.get())
        })
        .unwrap()
    };
    let (bounds, (copies, checks)) = frame(false, false, cx);
    assert!(copies >= 2, "expected both command copy icons");
    let assert_feedback = |input, output, cx: &mut TestAppContext| {
        let active = usize::from(input) + usize::from(output);
        assert_eq!(
            frame(input, output, cx),
            (bounds, (copies - active, checks + active))
        );
    };
    let input = json!({"command": COMMAND_INPUT}).to_string();
    let click_copy = |id, expected: &str, cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(String::new()));
            let mut row = window.within(key.clone());
            row.hover("block-disclosure", cx);
            row.click(id, cx);
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(expected)
            );
            window.hover("status-bar", cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    click_copy("copy", &input, cx);
    assert_feedback(true, false, cx);
    cx.executor().advance_clock(COPY_FEEDBACK_DURATION / 2);
    click_copy("copy", &input, cx);
    let last_tick = Duration::from_millis(1);
    cx.executor()
        .advance_clock(COPY_FEEDBACK_DURATION - last_tick);
    assert_feedback(true, false, cx);
    cx.executor().advance_clock(last_tick);
    assert_feedback(false, false, cx);

    click_copy("copy", &input, cx);
    click_copy("copy-output", COMMAND_FAILURE, cx);
    assert_feedback(false, true, cx);
    cx.executor().advance_clock(COPY_FEEDBACK_DURATION);
    assert_feedback(false, false, cx);

    cx.update_window(window.into(), |_, window, cx| {
        let focus = view.read(cx).transcript.read(cx).test_focus(&key, cx);
        window.focus(&focus, cx);
        window.render_frame(cx);
        window.press(Shortcut::COPY_DISCLOSURE.0, cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(input)
        );
    })
    .unwrap();
    assert_feedback(true, false, cx);
}

#[gpui_kit::test]
fn command_stays_collapsed_at_a_stable_height_and_copies_original_input_and_output(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup(cx);
    restore(
        &controller,
        vec![
            TranscriptItem::Message(Message::user("Summarize the workspace")),
            TranscriptItem::Message(Message::assistant("Workspace notes.\n\n".repeat(80))),
        ],
        cx,
    );
    let id = begin_command_turn(&controller, cx);
    render(window, cx);
    assert!(
        top(&view, cx) > (0, px(0.)),
        "transcript must be scrollable"
    );
    assert!(transcript(&view, cx).read_with(cx, |v, cx| v.scroller().read(cx).is_following_tail()));
    let prompt = rows(&view, cx)
        .into_iter()
        .rfind(|row| matches!(row.data.kind, BlockKind::Prompt { .. }))
        .unwrap();
    let prompt_top = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, _| {
            window
                .within(prompt.data.key.clone())
                .find("prompt-card")
                .bounds()
                .top()
        })
        .unwrap()
    };
    let collapsed_height = |row: &TimelineRow, cx: &mut TestAppContext| {
        assert!(!row.expanded);
        cx.update_window(window.into(), |_, window, _| {
            let step = window.within(row.data.key.clone());
            let header = step.find("block-disclosure");
            assert_eq!(header.expanded(), Some(false));
            assert_eq!(header.bounds().size.height, px(32.));
            step.find("timeline-step").bounds().size.height
        })
        .unwrap()
    };
    let before_start = prompt_top(cx);
    event(
        &controller,
        E::Turn(TurnEvent::Intermediate {
            turn_id: id,
            message: command(),
            attempt_id: None,
        }),
        cx,
    );
    render(window, cx);
    let running = command_row(&view, cx);
    let height = collapsed_height(&running, cx);
    let after_start = prompt_top(cx);
    assert!(
        after_start <= before_start,
        "the prompt moved down as the command started"
    );
    let mut job = foreground_job("cmd", COMMAND_INPUT);
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(job.clone()),
        }),
        cx,
    );
    render(window, cx);
    assert_eq!(collapsed_height(&command_row(&view, cx), cx), height);
    assert_eq!(prompt_top(cx), after_start);
    cx.update_window(window.into(), |_, window, cx| {
        let focus = view
            .read(cx)
            .transcript
            .read(cx)
            .test_focus(&running.data.key, cx);
        window.focus(&focus, cx);
        let mut step = window.within(running.data.key.clone());
        for action in ["copy", "show-in-jobs"] {
            step.press("tab", cx);
            assert_eq!(step.find(action).focused(), Some(true));
        }
    })
    .unwrap();

    job.status = CommandJobStatus::Exited {
        code: Some(101),
        success: false,
        signal: None,
    };
    job.finished_at = Some(SystemTime::now());
    event(
        &controller,
        E::Job(JobEvent::Updated { job: Box::new(job) }),
        cx,
    );
    render(window, cx);
    assert_eq!(collapsed_height(&command_row(&view, cx), cx), height);
    assert_eq!(prompt_top(cx), after_start);
    cx.update_window(window.into(), |_, window, cx| {
        let step = window.within(running.data.key.clone());
        assert!(step.try_find("show-in-jobs").is_none());
        assert_eq!(
            step.find("block-disclosure").focused(),
            Some(true),
            "focus after the foreground job exits"
        );
        window.press(Shortcut::COPY_DISCLOSURE.0, cx);
    })
    .unwrap();

    finish_command(&controller, id, cx);
    render(window, cx);
    let finished = command_row(&view, cx);
    assert_eq!(running.data.key, finished.data.key);
    assert_eq!(collapsed_height(&finished, cx), height);
    cx.update_window(window.into(), |_, window, _| {
        let copy = window.within(finished.data.key.clone()).find("copy");
        assert_copy_label(&copy, "Copied input");
    })
    .unwrap();
    // The waiting tip may return, but the command itself must not shrink and
    // pull the preceding content down as it finishes.
    assert!(
        prompt_top(cx) <= after_start,
        "the prompt rebounded downward"
    );
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
            Some(json!({"command":COMMAND_INPUT}).to_string().as_str())
        );
        let mut row = window.within(finished.data.key.clone());
        row.hover("block-disclosure", cx);
        row.click("copy-output", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
            Some(COMMAND_FAILURE)
        );
    })
    .unwrap();
    assert!(command_row(&view, cx).expanded);
    event(
        &controller,
        E::Turn(TurnEvent::ModelCallStarted {
            turn_id: id,
            call: 2,
        }),
        cx,
    );
    event(
        &controller,
        SessionStreamState::new(
            id,
            zevria_content::AssistantStreamSnapshot {
                message: Some(Message::assistant("I'll report the result.")),
                attempt: None,
            },
        ),
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
fn opening_a_running_command_keeps_its_details_open_after_results(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    let turn_id = begin_command_turn(&controller, cx);
    event(
        &controller,
        E::Turn(TurnEvent::Intermediate {
            turn_id,
            message: command(),
            attempt_id: None,
        }),
        cx,
    );
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(foreground_job("cmd", COMMAND_INPUT)),
        }),
        cx,
    );
    render(window, cx);
    let running = command_row(&view, cx);
    assert!(!running.expanded);
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(running.data.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    assert!(command_row(&view, cx).expanded);
    cx.update_window(window.into(), |_, window, cx| {
        let mut row = window.within(running.data.key.clone());
        assert_eq!(row.find("command-open-jobs").label(), Some("Show in Jobs"));
        assert_eq!(row.find_all("job-stop").len(), 1);
        assert_eq!(row.find_all("job-background").len(), 1);
        assert!(row.try_find("command-stdout").is_none());
        for action in [
            "copy",
            "show-in-jobs",
            "job-background",
            "job-stop",
            "command-open-jobs",
        ] {
            row.press("tab", cx);
            assert_eq!(row.find(action).focused(), Some(true));
        }
    })
    .unwrap();

    finish_command(&controller, turn_id, cx);
    render(window, cx);
    let finished = command_row(&view, cx);
    assert_eq!(finished.data.key, running.data.key);
    assert!(finished.expanded);
    cx.update_window(window.into(), |_, window, _| {
        let row = window.within(finished.data.key.clone());
        let disclosure = row.find("block-disclosure");
        assert_eq!(disclosure.expanded(), Some(true));
        assert!(row.try_find("command-open-jobs").is_none());
        assert!(row.try_find("command-stdout").is_some());
        assert!(row.try_find("command-stderr").is_some());
        assert_eq!(
            disclosure.focused(),
            Some(true),
            "focus after output replaces the details link"
        );
    })
    .unwrap();
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
