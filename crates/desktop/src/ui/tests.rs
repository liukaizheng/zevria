//! Headless tests exercise real native controls and production window content.
#[path = "job_tests.rs"]
mod jobs;
#[path = "model_tests.rs"]
mod models;
#[path = "prompt_preview_tests.rs"]
mod prompt_preview;
#[path = "subtask_tests.rs"]
mod subtasks;
#[path = "turn_tests.rs"]
mod turns;
#[path = "web_tests.rs"]
mod web;
#[cfg(windows)]
#[path = "windows_tests.rs"]
mod windows;
#[path = "thread_tests.rs"]
mod working_thread;
use super::{theme, window::DesktopWindow};
use crate::{
    actions::{self, Shortcut},
    controller::SessionController,
};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, Point, TestAppContext, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use rig_core::message::Message;
use zevria_foundation::{
    QuestionAnswer, QuestionOption, QuestionPrompt, QuestionPromptKind, QuestionRequest,
    QuestionRequestId, QuestionResponse, SessionMode, TurnId,
};
use zevria_session_api::{ControlCommand, SessionCommand, SessionEvent as E, TurnCommand};
use zevria_session_api::{ManagementEvent, PlanEvent, QuestionEvent, TurnEvent};
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
fn event(
    controller: &Entity<SessionController>,
    event: impl Into<zevria_session_api::SessionUpdate>,
    cx: &mut TestAppContext,
) {
    controller.update(cx, |this, cx| this.fixture_event(event, cx));
    cx.run_until_parked();
}

fn expect_skill_completion_query(receiver: &mut Receiver) {
    use zevria_instructions::skill::SkillManagementRequest;
    use zevria_session_api::ManagementCommand;

    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Manage(ManagementCommand::Skills {
            request: SkillManagementRequest::List { query }, ..
        }) if query.is_empty()
    ));
}

mod status {
    use super::*;
    use crate::ui::status::StatusLine;
    use std::time::Duration;
    use zevria_foundation::{ModelProfileRef, ModelRole};
    use zevria_model::{ContextTokenSnapshot, ContextTokenSource, TokenUsage};
    use zevria_session_api::event::{NetworkStatus, NetworkTransport};

    const TURN: TurnId = TurnId::new(1);

    fn start(controller: &Entity<SessionController>, cx: &mut TestAppContext) {
        event(
            controller,
            E::Turn(TurnEvent::Started {
                turn_id: TURN,
                mode: SessionMode::Build,
                message: Message::user("Check cache usage"),
            }),
            cx,
        );
        event(
            controller,
            E::Turn(TurnEvent::ModelCallStarted {
                turn_id: TURN,
                call: 1,
            }),
            cx,
        );
    }

    fn usage_event(profile: &str, input_tokens: u64, cached_tokens: u64, output_tokens: u64) -> E {
        E::Turn(TurnEvent::UsageUpdated {
            turn_id: TURN,
            usage: TokenUsage {
                input_tokens,
                cached_tokens,
                output_tokens,
                total_tokens: input_tokens + output_tokens,
            },
            profile: ModelProfileRef::new("fixture", profile),
            model_role: ModelRole::Build,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        })
    }

    fn context_event(profile: &str, tokens: u64) -> E {
        E::Turn(TurnEvent::ContextUsageUpdated {
            turn_id: TURN,
            snapshot: Box::new(ContextTokenSnapshot {
                profile: ModelProfileRef::new("fixture", profile),
                model_role: ModelRole::Build,
                projected_input_tokens: Some(tokens),
                known_input_tokens: tokens,
                unresolved_media: Default::default(),
                source: ContextTokenSource::UsagePlusDelta,
                automatic_trigger: 80_000,
                input_token_limit: 100_000,
                context_window_tokens: 128_000,
            }),
        })
    }

