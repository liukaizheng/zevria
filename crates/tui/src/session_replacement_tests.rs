//! Transition snapshots must reflect committed root state, never UI drafts or telemetry.
use super::*;
use crate::{App, RestorationInput};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use zevria_foundation::{
    ModelContextPolicy, ModelProfileRef, ModelRole, ReasoningLevel as Level, SessionMode,
};
use zevria_model::models::{
    ModelCandidate, ModelManagementRequest as Request, ModelManagementResult as Result,
    ModelSelection,
};
use zevria_session_api::{ManagementCommand, ModeSelectionResult, session_event_channel};
use zevria_transcript::transcript::TranscriptItem;

fn original_models() -> SessionModels {
    SessionModels::new(
        ModelSelection::new(
            ModelProfileRef::new("build-provider", "build-model"),
            Level::Medium,
        ),
        ModelSelection::new(
            ModelProfileRef::new("plan-provider", "plan-model"),
            Level::Low,
        ),
    )
    .unwrap()
}

fn views() -> SessionViews {
    let models = original_models();
    let mut root = App::new();
    root.restore_session(RestorationInput {
        items: vec![
            TranscriptItem::SessionModels(models.clone()),
            TranscriptItem::SessionMode(SessionMode::Plan),
        ],
        workflow: zevria_workflow::PlanWorkflowState::Idle,
        selected_mode: SessionMode::Plan,
        model_profiles: ModelRole::ALL
            .into_iter()
            .map(|role| {
                let profile = match role {
                    ModelRole::Build => models.for_mode(SessionMode::Build).profile.clone(),
                    ModelRole::Plan => models.for_mode(SessionMode::Plan).profile.clone(),
                    _ => ModelProfileRef::new("other-provider", role.name()),
                };
                (role, profile)
            })
            .collect(),
        contexts: vec![],
        reasoning: std::array::from_fn(|index| {
            if index == ModelRole::Plan.index() {
                Level::Low
            } else {
                Level::Medium
            }
        }),
        persistence_error: None,
    });
    SessionViews::new(root, PathBuf::from("."))
}

fn handoff() -> PlanHandoff {
    PlanHandoff::new(
        zevria_workflow::PlanArtifact {
            version: zevria_workflow::PlanVersion {
                id: zevria_workflow::PlanId::new(),
                revision: 1,
            },
            title: "Approved Plan".into(),
            markdown: "# Approved Plan\n\nImplement this exact artifact.\n".into(),
            source_turn_id: zevria_foundation::TurnId::new(1),
        },
        "source-session",
    )
}

/// Submit a picker draft, but leave the result unacknowledged.
fn pending_selection(views: &mut SessionViews) -> (String, Result) {
    let scope = ModelSelectionScope::SessionOnly;
    views.open_model_picker(scope);
    let Some(SessionCommand::Manage(ManagementCommand::Models {
        request_id,
        request: Request::List {
            mode: SessionMode::Plan,
            ..
        },
    })) = views.overlays.models.commands.pop_front()
    else {
        panic!("Plan catalog request");
    };
    let context = ModelContextPolicy {
        profile: ModelProfileRef::new("replacement-provider", "replacement-plan"),
        context_window_tokens: 10000,
        input_token_limit: 9000,
        retained_user_tokens: 100,
    };
    views.apply(SessionEvent::ModelsResult {
        request_id: request_id.clone(),
        result: Result::Catalog {
            mode: SessionMode::Plan,
            scope,
            current: original_models().for_mode(SessionMode::Plan).clone(),
            profiles: vec![ModelCandidate {
                context: context.clone(),
                reasoning_levels: vec![Level::Low, Level::High],
            }],
            revision: "catalog-revision".into(),
        },
    });
    views.overlays.models.handle_key(KeyCode::Enter);
    views.overlays.models.handle_key(KeyCode::End);
    views.overlays.models.handle_key(KeyCode::Enter);
    assert!(
        matches!(views.overlays.models.commands.pop_front(), Some(SessionCommand::Manage(ManagementCommand::Models { request: Request::Select { target, .. }, .. })) if target == ModelSelection::new(context.profile.clone(), Level::High))
    );
    (
        request_id,
        Result::Changed {
            role: ModelRole::Plan,
            scope,
            context,
            snapshot: None,
            reasoning_level: Level::High,
            revision: "catalog-revision".into(),
            unchanged: false,
        },
    )
}

