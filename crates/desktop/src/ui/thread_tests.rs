use super::*;
use crate::{
    disclosure::Phase,
    ui::blocks::{BlockData, BlockKind, LiveTail, NoticeLevel},
};
use rig_core::message::{AssistantContent, AssistantMessage, Reasoning};
use zevria_content::{AssistantStreamSnapshot, ResponseAttempt, ResponseAttemptOutcome};
use zevria_transcript::transcript::TranscriptItem;

fn thinking(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
        Reasoning::new(text),
    )]))
}
fn start(controller: &Entity<SessionController>, id: TurnId, cx: &mut TestAppContext) {
    event(
        controller,
        E::TurnStarted {
            turn_id: id,
            mode: SessionMode::Build,
            message: Message::user("Trace the source"),
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
fn stream(
    controller: &Entity<SessionController>,
    id: TurnId,
    message: Message,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::AssistantStreamUpdated {
            turn_id: id,
            snapshot: AssistantStreamSnapshot {
                message: Some(message),
                attempt: None,
            },
        },
        cx,
    );
}
fn data(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    view.read_with(cx, |view, cx| view.transcript.read(cx).test_data())
}
fn thoughts(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    data(view, cx)
        .into_iter()
        .filter(|b| matches!(b.kind, BlockKind::Reasoning))
        .collect()
}
fn render(window: Window, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn live_thinking_is_accessible_and_explicit_collapse_survives_deltas_and_commit(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(100);
    start(&controller, id, cx);
    stream(&controller, id, thinking("### **Trace** the source"), cx);
    render(window, cx);
    let thought = thoughts(&view, cx).remove(0);
    assert!(thought.expanded);
    assert_eq!(thought.phase, Phase::Live);
    assert!(
        data(&view, cx)
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::Live(LiveTail::Activity(p)) if p == "Thinking"))
    );
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&thought.key, cx)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, cx| {
        let header = window
            .within(thought.key.clone())
            .find("thinking-disclosure");
        assert_eq!(header.role(), Some(gpui_kit::Role::Button));
        assert_eq!(header.expanded(), Some(true));
        assert_eq!(header.label(), Some("Thinking"));
        assert_eq!(header.description(), Some("Trace the source"));
        assert!(header.bounds().size.height >= px(28.));
        window
            .within(thought.key.clone())
            .click("thinking-disclosure", cx);
    })
    .unwrap();
    stream(
        &controller,
        id,
        thinking("### **Trace** the source carefully"),
        cx,
    );
    assert!(!thoughts(&view, cx)[0].expanded);
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: thinking("### **Trace** the source carefully"),
            attempt_id: None,
        },
        cx,
    );
    render(window, cx);
    let committed = thoughts(&view, cx).remove(0);
    assert!(!committed.expanded);
    assert_ne!(committed.key, thought.key);
    assert_eq!(
        identity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&committed.key, cx)
            .unwrap())
    );
    start(&controller, TurnId::new(101), cx);
    stream(&controller, TurnId::new(101), thinking("A new thought"), cx);
    let next = thoughts(&view, cx).pop().unwrap();
    assert!(
        next.expanded,
        "a new turn must not inherit the old collapse"
    );
    assert_ne!(next.key, thought.key);
}

