//! Hints behavior and presentation tests.
use super::*;

#[test]
fn navigation_hints_keep_shortcuts_and_essential_narrow_fallbacks_readable() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("prompt")));
    let wide = context_help(&app);
    for hint in [
        "PgUp/Ctrl+B page up",
        "Ctrl+U half page up",
        "gg start",
        "G bottom",
        "zm fold turns",
        "zR unfold all",
        "v select",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
    let medium = rendered_text(&mut app, 80, 20);
    for hint in ["i input", "v select", "Shift+Tab Plan", "? help"] {
        assert!(medium.contains(hint), "missing {hint}: {medium}");
    }
    let narrow = rendered_text(&mut app, 36, 20);
    assert!(narrow.contains("i input"));
    assert!(narrow.contains("? help"));

    app.select_message_for_test(cursor(0, 0));
    let wide = context_help(&app);
    for hint in [
        "j/↓ next message",
        "Ctrl+U previous user",
        "Enter blocks",
        "y copy message",
        "za toggle fold",
        "zm fold turns",
        "zR unfold all",
        "Ctrl+E edit",
        "Esc exit",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
    assert!(!wide.contains("yy output"));
    assert!(
        !wide.contains("Ctrl+B"),
        "Select has no control page shortcuts"
    );
    let medium = rendered_text(&mut app, 100, 20);
    for hint in ["Enter blocks", "y copy", "Ctrl+E edit", "? help"] {
        assert!(medium.contains(hint), "missing {hint}: {medium}");
    }
    let narrow = rendered_text(&mut app, 36, 20);
    assert!(narrow.contains("y copy message · ? help"));

    app.handle_event(key(KeyCode::Enter));
    let wide = context_help(&app);
    for hint in [
        "j/↓ next block",
        "y copy/params",
        "yy output",
        "za toggle fold",
        "zm fold turns",
        "zR unfold all",
        "Enter inspect child",
        "Esc back",
        "Ctrl+E edit",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
    let medium = rendered_text(&mut app, 100, 20);
    for hint in [
        "y copy/params",
        "Enter inspect child",
        "Ctrl+E edit",
        "? help",
    ] {
        assert!(medium.contains(hint), "missing {hint}: {medium}");
    }
    let narrow = rendered_text(&mut app, 36, 20);
    assert!(narrow.contains("y copy/params · ? help"));

    let mut inspect = App::acp_inspect("ACP inspect");
    inspect.seed_history_entry(history_message(Message::user("prompt")));
    let wide = context_help(&inspect);
    for hint in [
        "PgUp/Ctrl+B page up",
        "Ctrl+U half page up",
        "Ctrl+O root",
        "d diagnostics",
        "zm fold turns",
        "zR unfold all",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
    let compact = rendered_text(&mut inspect, 120, 20);
    assert!(compact.contains("ACP inspect"));
    inspect.select_for_test(cursor(0, 0));
    let wide = context_help(&inspect);
    for hint in [
        "Ctrl+U previous user",
        "Enter inspect child",
        "za toggle fold",
        "zm fold turns",
        "zR unfold all",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
    let compact = rendered_text(&mut inspect, 100, 20);
    assert!(compact.contains("ACP inspect"));
    inspect.handle_event(key(KeyCode::Esc));
    let wide = context_help(&inspect);
    for hint in [
        "j/↓ next message",
        "Enter blocks",
        "y copy message",
        "zm fold turns",
        "zR unfold all",
        "Esc exit",
    ] {
        assert!(wide.contains(hint), "missing {hint}: {wide}");
    }
}

#[test]
fn fold_hints_do_not_advertise_removed_commands_at_any_width() {
    for mut app in [App::new(), App::acp_inspect("ACP inspect")] {
        app.seed_history_entry(history_message(Message::user("prompt")));
        for scope in [
            None,
            Some(SelectionScope::Message),
            Some(SelectionScope::Block),
        ] {
            match scope {
                None => app.select_for_test(None),
                Some(SelectionScope::Message) => app.select_message_for_test(cursor(0, 0)),
                Some(SelectionScope::Block) => app.select_for_test(cursor(0, 0)),
            }
            for width in [36, 80, 100, 120, 180, 240, 280] {
                let text = rendered_text(&mut app, width, 20);
                for removed in [
                    "zr",
                    "zt",
                    "zl",
                    "zL",
                    "non-text",
                    "zM/zR all",
                    "fold/unfold all",
                ] {
                    assert!(
                        !text.contains(removed),
                        "advertised {removed} at width {width}: {text}"
                    );
                }
            }
        }
    }
}

#[test]
fn selection_help_mentions_tool_yank_bindings() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("hi")));
    app.select_for_test(cursor(0, 0));

    let rendered = context_help(&app);

    assert!(rendered.contains("y copy/params"));
    assert!(rendered.contains("yy output"));
}
