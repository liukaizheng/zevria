use super::*;
use ratatui::crossterm::event::KeyEvent;
use ratatui::{Terminal, backend::TestBackend};
use zevria_session_api::ModeSelectionResult;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn control(character: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        assert_eq!(app.handle_event(key(KeyCode::Char(character))), None);
    }
}

fn editable() -> App {
    let mut app = App::new();
    app.handle_event(key(KeyCode::Char('i')));
    app
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
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
fn exact_press_shortcuts_work_with_empty_drafts_and_open_menus() {
    let mut app = editable();
    for (undo, redo) in [('z', 'y'), ('Z', 'Y')] {
        type_text(&mut app, "/com");
        assert!(app.command_menu_active());
        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            assert_eq!(
                app.handle_event(Event::Key(KeyEvent::new(KeyCode::Char(undo), modifiers))),
                None
            );
            assert_eq!(app.composer.text(), "/com");
        }
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            let mut event = KeyEvent::new(KeyCode::Char(undo), KeyModifiers::CONTROL);
            event.kind = kind;
            app.handle_event(Event::Key(event));
            assert_eq!(app.composer.text(), "/com");
        }
        assert_eq!(app.handle_event(control(undo)), None);
        assert!(app.composer.is_empty());
        assert!(!app.command_menu_active());
        assert_eq!(app.handle_event(control(redo)), None);
        assert_eq!(app.composer.text(), "/com");
        assert!(app.command_menu_active());
        app.composer.clear();
    }
    for character in ['z', 'y'] {
        assert_eq!(app.handle_event(control(character)), None);
        assert!(app.composer.is_empty());
    }
    assert!(app.history().is_empty());
    assert!(!app.session.is_busy());
}

#[test]
fn paste_completion_dismissal_and_user_clear_are_individual_transactions() {
    let mut app = editable();
    type_text(&mut app, "/com");
    app.handle_event(key(KeyCode::Tab));
    assert_eq!(app.composer.text(), "/compact ");
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "/com");
    app.handle_event(key(KeyCode::Esc));
    assert!(app.composer.is_empty());
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "/com");
    app.handle_event(Event::Paste("\nfirst\nsecond".into()));
    assert_eq!(app.composer.text(), "/com\nfirst\nsecond");
    assert_eq!(app.handle_event(control('c')), None);
    assert!(app.composer.is_empty());
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "/com\nfirst\nsecond");
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "/com");
    assert!(app.command_menu_active());
    assert!(!app.session.is_busy());
}

#[test]
fn enter_completion_is_atomic_unless_submission_consumes_the_draft() {
    let mut app = editable();
    app.composer.replace("/ne\u{2003}雪\nnext".into(), 3);
    let original = app.composer.snapshot();
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.composer.text(), "/new\u{2003}雪\nnext");
    assert_eq!(app.composer.cursor(), "/new\u{2003}".len());
    assert_eq!(app.handle_event(control('z')), None);
    assert_eq!(app.composer.text(), "/ne\u{2003}雪\nnext");
    assert!(
        !app.composer.matches_draft(&original),
        "undo does not revive acknowledgement identity"
    );
    assert_eq!(app.composer.cursor(), 3);
    assert_eq!(app.handle_event(control('y')), None);
    assert_eq!(app.composer.text(), "/new\u{2003}雪\nnext");
    assert!(app.history().is_empty());
    assert!(!app.session.is_busy());

    let mut submitted = editable();
    type_text(&mut submitted, "/ne");
    assert_eq!(
        submitted.handle_event(key(KeyCode::Enter)),
        Some(UiAction::RunCommand(SlashCommand::New))
    );
    assert!(submitted.composer.is_empty());
    for character in ['z', 'y'] {
        assert_eq!(submitted.handle_event(control(character)), None);
        assert!(
            submitted.composer.is_empty(),
            "submitted commands cannot be resurrected"
        );
    }
}

#[test]
fn focus_and_navigation_close_typing_runs_without_losing_history() {
    for navigation in [
        key(KeyCode::Left),
        key(KeyCode::Right),
        key(KeyCode::PageUp),
        key(KeyCode::Home),
        Event::FocusLost,
        Event::FocusGained,
    ] {
        let mut app = editable();
        type_text(&mut app, "one");
        app.handle_event(navigation);
        type_text(&mut app, "two");
        app.handle_event(control('z'));
        assert_eq!(app.composer.text(), "one");
        app.handle_event(control('z'));
        assert!(app.composer.is_empty());
    }
    let mut app = editable();
    type_text(&mut app, "one");
    app.handle_event(key(KeyCode::Esc));
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "one");
    app.handle_event(key(KeyCode::Char('i')));
    type_text(&mut app, "two");
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "one");
}

#[test]
fn noneditable_composers_keep_content_and_redo_unchanged() {
    for gate in [0, 2, 3, 4] {
        let mut app = editable();
        type_text(&mut app, "one");
        app.handle_event(Event::Paste("two".into()));
        app.handle_event(control('z'));
        match gate {
            0 => app.interaction.enter_normal(),
            2 => app.composer.begin_paste(),
            3 => app.interaction.enter_selection(Selection {
                history_index: 0,
                content_index: 0,
            }),
            4 => app.pane = PaneState::acp_inspect("locked"),
            _ => unreachable!(),
        }
        let draft = app.composer.snapshot();
        for character in ['z', 'y'] {
            app.handle_event(control(character));
            assert_eq!(app.composer.snapshot(), draft, "gate {gate}");
            assert_eq!(app.composer.is_paste_pending(), gate == 2, "gate {gate}");
        }
        assert!(app.composer.redo());
        assert_eq!(app.composer.text(), "onetwo");
    }
}