    fn assert_response(
        window: Window,
        controller: &Entity<SessionController>,
        cache: Option<&str>,
        total: Option<&str>,
        cx: &mut TestAppContext,
    ) {
        controller.read_with(cx, |c, _| {
            let status = StatusLine::new(c.model.visible(), c.cancelling);
            assert_eq!(status.cache.as_deref(), cache);
            assert_eq!(status.total.as_deref(), total);
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.try_find("status-cache").is_some(), cache.is_some());
            assert_eq!(window.try_find("status-total").is_some(), total.is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn cache_segment_obeys_compact_policy_and_keeps_status_inside_viewport(
        cx: &mut TestAppContext,
    ) {
        // At 1016px the sidebar leaves exactly 760px: the first expanded width.
        let mut activity_width = None;
        let mut response_widths = None;
        for width in [1920., 1240., 1016., 720.] {
            let (window, view, controller, _) = setup_at_size(cx, width, 840.);
            start(&controller, cx);
            event(&controller, context_event("model", 12_300), cx);
            event(&controller, usage_event("model", 12_300, 10_200, 2_100), cx);
            event(
                &controller,
                E::Turn(TurnEvent::NetworkStatus {
                    turn_id: TURN,
                    call: 1,
                    attempt: 1,
                    transport: NetworkTransport::Http,
                    status: NetworkStatus::Quiet {
                        idle_for: Duration::from_secs(45),
                        retry_in: Duration::from_secs(15),
                        will_retry: true,
                    },
                }),
                cx,
            );
            controller.read_with(cx, |c, _| {
                let status = StatusLine::new(c.model.visible(), c.cancelling);
                assert_eq!(status.cache.as_deref(), Some("cached 10.2k (83%)"));
                assert_eq!(status.total.as_deref(), Some("total 14.4k"));
                assert!(status.network.unwrap().starts_with("No response progress"));
            });
            cx.update_window(window.into(), |_, window, cx| {
                window.render_frame(cx);
                let compact = view.read(cx).compact;
                assert_eq!(compact, width == 720.);
                let bar = window.find("status-bar").bounds();
                assert!(bar.origin.x >= px(0.) && bar.right() <= window.viewport_size().width);
                // In-bounds boxes alone can miss activity squeezed to just an ellipsis.
                let activity = window.find("status-activity").bounds().size.width;
                assert_eq!(
                    *activity_width.get_or_insert(activity),
                    activity,
                    "{width}px activity"
                );
                let mut segments = vec![
                    "status-activity",
                    "status-context",
                    "status-context-label",
                    "status-network",
                ];
                if compact {
                    assert!(window.try_find("status-cache").is_none());
                    assert!(window.try_find("status-total").is_none());
                } else {
                    segments.extend([
                        "status-context-gauge",
                        "status-context-fill",
                        "status-cache",
                        "status-total",
                    ]);
                    let gauge = window.find("status-context-gauge").bounds();
                    let fill = window.find("status-context-fill").bounds();
                    assert!(gauge.size.width <= px(64.), "{width}px gauge {gauge:?}");
                    assert!(fill.origin.x >= gauge.origin.x && fill.right() <= gauge.right());
                    let cache = window.find("status-cache").bounds();
                    let total = window.find("status-total").bounds();
                    let widths = (cache.size.width, total.size.width);
                    assert_eq!(
                        *response_widths.get_or_insert(widths),
                        widths,
                        "{width}px response"
                    );
                    assert!(cache.right() <= total.origin.x);
                }
                for id in segments {
                    let segment = window.find(id);
                    let bounds = segment.bounds();
                    assert!(
                        segment.visible() && bounds.size.width > px(0.),
                        "{width}px: {id} {bounds:?}; context {:?}; network {:?}; bar {bar:?}",
                        window.find("status-context").bounds(),
                        window.find("status-network").bounds(),
                    );
                    assert!(
                        bounds.origin.x >= bar.origin.x && bounds.right() <= bar.right(),
                        "{width}px: {id} {bounds:?} outside {bar:?}"
                    );
                    assert!(
                        bounds.origin.y >= bar.origin.y && bounds.bottom() <= bar.bottom(),
                        "{width}px: {id} {bounds:?} outside {bar:?}"
                    );
                }
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn cache_segment_tracks_responses_without_context_profile_or_role_leaks(
        cx: &mut TestAppContext,
    ) {
        let (window, _, controller, _receiver) = setup(cx);
        start(&controller, cx);
        event(&controller, context_event("model", 12_300), cx);
        assert_response(window, &controller, None, None, cx);
        event(&controller, usage_event("model", 12_300, 10_200, 2_100), cx);
        assert_response(
            window,
            &controller,
            Some("cached 10.2k (83%)"),
            Some("total 14.4k"),
            cx,
        );

        event(
            &controller,
            E::Turn(TurnEvent::ModelCallStarted {
                turn_id: TURN,
                call: 2,
            }),
            cx,
        );
        event(&controller, usage_event("model", 20_000, 10_000, 3_000), cx);
        assert_response(
            window,
            &controller,
            Some("cached 10k (50%)"),
            Some("total 23k"),
            cx,
        );
        event(&controller, context_event("model", 41_000), cx);
        assert_response(
            window,
            &controller,
            Some("cached 10k (50%)"),
            Some("total 23k"),
            cx,
        );

        event(&controller, context_event("replacement", 5_000), cx);
        assert_response(window, &controller, None, None, cx);
        event(
            &controller,
            usage_event("replacement", 5_000, 5_000, 400),
            cx,
        );
        assert_response(
            window,
            &controller,
            Some("cached 5k (100%)"),
            Some("total 5.4k"),
            cx,
        );
        event(
            &controller,
            E::Turn(TurnEvent::Completed {
                turn_id: TURN,
                message: Message::assistant("Done"),
                attempt_id: None,
            }),
            cx,
        );
        assert_response(
            window,
            &controller,
            Some("cached 5k (100%)"),
            Some("total 5.4k"),
            cx,
        );
        event(
            &controller,
            E::Turn(TurnEvent::ModeChanged {
                mode: SessionMode::Plan,
            }),
            cx,
        );
        assert_response(window, &controller, None, None, cx);
    }
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
        E::Turn(TurnEvent::Started {
            turn_id: TurnId::new(8),
            mode: SessionMode::Build,
            message: Message::user("busy"),
        }),
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
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version,
                    title: "Plan".into(),
                    markdown: "# Plan".into(),
                    source_turn_id: TurnId::new(1),
                },
            }),
        }),
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
fn dismissed_ready_plan_send_revises_and_rejection_restores_the_draft(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.input("next draft", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let version = PlanVersion {
        id: PlanId::new(),
        revision: 4,
    };
    let ready = PlanWorkflowState::Ready {
        artifact: PlanArtifact {
            version,
            title: "Plan".into(),
            markdown: "# Plan".into(),
            source_turn_id: TurnId::new(1),
        },
    };
    event(
        &controller,
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(ready.clone()),
        }),
        cx,
    );
    controller.update(cx, |this, cx| this.submit(cx));
    assert!(
        receiver.try_recv().is_err(),
        "visible review requires an explicit choice"
    );
    assert!(!controller.read_with(cx, |this, _| this.model.root.is_busy()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(controller.read_with(cx, |this, _| this.model.root.workflow.dialog().is_none()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press(Shortcut::CONFIRM.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        receiver.try_recv().is_err(),
        "the decision shortcut cannot implement a hidden review"
    );
    // Ready forces Plan mode even if the old Build orchestration toggle remains set.
    controller.update(cx, |this, _| this.orchestrate = true);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("send", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Turn(TurnCommand::RevisePlanWithMessage {
            expected: version,
            text: "next draft".into(),
        })
    );
    assert!(receiver.try_recv().is_err());
    assert!(controller.read_with(cx, |this, _| this.model.root.is_busy()));
    event(
        &controller,
        E::Turn(TurnEvent::Rejected {
            turn_id: TurnId::new(10),
            error: "revision changed".into(),
        }),
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |this, _| this.draft.clone()),
        "next draft"
    );
    assert_eq!(
        view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string()),
        "next draft"
    );
    assert!(!controller.read_with(cx, |this, _| this.model.root.is_busy()));
    assert!(controller.read_with(cx, |this, _| this.model.root.workflow.dialog().is_none()));
    assert_eq!(
        controller.read_with(cx, |this, _| this.model.root.workflow.snapshot().clone()),
        ready
    );
}

#[gpui_kit::test]
fn blank_plan_panel_click_clears_button_focus(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    for width in [1240., 1920.] {
        let (window, view, controller, _) = setup_at_size(cx, width, 840.);
        event(
            &controller,
            E::Plan(PlanEvent::StateChanged {
                state: Box::new(PlanWorkflowState::Ready {
                    artifact: PlanArtifact {
                        version: PlanVersion {
                            id: PlanId::new(),
                            revision: 1,
                        },
                        title: "Plan".into(),
                        markdown: "Review the source".into(),
                        source_turn_id: TurnId::new(1),
                    },
                }),
            }),
            cx,
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(view.read(cx).plan_overlay, width == 1240.);
            window.click("plan-copy", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("Review the source".to_owned())
            );
            // Kit buttons suppress mouse-down focus; exercise an active ring via Tab.
            for _ in 0..32 {
                if window.find("plan-copy").focused() == Some(true) {
                    break;
                }
                window.focus_next(cx);
                window.render_frame(cx);
            }
            assert_eq!(window.find("plan-copy").focused(), Some(true));
            window.click("plan-copy", cx);
            assert_eq!(window.find("plan-copy").focused(), Some(true));
            assert!(!view.read(cx).window_focus.is_focused(window));
            window.click_at("plan-review", gpui_kit::point(px(6.), px(6.)), cx);
            assert!(!window.find("plan-copy").focused().unwrap());
            assert!(view.read(cx).window_focus.is_focused(window));
        })
        .unwrap();
    }
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
            E::Plan(PlanEvent::StateChanged {
                state: Box::new(PlanWorkflowState::Ready {
                    artifact: PlanArtifact {
                        version: PlanVersion {
                            id: PlanId::new(),
                            revision: 1,
                        },
                        title: "Math plan".into(),
                        markdown: PROOF.into(),
                        source_turn_id: TurnId::new(1),
                    },
                }),
            }),
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
            format!("{:?}-entry-{index}", conversation.epoch())
        });
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        cx.update_window(window.into(), |_, window, cx| {
            assert!(window.find(key).visible());
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
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("ask"),
        }),
        cx,
    );
    let request_id = QuestionRequestId::new("question");
    event(
        &controller,
        E::Question(QuestionEvent::Asked {
            turn_id,
            request: QuestionRequest {
                allow_choice_notes: false,
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
        }),
        cx,
    );
    controller.update(cx, |this, cx| {
        this.model.controls.question.as_mut().unwrap().answers[0].text = "Answer".into();
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
fn native_choice_note_editor_keeps_drafts_and_sends_the_chosen_note(cx: &mut TestAppContext) {
    use gpui_kit::Focusable as _;
    use gpui_kit::component::input::InputEvent;

    let (window, view, controller, mut receiver) = setup(cx);
    let turn_id = TurnId::new(9);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Plan,
            message: Message::user("ask"),
        }),
        cx,
    );
    let request_id = QuestionRequestId::new("native-note");
    let mut request = QuestionRequest {
        allow_choice_notes: true,
        id: request_id.clone(),
        source_label: None,
        dismissible: true,
        questions: vec![QuestionPrompt {
            id: "scope".into(),
            header: "Scope".into(),
            question: "Which scope?".into(),
            kind: QuestionPromptKind::SingleSelect { allow_other: true },
            options: vec![
                QuestionOption {
                    label: "Focused (Recommended)".into(),
                    description: "Keep it narrow".into(),
                    detail: None,
                },
                QuestionOption {
                    label: "Broad".into(),
                    description: "Include adjacent work".into(),
                    detail: None,
                },
            ],
            required: true,
            default: None,
        }],
    };
    event(
        &controller,
        E::Question(QuestionEvent::Asked {
            turn_id,
            request: request.clone(),
        }),
        cx,
    );
    let note = ("question-note", 0usize);
    let focused = ("question-option", 0usize);
    let broad = ("question-option", 1usize);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(note).is_none());
        window.click(focused, cx);
        window.render_frame(cx);
        assert!(view.read(cx).question_note_editor.is_none());
        window.click(note, cx);
        window.render_frame(cx);
        let editor = view.read(cx).question_note_editor.as_ref().unwrap();
        assert!(editor.input.focus_handle(cx).is_focused(window));
        window.input("Only for the first release", cx);
        window.press("enter", cx);
        window.input("Revisit later", cx);
    })
    .unwrap();
    let original = view.read_with(cx, |view, _| {
        view.question_note_editor.as_ref().unwrap().input.clone()
    });
    assert!(
        receiver.try_recv().is_err(),
        "Enter in the note does not submit the form"
    );

    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(broad, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.question_note_editor.is_none()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(note, cx);
        window.render_frame(cx);
        window.input("Do not send this draft", cx);
    })
    .unwrap();
    assert_eq!(
        controller
            .read_with(cx, |controller, _| {
                controller
                    .model
                    .controls
                    .question
                    .as_ref()
                    .unwrap()
                    .choice_note_draft(0, 1)
                    .map(str::to_owned)
            })
            .as_deref(),
        Some("Do not send this draft")
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(focused, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.question_note_editor.is_none()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(note, cx);
    })
    .unwrap();
    const NOTE: &str = "Only for the first release\nRevisit later";
    let reopened = view.read_with(cx, |view, cx| {
        let editor = view.question_note_editor.as_ref().unwrap();
        assert_eq!(editor.input.read(cx).value().as_ref(), NOTE);
        editor.input.clone()
    });
    assert_ne!(original.entity_id(), reopened.entity_id());
    // Even a late event from the first editor for the same option must not overwrite the draft.
    cx.update_window(window.into(), |_, window, cx| {
        original.update(cx, |input, cx| {
            input.set_value("stale edit", window, cx);
            cx.emit(InputEvent::Change);
        });
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("answer-question", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let SessionCommand::Control(ControlCommand::AnswerQuestion {
        request_id: actual_id,
        response,
    }) = receiver.try_recv().unwrap()
    else {
        panic!("expected AnswerQuestion");
    };
    assert_eq!(actual_id, request_id);
    assert_eq!(
        response,
        QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                id: "scope".into(),
                answer: Some("Focused (Recommended)".to_string().into()),
                note: Some(NOTE.into()),
            }]
        }
    );
    // Pending submission freezes the draft, even if an input change was already queued.
    cx.update_window(window.into(), |_, window, cx| {
        reopened.update(cx, |input, cx| {
            input.set_value("late change", window, cx);
            cx.emit(InputEvent::Change);
        });
        window.render_frame(cx);
    })
    .unwrap();
    controller.read_with(cx, |controller, _| {
        let form = controller.model.controls.question.as_ref().unwrap();
        assert!(form.pending);
        assert_eq!(form.choice_note_draft(0, 0), Some(NOTE));
    });
    event(
        &controller,
        E::Question(QuestionEvent::Closed {
            turn_id,
            request_id: QuestionRequestId::new("stale"),
        }),
        cx,
    );
    assert!(view.read_with(cx, |v, _| v.question_note_editor.is_some()));
    event(
        &controller,
        E::Question(QuestionEvent::Closed {
            turn_id,
            request_id,
        }),
        cx,
    );
    assert!(view.read_with(cx, |v, _| v.question_note_editor.is_none()));
    cx.update_window(window.into(), |_, window, cx| {
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
    })
    .unwrap();
    request.id = QuestionRequestId::new("new-question");
    request.questions[0].default = Some("Focused (Recommended)".to_string().into());
    event(
        &controller,
        E::Question(QuestionEvent::Asked { turn_id, request }),
        cx,
    );
    reopened.update(cx, |_, cx| cx.emit(InputEvent::Change));
    assert_eq!(
        controller.read_with(cx, |controller, _| {
            controller
                .model
                .controls
                .question
                .as_ref()
                .unwrap()
                .choice_note_draft(0, 0)
                .map(str::to_owned)
        }),
        Some(String::new())
    );
    assert!(receiver.try_recv().is_err());
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
        E::Turn(TurnEvent::Rejected {
            turn_id: TurnId::new(10),
            error: "not accepted".into(),
        }),
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
        E::Turn(TurnEvent::Started {
            turn_id: TurnId::new(11),
            mode: SessionMode::Build,
            message: Message::user("submitted"),
        }),
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
    let (window, view, controller, _) = setup(cx);
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
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("edit file"),
        }),
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
        E::Turn(TurnEvent::Intermediate {
            turn_id,
            message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            attempt_id: None,
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    event(
        &controller,
        E::Turn(TurnEvent::ToolResults {
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
        }),
        cx,
    );
    let key = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            // Completed edits do not prepare the diff until opened.
            let tool = controller
                .read(cx)
                .model
                .root
                .conversation
                .history()
                .iter()
                .find_map(|entry| {
                    let zevria_presentation::conversation::HistoryEntry::Conversation(entry) =
                        entry
                    else {
                        return None;
                    };
                    entry
                        .blocks
                        .iter()
                        .find(|b| b.native_tool().is_some())
                        .map(|b| b.id)
                })
                .unwrap();
            let key = format!(
                "{:?}-{}",
                controller.read(cx).model.root.conversation.epoch(),
                tool.0
            );
            assert_eq!(
                window
                    .within(key.clone())
                    .find("block-disclosure")
                    .expanded(),
                Some(false)
            );
            assert!(!view.read(cx).transcript.read(cx).test_prepared(&key, cx));
            assert!(window.try_find("native-diff").is_none());
            window.within(key.clone()).click("block-disclosure", cx);
            key
        })
        .unwrap();
    assert!(view.read_with(cx, |v, cx| v.transcript.read(cx).test_prepared(&key, cx)));
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.within(key).try_find("native-diff").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn restored_image_is_decoded_only_on_disclosure_and_stays_inside_its_step(cx: &mut TestAppContext) {
    use rig_core::message::{
        AssistantContent, AssistantMessage, CallId, ToolCall, ToolFunction, ToolName,
        ToolResultContent, UserContent,
    };
    use zevria_content::{ImageReadSummary, PromptImage};
    use zevria_foundation::{ToolCallOutcome, ToolResultMetadata};
    use zevria_transcript::transcript::TranscriptItem as T;
    let (window, view, controller, _) = setup(cx);
    let image = PromptImage::from_rgba(420, 360, &vec![180; 420 * 360 * 4]).unwrap();
    let path = "docs/capture.png";
    let call = ToolCall::new(
        CallId::from_wire("capture"),
        ToolFunction::new(
            ToolName::new("read_image").unwrap(),
            serde_json::json!({"file_path":path}),
        ),
    );
    let summary = ImageReadSummary {
        path: path.into(),
        mime: image.mime_type().into(),
        dimensions: image.dimensions(),
        bytes: image.encoded_len(),
        original: Some(((840, 720), 123_456)),
    }
    .to_string();
    let result = call.result(vec![
        ToolResultContent::text(summary.clone()),
        image.to_tool_result_content(),
    ]);
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            T::Message(Message::user("Inspect the capture")),
            T::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::ToolCall(call),
            ]))),
            T::ToolResults {
                message: Message::User {
                    content: vec![UserContent::ToolResult(result)],
                },
                metadata: vec![ToolResultMetadata {
                    id: "capture".into(),
                    call_id: None,
                    tool_name: "read_image".into(),
                    outcome: ToolCallOutcome::Success,
                    diagnostic: None,
                    detail: None,
                }],
                skill_applications: vec![],
            },
            T::Message(Message::assistant("The capture is legible.")),
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    let key = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_data(cx)
            .into_iter()
            .find(|row| row.position.is_some())
            .unwrap()
            .data
            .key
            .clone()
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("image-read-preview").is_none());
        assert!(!view.read(cx).transcript.read(cx).test_prepared(&key, cx));
        window.within(key.clone()).click("block-disclosure", cx);
    })
    .unwrap();
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
    cx.update_window(window.into(), |_, window, cx| {
        let answer_y = window.find("answer").bounds().origin.y;
        let mut row = window.within(key);
        let preview = row.find("image-read-preview").bounds();
        assert!(preview.size.height <= px(240.) && preview.bottom() < answer_y);
        row.hover("block-disclosure", cx);
        row.click("copy-output", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(format!("{summary}\n[image]"))
        );
    })
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
        E::Manage(ManagementEvent::ModeResult {
            request_id,
            result: zevria_session_api::ModeSelectionResult::Accepted {
                mode,
                changed: false,
            },
        }),
        cx,
    );
    expect_skill_completion_query(&mut receiver);
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
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("original"),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| window.input("later", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.press(if cfg!(windows) { "ctrl-z" } else { "cmd-z" }, cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(controller.read_with(cx, |this, _| this.draft.is_empty()));
    event(
        &controller,
        E::Turn(TurnEvent::Failed {
            turn_id,
            error: "fixture failure".into(),
        }),
        cx,
    );
    assert!(controller.read_with(cx, |this, _| this.draft.is_empty()));
    assert_eq!(
        controller.read_with(cx, |this, _| this.draft_slot.recovery().to_vec()),
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
        E::Manage(ManagementEvent::ModeResult {
            request_id,
            result: zevria_session_api::ModeSelectionResult::Accepted {
                mode,
                changed: true,
            },
        }),
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.root.next_mode()),
        SessionMode::Plan
    );
    expect_skill_completion_query(&mut receiver);
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
fn transcript_and_composer_share_one_axis_at_every_width(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 1920., 1080.);
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
    let assert_column = |pane_left: f32, window: &mut gpui_kit::Window, cx: &mut gpui_kit::App| {
        window.render_frame(cx);
        let assert_close = |actual: gpui_kit::Pixels, expected: f32| {
            assert!(
                (f32::from(actual) - expected).abs() <= 1.,
                "expected {expected}px, got {actual:?}"
            );
        };
        let pane_right = f32::from(window.viewport_size().width);
        let expected_width =
            super::style::COLUMN.min(pane_right - pane_left - super::style::COLUMN_INSET * 2.);
        let row = window.find(key.clone()).bounds();
        let composer = window.find("composer-column").bounds();
        assert_close(row.size.width, expected_width);
        assert_close(
            row.origin.x - px(pane_left),
            pane_right - f32::from(row.right()),
        );
        assert_close(composer.origin.x, f32::from(row.origin.x));
        assert_close(composer.size.width, f32::from(row.size.width));
        assert_close(
            window.find("composer-panel").bounds().size.width,
            expected_width,
        );
        let panel = window.find("composer-panel").bounds();
        let mode = window.find("mode-build").bounds();
        assert!(mode.origin.x >= panel.origin.x && mode.bottom() <= panel.bottom());
    };
    cx.update_window(window.into(), |_, window, cx| {
        assert_column(256., window, cx)
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::TOGGLE_SIDEBAR.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| assert_column(0., window, cx))
        .unwrap();
    cx.simulate_window_resize(window.into(), size(px(1240.), px(840.)));
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| assert_column(0., window, cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::TOGGLE_SIDEBAR.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        assert_column(256., window, cx)
    })
    .unwrap();
    cx.simulate_window_resize(window.into(), size(px(720.), px(520.)));
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        assert!(view.read(cx).compact);
        assert_column(256., window, cx);
        window.press(Shortcut::TOGGLE_SIDEBAR.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| assert_column(0., window, cx))
        .unwrap();
}

