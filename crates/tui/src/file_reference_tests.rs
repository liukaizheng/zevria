use super::*;
use crate::{
    completion::FileSearchStatus,
    workspace_files::{SearchRequest, SearchResult},
};
use zevria_content::UserPrompt;

fn fixture(text: &str) -> SessionViews {
    let mut views = test_session_views(App::new());
    views.handle_event(key(KeyCode::Char('i')));
    views.handle_event(Event::Paste(text.into()));
    views
}

fn response(request: &SearchRequest, paths: &[&str]) -> SearchResult {
    SearchResult {
        request: request.clone(),
        paths: paths.iter().map(|path| (*path).into()).collect(),
        status: FileSearchStatus::default(),
    }
}

#[test]
fn relative_workspace_is_captured_at_construction_not_at_search_time() {
    let views = SessionViews::new(App::new(), PathBuf::from("."));
    assert!(views.startup_workspace.is_absolute());
    assert_eq!(views.startup_workspace, std::env::current_dir().unwrap());
}

#[test]
fn file_escape_dismisses_only_the_popup_during_recall_and_completion_stays_text() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("Explain @a")));
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    let mut views = test_session_views(app);
    let first = views.file_search_request().unwrap();
    views.handle_event(key(KeyCode::Esc));
    assert!(views.root.is_recalling());
    assert_eq!(views.root.input(), "Explain @a");
    assert!(!views.file_search_completed(response(&first, &["late"])));
    views.handle_event(key(KeyCode::Left));
    let next = views.file_search_request().unwrap();
    views.file_search_completed(response(&next, &["app.rs"]));
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert!(views.root.is_recalling());
    assert_eq!(views.root.input(), "Explain @app.rs ");
    let Some(UiAction::EditTranscript(edit)) = views.handle_event(ctrl_enter()) else {
        panic!("recalled prompt edit");
    };
    assert!(
        matches!(edit.replacement, zevria_session_api::TranscriptEditReplacement::Message { text, .. } if text == UserPrompt::from_text("Explain @app.rs"))
    );
}

#[test]
fn pending_clipboard_owns_delivery_and_retires_file_responses_without_changing_ack_identity() {
    let mut views = fixture("@a");
    let request = views.file_search_request().unwrap();
    views.file_search_completed(response(&request, &["a.rs"]));
    let Some(UiAction::ReadClipboard { generation, cursor }) =
        views.handle_event(modified_key(KeyCode::Char('v'), KeyModifiers::CONTROL))
    else {
        panic!("clipboard read");
    };
    let origin = views.clipboard_origin().unwrap();
    assert!(views.file_search_request().is_none());
    assert!(!views.file_search_completed(response(&request, &["late"])));
    views.clipboard_completed(
        origin,
        generation,
        cursor,
        crate::clipboard::ClipboardResult::Text("b".into()),
    );
    assert_eq!(views.root.input(), "@ab");
    assert!(views.file_search_request().is_some());
}

#[test]
fn shifted_at_activates_and_repeat_release_events_do_not_type_or_accept() {
    let mut views = fixture("Explain ");
    views.handle_event(modified_key(KeyCode::Char('@'), KeyModifiers::SHIFT));
    let request = views.file_search_request().unwrap();
    assert_eq!(request.completion.identity.query.prefix, "");
    assert!(views.file_search_completed(response(&request, &["app.rs"])));
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        for code in [KeyCode::Char('@'), KeyCode::Enter, KeyCode::Tab] {
            assert_eq!(
                views.handle_event(Event::Key(KeyEvent::new_with_kind(
                    code,
                    KeyModifiers::NONE,
                    kind
                ))),
                None
            );
            assert_eq!(views.root.input(), "Explain @");
        }
    }
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.root.input(), "Explain @app.rs ");
    assert!(views.root.history().is_empty());
}

