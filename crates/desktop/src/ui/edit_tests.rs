//! Native transcript edit interactions, including the durable acceptance boundary.
use super::*;
use gpui_kit::{App, Window as NativeWindow};
use zevria_content::{PromptBlock, PromptImage, UserPrompt};
use zevria_foundation::{RequestBehavior, SubtaskId};
use zevria_presentation::blocks::NativeToolCallStatus as ToolCallStatus;
use zevria_session_api::{
    JobEvent, SubtaskEvent, TranscriptEdit, TranscriptEditReplacement, TranscriptEditTarget,
};
use zevria_transcript::TranscriptItem;

const REPLACEMENT: TurnId = TurnId::new(42);
const SELECT_ALL: &str = if cfg!(windows) { "ctrl-a" } else { "cmd-a" };

fn seed(controller: &Entity<SessionController>, prompts: &[UserPrompt], cx: &mut TestAppContext) {
    controller.update(cx, |controller, cx| {
        let conversation = &mut controller.model.root.conversation;
        for (index, prompt) in prompts.iter().enumerate() {
            let turn = conversation.allocate_turn();
            conversation.push_user_turn(prompt.to_message(), turn);
            conversation.push_message(
                Message::assistant(format!("Reply {index}")),
                ToolCallStatus::Finished,
            );
        }
        cx.notify();
    });
    cx.run_until_parked();
}

// Flush controller observers between distinct native input events. Rendering a
// frame inside the app update does not run deferred entity notifications.
fn interact(window: Window, cx: &mut TestAppContext, f: impl FnOnce(&mut NativeWindow, &mut App)) {
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        f(window, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn click_first_edit(window: &mut NativeWindow, cx: &mut App) {
    let button = window
        .find_all("edit-prompt")
        .into_iter()
        .find(|button| button.visible())
        .expect("visible edit action");
    assert_eq!(button.label(), Some("Edit and resend"));
    let offset = button.bounds().center() - window.find("zevria-desktop").bounds().origin;
    window.click_at("zevria-desktop", offset, cx);
}

fn replace_text(window: &mut NativeWindow, text: &str, cx: &mut App) {
    window.press(SELECT_ALL, cx);
    if text.is_empty() {
        window.press("backspace", cx);
    } else {
        window.input(text, cx);
    }
}

fn assert_composer(view: &Entity<DesktopWindow>, text: &str, cx: &mut TestAppContext) {
    view.read_with(cx, |view, cx| {
        assert_eq!(view.composer.read(cx).value().as_ref(), text);
        assert_eq!(view.controller.read(cx).draft, text);
    });
}

fn take_edit(receiver: &mut Receiver) -> TranscriptEdit {
    let command = receiver.try_recv().expect("edit command");
    let SessionCommand::Turn(TurnCommand::EditTranscript(edit)) = command else {
        panic!("expected transcript edit, got {command:?}");
    };
    edit
}

fn start_replacement(
    controller: &Entity<SessionController>,
    prompt: &UserPrompt,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::Turn(TurnEvent::Started {
            turn_id: REPLACEMENT,
            mode: SessionMode::Build,
            message: prompt.to_message(),
        }),
        cx,
    );
}

fn image(red: u8) -> PromptImage {
    PromptImage::from_rgba(1, 1, &[red, 0, 0, 255]).unwrap()
}

#[gpui_kit::test]
fn recall_refocuses_identical_text_and_cancel_restores_the_composer(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, &["same prompt".into()], cx);
    let saved_image = image(1);
    controller.update(cx, |controller, cx| {
        controller.draft_images.push(saved_image.clone());
        controller.orchestrate = true;
        controller.edit_draft("same prompt".into(), cx);
    });
    interact(window, cx, |window, cx| {
        window.press("left", cx);
        assert!(view.read(cx).composer.read(cx).cursor() < "same prompt".len());
        assert_eq!(window.find("orchestrate").checked(), Some(true));
        click_first_edit(window, cx);
    });
    interact(window, cx, |window, cx| {
        assert_eq!(window.find("composer-panel").focused(), Some(true));
        assert_eq!(
            view.read(cx).composer.read(cx).cursor(),
            "same prompt".len()
        );
        assert!(window.find("composer-edit-banner").visible());
        assert!(window.find("prompt-editing-badge").visible());
        assert_eq!(window.find("send").label(), Some("Resend"));
        assert_eq!(window.find("mode-plan").disabled(), Some(true));
        assert_eq!(window.find("orchestrate").disabled(), Some(true));
        assert_eq!(window.find("status-model").disabled(), Some(true));
        assert!(window.try_find("composer-attachments").is_none());
        window.press(Shortcut::TOGGLE_MODE.0, cx);
        window.press(Shortcut::OPEN_MODELS.0, cx);
        window.press("escape", cx);
    });
    interact(window, cx, |window, _| {
        assert!(window.try_find("composer-edit-banner").is_none());
        assert!(window.try_find("prompt-editing-badge").is_none());
        assert_eq!(window.find("orchestrate").checked(), Some(true));
        assert!(window.find("composer-attachments").visible());
        assert_eq!(window.find("composer-panel").focused(), Some(true));
    });
    assert!(receiver.try_recv().is_err());
    assert_composer(&view, "same prompt", cx);
    controller.read_with(cx, |controller, _| {
        assert_eq!(controller.model.root.next_mode(), SessionMode::Build);
        assert!(!controller.model.controls.models.is_open());
        assert_eq!(controller.draft_images, [saved_image]);
        assert!(controller.edit.is_none());
    });
}