#[gpui_kit::test]
fn non_live_sessions_keep_resting_window_focus_and_shortcuts(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("composer-panel").focused(), Some(true));
    })
    .unwrap();
    controller.update(cx, |controller, cx| {
        controller.phase = crate::controller::Phase::Starting;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).window_focus.is_focused(window));
        window.press(Shortcut::TOGGLE_SIDEBAR.0, cx);
    })
    .unwrap();
    assert_eq!(
        view.read_with(cx, |view, _| view.sidebar_override),
        Some(false)
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.blur(cx);
        assert!(window.focused(cx).is_none());
    })
    .unwrap();
    controller.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).window_focus.is_focused(window));
    })
    .unwrap();
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
        window.press(Shortcut::TOGGLE_SIDEBAR.0, cx)
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

fn ask_multi_select_with_previews(controller: &Entity<SessionController>, cx: &mut TestAppContext) {
    let turn_id = TurnId::new(21);
    event(
        controller,
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Plan,
            message: Message::user("Plan validation"),
        }),
        cx,
    );
    event(
        controller,
        E::Question(QuestionEvent::Asked {
            turn_id,
            request: QuestionRequest {
                id: QuestionRequestId::new("targets"),
                source_label: None,
                dismissible: true,
                allow_choice_notes: true,
                questions: vec![QuestionPrompt {
                    id: "targets".into(),
                    header: "Targets".into(),
                    question: "Which crates should we validate?".into(),
                    kind: QuestionPromptKind::MultiSelect {
                        min_selections: Some(1),
                        max_selections: None,
                        allow_other: true,
                    },
                    options: vec![
                        QuestionOption {
                            label: "Core".into(),
                            description: "Validate shared behavior.".into(),
                            detail: Some("```rust\nfn validate_core() {}\n```".into()),
                        },
                        QuestionOption {
                            label: "TUI".into(),
                            description: "Validate the terminal layout.".into(),
                            detail: Some("```rust\nfn validate_tui() {}\n```".into()),
                        },
                    ],
                    required: true,
                    default: None,
                }],
            },
        }),
        cx,
    );
}