#[test]
fn recall_is_a_fresh_baseline_and_cancel_restores_original_history() {
    let mut app = editable();
    type_text(&mut app, "saved");
    app.handle_event(Event::Paste(" redo".into()));
    app.handle_event(control('z'));
    app.conversation
        .push_message(Message::user("original"), ToolCallStatus::Finished);
    let selection = Selection {
        history_index: 0,
        content_index: 0,
    };
    app.interaction.enter_selection(selection);
    assert!(app.recall_selected());
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "original");
    type_text(&mut app, " changed");
    app.handle_event(control('z'));
    assert_eq!(app.composer.text(), "original");
    assert_eq!(app.history().len(), 1);
    app.handle_event(key(KeyCode::Esc));
    assert!(!app.edit.is_recalling());
    assert_eq!(app.composer.text(), "saved");
    app.handle_event(key(KeyCode::Char('i')));
    app.handle_event(control('y'));
    assert_eq!(app.composer.text(), "saved redo");
    app.handle_event(control('z'));
    app.handle_event(control('z'));
    assert!(app.composer.is_empty());
    assert_eq!(app.history().len(), 1);
}

#[test]
fn rejected_submissions_restore_history_but_acceptance_and_resets_discard_it() {
    for accepted in [false, true] {
        let mut app = editable();
        type_text(&mut app, "first");
        app.handle_event(Event::Paste(" second".into()));
        assert!(matches!(app.submit(), Some(UiAction::Submit { .. })));
        assert!(app.composer.is_empty());
        let turn_id = TurnId::new(1);
        if accepted {
            app.reduce(SessionEvent::TurnStarted {
                turn_id,
                mode: SessionMode::Build,
                message: Message::user("first second"),
            });
            app.reduce(SessionEvent::TurnCancelled { turn_id });
        } else {
            app.reduce(SessionEvent::TurnRejected {
                turn_id,
                error: "rejected".into(),
            });
        }
        if !app.interaction.is_insert() {
            app.handle_event(key(KeyCode::Char('i')));
        }
        app.handle_event(control('z'));
        assert_eq!(app.composer.text(), if accepted { "" } else { "first" });
        app.handle_event(control('y'));
        assert_eq!(
            app.composer.text(),
            if accepted { "" } else { "first second" }
        );
        app.restore(vec![]);
        if !app.interaction.is_insert() {
            app.handle_event(key(KeyCode::Char('i')));
        }
        app.handle_event(control('z'));
        app.handle_event(control('y'));
        assert!(app.composer.is_empty());
    }
}

#[test]
fn mode_selection_preserves_history_or_discards_an_accepted_command() {
    for command in [false, true] {
        for accepted in [false, true] {
            let mut app = editable();
            type_text(&mut app, if command { "/plan" } else { "draft" });
            let Some(UiAction::SetMode { request_id, mode }) =
                app.begin_mode_selection(SessionMode::Plan, command)
            else {
                panic!("mode selection")
            };
            let result = if accepted {
                ModeSelectionResult::Accepted {
                    mode,
                    changed: true,
                }
            } else {
                ModeSelectionResult::Rejected {
                    code: "rejected".into(),
                    message: "not saved".into(),
                }
            };
            app.accept_mode_selection(&request_id, result);
            if command && accepted {
                app.handle_event(control('z'));
                assert!(app.composer.is_empty());
                continue;
            }
            type_text(&mut app, "!");
            app.handle_event(control('z'));
            assert_eq!(app.composer.text(), if command { "/plan" } else { "draft" });
            app.handle_event(control('z'));
            assert!(app.composer.is_empty());
        }
    }
}

#[test]
fn multiline_history_recomputes_caret_scroll_and_editable_hints() {
    let mut app = editable();
    let text = "first line\nsecond\nthird\nfourth\nfifth\nsixth\nseventh";
    app.handle_event(Event::Paste(text.into()));
    render(&mut app, 24, 14);
    let before = app.view.composer_viewport().clone();
    assert!(before.visible_range().start() > 0);
    app.handle_event(control('c'));
    render(&mut app, 24, 14);
    assert_eq!(app.view.composer_viewport().visible_range().start(), 0);
    app.handle_event(control('z'));
    render(&mut app, 24, 14);
    assert_eq!(app.composer.cursor(), text.len());
    assert_eq!(
        app.view.composer_viewport().visible_range(),
        before.visible_range()
    );
    let wide = render(&mut app, 180, 20);
    assert!(wide.contains("Ctrl+Z undo"));
    assert!(wide.contains("Ctrl+Y redo"));
    let narrow = render(&mut app, 36, 20);
    assert!(narrow.contains("Ctrl+Enter send"));
    assert!(!narrow.contains("Ctrl+Z"));
    app.handle_event(key(KeyCode::Esc));
    let normal = render(&mut app, 180, 20);
    assert!(!normal.contains("Ctrl+Z undo"));
    assert!(!normal.contains("Ctrl+Y redo"));
}
