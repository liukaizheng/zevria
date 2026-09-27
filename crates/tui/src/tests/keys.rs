//! Keys behavior and presentation tests.
use super::*;

#[test]
fn typing_then_ctrl_enter_returns_to_normal_without_changing_the_workflow() {
    for mode in [SessionMode::Build, SessionMode::Plan, SessionMode::Build] {
        let mut app = App::new();
        app.set_mode_for_test(mode);
        enter_insert(&mut app);
        for ch in "hi there".chars() {
            assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
        }
        assert_eq!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "hi there".into(),
                mode,
            })
        );
        assert!(app.is_busy());
        assert_eq!(app.active_turn_id(), None);
        assert!(app.interaction().is_normal());
        assert_eq!(app.next_mode(), mode);
        assert_eq!(app.in_flight_mode(), Some(mode));
        assert!(!cursor_visible_after_render(&mut app, 80, 20));
        assert!(app.input().is_empty());
        assert!(
            app.history().is_empty(),
            "TurnStarted owns the user message"
        );

        apply_turn_event(
            &mut app,
            SessionEvent::TurnStarted {
                turn_id: TEST_TURN_ID,
                message: Message::user("hi there"),
                mode,
            },
        );
        assert!(app.interaction().is_normal());
        assert!(
            app.history()
                .last()
                .is_some_and(|entry| conversation_has_role(entry, PresentationRole::User))
        );
    }
}

#[test]
fn empty_and_invalid_submissions_preserve_insert_focus_and_the_complete_draft() {
    for draft in [
        "",
        " \n\t",
        "$missing arguments",
        "/ensemble-plan   ",
        "/unknown",
    ] {
        let mut app = App::new();
        enter_insert(&mut app);
        app.set_input_for_test(draft, draft.len());
        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert!(app.interaction().is_insert());
        assert_eq!(app.input(), draft);
        assert_eq!(app.input_cursor(), draft.len());
        assert!(!app.is_busy());
        assert!(cursor_visible_after_render(&mut app, 80, 20));
    }
}

#[test]
fn plain_enter_inserts_a_newline_and_ctrl_enter_submits_all_lines() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "first line".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "first line\n");
    assert_eq!(app.input_cursor(), app.input().len());
    assert!(app.interaction().is_insert());
    assert!(!app.is_busy());

    for ch in "second line".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "first line\nsecond line".into(),
            mode: SessionMode::Build,
        })
    );
}

#[test]
fn paste_is_one_cursor_aware_multiline_editor_event() {
    let mut app = App::new();
    enter_insert(&mut app);
    // This arbitrary byte offset lies inside the first grapheme and is
    // clamped before insertion.
    app.set_composer_raw_for_test("a\u{301}b", 1, Some(9), 4);

    assert_eq!(
        app.handle_event(Event::Paste("first\nsecond".to_string())),
        None
    );
    assert_eq!(app.input(), "first\nseconda\u{301}b");
    assert_eq!(app.input_cursor(), "first\nsecond".len());
    assert_eq!(app.composer_preferred_column(), None);
    assert_eq!(app.command_menu_selection(), 0);
    assert!(!app.is_busy(), "pasting a newline cannot submit a prefix");
}

#[test]
fn paste_requires_the_visible_composer_to_be_editable() {
    let mut normal = App::new();
    normal.set_input_for_test("draft", "draft".len());
    assert_eq!(
        normal.handle_event(Event::Paste(" pasted".to_string())),
        None
    );
    assert_eq!(normal.input(), "draft");

    let mut busy = App::new();
    busy.set_focus_for_test(crate::app::FocusState::Insert);
    busy.begin_operation_for_test(OperationKind::Submit, SessionMode::Build);
    busy.set_input_for_test("draft", "draft".len());
    assert_eq!(busy.handle_event(Event::Paste(" pasted".to_string())), None);
    assert_eq!(busy.input(), "draft pasted");

    let mut selected = App::new();
    selected.set_focus_for_test(crate::app::FocusState::Insert);
    selected.set_input_for_test("draft", "draft".len());
    selected.select_for_test(cursor(0, 0));
    assert_eq!(
        selected.handle_event(Event::Paste(" pasted".to_string())),
        None
    );
    assert_eq!(selected.input(), "draft");

    let mut inspect_only = App::subtask_inspect("test");
    inspect_only.set_focus_for_test(crate::app::FocusState::Insert);
    inspect_only.set_input_for_test("draft", "draft".len());
    assert_eq!(
        inspect_only.handle_event(Event::Paste(" pasted".to_string())),
        None
    );
    assert_eq!(inspect_only.input(), "draft");

    let artifact = test_plan_artifact();
    let mut approval = App::new();
    approval.set_focus_for_test(crate::app::FocusState::Insert);
    approval.set_input_for_test("hidden draft", "hidden draft".len());
    approval.restore_plan_state(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    assert_eq!(
        approval.handle_event(Event::Paste(" pasted".to_string())),
        None
    );
    assert_eq!(approval.input(), "hidden draft");

    let mut recovery = App::new();
    recovery.set_focus_for_test(crate::app::FocusState::Insert);
    recovery.set_input_for_test("hidden draft", "hidden draft".len());
    recovery.restore_plan_state(PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact),
    });
    assert!(recovery.open_plan_recovery_for_test());
    assert_eq!(
        recovery.handle_event(Event::Paste(" pasted".to_string())),
        None
    );
    assert_eq!(recovery.input(), "hidden draft");
}

#[test]
fn plan_dialog_keeps_copy_ownership_and_never_replays_composer_history() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.handle_event(Event::Paste("draft".into()));
    app.handle_event(Event::Paste(" extra".into()));
    app.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    let artifact = test_plan_artifact();
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input(), "draft");
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        Some(UiAction::Copy {
            text: artifact.markdown
        })
    );
    assert_eq!(app.input(), "draft");
    let rendered = rendered_text(&mut app, 180, 20);
    assert!(!rendered.contains("Ctrl+Z undo"));
    assert!(!rendered.contains("Ctrl+Y redo"));
}

#[test]
fn ordinary_composer_and_recall_edits_accept_paste() {
    let mut composer = App::new();
    enter_insert(&mut composer);
    assert_eq!(
        composer.handle_event(Event::Paste("first\nsecond".to_string())),
        None
    );
    assert_eq!(composer.input(), "first\nsecond");

    let mut recall = App::new();
    recall.seed_history_entry(history_message(Message::user("original")));
    recall.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut recall), None);
    assert!(recall.is_recalling());
    assert_eq!(
        recall.handle_event(Event::Paste("\nrevision".to_string())),
        None
    );
    assert_eq!(recall.input(), "original\nrevision");
}

#[test]
fn horizontal_editing_and_backspace_follow_grapheme_boundaries() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "a\u{301}界c".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    app.handle_event(key(KeyCode::Left));
    app.handle_event(key(KeyCode::Backspace));
    assert_eq!(
        app.input(),
        "a\u{301}c",
        "the wide grapheme is removed whole"
    );
    assert_eq!(app.input_cursor(), "a\u{301}".len());

    app.handle_event(key(KeyCode::Left));
    assert_eq!(app.input_cursor(), 0, "the combining sequence is one stop");
    app.handle_event(key(KeyCode::Right));
    app.handle_event(key(KeyCode::Char('!')));
    assert_eq!(app.input(), "a\u{301}!c");
}

#[test]
fn control_arrows_use_editor_word_tokens_across_punctuation_and_newlines() {
    let input = "alpha...beta_gamma\nnext";
    let beta = input.find("beta_gamma").unwrap();
    let next = input.find("next").unwrap();
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test(input, beta + "beta_".len());

    assert_eq!(
        app.handle_event(modified_key(KeyCode::Left, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input_cursor(), beta);
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Left, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input_cursor(), 0);
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Right, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input_cursor(), beta);
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Right, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(app.input_cursor(), next);

    app.set_input_for_test(input, beta + 2);
    assert_eq!(app.handle_event(key(KeyCode::Left)), None);
    assert_eq!(
        app.input_cursor(),
        beta + 1,
        "plain Left remains grapheme-wise"
    );

    app.set_input_for_test(input, beta + 2);
    assert_eq!(
        app.handle_event(modified_key(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )),
        None
    );
    assert_eq!(
        app.input_cursor(),
        beta + 2,
        "unsupported modifiers are ignored rather than interpreted as plain arrows"
    );
}