#[gpui_kit::test]
fn untouched_thinking_folds_only_after_turn_end_and_tail_following(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    let id = TurnId::new(102);
    start(&controller, id, cx);
    let body = "Trace the supplied provider text.\n\n".repeat(90);
    stream(&controller, id, thinking(&body), cx);
    render(window, cx);
    let scroller = view.read_with(cx, |view, _| view.transcript.clone());
    scroller.update(cx, |view, cx| {
        view.scroller().update(cx, |scroller, cx| {
            scroller.scroll_to_item(0, cx);
        })
    });
    render(window, cx);
    let before = scroller.read_with(cx, |view, cx| view.scroller().read(cx).logical_scroll_top());
    stream(
        &controller,
        id,
        thinking(&format!("{body}More evidence.")),
        cx,
    );
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: thinking(&format!("{body}More evidence.")),
            attempt_id: None,
        },
        cx,
    );
    render(window, cx);
    let after = scroller.read_with(cx, |view, cx| view.scroller().read(cx).logical_scroll_top());
    assert_eq!(
        before, after,
        "deltas, commit and turn-end folding must not move the reader"
    );
    assert!(
        thoughts(&view, cx)[0].expanded,
        "fold is pending while scrolled away"
    );
    scroller.update(cx, |view, cx| view.jump_to_latest(cx));
    render(window, cx);
    assert!(!thoughts(&view, cx)[0].expanded);
    assert!(scroller.read_with(cx, |view, cx| view.scroller().read(cx).is_following_tail()));
    start(&controller, TurnId::new(103), cx);
    stream(&controller, TurnId::new(103), thinking("Untouched"), cx);
    event(
        &controller,
        E::TurnCompleted {
            turn_id: TurnId::new(103),
            message: thinking("Untouched"),
            attempt_id: None,
        },
        cx,
    );
    assert!(!thoughts(&view, cx).last().unwrap().expanded);
}

#[gpui_kit::test]
fn restored_thinking_starts_folded_and_keyboard_disclosures_work(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            TranscriptItem::Message(thinking("**Read** the file")),
            TranscriptItem::Message(thinking("Then test")),
        ]);
        cx.notify();
    });
    render(window, cx);
    let thought = thoughts(&view, cx).remove(0);
    assert_eq!(thought.phase, Phase::Restored);
    assert!(!thought.expanded);
    cx.update_window(window.into(), |_, window, cx| {
        let focus = view
            .read(cx)
            .transcript
            .read(cx)
            .test_focus(&thought.key, cx);
        window.focus(&focus, cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert!(thoughts(&view, cx)[0].expanded);
    cx.update_window(window.into(), |_, window, cx| window.press("space", cx))
        .unwrap();
    assert!(!thoughts(&view, cx)[0].expanded);
    for (key, expected) in [
        ("alt-cmd-right", true),
        ("alt-cmd-left", false),
        ("alt-shift-cmd-right", true),
        ("alt-shift-cmd-left", false),
    ] {
        cx.update_window(window.into(), |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
        let states = thoughts(&view, cx);
        assert_eq!(states[0].expanded, expected, "{key}");
        if key.contains("shift") {
            assert!(states.iter().all(|b| b.expanded == expected));
        }
    }
    cx.update_window(window.into(), |_, window, cx| {
        view.update(cx, |view, cx| {
            view.composer
                .update(cx, |input, cx| input.focus(window, cx))
        });
        window.press("alt-cmd-right", cx);
    })
    .unwrap();
    let states = thoughts(&view, cx);
    assert!(!states[0].expanded);
    assert!(states[1].expanded, "composer expands the latest thinking");
}

#[gpui_kit::test]
fn opaque_only_thinking_has_no_header_and_truthful_waiting_tip(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(104);
    start(&controller, id, cx);
    let opaque = zevria_content::reasoning::from_blocks(
        None,
        vec![
            zevria_content::reasoning::ReasoningContent::Encrypted("ciphertext".into()),
            zevria_content::reasoning::ReasoningContent::Redacted {
                data: "secret".into(),
            },
        ],
    );
    stream(
        &controller,
        id,
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
            opaque,
        )])),
        cx,
    );
    render(window, cx);
    assert!(thoughts(&view, cx).is_empty());
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("thinking-disclosure").is_none())
    })
    .unwrap();
    assert!(data(&view, cx).iter().any(|b| matches!(&b.kind, BlockKind::Live(LiveTail::Activity(p)) if p == "Waiting for the model")));
}

