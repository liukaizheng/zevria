use super::*;
use crate::{
    completion::FileSearchStatus,
    workspace_files::{SearchRequest, SearchResult},
};
use zevria_content::UserPrompt;

fn captured_startup_workspace() -> PathBuf {
    let current = std::env::current_dir().expect("current test directory");
    let workspace = current
        .ancestors()
        .last()
        .expect("filesystem root")
        .join("captured/startup/workspace");
    assert!(workspace.is_absolute());
    workspace
}

fn live_views() -> (SessionViews, EnsembleStart) {
    let (start, _) = fixture(true);
    let mut views = SessionViews::new(App::new(), captured_startup_workspace());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(1),
        start: start.clone(),
        resumed: false,
    });
    for descriptor in &start.agents {
        let mut state = WorkerReviewState::new(descriptor.clone());
        queue_input(&mut state, WorkerControlId::new(), "initial");
        settle(&mut state);
        publish(&mut views, &start, &state);
    }
    (views, start)
}

fn response(request: &SearchRequest, paths: &[&str]) -> SearchResult {
    SearchResult {
        request: request.clone(),
        paths: paths.iter().map(|s| (*s).into()).collect(),
        status: FileSearchStatus::default(),
    }
}

#[test]
fn root_and_live_worker_searches_are_pane_local_while_work_runs() {
    let (mut views, start) = live_views();
    assert!(views.root.is_busy());
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("root @app".into()));
    let root = views.file_search_request().unwrap();
    views.file_search_completed(response(&root, &["a/app.rs", "b/app.rs"]));
    views.handle_event(key(KeyCode::Down, KeyModifiers::NONE));
    views.open_agent(&start.agents[0].id);
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("worker @app".into()));
    let worker = views.file_search_request().unwrap();
    assert_eq!(root.service, worker.service);
    assert_ne!(root.pane, worker.pane);
    assert_eq!(views.startup_workspace, captured_startup_workspace());
    assert!(!views.file_search_completed(response(&root, &["wrong pane"])));
    assert!(views.file_search_completed(response(&worker, &["docs/my file.md"])));
    assert_eq!(
        views.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
        None
    );
    assert_eq!(views.visible_app().input(), "worker @\"docs/my file.md\" ");
    assert_eq!(views.root.input(), "root @app");
    let Some(UiAction::WorkerControl(control)) =
        views.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("worker feedback");
    };
    assert!(
        matches!(&control.action, WorkerControlAction::SendFeedback { text } if text == &UserPrompt::from_text("worker @\"docs/my file.md\""))
    );
    assert!(!views.file_search_completed(response(&worker, &["after submission"])));
    // Preserve existing acknowledgement identity: suggestions did not count as
    // draft edits, so the accepted worker feedback can still clear its draft.
    views.apply(SessionEvent::WorkerControlResult {
        result: WorkerControlResult {
            control,
            accepted: true,
            detail: "accepted".into(),
        },
    });
    assert!(views.visible_app().input().is_empty());
    views.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let returned = views.file_search_request().unwrap();
    assert_ne!(root.completion.activation, returned.completion.activation);
    assert!(!views.file_search_completed(response(&root, &["old root"])));
    assert!(views.file_search_completed(response(&returned, &["b/app.rs", "a/app.rs"])));
    assert_eq!(
        views.root.command_menu_selection(),
        0,
        "still highlight b/app.rs"
    );
    views.handle_event(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(views.root.input(), "root @b/app.rs ");
}

#[test]
fn worker_ctrl_enter_ignores_highlighted_file_and_retirement_rejects_late_responses() {
    let (mut views, start) = live_views();
    views.open_agent(&start.agents[0].id);
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("@unresolved".into()));
    let worker = views.file_search_request().unwrap();
    views.file_search_completed(response(&worker, &["different.rs"]));
    let Some(UiAction::WorkerControl(control)) =
        views.handle_event(key(KeyCode::Enter, KeyModifiers::CONTROL))
    else {
        panic!("worker feedback");
    };
    assert!(
        matches!(control.action, WorkerControlAction::SendFeedback { text } if text == UserPrompt::from_text("@unresolved"))
    );
    assert!(!views.file_search_completed(response(&worker, &["late"])));
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    let reopened = views.file_search_request().unwrap();
    views.agents[0].app.freeze_worker();
    assert!(views.file_search_request().is_none());
    assert!(!views.file_search_completed(response(&reopened, &["frozen"])));
    views.prune_discarded_agent_panes(vec![start.run_id]);
    assert!(!views.file_search_completed(response(&reopened, &["retired"])));
    assert!(views.visible_agent_id().is_none());
}

#[test]
fn historical_workers_never_request_files_and_overlay_return_reconciles_live_workers() {
    let (mut views, start) = live_views();
    views.open_agent(&start.agents[0].id);
    views.handle_event(key(KeyCode::Char('i'), KeyModifiers::NONE));
    views.handle_event(Event::Paste("@a".into()));
    let request = views.file_search_request().unwrap();
    views.open_session_picker(Vec::new());
    assert!(!views.file_search_completed(response(&request, &["hidden"])));
    views.handle_event(key(KeyCode::Esc, KeyModifiers::NONE));
    let resumed = views.file_search_request().unwrap();
    assert!(views.file_search_completed(response(&resumed, &["active.rs"])));
    assert_eq!(views.visible_agent_id(), Some(&start.agents[0].id));
    views.agents[0].historical = true;
    views.agents[0].app.freeze_worker();
    assert!(views.file_search_request().is_none());
    views.handle_event(Event::Paste("must not type".into()));
    assert_eq!(views.visible_app().input(), "@a");
}