#[test]
fn control_k_deletes_to_logical_line_end_without_joining_lines() {
    let mut app = App::new();
    enter_insert(&mut app);
    let input = "alpha βeta\nnext";
    let cursor = "alpha ".len();
    app.set_composer_raw_for_test(input, cursor, Some(9), 4);

    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char('K'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(app.input(), "alpha \nnext");
    assert_eq!(app.input_cursor(), cursor);
    assert_eq!(app.composer_preferred_column(), None);
    assert_eq!(app.command_menu_selection(), 0);
}

#[test]
fn control_shift_k_takes_precedence_and_accepts_both_character_shapes() {
    for character in ['k', 'K'] {
        let mut app = App::new();
        enter_insert(&mut app);
        app.set_input_for_test("one\ntwo\nthree", 5);

        assert_eq!(
            app.handle_event(modified_key(
                KeyCode::Char(character),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            None
        );
        assert_eq!(app.input(), "one\nthree");
        assert_eq!(app.input_cursor(), 4);
    }
}

#[test]
fn new_editor_bindings_fall_through_the_command_menu_and_work_during_recall() {
    let mut menu = App::new();
    enter_insert(&mut menu);
    for character in "/resume".chars() {
        menu.handle_event(key(KeyCode::Char(character)));
    }
    assert!(menu.command_menu_active());
    menu.set_menu_selection_for_test(3);

    assert_eq!(
        menu.handle_event(modified_key(KeyCode::Left, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(menu.input_cursor(), 1);
    assert_eq!(menu.command_menu_selection(), 3);
    assert!(menu.command_menu_active());
    assert_eq!(
        menu.handle_event(modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(menu.input(), "/");
    assert_eq!(menu.command_menu_selection(), 0);

    let mut recall = App::new();
    recall.seed_history_entry(history_message(Message::user("one\ntwo")));
    recall.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut recall), None);
    recall.set_input_for_test("one\ntwo", 5);
    assert_eq!(
        recall.handle_event(modified_key(
            KeyCode::Char('K'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )),
        None
    );
    assert_eq!(recall.input(), "one");
    assert_eq!(recall.input_cursor(), 3);
    assert!(recall.is_recalling());
}

#[test]
fn new_editor_bindings_do_not_edit_in_normal_select_busy_or_locked_states() {
    let mut normal = App::new();
    normal.set_input_for_test("one\ntwo", 1);
    assert_eq!(
        normal.handle_event(modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(normal.input(), "one\ntwo");
    assert_eq!(normal.input_cursor(), 1);

    let mut selected = App::new();
    selected.seed_history_entry(history_message(Message::user("first")));
    selected.seed_history_entry(history_message(Message::assistant("second")));
    selected.set_focus_for_test(crate::app::FocusState::Insert);
    selected.set_input_for_test("one\ntwo", 1);
    selected.select_message_for_test(cursor(1, 0));
    assert_eq!(
        selected.handle_event(modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(selected.input(), "one\ntwo");
    assert_eq!(selected.selection(), cursor(1, 0));

    let mut busy = App::new();
    busy.set_focus_for_test(crate::app::FocusState::Insert);
    busy.begin_operation_for_test(OperationKind::Submit, SessionMode::Build);
    busy.set_input_for_test("one\ntwo", 1);
    assert_eq!(
        busy.handle_event(modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(busy.input(), "o\ntwo");
    assert_eq!(busy.input_cursor(), 1);

    let artifact = test_plan_artifact();
    let mut locked = App::new();
    locked.set_focus_for_test(crate::app::FocusState::Insert);
    locked.set_input_for_test("one\ntwo", 1);
    locked.restore_plan_state(PlanWorkflowState::Ready { artifact });
    assert_eq!(
        locked.handle_event(modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL,)),
        None
    );
    assert_eq!(locked.input(), "one\ntwo");
    assert_eq!(locked.input_cursor(), 1);
}

#[test]
fn starts_in_normal_mode_and_i_enables_the_input_box() {
    let mut app = App::new();

    assert!(!app.interaction().is_insert());
    for ch in "hello".chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(app.input().is_empty());
    assert!(!app.is_busy());

    enter_insert(&mut app);
    for ch in "hello".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.input(), "hello");
}

#[test]
fn escape_disables_the_input_box_and_preserves_the_draft() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "draft".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert!(!app.interaction().is_insert());
    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Backspace)), None);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.input(), "draft");
    assert!(
        !app.is_busy(),
        "Enter must not submit while the input box is disabled"
    );

    enter_insert(&mut app);
    app.handle_event(key(KeyCode::Char('!')));
    assert_eq!(app.input(), "draft!");
}

#[test]
fn normal_mode_j_k_and_gg_shift_g_drive_the_conversation_scroll() {
    let mut app = App::new();
    assert!(app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Char('j'))), None);
    assert!(!app.view_follow());
    assert_eq!(app.view_scroll(), 1);
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.view_scroll(), 2);
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.view_scroll(), 1);

    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('G'),
        KeyModifiers::SHIFT,
    )));
    assert!(app.view_follow(), "G pins the view to the bottom");

    app.handle_event(key(KeyCode::Char('j')));
    app.handle_event(key(KeyCode::Char('g')));
    assert!(app.pending_g());
    assert_eq!(app.view_scroll(), 2, "a single g only arms the sequence");
    app.handle_event(key(KeyCode::Char('g')));
    assert_eq!(app.view_scroll(), 0);
    assert!(!app.view_follow());
    assert!(!app.pending_g());
}

#[test]
fn normal_mode_arrow_keys_scroll_one_line_and_detach_follow() {
    let mut app = App::new();
    app.set_view_for_test(5, true);
    assert!(app.view_follow());
    assert!(!app.interaction().is_insert());

    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.view_scroll(), 4);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.view_scroll(), 5);
    assert!(!app.view_follow());
}

#[test]
fn normal_page_chords_match_page_keys_at_actual_pane_heights() {
    for inspect in [false, true] {
        for height in [5, 10, 17, 26] {
            let make_app = || {
                let mut app = if inspect {
                    App::subtask_inspect("paging")
                } else {
                    App::new()
                };
                app.seed_history_entry(history_message(Message::user("row\n".repeat(200))));
                app.set_view_for_test(100, false);
                app
            };
            let mut chords = make_app();
            let mut pages = make_app();
            let buffer = rendered_buffer(&mut chords, 80, height);
            rendered_text(&mut pages, 80, height);
            let rows = usize::from(conversation_content_area(&buffer, inspect).height).max(1);
            assert!(rows < usize::from(height));
            for _ in 0..2 {
                chords.handle_event(ctrl('b'));
                pages.handle_event(key(KeyCode::PageUp));
            }
            assert_eq!(chords.view_scroll(), 100 - 2 * rows);
            assert_eq!(chords.view_scroll(), pages.view_scroll());
            assert_eq!(chords.rendered_selection_window(), None);
            assert!(!chords.view_follow());
            for _ in 0..2 {
                chords.handle_event(ctrl('f'));
                pages.handle_event(key(KeyCode::PageDown));
            }
            assert_eq!(chords.view_scroll(), 100);
            assert_eq!(chords.view_scroll(), pages.view_scroll());
            assert_eq!(
                rendered_text(&mut chords, 80, height),
                rendered_text(&mut pages, 80, height)
            );
            chords.handle_event(key(KeyCode::Home));
            chords.handle_event(ctrl('b'));
            chords.handle_event(ctrl('u'));
            assert_eq!(chords.view_scroll(), 0);
            assert!(!chords.view_follow());
        }
    }
}

#[test]
fn half_pages_adapt_after_resize_redraw_and_preserve_the_composer() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("row\n".repeat(200))));
    app.set_input_for_test("draft\nsecond line", 3);
    app.set_view_for_test(100, false);
    let mut previous_half = None;
    for height in [10, 13, 20] {
        app.handle_event(Event::Resize(80, height));
        if previous_half.is_some() {
            let before = app.view_scroll();
            app.handle_event(ctrl('u'));
            assert_eq!(
                app.view_scroll(),
                before - 1,
                "resize invalidates the old allocation until redraw"
            );
        }
        let buffer = rendered_buffer(&mut app, 80, height);
        let content = crate::frame_layout::FrameLayout::compute(
            buffer.area,
            false,
            crate::frame_layout::LowerSurface::Composer {
                requested_height: 4,
            },
            false,
        )
        .conversation_content;
        let rows = usize::from(content.height);
        let half = (rows / 2).max(1);
        let before = app.view_scroll();
        app.handle_event(ctrl('u'));
        app.handle_event(ctrl('u'));
        assert_eq!(app.view_scroll(), before - 2 * half);
        app.handle_event(ctrl('d'));
        assert_eq!(app.view_scroll(), before - half);
        assert_eq!(app.input(), "draft\nsecond line");
        assert_eq!(app.input_cursor(), 3);
        assert!(app.interaction().is_normal());
        previous_half = Some(half);
    }
    for chord in ['b', 'f'] {
        app.handle_event(ctrl(chord));
        assert_eq!(app.input(), "draft\nsecond line");
        assert_eq!(app.input_cursor(), 3);
    }
}

