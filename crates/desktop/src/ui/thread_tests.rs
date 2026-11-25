use super::*;
use crate::{
    disclosure::Phase,
    timeline::{BlockKind, NoticeLevel, Step, TimelineRow},
};
use rig_core::message::{AssistantContent, AssistantMessage, Reasoning};
use zevria_content::{AssistantStreamSnapshot, ResponseAttempt, ResponseAttemptOutcome};
use zevria_session_api::SessionStreamState;
use zevria_session_api::{PlanEvent, QuestionEvent, TurnEvent};
use zevria_transcript::transcript::TranscriptItem;

fn thinking(text: &str) -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
        Reasoning::new(text),
    )]))
}
fn start(controller: &Entity<SessionController>, id: TurnId, cx: &mut TestAppContext) {
    event(
        controller,
        E::Turn(TurnEvent::Started {
            turn_id: id,
            mode: SessionMode::Build,
            message: Message::user("Trace the source"),
        }),
        cx,
    );
    event(
        controller,
        E::Turn(TurnEvent::ModelCallStarted {
            turn_id: id,
            call: 1,
        }),
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
        SessionStreamState::new(
            id,
            AssistantStreamSnapshot {
                message: Some(message),
                attempt: None,
            },
        ),
        cx,
    );
}
fn data(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<TimelineRow> {
    view.read_with(cx, |view, cx| view.transcript.read(cx).test_data(cx))
}
fn thoughts(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<TimelineRow> {
    data(view, cx)
        .into_iter()
        .filter(|row| row.data.thought().is_some())
        .collect()
}
fn render(window: Window, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn first_thought_retains_its_renderer_and_auto_collapses_on_prose(cx: &mut TestAppContext) {
    let (window, view, controller, _receiver) = setup(cx);
    let id = TurnId::new(100);
    start(&controller, id, cx);
    stream(&controller, id, thinking("### **Trace** the source"), cx);
    render(window, cx);
    let thought = thoughts(&view, cx).remove(0);
    assert!(thought.expanded);
    assert_eq!(thought.phase, Phase::Live);
    assert!(matches!(thought.data.kind, BlockKind::ThoughtGroup(_)));
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&thought.data.key, cx)
            .unwrap()
    });
    let answer = Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::Reasoning(Reasoning::new("### **Trace** the source carefully")),
        AssistantContent::text("Found the relevant code."),
    ]));
    stream(&controller, id, answer.clone(), cx);
    assert!(!thoughts(&view, cx)[0].expanded);
    event(
        &controller,
        E::Turn(TurnEvent::Completed {
            turn_id: id,
            message: answer,
            attempt_id: None,
        }),
        cx,
    );
    render(window, cx);
    let committed = thoughts(&view, cx).remove(0);
    assert!(!committed.expanded);
    assert!(committed.data.source.is_some());
    assert_ne!(committed.data.key, thought.data.key);
    assert_eq!(
        identity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&committed.data.key, cx)
            .unwrap())
    );
}

