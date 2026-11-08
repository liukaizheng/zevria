//! Headless tests exercise real native controls and production window content.
use super::{theme, window::DesktopWindow};
use crate::{actions, controller::SessionController};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, Point, TestAppContext, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use rig_core::message::Message;
use zevria_foundation::{
    QuestionPrompt, QuestionPromptKind, QuestionRequest, QuestionRequestId, SessionMode, TurnId,
};
use zevria_session_api::{ControlCommand, SessionCommand, SessionEvent as E, TurnCommand};
use zevria_workflow::{PlanArtifact, PlanDecision, PlanId, PlanVersion, PlanWorkflowState};
type Window = WindowHandle<gpui_kit::base::Root>;
type Receiver = tokio::sync::mpsc::UnboundedReceiver<SessionCommand>;
fn setup(
    cx: &mut TestAppContext,
) -> (
    Window,
    Entity<DesktopWindow>,
    Entity<SessionController>,
    Receiver,
) {
    setup_at_size(cx, 1240., 840.)
}
fn setup_at_size(
    cx: &mut TestAppContext,
    width: f32,
    height: f32,
) -> (
    Window,
    Entity<DesktopWindow>,
    Entity<SessionController>,
    Receiver,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (window, view, controller) = cx.update(|cx| {
        gpui_kit::init(cx);
        theme::apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
        actions::install(cx);
        let controller = cx.new(|cx| SessionController::fixture(sender, cx));
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(width), px(height)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| DesktopWindow::new(controller.clone(), window, cx)),
        )
        .unwrap();
        (
            window.downcast::<gpui_kit::base::Root>().unwrap(),
            view,
            controller,
        )
    });
    (window, view, controller, receiver)
}
fn event(controller: &Entity<SessionController>, event: E, cx: &mut TestAppContext) {
    controller.update(cx, |this, cx| this.fixture_event(event, cx));
    cx.run_until_parked();
}
#[gpui_kit::test]
fn restored_transcript_renders_and_enter_emits_submit(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    controller.update(cx, |this, cx| {
        this.model.root.conversation.restore(vec![
            zevria_transcript::transcript::TranscriptItem::Message(Message::user("restored")),
            zevria_transcript::transcript::TranscriptItem::Message(Message::assistant(
                "**answer**",
            )),
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        view.update(cx, |this, cx: &mut Context<DesktopWindow>| {
            this.composer
                .update(cx, |input, cx| input.focus(window, cx))
        });
        window.input("hello", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        matches!(receiver.try_recv().unwrap(),SessionCommand::Turn(TurnCommand::Submit{text,mode:SessionMode::Build,..})if text.display_projection()=="hello")
    );
}
#[gpui_kit::test]
fn busy_stop_and_escape_use_the_promoted_turn_id(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::TurnStarted {
            turn_id: TurnId::new(8),
            mode: SessionMode::Build,
            message: Message::user("busy"),
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Control(ControlCommand::CancelTurn {
            turn_id: Some(TurnId::new(8))
        })
    );
    assert!(controller.read_with(cx, |this, _| this.cancelling));
}
#[gpui_kit::test]
fn plan_panel_emits_versioned_resolution(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let version = PlanVersion {
        id: PlanId::new(),
        revision: 4,
    };
    event(
        &controller,
        E::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version,
                    title: "Plan".into(),
                    markdown: "# Plan".into(),
                    source_turn_id: TurnId::new(1),
                },
            },
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("plan-implement", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Turn(TurnCommand::ResolvePlan {
            expected: version,
            decision: PlanDecision::ImplementCurrent
        })
    );
}
#[gpui_kit::test]
fn restored_math_and_plan_document_and_panel_render_at_both_widths(cx: &mut TestAppContext) {
    const PROOF: &str = include_str!("../../tests/fixtures/holder-proof.md");
    for width in [1240., 720.] {
        let (window, _, controller, _receiver) = setup_at_size(cx, width, 1100.);
        controller.update(cx, |c, cx| {
            c.model.root.conversation.restore(vec![
                zevria_transcript::transcript::TranscriptItem::Message(Message::user(
                    "Prove Hölder’s inequality.",
                )),
                zevria_transcript::transcript::TranscriptItem::Message(Message::assistant(PROOF)),
            ]);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        event(
            &controller,
            E::PlanStateChanged {
                state: PlanWorkflowState::Ready {
                    artifact: PlanArtifact {
                        version: PlanVersion {
                            id: PlanId::new(),
                            revision: 1,
                        },
                        title: "Math plan".into(),
                        markdown: PROOF.into(),
                        source_turn_id: TurnId::new(1),
                    },
                },
            },
            cx,
        );
        let key = controller.read_with(cx, |c, _| {
            let conversation = &c.model.root.conversation;
            let index = conversation
                .history()
                .iter()
                .position(|e| {
                    matches!(
                        e,
                        zevria_presentation::conversation::HistoryEntry::PlanArtifact(_)
                    )
                })
                .unwrap();
            format!("{:?}-row-{index}", conversation.epoch())
        });
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update_window(window.into(), |_, window, cx| {
            window.within(key).click("expand", cx);
            window.click("plan-copy", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|i| i.text()),
                Some(PROOF.to_owned())
            );
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}
#[gpui_kit::test]
fn question_dialog_emits_answer_question(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let turn_id = TurnId::new(9);
    event(
        &controller,
        E::TurnStarted {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("ask"),
        },
        cx,
    );
    let request_id = QuestionRequestId::new("question");
    event(
        &controller,
        E::QuestionAsked {
            turn_id,
            request: QuestionRequest {
                id: request_id.clone(),
                source_label: None,
                dismissible: true,
                questions: vec![QuestionPrompt {
                    id: "answer".into(),
                    header: "Text".into(),
                    question: "Tell me".into(),
                    kind: QuestionPromptKind::Text {
                        min_length: None,
                        max_length: None,
                    },
                    options: Vec::new(),
                    required: true,
                    default: None,
                }],
            },
        },
        cx,
    );
    controller.update(cx, |this, cx| {
        this.model.question.as_mut().unwrap().answers[0].text = "Answer".into();
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("answer-question", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        matches!(receiver.try_recv().unwrap(),SessionCommand::Control(ControlCommand::AnswerQuestion{request_id:id,..})if id==request_id)
    );
}
#[gpui_kit::test]
fn rejected_submit_preserves_the_draft_and_unlocks(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("retain this", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    receiver.try_recv().unwrap();
    event(
        &controller,
        E::TurnRejected {
            turn_id: TurnId::new(10),
            error: "not accepted".into(),
        },
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |this, _| this.draft.clone()),
        "retain this"
    );
    assert!(!controller.read_with(cx, |this, _| this.model.root.is_busy()));
    assert_eq!(
        view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string()),
        "retain this"
    );
}
#[gpui_kit::test]
fn shift_enter_never_submits_and_acceptance_does_not_overwrite_a_new_draft(
    cx: &mut TestAppContext,
) {
    let (window, _, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("line", cx);
        window.press("shift-enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(receiver.try_recv().is_err());
    controller.update(cx, |this, cx| {
        this.draft = "submitted".into();
        this.orchestrate = true;
        this.submit(cx);
        this.draft = "new draft".into();
    });
    receiver.try_recv().unwrap();
    event(
        &controller,
        E::TurnStarted {
            turn_id: TurnId::new(11),
            mode: SessionMode::Build,
            message: Message::user("submitted"),
        },
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |this, _| this.draft.clone()),
        "new draft"
    );
    assert!(!controller.read_with(cx, |this, _| this.orchestrate));
}

#[gpui_kit::test]
fn native_close_is_vetoed_and_drops_the_frontend_endpoint(cx: &mut TestAppContext) {
    let (window, _, _, receiver) = setup(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window.into(), cx);
    assert!(!visual.simulate_close());
    assert!(receiver.is_closed());
}
#[gpui_kit::test]
fn enter_on_a_shared_builtin_completion_selects_mode_once(cx: &mut TestAppContext) {
    let (window, _, _, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("/plan", cx);
        window.press("enter", cx);
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
fn shared_palette_drives_syntax_and_native_diff_card_expansion(cx: &mut TestAppContext) {
    use rig_core::message::{
        AssistantContent, AssistantMessage, CallId, ToolCall, ToolFunction, ToolName,
    };
    use zevria_foundation::{
        FileChange, FileChangeOutput, ToolCallOutcome, ToolResultDetail, ToolResultMetadata,
    };
    let (window, _, controller, _) = setup(cx);
    cx.update(|cx| {
        let t = theme::palette(cx);
        let syntax = &gpui_kit::component::Theme::global(cx)
            .highlight_theme
            .style
            .syntax;
        assert_eq!(
            syntax.style("keyword").unwrap().color,
            Some(t.syntax.keyword)
        );
        assert_eq!(syntax.style("string").unwrap().color, Some(t.syntax.string));
    });
    let turn_id = TurnId::new(12);
    event(
        &controller,
        E::TurnStarted {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("edit file"),
        },
        cx,
    );
    let call = ToolCall::new(
        CallId::from_wire("edit"),
        ToolFunction::new(
            ToolName::new("edit").unwrap(),
            serde_json::json!({"file_path":"example.rs","old_string":"old","new_string":"new"}),
        ),
    );
    event(
        &controller,
        E::Intermediate {
            turn_id,
            message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            attempt_id: None,
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    event(
        &controller,
        E::ToolResults {
            turn_id,
            message: Message::tool_result(
                CallId::from_wire("edit"),
                ToolName::new("edit").unwrap(),
                "edited",
            ),
            metadata: vec![ToolResultMetadata {
                id: "edit".into(),
                call_id: None,
                tool_name: "edit".into(),
                outcome: ToolCallOutcome::Success,
                diagnostic: None,
                detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
                    path: "example.rs".into(),
                    change: FileChange::Update {
                        unified_diff:
                            "--- a/example.rs\n+++ b/example.rs\n@@ -1 +1 @@\n-old\n+new\n".into(),
                        move_path: None,
                    },
                }])),
            }],
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("expand", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn completion_arrows_and_tab_use_the_shared_registry_without_extra_turns(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| window.input("/", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.press("down", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |this, _| this.completion_selected), 1);
    cx.update_window(window.into(), |_, window, cx| window.press("down", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    let SessionCommand::Manage(zevria_session_api::ManagementCommand::SetMode { request_id, mode }) =
        receiver.try_recv().unwrap()
    else {
        panic!("one mode command")
    };
    assert_eq!(mode, SessionMode::Build);
    event(
        &controller,
        E::ModeResult {
            request_id,
            result: zevria_session_api::ModeSelectionResult::Accepted {
                mode,
                changed: false,
            },
        },
        cx,
    );
    // Tab only inserts a completion; it does not submit a second operation.
    cx.update_window(window.into(), |_, window, cx| window.input("/p", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.press("tab", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string()),
        "/plan "
    );
    assert_eq!(
        view.read_with(cx, |this, cx| this.composer.read(cx).cursor()),
        6
    );
    assert!(view.read_with(cx, |this, cx| this.completions(cx).is_empty()));
}

#[gpui_kit::test]
fn editing_and_undoing_does_not_revive_a_submitted_draft_token(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("original", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    receiver.try_recv().unwrap();
    let turn_id = TurnId::new(13);
    event(
        &controller,
        E::TurnStarted {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("original"),
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| window.input("later", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.press("cmd-z", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(controller.read_with(cx, |this, _| this.draft.is_empty()));
    event(
        &controller,
        E::TurnFailed {
            turn_id,
            error: "fixture failure".into(),
        },
        cx,
    );
    assert!(controller.read_with(cx, |this, _| this.draft.is_empty()));
    assert_eq!(
        controller.read_with(cx, |this, _| this.recovery.clone()),
        vec!["original".to_string()]
    );
}

#[gpui_kit::test]
fn compact_minimum_window_keeps_composer_controls_inside_the_viewport(cx: &mut TestAppContext) {
    let (window, view, _, _) = setup_at_size(cx, 720., 520.);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let send = window.find("send").bounds();
        assert!(send.origin.x >= px(0.) && send.right() <= px(720.));
        assert!(send.origin.y >= px(40.) && send.bottom() <= px(492.));
        assert!(window.find("mode-build").visible());
        assert!(window.find("mode-plan").visible());
    })
    .unwrap();
    assert!(view.read_with(cx, |v, _| v.compact));
    assert!(!view.read_with(cx, |v, _| v.sidebar_visible(720.)));
}

#[gpui_kit::test]
fn compact_segmented_modes_emit_the_same_authoritative_mode_commands(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup_at_size(cx, 720., 840.);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("mode-plan", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let SessionCommand::Manage(zevria_session_api::ManagementCommand::SetMode { request_id, mode }) =
        receiver.try_recv().unwrap()
    else {
        panic!("mode command");
    };
    assert_eq!(mode, SessionMode::Plan);
    event(
        &controller,
        E::ModeResult {
            request_id,
            result: zevria_session_api::ModeSelectionResult::Accepted {
                mode,
                changed: true,
            },
        },
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.root.next_mode()),
        SessionMode::Plan
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("mode-build", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Manage(zevria_session_api::ManagementCommand::SetMode {
            mode: SessionMode::Build,
            ..
        })
    ));
    assert!(receiver.try_recv().is_err());
}

#[gpui_kit::test]
fn width_class_changes_remeasure_retained_gutter_rows(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            zevria_transcript::transcript::TranscriptItem::Message(Message::user("A short prompt")),
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    let key = controller.read_with(cx, |c, _| {
        let conversation = &c.model.root.conversation;
        let zevria_presentation::conversation::HistoryEntry::Conversation(entry) =
            &conversation.history()[0]
        else {
            panic!("prompt entry");
        };
        format!("{:?}-{}", conversation.epoch(), entry.blocks[0].id.0)
    });
    let wide = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.find(key.clone()).bounds().size.height
        })
        .unwrap();
    cx.simulate_window_resize(window.into(), size(px(720.), px(840.)));
    cx.run_until_parked();
    let compact = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.find(key.clone()).bounds().size.height
        })
        .unwrap();
    assert!(view.read_with(cx, |v, _| v.compact));
    assert!(
        compact > wide,
        "compact {compact:?} should include the hanging label above prose, wide {wide:?}"
    );
    cx.simulate_window_resize(window.into(), size(px(1240.), px(840.)));
    cx.run_until_parked();
    let restored = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.find(key).bounds().size.height
        })
        .unwrap();
    assert!(!view.read_with(cx, |v, _| v.compact));
    assert_eq!(restored, wide);
}

#[gpui_kit::test]
fn reduce_motion_freezes_the_shared_work_glyph(cx: &mut TestAppContext) {
    let _ = setup(cx);
    cx.update(|cx| {
        cx.set_reduce_motion(true);
        assert_eq!(super::style::frame(cx), 0);
        assert_eq!(
            zevria_presentation::status_icon::StatusIcon::Running.glyph(super::style::frame(cx)),
            "◐"
        );
    });
}

#[gpui_kit::test]
fn sidebar_toggle_is_ui_local_and_obeys_the_automatic_breakpoint(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    assert!(view.read_with(cx, |v, _| v.sidebar_visible(900.)));
    assert!(!view.read_with(cx, |v, _| v.sidebar_visible(899.)));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("toggle-sidebar", cx);
    })
    .unwrap();
    assert_eq!(view.read_with(cx, |v, _| v.sidebar_override), Some(false));
    cx.update_window(window.into(), |_, window, cx| {
        window.press("ctrl-cmd-s", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.sidebar_override), Some(true));
    assert!(view.read_with(cx, |v, _| v.sidebar_visible(720.)));
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        controller.read_with(cx, |c, _| c.phase.clone()),
        crate::controller::Phase::Live
    );
}

#[gpui_kit::test]
fn completion_keyboard_selection_has_a_rendered_selected_state(cx: &mut TestAppContext) {
    let (window, view, _, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| window.input("/", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("completion-state", 0usize)).selected(),
            Some(true)
        );
        assert_eq!(
            window.find(("completion-state", 1usize)).selected(),
            Some(false)
        );
        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find(("completion-state", 0usize)).selected(),
            Some(false)
        );
        assert_eq!(
            window.find(("completion-state", 1usize)).selected(),
            Some(true)
        );
    })
    .unwrap();
    assert_eq!(view.read_with(cx, |v, _| v.completion_selected), 1);
    assert!(receiver.try_recv().is_err());
}

#[gpui_kit::test]
fn radio_question_options_other_and_optional_skip_keep_the_authoritative_form(
    cx: &mut TestAppContext,
) {
    let (window, _, controller, _) = setup(cx);
    let turn_id = TurnId::new(20);
    event(
        &controller,
        E::TurnStarted {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("choose"),
        },
        cx,
    );
    event(
        &controller,
        E::QuestionAsked {
            turn_id,
            request: QuestionRequest {
                id: QuestionRequestId::new("choice"),
                source_label: Some("Explore".into()),
                dismissible: true,
                questions: vec![QuestionPrompt {
                    id: "style".into(),
                    header: "Style".into(),
                    question: "Which style?".into(),
                    kind: QuestionPromptKind::SingleSelect { allow_other: true },
                    options: vec![zevria_foundation::QuestionOption {
                        label: "Gutter".into(),
                        description: "Compact log rows".into(),
                    }],
                    required: false,
                    default: None,
                }],
            },
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("question-option", 0usize)).role(),
            Some(gpui_kit::Role::RadioButton)
        );
        window.click(("question-option", 0usize), cx);
    })
    .unwrap();
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.question.as_ref().unwrap().answers[0]
            .selected
            .clone()),
        vec![0]
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("question-other", 0usize), cx)
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| {
        c.model.question.as_ref().unwrap().answers[0].other
    }));
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("question-skip", 0usize), cx)
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| {
        c.model.question.as_ref().unwrap().answers[0].skip
    }));
}
