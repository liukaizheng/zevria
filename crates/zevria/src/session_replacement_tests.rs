//! Production replacement wiring, durable model headers and isolation.
use super::*;
use zevria_foundation::ReasoningLevel as Level;
use zevria_instructions::{DirectiveContent, SkillName, SkillSnapshot};
use zevria_model::models::{ModelSelection, SessionModels};
use zevria_session_api::{ManagementCommand, ModeSelectionResult, TurnCommand};
use zevria_tui::{App, RestorationInput, UiOutcome};
use zevria_workflow::{PlanDecision, PlanRecord, PlanWorkflowState};

fn tui_models(restoration: &runtime::SessionRestoration) -> SessionModels {
    let mut root = App::new();
    root.restore_session(RestorationInput {
        items: restoration.transcript_items.clone(),
        workflow: restoration.plan_state.clone(),
        selected_mode: restoration.selected_mode,
        model_profiles: ModelRole::ALL
            .into_iter()
            .map(|role| {
                (
                    role,
                    restoration.model_contexts[role.index()].profile.clone(),
                )
            })
            .collect(),
        contexts: restoration.model_snapshots.clone(),
        reasoning: restoration.reasoning_levels,
        persistence_error: None,
    });
    root.session_models().unwrap()
}

async fn select_complete(
    running: &runtime::RunningSession,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    target: ModelSelection,
    revision: &str,
) {
    running
        .command_sender()
        .send(SessionCommand::Manage(ManagementCommand::Models {
            request_id: "committed-selection".into(),
            request: Request::Select {
                mode,
                scope: Scope::SessionOnly,
                target: target.clone(),
                revision: revision.into(),
            },
        }))
        .unwrap();
    loop {
        if let SessionEvent::ModelsResult { request_id, result } = event(events).await {
            assert_eq!(request_id, "committed-selection");
            assert!(
                matches!(result, Result::Changed { role, reasoning_level, context, scope: Scope::SessionOnly, .. } if role == zevria_model::models::mode_role(mode) && context.profile == target.profile && reasoning_level == target.reasoning_level)
            );
            return;
        }
    }
}

async fn set_mode(
    running: &runtime::RunningSession,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
) {
    running
        .command_sender()
        .send(SessionCommand::Manage(ManagementCommand::SetMode {
            request_id: "mode-round-trip".into(),
            mode,
        }))
        .unwrap();
    assert!(matches!(event(events).await, SessionEvent::ModeResult {
        request_id, result: ModeSelectionResult::Accepted { mode: actual, .. },
    } if request_id == "mode-round-trip" && actual == mode));
    assert!(
        events.try_recv().is_err(),
        "mode management must not allocate a turn or call a provider"
    );
}

fn assert_models(
    restoration: &runtime::SessionRestoration,
    expected: &SessionModels,
    config: &Config,
) {
    assert_eq!(&tui_models(restoration), expected);
    let policies = config.routing().context_policies();
    for mode in SessionMode::ALL {
        let role = zevria_model::models::mode_role(mode);
        let selected = expected.for_mode(mode);
        let context = &restoration.model_contexts[role.index()];
        assert_eq!(&context.profile, &selected.profile);
        assert_eq!(
            restoration.reasoning_levels[role.index()],
            selected.reasoning_level
        );
        let model = &config.providers()[&selected.profile.provider].models[&selected.profile.model];
        assert_eq!(context.context_window_tokens, model.context_window_tokens);
        assert_eq!(
            context.input_token_limit,
            model
                .input_token_limit
                .unwrap_or(model.context_window_tokens)
        );
        assert_eq!(context.retained_user_tokens, model.retained_user_tokens);
    }
    for role in [ModelRole::Review, ModelRole::Explore, ModelRole::Builder] {
        assert_eq!(
            restoration.model_contexts[role.index()],
            policies[role.index()]
        );
        assert_eq!(
            restoration.reasoning_levels[role.index()],
            config.modes().for_role(role).reasoning_level
        );
    }
    for snapshot in &restoration.model_snapshots {
        let policy = &restoration.model_contexts[snapshot.model_role.index()];
        assert_eq!(snapshot.profile, policy.profile);
        assert_eq!(snapshot.context_window_tokens, policy.context_window_tokens);
        assert_eq!(snapshot.input_token_limit, policy.input_token_limit);
        assert_eq!(
            snapshot.automatic_trigger,
            policy.input_token_limit * config.compaction_policy().auto_trigger_percent() / 100
        );
    }
}