#[gpui_kit::test]
fn switching_edit_targets_keeps_the_pre_recall_draft(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    seed(
        &controller,
        &["first prompt".into(), "second prompt".into()],
        cx,
    );
    controller.update(cx, |controller, cx| {
        controller.orchestrate = true;
        controller.edit_draft(" unsent \n draft ".into(), cx);
    });
    interact(window, cx, click_first_edit);
    assert_composer(&view, "first prompt", cx);
    interact(window, cx, |window, cx| {
        window.input(" partial changes", cx)
    });
    interact(window, cx, |window, cx| window.click("edit-prompt", cx));
    interact(window, cx, |window, _| {
        assert!(window.find("prompt-editing-badge").visible());
        assert_eq!(window.find("composer-panel").focused(), Some(true));
    });
    assert_composer(&view, "second prompt", cx);
    controller.read_with(cx, |controller, _| {
        assert!(matches!(
            controller.edit.recalled_target(),
            Some((TranscriptEditTarget::PromptOrdinal(1), _))
        ));
    });
    interact(window, cx, |window, cx| window.click("cancel-edit", cx));
    interact(window, cx, |window, _| {
        assert!(window.try_find("composer-edit-banner").is_none());
        assert!(window.try_find("prompt-editing-badge").is_none());
        assert_eq!(window.find("orchestrate").checked(), Some(true));
        assert_eq!(window.find("composer-panel").focused(), Some(true));
    });
    assert_composer(&view, " unsent \n draft ", cx);
}

