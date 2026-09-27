use super::*;
use crate::clipboard::ClipboardResult;
use ratatui::crossterm::event::KeyEvent;
use ratatui::{Terminal, backend::TestBackend};
use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;

fn image() -> PromptImage {
    PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap()
}
fn key(code: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL))
}
fn paste(app: &mut App) -> (u64, usize) {
    let Some(UiAction::ReadClipboard { generation, cursor }) = app.handle_event(key('v')) else {
        panic!("editable Insert composer must request clipboard read");
    };
    (generation, cursor)
}
#[test]
fn clipboard_press_is_insert_only_and_pending_input_is_locked_and_cancellable() {
    let mut app = App::new();
    assert!(app.handle_event(key('v')).is_none());
    app.interaction.enter_insert();
    app.composer.insert_text("draft");
    let (generation, cursor) = paste(&mut app);
    assert!(!app.composer.is_empty());
    assert!(
        app.handle_event(Event::Paste("not inserted".into()))
            .is_none()
    );
    assert!(app.submit().is_none());
    assert!(app.interaction.is_insert());
    assert_eq!(app.composer.text(), "draft");
    app.handle_event(key('c'));
    assert!(app.composer.is_empty());
    app.clipboard_completed(generation, cursor, ClipboardResult::Image(image()));
    assert!(app.composer.is_empty(), "cancelled result must be stale");
    app.interaction.enter_insert();
    let mut repeat = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    repeat.kind = KeyEventKind::Repeat;
    assert!(app.handle_event(Event::Key(repeat)).is_none());
}
#[test]
fn clear_and_undo_restore_completed_images_but_never_a_stale_clipboard_read() {
    let mut app = App::new();
    app.interaction.enter_insert();
    app.handle_event(Event::Paste("draft ".into()));
    let (generation, cursor) = paste(&mut app);
    app.clipboard_completed(generation, cursor, ClipboardResult::Image(image()));
    let prompt = app.composer.prompt();
    let ranges = app.composer.image_ranges();
    assert!(rendered_text(&mut app).contains("1 images"));
    let (stale_generation, stale_cursor) = paste(&mut app);
    app.handle_event(key('c'));
    assert!(app.composer.is_empty());
    assert!(!rendered_text(&mut app).contains("1 images"));
    app.handle_event(key('z'));
    assert_eq!(app.composer.prompt(), prompt);
    assert_eq!(app.composer.cursor(), stale_cursor);
    assert_eq!(app.composer.image_ranges(), ranges);
    assert!(rendered_text(&mut app).contains("1 images"));
    assert!(!app.composer.is_paste_pending());
    // Even a new read at the exact same visible caret cannot admit the old result.
    let (generation, cursor) = paste(&mut app);
    app.clipboard_completed(
        stale_generation,
        stale_cursor,
        ClipboardResult::Image(image()),
    );
    assert_eq!(app.composer.prompt(), prompt);
    assert!(app.composer.is_paste_pending());
    app.clipboard_completed(
        generation,
        cursor,
        ClipboardResult::Text("\nfirst\nsecond".into()),
    );
    app.handle_event(key('z'));
    assert_eq!(
        app.composer.prompt(),
        prompt,
        "multiline clipboard paste is one step"
    );
    app.handle_event(key('z'));
    assert_eq!(
        app.composer.text(),
        "draft ",
        "attachment is one complete step"
    );
    assert!(app.composer.image_ranges().is_empty());
    app.handle_event(key('y'));
    assert_eq!(app.composer.prompt(), prompt);
    assert_eq!(app.composer.image_ranges(), ranges);
}

#[test]
fn tokens_keep_occurrences_at_unicode_caret_and_lookalikes_are_plain_text() {
    let mut composer = ComposerState::default();
    composer.insert_text("é [image 1] end");
    for _ in 0..4 {
        composer.move_left();
    }
    composer.attach_image(image()).unwrap();
    assert_eq!(composer.text(), "é [image 1][image 2] end");
    composer.attach_image(image()).unwrap();
    let prompt = composer.prompt();
    assert_eq!(prompt.images().count(), 2);
    assert_eq!(prompt.text_projection(), "é [image 1] end");
    composer.move_left();
    composer.insert_text("x");
    assert_eq!(
        composer.prompt().images().count(),
        1,
        "editing inside a registered token prunes only that occurrence"
    );
    assert!(composer.prompt().text_projection().contains("[image 3x]"));
}
fn move_left_to(composer: &mut ComposerState, cursor: usize) {
    while composer.cursor() > cursor {
        composer.move_left();
    }
    assert_eq!(composer.cursor(), cursor);
}