#[gpui_kit::test]
fn message_only_stop_keeps_unsaved_partial_before_a_neutral_stopped_node_but_not_restore(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(105);
    start(&controller, id, cx);
    stream(&controller, id, thinking("Partial provider thought"), cx);
    render(window, cx);
    event(&controller, E::TurnCancelled { turn_id: id }, cx);
    render(window, cx);
    let data = data(&view, cx);
    let index = data
        .iter()
        .position(|b| matches!(b.kind, BlockKind::Reasoning))
        .unwrap();
    assert_eq!(data[index].phase, Phase::Unsaved);
    assert!(data[index].expanded);
    assert_eq!(data[index].primary, "Partial provider thought");
    assert!(matches!(
        data[index + 1].kind,
        BlockKind::Notice {
            level: NoticeLevel::Neutral
        }
    ));
    assert_eq!(data[index + 1].body, "Stopped");
    controller.update(cx, |c, cx| {
        c.model
            .root
            .conversation
            .restore(vec![TranscriptItem::Message(Message::user(
                "Trace the source",
            ))]);
        cx.notify();
    });
    assert!(thoughts(&view, cx).is_empty());
}

#[gpui_kit::test]
fn attempt_reasoning_streams_in_place_and_interruption_is_not_marked_unsaved(
    cx: &mut TestAppContext,
) {
    use zevria_content::{
        AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
        AssistantSourceAddress,
    };
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(106);
    start(&controller, id, cx);
    let mut attempt = ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("p", "m"));
    attempt.presentation.push(AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Summary(0),
            item_id: None,
        },
        content: AssistantPresentationContent::Reasoning {
            text: "Readable attempt".into(),
        },
    });
    event(
        &controller,
        E::AssistantStreamUpdated {
            turn_id: id,
            snapshot: AssistantStreamSnapshot {
                message: None,
                attempt: Some(attempt.clone()),
            },
        },
        cx,
    );
    render(window, cx);
    let before = thoughts(&view, cx).remove(0);
    assert!(before.expanded);
    assert_eq!(before.phase, Phase::Live);
    assert!(
        data(&view, cx)
            .iter()
            .any(|b| matches!(&b.kind, BlockKind::Live(LiveTail::Activity(p)) if p == "Thinking"))
    );
    attempt.close(ResponseAttemptOutcome::Interrupted);
    let saved = zevria_transcript::IncompleteResponse::from_attempt(&attempt)
        .unwrap()
        .unwrap();
    event(
        &controller,
        E::ResponseAttemptUpdated {
            turn_id: id,
            attempt,
        },
        cx,
    );
    event(&controller, E::TurnCancelled { turn_id: id }, cx);
    render(window, cx);
    let interrupted = thoughts(&view, cx).remove(0);
    assert_eq!(before.key, interrupted.key);
    assert_eq!(interrupted.phase, Phase::Interrupted);
    assert!(interrupted.expanded);
    assert!(controller.read_with(cx, |c, _| c.unsaved_blocks.is_empty()));
    assert!(
        data(&view, cx)
            .iter()
            .any(|b| b.body == "Stopped before the response finished")
    );
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            TranscriptItem::Message(Message::user("Trace the source")),
            TranscriptItem::IncompleteResponse(saved),
        ]);
        cx.notify();
    });
    let restored = thoughts(&view, cx).remove(0);
    assert_eq!(restored.body, "Readable attempt");
    assert_eq!(restored.phase, Phase::Restored);
    assert!(!restored.expanded);
}

