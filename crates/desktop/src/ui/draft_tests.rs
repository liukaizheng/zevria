//! Native composer admission, acceptance, and draft recovery.
use super::*;
use crate::controller::Phase;
use zevria_content::PromptImage;
use zevria_model::{CompactionBackend, CompactionTrigger};
use zevria_presentation::conversation::HistoryEntry;
use zevria_session_api::{ManagementCommand, ModeSelectionResult, SubtaskEvent};
use zevria_workflow::PlanHandoff;

const TURN: TurnId = TurnId::new(13);
const FOLLOW_UP: &str = "newer draft";
const SELECT_ALL: &str = if cfg!(windows) { "ctrl-a" } else { "cmd-a" };

fn submit(window: Window, text: &str, cx: &mut TestAppContext) {
    interact(window, cx, |window, cx| window.input(text, cx));
    interact(window, cx, |window, cx| window.press("enter", cx));
}

#[track_caller]
fn assert_draft(view: &Entity<DesktopWindow>, text: &str, pending: bool, cx: &mut TestAppContext) {
    assert_composer(view, text, cx);
    view.read_with(cx, |view, cx| {
        let controller = view.controller.read(cx);
        assert_eq!(controller.draft_slot.has_pending(), pending);
        assert!(controller.draft_slot.recovery().is_empty());
        assert!(controller.draft_images.is_empty());
    });
}

fn start(controller: &Entity<SessionController>, message: Message, cx: &mut TestAppContext) {
    event(
        controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message,
        }),
        cx,
    );
}

fn expect_cancel(receiver: &mut Receiver, turn_id: Option<TurnId>) {
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Control(ControlCommand::CancelTurn { turn_id })
    );
}

fn hide_ready_plan(
    window: Window,
    controller: &Entity<SessionController>,
    cx: &mut TestAppContext,
) -> PlanArtifact {
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 2,
        },
        title: "Plan".into(),
        markdown: "# Implement the Plan".into(),
        source_turn_id: TurnId::new(1),
    };
    event(
        controller,
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            }),
        }),
        cx,
    );
    interact(window, cx, |window, cx| window.press("escape", cx));
    artifact
}

#[gpui_kit::test]
fn stopping_a_committed_prompt_keeps_the_composer_empty_or_preserves_new_text(
    cx: &mut TestAppContext,
) {
    for replacement in ["", FOLLOW_UP] {
        let (window, view, controller, mut receiver) = setup(cx);
        submit(window, "first prompt", cx);
        let SessionCommand::Turn(TurnCommand::Submit { text, .. }) = receiver.try_recv().unwrap()
        else {
            panic!("expected a prompt")
        };
        assert_draft(&view, "", true, cx);
        start(&controller, text.to_message(), cx);
        assert_draft(&view, "", false, cx);
        interact(window, cx, |window, cx| {
            if replacement.is_empty() {
                window.press("escape", cx);
            } else {
                window.input(replacement, cx);
                window.click("stop", cx);
            }
        });
        expect_cancel(&mut receiver, Some(TURN));
        event(
            &controller,
            E::Turn(TurnEvent::Cancelled { turn_id: TURN }),
            cx,
        );
        assert_draft(&view, replacement, false, cx);
        controller.read_with(cx, |c, _| {
            assert_eq!(c.model.root.conversation.turn_starts(true).len(), 1);
        });
        interact(window, cx, |window, _| {
            assert!(window.try_find("recover-draft").is_none())
        });
    }
}

