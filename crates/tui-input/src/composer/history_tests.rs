use super::*;

fn type_text(state: &mut ComposerState, text: &str) {
    for character in text.chars() {
        state.insert_character(character);
    }
}

fn image() -> PromptImage {
    PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap()
}

#[test]
fn typing_spaces_and_backspace_are_separate_action_based_runs() {
    let mut state = ComposerState::default();
    assert!(!state.undo());
    assert!(!state.redo());
    type_text(&mut state, "one two");
    state.backspace();
    state.backspace();
    assert_eq!(state.text(), "one t");
    assert_eq!(state.history.undo.len(), 2);
    assert!(state.undo());
    assert_eq!((state.text(), state.cursor()), ("one two", 7));
    assert!(state.undo());
    assert_eq!((state.text(), state.cursor()), ("", 0));
    assert!(!state.undo());
    assert!(state.redo());
    assert_eq!(state.text(), "one two");
    assert!(state.redo());
    assert_eq!((state.text(), state.cursor()), ("one t", 5));
    assert!(!state.redo());
}

#[test]
fn navigation_and_other_actions_close_groups_even_at_boundaries() {
    let boundaries: [fn(&mut ComposerState); 8] = [
        ComposerState::move_left,
        ComposerState::move_right,
        ComposerState::move_word_left,
        ComposerState::move_word_right,
        ComposerState::move_menu_up,
        ComposerState::move_menu_down,
        ComposerState::reset_navigation,
        |state| state.move_vertical(1, 80),
    ];
    for boundary in boundaries {
        let mut state = ComposerState::default();
        type_text(&mut state, "abc");
        boundary(&mut state);
        let before = state.content_snapshot();
        type_text(&mut state, "de");
        assert!(state.undo());
        assert_eq!(state.content_snapshot(), before);
        assert!(state.undo());
        assert!(state.is_empty());
    }
    let mut state = ComposerState::default();
    type_text(&mut state, "first");
    state.insert_character('\n');
    type_text(&mut state, "second");
    state.insert_text("\nthird\nfourth");
    for expected in ["first\nsecond", "first\n", "first", ""] {
        assert!(state.undo());
        assert_eq!(state.text(), expected);
    }
}

#[test]
fn new_edits_discard_redo_but_navigation_and_noop_edits_do_not() {
    let mut state = ComposerState::default();
    type_text(&mut state, "draft");
    assert!(state.undo());
    let generation = state.generation();
    state.move_left();
    state.backspace();
    state.insert_text("");
    state.delete_current_line();
    state.delete_to_line_end();
    assert!(state.accept_highlighted_completion().is_none());
    assert!(!state.cancel_completion());
    assert!(!state.clear_if_nonempty());
    assert!(!state.undo());
    assert_eq!(state.generation(), generation);
    assert!(state.redo());
    assert_eq!(state.text(), "draft");
    assert!(state.undo());
    state.insert_character('x');
    assert!(!state.redo());
    assert_eq!(state.text(), "x");
}

#[test]
fn undo_redo_close_runs_and_generations_never_go_backwards() {
    let mut state = ComposerState::default();
    type_text(&mut state, "one");
    let draft = state.snapshot();
    state.close_edit_group();
    assert!(
        state.matches_draft(&draft),
        "group state is not ack identity"
    );
    assert!(state.undo());
    assert!(state.redo());
    assert!(!state.matches_draft(&draft));
    assert_eq!(state.generation(), draft.generation + 2);
    state.insert_character('!');
    assert!(state.undo());
    assert_eq!(state.text(), "one");
    assert!(state.undo());
    assert!(state.is_empty());
}

#[test]
fn retention_cap_is_shared_by_both_stacks() {
    let mut state = ComposerState::default();
    for _ in 0..HISTORY_LIMIT + 20 {
        state.insert_text("x");
    }
    assert_eq!(state.history.undo.len(), HISTORY_LIMIT);
    for _ in 0..HISTORY_LIMIT {
        assert!(state.undo());
        assert_eq!(
            state.history.undo.len() + state.history.redo.len(),
            HISTORY_LIMIT
        );
    }
    assert_eq!(state.text(), "x".repeat(20));
    assert!(!state.undo());
    for _ in 0..HISTORY_LIMIT {
        assert!(state.redo());
    }
    assert!(!state.redo());
    assert_eq!(state.text().len(), HISTORY_LIMIT + 20);
}