fn rendered_text(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn backspace_removes_image_only_draft_in_one_edit() {
    let mut composer = ComposerState::default();
    composer.attach_image(image()).unwrap();
    composer.move_vertical(1, 80);
    composer.set_menu_selection_for_test(4);
    let generation = composer.generation();

    composer.backspace();

    assert_eq!(composer.text(), "");
    assert!(composer.image_ranges().is_empty());
    assert_eq!(composer.cursor(), 0);
    assert!(composer.is_empty());
    assert!(composer.prompt().is_blank());
    assert_eq!(composer.generation(), generation.wrapping_add(1));
    assert_eq!(composer.preferred_column(), None);
    assert_eq!(composer.menu().selected(), 0);
}

#[test]
fn backspace_removes_surrounded_image_at_every_interior_position_and_end() {
    let mut composer = ComposerState::default();
    composer.insert_text("before ");
    composer.attach_image(image()).unwrap();
    let range = composer.image_ranges()[0].clone();
    composer.insert_text(" after");
    let draft = composer.snapshot();

    for cursor in range.start + 1..=range.end {
        composer.restore(draft.clone());
        move_left_to(&mut composer, cursor);
        composer.backspace();

        assert_eq!(composer.text(), "before  after", "caret {cursor}");
        assert_eq!(composer.cursor(), range.start, "caret {cursor}");
        assert!(composer.image_ranges().is_empty(), "caret {cursor}");
        assert_eq!(composer.prompt(), UserPrompt::from_text("before  after"));
    }
}

#[test]
fn backspace_at_buffer_start_is_a_noop_even_with_an_image_ahead() {
    let mut composer = ComposerState::default();
    composer.attach_image(image()).unwrap();
    move_left_to(&mut composer, 0);
    let draft = composer.snapshot();

    composer.backspace();

    assert!(composer.matches_draft(&draft));
    assert_eq!(composer.text(), "[image 1]");
    assert_eq!(composer.image_ranges(), vec![0.."[image 1]".len()]);
    assert_eq!(composer.prompt().images().count(), 1);
    assert_eq!(composer.cursor(), 0);
}

#[test]
fn backspace_at_marker_start_deletes_only_the_preceding_grapheme() {
    for grapheme in ["x", "e\u{301}", "界", "👩🏽‍💻"] {
        let mut composer = ComposerState::default();
        composer.insert_text("before ");
        composer.insert_text(grapheme);
        composer.attach_image(image()).unwrap();
        let start = composer.image_ranges()[0].start;
        move_left_to(&mut composer, start);

        composer.backspace();

        assert_eq!(composer.text(), "before [image 1]", "{grapheme}");
        assert_eq!(composer.cursor(), "before ".len());
        assert_eq!(
            composer.image_ranges(),
            vec!["before ".len()..composer.text().len()]
        );
        assert_eq!(composer.prompt().images().count(), 1);
        assert_eq!(composer.prompt().text_projection(), "before ");
    }
}

#[test]
fn backspace_deletes_trailing_text_one_grapheme_at_a_time_without_detaching_image() {
    let mut composer = ComposerState::default();
    composer.attach_image(image()).unwrap();
    let ranges = composer.image_ranges();
    composer.insert_text("e\u{301}界👩🏽‍💻");

    for remaining in ["e\u{301}界", "e\u{301}", ""] {
        composer.backspace();

        assert_eq!(composer.text(), format!("[image 1]{remaining}"));
        assert_eq!(composer.cursor(), composer.text().len());
        assert_eq!(composer.image_ranges(), ranges);
        assert_eq!(composer.prompt().images().count(), 1);
        assert_eq!(composer.prompt().text_projection(), remaining);
    }
}

#[test]
fn backspace_preserves_other_occurrences_labels_ranges_and_prompt_order() {
    let duplicate = image();
    let other = PromptImage::from_rgba(1, 1, &[4, 5, 6, 255]).unwrap();
    let mut composer = ComposerState::default();
    composer.insert_text("é ");
    composer.attach_image(duplicate.clone()).unwrap();
    composer.insert_text(" / ");
    composer.attach_image(duplicate.clone()).unwrap();
    composer.insert_text(" / ");
    composer.attach_image(other.clone()).unwrap();
    composer.insert_text(" fin");
    let ranges = composer.image_ranges();
    let draft = composer.snapshot();

    let cases = [
        (
            0,
            "é  / [image 2] / [image 3] fin",
            vec![
                PromptBlock::Text("é  / ".into()),
                PromptBlock::Image(duplicate.clone()),
                PromptBlock::Text(" / ".into()),
                PromptBlock::Image(other.clone()),
                PromptBlock::Text(" fin".into()),
            ],
        ),
        (
            1,
            "é [image 1] /  / [image 3] fin",
            vec![
                PromptBlock::Text("é ".into()),
                PromptBlock::Image(duplicate.clone()),
                PromptBlock::Text(" /  / ".into()),
                PromptBlock::Image(other),
                PromptBlock::Text(" fin".into()),
            ],
        ),
        (
            2,
            "é [image 1] / [image 2] /  fin",
            vec![
                PromptBlock::Text("é ".into()),
                PromptBlock::Image(duplicate.clone()),
                PromptBlock::Text(" / ".into()),
                PromptBlock::Image(duplicate),
                PromptBlock::Text(" /  fin".into()),
            ],
        ),
    ];
    for (target, expected_text, blocks) in cases {
        let expected_prompt = UserPrompt::new(blocks).unwrap();
        let expected_ranges = (1..=3)
            .filter(|ordinal| *ordinal != target + 1)
            .map(|ordinal| {
                let token = format!("[image {ordinal}]");
                let start = expected_text.find(&token).unwrap();
                start..start + token.len()
            })
            .collect::<Vec<_>>();
        for cursor in ranges[target].start + 1..=ranges[target].end {
            composer.restore(draft.clone());
            move_left_to(&mut composer, cursor);
            composer.backspace();

            assert_eq!(
                composer.text(),
                expected_text,
                "image {target}, caret {cursor}"
            );
            assert_eq!(composer.cursor(), ranges[target].start);
            assert_eq!(composer.image_ranges(), expected_ranges);
            assert_eq!(composer.prompt(), expected_prompt);
        }
    }
}

#[test]
fn backspace_at_adjacent_marker_start_removes_only_the_previous_occurrence() {
    let mut composer = ComposerState::default();
    for _ in 0..3 {
        composer.attach_image(image()).unwrap();
    }
    let start = composer.image_ranges()[1].start;
    move_left_to(&mut composer, start);

    composer.backspace();

    assert_eq!(composer.text(), "[image 2][image 3]");
    assert_eq!(composer.cursor(), 0);
    assert_eq!(composer.image_ranges(), vec![0..9, 9..18]);
    assert_eq!(composer.prompt().images().count(), 2);
    assert_eq!(composer.prompt().text_projection(), "");
}

#[test]
fn backspace_clamps_caret_when_image_removal_joins_unicode_graphemes() {
    let mut composer = ComposerState::default();
    composer.insert_text("🇦");
    composer.attach_image(image()).unwrap();
    let range = composer.image_ranges()[0].clone();
    composer.insert_text("🇧");
    let draft = composer.snapshot();

    for cursor in [range.start + 1, range.end] {
        composer.restore(draft.clone());
        move_left_to(&mut composer, cursor);
        composer.backspace();

        assert_eq!(composer.text(), "🇦🇧");
        assert!(composer.image_ranges().is_empty());
        assert_eq!(composer.cursor(), 0, "the remaining flag is one grapheme");
        assert_eq!(composer.prompt(), UserPrompt::from_text("🇦🇧"));
        composer.move_right();
        assert_eq!(composer.cursor(), "🇦🇧".len());
        composer.backspace();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
    }
}

#[test]
fn backspace_keeps_typed_and_terminal_pasted_lookalikes_as_plain_text() {
    for terminal_paste in [false, true] {
        let mut app = App::new();
        app.interaction.enter_insert();
        let text = "e\u{301} [image 1]👩🏽‍💻";
        if terminal_paste {
            app.handle_event(Event::Paste(text.into()));
        } else {
            for character in text.chars() {
                app.handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )));
            }
        }
        for expected in ["e\u{301} [image 1]", "e\u{301} [image 1"] {
            assert!(
                app.handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Backspace,
                    KeyModifiers::NONE,
                )))
                .is_none()
            );
            assert_eq!(app.composer.text(), expected);
            assert_eq!(app.composer.cursor(), expected.len());
            assert!(app.composer.image_ranges().is_empty());
            assert_eq!(app.composer.prompt(), UserPrompt::from_text(expected));
        }
        move_left_to(&mut app.composer, "e\u{301} [im".len());
        app.composer.backspace();
        assert_eq!(app.composer.text(), "e\u{301} [iage 1");
        assert_eq!(app.composer.cursor(), "e\u{301} [i".len());
        assert!(app.composer.image_ranges().is_empty());
    }
}

