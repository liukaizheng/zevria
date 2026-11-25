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
use zevria_session_api::ManagementEvent;

#[test]
fn alt_m_opens_session_only_from_noncapturing_surfaces_without_consuming_drafts() {
    use crate::app::FocusState;
    use ratatui::crossterm::event::{KeyEvent, KeyEventKind};
    for (insert, completion, selection) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let mut root = App::new();
        root.restore(vec![
            TranscriptItem::Message(rig_core::message::Message::user("saved prompt")),
            TranscriptItem::Message(rig_core::message::Message::assistant("saved answer")),
        ]);
        root.set_focus_for_test(if insert {
            FocusState::Insert
        } else {
            FocusState::Normal
        });
        let text = if completion {
            "/mod"
        } else {
            "é draft\nsecond line"
        };
        root.set_input_for_test(text, if completion { text.len() } else { 2 });
        let mut views = SessionViews::new(root, PathBuf::from("."));
        if selection {
            views.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('v'),
                KeyModifiers::NONE,
            )));
        }
        let draft = views.workspace.root.draft_for_test();
        let history = format!("{:?}", views.workspace.root.history());
        let surface = views.workspace.root.surface();
        let shortcut = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
        assert!(views.handle_event(Event::Key(shortcut)).is_none());
        let Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request_id,
            request:
                Request::List {
                    mode: SessionMode::Build,
                    scope: ModelSelectionScope::SessionOnly,
                },
        })) = views.workspace.controls.models.commands.pop_front()
        else {
            panic!("shortcut must list the captured role, session only");
        };
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            views.handle_event(Event::Key(KeyEvent { kind, ..shortcut }));
        }
        assert!(views.workspace.controls.models.commands.is_empty());
        assert_eq!(views.workspace.root.draft_for_test(), draft);
        assert_eq!(format!("{:?}", views.workspace.root.history()), history);
        views.workspace.controls.models.cancel();
        views.apply(SessionEvent::Manage(ManagementEvent::ModelsResult {
            request_id,
            result: Box::new(Result::Cancelled),
        }));
        assert_eq!(views.workspace.root.draft_for_test(), draft);
        assert_eq!(views.workspace.root.surface().id, surface.id);
        assert!(!views.workspace.root.is_busy());
    }
}

#[test]
fn help_retains_shortcut_ownership_and_does_not_open_a_model_lifecycle() {
    let mut views = SessionViews::new(App::new(), PathBuf::from("."));
    let owner = views.workspace.root.surface();
    views
        .workspace
        .controls
        .show_help(owner.context(), &views.workspace.root.hint_eligibility());
    views.handle_event(Event::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('m'),
        KeyModifiers::ALT,
    )));
    assert!(!views.workspace.controls.models.is_open());
    assert!(!views.workspace.root.is_busy());
}

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
        assert!(views.workspace.root.input().is_empty());
        assert!(views.workspace.root.history().is_empty());
        // The runtime's RunCommand arm opens this same management path.
        views.open_model_picker(scope);
        assert!(views.workspace.controls.models.is_open());
        assert!(views.workspace.root.is_busy());
        assert!(matches!(
            views.workspace.controls.models.commands.pop_front(),
            Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request: Request::List { mode: SessionMode::Build, scope: actual_scope }, ..
            })) if actual_scope == scope
        ));
        assert_eq!(views.handle_event(enter), None);
        assert!(
            views.workspace.controls.models.commands.is_empty(),
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
            image_accounting: Default::default(),
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
        })) = views.workspace.controls.models.commands.pop_front()
        else {
            panic!("captured list")
        };
        assert_eq!(actual_scope, scope);
        assert!(views.workspace.root.is_busy());
        assert_eq!(views.model_profiles, original_profiles);
        views.apply(SessionEvent::Manage(ManagementEvent::ModelsResult {
            request_id: request_id.clone(),
            result: Box::new(Result::Catalog {
                mode: SessionMode::Plan,
                scope,
                current: ModelSelection::new(old.clone(), Level::Medium),
                profiles: vec![ModelCandidate {
                    context: context.clone(),
                    reasoning_levels: vec![Level::Medium],
                }],
                revision: "revision".into(),
            }),
        }));
        // A later composer mode cannot redirect this picker selection.
        views.workspace.root.set_mode_for_test(SessionMode::Build);
        views.workspace.controls.models.handle_key(KeyCode::Enter);
        assert!(views.workspace.controls.models.commands.is_empty());
        views.workspace.controls.models.handle_key(KeyCode::Enter);
        assert!(
            matches!(views.workspace.controls.models.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models { request: Request::Select { mode: SessionMode::Plan, scope: actual_scope, .. }, .. })) if actual_scope == scope)
        );
        assert_eq!(views.model_profiles, original_profiles);
        let snapshot = ContextTokenSnapshot {
            profile: context.profile.clone(),
            model_role: ModelRole::Plan,
            projected_input_tokens: Some(123),
            known_input_tokens: 123,
            unresolved_media: Default::default(),
            source: ContextTokenSource::LocalEstimate,
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
            views.apply(SessionEvent::Manage(ManagementEvent::ModelsResult {
                request_id: id,
                result: Box::new(result),
            }));
            assert_eq!(views.model_profiles, original_profiles);
            assert!(views.workspace.controls.models.is_open() && views.workspace.root.is_busy());
        }
        views.apply(SessionEvent::Manage(ManagementEvent::ModelsResult {
            request_id,
            result: Box::new(changed),
        }));
        assert_eq!(
            views.model_profiles.get(ModelRole::Plan),
            Some(&context.profile)
        );
        assert_eq!(views.model_profiles.get(ModelRole::Build), Some(&old));
        assert!(!views.workspace.controls.models.is_open() && !views.workspace.root.is_busy());
        assert!(
            views.workspace.root.history().is_empty(),
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
            views.workspace.children[0].app.surface().id.pane,
        ));
        views.open_model_picker(scope);
        assert!(
            !views.workspace.controls.models.is_open()
                && views.workspace.controls.models.commands.is_empty()
        );
        assert!(!views.workspace.root.is_busy());
        views.visible = None;
        let profiles = views.model_profiles.clone();
        views.open_model_picker(scope);
        let Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request_id,
            ..
        })) = views.workspace.controls.models.commands.pop_front()
        else {
            panic!("list")
        };
        views.workspace.controls.models.cancel();
        views.apply(SessionEvent::Manage(ManagementEvent::ModelsResult {
            request_id,
            result: Box::new(Result::Cancelled),
        }));
        assert_eq!(views.model_profiles, profiles);
        assert!(!views.workspace.controls.models.is_open() && !views.workspace.root.is_busy());
    }
}