#[gpui_kit::test]
fn resend_locks_until_started_and_truncates_projected_rows_on_acceptance(cx: &mut TestAppContext) {
    use crate::timeline::BlockKind;
    let (window, view, controller, mut receiver) = setup(cx);
    seed(
        &controller,
        &["first prompt".into(), "discard this later turn".into()],
        cx,
    );
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| {
        replace_text(window, "/orchestrate revised work", cx)
    });
    interact(window, cx, |window, cx| window.press("enter", cx));
    interact(window, cx, |window, cx| {
        assert!(window.find("composer-edit-banner").visible());
        assert!(window.find("prompt-editing-badge").visible());
        assert!(window.try_find("cancel-edit").is_none());
        assert!(window.try_find("edit-prompt").is_none());
        assert!(window.find("stop").visible());
        window.input("not editable yet", cx);
    });
    assert_composer(&view, "/orchestrate revised work", cx);
    assert_eq!(
        take_edit(&mut receiver),
        TranscriptEdit {
            target: TranscriptEditTarget::PromptOrdinal(0),
            replacement: TranscriptEditReplacement::Message {
                text: "revised work".into(),
                mode: SessionMode::Build,
                behavior: RequestBehavior::Orchestrate,
            },
        }
    );
    controller.read_with(cx, |controller, _| {
        assert!(controller.edit.is_awaiting_acceptance());
        assert_eq!(
            controller.model.root.conversation.turn_starts(true).len(),
            2
        );
    });
    start_replacement(&controller, &"/orchestrate revised work".into(), cx);
    assert_composer(&view, "", cx);
    view.read_with(cx, |view, cx| {
        let rows = view.transcript.read(cx).test_data(cx);
        let prompts: Vec<_> = rows
            .iter()
            .filter(|row| matches!(row.data.kind, BlockKind::Prompt { .. }))
            .map(|row| row.data.body.as_ref())
            .collect();
        assert_eq!(prompts, ["/orchestrate revised work"]);
        assert!(!rows.iter().any(|row| row.data.body.contains("Reply 1")));
    });
    interact(window, cx, |window, cx| {
        assert!(window.try_find("composer-edit-banner").is_none());
        assert!(window.try_find("prompt-editing-badge").is_none());
        window.input("new draft while streaming", cx);
    });
    assert_composer(&view, "new draft while streaming", cx);
}

#[gpui_kit::test]
fn rejected_and_stopped_edits_keep_the_original_tail_and_edited_draft(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, &["first".into(), "later".into()], cx);
    controller.update(cx, |controller, cx| {
        controller.edit_draft("saved draft".into(), cx)
    });
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| {
        replace_text(window, "replacement", cx)
    });
    interact(window, cx, |window, cx| window.press("left", cx));
    let caret = "replacement".len() - 1;
    assert_eq!(
        view.read_with(cx, |view, cx| view.composer.read(cx).cursor()),
        caret
    );
    for (stop_with, terminal) in [
        (
            None,
            TurnEvent::Rejected {
                turn_id: REPLACEMENT,
                error: "edit rejected".into(),
            },
        ),
        (
            Some("stop"),
            TurnEvent::Cancelled {
                turn_id: REPLACEMENT,
            },
        ),
        (
            Some("escape"),
            TurnEvent::Cancelled {
                turn_id: REPLACEMENT,
            },
        ),
    ] {
        interact(window, cx, |window, cx| window.click("send", cx));
        take_edit(&mut receiver);
        if let Some(stop) = stop_with {
            interact(window, cx, |window, cx| {
                if stop == "stop" {
                    // Pointer clicks don't take focus in Kit. Tab to Stop
                    // first so its removal exercises the focus handoff.
                    for _ in 0..32 {
                        if window.find("stop").focused() == Some(true) {
                            break;
                        }
                        window.press("tab", cx);
                    }
                    assert_eq!(window.find("stop").focused(), Some(true));
                    window.click("stop", cx);
                } else {
                    window.press(stop, cx);
                }
            });
            assert!(matches!(
                receiver.try_recv().unwrap(),
                SessionCommand::Control(ControlCommand::CancelTurn { turn_id: None })
            ));
        }
        event(&controller, E::Turn(terminal), cx);
        if stop_with.is_none() {
            expect_skill_completion_query(&mut receiver);
        }
        assert_composer(&view, "replacement", cx);
        interact(window, cx, |window, cx| {
            assert_eq!(window.find("composer-panel").focused(), Some(true));
            assert_eq!(view.read(cx).composer.read(cx).cursor(), caret);
        });
        controller.read_with(cx, |controller, _| {
            assert!(controller.edit.is_recalling());
            assert_eq!(
                controller.model.root.conversation.turn_starts(true).len(),
                2
            );
            assert!(!controller.draft_slot.has_pending());
        });
    }
    interact(window, cx, |window, cx| {
        assert_eq!(window.find("send").label(), Some("Resend"));
        window.press("escape", cx);
    });
    assert_composer(&view, "saved draft", cx);
}