#[gpui_kit::test]
fn reasoning_tool_reasoning_answer_order_is_identical_live_committed_and_restored(
    cx: &mut TestAppContext,
) {
    use rig_core::message::{CallId, Text, ToolCall, ToolFunction, ToolName};
    let (_, view, controller, _) = setup(cx);
    let id = TurnId::new(107);
    start(&controller, id, cx);
    let first = Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::Reasoning(Reasoning::new("First thought")),
        AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire("test"),
            ToolFunction::new(
                ToolName::new("command").unwrap(),
                serde_json::json!({"command":"cargo test"}),
            ),
        )),
    ]));
    event(
        &controller,
        E::Intermediate {
            turn_id: id,
            message: first.clone(),
            attempt_id: None,
        },
        cx,
    );
    event(
        &controller,
        E::ModelCallStarted {
            turn_id: id,
            call: 2,
        },
        cx,
    );
    let second = Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::Reasoning(Reasoning::new("Second thought")),
        AssistantContent::Text(Text::new("The answer")),
    ]));
    stream(&controller, id, second.clone(), cx);
    let order = |cx: &TestAppContext| {
        data(&view, cx)
            .iter()
            .filter_map(|b| match b.kind {
                BlockKind::Reasoning => Some("thinking"),
                BlockKind::Tool { .. } => Some("tool"),
                BlockKind::Prose {
                    role: zevria_presentation::blocks::PresentationRole::Assistant,
                    ..
                } => Some("answer"),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(order(cx), ["thinking", "tool", "thinking", "answer"]);
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: second.clone(),
            attempt_id: None,
        },
        cx,
    );
    assert_eq!(order(cx), ["thinking", "tool", "thinking", "answer"]);
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            TranscriptItem::Message(Message::user("Trace the source")),
            TranscriptItem::Message(first),
            TranscriptItem::Message(second),
        ]);
        cx.notify();
    });
    assert_eq!(order(cx), ["thinking", "tool", "thinking", "answer"]);
    assert!(thoughts(&view, cx).iter().all(|b| !b.expanded));
}

#[gpui_kit::test]
fn explicit_expand_survives_turn_end_but_projection_replacement_retires_choices(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _) = setup(cx);
    let id = TurnId::new(108);
    start(&controller, id, cx);
    stream(&controller, id, thinking("An explicit choice"), cx);
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.press("alt-cmd-left", cx);
        window.press("alt-cmd-right", cx);
    })
    .unwrap();
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: thinking("An explicit choice"),
            attempt_id: None,
        },
        cx,
    );
    assert!(thoughts(&view, cx)[0].expanded);
    controller.update(cx, |c, cx| {
        c.model
            .root
            .conversation
            .restore(vec![TranscriptItem::Message(thinking(
                "An explicit choice",
            ))]);
        cx.notify();
    });
    assert!(!thoughts(&view, cx)[0].expanded);
}

#[gpui_kit::test]
fn failed_message_tail_is_retained_but_a_foreign_terminal_cannot_capture_it(
    cx: &mut TestAppContext,
) {
    let (_, view, controller, _) = setup(cx);
    let id = TurnId::new(109);
    start(&controller, id, cx);
    stream(&controller, id, thinking("Partial before failure"), cx);
    event(
        &controller,
        E::TurnCancelled {
            turn_id: TurnId::new(999),
        },
        cx,
    );
    assert!(controller.read_with(cx, |c, _| c.unsaved_blocks.is_empty()));
    event(
        &controller,
        E::TurnFailed {
            turn_id: id,
            error: "Connection closed".into(),
        },
        cx,
    );
    let blocks = data(&view, cx);
    let index = blocks
        .iter()
        .position(|b| matches!(b.kind, BlockKind::Reasoning))
        .unwrap();
    assert_eq!(blocks[index].phase, Phase::Unsaved);
    assert!(blocks[index].expanded);
    assert!(matches!(
        blocks[index + 1].kind,
        BlockKind::Notice {
            level: NoticeLevel::Error
        }
    ));
}

