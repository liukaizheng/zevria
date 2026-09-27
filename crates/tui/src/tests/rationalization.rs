//! Cross-surface contracts for shared keys, hints, help, and modal chrome.
use super::*;

#[test]
fn normal_help_is_scrollable_and_esc_q_question_mark_close_without_editing() {
    for close in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('?')] {
        let mut app = App::new();
        app.set_input_for_test("retained draft", 14);
        assert_eq!(app.handle_event(key(KeyCode::Char('?'))), None);
        assert_eq!(app.surface().id.kind, crate::input::SurfaceKind::Help);
        let first = rendered_text(&mut app, 60, 24);
        assert!(first.contains("Normal help"), "{first}");
        assert!(first.contains("i input"));
        app.handle_event(key(KeyCode::End));
        let last = rendered_text(&mut app, 60, 24);
        assert!(last.contains("zR unfold all"), "{last}");
        assert_ne!(first, last);
        app.handle_event(Event::Paste("not a draft edit".into()));
        assert_eq!(app.input(), "retained draft");
        assert!(!cursor_visible_after_render(&mut app, 60, 24));
        assert_eq!(app.handle_event(key(close)), None);
        assert_eq!(app.surface().id.kind, crate::input::SurfaceKind::Transcript);
        assert_eq!(app.input(), "retained draft");
    }
    let mut app = App::new();
    enter_insert(&mut app);
    app.handle_event(key(KeyCode::Char('?')));
    assert_eq!(app.input(), "?");
    assert_eq!(app.surface().id.kind, crate::input::SurfaceKind::Composer);
}

#[test]
fn picker_help_suspends_selection_and_preserves_workspace_footer_at_sixty_columns() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(
        (0..30)
            .map(|index| session_summary(&format!("row-{index:02}"), Some("session")))
            .collect(),
    );
    rendered_views_text(&mut views, 60, 24);
    views.handle_event(key(KeyCode::Char('G')));
    views.handle_event(key(KeyCode::Char('k')));
    views.handle_event(key(KeyCode::Char('?')));
    let rows = rendered_views_rows(&mut views, 60, 24);
    assert!(rows.concat().contains("Sessions help"));
    assert!(rows.last().unwrap().contains("Build"));
    views.handle_event(key(KeyCode::PageDown));
    views.handle_event(key(KeyCode::Char('q')));
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResumeSession {
            path: PathBuf::from("/tmp/sessions/row-28.jsonl")
        })
    );
}

#[test]
fn picker_vim_pages_and_half_pages_use_the_last_rendered_height() {
    let mut views = test_session_views(App::new());
    views.open_session_picker(
        (0..30)
            .map(|index| session_summary(&format!("row-{index:02}"), None))
            .collect(),
    );
    rendered_views_text(&mut views, 60, 24); // 14-row modal, 12 content rows.
    for event in [
        key(KeyCode::Char('G')),
        key(KeyCode::Char('g')),
        modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL),
        modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL),
        key(KeyCode::Char('j')),
    ] {
        assert_eq!(views.handle_event(event), None);
    }
    assert_eq!(
        views.handle_event(key(KeyCode::Enter)),
        Some(UiAction::ResumeSession {
            path: PathBuf::from("/tmp/sessions/row-07.jsonl")
        })
    );
    views.open_session_picker(vec![]);
    assert_eq!(views.handle_event(key(KeyCode::Char('q'))), None);
    assert_eq!(
        views.handle_event(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(UiAction::Quit)
    );
}

#[test]
fn skills_help_and_filter_do_not_leak_keys_into_the_composer() {
    let mut views = test_session_views(App::new());
    views.overlays.show_skills();
    views.handle_event(key(KeyCode::Char('?')));
    let help = rendered_views_text(&mut views, 60, 24);
    assert!(help.contains("Skills help"));
    views.handle_event(key(KeyCode::Char('?')));
    views.handle_event(key(KeyCode::Char('/')));
    for ch in "jkq?".chars() {
        views.handle_event(key(KeyCode::Char(ch)));
    }
    let filter = rendered_views_text(&mut views, 60, 24);
    assert!(filter.contains("Filter: jkq?"), "{filter}");
    assert!(!filter.contains("Skills help"));
    assert!(views.root().input().is_empty());
    views.handle_event(key(KeyCode::Esc));
    views.handle_event(key(KeyCode::Char('q')));
    assert!(!views.overlays.skills.is_open());
}

#[test]
fn plan_help_never_confirms_a_choice_and_returns_to_the_same_decision() {
    let mut app = App::new();
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    });
    app.handle_event(key(KeyCode::Char('1')));
    let choice = app.plan_choice();
    app.handle_event(key(KeyCode::Char('?')));
    let help = rendered_text(&mut app, 60, 24);
    assert!(help.contains("Plan decision help"));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.handle_event(key(KeyCode::Char('?'))), None);
    assert_eq!(app.plan_choice(), choice);
    assert!(!app.is_busy());
    assert_eq!(app.surface().id.kind, crate::input::SurfaceKind::PlanReview);
}

#[test]
fn pane_help_survives_transfer_into_workspace_ownership() {
    let mut app = App::new();
    app.handle_event(key(KeyCode::Char('?')));
    let mut views = test_session_views(app);
    assert!(rendered_views_text(&mut views, 60, 24).contains("Normal help"));
    views.handle_event(key(KeyCode::Char('?')));
    assert!(!rendered_views_text(&mut views, 60, 24).contains("Normal help"));
    assert_eq!(
        views.root().surface().id.kind,
        crate::input::SurfaceKind::Transcript
    );
}