#[gpui_kit::test]
fn recalling_blocks_plan_review_and_decisions(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    seed(&controller, &["first prompt".into()], cx);
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: "Previous plan".into(),
        markdown: "The original plan.".into(),
        source_turn_id: REPLACEMENT,
    };
    event(
        &controller,
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact),
                draft: None,
            }),
        }),
        cx,
    );
    interact(window, cx, |window, _| {
        assert_ne!(window.find("review-plan").disabled(), Some(true));
    });
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| {
        assert_eq!(window.find("review-plan").disabled(), Some(true));
        window.click("review-plan", cx);
    });
    assert!(controller.read_with(cx, |controller, _| {
        controller.model.root.workflow.dialog().is_none()
    }));

    // Exercise the controller guard independently of the title-bar button.
    controller.update(cx, |controller, cx| {
        assert!(controller.model.root.workflow.open_review());
        cx.notify();
    });
    interact(window, cx, |window, cx| window.click("plan-implement", cx));
    interact(window, cx, |window, cx| window.click("plan-fresh", cx));
    interact(window, cx, |window, cx| {
        window.press(Shortcut::CONFIRM.0, cx)
    });
    assert!(receiver.try_recv().is_err());
    assert!(
        controller.read_with(cx, |controller, _| controller.edit.is_recalling()
            && !controller.model.root.is_busy())
    );

    interact(window, cx, |window, cx| window.press("escape", cx));
    interact(window, cx, |window, cx| window.click("cancel-edit", cx));
    interact(window, cx, |window, cx| {
        assert_ne!(window.find("review-plan").disabled(), Some(true));
        window.click("review-plan", cx);
    });
    assert!(controller.read_with(cx, |controller, _| {
        controller.model.root.workflow.dialog().is_some()
    }));
}

#[gpui_kit::test]
fn capturing_dialog_refreshes_retained_edit_actions(cx: &mut TestAppContext) {
    let (window, _, controller, _) = setup(cx);
    seed(&controller, &["first prompt".into()], cx);
    interact(window, cx, |window, _| {
        assert!(window.find("edit-prompt").visible())
    });
    controller.update(cx, |controller, cx| {
        controller.set_capturing_dialog(true, cx)
    });
    interact(window, cx, |window, _| {
        assert!(window.try_find("edit-prompt").is_none());
        assert_eq!(window.find("send").disabled(), Some(true));
    });
    controller.update(cx, |controller, cx| {
        controller.set_capturing_dialog(false, cx)
    });
    interact(window, cx, |window, _| {
        assert!(window.find("edit-prompt").visible());
        assert_ne!(window.find("send").disabled(), Some(true));
    });
}

#[gpui_kit::test]
fn builtin_completion_only_inserts_when_an_ordinary_draft_has_recovered_images(
    cx: &mut TestAppContext,
) {
    use crate::session::DraftContent;

    let (window, view, controller, mut receiver) = setup(cx);
    let attachment = image(1);
    let revision = controller.update(cx, |controller, cx| {
        controller.draft_slot.retain(DraftContent {
            text: String::new(),
            images: vec![attachment.clone()],
        });
        controller.recover_draft(cx);
        controller.model.root.conversation.revision()
    });
    interact(window, cx, |window, cx| window.input("/ne", cx));
    assert!(view.read_with(cx, |view, cx| {
        view.completions(cx).iter().any(|(name, _)| name == "/new")
    }));
    interact(window, cx, |window, cx| window.press("enter", cx));
    assert_composer(&view, "/new ", cx);
    assert!(receiver.try_recv().is_err());
    controller.read_with(cx, |controller, _| {
        assert!(controller.edit.is_none());
        assert_eq!(controller.draft_images, [attachment]);
        assert_eq!(
            controller.model.root.conversation.revision(),
            revision,
            "accepting a completion must not try to submit the command"
        );
    });
}