#[test]
fn control_paging_handles_stream_growth_and_repins_only_after_render() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("row\n".repeat(60))));
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("stream\n".repeat(10))).into(),
    });
    rendered_text(&mut app, 80, 14);
    let bottom = app.view_scroll();
    app.handle_event(ctrl('u'));
    app.handle_event(ctrl('d'));
    assert_eq!(app.view_scroll(), bottom);
    assert!(!app.view_follow());
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("stream\n".repeat(15))).into(),
    });
    rendered_text(&mut app, 80, 14);
    assert_eq!(app.view_scroll(), bottom);
    assert!(!app.view_follow(), "old bottom is not the rendered bottom");
    app.handle_event(ctrl('f'));
    assert!(!app.view_follow());
    rendered_text(&mut app, 80, 14);
    assert!(app.view_follow());
    assert!(app.view_scroll() > bottom);
    let bottom = app.view_scroll();
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("stream\n".repeat(25))).into(),
    });
    rendered_text(&mut app, 80, 14);
    assert!(app.view_follow());
    assert!(app.view_scroll() > bottom);
}

#[test]
fn paging_is_safe_for_empty_short_zero_height_and_unrendered_panes() {
    for content in [None, Some("short")] {
        let mut app = App::new();
        if let Some(content) = content {
            app.seed_history_entry(history_message(Message::user(content)));
        }
        app.handle_event(ctrl('f'));
        app.handle_event(ctrl('d'));
        assert_eq!(app.view_scroll(), 2, "unmeasured minimum");
        for height in [0, 1, 3, 20] {
            rendered_text(&mut app, 80, height);
            for event in [ctrl('b'), ctrl('u'), ctrl('d'), ctrl('f')] {
                assert_eq!(app.handle_event(event), None);
                rendered_text(&mut app, 80, height);
            }
            // A zero-height pane can still have a nonzero scroll range.
            // Overshoot that short content before expecting a bottom re-pin.
            for _ in 0..5 {
                app.handle_event(ctrl('f'));
            }
            rendered_text(&mut app, 80, height);
            assert!(app.view_follow());
            if height == 20 {
                assert_eq!(app.view_scroll(), 0);
            }
        }
    }
}

#[test]
fn insert_and_approval_keep_fixed_page_steps_and_ignore_control_paging() {
    for approval in [false, true] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("row\n".repeat(200))));
        if approval {
            app.reduce_without_effects(SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Ready {
                    artifact: test_plan_artifact(),
                },
            });
        } else {
            enter_insert(&mut app);
        }
        app.set_input_for_test("draft", 2);
        rendered_text(&mut app, 80, 30);
        let page = app
            .rendered_selection_window()
            .map_or(1, |rows| rows.len())
            .max(1);
        app.set_view_for_test(25, false);
        for chord in ['b', 'f', 'u', 'd'] {
            assert_eq!(app.handle_event(ctrl(chord)), None);
            assert_eq!(app.view_scroll(), 25);
            assert_eq!(app.input(), "draft");
            assert_eq!(app.input_cursor(), 2);
            if approval {
                assert_eq!(app.plan_choice(), PlanChoice::Revise);
            } else {
                assert!(app.interaction().is_insert());
            }
        }
        app.handle_event(key(KeyCode::PageUp));
        assert_eq!(
            app.view_scroll(),
            if approval {
                25usize.saturating_sub(page)
            } else {
                25
            }
        );
        app.handle_event(key(KeyCode::PageDown));
        assert_eq!(app.view_scroll(), 25);
    }
}

#[test]
fn control_paging_does_not_escape_picker_or_question_input_ownership() {
    for question in [false, true] {
        let mut root = App::new();
        if question {
            start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
        }
        root.seed_history_entry(history_message(Message::user("row\n".repeat(100))));
        root.set_input_for_test("hidden draft", 3);
        root.set_view_for_test(20, false);
        let mut views = test_session_views(root);
        if question {
            views.apply(SessionEvent::QuestionAsked {
                turn_id: TEST_TURN_ID,
                request: question_request("paging-question"),
            });
        } else {
            views.open_session_picker(vec![session_summary("paging-session", None)]);
        }
        rendered_views_text(&mut views, 80, 20);
        for event in [
            ctrl('b'),
            ctrl('f'),
            ctrl('u'),
            ctrl('d'),
            key(KeyCode::PageUp),
            key(KeyCode::PageDown),
        ] {
            assert_eq!(views.handle_event(event), None);
            assert_eq!(views.root().view_scroll(), 20);
            assert!(!views.root().view_follow());
            assert_eq!(views.root().input(), "hidden draft");
            assert_eq!(views.root().input_cursor(), 3);
            assert_eq!(views.root().selection(), None);
        }
        let modal = rendered_views_text(&mut views, 80, 20);
        assert!(modal.contains(if question { "Scope" } else { "Resume session" }));
    }
}

#[test]
fn inspect_control_d_scrolls_without_toggling_plain_d_diagnostics() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "row\n".repeat(100),
            message_id: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Stderr {
            text: "diagnostic sentinel".into(),
        },
    );
    app.set_view_for_test(10, false);
    let buffer = rendered_buffer(&mut app, 80, 10);
    let half = (usize::from(conversation_content_area(&buffer, true).height) / 2).max(1);
    for diagnostics in [false, true] {
        let before = app.view_scroll();
        app.handle_event(ctrl('d'));
        assert_eq!(app.view_scroll(), before + half);
        rendered_text(&mut app, 80, 10);
        assert_eq!(
            laid_out_transcript_text(&app).contains("diagnostic sentinel"),
            diagnostics
        );
        app.handle_event(key(KeyCode::Char('d')));
        rendered_text(&mut app, 80, 10);
        assert_eq!(
            laid_out_transcript_text(&app).contains("diagnostic sentinel"),
            !diagnostics
        );
    }
}

#[test]
fn downward_scroll_repins_only_at_rendered_bottom_and_tracks_streaming_growth() {
    let mut app = App::new();
    for index in 0..12 {
        app.seed_history_entry(history_message(Message::user(format!("msg {index:02}"))));
    }
    let _ = rendered_text(&mut app, 30, 10);
    let bottom = app.view_scroll();
    assert!(bottom >= 2, "fixture must be taller than the viewport");

    for _ in 0..2 {
        assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    }
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom - 2);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(!app.view_follow(), "the key press detaches before render");
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(
        !app.view_follow(),
        "stopping short of the bottom stays detached"
    );

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert!(
        !app.view_follow(),
        "render resolves whether Down reached the bottom"
    );
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom);
    assert!(
        app.view_follow(),
        "touching the bottom restores live-tail follow"
    );

    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::assistant(
                "stream row 0\nstream row 1\nstream row 2\nstream row 3\nstream row 4\nlatest streamed tail",
            )).into(),
},
    );
    let streamed = rendered_text(&mut app, 30, 10);
    assert!(app.view_follow());
    assert!(
        app.view_scroll() > bottom,
        "the growing tail advances the viewport"
    );
    assert!(streamed.contains("latest streamed tail"));
}

#[test]
fn page_down_overshoot_repins_at_the_rendered_bottom() {
    let mut app = App::new();
    for index in 0..12 {
        app.seed_history_entry(history_message(Message::user(format!("msg {index:02}"))));
    }
    let _ = rendered_text(&mut app, 30, 10);
    let bottom = app.view_scroll();
    assert!(bottom >= 3, "fixture must be taller than the viewport");

    for _ in 0..3 {
        assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    }
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom - 3);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::PageDown)), None);
    assert!(
        app.view_scroll() > bottom,
        "PageDown should overshoot this fixture"
    );
    assert!(!app.view_follow(), "the next frame resolves the overshoot");
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom);
    assert!(app.view_follow());
}

#[test]
fn later_navigation_supersedes_an_unrendered_bottom_repin() {
    let mut app = App::new();

    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.handle_event(key(KeyCode::Home)), None);
    let _ = rendered_text(&mut app, 40, 10);
    assert_eq!(app.view_scroll(), 0);
    assert!(
        !app.view_follow(),
        "Home supersedes the earlier downward intent"
    );

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    let _ = rendered_text(&mut app, 40, 10);
    assert_eq!(app.view_scroll(), 0);
    assert!(
        app.view_follow(),
        "a downward scroll on a zero-range transcript is already at the bottom"
    );
}

#[test]
fn any_other_key_disarms_a_pending_gg() {
    let mut app = App::new();
    for _ in 0..3 {
        app.handle_event(key(KeyCode::Char('j')));
    }
    app.handle_event(key(KeyCode::Char('g')));
    assert!(app.pending_g());
    app.handle_event(key(KeyCode::Char('j')));
    assert!(!app.pending_g());
    app.handle_event(key(KeyCode::Char('g')));
    assert_eq!(
        app.view_scroll(),
        4,
        "g after a disarm arms again instead of jumping"
    );
    app.handle_event(key(KeyCode::Char('g')));
    assert_eq!(app.view_scroll(), 0);
}