#[test]
fn line_deletion_through_a_marker_keeps_existing_attachment_invalidation() {
    let mut composer = ComposerState::default();
    composer.insert_text("before ");
    composer.attach_image(image()).unwrap();
    composer.insert_text(" after\nnext");
    move_left_to(&mut composer, "before [im".len());

    composer.delete_to_line_end();

    assert_eq!(composer.text(), "before [im\nnext");
    assert_eq!(composer.cursor(), "before [im".len());
    assert!(composer.image_ranges().is_empty());
    assert_eq!(composer.prompt(), UserPrompt::from_text("before [im\nnext"));
}

#[test]
fn app_backspace_updates_image_count_and_submits_only_surviving_attachments() {
    for inside in [false, true] {
        let mut app = App::new();
        app.interaction.enter_insert();
        app.handle_event(Event::Paste("before ".into()));
        let (generation, cursor) = paste(&mut app);
        app.clipboard_completed(generation, cursor, ClipboardResult::Image(image()));
        let removed_range = app.composer.image_ranges()[0].clone();
        app.handle_event(Event::Paste(" between ".into()));
        let survivor = PromptImage::from_rgba(1, 1, &[4, 5, 6, 255]).unwrap();
        let (generation, cursor) = paste(&mut app);
        app.clipboard_completed(generation, cursor, ClipboardResult::Image(survivor.clone()));
        app.handle_event(Event::Paste(" after".into()));
        let cursor = if inside {
            removed_range.start + 4
        } else {
            removed_range.end
        };
        while app.composer.cursor() > cursor {
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        }
        assert_eq!(app.composer.cursor(), cursor);
        assert!(rendered_text(&mut app).contains("2 images"));

        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            )))
            .is_none()
        );

        assert_eq!(app.composer.text(), "before  between [image 2] after");
        assert_eq!(app.composer.cursor(), "before ".len());
        assert_eq!(app.composer.image_ranges().len(), 1);
        let rendered = rendered_text(&mut app);
        assert!(rendered.contains("1 images"));
        assert!(!rendered.contains("2 images"));
        let Some(UiAction::Submit { text, .. }) = app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        ))) else {
            panic!("remaining text and image must submit");
        };
        assert_eq!(
            text,
            UserPrompt::new(vec![
                PromptBlock::Text("before  between ".into()),
                PromptBlock::Image(survivor),
                PromptBlock::Text(" after".into()),
            ])
            .unwrap()
        );
    }
}