#[test]
fn enter_tab_and_ctrl_i_insert_only_and_ctrl_enter_submits_exact_literal_text() {
    for accept in [
        key(KeyCode::Enter),
        key(KeyCode::Tab),
        modified_key(KeyCode::Char('i'), KeyModifiers::CONTROL),
    ] {
        let mut views = fixture("Explain @app suffix");
        for _ in 0.." suffix".len() {
            views.handle_event(key(KeyCode::Left));
        }
        let request = views.file_search_request().unwrap();
        assert!(views.file_search_completed(response(&request, &["docs/my file.md", "other.rs"])));
        assert_eq!(views.handle_event(accept), None);
        assert_eq!(views.root.input(), "Explain @\"docs/my file.md\" suffix");
        assert!(!views.root.is_busy());
        assert!(views.root.history().is_empty());
        assert_eq!(
            views.handle_event(ctrl_enter()),
            Some(UiAction::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: UserPrompt::from_text("Explain @\"docs/my file.md\" suffix"),
                mode: SessionMode::Build,
            })
        );
        assert!(!views.file_search_completed(response(&request, &["late path"])));
    }
    let mut unresolved = fixture("Explain @app");
    let request = unresolved.file_search_request().unwrap();
    unresolved.file_search_completed(response(&request, &["must-not-be-inserted.rs"]));
    assert_eq!(
        unresolved.handle_event(ctrl_enter()),
        Some(UiAction::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: UserPrompt::from_text("Explain @app"),
            mode: SessionMode::Build,
        })
    );
}