#[test]
fn submission_returns_to_transcript_navigation_before_acknowledgement_and_while_streaming() {
    let mut app = App::new();
    for index in 0..12 {
        app.seed_history_entry(history_message(Message::user(format!("msg {index:02}"))));
    }
    let _ = rendered_text(&mut app, 30, 10);

    enter_insert(&mut app);
    for ch in "go".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "go".into(),
            mode: SessionMode::Build,
        })
    );
    assert!(app.is_busy());
    assert!(app.interaction().is_normal());
    assert!(!cursor_visible_after_render(&mut app, 30, 10));
    let bottom = app.view_scroll();
    assert!(bottom > 0);
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(app.input().is_empty());
    app.handle_event(key(KeyCode::Char('G')));

    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("go"),
        mode: SessionMode::Build,
    });
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant(
            "stream row 0\nstream row 1\nstream row 2\nstream row 3\nstream row 4\nlatest streamed tail",
        )).into(),
});
    let streamed = rendered_text(&mut app, 30, 10);
    assert!(streamed.contains("latest streamed tail"));
    assert!(app.view_follow());
    let bottom = app.view_scroll();
    assert!(bottom >= 2, "fixture must be taller than the viewport");
    assert!(app.interaction().is_normal());
    assert!(!cursor_visible_after_render(&mut app, 30, 10));

    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Char('k'))), None);
    assert_eq!(app.view_scroll(), bottom - 2);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(!app.view_follow());
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom - 1);
    assert!(!app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Char('j'))), None);
    assert_eq!(app.view_scroll(), bottom);
    assert!(!app.view_follow(), "render resolves the bottom re-pin");
    let _ = rendered_text(&mut app, 30, 10);
    assert_eq!(app.view_scroll(), bottom);
    assert!(app.view_follow());

    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert!(
        app.input().is_empty(),
        "Normal focus does not edit the draft"
    );
    enter_insert(&mut app);
    assert!(cursor_visible_after_render(&mut app, 30, 10));
    app.handle_event(Event::Paste("next draft".into()));
    app.handle_event(key(KeyCode::Enter));
    app.handle_event(key(KeyCode::Char('j')));
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.input(), "next draft\njk");
    assert_eq!(app.handle_event(ctrl_enter()), None);
    assert!(app.interaction().is_insert());
    assert_eq!(app.input(), "next draft\njk");
}

#[test]
fn an_in_flight_turn_preserves_insert_focus_and_new_drafts() {
    let mut local = App::new();
    enter_insert(&mut local);
    for ch in "go".chars() {
        local.handle_event(key(KeyCode::Char(ch)));
    }
    assert!(local.handle_event(ctrl_enter()).is_some());
    assert!(local.is_busy());
    assert!(local.interaction().is_normal());
    assert!(!cursor_visible_after_render(&mut local, 80, 20));

    assert_eq!(local.handle_event(key(KeyCode::Char('i'))), None);
    assert!(local.interaction().is_insert());
    assert_eq!(local.handle_event(key(KeyCode::Char('x'))), None);
    assert_eq!(local.input(), "x");
    assert!(cursor_visible_after_render(&mut local, 80, 20));

    local.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("go"),
        mode: SessionMode::Build,
    });
    assert!(local.interaction().is_insert());
    assert_eq!(local.input(), "x");
    local.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
        message: Message::assistant("done"),
    });
    assert!(!local.is_busy());
    assert!(local.interaction().is_insert());
    assert_eq!(local.input(), "x");
    local.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert!(local.input().is_empty());
    local.handle_event(modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert_eq!(local.input(), "x");

    enter_insert(&mut local);
    local.handle_event(key(KeyCode::Char('j')));
    local.handle_event(key(KeyCode::Char('k')));
    assert_eq!(local.input(), "xjk");
    local.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(local.input(), "x");

    let authoritative_turn_id = TurnId::new(2);
    let mut authoritative = App::new();
    enter_insert(&mut authoritative);
    authoritative.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: authoritative_turn_id,
        message: Message::user("engine-started"),
        mode: SessionMode::Build,
    });
    assert!(authoritative.is_busy());
    assert!(authoritative.interaction().is_insert());
    assert_eq!(authoritative.handle_event(key(KeyCode::Char('i'))), None);
    assert!(authoritative.interaction().is_insert());

    authoritative.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: authoritative_turn_id,
        message: Message::assistant("done"),
    });
    assert!(!authoritative.is_busy());
    assert!(authoritative.interaction().is_insert());
    assert_eq!(authoritative.input(), "i");
}

#[test]
fn v_types_in_insert_and_completion_and_ctrl_v_still_requests_clipboard_paste() {
    for draft in ["draft", "/re"] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("selectable history")));
        enter_insert(&mut app);
        app.set_input_for_test(draft, draft.len());
        rendered_text(&mut app, 80, 12);
        assert!(app.rendered_selection_window().is_some());
        assert_eq!(app.command_menu_active(), draft.starts_with('/'));
        assert_eq!(app.handle_event(key(KeyCode::Char('v'))), None);
        assert_eq!(app.input(), format!("{draft}v"));
        assert!(app.interaction().is_insert());
        assert_eq!(app.selection(), None);
        let Some(UiAction::ReadClipboard { cursor, .. }) = app.handle_event(ctrl('v')) else {
            panic!("Ctrl+V must retain clipboard ownership in {draft:?}");
        };
        assert_eq!(cursor, app.input_cursor());
        assert_eq!(app.input(), format!("{draft}v"));
        assert!(app.interaction().is_insert());
        assert_eq!(app.selection(), None);
    }
}

#[test]
fn v_requires_normal_focus_even_when_insert_cannot_edit() {
    for mut app in [App::subtask_inspect("child"), App::acp_inspect("agent")] {
        app.seed_history_entry(history_message(Message::user("selectable history")));
        app.set_input_for_test("hidden draft", 3);
        rendered_text(&mut app, 80, 12);
        app.set_focus_for_test(crate::app::FocusState::Insert);
        assert!(app.capabilities().edit_draft.is_err());
        let before = (app.view_scroll(), app.view_follow());
        assert_eq!(app.handle_event(key(KeyCode::Char('v'))), None);
        assert!(app.interaction().is_insert());
        assert_eq!(app.selection(), None);
        assert_eq!(app.input(), "hidden draft");
        assert_eq!(app.input_cursor(), 3);
        assert_eq!((app.view_scroll(), app.view_follow()), before);
    }
}

#[test]
fn v_selection_ignores_modified_uppercase_repeat_and_release_keys() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("selectable history")));
    app.set_input_for_test("draft", 2);
    rendered_text(&mut app, 80, 12);
    let before = (app.view_scroll(), app.view_follow());
    let rebuilds = app.view_cache().rebuilds;
    let mut events = vec![
        key(KeyCode::Char('V')),
        modified_key(KeyCode::Char('V'), KeyModifiers::SHIFT),
    ];
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        KeyModifiers::ALT | KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyModifiers::SUPER,
        KeyModifiers::HYPER,
        KeyModifiers::META,
    ] {
        events.push(modified_key(KeyCode::Char('v'), modifiers));
    }
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        events.push(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
            kind,
        )));
    }
    for event in events {
        assert_eq!(app.handle_event(event), None);
        assert!(app.interaction().is_normal());
        assert_eq!(app.selection(), None);
        assert_eq!(app.input(), "draft");
        assert_eq!(app.input_cursor(), 2);
        assert_eq!((app.view_scroll(), app.view_follow()), before);
        assert_eq!(app.view_cache().rebuilds, rebuilds);
    }
    press_v(&mut app);
    assert_eq!(app.selection(), cursor(0, 0));
    app.handle_event(key(KeyCode::Esc));
    assert!(app.interaction().is_normal());
    assert_eq!(app.selection(), None);
}

#[test]
fn v_does_not_steal_plan_review_or_recovery_input() {
    for recovery in [false, true] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("selectable history")));
        app.set_input_for_test("hidden draft", 3);
        let artifact = test_plan_artifact();
        if recovery {
            app.restore_plan_state(PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact),
            });
            assert!(app.open_plan_recovery_for_test());
        } else {
            app.restore_plan_state(PlanWorkflowState::Ready { artifact });
        }
        rendered_text(&mut app, 100, 24);
        assert!(app.rendered_selection_window().is_some());
        let dialog = app.render_parts().plan_dialog.unwrap().state;
        let before = (app.view_scroll(), app.view_follow());
        assert_eq!(app.handle_event(key(KeyCode::Char('v'))), None);
        assert_eq!(app.render_parts().plan_dialog.unwrap().state, dialog);
        assert!(app.interaction().is_normal());
        assert_eq!(app.selection(), None);
        assert_eq!(app.input(), "hidden draft");
        assert_eq!(app.input_cursor(), 3);
        assert_eq!((app.view_scroll(), app.view_follow()), before);
    }
}