#[test]
fn app_backspace_empties_image_only_draft_and_prevents_submission() {
    for steps_left in [0, 4] {
        let mut app = App::new();
        app.interaction.enter_insert();
        let (generation, cursor) = paste(&mut app);
        app.clipboard_completed(generation, cursor, ClipboardResult::Image(image()));
        for _ in 0..steps_left {
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        }
        assert!(rendered_text(&mut app).contains("1 images"));

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));

        assert!(app.composer.is_empty());
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.composer.cursor(), 0);
        assert!(app.composer.image_ranges().is_empty());
        assert!(!rendered_text(&mut app).contains("1 images"));
        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::CONTROL,
            )))
            .is_none()
        );
        assert!(!app.session.is_busy());
        assert!(app.interaction.is_insert());
    }
}

#[test]
fn recalled_prompts_and_restored_drafts_keep_atomic_backspace() {
    for inside in [false, true] {
        let mut app = App::new();
        app.composer.insert_text("draft ");
        app.composer.attach_image(image()).unwrap();
        let range = app.composer.image_ranges()[0].clone();
        app.composer.insert_text(" tail");
        let saved_cursor = if inside { range.start + 4 } else { range.end };
        move_left_to(&mut app.composer, saved_cursor);
        let saved_prompt = app.composer.prompt();
        let saved_ranges = app.composer.image_ranges();
        let recalled = UserPrompt::new(vec![
            PromptBlock::Text("recalled ".into()),
            PromptBlock::Image(image()),
            PromptBlock::Text(" tail".into()),
        ])
        .unwrap();
        app.conversation
            .push_message(recalled.to_message(), ToolCallStatus::Finished);
        app.interaction.enter_selection(Selection {
            history_index: 0,
            content_index: 1,
        });
        assert!(app.handle_event(key('e')).is_none());
        assert!(app.edit.is_recalling());
        assert_eq!(app.composer.prompt(), recalled);
        let range = app.composer.image_ranges()[0].clone();
        let cursor = if inside { range.start + 4 } else { range.end };
        move_left_to(&mut app.composer, cursor);

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));

        assert_eq!(
            app.composer.prompt(),
            UserPrompt::from_text("recalled  tail")
        );
        assert_eq!(app.composer.text(), "recalled  tail");
        assert!(app.composer.image_ranges().is_empty());
        assert_eq!(app.composer.cursor(), "recalled ".len());

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!app.edit.is_recalling());
        assert_eq!(app.composer.prompt(), saved_prompt);
        assert_eq!(app.composer.image_ranges(), saved_ranges);
        assert_eq!(app.composer.cursor(), saved_cursor);
        app.interaction.enter_insert();

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));

        assert_eq!(app.composer.prompt(), UserPrompt::from_text("draft  tail"));
        assert_eq!(app.composer.text(), "draft  tail");
        assert!(app.composer.image_ranges().is_empty());
        assert_eq!(app.composer.cursor(), "draft ".len());
    }
}

