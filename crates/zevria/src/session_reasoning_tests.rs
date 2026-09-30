//! Unified model selections: same-profile reasoning, scopes and strict resume.
use super::*;
use zevria_foundation::ReasoningLevel as Level;
use zevria_model::models::{ModelSelection, SessionModels};
use zevria_session_api::ManagementCommand;

async fn change(
    running: &runtime::RunningSession,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    scope: Scope,
    revision: String,
) {
    let role = zevria_model::models::mode_role(mode);
    let profile = running.restoration().model_contexts[role.index()]
        .profile
        .clone();
    running
        .command_sender()
        .send(SessionCommand::Manage(ManagementCommand::Models {
            request_id: "selection".into(),
            request: Request::Select {
                mode,
                scope,
                target: ModelSelection::new(profile, Level::High),
                revision,
            },
        }))
        .unwrap();
    let result = event(events).await;
    assert!(
        matches!(result, SessionEvent::ModelsResult {
        result: Result::Changed { role: actual, reasoning_level: Level::High, .. }, ..
    } if actual == role),
        "{result:?}"
    );
}

#[tokio::test]
async fn native_worker_resume_rejects_removed_reasoning_without_mutation_or_provider_traffic() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let workspace = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let execution = zevria_acp::ExecutionProfile::EnsembleWorker;
    let directory = runtime::sessions_dir(&workspace, execution);
    let mut writer = TranscriptWriter::create_with_id(&directory, "worker-reasoning").unwrap();
    let selections = SessionModels::new(
        ModelSelection::new(ModelProfileRef::new("p", "old"), Level::Medium),
        ModelSelection::new(ModelProfileRef::new("p", "new/with:separators"), Level::Max),
    )
    .unwrap();
    writer
        .rewrite(&[
            TranscriptItem::SessionModels(selections),
            TranscriptItem::Message(rig_core::message::Message::user("saved worker prompt")),
        ])
        .unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);
    let before = std::fs::read(&path).unwrap();
    let error = match runtime::start_session_with_profile(
        &fixture.config,
        &workspace,
        runtime::SessionStart::Resume(path.clone()),
        execution,
    )
    .await
    {
        Ok(_) => panic!("unsupported saved reasoning was silently substituted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("saved reasoning level max"), "{error}");
    assert!(error.contains("Restore") && error.contains("new session"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn unified_reasoning_scopes_all_modes_resume_exactly_and_reject_removed_levels() {
    for mode in SessionMode::ALL {
        for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
            let mut server = Server::new().await;
            let fixture = Fixture::new(&server.url);
            let ordinary = std::fs::read(&fixture.path).unwrap();
            let model_bytes = std::fs::read(&fixture.models_path).unwrap();
            let revision_before = revision(&load(&fixture.path).unwrap()).unwrap();
            let mut running = fixture
                .start(runtime::SessionStart::New {
                    inherited_models: None,
                })
                .await;
            let role = zevria_model::models::mode_role(mode);
            let context = running.restoration().model_contexts[role.index()].clone();
            let initial = running.restoration().reasoning_levels;
            let path = running.restoration().transcript_path.clone();
            let mut events = running.take_event_receiver().unwrap();
            change(&running, &mut events, mode, scope, revision_before.clone()).await;
            assert_eq!(std::fs::read(&fixture.models_path).unwrap(), model_bytes);
            assert_eq!(
                transcript::load(&path).unwrap().len(),
                2,
                "only metadata, no prompt, turn or instruction change"
            );
            let saved = transcript::load_report(&path).unwrap();
            assert_eq!(
                saved
                    .session_models()
                    .unwrap()
                    .unwrap()
                    .reasoning_for_mode(mode),
                Level::High
            );
            if scope == Scope::SessionOnly {
                assert_eq!(std::fs::read(&fixture.path).unwrap(), ordinary);
                assert_eq!(
                    revision(&load(&fixture.path).unwrap()).unwrap(),
                    revision_before
                );
            } else {
                assert_ne!(
                    revision(&load(&fixture.path).unwrap()).unwrap(),
                    revision_before
                );
                let config = load(&fixture.path).unwrap();
                for other in ModelRole::ALL {
                    assert_eq!(
                        config.modes().for_role(other).reasoning_level,
                        if other == role {
                            Level::High
                        } else {
                            initial[other.index()]
                        }
                    );
                }
            }
            running
                .command_sender()
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        mode,
                        text: "persist this session".into(),
                    },
                ))
                .unwrap();
            completed(
                &mut events,
                &context.profile,
                mode,
                context.input_token_limit,
            )
            .await;
            let request = server
                .expect(&context.profile.provider, &context.profile.model)
                .await;
            assert_eq!(request["reasoning"]["effort"], "high");
            running.shutdown().await.unwrap();
            let resumed = fixture
                .start(runtime::SessionStart::Resume(path.clone()))
                .await;
            for other in ModelRole::ALL {
                assert_eq!(
                    resumed.restoration().reasoning_levels[other.index()],
                    if other == role {
                        Level::High
                    } else {
                        initial[other.index()]
                    }
                );
            }
            resumed.shutdown().await.unwrap();
            let fresh = fixture
                .start(runtime::SessionStart::New {
                    inherited_models: None,
                })
                .await;
            assert_eq!(
                fresh.restoration().reasoning_levels[role.index()],
                if scope == Scope::SessionOnly {
                    initial[role.index()]
                } else {
                    Level::High
                }
            );
            fresh.shutdown().await.unwrap();

            // Later config reasoning defaults cannot retarget an existing root.
            let mut config: toml::Table =
                toml::from_str(&std::fs::read_to_string(&fixture.path).unwrap()).unwrap();
            config["modes"][role.name()]["reasoning_level"] = "low".into();
            std::fs::write(&fixture.path, toml::to_string(&config).unwrap()).unwrap();
            let resumed = fixture
                .start(runtime::SessionStart::Resume(path.clone()))
                .await;
            assert_eq!(
                resumed.restoration().reasoning_levels[role.index()],
                Level::High
            );
            resumed.shutdown().await.unwrap();

            let mut models: Value = serde_json::from_slice(&model_bytes).unwrap();
            models["providers"][&context.profile.provider]["models"][&context.profile.model]["reasoning_levels"] =
                json!(["low", "medium"]);
            std::fs::write(
                &fixture.models_path,
                serde_json::to_string_pretty(&models).unwrap(),
            )
            .unwrap();
            let saved_bytes = std::fs::read(&path).unwrap();
            let error = match runtime::start_session(
                &fixture.config,
                fixture.directory.path(),
                runtime::SessionStart::Resume(path.clone()),
            )
            .await
            {
                Ok(_) => panic!("unsupported saved reasoning was silently substituted"),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains("saved reasoning level high"), "{error}");
            assert!(error.contains("no fallback is applied"), "{error}");
            assert_eq!(std::fs::read(&path).unwrap(), saved_bytes);
            assert!(
                server.requests.try_recv().is_err(),
                "management and restoration must not call a provider"
            );
        }
    }
}
