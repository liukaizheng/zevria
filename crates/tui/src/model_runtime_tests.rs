use super::*;
use crate::command::SlashCommand;
use zevria_foundation::ModelContextPolicy;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::ModelRole;
use zevria_foundation::ReasoningLevel as Level;
use zevria_foundation::SessionMode;
use zevria_model::ContextTokenSnapshot;
use zevria_model::ContextTokenSource;
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
use zevria_model::models::{ModelCandidate, ModelSelection};

#[test]
fn completion_enter_opens_model_selection_with_the_existing_management_scope() {
    for (prefix, command, scope) in [
        (
            "/mod",
            SlashCommand::Model,
            ModelSelectionScope::SessionAndDefault,
        ),
        (
            "/model-s",
            SlashCommand::ModelSession,
            ModelSelectionScope::SessionOnly,
        ),
    ] {
        let mut root = App::new();
        root.set_focus_for_test(crate::app::FocusState::Insert);
        root.set_input_for_test(prefix, prefix.len());
        let mut views = SessionViews::new(root, PathBuf::from("."));
        let enter = Event::Key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            views.handle_event(enter.clone()),
            Some(UiAction::RunCommand(command))
        );
        assert!(views.root.input().is_empty());
        assert!(views.root.history().is_empty());
        // The runtime's RunCommand arm opens this same management path.
        views.open_model_picker(scope);
        assert!(views.overlays.models.is_open());
        assert!(views.root.is_busy());
        assert!(matches!(
            views.overlays.models.commands.pop_front(),
            Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request: Request::List { mode: SessionMode::Build, scope: actual_scope }, ..
            })) if actual_scope == scope
        ));
        assert_eq!(views.handle_event(enter), None);
        assert!(
            views.overlays.models.commands.is_empty(),
            "catalog request is still pending"
        );
    }
}

#[test]
fn model_telemetry_changes_only_after_matching_scope_mode_and_request_success() {
    for scope in [
        ModelSelectionScope::SessionOnly,
        ModelSelectionScope::SessionAndDefault,
    ] {
        let old = ModelProfileRef::new("configured", "old");
        let context = ModelContextPolicy {
            profile: ModelProfileRef::new("configured", "new"),
            context_window_tokens: 10000,
            input_token_limit: 9000,
            retained_user_tokens: 100,
        };
        let mut root = App::new().with_model_profiles([
            (ModelRole::Build, old.clone()),
            (ModelRole::Plan, old.clone()),
        ]);
        root.set_mode_for_test(SessionMode::Plan);
        let mut views = SessionViews::new(root, PathBuf::from("."));
        let original_profiles = views.model_profiles.clone();
        views.open_model_picker(scope);
        let Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request_id,
            request:
                Request::List {
                    mode: SessionMode::Plan,
                    scope: actual_scope,
                },
        })) = views.overlays.models.commands.pop_front()
        else {
            panic!("captured list")
        };
        assert_eq!(actual_scope, scope);
        assert!(views.root.is_busy());
        assert_eq!(views.model_profiles, original_profiles);
        views.apply(SessionEvent::ModelsResult {
            request_id: request_id.clone(),
            result: Result::Catalog {
                mode: SessionMode::Plan,
                scope,
                current: ModelSelection::new(old.clone(), Level::Medium),
                profiles: vec![ModelCandidate {
                    context: context.clone(),
                    reasoning_levels: vec![Level::Medium],
                }],
                revision: "revision".into(),
            },
        });
        // A later composer mode cannot redirect this picker selection.
        views.root.set_mode_for_test(SessionMode::Build);
        views.overlays.models.handle_key(KeyCode::Enter);
        assert!(views.overlays.models.commands.is_empty());
        views.overlays.models.handle_key(KeyCode::Enter);
        assert!(
            matches!(views.overlays.models.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models { request: Request::Select { mode: SessionMode::Plan, scope: actual_scope, .. }, .. })) if actual_scope == scope)
        );
        assert_eq!(views.model_profiles, original_profiles);
        let snapshot = ContextTokenSnapshot {
            profile: context.profile.clone(),
            model_role: ModelRole::Plan,
            projected_input_tokens: 123,
            source: ContextTokenSource::ConservativeEstimate,
            automatic_trigger: 8100,
            input_token_limit: context.input_token_limit,
            context_window_tokens: context.context_window_tokens,
        };
        let changed = Result::Changed {
            role: ModelRole::Plan,
            scope,
            context: context.clone(),
            snapshot: Some(snapshot),
            reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            revision: "revision".into(),
            unchanged: false,
        };
        let mut wrong_scope = changed.clone();
        let Result::Changed {
            scope: invalid_scope,
            ..
        } = &mut wrong_scope
        else {
            unreachable!()
        };
        *invalid_scope = if scope == ModelSelectionScope::SessionOnly {
            ModelSelectionScope::SessionAndDefault
        } else {
            ModelSelectionScope::SessionOnly
        };
        let mut wrong_mode = changed.clone();
        let Result::Changed { role, .. } = &mut wrong_mode else {
            unreachable!()
        };
        *role = ModelRole::Build;
        for (id, result) in [
            ("previous-picker".into(), changed.clone()),
            (request_id.clone(), wrong_scope),
            (request_id.clone(), wrong_mode),
        ] {
            views.apply(SessionEvent::ModelsResult {
                request_id: id,
                result,
            });
            assert_eq!(views.model_profiles, original_profiles);
            assert!(views.overlays.models.is_open() && views.root.is_busy());
        }
        views.apply(SessionEvent::ModelsResult {
            request_id,
            result: changed,
        });
        assert_eq!(
            views.model_profiles.get(ModelRole::Plan),
            Some(&context.profile)
        );
        assert_eq!(views.model_profiles.get(ModelRole::Build), Some(&old));
        assert!(!views.overlays.models.is_open() && !views.root.is_busy());
        assert!(
            views.root.history().is_empty(),
            "management never creates a turn"
        );
    }
}

#[test]
fn inspect_panes_cannot_open_either_scope_and_cancel_preserves_profiles() {
    for scope in [
        ModelSelectionScope::SessionOnly,
        ModelSelectionScope::SessionAndDefault,
    ] {
        let mut views = SessionViews::new(App::new(), PathBuf::from("."));
        views.ensure_child(&SubtaskId::new("inspect-test"));
        views.visible = Some(VisiblePane::Subtask(
            views.children[0].app.surface().id.pane,
        ));
        views.open_model_picker(scope);
        assert!(!views.overlays.models.is_open() && views.overlays.models.commands.is_empty());
        assert!(!views.root.is_busy());
        views.visible = None;
        let profiles = views.model_profiles.clone();
        views.open_model_picker(scope);
        let Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request_id,
            ..
        })) = views.overlays.models.commands.pop_front()
        else {
            panic!("list")
        };
        views.overlays.models.cancel();
        views.apply(SessionEvent::ModelsResult {
            request_id,
            result: Result::Cancelled,
        });
        assert_eq!(views.model_profiles, profiles);
        assert!(!views.overlays.models.is_open() && !views.root.is_busy());
    }
}