#[test]
fn image_only_submit_rejection_restores_complete_attempt_and_acceptance_discards_it() {
    let mut app = App::new();
    app.interaction.enter_insert();
    app.composer.attach_image(image()).unwrap();
    let attempted = app.composer.prompt();
    let Some(UiAction::Submit { text, .. }) = app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    ))) else {
        panic!("image-only submission");
    };
    assert_eq!(text, attempted);
    assert!(app.composer.is_empty());
    assert!(app.interaction.is_normal());
    assert_eq!(app.session.next_mode(), SessionMode::Build);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    assert!(!terminal.backend().cursor_visible());
    app.reduce(SessionEvent::TurnRejected {
        turn_id: TurnId::new(1),
        error: "capacity".into(),
    });
    assert_eq!(app.composer.prompt(), attempted);
    assert!(app.interaction.is_normal());
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('i'),
        KeyModifiers::NONE,
    )));
    assert!(app.composer_editable());
    let Some(UiAction::Submit {
        behavior: _,
        text,
        mode,
    }) = app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )))
    else {
        panic!("retry restored prompt");
    };
    assert!(app.interaction.is_normal());
    assert_eq!(text, attempted);
    app.reduce(SessionEvent::TurnStarted {
        turn_id: TurnId::new(2),
        message: text.to_message(),
        mode,
    });
    app.reduce(SessionEvent::TurnFailed {
        turn_id: TurnId::new(2),
        error: "provider".into(),
    });
    assert!(
        app.composer.is_empty(),
        "accepted provider failure is not an unsubmitted draft"
    );
    assert!(app.interaction.is_normal());
    assert!(
        app.conversation
            .recallable_selection(Selection {
                history_index: 1,
                content_index: 0
            })
            .is_some()
    );
}
#[test]
fn worker_image_echoes_do_not_duplicate_accepted_occurrences_and_ordinals_are_correlated() {
    use crate::presentation::PresentationBlockKind;
    use zevria_workflow::AgentRunEvent;
    use zevria_workflow::WorkerControlId;
    use zevria_workflow::WorkerInput;
    use zevria_workflow::WorkerPromptKind;
    use zevria_workflow::WorkerReviewEvent;
    let image = image();
    let prompt = UserPrompt::new(vec![
        PromptBlock::Image(image.clone()),
        PromptBlock::Image(image.clone()),
    ])
    .unwrap();
    let ordinals = |app: &App| {
        app.conversation
            .history()
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Conversation(entry) => Some(&entry.blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::Image { ordinal, .. } => Some(*ordinal),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut app = App::new();
    let mut reducer = crate::agent_transcript::AgentTranscriptReducer::default();
    for event in [
        AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::Initial,
                    text: prompt.clone(),
                },
            }),
        },
        AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::Dispatched {
                generation: 1,
                attempt: 1,
            }),
        },
        AgentRunEvent::Prompt {
            text: prompt.display_projection(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: Some("echo".into()),
        },
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: Some("echo".into()),
        },
    ] {
        reducer.apply_event(&mut app.conversation, event);
    }
    assert_eq!(ordinals(&app), vec![1, 2]);
    let image_blocks = app
        .conversation
        .history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(&entry.blocks),
            _ => None,
        })
        .flatten()
        .filter(|block| matches!(block.kind, PresentationBlockKind::Image { .. }))
        .collect::<Vec<_>>();
    assert_eq!(image_blocks[0].prompt_group, Some(image_blocks[0].id));
    assert_eq!(image_blocks[1].prompt_group, image_blocks[0].prompt_group);
    assert!(image_blocks[0].prompt.is_some());
    assert!(image_blocks[1].prompt.is_none());
    assert_eq!(image_blocks[0].primary_copy(), image.label(1));
    assert_eq!(image_blocks[1].primary_copy(), image.label(2));
    let mut app = App::new();
    let mut reducer = crate::agent_transcript::AgentTranscriptReducer::default();
    for id in ["first", "first", "second"] {
        reducer.apply_event(
            &mut app.conversation,
            AgentRunEvent::UserImage {
                image: image.clone(),
                message_id: Some(id.into()),
            },
        );
    }
    assert_eq!(ordinals(&app), vec![1, 2, 1]);
}