#[gpui_kit::test]
fn minimum_window_question_dock_and_plan_overlay_fit_and_keyboard_decisions_preserve_draft(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup_at_size(cx, 720., 520.);
    controller.update(cx, |c, cx| c.edit_draft("Retain my draft".into(), cx));
    let id = TurnId::new(110);
    start(&controller, id, cx);
    let request_id = QuestionRequestId::new("dock");
    event(
        &controller,
        E::QuestionAsked {
            turn_id: id,
            request: QuestionRequest {
                id: request_id.clone(),
                source_label: None,
                dismissible: false,
                questions: vec![QuestionPrompt {
                    id: "a".into(),
                    header: "Choose".into(),
                    question: "What should change?".into(),
                    kind: QuestionPromptKind::Text {
                        min_length: None,
                        max_length: None,
                    },
                    options: vec![],
                    required: true,
                    default: None,
                }],
            },
        },
        cx,
    );
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("composer-panel").is_none());
        let dock = window.find("question-dialog").bounds();
        assert!(dock.size.height <= px(520. * 0.55 + 24.));
        let send = window.find("answer-question").bounds();
        assert!(send.origin.x >= px(0.) && send.right() <= px(720.) && send.bottom() <= px(520.));
        window.press("escape", cx);
    })
    .unwrap();
    assert!(!controller.read_with(cx, |c, _| c.model.question.as_ref().unwrap().pending));
    assert!(receiver.try_recv().is_err());
    controller.update(cx, |c, cx| {
        c.model.question.as_mut().unwrap().answers[0].text = "Answer".into();
        cx.notify();
    });
    cx.update_window(window.into(), |_, window, cx| window.press("cmd-enter", cx))
        .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Control(ControlCommand::AnswerQuestion { .. })
    ));
    event(
        &controller,
        E::QuestionClosed {
            turn_id: id,
            request_id,
        },
        cx,
    );
    event(
        &controller,
        E::TurnCompleted {
            turn_id: id,
            message: Message::assistant("Done"),
            attempt_id: None,
        },
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.draft.clone()),
        "Retain my draft"
    );
    let version = PlanVersion {
        id: PlanId::new(),
        revision: 2,
    };
    event(
        &controller,
        E::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version,
                    title: "Plan".into(),
                    markdown: "Review the source".into(),
                    source_turn_id: id,
                },
            },
        },
        cx,
    );
    render(window, cx);
    assert!(view.read_with(cx, |view, _| view.plan_overlay));
    cx.update_window(window.into(), |_, window, cx| {
        let panel = window.find("plan-review").bounds();
        assert!(
            panel.origin.x >= px(0.) && panel.right() <= px(720.) && panel.bottom() <= px(520.)
        );
        window.press("cmd-enter", cx);
    })
    .unwrap();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Turn(TurnCommand::ResolvePlan {
            expected: version,
            decision: PlanDecision::ImplementCurrent
        })
    );
}

#[gpui_kit::test]
fn sidebar_arrow_navigation_and_enter_inspect_the_pinned_session_children(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    let id = zevria_foundation::SubtaskId::new("child");
    controller.update(cx, |c, cx| {
        c.model
            .root
            .conversation
            .restore(vec![TranscriptItem::Message(thinking(
                "Keep my disclosure while inspecting a child",
            ))]);
        c.model.children.push(crate::session::ChildPane {
            id: id.clone(),
            title: "Inspect reader".into(),
            workspace: None,
            kind: zevria_foundation::SubtaskKind::Explore,
            status: Some(zevria_foundation::SubtaskStatus::Completed),
            model: zevria_presentation::SessionModel::default(),
        });
        cx.notify();
    });
    render(window, cx);
    let root_key = thoughts(&view, cx)[0].key.clone();
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&root_key, cx)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.press("alt-cmd-right", cx);
        let focus = view.read(cx).sidebar_focus[0].clone();
        window.focus(&focus, cx);
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.selected_child.clone()),
        Some(id)
    );
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.press("up", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| c.model.selected_child.is_none()));
    render(window, cx);
    assert!(thoughts(&view, cx)[0].expanded);
    assert_eq!(
        identity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&root_key, cx)
            .unwrap())
    );
}