#[gpui_kit::test]
fn later_call_thought_stays_in_order_and_keeps_pinned_intent(cx: &mut TestAppContext) {
    use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
    let (window, view, controller, _receiver) = setup(cx);
    let id = TurnId::new(102);
    start(&controller, id, cx);
    let inspect = ToolCall::new(
        CallId::from_wire("inspect"),
        ToolFunction::new(
            ToolName::new("read").unwrap(),
            serde_json::json!({"file_path":"src/lib.rs"}),
        ),
    );
    event(
        &controller,
        E::Turn(TurnEvent::Intermediate {
            turn_id: id,
            attempt_id: None,
            message: Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("Read the source")),
                AssistantContent::ToolCall(inspect),
            ])),
        }),
        cx,
    );
    event(
        &controller,
        E::Turn(TurnEvent::ToolResults {
            turn_id: id,
            metadata: vec![],
            message: Message::tool_result(
                CallId::from_wire("inspect"),
                ToolName::new("read").unwrap(),
                "contents",
            ),
        }),
        cx,
    );
    event(
        &controller,
        E::Turn(TurnEvent::ModelCallStarted {
            turn_id: id,
            call: 2,
        }),
        cx,
    );
    stream(&controller, id, thinking("Check the reported path"), cx);
    render(window, cx);
    let initial = data(&view, cx);
    let [prompt, first, step, later] = initial.as_slice() else {
        panic!("expected the live thought after the read step")
    };
    assert!(matches!(prompt.data.kind, BlockKind::Prompt { .. }));
    assert!(matches!(first.data.kind, BlockKind::ThoughtGroup(_)));
    assert!(!first.expanded);
    assert!(matches!(step.data.kind, BlockKind::Step(Step::Tool(_))));
    assert!(matches!(later.data.kind, BlockKind::ThoughtGroup(_)));
    assert_eq!(later.phase, Phase::Live);
    assert!(later.expanded);
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&later.data.key, cx)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(later.data.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    assert!(!thoughts(&view, cx)[1].expanded);
    cx.update_window(window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    stream(
        &controller,
        id,
        thinking("Check the reported path and revision"),
        cx,
    );
    assert!(thoughts(&view, cx)[1].expanded);
    event(
        &controller,
        E::Turn(TurnEvent::Completed {
            turn_id: id,
            message: thinking("Check the reported path and revision"),
            attempt_id: None,
        }),
        cx,
    );
    render(window, cx);
    let completed = data(&view, cx);
    let [_, completed_first, completed_step, committed] = completed.as_slice() else {
        panic!("expected both thought groups to stay in order on commit")
    };
    assert_eq!(completed_first.data.key, first.data.key);
    assert_eq!(completed_first.data.primary_copy(), "Read the source");
    assert!(!completed_first.expanded);
    assert_eq!(completed_step.data.key, step.data.key);
    assert_eq!(committed.phase, Phase::Finished);
    assert_ne!(committed.data.key, later.data.key);
    assert!(committed.expanded, "the deliberate reopen survives commit");
    assert_eq!(
        committed.data.primary_copy(),
        "Check the reported path and revision"
    );
    assert_eq!(
        identity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&committed.data.key, cx)
            .unwrap())
    );
    cx.update_window(window.into(), |_, window, cx| {
        let handle = view
            .read(cx)
            .transcript
            .read(cx)
            .test_focus(&committed.data.key, cx);
        window.focus(&handle, cx);
        window.press(Shortcut::COPY_DISCLOSURE.0, cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
            Some("Check the reported path and revision")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn blank_click_clears_block_focus_and_keeps_window_shortcuts(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup_at_size(cx, 1920., 1080.);
    controller.update(cx, |controller, cx| {
        controller
            .model
            .root
            .conversation
            .restore(vec![TranscriptItem::Message(thinking("Read the source"))]);
        cx.notify();
    });
    render(window, cx);
    let thought = thoughts(&view, cx).remove(0);
    cx.update_window(window.into(), |_, window, cx| {
        let disclosure_focus = view
            .read(cx)
            .transcript
            .read(cx)
            .test_focus(&thought.data.key, cx);
        window
            .within(thought.data.key.clone())
            .click("block-disclosure", cx);
        assert!(disclosure_focus.is_focused(window));
        assert!(!view.read(cx).window_focus.is_focused(window));

        // The centered column ends at x = 1588; this is empty pane space.
        let blank = gpui_kit::point(px(1800.), px(300.));
        window.click_at("zevria-desktop", blank, cx);
        assert!(!disclosure_focus.is_focused(window));
        assert!(view.read(cx).window_focus.is_focused(window));
        window.press(Shortcut::TOGGLE_MODE.0, cx);

        window.click_at("composer-panel", gpui_kit::point(px(4.), px(4.)), cx);
        assert_eq!(window.find("composer-panel").focused(), Some(true));
        assert!(!view.read(cx).window_focus.is_focused(window));
        window.click_at("zevria-desktop", blank, cx);
        assert_eq!(window.find("composer-panel").focused(), Some(false));
        assert!(view.read(cx).window_focus.is_focused(window));
    })
    .unwrap();
    cx.run_until_parked();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Manage(zevria_session_api::ManagementCommand::SetMode {
            mode: SessionMode::Plan,
            ..
        })
    ));
    assert!(receiver.try_recv().is_err());
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
        assert!(window.try_find("block-disclosure").is_none());
    })
    .unwrap();
    assert!(data(&view, cx).iter().any(
        |b| matches!(b.data.kind, BlockKind::Live) && &*b.data.body == "Waiting for the model"
    ));
}