#[test]
fn line_deletions_and_completion_are_atomic_with_caret_restoration() {
    for (text, cursor) in [("one", 1), ("one\ntwo\nthree", 5), ("one\ntwo", 5)] {
        for edit in [
            ComposerState::delete_current_line,
            ComposerState::delete_to_line_end,
        ] {
            let mut state = ComposerState::default();
            state.replace(text.into(), cursor);
            let before = state.content_snapshot();
            edit(&mut state);
            let after = state.content_snapshot();
            assert_ne!(before, after);
            assert!(state.undo());
            assert_eq!(state.content_snapshot(), before);
            assert!(state.redo());
            assert_eq!(state.content_snapshot(), after);
        }
    }
    for edit in [
        (|state: &mut ComposerState| state.accept_highlighted_completion().is_some())
            as fn(&mut ComposerState) -> bool,
        ComposerState::cancel_completion,
    ] {
        for text in ["/com suffix", "$com suffix"] {
            let mut state = ComposerState::default();
            state.with_skills(vec![SkillMeta {
                name: "commit".parse().unwrap(),
                description: "Commit changes".into(),
            }]);
            state.replace(text.into(), 4);
            let before = state.content_snapshot();
            assert!(edit(&mut state));
            let after = state.content_snapshot();
            assert!(state.undo());
            assert_eq!(state.content_snapshot(), before);
            assert!(state.completion_filter_active());
            assert_eq!(state.menu().selected(), 0);
            assert_eq!(state.preferred_column(), None);
            assert!(state.redo());
            assert_eq!(state.content_snapshot(), after);
        }
    }
}

#[test]
fn unicode_and_grapheme_joining_edits_restore_safe_carets() {
    let mut state = ComposerState::default();
    type_text(&mut state, "e\u{301}界👩🏽‍💻");
    let original = state.content_snapshot();
    for _ in 0..3 {
        state.backspace();
    }
    assert!(state.is_empty());
    assert!(state.undo());
    assert_eq!(state.content_snapshot(), original);
    assert!(state.undo());
    assert!(state.is_empty());
    assert!(state.redo());
    assert_eq!(state.content_snapshot(), original);

    state.replace("👩💻".into(), "👩".len());
    state.insert_character('\u{200d}');
    assert_eq!(state.text(), "👩‍💻");
    assert_eq!(state.cursor(), state.text().len());
    assert!(state.undo());
    assert_eq!((state.text(), state.cursor()), ("👩💻", "👩".len()));
    assert!(state.redo());
    assert_eq!(state.cursor(), state.text().len());
    state.backspace();
    assert!(state.is_empty());
    assert!(state.undo());
    assert_eq!(state.text(), "👩‍💻");
}

#[test]
fn image_transactions_restore_exact_blocks_ranges_cursor_and_ordinals() {
    let mut state = ComposerState::default();
    state.insert_text("é [image 1] / ");
    let plain = state.content_snapshot();
    let attachment = image();
    state.attach_image(attachment.clone()).unwrap();
    let attached = state.content_snapshot();
    let prompt = state.prompt();
    assert_eq!(
        prompt.blocks(),
        &[
            PromptBlock::Text(plain.text.clone()),
            PromptBlock::Image(attachment.clone())
        ]
    );
    assert_eq!(state.next_ordinal, 2);
    assert!(state.undo());
    assert_eq!(state.content_snapshot(), plain);
    assert!(state.redo());
    assert_eq!(state.content_snapshot(), attached);
    assert_eq!(state.prompt(), prompt);

    state.move_left();
    let interior = state.content_snapshot();
    state.insert_character('x');
    assert_eq!(state.prompt().images().count(), 0);
    assert!(state.undo());
    assert_eq!(state.content_snapshot(), interior);
    assert_eq!(state.prompt(), prompt);
    state.backspace();
    assert_eq!(state.text(), plain.text);
    assert!(state.undo());
    assert_eq!(state.content_snapshot(), interior);
    assert_eq!(state.prompt(), prompt);
    assert!(state.clear_if_nonempty());
    assert!(state.undo());
    assert_eq!(state.content_snapshot(), interior);
    assert_eq!(state.prompt(), prompt);

    state.move_right();
    state.attach_image(attachment.clone()).unwrap();
    assert_eq!(state.next_ordinal, 3);
    assert!(state.undo());
    state.attach_image(attachment).unwrap();
    assert_eq!(
        state.next_ordinal, 3,
        "undone ordinal allocation is reusable"
    );
    assert!(!state.redo());
    assert_eq!(state.prompt().images().count(), 2);
}