#[gpui_kit::test]
fn root_question_dock_keeps_ownership_while_a_child_is_inspected(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let child_id = zevria_foundation::SubtaskId::new("inspected");
    controller.update(cx, |c, cx| {
        let mut model = zevria_presentation::SessionModel::default();
        model.set_scope(zevria_presentation::PaneScope::Inspect {
            role: Some(zevria_foundation::ModelRole::Explore),
        });
        c.model.children.push(crate::session::ChildPane {
            id: child_id.clone(),
            title: "Inspect".into(),
            workspace: None,
            kind: zevria_foundation::SubtaskKind::Explore,
            status: Some(zevria_foundation::SubtaskStatus::Running),
            model,
        });
        c.model.selected_child = Some(child_id.clone());
        cx.notify();
    });
    let id = TurnId::new(112);
    start(&controller, id, cx);
    let request_id = QuestionRequestId::new("root-owned");
    event(
        &controller,
        E::QuestionAsked {
            turn_id: id,
            request: QuestionRequest {
                id: request_id.clone(),
                source_label: None,
                dismissible: true,
                questions: vec![QuestionPrompt {
                    id: "a".into(),
                    header: "Root question".into(),
                    question: "Continue?".into(),
                    kind: QuestionPromptKind::Text {
                        min_length: None,
                        max_length: None,
                    },
                    options: vec![],
                    required: true,
                    default: None,
                }],
            },
        },
        cx,
    );
    controller.update(cx, |c, cx| {
        c.model.question.as_mut().unwrap().answers[0].text = "Yes".into();
        cx.notify();
    });
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("question-dialog").visible());
        window.press("cmd-enter", cx);
    })
    .unwrap();
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Control(ControlCommand::AnswerQuestion { request_id: actual, .. }) if actual == request_id)
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.selected_child.clone()),
        Some(child_id)
    );
}

#[gpui_kit::test]
fn command_period_can_stop_a_turn_even_with_a_non_dismissible_question(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let id = TurnId::new(111);
    start(&controller, id, cx);
    event(
        &controller,
        E::QuestionAsked {
            turn_id: id,
            request: QuestionRequest {
                id: QuestionRequestId::new("must-answer"),
                source_label: None,
                dismissible: false,
                questions: vec![QuestionPrompt {
                    id: "answer".into(),
                    header: "Question".into(),
                    question: "Which file?".into(),
                    kind: QuestionPromptKind::Text {
                        min_length: None,
                        max_length: None,
                    },
                    options: vec![],
                    required: true,
                    default: None,
                }],
            },
        },
        cx,
    );
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();
    assert!(receiver.try_recv().is_err());
    cx.update_window(window.into(), |_, window, cx| window.press("cmd-.", cx))
        .unwrap();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Control(ControlCommand::CancelTurn { turn_id: Some(id) })
    );
}

#[gpui_kit::test]
fn appearance_adapter_refreshes_palette_and_highlighter_and_named_themes_stay_pinned(
    cx: &mut TestAppContext,
) {
    let (window, _, _, _) = setup(cx);
    cx.update(|cx| theme::select(theme::Selection::System, cx));
    // Kit 0.9's TestWindow::simulate_appearance_change is public, but the
    // TestAppContext accessor is crate-private. Exercise the exact adapter
    // called by the native observer without reaching into Kit internals.
    cx.update(|cx| theme::apply_appearance(gpui_kit::WindowAppearance::Dark, cx));
    render(window, cx);
    cx.update(|cx| {
        assert_eq!(
            theme::palette(cx),
            zevria_theme::ZEVRIA_GRAPHITE.map(theme::color)
        )
    });
    cx.update(|cx| theme::apply_appearance(gpui_kit::WindowAppearance::Light, cx));
    render(window, cx);
    cx.update(|cx| {
        let t = theme::palette(cx);
        assert_eq!(t, zevria_theme::ZEVRIA_GRAPHITE_LIGHT.map(theme::color));
        assert_eq!(
            gpui_kit::component::Theme::global(cx)
                .highlight_theme
                .style
                .syntax
                .style("keyword")
                .unwrap()
                .color,
            Some(t.syntax.keyword)
        );
        theme::select(theme::Selection::Named(zevria_theme::ZEVRIA_DARK), cx);
    });
    cx.update(|cx| theme::apply_appearance(gpui_kit::WindowAppearance::Dark, cx));
    render(window, cx);
    cx.update(|cx| theme::apply_appearance(gpui_kit::WindowAppearance::Light, cx));
    render(window, cx);
    cx.update(|cx| {
        assert_eq!(
            theme::palette(cx),
            zevria_theme::ZEVRIA_DARK.map(theme::color)
        )
    });
}