#[test]
fn host_controls_reject_images_without_exposing_hidden_prefixes() {
    for command in ["/implement ", "/build ", "/plan "] {
        let mut app = App::new();
        app.interaction.enter_insert();
        app.composer.insert_text(command);
        app.composer.attach_image(image()).unwrap();
        let draft = app.composer.snapshot();
        assert!(app.submit().is_none());
        assert!(app.composer.matches_draft(&draft));
        assert!(app.interaction.is_insert());
        assert!(!app.mode_selection_pending());
    }
    let mut app = App::new();
    let prompt = UserPrompt::new(vec![
        PromptBlock::Image(image()),
        PromptBlock::Text("/implement".into()),
    ])
    .unwrap();
    app.composer.replace_prompt(&prompt);
    assert!(
        matches!(app.composer.classify(), Ok(ClassifiedInput::Message(value)) if value == prompt)
    );
    app.composer
        .replace_prompt(&prompt.with_prefix(" /implement "));
    assert!(matches!(
        app.composer.classify(),
        Ok(ClassifiedInput::Message(_))
    ));
}
#[test]
fn completion_enter_preserves_images_and_never_submits_an_attached_control_or_prompt() {
    for (prefix, completed) in [
        ("/bui", "/build "),
        ("/mod", "/model "),
        ("/imp", "/implement "),
        ("/ne", "/new "),
        ("/ensemble-p", "/ensemble-plan "),
        ("/ensemble-r", "/ensemble-review "),
        ("$comm", "$commit "),
    ] {
        let mut app = App::new().with_skills(vec![zevria_instructions::SkillMeta {
            name: "commit".parse().unwrap(),
            description: "Commit changes".into(),
        }]);
        app.interaction.enter_insert();
        let image = image();
        let prompt = UserPrompt::new(vec![
            PromptBlock::Text(prefix.into()),
            PromptBlock::Image(image.clone()),
            PromptBlock::Text("\n雪".into()),
        ])
        .unwrap();
        app.composer.replace_prompt(&prompt);
        move_left_to(&mut app.composer, prefix.len());
        let draft = app.composer.snapshot();
        assert!(app.command_menu_active());
        assert_eq!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            None
        );
        assert_eq!(
            app.composer.prompt(),
            UserPrompt::new(vec![
                PromptBlock::Text(completed.into()),
                PromptBlock::Image(image),
                PromptBlock::Text("\n雪".into()),
            ])
            .unwrap()
        );
        assert!(!app.session.is_busy());
        assert!(!app.mode_selection_pending());
        assert!(app.history().is_empty());
        assert_eq!(app.handle_event(key('z')), None);
        assert_eq!(app.composer.prompt(), prompt);
        assert!(
            !app.composer.matches_draft(&draft),
            "undo must not revive acknowledgement identity"
        );
        assert_eq!(app.composer.cursor(), prefix.len());
    }
}