#[gpui_kit::test]
fn authored_commands_clear_while_pending_and_are_never_recovered_on_cancel(
    cx: &mut TestAppContext,
) {
    for accepted in [false, true] {
        for text in [
            "/orchestrate independent work",
            "$review change",
            "/prompt docs:summary topic=release",
        ] {
            let (window, view, controller, mut receiver) = setup(cx);
            controller.update(cx, |c, _| {
                c.model
                    .controls
                    .registry
                    .set_skills(vec![zevria_instructions::skill::SkillMeta {
                        name: "review".parse().unwrap(),
                        description: "Review the change".into(),
                    }]);
            });
            submit(window, text, cx);
            let command = receiver.try_recv().unwrap();
            match text.split_whitespace().next().unwrap() {
                "/orchestrate" => assert!(matches!(
                    command,
                    SessionCommand::Turn(TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Orchestrate,
                        ..
                    })
                )),
                "$review" => assert!(matches!(
                    command,
                    SessionCommand::Turn(TurnCommand::InvokeSkill { .. })
                )),
                "/prompt" => assert!(matches!(
                    command,
                    SessionCommand::Turn(TurnCommand::InvokeMcpPrompt { .. })
                )),
                _ => unreachable!(),
            }
            assert_draft(&view, "", true, cx);
            if accepted {
                // A new draft typed while the engine is still preparing the
                // prompt must also survive Started itself.
                interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
                start(&controller, Message::user(text), cx);
                assert_draft(&view, FOLLOW_UP, false, cx);
            }
            interact(window, cx, |window, cx| window.press(Shortcut::STOP.0, cx));
            expect_cancel(&mut receiver, accepted.then_some(TURN));
            event(
                &controller,
                E::Turn(TurnEvent::Cancelled { turn_id: TURN }),
                cx,
            );
            assert_draft(&view, if accepted { FOLLOW_UP } else { "" }, false, cx);
        }
    }
}

#[gpui_kit::test]
fn stopping_an_unaccepted_message_discards_it_even_during_preturn_compaction(
    cx: &mut TestAppContext,
) {
    for compacting in [false, true] {
        let (window, view, controller, mut receiver) = setup(cx);
        submit(window, "pending prompt", cx);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionCommand::Turn(TurnCommand::Submit { .. })
        ));
        let replacement = if compacting {
            event(
                &controller,
                E::Turn(TurnEvent::CompactionStarted {
                    turn_id: TURN,
                    trigger: CompactionTrigger::AutomaticPreTurn,
                }),
                cx,
            );
            interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
            FOLLOW_UP
        } else {
            ""
        };
        assert_draft(&view, replacement, true, cx);
        interact(window, cx, |window, cx| window.press("escape", cx));
        expect_cancel(&mut receiver, compacting.then_some(TURN));
        event(
            &controller,
            E::Turn(TurnEvent::Cancelled { turn_id: TURN }),
            cx,
        );
        assert_draft(&view, replacement, false, cx);
        controller.read_with(cx, |c, _| {
            assert!(c.model.root.pending_prompt().is_none());
            assert!(c.model.root.conversation.turn_starts(true).is_empty());
        });
    }
}

#[gpui_kit::test]
fn failed_preturn_compaction_retains_an_unaccepted_prompt_and_images_for_recovery(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    let image = PromptImage::from_rgba(1, 1, &[10, 20, 30, 255]).unwrap();
    controller.update(cx, |c, cx| {
        c.draft_images.push(image.clone());
        cx.notify();
    });
    submit(window, "original prompt", cx);
    let SessionCommand::Turn(TurnCommand::Submit { text, .. }) = receiver.try_recv().unwrap()
    else {
        panic!("expected a prompt with an image")
    };
    assert_draft(&view, "", true, cx);
    event(
        &controller,
        E::Turn(TurnEvent::CompactionStarted {
            turn_id: TURN,
            trigger: CompactionTrigger::AutomaticPreTurn,
        }),
        cx,
    );
    interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
    event(
        &controller,
        E::Turn(TurnEvent::Failed {
            turn_id: TURN,
            error: "compaction unavailable".into(),
        }),
        cx,
    );
    assert_composer(&view, FOLLOW_UP, cx);
    controller.read_with(cx, |c, _| {
        assert!(!c.draft_slot.has_pending());
        let [draft] = c.draft_slot.recovery() else {
            panic!("one recoverable prompt")
        };
        assert_eq!(draft.to_user_prompt(None), text);
    });
    interact(window, cx, |window, cx| {
        window.press(SELECT_ALL, cx);
        window.press("backspace", cx);
    });
    interact(window, cx, |window, cx| window.click("recover-draft", cx));
    assert_composer(&view, "original prompt", cx);
    controller.read_with(cx, |c, _| {
        assert_eq!(c.draft_images, [image]);
        assert!(c.draft_slot.recovery().is_empty());
    });
}