#[gpui_kit::test]
fn restored_image_blocks_resend_in_order_and_failed_edits_recover_attachments(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    let first = image(1);
    let second = image(2);
    let original = UserPrompt::new(vec![
        PromptBlock::Image(first),
        PromptBlock::Text("before ".into()),
        PromptBlock::Image(second.clone()),
        PromptBlock::Text("after".into()),
    ])
    .unwrap();
    controller.update(cx, |controller, cx| {
        controller.model.root.conversation.restore(vec![
            TranscriptItem::Message(original.to_message()),
            TranscriptItem::Message(Message::assistant("old reply")),
        ]);
        cx.notify();
    });
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| {
        assert!(window.find("composer-attachments").visible());
        assert_eq!(
            window.find(("remove-attachment", 1usize)).label(),
            Some("Remove image 2")
        );
        window.click("send", cx);
    });
    interact(window, cx, |window, _| {
        assert_eq!(
            window.find(("remove-attachment", 0usize)).disabled(),
            Some(true)
        );
    });
    assert!(
        matches!(take_edit(&mut receiver).replacement, TranscriptEditReplacement::Message { text, .. } if text == original)
    );
    event(
        &controller,
        E::Turn(TurnEvent::Rejected {
            turn_id: REPLACEMENT,
            error: "retry".into(),
        }),
        cx,
    );
    expect_skill_completion_query(&mut receiver);
    interact(window, cx, |window, cx| {
        window.click(("remove-attachment", 0usize), cx)
    });
    interact(window, cx, |window, cx| {
        assert!(window.try_find(("remove-attachment", 1usize)).is_none());
        window.click("send", cx);
    });
    let edited = UserPrompt::new(vec![
        PromptBlock::Text("before after".into()),
        PromptBlock::Image(second.clone()),
    ])
    .unwrap();
    assert!(
        matches!(take_edit(&mut receiver).replacement, TranscriptEditReplacement::Message { text, .. } if text == edited)
    );
    start_replacement(&controller, &edited, cx);
    interact(window, cx, |window, cx| {
        assert!(window.try_find("composer-attachments").is_none());
        window.input("newer draft", cx);
    });
    event(
        &controller,
        E::Turn(TurnEvent::Failed {
            turn_id: REPLACEMENT,
            error: "post-acceptance failure".into(),
        }),
        cx,
    );
    assert_composer(&view, "newer draft", cx);
    controller.read_with(cx, |controller, _| {
        assert!(controller.edit.is_none());
        assert_eq!(controller.draft_slot.recovery()[0].images, [second.clone()]);
        let conversation = &controller.model.root.conversation;
        let selection = conversation.message_recall_selection(0).unwrap();
        assert_eq!(
            conversation.recallable_selection(selection).unwrap().0,
            edited
        );
        assert!(
            !conversation
                .history()
                .iter()
                .any(|entry| format!("{entry:?}").contains("old reply"))
        );
    });
    interact(window, cx, |window, cx| replace_text(window, "", cx));
    interact(window, cx, |window, cx| window.click("recover-draft", cx));
    interact(window, cx, |window, cx| {
        assert!(window.find("composer-attachments").visible());
        window.click("send", cx);
    });
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Turn(TurnCommand::Submit { text, .. }) if text == edited)
    );
}

#[gpui_kit::test]
fn cancellation_after_acceptance_restores_the_draft_not_the_discarded_turns(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, &["old".into(), "old tail".into()], cx);
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| replace_text(window, "edited", cx));
    interact(window, cx, |window, cx| window.press("enter", cx));
    take_edit(&mut receiver);
    start_replacement(&controller, &"edited".into(), cx);
    event(
        &controller,
        E::Turn(TurnEvent::Cancelled {
            turn_id: REPLACEMENT,
        }),
        cx,
    );
    assert_composer(&view, "edited", cx);
    controller.read_with(cx, |controller, _| {
        assert!(controller.edit.is_none());
        assert_eq!(
            controller.model.root.conversation.turn_starts(true).len(),
            1
        );
    });
}