#[test]
fn double_escape_from_insert_mode_selects_and_select_exit_returns_to_normal() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("hi")));
    enter_insert(&mut app);
    rendered_text(&mut app, 80, 12);
    let now = Instant::now();

    app.handle_event_at(key(KeyCode::Esc), now);
    assert!(!app.interaction().is_insert());
    assert_eq!(app.selection(), None);
    app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(100));
    assert_eq!(app.selection(), cursor(0, 0));

    app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(200));
    assert_eq!(app.selection(), None);
    assert!(!app.interaction().is_insert());
    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert!(app.input().is_empty());
}

#[test]
fn ctrl_c_quits_but_a_single_escape_does_not() {
    let mut app = App::new();

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);

    let action = app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(action, Some(UiAction::Quit));
}

#[test]
fn ctrl_c_clears_any_nonempty_draft_before_it_quits() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test(" \n", " \n".len());
    app.set_composer_scroll_for_test(3);
    app.set_menu_selection_for_test(2);

    assert_eq!(app.handle_event(ctrl('c')), None);
    assert!(app.input().is_empty());
    assert_eq!(app.input_cursor(), 0);
    assert_eq!(app.composer_scroll(), 0);
    assert_eq!(app.command_menu_selection(), 0);
    assert_eq!(app.handle_event(ctrl('c')), Some(UiAction::Quit));
}

#[test]
fn select_mode_navigates_messages_then_blocks_and_clamps_in_each_scope() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::User {
        content: vec![
            UserContent::Text(Text::new("user one")),
            UserContent::Text(Text::new("user two")),
        ],
    }));
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::text("assistant one"),
            AssistantContent::text("assistant two"),
        ],
    }));
    app.seed_history_entry(HistoryEntry::Error("network down".to_string()));
    rendered_text(&mut app, 80, 20);
    double_escape(&mut app);
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert_eq!(app.selection(), cursor(2, 0));
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(
        app.selection(),
        cursor(2, 0),
        "j clamps at the newest message"
    );
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.selection(), cursor(1, 0));
    app.handle_event(key(KeyCode::Up));
    assert_eq!(app.selection(), cursor(0, 0));
    app.handle_event(key(KeyCode::Up));
    assert_eq!(
        app.selection(),
        cursor(0, 0),
        "k clamps at the oldest message"
    );
    app.handle_event(key(KeyCode::Down));
    assert_eq!(app.selection(), cursor(1, 0));
    app.handle_event(key(KeyCode::Enter));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Block));
    assert!(!app.interaction().selection_reveal());
    app.handle_event(key(KeyCode::Up));
    assert_eq!(app.selection(), cursor(1, 0), "blocks do not cross entries");
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(1, 1));
    app.handle_event(key(KeyCode::Down));
    assert_eq!(app.selection(), cursor(1, 1), "last block clamps");
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.selection(), cursor(1, 0));
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert!(!app.interaction().selection_reveal());
    assert_eq!(app.selection(), cursor(1, 0));
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection(), None);
}

#[test]
fn escape_exits_select_mode_and_typing_is_ignored_while_selecting() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("hi")));
    app.select_for_test(cursor(0, 0));

    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(app.input().is_empty());

    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert_eq!(app.selection(), cursor(0, 0));
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection(), None);
    // The exit press must not count toward the next double-Esc.
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection(), None);
}

#[test]
fn live_and_restored_ensemble_entries_display_prompts_but_copy_full_commands() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        for restored in [false, true] {
            for prompt in [
                "inspect error handling",
                "inspect error handling\n\nkeep the failure details",
                "/ensemble-plan should stay literal\n/ensemble-review should too",
                "",
            ] {
                let start = editable_ensemble_start("display-run", workflow, prompt);
                let mut app = App::new();
                if restored {
                    app.restore(vec![TranscriptItem::Ensemble(EnsembleRecord::Started {
                        start,
                    })]);
                } else {
                    app.reduce_without_effects(SessionEvent::EnsembleStarted {
                        turn_id: TEST_TURN_ID,
                        start,
                        resumed: false,
                    });
                }
                app.select_for_test(cursor(0, 0));
                let _ = rendered_buffer(&mut app, 100, 20);
                let rendered = laid_out_transcript_text(&app);
                let prompt_body = if prompt.is_empty() { " " } else { prompt };
                let confirmation = if workflow == EnsembleWorkflow::Plan {
                    "0/1 workers confirmed · synthesis waits for every exact revision\n"
                } else {
                    ""
                };
                let status = if restored { "interrupted" } else { "queued" };
                let turn = " · #1";
                assert_eq!(
                    rendered,
                    format!(
                        "● {workflow}{turn}\n{prompt_body}\n{confirmation}{} Fake ACP · read-only · {status}",
                        if restored { "◼" } else { "○" }
                    ),
                    "{workflow:?}, restored={restored}, prompt={prompt:?}"
                );
                let command = format!("{} {prompt}", workflow.slash_command());
                assert!(!rendered.contains(&command));

                let prompt_lines = prompt_body.lines().collect::<Vec<_>>();
                let entry = &app.view_cache().entries()[0];
                assert_eq!(
                    entry.lines[1..1 + prompt_lines.len()]
                        .iter()
                        .map(line_text)
                        .collect::<Vec<_>>(),
                    prompt_lines,
                    "each prompt line must be an ordinary display line"
                );
                assert_eq!(
                    entry.selection,
                    Some(RowRange::new(1, 1 + prompt_lines.len()))
                );
                assert_eq!(
                    app.handle_event(key(KeyCode::Char('y'))),
                    Some(UiAction::Copy { text: command })
                );
            }
        }
    }
}

#[test]
fn ensemble_multiline_selection_tracks_wrapping_and_worker_navigation() {
    fn assert_selection(app: &App, buffer: &ratatui::buffer::Buffer, expected: RowRange) {
        assert_eq!(app.view_scroll(), 0, "the complete fixture should fit");
        assert_eq!(app.view_cache().entries()[0].selection, Some(expected));
        let content = conversation_content_area(buffer, false);
        for y in content.y..content.bottom() {
            let row = usize::from(y - content.y);
            let selected = row >= expected.start() && row < expected.end();
            assert!(
                (content.x..content.right())
                    .all(|x| (buffer[(x, y)].bg == SELECTION_BG) == selected),
                "selection band at row {row} must match {expected:?}"
            );
        }
    }

    let prompt = "inspect error handling across worker boundaries and preserve failure details\n\nkeep /ensemble-plan and /ensemble-review literal";
    let failure = "unable to inspect the requested source file";
    let statuses = [
        AgentRunStatus::Confirmed,
        AgentRunStatus::Completed,
        AgentRunStatus::Failed,
    ];
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let mut start = editable_ensemble_start("wrapped-run", workflow, prompt);
        start.agents = (1..=3)
            .map(|index| AgentRunDescriptor {
                id: AgentRunId::from_string(format!("wrapped-worker-{index}")),
                agent: "fake".to_string(),
                label: format!("Worker {index}"),
                safe_mode: "read-only".to_string(),
            })
            .collect();
        let mut app = App::new();
        app.reduce_without_effects(SessionEvent::EnsembleStarted {
            turn_id: TEST_TURN_ID,
            start: start.clone(),
            resumed: false,
        });
        for (descriptor, status) in start.agents.iter().zip(statuses) {
            app.reduce_without_effects(SessionEvent::AgentRunUpdated {
                turn_id: TEST_TURN_ID,
                ensemble_run_id: start.run_id.clone(),
                agent_run_id: descriptor.id.clone(),
                event: AgentRunEvent::Status {
                    status,
                    detail: (status == AgentRunStatus::Failed).then(|| failure.to_string()),
                },
            });
        }

        // Resize the same entry so both wrapping and cache invalidation are exercised.
        for width in [36, 100] {
            app.select_for_test(cursor(0, 0));
            let buffer = rendered_buffer(&mut app, width, 40);
            let wrap_width = conversation_content_area(&buffer, false).width;
            let prompt_lines = prompt
                .lines()
                .map(ratatui::text::Line::raw)
                .collect::<Vec<_>>();
            let prompt_height = crate::layout::prepare::wrapped_height(&prompt_lines, wrap_width);
            if width == 36 {
                assert!(prompt_height > prompt_lines.len(), "the prompt must wrap");
            } else {
                assert_eq!(prompt_height, prompt_lines.len());
            }
            let prompt_range = RowRange::from_start_len(1, prompt_height);
            assert_selection(&app, &buffer, prompt_range);
            let rendered = laid_out_transcript_text(&app);
            assert!(rendered.contains(prompt));
            assert!(!rendered.contains(&start.command()));
            assert_eq!(app.handle_event(key(KeyCode::Enter)), None);

            let confirmation = "2/3 workers confirmed · synthesis waits for every exact revision";
            let mut worker_start = prompt_range.end();
            if workflow == EnsembleWorkflow::Plan {
                assert!(rendered.contains(confirmation));
                worker_start += crate::layout::prepare::wrapped_height(
                    &[ratatui::text::Line::raw(confirmation)],
                    wrap_width,
                );
            } else {
                assert!(!rendered.contains("workers confirmed"));
            }
            for (index, (descriptor, status)) in start.agents.iter().zip(statuses).enumerate() {
                app.handle_event(key(KeyCode::Char('j')));
                assert_eq!(app.selection(), cursor(0, index + 1));
                let mut worker_lines = vec![ratatui::text::Line::raw(format!(
                    "{} {} · {} · {status}",
                    crate::status_icon::StatusIcon::agent(status).glyph(0),
                    descriptor.label,
                    descriptor.safe_mode
                ))];
                if status == AgentRunStatus::Failed {
                    worker_lines.push(ratatui::text::Line::raw(format!("    {failure}")));
                }
                for line in &worker_lines {
                    assert!(rendered.contains(&line_text(line)));
                }
                let range = RowRange::from_start_len(
                    worker_start,
                    crate::layout::prepare::wrapped_height(&worker_lines, wrap_width),
                );
                let buffer = rendered_buffer(&mut app, width, 40);
                assert_selection(&app, &buffer, range);
                assert_eq!(
                    app.handle_event(key(KeyCode::Enter)),
                    Some(UiAction::OpenAgentRun {
                        id: descriptor.id.clone()
                    })
                );
                worker_start = range.end();
            }
            assert_eq!(app.view_cache().entries()[0].height, worker_start);
            for index in (0..start.agents.len()).rev() {
                app.handle_event(key(KeyCode::Up));
                assert_eq!(app.selection(), cursor(0, index));
            }
            let buffer = rendered_buffer(&mut app, width, 40);
            assert_selection(&app, &buffer, prompt_range);
        }
    }
}