#[gpui_kit::test]
fn preview_button_changes_markdown_without_toggling_multi_select_then_send_returns_array(
    cx: &mut TestAppContext,
) {
    let (window, _, controller, mut receiver) = setup(cx);
    ask_multi_select_with_previews(&controller, cx);
    let preview = ("question-preview-pane", 0usize);
    let core = ("question-option", 0usize);
    let tui = ("question-option", 1usize);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(preview).visible());
        assert_eq!(window.find(core).role(), Some(gpui_kit::Role::CheckBox));
        assert_eq!(window.find(tui).role(), Some(gpui_kit::Role::CheckBox));
        window.click(("question-preview-option", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        controller.read_with(cx, |c, _| {
            let form = c.model.controls.question.as_ref().unwrap();
            (form.preview(0), form.answers[0].selected.clone())
        }),
        (Some(1), vec![])
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("copy-code", cx);
        assert!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| text.contains("fn validate_tui()"))
        );
        window.click(core, cx);
        window.render_frame(cx);
        window.click(tui, cx);
        window.render_frame(cx);
        assert!(window.try_find(("question-note", 0usize)).is_none());
        window.click("answer-question", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let SessionCommand::Control(ControlCommand::AnswerQuestion {
        request_id,
        response,
        ..
    }) = receiver.try_recv().unwrap()
    else {
        panic!("expected the batch answer");
    };
    assert_eq!(request_id.as_str(), "targets");
    assert_eq!(
        serde_json::to_value(response).unwrap(),
        serde_json::json!({
            "status": "answered", "answers": [{"id": "targets", "answer": ["Core", "TUI"]}]
        })
    );
}