#[test]
fn new_and_both_fresh_delivery_paths_snapshot_an_immediately_acknowledged_selection() {
    for delivery in ["new", "direct-fresh", "drained-fresh"] {
        let mut views = views();
        let (request_id, result) = pending_selection(&mut views);
        let expected = original_models()
            .with_selection(
                SessionMode::Plan,
                ModelSelection::new(
                    ModelProfileRef::new("replacement-provider", "replacement-plan"),
                    Level::High,
                ),
            )
            .unwrap();
        assert_eq!(
            views.root.session_models().unwrap(),
            original_models(),
            "pending draft is not a selection"
        );
        let acknowledged = SessionEvent::ModelsResult { request_id, result };
        let handoff = handoff();
        let outcome = if delivery == "drained-fresh" {
            let (events, mut updates) = session_event_channel(8);
            events.try_send(acknowledged).unwrap();
            events
                .try_send(SessionEvent::FreshPlanHandoffRequested {
                    handoff: handoff.clone(),
                })
                .unwrap();
            let mut done = false;
            let outcome = drain_update_burst(&mut views, &mut updates, &mut done)
                .unwrap()
                .unwrap();
            assert!(!done);
            outcome
        } else {
            assert!(
                apply_session_update(&mut views, SessionUpdate::Lifecycle(acknowledged))
                    .unwrap()
                    .is_none()
            );
            if delivery == "new" {
                views
                    .root
                    .set_focus_for_test(crate::app::FocusState::Insert);
                views.root.set_input_for_test("/new", 4);
                assert_eq!(
                    views.handle_event(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::CONTROL
                    ))),
                    Some(UiAction::RunCommand(SlashCommand::New))
                );
                new_session_outcome(&views.root).unwrap()
            } else {
                apply_session_update(
                    &mut views,
                    SessionUpdate::Lifecycle(SessionEvent::FreshPlanHandoffRequested {
                        handoff: handoff.clone(),
                    }),
                )
                .unwrap()
                .unwrap()
            }
        };
        assert_eq!(
            outcome,
            if delivery == "new" {
                UiOutcome::New {
                    models: expected.clone(),
                }
            } else {
                UiOutcome::Fresh {
                    handoff,
                    models: expected.clone(),
                }
            }
        );
        assert_eq!(views.root.session_models().unwrap(), expected);
        assert_eq!(views.root.status_for_test().reasoning, Some(Level::High));
        assert_eq!(
            views.root.status_for_test().profile,
            Some(expected.for_mode(SessionMode::Plan).profile.clone())
        );
        // Acknowledgement did not rewrite the immutable restoration projection.
        assert!(views.root.history().is_empty());
    }
}