#[gpui_kit::test]
fn failure_or_engine_disconnect_does_not_recover_a_committed_prompt_with_images(
    cx: &mut TestAppContext,
) {
    for disconnect in [false, true] {
        let (window, view, controller, mut receiver) = setup(cx);
        let image = PromptImage::from_rgba(1, 1, &[30, 20, 10, 255]).unwrap();
        controller.update(cx, |c, cx| {
            c.draft_images.push(image);
            cx.notify();
        });
        submit(window, "prompt with image", cx);
        let SessionCommand::Turn(TurnCommand::Submit { text, .. }) = receiver.try_recv().unwrap()
        else {
            panic!("expected a prompt")
        };
        assert_draft(&view, "", true, cx);
        start(&controller, text.to_message(), cx);
        assert_draft(&view, "", false, cx);
        let replacement = if disconnect {
            interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
            drop(receiver);
            // This next send observes the dead engine command channel.
            interact(window, cx, |window, cx| window.click("stop", cx));
            assert!(controller.read_with(cx, |c, _| matches!(c.phase, Phase::Failed(_))));
            FOLLOW_UP
        } else {
            event(
                &controller,
                E::Turn(TurnEvent::Failed {
                    turn_id: TURN,
                    error: "failure after acceptance".into(),
                }),
                cx,
            );
            ""
        };
        assert_draft(&view, replacement, false, cx);
        controller.read_with(cx, |c, _| {
            let conversation = &c.model.root.conversation;
            let selection = conversation.message_recall_selection(0).unwrap();
            assert_eq!(
                conversation.recallable_selection(selection).unwrap().0,
                text
            );
        });
    }
}

#[gpui_kit::test]
fn dead_command_channel_restores_an_unaccepted_prompt_but_not_a_control(cx: &mut TestAppContext) {
    for (input, expected) in [("unsent prompt", "unsent prompt"), ("/compact", "")] {
        let (window, view, controller, receiver) = setup(cx);
        drop(receiver);
        submit(window, input, cx);
        assert_draft(&view, expected, false, cx);
        assert!(controller.read_with(cx, |c, _| matches!(c.phase, Phase::Failed(_))));
    }
}

#[gpui_kit::test]
fn compact_clears_on_enter_from_exact_text_or_completion_and_stays_consumed(
    cx: &mut TestAppContext,
) {
    let trigger = CompactionTrigger::Manual;
    for (input, terminal) in [
        (
            "/compact",
            TurnEvent::CompactionCompleted {
                turn_id: TURN,
                trigger,
                backend: CompactionBackend::LocalSummary,
            },
        ),
        (
            "/co",
            TurnEvent::CompactionCompleted {
                turn_id: TURN,
                trigger,
                backend: CompactionBackend::LocalSummary,
            },
        ),
        ("/co", TurnEvent::Cancelled { turn_id: TURN }),
        (
            "/compact",
            TurnEvent::Failed {
                turn_id: TURN,
                error: "compaction failed".into(),
            },
        ),
    ] {
        let (window, view, controller, mut receiver) = setup(cx);
        interact(window, cx, |window, cx| window.input(input, cx));
        if input == "/compact" {
            // Dismiss the exact-match palette to exercise direct submission.
            interact(window, cx, |window, cx| window.press("escape", cx));
            assert!(view.read_with(cx, |view, _| view.completion_dismissed.is_some()));
        }
        interact(window, cx, |window, cx| window.press("enter", cx));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionCommand::Turn(TurnCommand::Compact { .. })
        ));
        assert_draft(&view, "", false, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.completion_selected, 0);
            assert_eq!(view.completion_dismissed, None);
        });
        event(
            &controller,
            E::Turn(TurnEvent::CompactionStarted {
                turn_id: TURN,
                trigger,
            }),
            cx,
        );
        assert!(controller.read_with(cx, |c, _| c.model.root.session.is_compacting()));
        assert_draft(&view, "", false, cx);
        let replacement = if matches!(terminal, TurnEvent::Failed { .. }) {
            interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
            FOLLOW_UP
        } else {
            ""
        };
        if matches!(terminal, TurnEvent::Cancelled { .. }) {
            interact(window, cx, |window, cx| window.click("stop", cx));
            expect_cancel(&mut receiver, Some(TURN));
        }
        event(&controller, E::Turn(terminal), cx);
        assert_draft(&view, replacement, false, cx);
        assert!(!controller.read_with(cx, |c, _| c.model.root.is_busy()));
    }
}

