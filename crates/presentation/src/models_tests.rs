use super::*;
use zevria_foundation::{ModelContextPolicy, ModelProfileRef, ModelRole, ReasoningLevel as Level};
use zevria_model::models::ModelSelectionScope::{SessionAndDefault, SessionOnly};
fn candidate(index: usize) -> ModelCandidate {
    ModelCandidate {
        context: ModelContextPolicy {
            profile: ModelProfileRef::new("p", format!("model-{index}")),
            context_window_tokens: 10000,
            input_token_limit: 9000,
            retained_user_tokens: 100,
            image_accounting: Default::default(),
        },
        reasoning_levels: vec![Level::Low, Level::Medium, Level::High],
    }
}
fn selection(index: usize) -> ModelSelection {
    ModelSelection::new(candidate(index).context.profile, Level::Medium)
}
fn catalog(picker: &mut ModelPicker) -> Result {
    Result::Catalog {
        mode: picker.mode.unwrap(),
        scope: picker.scope.unwrap(),
        current: selection(0),
        profiles: vec![candidate(0), candidate(1)],
        revision: "r1".into(),
    }
}
fn loaded(mode: SessionMode, scope: ModelSelectionScope) -> ModelPicker {
    let mut picker = ModelPicker::default();
    assert!(picker.show(mode, scope));
    let id = picker.request_id.clone();
    let result = catalog(&mut picker);
    assert!(picker.accept(&id, &result));
    picker.commands.clear();
    picker
}
fn preview(picker: &ModelPicker) -> ModelSelectionPreview {
    ModelSelectionPreview {
        request_id: picker.request_id.clone(),
        mode: picker.mode.unwrap(),
        scope: picker.scope.unwrap(),
        target: picker.phase.target().unwrap().clone(),
        source: selection(0),
        revision: "r1".into(),
        session_generation: "engine-issued-runtime".into(),
        generation: 42,
        reason: "Source-only opaque checkpoint".into(),
    }
}
fn changed(picker: &ModelPicker) -> Result {
    Result::Changed {
        role: mode_role(picker.mode.unwrap()),
        scope: picker.scope.unwrap(),
        context: candidate(1).context,
        reasoning_level: Level::Medium,
        snapshot: None,
        revision: "r1".into(),
        unchanged: false,
    }
}
#[test]
fn captures_assignment_requires_supported_effort_and_rejects_duplicate_activation() {
    for mode in SessionMode::ALL {
        for scope in [SessionOnly, SessionAndDefault] {
            let mut picker = loaded(mode, scope);
            let id = picker.request_id.clone();
            assert!(!picker.show(SessionMode::Build, SessionOnly));
            assert_eq!(picker.request_id, id);
            assert!(!picker.apply());
            assert!(!picker.choose_model(99));
            assert!(picker.choose_model(1));
            assert!(matches!(
                picker.phase.step(),
                SelectionStep::Reasoning { selected: 1, .. }
            ));
            assert!(!picker.choose_effort(99));
            assert!(picker.choose_effort(2));
            assert!(picker.apply());
            assert!(!picker.apply());
            assert!(
                matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(ManagementCommand::Models { request: Request::Select { mode: m, scope: s, target, revision }, .. })) if m == mode && s == scope && target.reasoning_level == Level::High && revision == "r1")
            );
        }
    }
}
#[test]
fn request_mode_scope_role_target_and_revision_must_correlate() {
    let mut picker = loaded(SessionMode::Plan, SessionOnly);
    picker.choose_model(1);
    picker.apply();
    let id = picker.request_id.clone();
    let good = preview(&picker);
    for bad in [
        ModelSelectionPreview {
            request_id: "stale".into(),
            ..good.clone()
        },
        ModelSelectionPreview {
            mode: SessionMode::Build,
            ..good.clone()
        },
        ModelSelectionPreview {
            scope: SessionAndDefault,
            ..good.clone()
        },
        ModelSelectionPreview {
            revision: "stale".into(),
            ..good.clone()
        },
        ModelSelectionPreview {
            target: selection(0),
            ..good.clone()
        },
    ] {
        assert!(!picker.accept(&id, &Result::ConfirmationRequired(bad)));
    }
    let result = changed(&picker);
    assert!(!picker.accept("stale-session", &result));
    for index in 0..4 {
        let mut bad = result.clone();
        if let Result::Changed {
            role,
            scope,
            reasoning_level,
            context,
            ..
        } = &mut bad
        {
            match index {
                0 => *role = ModelRole::Build,
                1 => *scope = SessionAndDefault,
                2 => *reasoning_level = Level::Low,
                _ => *context = candidate(0).context,
            }
        }
        assert!(!picker.accept(&id, &bad));
    }
    assert!(picker.accept(&id, &Result::ConfirmationRequired(good.clone())));
    assert!(!picker.apply());
    picker.commands.clear();
    assert!(picker.confirm_conversion());
    assert!(
        matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(ManagementCommand::Models { request: Request::Confirm { preview }, .. })) if preview == good)
    );
    assert!(picker.accept(&id, &result));
    assert!(!picker.is_open());
}
#[test]
fn late_catalog_and_preview_cannot_reopen_cancelling_but_committed_success_wins() {
    let mut picker = ModelPicker::default();
    picker.show(SessionMode::Build, SessionOnly);
    let id = picker.request_id.clone();
    picker.cancel();
    picker.cancel();
    assert_eq!(picker.commands.len(), 2); // List + one Cancel
    let result = catalog(&mut picker);
    assert!(picker.accept(&id, &result));
    assert!(matches!(picker.phase, ModelPhase::Cancelling { .. }));
    assert!(!picker.choose_model(0));
    assert!(picker.accept(&id, &Result::Cancelled));
    let mut picker = loaded(SessionMode::Build, SessionOnly);
    picker.choose_model(1);
    picker.apply();
    let id = picker.request_id.clone();
    let preview = preview(&picker);
    let result = changed(&picker);
    picker.cancel();
    assert!(picker.accept(&id, &Result::ConfirmationRequired(preview)));
    assert!(matches!(picker.phase, ModelPhase::Cancelling { .. }));
    assert!(picker.accept(&id, &result));
    assert!(!picker.is_open());
    assert!(!picker.accept(&id, &Result::Cancelled));
}
#[test]
fn partial_save_error_and_disconnect_new_session_retire_old_identity() {
    for scope in [SessionOnly, SessionAndDefault] {
        let mut picker = loaded(SessionMode::Plan, scope);
        picker.choose_model(1);
        picker.apply();
        let id = picker.request_id.clone();
        assert!(picker.accept(
            &id,
            &Result::Rejected {
                code: "session_save_failed".into(),
                message: "Fix transcript persistence and retry".into(),
                checkpoint_installed: false,
                current_revision: Some("saved-global".into())
            }
        ));
        assert!(picker.error().is_some());
        assert_eq!(
            picker.revision,
            if scope == SessionOnly {
                "r1"
            } else {
                "saved-global"
            }
        );
        let stale = changed_for_reset();
        picker.reset();
        assert!(picker.commands.is_empty());
        picker.show(SessionMode::Plan, scope);
        assert_ne!(picker.request_id, id);
        assert!(!picker.accept(&id, &stale));
        assert!(picker.pending());
    }
}
#[test]
fn cancelling_partial_save_failures_retain_an_authoritative_notice_after_settlement() {
    for (checkpoint_installed, revision) in
        [(true, None), (false, Some("committed-global".to_string()))]
    {
        let mut picker = loaded(SessionMode::Build, SessionAndDefault);
        picker.choose_model(1);
        picker.apply();
        picker.cancel();
        let id = picker.request_id.clone();
        let message = "The global default or portable checkpoint was saved, but the session selection is unchanged.";
        assert!(picker.accept(
            &id,
            &Result::Rejected {
                code: "session_save_failed".into(),
                message: message.into(),
                checkpoint_installed,
                current_revision: revision
            }
        ));
        assert!(!picker.is_open());
        assert_eq!(picker.take_settlement_notice().as_deref(), Some(message));
        assert!(picker.take_settlement_notice().is_none());
        assert!(!picker.accept(&id, &Result::Cancelled));
    }
}
fn changed_for_reset() -> Result {
    let mut p = loaded(SessionMode::Plan, SessionOnly);
    p.choose_model(1);
    p.apply();
    changed(&p)
}
