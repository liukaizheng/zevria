//! Headless regressions for native Side Chat composition and references.
use super::*;
use zevria_content::PromptImage;
use zevria_foundation::{
    ModelProfileRef, ReasoningLevel, SideChatDescriptor, SideChatId, SideChatStatus,
};
use zevria_session_api::{SideChatCommand, SideChatEvent};

fn created(id: SideChatId, request_id: String, awaiting_initial_turn: bool) -> E {
    E::SideChat(SideChatEvent::Created {
        request_id,
        descriptor: Box::new(SideChatDescriptor {
            id,
            short_id: id.short(),
            title: "Parser investigation".into(),
            forked_mode: SessionMode::Build,
            forked_profile: ModelProfileRef::new("fixture", "model"),
            forked_reasoning: ReasoningLevel::Medium,
            inherited_turn_count: 0,
            turn_count: 0,
            status: SideChatStatus::Idle,
        }),
        quoted_excerpt: None,
        awaiting_initial_turn,
    })
}

fn side_turn(id: SideChatId, turn: TurnEvent) -> E {
    E::SideChat(SideChatEvent::Session {
        id,
        event: Box::new(E::Turn(turn)),
    })
}

fn pixel() -> PromptImage {
    PromptImage::from_rgba(1, 1, &[40, 70, 90, 255]).unwrap()
}

fn request(receiver: &mut Receiver) -> (String, zevria_content::UserPrompt) {
    let SessionCommand::Control(ControlCommand::SideChat(SideChatCommand::Create {
        request_id,
        initial_prompt: Some(prompt),
        ..
    })) = receiver.try_recv().unwrap()
    else {
        panic!("expected a Side Chat creation");
    };
    (request_id, prompt)
}

#[gpui_kit::test]
fn image_chooser_captures_input_and_failed_side_creation_retains_the_attached_draft(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel.png");
    std::fs::write(&path, pixel().encoded_bytes()).unwrap();
    let draft = "/side What does this image show?";
    controller.update(cx, |c, cx| c.edit_draft(draft.into(), cx));
    cx.run_until_parked();
    interact(window, cx, |window, cx| window.click("attach-image", cx));
    assert!(cx.did_prompt_for_paths());
    controller.update(cx, |c, cx| {
        assert!(c.capturing_dialog());
        c.submit(cx);
    });
    assert!(receiver.try_recv().is_err());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.multiple && !options.directories);
        Some(vec![path])
    });
    cx.run_until_parked();
    controller.update(cx, |c, cx| {
        assert!(!c.capturing_dialog());
        assert_eq!(c.draft_images.len(), 1);
        c.submit(cx);
        c.submit(cx); // A duplicate must not launch a second fork.
    });
    let (request_id, prompt) = request(&mut receiver);
    assert_eq!(prompt.text_projection(), "What does this image show?");
    assert_eq!(prompt.images().count(), 1);
    assert!(receiver.try_recv().is_err());
    assert_composer(&view, draft, cx);

    event(
        &controller,
        E::SideChat(SideChatEvent::CreateFailed {
            request_id,
            error: "fixture failure".into(),
        }),
        cx,
    );
    assert_composer(&view, draft, cx);
    controller.update(cx, |c, cx| {
        assert_eq!(c.draft_images.len(), 1);
        c.submit(cx);
    });
    let (retry, retried) = request(&mut receiver);
    assert_eq!(retried, prompt);
    event(
        &controller,
        created(SideChatId::random(), "unrelated".into(), false),
        cx,
    );
    assert_composer(&view, draft, cx);
    event(&controller, created(SideChatId::random(), retry, true), cx);
    assert_composer(&view, "", cx);
    controller.read_with(cx, |c, _| assert!(c.draft_images.is_empty()));
}