#[test]
fn pending_clipboard_blocks_completion_enter_until_delivery() {
    let mut app = App::new();
    app.interaction.enter_insert();
    app.composer.insert_text("/bui");
    assert!(app.command_menu_active());
    let (generation, cursor) = paste(&mut app);
    assert!(!app.command_menu_active());
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.handle_event(enter.clone()), None);
    assert_eq!(app.composer.text(), "/bui");
    assert!(app.composer.is_paste_pending());
    assert!(!app.mode_selection_pending());
    app.clipboard_completed(generation, cursor, ClipboardResult::Text(String::new()));
    assert!(app.command_menu_active());
    assert!(matches!(
        app.handle_event(enter),
        Some(UiAction::SetMode {
            mode: SessionMode::Build,
            ..
        })
    ));
}

#[test]
fn mode_shortcut_preserves_exact_image_draft_and_blocks_during_clipboard_read() {
    let mut app = App::new();
    app.apply_selected_mode(SessionMode::Build);
    app.interaction.enter_insert();
    app.composer.insert_text("é before ");
    app.composer.attach_image(image()).unwrap();
    app.composer.insert_text(" after");
    app.composer.move_left();
    let draft = app.composer.snapshot();
    let shortcut = || Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    let (generation, cursor) = paste(&mut app);
    assert!(app.handle_event(shortcut()).is_none());
    app.clipboard_completed(generation, cursor, ClipboardResult::Empty);
    assert!(app.composer.matches_draft(&draft));
    let Some(UiAction::SetMode { request_id, mode }) = app.handle_event(shortcut()) else {
        panic!("mode shortcut should request selection");
    };
    assert_eq!(mode, SessionMode::Plan);
    assert!(app.composer.matches_draft(&draft));
    assert!(
        app.reduce(SessionEvent::ModeResult {
            request_id,
            result: zevria_session_api::ModeSelectionResult::Accepted {
                mode,
                changed: true
            },
        })
        .is_empty()
    );
    assert!(app.composer.matches_draft(&draft));
    assert!(app.composer_editable());
}

#[test]
fn recall_is_whole_ordered_prompt_and_completion_keeps_attachments() {
    let image = image();
    let prompt = UserPrompt::new(vec![
        PromptBlock::Text("before ".into()),
        PromptBlock::Image(image),
        PromptBlock::Text(" after".into()),
    ])
    .unwrap();
    let mut app = App::new();
    app.conversation
        .push_message(prompt.to_message(), ToolCallStatus::Finished);
    for content_index in 0..3 {
        let recalled = app
            .conversation
            .recallable_selection(Selection {
                history_index: 0,
                content_index,
            })
            .unwrap();
        assert_eq!(recalled.0, prompt);
    }
    app.composer.replace_prompt(&prompt.with_prefix("/res "));
    while app.composer.cursor() > 4 {
        app.composer.move_left();
    }
    assert!(app.composer.accept_highlighted_completion().is_some());
    assert_eq!(app.composer.prompt().images().count(), 1);
    let before = app.composer.prompt();
    app.composer.replace_prompt(&before);
    assert_eq!(app.composer.prompt(), before);
}