#[test]
fn ctrl_e_recalls_the_full_command_only_from_an_ensemble_prompt_row() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let prompt = "design the durable edit\nkeep /ensemble-plan and /ensemble-review literal";
        let start = editable_ensemble_start("editable-run", workflow, prompt);
        assert_eq!(
            start.command(),
            format!("{} {prompt}", workflow.slash_command())
        );
        let mut app = idle_app_with_ensemble(start.clone());
        app.select_for_test(cursor(0, 0));
        assert!(rendered_text(&mut app, 100, 20).contains("Ctrl+E edit"));

        assert_eq!(ctrl_e(&mut app), None);
        assert_eq!(app.input(), start.command());
        assert!(matches!(
            app.recalled_edit_target(),
            Some(TranscriptEditTarget::EnsembleRun(run_id)) if run_id == &start.run_id
        ));

        let mut worker = idle_app_with_ensemble(start);
        worker.select_for_test(cursor(0, 1));
        assert!(!rendered_text(&mut worker, 100, 20).contains("Ctrl+E edit"));
        assert_eq!(ctrl_e(&mut worker), None);
        assert!(!worker.is_recalling());
        assert!(worker.input().is_empty());
    }
}

#[test]
fn escape_from_an_ensemble_edit_restores_the_exact_composer_state() {
    let start = editable_ensemble_start(
        "escape-run",
        EnsembleWorkflow::Review,
        "review the boundary",
    );
    let mut app = idle_app_with_ensemble(start);
    app.set_input_for_test("saved draft", "saved".len());
    app.set_focus_for_test(crate::app::FocusState::Insert);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    let edited = format!("{}!", app.input());
    let cursor = edited.len();
    app.set_input_for_test(edited, cursor);

    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(app.input(), "saved draft");
    assert_eq!(app.input_cursor(), "saved".len());
    assert!(!app.interaction().is_insert());
    assert!(!app.is_recalling());
}

#[test]
fn ensemble_row_edits_reclassify_exact_plan_and_review_commands() {
    for original in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        for replacement_workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
            let start = editable_ensemble_start(
                &format!("{original:?}-{replacement_workflow:?}"),
                original,
                "old prompt",
            );
            let mut app = idle_app_with_ensemble(start.clone());
            app.select_for_test(cursor(0, 0));
            ctrl_e(&mut app);
            let input = format!(
                "{} replacement prompt",
                replacement_workflow.slash_command()
            );
            let cursor = input.len();
            app.set_input_for_test(input, cursor);

            assert_eq!(
                app.handle_event(ctrl_enter()),
                Some(UiAction::EditTranscript(TranscriptEdit {
                    target: TranscriptEditTarget::EnsembleRun(start.run_id),
                    replacement: TranscriptEditReplacement::Ensemble {
                        workflow: replacement_workflow,
                        prompt: "replacement prompt".into(),
                    },
                }))
            );
            assert!(app.interaction().is_normal());
            assert!(app.is_awaiting_edit_acceptance());
            assert!(app.input().is_empty());
            assert_eq!(app.next_mode(), SessionMode::Build);
            assert_eq!(
                app.in_flight_mode(),
                Some(match replacement_workflow {
                    EnsembleWorkflow::Plan => SessionMode::Plan,
                    EnsembleWorkflow::Review => SessionMode::Build,
                })
            );
        }
    }
}

#[test]
fn ensemble_row_plain_and_leading_space_edits_become_messages() {
    for (edited, expected) in [
        ("plain replacement", "plain replacement"),
        (" /ensemble-plan escaped", "/ensemble-plan escaped"),
        (" $review escaped", "$review escaped"),
    ] {
        let start = editable_ensemble_start(
            &format!("literal-{}", edited.len()),
            EnsembleWorkflow::Review,
            "old prompt",
        );
        let mut app = idle_app_with_ensemble(start.clone()).with_skills(vec![SkillMeta {
            name: "review".parse().unwrap(),
            description: "Review".to_string(),
        }]);
        app.set_mode_for_test(SessionMode::Plan);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        app.set_input_for_test(edited, edited.len());

        assert_eq!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::EnsembleRun(start.run_id),
                replacement: TranscriptEditReplacement::Message {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: expected.into(),
                    mode: SessionMode::Plan,
                },
            }))
        );
        assert_eq!(app.in_flight_mode(), Some(SessionMode::Plan));
    }
}

#[test]
fn ensemble_row_exact_skill_edits_remain_typed_skills() {
    let start =
        editable_ensemble_start("ensemble-to-skill", EnsembleWorkflow::Review, "old prompt");
    let mut app = idle_app_with_ensemble(start.clone()).with_skills(vec![SkillMeta {
        name: "review".parse().unwrap(),
        description: "Review".to_string(),
    }]);
    app.set_mode_for_test(SessionMode::Plan);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    let input = "$review  inspect this \n carefully ";
    app.set_input_for_test(input, input.len());

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            target: TranscriptEditTarget::EnsembleRun(start.run_id),
            replacement: TranscriptEditReplacement::Skill {
                name: "review".parse().unwrap(),
                args: "inspect this \n carefully".into(),
                mode: SessionMode::Plan,
            },
        }))
    );
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Plan));
}

#[test]
fn invalid_ensemble_row_replacements_leave_the_recall_and_draft_active() {
    for edited in [
        "/ensemble nope",
        "/ensemble-plna typo",
        "/compact",
        "$missing inspect this",
        "/ensemble-plan   ",
        "/ensemble-review\n\t",
    ] {
        let start = editable_ensemble_start(
            &format!("invalid-{}", edited.len()),
            EnsembleWorkflow::Review,
            "old prompt",
        );
        let mut app = idle_app_with_ensemble(start);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        app.set_input_for_test(edited, edited.len());
        let draft = app.input().to_string();

        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert_eq!(app.input(), draft);
        assert!(app.interaction().is_insert());
        assert!(app.is_recalling());
        assert!(!app.is_awaiting_edit_acceptance());
        assert!(!app.is_busy());
        assert!(matches!(app.history().last(), Some(HistoryEntry::Error(_))));
    }
}

#[test]
fn blank_exact_ensemble_edits_stay_active_and_correctable() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let start = editable_ensemble_start("blank-edit", workflow, "old prompt");
        let mut app = idle_app_with_ensemble(start);
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        let input = format!("{}   ", workflow.slash_command());
        let cursor = input.len();
        app.set_input_for_test(input, cursor);
        let draft = app.input().to_string();

        assert_eq!(app.handle_event(ctrl_enter()), None);
        assert_eq!(app.input(), draft);
        assert!(app.interaction().is_insert());
        assert!(app.is_recalling());
        assert!(!app.is_awaiting_edit_acceptance());
        assert!(!app.is_busy());
        assert!(matches!(
            app.history().last(),
            Some(HistoryEntry::Error(error))
                if error == &format!("{} requires a prompt.", workflow.slash_command())
        ));
    }
}

