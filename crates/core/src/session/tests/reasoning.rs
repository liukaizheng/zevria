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
                profile,
                context_window_tokens: 100_000,
                input_token_limit: 100_000,
                retained_user_tokens: 100,
            },
            reasoning_levels: vec![Level::Low, Level::Medium, Level::High],
        })
        .collect()
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

#[tokio::test]
async fn invalid_or_stale_reasoning_never_saves_or_installs() {
    let settings = Arc::new(Settings::default());
    let (_directory, mut engine) = root(settings.clone());
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