#[test]
fn image_deletion_and_grapheme_joining_replay_exact_occurrences() {
    for (delete, inside) in [
        (ComposerState::backspace as fn(&mut ComposerState), false),
        (ComposerState::delete_to_line_end, true),
        (ComposerState::delete_current_line, true),
    ] {
        let mut state = ComposerState::default();
        state.insert_text("🇦");
        state.attach_image(image()).unwrap();
        state.insert_text("🇧");
        state.move_left();
        if inside {
            state.move_left(); // Inside the marker for line-deletion invalidation.
        }
        let before = state.content_snapshot();
        let prompt = state.prompt();
        delete(&mut state);
        let after = state.content_snapshot();
        assert!(state.image_ranges().is_empty());
        assert_eq!(state.cursor(), clamp_cursor(state.text(), state.cursor()));
        assert!(state.undo());
        assert_eq!(state.content_snapshot(), before);
        assert_eq!(state.prompt(), prompt);
        assert!(state.redo());
        assert_eq!(state.content_snapshot(), after);
    }
}

#[test]
fn failed_image_validation_preserves_redo_and_ack_identity() {
    let mut state = ComposerState::default();
    for _ in 0..zevria_content::prompt::MAX_PROMPT_IMAGES {
        state.attach_image(image()).unwrap();
    }
    state.insert_text("suffix");
    assert!(state.undo());
    let draft = state.snapshot();
    assert!(state.attach_image(image()).is_err());
    assert!(state.matches_draft(&draft));
    assert!(state.redo());
    assert!(state.text().ends_with("suffix"));
}

#[test]
fn clearing_cancels_pending_work_without_recording_or_replaying_it() {
    let mut state = ComposerState::default();
    state.insert_text("draft");
    state.begin_paste();
    let generation = state.generation();
    assert!(state.clear_if_nonempty());
    assert!(state.undo());
    assert_eq!(state.text(), "draft");
    assert!(!state.is_paste_pending());
    assert!(!state.finish_paste(generation));
    assert!(state.undo());
    assert!(state.is_empty());
    state.begin_paste();
    assert!(state.clear_if_nonempty());
    assert!(!state.undo(), "cancel-only clear has no blank entry");
    assert!(state.redo());
    assert_eq!(state.text(), "draft");
}

#[test]
fn restored_drafts_keep_both_stacks_but_baselines_and_resets_do_not() {
    let mut state = ComposerState::default();
    type_text(&mut state, "saved");
    state.insert_text(" paste");
    assert!(state.undo());
    let saved = state.snapshot();
    let recalled = UserPrompt::new(vec![
        PromptBlock::Text("recall".into()),
        PromptBlock::Image(image()),
    ])
    .unwrap();
    state.replace_prompt(&recalled);
    assert_eq!(state.prompt(), recalled);
    assert!(!state.undo());
    state.restore(saved.clone());
    assert!(state.redo());
    assert_eq!(state.text(), "saved paste");
    state.restore(saved);
    state.insert_character('!');
    assert!(state.undo());
    assert_eq!(state.text(), "saved", "restoration closes the typing group");
    assert!(state.undo());
    assert!(state.is_empty());
    state.clear();
    assert!(!state.redo());
    assert!(!state.undo());
    assert_eq!(state.next_ordinal, 0);
}