#[test]
fn ordinary_message_recalls_can_become_plan_or_review_ensembles() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("ordinary prompt")));
        app.select_for_test(cursor(0, 0));
        ctrl_e(&mut app);
        let input = format!("{} replacement prompt", workflow.slash_command());
        let cursor = input.len();
        app.set_input_for_test(input, cursor);

        assert_eq!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(0),
                replacement: TranscriptEditReplacement::Ensemble {
                    workflow,
                    prompt: "replacement prompt".into(),
                },
            }))
        );
        assert_eq!(
            app.in_flight_mode(),
            Some(match workflow {
                EnsembleWorkflow::Plan => SessionMode::Plan,
                EnsembleWorkflow::Review => SessionMode::Build,
            })
        );
    }
}

#[test]
fn ensemble_tail_rewrites_commit_only_on_the_matching_acceptance_event() {
    let old = editable_ensemble_start("old-acceptance", EnsembleWorkflow::Review, "old");
    let fresh = editable_ensemble_start("fresh-acceptance", EnsembleWorkflow::Plan, "fresh");
    let mut ensemble = idle_app_with_ensemble(old.clone());
    ensemble.seed_history_entry(history_message(Message::assistant("old tail")));
    ensemble.select_for_test(cursor(0, 0));
    ctrl_e(&mut ensemble);
    ensemble.set_input_for_test("/ensemble-plan fresh", "/ensemble-plan fresh".len());
    assert!(matches!(
        ensemble.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(_))
    ));
    assert_eq!(
        ensemble.history().len(),
        2,
        "pending edits retain the old tail"
    );

    let effects = ensemble.reduce(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(2),
        start: fresh.clone(),
        resumed: false,
    });
    assert_eq!(ensemble.history().len(), 1);
    assert!(matches!(
        ensemble.history().first(),
        Some(HistoryEntry::Ensemble(entry)) if entry.run_id == fresh.run_id && entry.header == Some(crate::presentation::NativeHeader::Prompt(crate::presentation::DisplayTurn(1)))
    ));
    assert_eq!(
        effects,
        vec![AppEffect::PruneEnsembleRuns(vec![old.run_id.clone()])]
    );

    let mut message = idle_app_with_ensemble(old.clone());
    message.seed_history_entry(history_message(Message::assistant("old tail")));
    message.select_for_test(cursor(0, 0));
    ctrl_e(&mut message);
    message.set_input_for_test("ordinary replacement", "ordinary replacement".len());
    message.handle_event(ctrl_enter());
    assert_eq!(message.history().len(), 2);
    let effects = message.reduce(SessionEvent::TurnStarted {
        turn_id: TurnId::new(3),
        message: Message::user("ordinary replacement"),
        mode: SessionMode::Build,
    });
    assert_eq!(
        effects,
        vec![AppEffect::PruneEnsembleRuns(vec![old.run_id.clone()])]
    );
    assert_eq!(message.history().len(), 1);
    assert!(
        matches!(&message.history()[0], HistoryEntry::Conversation(entry)
        if entry.header == Some(crate::presentation::NativeHeader::Prompt(crate::presentation::DisplayTurn(1))))
    );
    assert!(conversation_has_role(
        &message.history()[0],
        PresentationRole::User
    ));

    for cancelled in [false, true] {
        let mut rejected = idle_app_with_ensemble(old.clone());
        rejected.seed_history_entry(history_message(Message::assistant("old tail")));
        rejected.select_for_test(cursor(0, 0));
        ctrl_e(&mut rejected);
        rejected.set_input_for_test("replacement", "replacement".len());
        rejected.handle_event(ctrl_enter());
        if cancelled {
            rejected.reduce_without_effects(SessionEvent::TurnCancelled {
                turn_id: TurnId::new(4),
            });
        } else {
            rejected.reduce_without_effects(SessionEvent::TurnRejected {
                turn_id: TurnId::new(4),
                error: "pre-acceptance failure".to_string(),
            });
        }
        assert!(matches!(
            rejected.history().first(),
            Some(HistoryEntry::Ensemble(entry)) if entry.run_id == old.run_id
        ));
        assert!(conversation_has_role(
            &rejected.history()[1],
            PresentationRole::Assistant
        ));
        // Rejected edits never emit pruning effects or truncate the old tail.
    }
}

#[test]
fn ctrl_e_recalls_a_selected_user_message_into_the_composer() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("original question")));
    app.seed_history_entry(history_message(Message::assistant("an answer")));
    app.select_for_test(cursor(0, 0));

    assert_eq!(ctrl_e(&mut app), None);
    assert_eq!(app.input(), "original question");
    assert!(app.interaction().is_insert());
    assert_eq!(app.selection(), None, "the edit takes over the input box");
    assert!(app.is_recalling());
}

#[test]
fn ctrl_e_ignores_non_editable_selections() {
    let mut assistant = App::new();
    assistant.seed_history_entry(history_message(Message::user("question")));
    assistant.seed_history_entry(history_message(Message::assistant("answer")));
    assistant.select_for_test(cursor(1, 0));
    assert_eq!(ctrl_e(&mut assistant), None);
    assert!(!assistant.is_recalling());

    let mut error = App::new();
    error.seed_history_entry(HistoryEntry::Error("boom".to_string()));
    error.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut error), None);
    assert!(!error.is_recalling());

    let mut document = App::new();
    document.seed_history_entry(history_message(Message::User {
        content: vec![UserContent::Document(Document {
            data: DocumentSourceKind::String("payload".to_string()),
            media_type: None,
            additional_params: None,
        })],
    }));
    document.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut document), None);
    assert!(!document.is_recalling());
    assert!(document.input().is_empty());
}

#[test]
fn ctrl_e_requires_all_turns_finished_and_an_idle_pane() {
    let mut busy = App::new();
    busy.seed_history_entry(history_message(Message::user("question")));
    busy.select_for_test(cursor(0, 0));
    assert!(busy.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    assert_eq!(ctrl_e(&mut busy), None);
    assert!(!busy.is_recalling());

    let mut executing = App::new();
    executing.seed_history_entry(history_message(Message::user("question")));
    executing.select_for_test(cursor(0, 0));
    apply_turn_event(
        &mut executing,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                json!({}),
            )]),
        },
    );
    assert!(executing.has_executing_tool_calls());
    assert_eq!(ctrl_e(&mut executing), None);
    assert!(!executing.is_recalling());

    let mut inspect_only = App::subtask_inspect("test");
    inspect_only.seed_history_entry(history_message(Message::user("question")));
    inspect_only.select_for_test(cursor(0, 0));
    assert_eq!(ctrl_e(&mut inspect_only), None);
    assert!(!inspect_only.is_recalling());
    assert!(inspect_only.input().is_empty());
}

#[test]
fn ctrl_enter_submits_the_revision_and_keeps_the_pane_until_the_engine_confirms() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(history_message(Message::assistant("first answer")));
    app.seed_history_entry(history_message(Message::user("second")));
    app.seed_history_entry(history_message(Message::assistant("second answer")));
    app.select_for_test(cursor(2, 0));
    ctrl_e(&mut app);
    for ch in " revised".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }

    let action = app.handle_event(ctrl_enter());
    assert_eq!(
        action,
        Some(UiAction::EditTranscript(TranscriptEdit {
            // "second" is the pane's second user row, so prompt #1.
            target: TranscriptEditTarget::PromptOrdinal(1),
            replacement: TranscriptEditReplacement::Message {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second revised".into(),
                mode: SessionMode::Build,
            },
        }))
    );
    // Nothing leaves the pane yet: the engine may still reject the edit.
    assert_eq!(app.history().len(), 4);
    assert!(!app.is_recalling());
    assert!(app.is_awaiting_edit_acceptance());
    assert!(app.is_busy());
    assert!(app.interaction().is_normal());
    assert!(!cursor_visible_after_render(&mut app, 80, 20));
    assert!(app.input().is_empty());
    assert_eq!(app.selection(), None);
    assert!(app.view_follow());
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Build));

    // The engine's TurnStarted confirms it: the target and everything after
    // it go, and the revision lands as the newest turn.
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("second revised"),
            mode: SessionMode::Build,
        },
    );
    assert!(!app.is_awaiting_edit_acceptance());
    assert!(app.interaction().is_normal());
    assert_eq!(app.history().len(), 3);
    assert!(
        app.history()
            .last()
            .is_some_and(|entry| conversation_has_role(entry, PresentationRole::User))
    );
}