#[gpui_kit::test]
fn resume_clears_the_composer_and_reveals_the_compact_sidebar_on_each_invocation(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup_at_size(cx, 720., 840.);
    for _ in 0..2 {
        assert!(!view.read_with(cx, |view, _| view.sidebar_visible(720.)));
        submit(window, "/resume", cx);
        assert_draft(&view, "", false, cx);
        assert!(controller.read_with(cx, |c, _| c.show_sessions));
        assert_eq!(
            view.read_with(cx, |view, _| view.sidebar_override),
            Some(true)
        );
        interact(window, cx, |window, cx| {
            assert!(window.find("root-pane").visible());
            window.press(Shortcut::TOGGLE_SIDEBAR.0, cx);
        });
    }
    assert!(receiver.try_recv().is_err());
}

#[gpui_kit::test]
fn implementation_controls_clear_on_dispatch_even_when_cancelled_or_failed(
    cx: &mut TestAppContext,
) {
    for (slash, decision) in [
        ("/implement", PlanDecision::ImplementCurrent),
        ("/implement-fresh", PlanDecision::ImplementFresh),
    ] {
        for cancel in [true, false] {
            let (window, view, controller, mut receiver) = setup(cx);
            let artifact = hide_ready_plan(window, &controller, cx);
            submit(window, slash, cx);
            assert_eq!(
                receiver.try_recv().unwrap(),
                SessionCommand::Turn(TurnCommand::ResolvePlan {
                    expected: artifact.version,
                    decision,
                })
            );
            assert_draft(&view, "", false, cx);
            let current = decision == PlanDecision::ImplementCurrent;
            if current {
                event(
                    &controller,
                    E::Plan(PlanEvent::HandoffStarted {
                        turn_id: TURN,
                        handoff: Box::new(PlanHandoff::new(artifact, "source")),
                    }),
                    cx,
                );
                assert_draft(&view, "", false, cx);
            }
            let terminal = if cancel {
                interact(window, cx, |window, cx| window.press(Shortcut::STOP.0, cx));
                expect_cancel(&mut receiver, current.then_some(TURN));
                TurnEvent::Cancelled { turn_id: TURN }
            } else {
                TurnEvent::Failed {
                    turn_id: TURN,
                    error: "handoff failed".into(),
                }
            };
            event(&controller, E::Turn(terminal), cx);
            assert_draft(&view, "", false, cx);
        }
    }
}

#[gpui_kit::test]
fn rejected_mode_commands_cannot_concatenate_with_or_recover_over_new_input(
    cx: &mut TestAppContext,
) {
    for (slash, expected) in [("/plan", SessionMode::Plan), ("/build", SessionMode::Build)] {
        let (window, view, controller, mut receiver) = setup(cx);
        submit(window, slash, cx);
        let SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) =
            receiver.try_recv().unwrap()
        else {
            panic!("expected a mode command")
        };
        assert_eq!(mode, expected);
        assert_draft(&view, "", false, cx);
        interact(window, cx, |window, cx| window.input(FOLLOW_UP, cx));
        assert_composer(&view, FOLLOW_UP, cx);
        event(
            &controller,
            E::Manage(ManagementEvent::ModeResult {
                request_id,
                result: ModeSelectionResult::Rejected {
                    code: "busy".into(),
                    message: "Mode change refused".into(),
                },
            }),
            cx,
        );
        assert_draft(&view, FOLLOW_UP, false, cx);
    }
}