#[test]
fn loading_empty_and_unavailable_rows_are_inert_and_diagnostics_never_become_history() {
    for status in [
        FileSearchStatus {
            loading: true,
            ..Default::default()
        },
        FileSearchStatus::default(),
        FileSearchStatus {
            unavailable: true,
            ..Default::default()
        },
        FileSearchStatus {
            incomplete: true,
            omitted: 3,
            errors: 2,
            ..Default::default()
        },
    ] {
        let mut views = fixture("@none");
        let request = views.file_search_request().unwrap();
        views.file_search_completed(SearchResult {
            status,
            ..response(&request, &[])
        });
        for event in [
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            modified_key(KeyCode::Char('i'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(views.handle_event(event), None);
            assert_eq!(views.root.input(), "@none");
        }
        assert!(views.root.history().is_empty());
        assert_eq!(
            views.handle_event(ctrl_enter()),
            Some(UiAction::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "@none".into(),
                mode: SessionMode::Build
            })
        );
    }
}

#[test]
fn late_results_cannot_reopen_after_escape_clear_undo_or_restore_and_queries_coalesce() {
    let mut views = fixture("@a");
    let first = views.file_search_request().unwrap();
    views.handle_event(key(KeyCode::Char('b')));
    let second = views.file_search_request().unwrap();
    assert_eq!(first.completion.activation, second.completion.activation);
    assert_ne!(first.completion.request, second.completion.request);
    assert!(!views.file_search_completed(response(&first, &["stale"])));
    assert!(views.file_search_completed(response(&second, &["abc.rs"])));
    views.handle_event(key(KeyCode::Esc));
    assert_eq!(views.root.input(), "@ab");
    assert!(views.root.interaction().is_insert());
    assert!(!views.root.command_menu_active());
    assert!(!views.file_search_completed(response(&second, &["stale"])));
    views.handle_event(key(KeyCode::Left));
    let moved = views.file_search_request().unwrap();
    assert_ne!(moved.completion.activation, second.completion.activation);
    views.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(views.root.input(), "");
    assert!(!views.file_search_completed(response(&moved, &["stale"])));
    views.handle_event(modified_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
    let undone = views.file_search_request().unwrap();
    assert_eq!(views.root.input(), "@ab");
    assert_ne!(
        undone.completion.identity.generation,
        moved.completion.identity.generation
    );
    assert!(!views.file_search_completed(response(&moved, &["stale"])));
    views.root.set_input_for_test("@restored", 9);
    assert!(!views.file_search_completed(response(&undone, &["stale"])));
}

#[test]
fn overlays_and_session_replacement_invalidate_requests_without_stealing_focus() {
    let mut views = fixture("@a");
    let first = views.file_search_request().unwrap();
    views.open_session_picker(Vec::new());
    assert!(!views.file_search_completed(response(&first, &["stale"])));
    views.handle_event(key(KeyCode::Enter));
    assert_eq!(views.root.input(), "@a");
    views.handle_event(key(KeyCode::Esc));
    let reopened = views.file_search_request().unwrap();
    assert_ne!(reopened.completion.activation, first.completion.activation);
    assert!(!views.file_search_completed(response(&first, &["stale"])));
    assert!(views.file_search_completed(response(&reopened, &["active.rs"])));
    let mut replacement = fixture("@a");
    assert!(!replacement.file_search_completed(response(&reopened, &["wrong-session"])));
    assert_eq!(replacement.root.input(), "@a");
}

#[test]
fn navigation_clamps_and_refresh_preserves_highlighted_path_and_remeasures() {
    let mut views = fixture("@");
    let request = views.file_search_request().unwrap();
    views.file_search_completed(response(&request, &["a.rs", "b.rs", "c.rs"]));
    rendered_views_text(&mut views, 100, 24);
    assert!(views.root.render_parts().view.completion_page_rows() > 1);
    views.handle_event(key(KeyCode::End));
    assert_eq!(views.root.command_menu_selection(), 2);
    views.handle_event(key(KeyCode::Down));
    assert_eq!(views.root.command_menu_selection(), 2);
    views.file_search_completed(response(&request, &["c.rs", "a.rs"]));
    assert_eq!(views.root.command_menu_selection(), 0);
    assert_eq!(views.root.render_parts().view.completion_page_rows(), 1);
    views.handle_event(key(KeyCode::PageDown));
    assert_eq!(views.root.command_menu_selection(), 1);
    views.handle_event(key(KeyCode::Home));
    views.handle_event(key(KeyCode::Up));
    assert_eq!(views.root.command_menu_selection(), 0);
    views.handle_event(key(KeyCode::Enter));
    assert_eq!(views.root.input(), "@c.rs ");
}

#[test]
fn files_popup_hints_status_and_tiny_terminal_rendering_are_safe_and_pure() {
    let mut views = fixture("Explain @a");
    let request = views.file_search_request().unwrap();
    let loading = rendered_views_text(&mut views, 100, 24);
    assert!(loading.contains("Files") && loading.contains("Loading workspace files"));
    let paths = (0..50).map(|n| format!("dir{n:02}/app.rs")).collect();
    views.file_search_completed(SearchResult {
        request: request.clone(),
        paths,
        status: FileSearchStatus {
            incomplete: true,
            errors: 2,
            ..Default::default()
        },
    });
    for (width, height) in [(100, 24), (40, 24), (30, 24), (40, 16), (30, 10)] {
        let text = rendered_views_text(&mut views, width, height);
        assert!(text.contains("Files"), "{text}");
        assert!(text.contains("Partial index"), "{text}");
        if height >= 24 {
            assert!(text.contains("Enter complete"), "{text}");
        }
        assert!(!text.contains("Enter accept/run"), "{text}");
    }
    for width in 0..8 {
        for height in 0..8 {
            rendered_views_text(&mut views, width, height);
        }
    }
    assert_eq!(views.file_search_request(), Some(request));
    assert_eq!(views.root.input(), "Explain @a");
    assert!(views.root.history().is_empty());
}

#[test]
fn file_completion_inside_skill_and_ensemble_arguments_never_dispatches_on_accept() {
    for (text, expected) in [
        ("$review @a", "$review @app.rs "),
        ("/ensemble-plan @a", "/ensemble-plan @app.rs "),
    ] {
        let mut app = app_with_skills();
        enter_insert(&mut app);
        let mut views = test_session_views(app);
        views.handle_event(Event::Paste(text.into()));
        let request = views.file_search_request().unwrap();
        views.file_search_completed(response(&request, &["app.rs"]));
        assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(views.root.input(), expected);
        assert!(!views.root.is_busy());
    }
}
