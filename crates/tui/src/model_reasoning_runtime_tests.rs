use super::*;
use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel as Level, SessionMode};
use zevria_model::models::{
    ModelCandidate, ModelManagementRequest as Request, ModelManagementResult as Result,
    ModelSelection,
};
use zevria_session_api::ManagementCommand;

#[test]
fn reasoning_updates_only_matching_role_without_touching_profiles_history_or_context() {
    for scope in [
        ModelSelectionScope::SessionOnly,
        ModelSelectionScope::SessionAndDefault,
    ] {
        let profile = ModelProfileRef::new("configured", "same");
        let mut root = App::new()
            .with_model_profiles(ModelRole::ALL.map(|role| (role, profile.clone())))
            .with_reasoning_levels([Level::Medium; ModelRole::COUNT]);
        root.restore(Vec::new()); // startup restoration must retain the seeded levels
        root.set_mode_for_test(SessionMode::Plan);
        let mut views = SessionViews::new(root, PathBuf::from("."));
        let before = views.root.status_for_test();
        views.open_model_picker(scope);
        assert!(views.root.is_busy());
        let Some(SessionCommand::Manage(ManagementCommand::Models {
            request_id,
            request:
                Request::List {
                    mode: SessionMode::Plan,
                    ..
                },
        })) = views.overlays.models.commands.pop_front()
        else {
            panic!("list")
        };
        let context = zevria_foundation::ModelContextPolicy {
            profile: profile.clone(),
            context_window_tokens: 10000,
            input_token_limit: 9000,
            retained_user_tokens: 100,
        };
        views.apply(SessionEvent::ModelsResult {
            request_id: request_id.clone(),
            result: Result::Catalog {
                mode: SessionMode::Plan,
                scope,
                current: ModelSelection::new(profile.clone(), Level::Medium),
                profiles: vec![ModelCandidate {
                    context: context.clone(),
                    reasoning_levels: vec![Level::Medium, Level::High],
                }],
                revision: "r".into(),
            },
        });
        views.overlays.models.handle_key(KeyCode::Enter);
        assert!(views.overlays.models.commands.is_empty());
        views.overlays.models.handle_key(KeyCode::End);
        views.overlays.models.handle_key(KeyCode::Enter);
        assert_eq!(views.root.status_for_test().reasoning, Some(Level::Medium));
        let changed = Result::Changed {
            role: ModelRole::Plan,
            scope,
            context,
            snapshot: None,
            reasoning_level: Level::High,
            revision: "r".into(),
            unchanged: false,
        };
        views.apply(SessionEvent::ModelsResult {
            request_id: "old".into(),
            result: changed.clone(),
        });
        assert_eq!(views.root.status_for_test().reasoning, Some(Level::Medium));
        assert!(views.root.is_busy());
        views.apply(SessionEvent::ModelsResult {
            request_id,
            result: changed,
        });
        assert!(!views.overlays.models.is_open() && !views.root.is_busy());
        let after = views.root.status_for_test();
        assert_eq!(after.reasoning, Some(Level::High));
        assert_eq!(after.profile, before.profile);
        assert_eq!(after.context, before.context);
        assert_eq!(after.response, before.response);
        assert!(views.root.history().is_empty());
        views.root.set_mode_for_test(SessionMode::Build);
        assert_eq!(views.root.status_for_test().reasoning, Some(Level::Medium));
    }
}

#[test]
fn child_status_uses_its_configured_reasoning_not_the_root_override() {
    let mut root = App::new()
        .with_model_profiles(
            ModelRole::ALL.map(|role| (role, ModelProfileRef::new("p", role.name()))),
        )
        .with_reasoning_levels([Level::Medium; ModelRole::COUNT]);
    root.install_reasoning(ModelRole::Build, Level::High);
    root.install_reasoning(ModelRole::Plan, Level::Low);
    let mut views = SessionViews::new(root, PathBuf::from("."));
    let index = views.ensure_child(&SubtaskId::new("reasoning-child"));
    for role in [ModelRole::Explore, ModelRole::Builder] {
        views.children[index].app.set_inspect_model_role(role);
        assert_eq!(
            views.children[index].app.status_for_test().reasoning,
            Some(Level::Medium)
        );
    }
}

#[test]
fn inspect_panes_are_blocked_and_ctrl_c_cancels_only_the_reasoning_picker() {
    let mut views = SessionViews::new(
        App::new().with_reasoning_levels([Level::Medium; ModelRole::COUNT]),
        PathBuf::from("."),
    );
    views.ensure_child(&SubtaskId::new("inspect-test"));
    views.visible = Some(VisiblePane::Subtask(
        views.children[0].app.surface().id.pane,
    ));
    views.open_model_picker(ModelSelectionScope::SessionOnly);
    assert!(!views.overlays.models.is_open() && !views.root.is_busy());
    views.visible = None;
    views.open_model_picker(ModelSelectionScope::SessionOnly);
    let Some(SessionCommand::Manage(ManagementCommand::Models { request_id, .. })) =
        views.overlays.models.commands.pop_front()
    else {
        panic!("list")
    };
    let event = Event::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ));
    assert!(views.handle_event(event).is_none());
    assert!(matches!(
        views.overlays.models.commands.pop_front(),
        Some(SessionCommand::Manage(ManagementCommand::Models {
            request: Request::Cancel,
            ..
        }))
    ));
    views.apply(SessionEvent::ModelsResult {
        request_id,
        result: Result::Cancelled,
    });
    assert!(!views.overlays.models.is_open() && !views.root.is_busy());
    assert_eq!(views.root.status_for_test().reasoning, Some(Level::Medium));
}