#[gpui_kit::test]
fn new_clears_before_replacement_shutdown_finishes(cx: &mut TestAppContext) {
    use crate::{
        controller::tests::{config, until},
        engine::EngineRuntime,
    };
    cx.executor().allow_parking(); // This test uses a real in-process session.
    let directory = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(directory.path()).unwrap();
    let config = config(&workspace, "127.0.0.1:9");
    let (window, view, controller, _) = setup(cx);
    cx.update(|cx| cx.set_global(EngineRuntime::new().unwrap()));
    controller.update(cx, |c, cx| {
        c.config = Some(config.into());
        c.workspace = Some(workspace);
        c.start_initial(false, cx);
    });
    until(cx, |cx| {
        controller.read_with(cx, |c, _| c.phase == Phase::Live)
    });
    let original = controller.read_with(cx, |c, _| c.model.transcript_path.clone());
    interact(window, cx, |window, cx| window.input("/new", cx));
    cx.update_window(window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    // Inspect before polling the async transition. The old session is still installed.
    controller.read_with(cx, |c, _| {
        assert_eq!(c.phase, Phase::Stopping);
        assert_eq!(c.model.transcript_path, original);
    });
    assert_draft(&view, "", false, cx);
    until(cx, |cx| {
        controller.read_with(cx, |c, _| {
            c.phase == Phase::Live && c.model.transcript_path != original
        })
    });
    let lock = controller.read_with(cx, |c, _| {
        c.model.transcript_path.with_extension("jsonl.lock")
    });
    controller.update(cx, |c, cx| c.quit(cx));
    until(cx, |_| !lock.exists());
}

#[gpui_kit::test]
fn new_without_config_leaves_the_command_in_the_composer(cx: &mut TestAppContext) {
    let (window, view, controller, _receiver) = setup(cx);
    assert!(controller.read_with(cx, |c, _| c.config.is_none()));
    submit(window, "/new ", cx);
    assert_draft(&view, "/new ", false, cx);
    assert!(controller.read_with(cx, |c, _| c.phase == Phase::Live));
}

#[gpui_kit::test]
fn blocked_model_slashes_keep_the_command_and_explain_why(cx: &mut TestAppContext) {
    for (slash, expected) in [
        ("/model", "Resolve the pending Plan before changing models."),
        (
            "/model-session",
            "Resolve the session saving error before changing models.",
        ),
    ] {
        let (window, view, controller, mut receiver) = setup(cx);
        if slash == "/model" {
            hide_ready_plan(window, &controller, cx);
        } else {
            event(
                &controller,
                E::PersistenceChanged {
                    path: "test.jsonl".into(),
                    error: Some("disk unavailable".into()),
                },
                cx,
            );
        }
        interact(window, cx, |window, cx| window.input(slash, cx));
        interact(window, cx, |window, cx| window.press("escape", cx));
        interact(window, cx, |window, cx| window.press("enter", cx));
        assert_draft(&view, slash, false, cx);
        assert!(receiver.try_recv().is_err());
        controller.read_with(cx, |c, _| {
            assert!(!c.model.controls.models.is_open());
            assert!(matches!(c.model.root.conversation.history().last(), Some(HistoryEntry::Error(error)) if error == expected));
        });
    }
}

#[gpui_kit::test]
fn empty_subtasks_command_reports_a_notice_without_arming_the_panel(cx: &mut TestAppContext) {
    use zevria_foundation::{SubtaskId, SubtaskStatus};
    let (window, view, controller, mut receiver) = setup(cx);
    controller.update(cx, |c, cx| c.toggle_subtasks(cx));
    assert!(!controller.read_with(cx, |c, _| c.model.controls.subtasks_open));
    submit(window, "/subtasks", cx);
    assert_draft(&view, "", false, cx);
    controller.read_with(cx, |c, _| {
        assert!(!c.model.controls.subtasks_open);
        assert!(matches!(c.model.root.conversation.history().last(), Some(HistoryEntry::Error(error)) if error == "No subtasks in this session."));
    });
    event(
        &controller,
        E::Subtask(SubtaskEvent::Status {
            id: SubtaskId::new("child"),
            status: SubtaskStatus::Starting,
            turn_id: TURN,
        }),
        cx,
    );
    controller.read_with(cx, |c, _| {
        assert_eq!(c.model.children.len(), 1);
        assert!(!c.model.subtasks_panel_visible());
    });
    assert!(receiver.try_recv().is_err());
}