#[test]
fn edited_pre_turn_compaction_moves_its_marker_before_the_revised_prompt() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(history_message(Message::assistant("first answer")));
    app.seed_history_entry(history_message(Message::user("second")));
    app.seed_history_entry(history_message(Message::assistant("second answer")));
    app.select_for_test(cursor(2, 0));
    ctrl_e(&mut app);
    app.handle_event(key(KeyCode::Char('!')));
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(_))
    ));

    let turn_id = TurnId::new(77);
    app.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id,
        trigger: CompactionTrigger::AutomaticPreTurn,
    });
    app.reduce_without_effects(SessionEvent::CompactionCompleted {
        turn_id,
        trigger: CompactionTrigger::AutomaticPreTurn,
        backend: CompactionBackend::LocalSummary,
    });
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id,
        message: Message::user("second!"),
        mode: SessionMode::Build,
    });

    assert_eq!(app.history().len(), 4);
    assert!(matches!(&app.history()[2], HistoryEntry::CompactionDivider));
    assert!(conversation_has_role(
        &app.history()[3],
        PresentationRole::User
    ));
    assert!(app.is_busy());
    assert_eq!(app.active_turn_id(), Some(turn_id));
}

#[test]
fn prompt_ordinals_skip_rows_that_are_not_user_prompts() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(history_message(Message::assistant("answer")));
    app.seed_history_entry(HistoryEntry::Error("boom".to_string()));
    app.seed_history_entry(history_message(Message::user("second")));
    app.select_for_test(cursor(3, 0));
    ctrl_e(&mut app);

    assert_eq!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            // The assistant reply and the error notice are not prompts.
            target: TranscriptEditTarget::PromptOrdinal(1),
            replacement: TranscriptEditReplacement::Message {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second".into(),
                mode: SessionMode::Build,
            },
        }))
    );
}

#[test]
fn a_rejected_edit_leaves_the_pane_intact() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(history_message(Message::assistant("first answer")));
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.handle_event(key(KeyCode::Char('!')));
    app.handle_event(ctrl_enter());
    assert!(app.is_awaiting_edit_acceptance());

    apply_turn_event(
        &mut app,
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "the edited prompt #0 is not in the session history".to_string(),
        },
    );

    assert!(!app.is_awaiting_edit_acceptance());
    assert!(!app.is_busy());
    // Both original turns survive; only the error notice is added.
    assert_eq!(app.history().len(), 3);
    assert!(matches!(app.history().last(), Some(HistoryEntry::Error(_))));
}

#[test]
fn a_confirmed_edit_rerenders_the_replaced_prompt() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(history_message(Message::assistant("first answer")));
    app.seed_history_entry(history_message(Message::user("original question")));
    app.seed_history_entry(history_message(Message::assistant("second answer")));

    // Select the prompt and draw, exactly as a user reaching for Ctrl+E
    // does: both frames populate the layout cache for that row.
    app.select_for_test(cursor(2, 0));
    let _ = rendered_text(&mut app, 100, 20);
    ctrl_e(&mut app);
    let _ = rendered_text(&mut app, 100, 20);

    for ch in " revised".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    app.handle_event(ctrl_enter());
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("original question revised"),
            mode: SessionMode::Build,
        },
    );

    // The revision occupies the row the replaced prompt held, so the cached
    // rendering of that row must not survive the truncation.
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(
        rendered.contains("original question revised"),
        "the revision is missing from the pane:\n{rendered}"
    );
    assert!(
        !rendered.contains("second answer"),
        "the replaced turn is still drawn:\n{rendered}"
    );
}

#[test]
fn escape_cancels_the_edit_restoring_the_draft_and_mode() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("question")));
    app.seed_history_entry(history_message(Message::assistant("answer")));
    app.set_input_for_test("my draft", "my draft".len());
    enter_insert(&mut app);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.handle_event(key(KeyCode::Char('!')));
    assert_eq!(app.input(), "question!");
    let now = Instant::now();

    assert_eq!(app.handle_event_at(key(KeyCode::Esc), now), None);
    assert_eq!(app.input(), "my draft", "the prior draft is restored");
    assert!(
        !app.interaction().is_insert(),
        "selection is the sole focus state, so recall cancellation returns to Normal"
    );
    assert!(!app.is_recalling());
    assert_eq!(app.selection(), None);

    // The cancel press never counts toward the double-Esc select sequence.
    rendered_text(&mut app, 80, 12);
    app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(100));
    assert_eq!(
        app.selection(),
        None,
        "one fresh Esc only arms the sequence"
    );
    app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(200));
    assert_eq!(
        app.selection(),
        cursor(1, 0),
        "two fresh Esces select normally"
    );
}

#[test]
fn a_sigil_prefixed_recall_reclassifies_and_palette_escape_restores_the_draft() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("/usr")));
    app.seed_history_entry(history_message(Message::assistant("answer")));
    app.set_input_for_test("my draft", "my draft".len());
    enter_insert(&mut app);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);

    // The transcript does not retain whether a leading-space escape was used,
    // so a stored sigil-prefixed row is classified again when recalled.
    assert!(app.command_menu_active());
    let rendered = rendered_text(&mut app, 100, 8);
    assert!(rendered.contains("Command"));

    // Palette Esc cancels the complete recall and restores the saved draft.
    assert_eq!(app.handle_event(key(KeyCode::Esc)), None);
    assert!(!app.is_recalling());
    assert_eq!(app.input(), "my draft");
}

#[test]
fn selection_title_advertises_ctrl_e_only_for_editable_user_text() {
    let mut editable = App::new();
    editable.seed_history_entry(history_message(Message::user("question")));
    editable.select_for_test(cursor(0, 0));
    assert!(rendered_text(&mut editable, 100, 20).contains("Ctrl+E edit"));

    let mut assistant = App::new();
    assistant.seed_history_entry(history_message(Message::assistant("answer")));
    assistant.select_for_test(cursor(0, 0));
    assert!(!rendered_text(&mut assistant, 100, 20).contains("Ctrl+E edit"));

    let mut busy = App::new();
    busy.seed_history_entry(history_message(Message::user("question")));
    busy.select_for_test(cursor(0, 0));
    assert!(busy.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    assert!(!rendered_text(&mut busy, 100, 20).contains("Ctrl+E edit"));
}

#[test]
fn editing_title_marks_an_active_recall() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("question")));
    app.seed_history_entry(history_message(Message::assistant("answer")));
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);

    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("recalling"));
    assert!(rendered.contains("Esc cancel"));
}

#[test]
fn yank_copies_only_the_selected_content_item() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("a question")));
    let long_arguments = json!({ "city": "Paris", "detail": "x".repeat(300) });
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new_or_mint("call_1"),
                ToolFunction::new("get_weather".to_string(), long_arguments.clone()),
            )),
            AssistantContent::Text(Text::new("# Answer\n\ndone")),
        ],
    }));
    app.seed_history_entry(HistoryEntry::Error("network down".to_string()));
    app.seed_history_entry(history_message(Message::System {
        content: "system guidance".to_string(),
    }));

    app.select_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "a question".to_string()
        })
    );

    // Tool calls copy only their argument payload. With no retained result,
    // the second y does not replace the clipboard contents.
    app.select_for_test(cursor(1, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: long_arguments.to_string()
        })
    );
    assert_eq!(app.handle_event(key(KeyCode::Char('y'))), None);

    app.select_for_test(cursor(1, 1));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "# Answer\n\ndone".to_string()
        })
    );

    app.select_for_test(cursor(2, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "network down".to_string()
        })
    );

    app.select_for_test(cursor(3, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "system guidance".to_string()
        })
    );
}

#[test]
fn tool_yank_preserves_raw_malformed_arguments() {
    let mut app = App::new();
    app.seed_history_entry(history_message(assistant_message(vec![tool_call(
        "call_1",
        None,
        "command",
        serde_json::Value::String("{".to_string()),
    )])));
    app.select_for_test(cursor(0, 0));

    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "{".to_string()
        })
    );
}

#[test]
fn non_y_keys_cancel_a_pending_tool_output_yank() {
    let arguments = json!({ "command": "printf hi" });
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call("call_1", None, "command", arguments.clone()),
                AssistantContent::text("after tool"),
            ]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("call_1", None, "command", "tool output"),
        },
    );
    app.select_for_test(cursor(0, 0));

    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        })
    );
    assert_eq!(app.handle_event(key(KeyCode::Char('x'))), None);
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        }),
        "an unrelated key should cancel the pending output yank"
    );

    app.handle_event(key(KeyCode::Down));
    app.handle_event(key(KeyCode::Up));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        }),
        "navigation should also cancel the pending output yank"
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "tool output".to_string()
        })
    );
}