async fn submit(
    running: &runtime::RunningSession,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    expected: &SessionModels,
    server: &mut Server,
) -> Value {
    let selected = expected.for_mode(mode);
    let context =
        &running.restoration().model_contexts[zevria_model::models::mode_role(mode).index()];
    running
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: "new conversation only".into(),
            mode,
            behavior: zevria_foundation::RequestBehavior::Standard,
        }))
        .unwrap();
    completed(events, &selected.profile, mode, context.input_token_limit).await;
    let request = server
        .expect(&selected.profile.provider, &selected.profile.model)
        .await;
    assert_eq!(
        request["reasoning"]["effort"],
        selected.reasoning_level.to_string()
    );
    assert!(
        !request["input"]
            .to_string()
            .contains("zevria_session_models")
    );
    assert!(!request["input"].to_string().contains("SOURCE_CONVERSATION"));
    request
}

#[tokio::test]
async fn repeated_new_replacements_inherit_configured_and_overridden_pairs_after_resume() {
    // Cover neither, either, and both overridden modes. Configuration-origin
    // choices must be inherited just as faithfully as /model-session choices.
    for overridden in [0, 1, 2, 3] {
        let mut server = Server::new().await;
        let fixture = Fixture::new(&server.url);
        let original_config = std::fs::read(&fixture.path).unwrap();
        let original_catalog = std::fs::read(&fixture.models_path).unwrap();
        let original_revision = revision(&load(&fixture.path).unwrap()).unwrap();
        let mut source = fixture
            .start(runtime::SessionStart::New {
                inherited_models: None,
            })
            .await;
        let source_path = source.restoration().transcript_path.clone();
        let source_id = source.restoration().session_id.clone();
        let mut expected = tui_models(source.restoration());
        let mut events = source.take_event_receiver().unwrap();
        for (bit, mode, profile, level) in [
            (
                1,
                SessionMode::Build,
                ModelProfileRef::new("q", "old"),
                Level::High,
            ),
            (
                2,
                SessionMode::Plan,
                ModelProfileRef::new("q", "new/with:separators"),
                Level::Low,
            ),
        ] {
            if overridden & bit != 0 {
                let target = ModelSelection::new(profile, level);
                select_complete(
                    &source,
                    &mut events,
                    mode,
                    target.clone(),
                    &original_revision,
                )
                .await;
                expected = expected.with_selection(mode, target).unwrap();
            }
        }
        source.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&fixture.path).unwrap(), original_config);
        assert_eq!(
            std::fs::read(&fixture.models_path).unwrap(),
            original_catalog
        );
        assert_eq!(
            revision(&load(&fixture.path).unwrap()).unwrap(),
            original_revision
        );
        assert!(server.requests.try_recv().is_err());
        assert_eq!(
            transcript::load_report(&source_path)
                .unwrap()
                .session_models()
                .unwrap()
                .unwrap(),
            &expected
        );

        // Seed non-model source state to prove that /new carries only choices.
        let mut items = transcript::load(&source_path).unwrap();
        items[1] = TranscriptItem::SessionMode(SessionMode::Plan);
        items.push(TranscriptItem::Message(rig_core::message::Message::user(
            "SOURCE_CONVERSATION",
        )));
        let skill = SkillSnapshot::new(
            SkillName::parse("source-skill").unwrap(),
            "Source-only skill",
            "SOURCE_SKILL_BODY",
        )
        .unwrap();
        items.push(TranscriptItem::SkillInvocation(
            zevria_instructions::SkillInvocation::new(
                skill.name().clone(),
                "",
                zevria_instructions::SkillApplication::Activate(skill.clone()),
            ),
        ));
        items.push(TranscriptItem::Directive(DirectiveContent::skill(&skill)));
        items.push(TranscriptItem::Plan(PlanRecord::Started {
            id: zevria_workflow::PlanId::new(),
        }));
        let mut writer = TranscriptWriter::append_to(source_path.clone()).unwrap();
        writer.rewrite(&items).unwrap();
        drop(writer);
        let source_before = std::fs::read(&source_path).unwrap();
        fixture.change_globals();
        let mut assignments: toml::Table =
            toml::from_str(&std::fs::read_to_string(&fixture.path).unwrap()).unwrap();
        for (role, level) in [
            ("build", "high"),
            ("plan", "medium"),
            ("review", "low"),
            ("explore", "high"),
            ("builder", "low"),
        ] {
            assignments["modes"][role]["reasoning_level"] = level.into();
        }
        std::fs::write(&fixture.path, toml::to_string(&assignments).unwrap()).unwrap();
        for path in [&fixture.path, &fixture.models_path] {
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions).unwrap();
        }
        let current_config = load(&fixture.path).unwrap();
        let config_before = std::fs::read(&fixture.path).unwrap();
        let catalog_before = std::fs::read(&fixture.models_path).unwrap();
        let revision_before = revision(&current_config).unwrap();
        let config_metadata = std::fs::metadata(&fixture.path).unwrap();
        let catalog_metadata = std::fs::metadata(&fixture.models_path).unwrap();
        let assert_config_unchanged = || {
            assert_eq!(std::fs::read(&fixture.path).unwrap(), config_before);
            assert_eq!(std::fs::read(&fixture.models_path).unwrap(), catalog_before);
            assert_eq!(
                revision(&load(&fixture.path).unwrap()).unwrap(),
                revision_before
            );
            for (path, before) in [
                (&fixture.path, &config_metadata),
                (&fixture.models_path, &catalog_metadata),
            ] {
                let after = std::fs::metadata(path).unwrap();
                assert_eq!(after.modified().unwrap(), before.modified().unwrap());
                assert_eq!(after.permissions(), before.permissions());
            }
        };
        let resumed = fixture
            .start(runtime::SessionStart::Resume(source_path.clone()))
            .await;
        assert_models(resumed.restoration(), &expected, &current_config);
        assert_eq!(resumed.restoration().selected_mode, SessionMode::Plan);
        assert!(matches!(
            resumed.restoration().plan_state,
            PlanWorkflowState::Planning { .. }
        ));
        assert!(
            !transcript::replay_active_skills(&resumed.restoration().transcript_items)
                .unwrap()
                .is_empty()
        );
        let mut inherited = tui_models(resumed.restoration());
        resumed.shutdown().await.unwrap();
        let mut previous_id = source_id;
        let mut previous_path = source_path.clone();
        let mut prefix = None;
        let mut build_cache_key = None;
        for replacement in 0..3 {
            let outcome = UiOutcome::New { models: inherited };
            let mut running = fixture
                .start(crate::replacement_start(outcome).unwrap())
                .await;
            assert_models(running.restoration(), &expected, &current_config);
            assert_ne!(running.restoration().session_id, previous_id);
            assert_ne!(running.restoration().transcript_path, previous_path);
            assert_eq!(running.restoration().selected_mode, SessionMode::Build);
            assert_eq!(running.restoration().plan_state, PlanWorkflowState::Idle);
            assert_eq!(
                running.restoration().transcript_items,
                vec![
                    TranscriptItem::SessionModels(expected.clone()),
                    TranscriptItem::SessionMode(SessionMode::Build),
                ]
            );
            assert!(transcript::model_input(&running.restoration().transcript_items).is_empty());
            assert!(
                transcript::replay_active_skills(&running.restoration().transcript_items)
                    .unwrap()
                    .is_empty()
            );
            let path = running.restoration().transcript_path.clone();
            let empty_bytes = std::fs::read(&path).unwrap();
            previous_id = running.restoration().session_id.clone();
            previous_path = path.clone();
            let mut events = running.take_event_receiver().unwrap();
            assert!(events.try_recv().is_err());
            assert!(
                server.requests.try_recv().is_err(),
                "/new has no opening generation"
            );
            if replacement != 0 {
                let mut per_role_keys = std::collections::HashMap::new();
                for mode in [
                    SessionMode::Plan,
                    SessionMode::Build,
                    SessionMode::Plan,
                    SessionMode::Build,
                ] {
                    set_mode(&running, &mut events, mode).await;
                    assert!(server.requests.try_recv().is_err());
                    let request = submit(&running, &mut events, mode, &expected, &mut server).await;
                    let stable = request["instructions"]
                        .as_str()
                        .unwrap()
                        .split_once("## Workflow policy:")
                        .unwrap()
                        .0
                        .to_string();
                    if let Some(prefix) = &prefix {
                        assert_eq!(&stable, prefix);
                    } else {
                        prefix = Some(stable);
                    }
                    let key = request["prompt_cache_key"].as_str().unwrap().to_string();
                    if let Some(previous) = per_role_keys.insert(mode, key.clone()) {
                        assert_eq!(previous, key);
                    }
                }
                let key = per_role_keys[&SessionMode::Build].clone();
                if let Some(previous) = &build_cache_key {
                    assert_ne!(
                        &key, previous,
                        "replacement retains a new session/cache identity"
                    );
                }
                build_cache_key = Some(key);
            }
            running.shutdown().await.unwrap();
            if replacement == 0 {
                assert_eq!(std::fs::read(&path).unwrap(), empty_bytes);
            }
            assert_eq!(
                transcript::load_report(&path)
                    .unwrap()
                    .session_models()
                    .unwrap()
                    .unwrap(),
                &expected
            );
            let restored = fixture.start(runtime::SessionStart::Resume(path)).await;
            assert_models(restored.restoration(), &expected, &current_config);
            inherited = tui_models(restored.restoration());
            restored.shutdown().await.unwrap();
            assert_eq!(std::fs::read(&source_path).unwrap(), source_before);
            assert_config_unchanged();
        }
        let mut independent = fixture
            .start(runtime::SessionStart::New {
                inherited_models: None,
            })
            .await;
        assert_eq!(
            independent.restoration().model_contexts,
            current_config.routing().context_policies()
        );
        let defaults = tui_models(independent.restoration());
        assert_ne!(defaults, expected);
        let mut events = independent.take_event_receiver().unwrap();
        let request = submit(
            &independent,
            &mut events,
            SessionMode::Build,
            &defaults,
            &mut server,
        )
        .await;
        assert_eq!(
            Some(
                request["instructions"]
                    .as_str()
                    .unwrap()
                    .split_once("## Workflow policy:")
                    .unwrap()
                    .0
            ),
            prefix.as_deref(),
            "selection inheritance never changes the cacheable instruction prefix"
        );
        independent.shutdown().await.unwrap();
        assert_config_unchanged();
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn same_session_and_fresh_plan_implementation_use_the_saved_build_route() {
    for decision in [PlanDecision::ImplementCurrent, PlanDecision::ImplementFresh] {
        let mut server = Server::new().await;
        let fixture = Fixture::new(&server.url);
        let expected = SessionModels::new(
            ModelSelection::new(ModelProfileRef::new("q", "old"), Level::High),
            ModelSelection::new(ModelProfileRef::new("p", "new/with:separators"), Level::Low),
        )
        .unwrap();
        let mut writer = TranscriptWriter::create_with_id(
            &transcript::sessions_dir(fixture.directory.path()),
            "source-plan",
        )
        .unwrap();
        let approved = plan_handoff("source-plan");
        writer
            .rewrite(&[
                TranscriptItem::SessionModels(expected.clone()),
                TranscriptItem::SessionMode(SessionMode::Plan),
                TranscriptItem::Message(rig_core::message::Message::user("prepare the Plan")),
                TranscriptItem::Plan(PlanRecord::Started {
                    id: approved.artifact.version.id,
                }),
                TranscriptItem::Plan(PlanRecord::Ready {
                    artifact: approved.artifact.clone(),
                }),
            ])
            .unwrap();
        let source_path = writer.path().to_path_buf();
        drop(writer);
        let config_before = std::fs::read(&fixture.path).unwrap();
        let catalog_before = std::fs::read(&fixture.models_path).unwrap();
        let mut source = fixture
            .start(runtime::SessionStart::Resume(source_path.clone()))
            .await;
        let models = tui_models(source.restoration());
        assert_eq!(models, expected);
        let mut events = source.take_event_receiver().unwrap();
        source
            .command_sender()
            .send(SessionCommand::Turn(TurnCommand::ResolvePlan {
                expected: approved.artifact.version,
                decision,
            }))
            .unwrap();
        let mut running;
        let source_before;
        if decision == PlanDecision::ImplementFresh {
            let handoff = loop {
                match event(&mut events).await {
                    SessionEvent::FreshPlanHandoffRequested { handoff } => break handoff,
                    SessionEvent::TurnFailed { error, .. }
                    | SessionEvent::TurnRejected { error, .. } => {
                        panic!("Plan approval failed: {error}")
                    }
                    _ => {}
                }
            };
            assert_eq!(handoff, approved);
            assert!(
                server.requests.try_recv().is_err(),
                "source never performs fresh implementation"
            );
            source.shutdown().await.unwrap();
            source_before = Some(std::fs::read(&source_path).unwrap());
            running = fixture
                .start(crate::replacement_start(UiOutcome::Fresh { handoff, models }).unwrap())
                .await;
            assert_ne!(running.restoration().session_id, "source-plan");
            assert_ne!(running.restoration().transcript_path, source_path);
            assert_eq!(
                running.restoration().transcript_items,
                vec![
                    TranscriptItem::SessionModels(expected.clone()),
                    TranscriptItem::SessionMode(SessionMode::Build)
                ]
            );
            events = running.take_event_receiver().unwrap();
        } else {
            source_before = None;
            running = source;
        }
        assert_models(running.restoration(), &expected, &fixture.config);
        completed(
            &mut events,
            &expected.for_mode(SessionMode::Build).profile,
            SessionMode::Build,
            100000,
        )
        .await;
        let request = server.expect("q", "old").await;
        assert_eq!(request["reasoning"]["effort"], "high");
        assert!(
            request["input"]
                .to_string()
                .contains("Implement approved test plan")
        );
        assert!(
            !request["input"]
                .to_string()
                .contains("zevria_session_models")
        );
        let destination = running.restoration().transcript_path.clone();
        if decision == PlanDecision::ImplementFresh {
            assert_persisted_handoff(&destination, &approved, &expected);
        }
        // Returning to Plan after implementation still uses its independent choice.
        set_mode(&running, &mut events, SessionMode::Plan).await;
        let plan_request = submit(
            &running,
            &mut events,
            SessionMode::Plan,
            &expected,
            &mut server,
        )
        .await;
        assert_eq!(plan_request["reasoning"]["effort"], "low");
        running.shutdown().await.unwrap();
        if let Some(source_before) = source_before {
            assert_eq!(std::fs::read(&source_path).unwrap(), source_before);
        } else {
            assert_eq!(destination, source_path);
        }
        let restored = fixture
            .start(runtime::SessionStart::Resume(destination))
            .await;
        assert_models(restored.restoration(), &expected, &fixture.config);
        restored.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&fixture.path).unwrap(), config_before);
        assert_eq!(std::fs::read(&fixture.models_path).unwrap(), catalog_before);
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn unavailable_inherited_catalog_entries_and_levels_fail_before_generation_without_fallback()
{
    for mode in SessionMode::ALL {
        for failure in ["provider", "model", "reasoning"] {
            let mut server = Server::new().await;
            let fixture = Fixture::new(&server.url);
            let expected = SessionModels::new(
                ModelSelection::new(ModelProfileRef::new("q", "old"), Level::High),
                ModelSelection::new(
                    ModelProfileRef::new("q", "new/with:separators"),
                    Level::High,
                ),
            )
            .unwrap();
            let mut writer = TranscriptWriter::create_with_id(
                &transcript::sessions_dir(fixture.directory.path()),
                "catalog-source",
            )
            .unwrap();
            writer
                .rewrite(&[
                    TranscriptItem::SessionModels(expected.clone()),
                    TranscriptItem::SessionMode(SessionMode::Build),
                ])
                .unwrap();
            let path = writer.path().to_path_buf();
            drop(writer);
            let source_before = std::fs::read(&path).unwrap();
            let mut catalog: Value =
                serde_json::from_slice(&std::fs::read(&fixture.models_path).unwrap()).unwrap();
            let model = &expected.for_mode(mode).profile.model;
            match failure {
                "provider" => {
                    catalog["providers"].as_object_mut().unwrap().remove("q");
                }
                "model" => {
                    catalog["providers"]["q"]["models"]
                        .as_object_mut()
                        .unwrap()
                        .remove(model);
                }
                "reasoning" => {
                    catalog["providers"]["q"]["models"][model]["reasoning_levels"] =
                        json!(["low", "medium"]);
                }
                _ => unreachable!(),
            }
            std::fs::write(
                &fixture.models_path,
                serde_json::to_vec_pretty(&catalog).unwrap(),
            )
            .unwrap();
            let config_before = std::fs::read(&fixture.path).unwrap();
            let catalog_before = std::fs::read(&fixture.models_path).unwrap();
            for outcome in [
                UiOutcome::New {
                    models: expected.clone(),
                },
                UiOutcome::Fresh {
                    handoff: plan_handoff("catalog-source"),
                    models: expected.clone(),
                },
            ] {
                let error = match runtime::start_session(
                    &fixture.config,
                    fixture.directory.path(),
                    crate::replacement_start(outcome).unwrap(),
                )
                .await
                {
                    Ok(running) => {
                        running.shutdown().await.unwrap();
                        panic!("unavailable inherited selection was substituted");
                    }
                    Err(error) => format!("{error:#}"),
                };
                let failed_mode = if failure == "provider" {
                    SessionMode::Build
                } else {
                    mode
                };
                assert!(
                    error.contains(&format!("{failed_mode:?}")) && error.contains("inherited"),
                    "{error}"
                );
                assert!(
                    error.contains("independent new session")
                        && error.contains("not an inheriting /new")
                        && error.contains("no fallback"),
                    "{error}"
                );
                if failure == "reasoning" {
                    assert!(error.contains("reasoning level high"), "{error}");
                }
                assert_eq!(std::fs::read(&path).unwrap(), source_before);
                assert_eq!(std::fs::read(&fixture.path).unwrap(), config_before);
                assert_eq!(std::fs::read(&fixture.models_path).unwrap(), catalog_before);
                assert_eq!(
                    transcript::list_sessions(path.parent().unwrap())
                        .unwrap()
                        .len(),
                    1,
                    "failed startup never writes a destination header"
                );
                assert!(server.requests.try_recv().is_err());
            }
            let independent = fixture
                .start(runtime::SessionStart::New {
                    inherited_models: None,
                })
                .await;
            assert_eq!(
                independent.restoration().model_contexts,
                load(&fixture.path).unwrap().routing().context_policies()
            );
            independent.shutdown().await.unwrap();
            assert!(server.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn native_workers_cannot_receive_root_replacement_inheritance() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let selections = SessionModels::new(
        fixture.config.modes().build.selection(),
        fixture.config.modes().plan.selection(),
    )
    .unwrap();
    let error = match runtime::start_session_with_profile(
        &fixture.config,
        fixture.directory.path(),
        runtime::SessionStart::New {
            inherited_models: Some(selections),
        },
        zevria_acp::ExecutionProfile::EnsembleWorker,
    )
    .await
    {
        Ok(_) => panic!("worker accepted root inheritance"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("interactive root replacements"));
    assert!(!fixture.directory.path().join(".zevria").exists());
    assert!(server.requests.try_recv().is_err());
}