#[gpui_kit::test]
fn completions_and_job_continuations_do_not_dispatch_during_recall(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, &["$review".into()], cx);
    controller.update(cx, |controller, cx| {
        controller.draft_images.push(image(1));
        cx.notify();
        controller.model.controls.registry.set_skills(vec![
            zevria_instructions::skill::SkillMeta {
                name: "review".parse().unwrap(),
                description: "Review work".into(),
            },
        ]);
    });
    let available = || {
        E::Job(JobEvent::ContinuationChanged {
            mode: Some(SessionMode::Build),
        })
    };
    event(&controller, available(), cx);
    assert!(
        receiver.try_recv().is_err(),
        "an image-only draft blocks continuation"
    );
    interact(window, cx, click_first_edit);
    assert_composer(&view, "$review", cx);
    assert!(
        view.read_with(cx, |view, cx| view.completions(cx).is_empty()),
        "bare recalled skill should not open completion"
    );
    interact(window, cx, |window, cx| replace_text(window, "/pl", cx));
    assert!(!view.read_with(cx, |view, cx| view.completions(cx).is_empty()));
    interact(window, cx, |window, cx| window.press("enter", cx));
    assert_composer(&view, "/plan ", cx);
    interact(window, cx, |window, cx| window.press("enter", cx));
    interact(window, cx, |window, cx| {
        assert!(window.find("composer-edit-banner").visible());
        replace_text(window, "", cx);
    });
    assert_composer(&view, "", cx);
    event(&controller, available(), cx);
    assert!(receiver.try_recv().is_err());
    controller.read_with(cx, |controller, _| {
        assert!(controller.edit.is_recalling());
        assert_eq!(controller.model.root.next_mode(), SessionMode::Build);
        assert!(!controller.model.root.is_busy());
    });
    interact(window, cx, |window, cx| window.press("escape", cx));
    assert!(
        receiver.try_recv().is_err(),
        "cancel restored the attachment"
    );
    interact(window, cx, |window, cx| {
        window.click(("remove-attachment", 0usize), cx)
    });
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Turn(TurnCommand::ContinueAfterJobs {
            mode: SessionMode::Build
        })
    ));
}

#[gpui_kit::test]
fn read_only_child_rows_never_offer_edit(cx: &mut TestAppContext) {
    let (window, _, controller, _) = setup(cx);
    seed(&controller, &["root prompt".into()], cx);
    let id = SubtaskId::new("child");
    event(
        &controller,
        E::Subtask(SubtaskEvent::Session {
            id: id.clone(),
            event: Box::new(E::Turn(TurnEvent::Started {
                turn_id: REPLACEMENT,
                mode: SessionMode::Build,
                message: Message::user("child prompt"),
            })),
        }),
        cx,
    );
    controller.update(cx, |controller, cx| controller.select_pane(Some(id), cx));
    interact(window, cx, |window, _| {
        assert!(window.find("prompt-card").visible());
        assert!(window.try_find("edit-prompt").is_none());
        assert!(window.try_find("composer-panel").is_none());
    });
    controller.update(cx, |controller, cx| controller.select_pane(None, cx));
    interact(window, cx, |window, _| {
        assert!(window.find("edit-prompt").visible())
    });
}

#[gpui_kit::test]
fn compact_edit_banner_and_resend_stay_in_the_viewport(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    let mut blocks = vec![PromptBlock::Text("Short recalled prompt".into())];
    blocks.extend((0..8).map(|i| PromptBlock::Image(image(i))));
    seed(&controller, &[UserPrompt::new(blocks).unwrap()], cx);
    view.read_with(cx, |view, _| view.transcript.clone())
        .update(cx, |view, cx| view.jump_to_beginning(cx));
    interact(window, cx, click_first_edit);
    interact(window, cx, |window, cx| {
        replace_text(window, &"A line of editable text\n".repeat(10), cx)
    });
    interact(window, cx, |window, _| {
        for control in ["composer-edit-banner", "send"] {
            let bounds = window.find(control).bounds();
            assert!(bounds.origin.x >= px(0.) && bounds.right() <= px(720.));
            assert!(
                bounds.origin.y >= px(40.) && bounds.bottom() <= px(492.),
                "{control}: {bounds:?}"
            );
        }
    });
}
