use super::*;
use zevria_foundation::ReasoningLevel as Level;
use zevria_model::models::{
    ModelCandidate, ModelManagementRequest as Request, ModelManagementResult as Result,
    ModelSelection, ModelSettingsService, SessionModels,
};

#[derive(Default)]
struct Settings {
    revision: Mutex<String>,
    fail: AtomicBool,
    saves: AtomicUsize,
}
impl ModelSettingsService for Settings {
    fn validate(&self, expected: &str) -> anyhow::Result<()> {
        anyhow::ensure!(*self.revision.lock().unwrap() == expected, "stale settings");
        Ok(())
    }
    fn save(&self, expected: &str, _: ModelRole, _: &ModelSelection) -> anyhow::Result<String> {
        self.validate(expected)?;
        self.saves.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "injected save failure");
        let mut revision = self.revision.lock().unwrap();
        revision.push('x');
        Ok(revision.clone())
    }
}
struct Provider {
    levels: [Level; ModelRole::COUNT],
    profiles: [ModelProfileRef; ModelRole::COUNT],
    resets: usize,
    installs: usize,
    forbid_history_inspection: bool,
}
impl ModelProvider for Provider {
    fn model_catalog(&self) -> Vec<ModelCandidate> {
        [
            test_profile(),
            ModelProfileRef::new("test-provider", "other"),
        ]
        .into_iter()
        .map(|profile| ModelCandidate {
            context: ModelContextPolicy {
                image_accounting: Default::default(),
                profile,
                context_window_tokens: 100_000,
                input_token_limit: 100_000,
                retained_user_tokens: 100,
            },
            reasoning_levels: vec![Level::Low, Level::Medium, Level::High],
        })
        .collect()
    }
    fn preflight_input(
        &self,
        target: &ModelProfileRef,
        input: &[zevria_model::request::ModelRequestItem<'_>],
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        assert!(
            !self.forbid_history_inspection,
            "effort management must not inspect/count/convert history"
        );
        Ok(zevria_model::models::ReplayPreflight::Compatible(
            zevria_model::estimates::estimate_model_input_for_profile(input, target)?,
        ))
    }
    fn install_model_update(&mut self, role: ModelRole, target: &ModelSelection) {
        self.profiles[role.index()] = target.profile.clone();
        self.levels[role.index()] = target.reasoning_level;
        self.installs += 1;
    }
    fn model_selection(&self, role: ModelRole) -> Option<ModelSelection> {
        Some(ModelSelection::new(
            self.profiles[role.index()].clone(),
            self.levels[role.index()],
        ))
    }
    fn complete<'a>(&'a mut self, _: ModelRequest<'a>, _: ProgressReporter) -> ProviderFuture<'a> {
        Box::pin(async { panic!("reasoning management must not make a model call") })
    }
    fn reset(&mut self) {
        self.resets += 1;
    }
}
fn root(settings: Arc<Settings>) -> (tempfile::TempDir, SessionEngine<Provider>) {
    let (directory, mut writer) = test_transcript();
    let items = vec![TranscriptItem::SessionModels(
        SessionModels::new(
            zevria_model::models::ModelSelection::new(
                test_profile(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            zevria_model::models::ModelSelection::new(
                test_profile(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap(),
    )];
    writer.rewrite(&items).unwrap();
    let provider = Provider {
        levels: [Level::Medium; ModelRole::COUNT],
        profiles: std::array::from_fn(|_| test_profile()),
        resets: 0,
        installs: 0,
        forbid_history_inspection: false,
    };
    let engine = engine(provider, writer)
        .with_transcript_items(items)
        .unwrap()
        .with_model_management(settings, String::new())
        .with_compaction_policy(test_compaction_policy(100_000, 90, 100));
    (directory, engine)
}
async fn manage(engine: &mut SessionEngine<Provider>, request: Request) -> Result {
    let (events, mut receiver) = session_event_channel(8);
    engine
        .handle_command(
            SessionCommand::Manage(ManagementCommand::Models {
                request_id: "reasoning".into(),
                request,
            }),
            &events,
        )
        .await
        .unwrap();
    let events = collect_events(&mut receiver).await;
    assert_eq!(
        events.len(),
        1,
        "management must not emit turn/maintenance events"
    );
    let SessionEvent::ModelsResult { request_id, result } = events.into_iter().next().unwrap()
    else {
        panic!("reasoning result")
    };
    assert_eq!(request_id, "reasoning");
    result
}
fn select(mode: SessionMode, scope: Scope, level: Level, revision: &str) -> Request {
    Request::Select {
        mode,
        scope,
        target: ModelSelection::new(test_profile(), level),
        revision: revision.into(),
    }
}

#[tokio::test]
async fn idle_reasoning_lists_selects_persists_and_reports_unchanged_without_reset() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let settings = Arc::new(Settings::default());
        let (_directory, mut engine) = root(settings.clone());
        let resets = engine.provider.resets;
        let before = std::fs::read(engine.conversation.path()).unwrap();
        assert!(matches!(
            manage(
                &mut engine,
                Request::List {
                    mode: SessionMode::Build,
                    scope
                }
            )
            .await,
            Result::Catalog {
                current: ModelSelection {
                    reasoning_level: Level::Medium,
                    ..
                },
                ..
            }
        ));
        assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), before);
        for (index, expected_unchanged) in [false, true].into_iter().enumerate() {
            let revision = engine.model_management().unwrap().revision.clone();
            assert!(
                matches!(manage(&mut engine, select(SessionMode::Build, scope, Level::High, &revision)).await,
                Result::Changed { role: ModelRole::Build, reasoning_level: Level::High, unchanged, .. } if unchanged == expected_unchanged)
            );
            assert_eq!(
                engine.provider.levels[ModelRole::Build.index()],
                Level::High
            );
            assert_eq!(
                engine.provider.levels[ModelRole::Plan.index()],
                Level::Medium
            );
            let loaded =
                zevria_transcript::transcript::load_report(engine.conversation.path()).unwrap();
            let saved = loaded.session_models().unwrap().unwrap();
            assert_eq!(saved.reasoning_for_mode(SessionMode::Build), Level::High);
            assert_eq!(saved.reasoning_for_mode(SessionMode::Plan), Level::Medium);
            assert_eq!(
                settings.saves.load(Ordering::SeqCst),
                if scope == Scope::SessionOnly {
                    0
                } else {
                    index + 1
                }
            );
        }
        assert_eq!(engine.provider.resets, resets);
        assert!(matches!(
            manage(&mut engine, Request::Cancel).await,
            Result::Cancelled
        ));
    }
}

/// Deliberately over capacity, with both effective opaque context and native
/// reasoning/tool records. Metadata maintenance must not ask whether it fits.
#[tokio::test]
async fn nonempty_over_limit_effort_updates_preserve_native_history_and_identity() {
    use zevria_model::{
        ProviderReplay,
        compaction::{CompactionBackend, CompactionCheckpoint},
        request::OwnedModelRequestItem,
    };
    for mode in [SessionMode::Build, SessionMode::Plan] {
        for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
            let settings = Arc::new(Settings::default());
            let (_directory, mut engine) = root(settings.clone());
            let checkpoint = CompactionCheckpoint::new(
                CompactionTrigger::Manual,
                CompactionBackend::OpenaiResponsesCompact,
                vec![
                    OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(
                        test_profile(),
                        vec![json!({"type":"compaction", "encrypted_content":"opaque-checkpoint"})],
                    ))
                    .unwrap(),
                ],
                vec!["original prompt".into()],
            )
            .unwrap();
            engine
                .commit(TranscriptItem::Compaction(checkpoint))
                .unwrap();
            engine
                .commit(TranscriptItem::Message(Message::user(
                    "large history ".repeat(100_000),
                )))
                .unwrap();
            let replay = ProviderReplay::openai_responses(
                test_profile(),
                vec![
                    json!({"type":"reasoning", "id":"reason-1", "summary":[{"type":"summary_text", "text":"visible thinking"}], "encrypted_content":"encrypted-reasoning", "signature":"native-signature", "future_field":true}),
                    json!({"type":"function_call", "status":"completed", "id":"item-1", "call_id":"call-1", "name":"command", "arguments":"{ \"command\": \"true\" }"}),
                ],
            );
            let Message::Assistant(message) = replay.to_message().unwrap() else {
                unreachable!()
            };
            let call = message
                .content
                .into_iter()
                .find_map(|part| match part {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .unwrap();
            engine
                .commit(TranscriptItem::provider_message(replay).unwrap())
                .unwrap();
            engine
                .commit(TranscriptItem::Message(Message::User {
                    content: vec![rig_core::message::UserContent::ToolResult(
                        rig_core::message::ToolResult {
                            call: call.id,
                            name: call.function.name,
                            is_error: false,
                            content: vec![rig_core::message::ToolResultContent::text(
                                "saved tool output",
                            )],
                        },
                    )],
                }))
                .unwrap();
            let records = serde_json::to_value(&engine.conversation.items()[1..]).unwrap();
            let identity = engine.conversation.input_identity().cloned();
            let input = engine
                .conversation
                .model_input()
                .iter()
                .map(|item| item.to_owned_item().unwrap())
                .collect::<Vec<_>>();
            let contexts = engine.restored_model_contexts();
            let resets = engine.provider.resets;
            engine.provider.forbid_history_inspection = true;
            for unchanged in [false, true] {
                let revision = engine.model_management().unwrap().revision.clone();
                let result = manage(&mut engine, select(mode, scope, Level::High, &revision)).await;
                assert!(
                    matches!(result, Result::Changed { snapshot: None, unchanged: same, .. } if same == unchanged),
                    "{result:?}"
                );
                assert_eq!(
                    serde_json::to_value(&engine.conversation.items()[1..]).unwrap(),
                    records
                );
                assert_eq!(engine.conversation.input_identity(), identity.as_ref());
                assert_eq!(
                    engine
                        .conversation
                        .model_input()
                        .iter()
                        .map(|item| item.to_owned_item().unwrap())
                        .collect::<Vec<_>>(),
                    input
                );
                engine.provider.forbid_history_inspection = false;
                assert_eq!(engine.restored_model_contexts(), contexts);
                engine.provider.forbid_history_inspection = true;
                let loaded =
                    zevria_transcript::transcript::load_report(engine.conversation.path()).unwrap();
                assert_eq!(serde_json::to_value(&loaded.items()[1..]).unwrap(), records);
                let saved = loaded.session_models().unwrap().unwrap();
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
            assert_eq!(engine.provider.resets, resets);
        }
    }
}

#[tokio::test]
async fn unchanged_effort_with_changed_context_installs_policy_and_replacement_snapshot() {
    for mode in SessionMode::ALL {
        for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
            let settings = Arc::new(Settings::default());
            let (_directory, engine) = root(settings);
            let mut engine = engine.with_compaction_policy(test_compaction_policy(90_000, 90, 100));
            let before = engine.conversation.items().to_vec();
            let result = manage(&mut engine, select(mode, scope, Level::Medium, "")).await;
            assert!(
                matches!(result, Result::Changed { unchanged: false, snapshot: Some(ref snapshot), .. } if snapshot.input_token_limit == 100_000),
                "{result:?}"
            );
            assert_eq!(
                engine
                    .context_policy(zevria_model::models::mode_role(mode))
                    .input_token_limit,
                100_000
            );
            assert_eq!(
                engine
                    .context_policy(zevria_model::models::mode_role(
                        if mode == SessionMode::Build {
                            SessionMode::Plan
                        } else {
                            SessionMode::Build
                        }
                    ))
                    .input_token_limit,
                90_000
            );
            assert_eq!(engine.conversation.items(), before);
        }
    }
}

fn seed_native_failure_history(engine: &mut SessionEngine<Provider>) {
    engine
        .commit(TranscriptItem::Message(Message::user("retain this prompt")))
        .unwrap();
    engine.commit(TranscriptItem::provider_message(zevria_model::ProviderReplay::openai_responses(test_profile(), vec![
        json!({"type":"reasoning","id":"saved-reasoning","summary":[{"type":"summary_text","text":"retained thinking"}],"encrypted_content":"saved-cipher","signature":"saved-signature"}),
    ])).unwrap()).unwrap();
    engine.provider.forbid_history_inspection = true;
}

#[tokio::test]
async fn cancelled_effort_selection_never_saves_or_installs_with_nonempty_history() {
    for mode in SessionMode::ALL {
        for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
            let settings = Arc::new(Settings::default());
            let (_directory, mut engine) = root(settings.clone());
            seed_native_failure_history(&mut engine);
            let before = std::fs::read(engine.conversation.path()).unwrap();
            let identity = engine.conversation.input_identity().cloned();
            let cancellation = tokio_util::sync::CancellationToken::new();
            cancellation.cancel();
            let (events, mut receiver) = session_event_channel(8);
            engine
                .manage_models(
                    "cancelled-effort".into(),
                    select(mode, scope, Level::High, ""),
                    &events,
                    &cancellation,
                )
                .await
                .unwrap();
            assert!(matches!(
                collect_events(&mut receiver).await.as_slice(),
                [SessionEvent::ModelsResult {
                    result: Result::Cancelled,
                    ..
                }]
            ));
            assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), before);
            assert_eq!(engine.conversation.input_identity(), identity.as_ref());
            assert_eq!(engine.provider.installs, 0);
            assert_eq!(settings.saves.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn invalid_or_stale_reasoning_never_saves_or_installs() {
    let settings = Arc::new(Settings::default());
    let (_directory, mut engine) = root(settings.clone());
    seed_native_failure_history(&mut engine);
    let before = std::fs::read(engine.conversation.path()).unwrap();
    for request in [
        select(SessionMode::Build, Scope::SessionAndDefault, Level::Max, ""),
        select(SessionMode::Plan, Scope::SessionOnly, Level::High, "stale"),
    ] {
        assert!(matches!(
            manage(&mut engine, request).await,
            Result::Rejected { .. }
        ));
    }
    *settings.revision.lock().unwrap() = "external".into();
    assert!(matches!(
        manage(
            &mut engine,
            Request::List {
                mode: SessionMode::Build,
                scope: Scope::SessionOnly
            }
        )
        .await,
        Result::Rejected { .. }
    ));
    assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), before);
    assert_eq!(engine.provider.installs, 0);
    assert_eq!(settings.saves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn save_failure_precedes_header_and_header_failure_precedes_live_installation() {
    let settings = Arc::new(Settings::default());
    let (_directory, mut engine) = root(settings.clone());
    seed_native_failure_history(&mut engine);
    let path = engine.conversation.path().to_path_buf();
    let before = std::fs::read(&path).unwrap();
    settings.fail.store(true, Ordering::SeqCst);
    assert!(
        matches!(manage(&mut engine, select(SessionMode::Plan, Scope::SessionAndDefault, Level::High, "")).await,
        Result::Rejected { code, current_revision: None, .. } if code == "save_failed")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(engine.provider.installs, 0);
    settings.fail.store(false, Ordering::SeqCst);
    let backup = path.with_extension("backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let result = manage(
            &mut engine,
            select(SessionMode::Plan, scope, Level::High, ""),
        )
        .await;
        let Result::Rejected {
            code,
            current_revision,
            ..
        } = result
        else {
            panic!("{result:?}")
        };
        assert_eq!(code, "session_save_failed");
        assert_eq!(
            current_revision,
            (scope == Scope::SessionAndDefault).then(|| "x".into())
        );
        assert_eq!(engine.provider.installs, 0);
        assert_eq!(
            engine
                .conversation
                .session_models()
                .unwrap()
                .reasoning_for_mode(SessionMode::Plan),
            Level::Medium
        );
        assert_eq!(std::fs::read(&backup).unwrap(), before);
    }
    assert_eq!(engine.model_management().unwrap().revision, "x");
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert!(matches!(
        manage(
            &mut engine,
            select(SessionMode::Plan, Scope::SessionOnly, Level::High, "x")
        )
        .await,
        Result::Changed { .. }
    ));
}

#[tokio::test]
async fn model_change_installs_only_its_explicit_role_selection() {
    let (_directory, mut engine) = root(Arc::new(Settings::default()));
    for mode in [SessionMode::Build, SessionMode::Plan] {
        manage(
            &mut engine,
            select(mode, Scope::SessionOnly, Level::High, ""),
        )
        .await;
    }
    let (events, mut receiver) = session_event_channel(8);
    engine
        .handle_command(
            SessionCommand::Manage(ManagementCommand::Models {
                request_id: "model".into(),
                request: ModelManagementRequest::Select {
                    mode: SessionMode::Build,
                    scope: Scope::SessionOnly,
                    target: ModelSelection::new(
                        ModelProfileRef::new("test-provider", "other"),
                        Level::Medium,
                    ),
                    revision: String::new(),
                },
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(
        collect_events(&mut receiver)
            .await
            .into_iter()
            .any(|event| matches!(
                event,
                SessionEvent::ModelsResult {
                    result: ModelManagementResult::Changed {
                        reasoning_level: Level::Medium,
                        ..
                    },
                    ..
                }
            ))
    );
    let saved = engine.conversation.session_models().unwrap();
    assert_eq!(saved.reasoning_for_mode(SessionMode::Build), Level::Medium);
    assert_eq!(saved.reasoning_for_mode(SessionMode::Plan), Level::High);
    assert_eq!(
        engine.provider.levels[ModelRole::Build.index()],
        Level::Medium
    );
    assert_eq!(engine.provider.levels[ModelRole::Plan.index()], Level::High);
}

#[tokio::test]
async fn busy_reasoning_is_rejected_not_queued() {
    let (_directory, writer) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let (events, receiver) = session_event_channel(32);
    let mut running = RunningSession::start(
        engine(provider, writer),
        [submit("in flight")],
        events,
        receiver,
    );
    let call = running.call(&mut calls).await;
    for request in [
        Request::List {
            mode: SessionMode::Build,
            scope: Scope::SessionOnly,
        },
        select(SessionMode::Build, Scope::SessionOnly, Level::High, ""),
        Request::Cancel,
    ] {
        running.send(SessionCommand::Manage(ManagementCommand::Models {
            request_id: "busy-reasoning".into(),
            request,
        }));
        let event = running
            .until(|event| matches!(event, SessionEvent::ModelsResult { .. }))
            .await;
        assert!(
            matches!(event, SessionEvent::ModelsResult { result: Result::Rejected { code, message, .. }, .. }
            if code == "busy" && message.contains("request was not queued"))
        );
    }
    call.response.send(Ok(Message::assistant("done"))).unwrap();
    running.completed(call.turn.id).await;
    running.send(SessionCommand::Control(ControlCommand::Shutdown));
    running.finish().await;
    assert!(calls.try_recv().is_err());
}