#[gpui_kit::test]
fn side_answer_actions_and_reference_preview_are_available_only_in_the_correct_pane(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    let id = SideChatId::random();
    event(&controller, created(id, "new side".into(), true), cx);
    let turn_id = TurnId::new(1);
    event(
        &controller,
        side_turn(
            id,
            TurnEvent::Started {
                turn_id,
                message: Message::user("What does the parser do?"),
                mode: SessionMode::Build,
                references: Vec::new(),
            },
        ),
        cx,
    );
    event(
        &controller,
        side_turn(id, TurnEvent::ModelCallStarted { turn_id, call: 1 }),
        cx,
    );
    event(
        &controller,
        side_turn(
            id,
            TurnEvent::Intermediate {
                turn_id,
                message: Message::assistant("Inspecting the parser first."),
                attempt_id: None,
            },
        ),
        cx,
    );
    interact(window, cx, |window, _| {
        assert!(window.try_find("quote-side-answer").is_none())
    });
    event(
        &controller,
        side_turn(id, TurnEvent::ModelCallStarted { turn_id, call: 2 }),
        cx,
    );
    event(
        &controller,
        side_turn(
            id,
            TurnEvent::Completed {
                turn_id,
                message: Message::assistant("The parser skips blank lines."),
                attempt_id: None,
            },
        ),
        cx,
    );
    interact(window, cx, |window, cx| {
        assert_eq!(
            window.find("quote-side-answer").label(),
            Some("Quote in main")
        );
        window.click("reference-side-answer", cx);
    });
    let token = format!("@side:{}#1 ", id.short());
    assert_composer(&view, &token, cx);
    controller.update(cx, |c, cx| {
        assert!(c.model.root.conversation.history().is_empty());
        c.close_side_chat_panel(cx);
        c.submit(cx);
    });
    let SessionCommand::Turn(TurnCommand::Submit {
        text, references, ..
    }) = receiver.try_recv().unwrap()
    else {
        panic!("expected referenced root submission");
    };
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].excerpt, "The parser skips blank lines.");
    interact(window, cx, |window, _| {
        assert!(window.try_find(("side-reference-chip", 0usize)).is_some())
    });
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id,
            message: text.to_message(),
            mode: SessionMode::Build,
            references,
        }),
        cx,
    );
    interact(window, cx, |window, cx| {
        assert!(window.try_find(("side-reference-chip", 0usize)).is_some());
        window.click(("side-reference-chip", 0usize), cx);
        assert!(
            window
                .try_find(("side-reference-excerpt", 0usize))
                .is_some()
        );
        window.click(("copy-side-reference", 0usize), cx);
        assert_eq!(
            window.find(("copy-side-reference", 0usize)).label(),
            Some("Copied")
        );
    });
}

#[gpui_kit::test]
fn only_current_editable_composers_receive_image_results(cx: &mut TestAppContext) {
    let (_, _, controller, _) = setup(cx);
    let id = SideChatId::random();
    event(&controller, created(id, "new side".into(), false), cx);
    controller.update(cx, |c, cx| {
        let generation = c.begin_image_dialog(Some(id), cx).unwrap();
        let stale = generation.wrapping_sub(1);
        assert!(!c.finish_image_dialog(stale, cx));
        assert!(c.capturing_dialog());
        c.update_side_draft(id, "blocked behind dialog".into(), cx);
        assert!(c.model.side_chats[0].draft.is_empty());
        c.finish_image_attachment(Some(id), generation, Ok(vec![pixel()]), cx);
        assert!(c.model.side_chats[0].draft_images.is_empty());
        assert!(c.finish_image_dialog(generation, cx));
        c.finish_image_attachment(None, stale, Ok(vec![pixel()]), cx);
        c.finish_image_attachment(None, stale, Err("stale picker error".into()), cx);
        assert!(c.draft_images.is_empty());
        assert!(c.model.root.conversation.history().is_empty());

        c.model.side_chats[0].error = Some("previous attachment failed".into());
        c.finish_image_attachment(Some(id), generation, Ok(vec![pixel()]), cx);
        assert_eq!(c.model.side_chats[0].draft_images.len(), 1);
        assert!(c.model.side_chats[0].error.is_none());
        assert!(c.draft_images.is_empty());
        let second = c.begin_image_dialog(Some(id), cx).unwrap();
        c.remove_side_image(id, 0, cx);
        assert_eq!(c.model.side_chats[0].draft_images.len(), 1);
        assert!(c.finish_image_dialog(second, cx));
        c.remove_side_image(id, 0, cx);
        assert!(c.model.side_chats[0].draft_images.is_empty());
    });
}