#[gpui_kit::test]
fn question_preview_is_beside_choices_at_1240_and_below_at_720(cx: &mut TestAppContext) {
    for (width, compact) in [(1240., false), (720., true)] {
        let (window, view, controller, _) = setup_at_size(cx, width, 840.);
        ask_multi_select_with_previews(&controller, cx);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(view.read(cx).compact, compact);
            let pane = window.find(("question-preview-pane", 0usize)).bounds();
            let core = window.find(("question-option", 0usize)).bounds();
            let other = window.find(("question-other", 0usize)).bounds();
            assert!(pane.size.width > px(0.));
            assert!(pane.size.height > px(0.));
            if compact {
                assert!(pane.top() >= other.bottom(), "{pane:?} vs {other:?}");
            } else {
                assert!(
                    pane.left() >= core.right() && pane.top() < other.bottom(),
                    "{pane:?} vs {core:?}"
                );
            }
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn radio_question_options_other_and_optional_skip_keep_the_authoritative_form(
    cx: &mut TestAppContext,
) {
    let (window, _, controller, _) = setup(cx);
    let turn_id = TurnId::new(20);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id,
            mode: SessionMode::Build,
            message: Message::user("choose"),
        }),
        cx,
    );
    event(
        &controller,
        E::Question(QuestionEvent::Asked {
            turn_id,
            request: QuestionRequest {
                allow_choice_notes: false,
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
                        detail: None,
                    }],
                    required: false,
                    default: None,
                }],
            },
        }),
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
        controller.read_with(cx, |c, _| c
            .model
            .controls
            .question
            .as_ref()
            .unwrap()
            .answers[0]
            .selected
            .clone()),
        vec![0]
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(("question-note", 0usize)).is_none(),
            "worker forms must not offer notes"
        );
        window.click(("question-other", 0usize), cx)
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| {
        c.model.controls.question.as_ref().unwrap().answers[0].other
    }));
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("question-skip", 0usize), cx)
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| {
        c.model.controls.question.as_ref().unwrap().answers[0].skip
    }));
}