#[gpui_kit::test]
fn message_only_stop_keeps_unsaved_partial_before_a_neutral_stopped_node_but_not_restore(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, _receiver) = setup(cx);
    let id = TurnId::new(105);
    start(&controller, id, cx);
    stream(&controller, id, thinking("Partial provider thought"), cx);
    render(window, cx);
    event(
        &controller,
        E::Turn(TurnEvent::Cancelled { turn_id: id }),
        cx,
    );
    render(window, cx);
    let data = data(&view, cx);
    let index = data
        .iter()
        .position(|b| b.data.thought().is_some())
        .unwrap();
    assert_eq!(data[index].phase, Phase::Unsaved);
    assert!(!data[index].expanded);
    assert_eq!(data[index].data.primary_copy(), "Partial provider thought");
    assert!(matches!(
        data[index + 1].data.kind,
        BlockKind::Notice(NoticeLevel::Warning)
    ));
    assert_eq!(&*data[index + 1].data.body, "Stopped");
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
    let (window, view, controller, _receiver) = setup(cx);
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
        SessionStreamState::new(
            id,
            AssistantStreamSnapshot {
                message: None,
                attempt: Some(attempt.clone()),
            },
        ),
        cx,
    );
    render(window, cx);
    let before = thoughts(&view, cx).remove(0);
    assert!(before.expanded);
    assert_eq!(before.phase, Phase::Live);
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(before.data.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    render(window, cx);
    assert!(!thoughts(&view, cx)[0].expanded);
    let AssistantPresentationContent::Reasoning { text } = &mut attempt.presentation[0].content
    else {
        unreachable!()
    };
    text.push_str("\n\nRevised in place");
    attempt.touch();
    attempt.close(ResponseAttemptOutcome::Interrupted);
    let saved = zevria_transcript::IncompleteResponse::from_attempt(&attempt)
        .unwrap()
        .unwrap();
    event(
        &controller,
        E::Turn(TurnEvent::ResponseAttemptUpdated {
            turn_id: id,
            attempt: Box::new(attempt),
        }),
        cx,
    );
    event(
        &controller,
        E::Turn(TurnEvent::Cancelled { turn_id: id }),
        cx,
    );
    render(window, cx);
    let interrupted = thoughts(&view, cx).remove(0);
    assert_eq!(before.data.key, interrupted.data.key);
    assert_eq!(interrupted.phase, Phase::Interrupted);
    assert!(!interrupted.expanded);
    assert!(interrupted.data.primary_copy().contains("Revised in place"));
    assert!(controller.read_with(cx, |c, _| c.model.root.unsaved_blocks().is_empty()));
    assert!(
        data(&view, cx)
            .iter()
            .any(|b| &*b.data.body == "Stopped before the response finished")
    );
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            TranscriptItem::Message(Message::user("Trace the source")),
            TranscriptItem::IncompleteResponse(Box::new(saved)),
        ]);
        cx.notify();
    });
    let restored = thoughts(&view, cx).remove(0);
    assert_eq!(
        restored.data.primary_copy(),
        "Readable attempt\n\nRevised in place"
    );
    assert_eq!(restored.phase, Phase::Interrupted);
    assert!(!restored.expanded);
}