#[test]
fn pending_cancelled_rejected_and_incorrectly_correlated_results_cannot_replace_committed_choices()
{
    for terminal in [
        Result::Cancelled,
        Result::Rejected {
            code: "save_failed".into(),
            message: "not committed".into(),
            current_revision: None,
            checkpoint_installed: false,
        },
    ] {
        let mut views = views();
        let (request_id, changed) = pending_selection(&mut views);
        let mut wrong_role = changed.clone();
        if let Result::Changed { role, .. } = &mut wrong_role {
            *role = ModelRole::Build;
        }
        let mut wrong_scope = changed.clone();
        if let Result::Changed { scope, .. } = &mut wrong_scope {
            *scope = ModelSelectionScope::SessionAndDefault;
        }
        let mut wrong_target = changed.clone();
        if let Result::Changed { context, .. } = &mut wrong_target {
            context.profile.model = "other-model".into();
        }
        let mut wrong_level = changed.clone();
        if let Result::Changed {
            reasoning_level, ..
        } = &mut wrong_level
        {
            *reasoning_level = Level::Medium;
        }
        for (id, result) in [
            ("previous-picker".into(), changed.clone()),
            (request_id.clone(), wrong_role),
            (request_id.clone(), wrong_scope),
            (request_id.clone(), wrong_target),
            (request_id.clone(), wrong_level),
        ] {
            views.apply(SessionEvent::ModelsResult {
                request_id: id,
                result,
            });
            assert_eq!(
                new_session_outcome(&views.root).unwrap(),
                UiOutcome::New {
                    models: original_models()
                }
            );
        }
        views.apply(SessionEvent::ModelsResult {
            request_id: request_id.clone(),
            result: terminal,
        });
        // Even an otherwise matching result after a rejection/cancellation is stale.
        views.apply(SessionEvent::ModelsResult {
            request_id,
            result: changed,
        });
        assert_eq!(views.root.session_models().unwrap(), original_models());
        let handoff = handoff();
        assert_eq!(
            apply_session_update(
                &mut views,
                SessionUpdate::Lifecycle(SessionEvent::FreshPlanHandoffRequested {
                    handoff: handoff.clone()
                })
            )
            .unwrap(),
            Some(UiOutcome::Fresh {
                handoff,
                models: original_models()
            })
        );
    }
}

#[test]
fn mode_round_trips_display_the_committed_role_specific_profile_and_reasoning() {
    let mut views = views();
    let (request_id, result) = pending_selection(&mut views);
    views.apply(SessionEvent::ModelsResult { request_id, result });
    let models = views.root.session_models().unwrap();
    for mode in [
        SessionMode::Build,
        SessionMode::Plan,
        SessionMode::Build,
        SessionMode::Plan,
    ] {
        let Some(UiAction::SetMode {
            request_id,
            mode: target,
        }) = views.handle_event(Event::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )))
        else {
            panic!("idle mode switch");
        };
        assert_eq!(target, mode);
        views.apply(SessionEvent::ModeResult {
            request_id,
            result: ModeSelectionResult::Accepted {
                mode,
                changed: true,
            },
        });
        let selected = models.for_mode(mode);
        let status = views.root.status_for_test();
        assert_eq!(status.profile, Some(selected.profile.clone()));
        assert_eq!(status.reasoning, Some(selected.reasoning_level));
        assert_eq!(views.root.session_models().unwrap(), models);
    }
}

#[test]
fn incomplete_or_invalid_committed_state_errors_instead_of_using_telemetry_or_defaults() {
    for root in [
        App::new(),
        App::new().with_model_profiles([
            (ModelRole::Build, ModelProfileRef::new("p", "build")),
            (ModelRole::Plan, ModelProfileRef::new("p", "plan")),
        ]),
        App::new()
            .with_model_profiles([
                (ModelRole::Build, ModelProfileRef::new("p", "build")),
                (ModelRole::Plan, ModelProfileRef::new("p", " ")),
            ])
            .with_reasoning_levels([Level::Medium; ModelRole::COUNT]),
    ] {
        assert!(new_session_outcome(&root).is_err());
        let mut views = SessionViews::new(root, PathBuf::from("."));
        let handoff = SessionUpdate::Lifecycle(SessionEvent::FreshPlanHandoffRequested {
            handoff: handoff(),
        });
        assert!(apply_session_update(&mut views, handoff).is_err());
        let (events, mut updates) = session_event_channel(8);
        events
            .try_send(SessionEvent::FreshPlanHandoffRequested {
                handoff: self::handoff(),
            })
            .unwrap();
        assert!(drain_update_burst(&mut views, &mut updates, &mut false).is_err());
    }
    let mut views = views();
    views
        .root
        .restore_model_contexts([zevria_model::ContextTokenSnapshot {
            profile: ModelProfileRef::new("telemetry-only", "not-a-selection"),
            model_role: ModelRole::Plan,
            projected_input_tokens: 123,
            source: zevria_model::ContextTokenSource::ConservativeEstimate,
            automatic_trigger: 8100,
            input_token_limit: 9000,
            context_window_tokens: 10000,
        }]);
    assert_eq!(views.root.session_models().unwrap(), original_models());
}
