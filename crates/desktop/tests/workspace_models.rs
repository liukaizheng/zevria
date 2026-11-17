use rig_core::message::Message;
use zevria_desktop::session::{InputRoute, WorkspaceModel};
use zevria_foundation::{
    ModelContextPolicy, ModelProfileRef, ModelRole, ReasoningLevel as Level, SessionMode,
    SubtaskId, TurnId,
};
use zevria_model::{
    ContextTokenSnapshot, ContextTokenSource, ProviderReplay, TokenUsage,
    models::{
        ModelCandidate, ModelManagementResult as Result, ModelSelection,
        ModelSelectionScope as Scope, mode_role,
    },
};
use zevria_presentation::{ConversationTail, conversation::HistoryEntry};
use zevria_session_api::{SessionEvent as E, SessionUpdate as U};
const TURN: TurnId = TurnId::new(1);
fn context() -> ModelContextPolicy {
    ModelContextPolicy {
        profile: ModelProfileRef::new("p", "model"),
        context_window_tokens: 128_000,
        input_token_limit: 100_000,
        retained_user_tokens: 100,
        image_accounting: Default::default(),
    }
}
fn fixture(mode: SessionMode) -> WorkspaceModel {
    let mut model = WorkspaceModel::default();
    for role in ModelRole::ALL {
        model.root.install_model(role, &context(), None);
        model.root.install_reasoning(role, Level::Medium);
    }
    model.apply(U::Lifecycle(E::ModeChanged { mode }));
    model.apply(U::Lifecycle(E::TurnStarted {
        turn_id: TURN,
        mode,
        message: Message::user("saved prompt"),
    }));
    let role = mode_role(mode);
    model.apply(U::Lifecycle(E::UsageUpdated {
        turn_id: TURN,
        usage: TokenUsage {
            input_tokens: 2000,
            cached_tokens: 1600,
            output_tokens: 100,
            total_tokens: 2100,
        },
        profile: context().profile,
        model_role: role,
        input_token_limit: 100_000,
        context_window_tokens: 128_000,
    }));
    model.apply(U::Lifecycle(E::ContextUsageUpdated {
        turn_id: TURN,
        snapshot: ContextTokenSnapshot {
            profile: context().profile,
            model_role: role,
            projected_input_tokens: Some(3200),
            known_input_tokens: 3200,
            unresolved_media: Default::default(),
            source: ContextTokenSource::UsagePlusDelta,
            automatic_trigger: 80_000,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        },
    }));
    let replay = ProviderReplay::openai_responses(
        context().profile,
        vec![
            serde_json::json!({"type":"reasoning","id":"thinking","summary":[{"type":"summary_text","text":"saved visible thinking"}],"encrypted_content":"native-cipher"}),
            serde_json::json!({"type":"message","role":"assistant","id":"message","status":"completed","content":[{"type":"output_text","text":"saved answer","annotations":[]}]}),
        ],
    );
    model.apply(U::Lifecycle(E::TurnCompleted {
        turn_id: TURN,
        message: replay.to_message().unwrap(),
        attempt_id: None,
    }));
    model
}
fn load(model: &mut WorkspaceModel, mode: SessionMode, scope: Scope) -> String {
    assert!(model.open_model_picker(scope));
    let id = model.models.request_id.clone();
    model.apply(U::Lifecycle(E::ModelsResult {
        request_id: id.clone(),
        result: Result::Catalog {
            mode,
            scope,
            current: ModelSelection::new(context().profile, Level::Medium),
            profiles: vec![ModelCandidate {
                context: context(),
                reasoning_levels: vec![Level::Medium, Level::High],
            }],
            revision: "r".into(),
        },
    }));
    assert!(model.models.choose_model(0));
    assert!(model.models.choose_effort(1));
    assert!(model.models.apply());
    id
}
fn changed(mode: SessionMode, scope: Scope) -> Result {
    Result::Changed {
        role: mode_role(mode),
        scope,
        context: context(),
        reasoning_level: Level::High,
        snapshot: None,
        revision: "r".into(),
        unchanged: false,
    }
}
#[test]
fn nonempty_effort_success_is_correlated_in_place_and_retains_telemetry_and_block_identity() {
    for mode in SessionMode::ALL {
        for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
            let mut model = fixture(mode);
            let epoch = model.root.conversation.epoch();
            let ids = model
                .root
                .conversation
                .history()
                .iter()
                .filter_map(|entry| {
                    if let HistoryEntry::Conversation(entry) = entry {
                        Some(entry.blocks.iter().map(|b| b.id).collect::<Vec<_>>())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let history = format!("{:?}", model.root.conversation.history());
            let telemetry = model.root.session.telemetry(mode_role(mode)).clone();
            let profiles = model.root.model_profiles.clone();
            let id = load(&mut model, mode, scope);
            assert!(matches!(
                model.root.tail(std::time::Instant::now()),
                ConversationTail::None
            ));
            assert!(!model.open_model_picker(scope));
            assert!(matches!(
                model.route_input("must remain a draft", false),
                InputRoute::Retained
            ));
            assert!(model.root.select_mode(SessionMode::Plan).is_none());
            model.apply(U::Lifecycle(E::ModelsResult {
                request_id: "old-picker".into(),
                result: changed(mode, scope),
            }));
            assert_eq!(
                model.root.session.reasoning(mode_role(mode)),
                Some(Level::Medium)
            );
            model.models.cancel(); // Authoritative commit wins a cancellation race.
            model.apply(U::Lifecycle(E::ModelsResult {
                request_id: id,
                result: changed(mode, scope),
            }));
            assert!(!model.models.is_open() && !model.root.is_busy());
            assert_eq!(
                model.root.session.reasoning(mode_role(mode)),
                Some(Level::High)
            );
            assert_eq!(model.root.session.telemetry(mode_role(mode)), &telemetry);
            assert_eq!(model.root.conversation.epoch(), epoch);
            assert_eq!(format!("{:?}", model.root.conversation.history()), history);
            assert_eq!(
                model
                    .root
                    .conversation
                    .history()
                    .iter()
                    .filter_map(|entry| if let HistoryEntry::Conversation(entry) = entry {
                        Some(entry.blocks.iter().map(|b| b.id).collect::<Vec<_>>())
                    } else {
                        None
                    })
                    .collect::<Vec<_>>(),
                ids
            );
            assert!(std::sync::Arc::ptr_eq(
                &profiles,
                &model.root.model_profiles
            ));
            let saved = model.root.session_models().unwrap();
            assert_eq!(saved.reasoning_for_mode(mode), Level::High);
            assert_eq!(
                saved.reasoning_for_mode(if mode == SessionMode::Build {
                    SessionMode::Plan
                } else {
                    SessionMode::Build
                }),
                Level::Medium
            );
        }
    }
}
#[test]
fn cancellation_cannot_hide_a_partially_saved_global_default() {
    let mut model = fixture(SessionMode::Build);
    let id = load(&mut model, SessionMode::Build, Scope::SessionAndDefault);
    model.models.cancel();
    model.apply(U::Lifecycle(E::ModelsResult { request_id: id.clone(), result: Result::Rejected { code: "session_save_failed".into(), message: "The global default was saved, but this session's active and durable selections are unchanged.".into(), checkpoint_installed: false, current_revision: Some("saved-global-revision".into()) } }));
    assert!(!model.models.is_open() && !model.root.is_busy());
    assert_eq!(
        model.root.session.reasoning(ModelRole::Build),
        Some(Level::Medium)
    );
    assert!(model.root.conversation.history().iter().any(|entry| matches!(entry, HistoryEntry::Error(message) if message.contains("global default was saved"))));
    model.apply(U::Lifecycle(E::ModelsResult {
        request_id: id,
        result: Result::Cancelled,
    }));
    assert_eq!(
        model.root.session.reasoning(ModelRole::Build),
        Some(Level::Medium)
    );
}

#[test]
fn slash_routes_have_explicit_scopes_and_do_not_acquire_turn_bookkeeping() {
    let mut model = fixture(SessionMode::Build);
    for (command, scope) in [
        ("/model", Scope::SessionAndDefault),
        ("/model-session", Scope::SessionOnly),
    ] {
        assert!(
            matches!(model.route_input(command, false), InputRoute::Models(actual) if actual == scope)
        );
        assert!(!model.root.is_busy());
    }
}
#[test]
fn hidden_ready_plan_children_busy_roots_and_errors_block_opening_and_disconnect_retires_ids() {
    let mut model = fixture(SessionMode::Build);
    model.selected_child = Some(SubtaskId::new("read-only"));
    assert!(!model.open_model_picker(Scope::SessionOnly));
    model.selected_child = None;
    model
        .root
        .workflow
        .apply_snapshot(zevria_workflow::PlanWorkflowState::Ready {
            artifact: zevria_workflow::PlanArtifact {
                version: zevria_workflow::PlanVersion {
                    id: zevria_workflow::PlanId::new(),
                    revision: 1,
                },
                title: "Plan".into(),
                markdown: "Approved implementation".into(),
                source_turn_id: TURN,
            },
        });
    model.root.workflow.hide();
    assert!(model.root.workflow.dialog().is_none());
    assert!(!model.open_model_picker(Scope::SessionOnly));
    model
        .root
        .workflow
        .apply_snapshot(zevria_workflow::PlanWorkflowState::Idle);
    let id = load(&mut model, SessionMode::Build, Scope::SessionOnly);
    model.apply(U::Lifecycle(E::ModelsResult {
        request_id: id.clone(),
        result: Result::Rejected {
            code: "save_failed".into(),
            message: "Fix the config and retry".into(),
            checkpoint_installed: false,
            current_revision: None,
        },
    }));
    assert!(model.models.error().is_some() && model.root.is_busy());
    model.disconnected();
    assert!(!model.models.is_open() && !model.root.is_busy());
    assert!(model.open_model_picker(Scope::SessionOnly));
    assert_ne!(model.models.request_id, id);
    model.apply(U::Lifecycle(E::ModelsResult {
        request_id: id,
        result: changed(SessionMode::Build, Scope::SessionOnly),
    }));
    assert_eq!(
        model.root.session.reasoning(ModelRole::Build),
        Some(Level::Medium)
    );
}