#[gpui_kit::test]
fn failed_message_tail_is_retained_but_a_foreign_terminal_cannot_capture_it(
    cx: &mut TestAppContext,
) {
    let (_, view, controller, _receiver) = setup(cx);
    let id = TurnId::new(109);
    start(&controller, id, cx);
    stream(&controller, id, thinking("Partial before failure"), cx);
    event(
        &controller,
        E::Turn(TurnEvent::Cancelled {
            turn_id: TurnId::new(999),
        }),
        cx,
    );
    assert!(controller.read_with(cx, |c, _| c.model.root.unsaved_blocks().is_empty()));
    event(
        &controller,
        E::Turn(TurnEvent::Failed {
            turn_id: id,
            error: "Connection closed".into(),
        }),
        cx,
    );
    let blocks = data(&view, cx);
    let index = blocks
        .iter()
        .position(|b| b.data.thought().is_some())
        .unwrap();
    assert_eq!(blocks[index].phase, Phase::Unsaved);
    assert!(!blocks[index].expanded);
    assert!(matches!(
        blocks[index + 1].data.kind,
        BlockKind::Notice(NoticeLevel::Error)
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
        E::Question(QuestionEvent::Asked {
            turn_id: id,
            request: QuestionRequest {
                allow_choice_notes: false,
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
        }),
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
    assert!(!controller.read_with(cx, |c, _| {
        c.model.controls.question.as_ref().unwrap().pending
    }));
    assert!(receiver.try_recv().is_err());
    #[cfg(windows)]
    cx.update_window(window.into(), |_, window, cx| {
        use gpui_kit::Focusable as _;
        let input = view.read(cx).question_inputs[0].clone();
        input.update(cx, |input, cx| input.focus(window, cx));
        window.input("Answer", cx);
        assert!(input.focus_handle(cx).is_focused(window));
    })
    .unwrap();
    #[cfg(not(windows))]
    controller.update(cx, |c, cx| {
        c.model.controls.question.as_mut().unwrap().answers[0].text = "Answer".into();
        cx.notify();
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::CONFIRM.0, cx)
    })
    .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Control(ControlCommand::AnswerQuestion { .. })
    ));
    event(
        &controller,
        E::Question(QuestionEvent::Closed {
            turn_id: id,
            request_id,
        }),
        cx,
    );
    event(
        &controller,
        E::Turn(TurnEvent::Completed {
            turn_id: id,
            message: Message::assistant("Done"),
            attempt_id: None,
        }),
        cx,
    );
    expect_skill_completion_query(&mut receiver);
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
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version,
                    title: "Plan".into(),
                    markdown: "Review the source".into(),
                    source_turn_id: id,
                },
            }),
        }),
        cx,
    );
    render(window, cx);
    assert!(view.read_with(cx, |view, _| view.plan_overlay));
    cx.update_window(window.into(), |_, window, cx| {
        let panel = window.find("plan-review").bounds();
        assert!(
            panel.origin.x >= px(0.) && panel.right() <= px(720.) && panel.bottom() <= px(520.)
        );
        window.press(Shortcut::CONFIRM.0, cx);
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
fn subtasks_panel_navigation_inspects_children_without_resetting_disclosure(
    cx: &mut TestAppContext,
) {
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
    let root_key = thoughts(&view, cx)[0].data.key.clone();
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&root_key, cx)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(root_key.clone())
            .click("block-disclosure", cx);
        window.press(Shortcut::TOGGLE_SUBTASKS.0, cx);
    })
    .unwrap();
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("subtasks-panel").visible());
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.controls.selected_child.clone()),
        Some(id)
    );
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("subtask-row-child").focused().unwrap_or(false));
        window.press("up", cx);
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| c.model.controls.selected_child.is_none()));
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
        c.model.controls.selected_child = Some(child_id.clone());
        cx.notify();
    });
    let id = TurnId::new(112);
    start(&controller, id, cx);
    let request_id = QuestionRequestId::new("root-owned");
    event(
        &controller,
        E::Question(QuestionEvent::Asked {
            turn_id: id,
            request: QuestionRequest {
                allow_choice_notes: false,
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
        }),
        cx,
    );
    controller.update(cx, |c, cx| {
        c.model.controls.question.as_mut().unwrap().answers[0].text = "Yes".into();
        cx.notify();
    });
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("question-dialog").visible());
        window.press(Shortcut::CONFIRM.0, cx);
    })
    .unwrap();
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Control(ControlCommand::AnswerQuestion { request_id: actual, .. }) if actual == request_id)
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.controls.selected_child.clone()),
        Some(child_id)
    );
}

#[gpui_kit::test]
fn stop_shortcut_works_even_with_a_non_dismissible_question(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let id = TurnId::new(111);
    start(&controller, id, cx);
    event(
        &controller,
        E::Question(QuestionEvent::Asked {
            turn_id: id,
            request: QuestionRequest {
                allow_choice_notes: false,
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
        }),
        cx,
    );
    render(window, cx);
    cx.update_window(window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();
    assert!(receiver.try_recv().is_err());
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::STOP.0, cx)
    })
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
