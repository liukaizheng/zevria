use super::*;
#[path = "model_boundary_tests.rs"]
mod boundaries;
#[path = "model_lifecycle_tests.rs"]
mod lifecycle;
#[path = "reasoning_tests.rs"]
mod reasoning;
use zevria_foundation::ModelProfileRef;
use zevria_model::CompactionBackend;
use zevria_model::CompactionCheckpoint;
use zevria_model::CompactionPolicy;
use zevria_model::CompactionTrigger;
use zevria_model::OwnedModelRequestItem;
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
use zevria_model::models::ModelSelection;
use zevria_model::models::ModelSelectionScope as Scope;
use zevria_model::models::ModelSettingsService;
use zevria_model::models::ReplayPreflight;
fn selected(profile: ModelProfileRef) -> ModelSelection {
    ModelSelection::new(profile, zevria_foundation::ReasoningLevel::Medium)
}

fn profile(name: &str, url: &str) -> ResolvedModelProfile {
    let mut profile = resolved_profile(
        "catalog",
        name,
        url.into(),
        "test-secret",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        100_000,
    );
    profile.endpoint.input_token_count.enabled = false;
    profile
}

fn ledger(name: &str, handle: &str) -> ProviderReplay {
    ProviderReplay::openai_responses(
        ModelProfileRef::new("catalog", name),
        vec![
            json!({"type":"message","role":"assistant","id":format!("msg_{handle}"),"status":"completed","unknown":true,"content":[{"type":"output_text","text":"visible","provider_metadata":"private-meta"}]}),
            json!({"type":"reasoning","id":format!("reason_{handle}"),"summary":[],"encrypted_content":"private-cipher","signature":"private-signature"}),
            function_call(
                &format!("item_{handle}"),
                handle,
                "command",
                json!({"command":"never execute this history"}),
            ),
        ],
    )
}

fn tool_result(replay: &ProviderReplay) -> Message {
    let Message::Assistant { content, .. } = replay.to_message().unwrap() else {
        unreachable!()
    };
    let call = content
        .into_iter()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap();
    Message::User {
        content: vec![rig_core::message::UserContent::ToolResult(
            rig_core::message::ToolResult {
                call: call.id,
                provider: call.provider,
                name: call.function.name,
                content: vec![rig_core::message::ToolResultContent::text(
                    "completed output",
                )],
            },
        )],
    }
}

#[test]
fn replay_matrix_preserves_native_and_strips_foreign_and_legacy_hazards() {
    let replay = ledger("A", "native-handle");
    let trusted = zevria_model::ReplayMessage::new(replay.clone()).unwrap();
    let canonical = trusted.message();
    let result = tool_result(&replay);
    let input = vec![
        ModelRequestItem::replay_backed(&trusted),
        ModelRequestItem::message(&result),
    ];
    let native = zevria_responses::replay::project(&input, &replay.source_profile)
        .unwrap()
        .0;
    assert_eq!(&native[..replay.items.len()], replay.items.as_slice());
    for target in [
        ModelProfileRef::new("catalog", "B"),
        ModelProfileRef::new("other", "A"),
    ] {
        let portable = zevria_responses::replay::project(&input, &target)
            .unwrap()
            .0;
        let wire = serde_json::to_string(&portable).unwrap();
        for forbidden in [
            "private-meta",
            "private-cipher",
            "private-signature",
            "msg_native",
            "reason_native",
            "item_native",
            "native-handle",
        ] {
            assert!(!wire.contains(forbidden), "{forbidden}: {wire}");
        }
        assert!(wire.contains("visible") && wire.contains("completed output"));
        assert_eq!(portable[1]["call_id"], portable[2]["call_id"]);
        assert_eq!(
            portable,
            zevria_responses::replay::project(&input, &target)
                .unwrap()
                .0
        );
    }
    let legacy = zevria_responses::replay::project(
        &[
            ModelRequestItem::message(canonical),
            ModelRequestItem::message(&result),
        ],
        &replay.source_profile,
    )
    .unwrap()
    .0;
    let wire = serde_json::to_string(&legacy).unwrap();
    assert!(!wire.contains("private-") && !wire.contains("native-handle"));
    assert_eq!(
        native,
        zevria_responses::replay::project(&input, &replay.source_profile)
            .unwrap()
            .0
    );
}

#[test]
fn mixed_native_after_foreign_keeps_every_completed_function_result() {
    let a = ledger("A", "a-call");
    let b = ledger("B", "b-call");
    let am = zevria_model::ReplayMessage::new(a.clone()).unwrap();
    let bm = zevria_model::ReplayMessage::new(b.clone()).unwrap();
    let ar = tool_result(&a);
    let br = tool_result(&b);
    let input = [
        ModelRequestItem::replay_backed(&am),
        ModelRequestItem::message(&ar),
        ModelRequestItem::replay_backed(&bm),
        ModelRequestItem::message(&br),
    ];
    for target in [
        &a.source_profile,
        &b.source_profile,
        &ModelProfileRef::new("third", "C"),
    ] {
        let projected = zevria_responses::replay::project(&input, target).unwrap().0;
        let calls = projected
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["call_id"].clone())
            .collect::<Vec<_>>();
        let results = projected
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["call_id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(calls, results);
        assert_eq!(results.len(), 2);
    }
}

#[test]
fn reused_handles_are_occurrence_bound_and_ambiguous_results_fail() {
    let a = ledger("A", "reused");
    let mut b = ledger("A", "reused");
    b.items[2]["id"] = json!("new-logical");
    let am = zevria_model::ReplayMessage::new(a.clone()).unwrap();
    let bm = zevria_model::ReplayMessage::new(b.clone()).unwrap();
    let ar = tool_result(&a);
    let br = tool_result(&b);
    let target = ModelProfileRef::new("catalog", "B");
    assert!(
        zevria_responses::replay::project(
            &[
                ModelRequestItem::replay_backed(&am),
                ModelRequestItem::replay_backed(&bm)
            ],
            &target
        )
        .is_err()
    );
    let mut input = vec![
        ModelRequestItem::replay_backed(&am),
        ModelRequestItem::message(&ar),
        ModelRequestItem::replay_backed(&bm),
    ];
    input.push(ModelRequestItem::message(&ar));
    assert!(
        zevria_responses::replay::project(&input, &target).is_err(),
        "old logical/new provider ownership is ambiguous"
    );
    input.pop();
    input.push(ModelRequestItem::message(&br));
    let projected = zevria_responses::replay::project(&input, &target)
        .unwrap()
        .0;
    let calls = projected
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect::<Vec<_>>();
    assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
}

#[test]
fn omitted_unsupported_calls_are_not_confused_with_missing_function_results() {
    let replay = ProviderReplay::openai_responses(
        ModelProfileRef::new("catalog", "A"),
        vec![
            json!({"type":"custom_tool_call","id":"custom","call_id":"custom-call","name":"unsupported","input":"data"}),
            // A replay-backed item needs a real canonical assistant view. The
            // portable projection drops this provider-bound reasoning; never
            // manufacture an unrelated display-only message beside the ledger.
            json!({"type":"reasoning","id":"reason-custom","summary":[{"type":"summary_text","text":"display only"}]}),
        ],
    );
    let result = Message::User {
        content: vec![rig_core::message::UserContent::ToolResult(
            rig_core::message::ToolResult {
                call: rig_core::message::ToolCallId::new("custom-call").unwrap(),
                provider: None,
                name: "unsupported".into(),
                content: vec![rig_core::message::ToolResultContent::text("omitted output")],
            },
        )],
    };
    let message = zevria_model::ReplayMessage::new(replay).unwrap();
    let prompt = Message::user("preserved user");
    let target = ModelProfileRef::new("catalog", "B");
    let input = [
        ModelRequestItem::replay_backed(&message),
        ModelRequestItem::message(&result),
        ModelRequestItem::message(&prompt),
    ];
    let projected = zevria_responses::replay::project(&input, &target)
        .unwrap()
        .0;
    assert_eq!(projected.len(), 1);
    assert!(
        serde_json::to_string(&projected)
            .unwrap()
            .contains("preserved user")
    );
    assert!(zevria_responses::replay::project(&input[1..], &target).is_err());
}

#[tokio::test]
async fn malformed_and_opaque_input_reject_before_lazy_connection() {
    let a = profile("A", "http://127.0.0.1:1/v1/responses");
    let mut router = ResponsesRouter::from_routes(
        [(
            ModelRole::Build,
            a.clone(),
            zevria_foundation::ReasoningLevel::Medium,
        )],
        "preamble",
        ToolServer::new().run(),
        "root",
    )
    .unwrap();
    for replay in [
        ProviderReplay::openai_responses(
            ModelProfileRef::new("catalog", "B"),
            vec![json!({"type":"compaction","encrypted_content":"secret"})],
        ),
        ProviderReplay::openai_responses(
            a.profile.clone(),
            vec![json!({"type":"function_call","name":"bad"})],
        ),
    ] {
        let prompt = Message::user("continue");
        let request = ModelRequest {
            instructions: test_instructions(),
            input: vec![
                ModelRequestItem::replay_only(&replay),
                ModelRequestItem::message(&prompt),
            ],
            model_role: ModelRole::Build,
            allowed_tool_names: Some(&[]),
        };
        assert!(
            router
                .complete(request.clone(), discard_updates())
                .await
                .is_err()
        );
        assert!(router.count_input_tokens(request).await.is_err());
        assert_eq!(router.initialized_profile_count(), 0);
    }
    let mut unsupported = ledger("A", "call");
    unsupported.version = 99;
    assert!(
        zevria_responses::replay::preflight(
            &[ModelRequestItem::replay_only(&unsupported)],
            &a.profile
        )
        .is_err()
    );
}

#[derive(Default)]
struct Settings {
    revision: Mutex<String>,
    fail: std::sync::atomic::AtomicBool,
    save_calls: std::sync::atomic::AtomicUsize,
}
impl ModelSettingsService for Settings {
    fn validate(&self, expected: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            *self.revision.lock().unwrap() == expected,
            "configuration conflict"
        );
        Ok(())
    }
    fn save(
        &self,
        expected: &str,
        _role: ModelRole,
        _target: &ModelSelection,
    ) -> anyhow::Result<String> {
        self.save_calls.fetch_add(1, Ordering::Relaxed);
        self.validate(expected)?;
        anyhow::ensure!(!self.fail.load(Ordering::Relaxed), "injected save failure");
        let mut revision = self.revision.lock().unwrap();
        revision.push('x');
        Ok(revision.clone())
    }
}

fn engine(
    a: ResolvedModelProfile,
    b: ResolvedModelProfile,
    history: Vec<TranscriptItem>,
    settings: Arc<Settings>,
) -> (tempfile::TempDir, SessionEngine<ResponsesRouter>) {
    engine_with_access(a, b, history, settings, false)
}

fn engine_with_access(
    a: ResolvedModelProfile,
    b: ResolvedModelProfile,
    mut history: Vec<TranscriptItem>,
    settings: Arc<Settings>,
    read_only: bool,
) -> (tempfile::TempDir, SessionEngine<ResponsesRouter>) {
    history.insert(
        0,
        TranscriptItem::SessionModels(
            zevria_model::models::SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    a.profile.clone(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    b.profile.clone(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        ),
    );
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create_with_id(directory.path(), "model-test").unwrap();
    if !history.is_empty() {
        writer.rewrite(&history).unwrap();
    }
    if read_only {
        let path = writer.path().to_path_buf();
        drop(writer);
        writer = TranscriptWriter::read_only(path).unwrap();
    }
    let tools = ToolServer::new().run();
    let router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Plan,
                b.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Review,
                a.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        tools.clone(),
        "root",
    )
    .unwrap();
    let compaction = CompactionPolicy::new(
        Default::default(),
        [
            a.context_policy(),
            b.context_policy(),
            a.context_policy(),
            a.context_policy(),
            b.context_policy(),
        ],
    )
    .unwrap();
    let policies = SessionPolicies::new(
        TurnPolicy::new("Build", Some(vec![]), ModelRole::Build, false),
        TurnPolicy::new("Plan", Some(vec![]), ModelRole::Plan, false),
    );
    let engine = SessionEngine::new(
        router,
        tools,
        policies,
        writer,
        Arc::new(SkillCatalog::new(vec![]).unwrap()),
    )
    .unwrap()
    .with_transcript_items(history)
    .unwrap()
    .with_compaction_policy(compaction)
    .with_model_management(settings, String::new());
    (directory, engine)
}

async fn manage(engine: &mut SessionEngine<ResponsesRouter>, request: Request) -> Result {
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request_id: "selection".into(),
                request,
            }),
            &events,
        )
        .await
        .unwrap();
    if let Ok(SessionUpdate::Lifecycle(event)) = receiver.try_recv() {
        if let SessionEvent::ModelsResult { request_id, result } = event {
            assert_eq!(request_id, "selection");
            return result;
        }
        panic!("management emitted a conversation event: {event:?}");
    }
    panic!("missing authoritative result")
}

#[tokio::test]
async fn idle_selection_is_no_inference_only_chosen_route_and_unknown_noop_are_safe() {
    let a = profile("A", "http://127.0.0.1:1/v1/responses");
    let b = profile("B", &a.endpoint.base_url);
    let (_directory, mut engine) =
        engine(a.clone(), b.clone(), vec![], Arc::new(Settings::default()));
    assert!(matches!(
        manage(
            &mut engine,
            Request::Select {
                scope: Scope::SessionAndDefault,
                mode: SessionMode::Build,
                target: selected(a.profile.clone()),
                revision: String::new()
            }
        )
        .await,
        Result::Changed {
            unchanged: true,
            ..
        }
    ));
    assert!(matches!(
        manage(
            &mut engine,
            Request::Select {
                scope: Scope::SessionAndDefault,
                mode: SessionMode::Build,
                target: selected(ModelProfileRef::new("unknown", "unknown")),
                revision: String::new()
            }
        )
        .await,
        Result::Rejected { .. }
    ));
    assert!(matches!(
        manage(
            &mut engine,
            Request::Select {
                scope: Scope::SessionAndDefault,
                mode: SessionMode::Build,
                target: selected(b.profile.clone()),
                revision: "x".into()
            }
        )
        .await,
        Result::Changed {
            role: ModelRole::Build,
            unchanged: false,
            ..
        }
    ));
    assert_eq!(engine.provider().initialized_profile_count(), 0);
    assert_eq!(engine.conversation().items().len(), 1);
    assert_eq!(
        engine
            .conversation()
            .session_models()
            .unwrap()
            .for_mode(SessionMode::Build)
            .profile,
        b.profile
    );
    assert!(
        matches!(manage(&mut engine,Request::List { scope: Scope::SessionAndDefault, mode:SessionMode::Plan }).await, Result::Catalog { current, .. } if current.profile == b.profile)
    );
    assert!(
        matches!(manage(&mut engine,Request::List { scope: Scope::SessionAndDefault, mode:SessionMode::Build }).await, Result::Catalog { current, .. } if current.profile == b.profile)
    );
}

#[tokio::test]
async fn session_only_switches_are_role_local_noops_do_not_write_and_global_can_promote() {
    for mode in [SessionMode::Build, SessionMode::Plan] {
        let a = profile("A", "http://127.0.0.1:1/v1/responses");
        let b = profile("B", &a.endpoint.base_url);
        let target = if mode == SessionMode::Build {
            b.profile.clone()
        } else {
            a.profile.clone()
        };
        let settings = Arc::new(Settings::default());
        settings.fail.store(true, Ordering::Relaxed);
        let history = vec![TranscriptItem::Message(Message::user(
            "established conversation",
        ))];
        let (_directory, mut engine) =
            engine(a.clone(), b.clone(), history.clone(), settings.clone());
        let path = engine.conversation().path().to_path_buf();
        let original = std::fs::read(&path).unwrap();
        for (bad_target, revision) in [
            (ModelProfileRef::new("unknown", "unknown"), String::new()),
            (target.clone(), "stale".into()),
        ] {
            assert!(matches!(
                manage(
                    &mut engine,
                    Request::Select {
                        scope: Scope::SessionOnly,
                        mode,
                        target: selected(bad_target),
                        revision
                    }
                )
                .await,
                Result::Rejected { .. }
            ));
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        for unchanged in [false, true] {
            let bytes = std::fs::read(&path).unwrap();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let result = manage(
                &mut engine,
                Request::Select {
                    scope: Scope::SessionOnly,
                    mode,
                    target: selected(target.clone()),
                    revision: String::new(),
                },
            )
            .await;
            assert!(
                matches!(result, Result::Changed { scope: Scope::SessionOnly, role, context, revision, unchanged: actual, .. } if role == zevria_model::models::mode_role(mode) && context.profile == target && revision.is_empty() && actual == unchanged)
            );
            if unchanged {
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert_eq!(
                    std::fs::metadata(&path).unwrap().modified().unwrap(),
                    modified
                );
            }
            let models = transcript::load_report(&path)
                .unwrap()
                .session_models()
                .unwrap()
                .unwrap()
                .clone();
            for other in [SessionMode::Build, SessionMode::Plan] {
                let expected = if other == mode {
                    &target
                } else if other == SessionMode::Build {
                    &a.profile
                } else {
                    &b.profile
                };
                assert_eq!(&models.for_mode(other).profile, expected);
                assert!(
                    matches!(manage(&mut engine, Request::List { scope: Scope::SessionOnly, mode: other }).await, Result::Catalog { scope: Scope::SessionOnly, current, revision, .. } if current.profile == *expected && revision.is_empty())
                );
            }
            assert_eq!(&engine.conversation().items()[1..], history);
        }
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
        assert!(settings.revision.lock().unwrap().is_empty());
        assert_eq!(engine.provider().initialized_profile_count(), 0);
        // /model must not short-circuit just because the session already uses it.
        settings.fail.store(false, Ordering::Relaxed);
        assert!(
            matches!(manage(&mut engine, Request::Select { scope: Scope::SessionAndDefault, mode, target: selected(target), revision: String::new() }).await, Result::Changed { scope: Scope::SessionAndDefault, unchanged: true, revision, .. } if revision == "x")
        );
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn session_only_header_failure_without_conversion_preserves_selections_and_retries() {
    let a = profile("A", "http://127.0.0.1:1/v1/responses");
    let b = profile("B", &a.endpoint.base_url);
    let settings = Arc::new(Settings::default());
    settings.fail.store(true, Ordering::Relaxed);
    let (_directory, mut engine) = engine(a.clone(), b.clone(), vec![], settings.clone());
    let path = engine.conversation().path().to_path_buf();
    let bytes = std::fs::read(&path).unwrap();
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    let request = Request::Select {
        scope: Scope::SessionOnly,
        mode: SessionMode::Build,
        target: selected(b.profile.clone()),
        revision: String::new(),
    };
    assert!(
        matches!(manage(&mut engine, request.clone()).await, Result::Rejected {
        code, message, checkpoint_installed: false, current_revision: None,
    } if code == "session_save_failed" && message.contains("config was not modified") && message.contains("Retry /model-session"))
    );
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    assert_eq!(
        &engine
            .conversation()
            .session_models()
            .unwrap()
            .for_mode(SessionMode::Build)
            .profile,
        &a.profile
    );
    assert!(
        matches!(manage(&mut engine, Request::List { scope: Scope::SessionOnly, mode: SessionMode::Build }).await, Result::Catalog { current, revision, .. } if current.profile == a.profile && revision.is_empty())
    );
    assert_eq!(
        manage(&mut engine, Request::Cancel).await,
        Result::Cancelled
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    assert!(matches!(
        manage(&mut engine, request).await,
        Result::Changed {
            scope: Scope::SessionOnly,
            ..
        }
    ));
    assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
    assert!(settings.revision.lock().unwrap().is_empty());
}

fn opaque_history(a: &ResolvedModelProfile) -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::Message(Message::user("keep the original instruction")),
        TranscriptItem::Compaction(
            CompactionCheckpoint::new(
                CompactionTrigger::Manual,
                CompactionBackend::OpenaiResponsesCompact,
                vec![
                    OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(
                        a.profile.clone(),
                        vec![json!({"type":"compaction","encrypted_content":"source-secret"})],
                    ))
                    .unwrap(),
                ],
                vec!["keep the original instruction".into()],
            )
            .unwrap(),
        ),
    ]
}

#[tokio::test]
async fn confirmed_conversion_is_source_only_and_checkpoint_survives_failed_default_save() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = profile(
        "A",
        &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
    );
    let b = profile("B", "http://127.0.0.1:1/v1/responses");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert_eq!(request.body["model"], "A");
        assert!(request.body["tools"].as_array().is_none_or(Vec::is_empty));
        assert!(request.body["input"].to_string().contains("source-secret"));
        assert!(request.body.get("previous_response_id").is_none());
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("summary", "summary-msg", "portable summary").to_string()),
        )
        .await;
    });
    let settings = Arc::new(Settings::default());
    settings.fail.store(true, Ordering::Relaxed);
    let original = opaque_history(&a);
    let (_directory, mut engine) = engine(a.clone(), b.clone(), original.clone(), settings.clone());
    let Result::ConfirmationRequired(preview) = manage(
        &mut engine,
        Request::Select {
            scope: Scope::SessionAndDefault,
            mode: SessionMode::Build,
            target: selected(b.profile.clone()),
            revision: String::new(),
        },
    )
    .await
    else {
        panic!("confirmation required")
    };
    assert_eq!(engine.provider().initialized_profile_count(), 0);
    assert!(preview.reason.contains("shared root context"));
    assert!(matches!(
        manage(&mut engine, Request::Confirm { preview }).await,
        Result::Rejected {
            checkpoint_installed: true,
            ..
        }
    ));
    assert_eq!(
        &engine.conversation().items()[1..original.len() + 1],
        original.as_slice()
    );
    let restored = transcript::load_report(engine.conversation().path()).unwrap();
    restored.ensure_resumable().unwrap();
    assert_eq!(restored.items, engine.conversation().items());
    assert!(
        matches!(manage(&mut engine,Request::List { scope: Scope::SessionAndDefault, mode:SessionMode::Build }).await, Result::Catalog { current, .. } if current.profile == a.profile)
    );
    settings.fail.store(false, Ordering::Relaxed);
    assert!(matches!(
        manage(
            &mut engine,
            Request::Select {
                scope: Scope::SessionAndDefault,
                mode: SessionMode::Build,
                target: selected(b.profile.clone()),
                revision: String::new()
            }
        )
        .await,
        Result::Changed { .. }
    ));
    assert_eq!(
        engine.provider().initialized_profile_count(),
        1,
        "retry must not call destination or summarize again"
    );
    assert!(matches!(
        engine
            .provider()
            .preflight_input(&b.profile, &engine.model_input())
            .unwrap(),
        ReplayPreflight::Compatible(_)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn header_failure_after_checkpoint_preserves_route_and_retries_each_scope() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = profile(
            "A",
            &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
        );
        let b = profile("B", "http://127.0.0.1:1/v1/responses");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(receive_http_json(&mut stream).await.body["model"], "A");
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("summary", "msg", "portable summary").to_string()),
            )
            .await;
        });
        let settings = Arc::new(Settings::default());
        let (_directory, mut engine) =
            engine(a.clone(), b.clone(), opaque_history(&a), settings.clone());
        let Result::ConfirmationRequired(preview) = manage(
            &mut engine,
            Request::Select {
                scope,
                mode: SessionMode::Build,
                target: selected(b.profile.clone()),
                revision: String::new(),
            },
        )
        .await
        else {
            panic!("preview");
        };
        let path = engine.conversation().path().to_path_buf();
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = manage(&mut engine, Request::Confirm { preview }).await;
        let Result::Rejected {
            checkpoint_installed: true,
            current_revision,
            message,
            ..
        } = result
        else {
            panic!("partial save: {result:?}");
        };
        let revision = settings.revision.lock().unwrap().clone();
        assert!(message.contains("Portable checkpoint saved"));
        if scope == Scope::SessionOnly {
            assert!(current_revision.is_none());
            assert!(revision.is_empty());
            assert!(message.contains("config was not modified"));
            assert!(message.contains("Retry /model-session"));
            assert!(message.contains("without another summary call"));
            assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
        } else {
            assert_eq!(current_revision, Some(revision.clone()));
            assert!(message.contains("global default was saved"));
        }
        assert_eq!(
            &engine
                .conversation()
                .session_models()
                .unwrap()
                .for_mode(SessionMode::Build)
                .profile,
            &a.profile
        );
        assert_eq!(
            transcript::load(&saved).unwrap(),
            engine.conversation().items()
        );
        assert!(
            matches!(manage(&mut engine, Request::List { scope: Scope::SessionAndDefault, mode: SessionMode::Build }).await, Result::Catalog { current, .. } if current.profile == a.profile)
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&saved, &path).unwrap();
        assert!(matches!(
            manage(
                &mut engine,
                Request::Select {
                    scope,
                    mode: SessionMode::Build,
                    target: selected(b.profile.clone()),
                    revision
                }
            )
            .await,
            Result::Changed { .. }
        ));
        assert_eq!(
            engine.provider().initialized_profile_count(),
            1,
            "retry reuses checkpoint"
        );
        let durable = transcript::load_report(&path).unwrap();
        let models = durable.session_models().unwrap().unwrap();
        assert_eq!(&models.for_mode(SessionMode::Build).profile, &b.profile);
        assert_eq!(&models.for_mode(SessionMode::Plan).profile, &b.profile);
        assert_eq!(
            settings.save_calls.load(Ordering::Relaxed),
            if scope == Scope::SessionOnly { 0 } else { 2 }
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn stale_confirmation_and_cancel_do_not_mutate_or_call_models() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let a = profile("A", "http://127.0.0.1:1/v1/responses");
        let b = profile("B", &a.endpoint.base_url);
        let settings = Arc::new(Settings::default());
        let original = opaque_history(&a);
        let (_directory, mut engine) = engine(a, b.clone(), original.clone(), settings.clone());
        let Result::ConfirmationRequired(preview) = manage(
            &mut engine,
            Request::Select {
                scope,
                mode: SessionMode::Build,
                target: selected(b.profile),
                revision: String::new(),
            },
        )
        .await
        else {
            panic!("preview")
        };
        assert_eq!(preview.scope, scope);
        let mut stale = preview.clone();
        stale.generation += 1;
        let mut altered_scope = preview.clone();
        altered_scope.scope = if scope == Scope::SessionOnly {
            Scope::SessionAndDefault
        } else {
            Scope::SessionOnly
        };
        let mut stale_runtime = preview.clone();
        stale_runtime.session_generation.push_str("-reopened");
        let mut altered_target_level = preview.clone();
        altered_target_level.target.reasoning_level = zevria_foundation::ReasoningLevel::High;
        let mut altered_source_level = preview.clone();
        altered_source_level.source.reasoning_level = zevria_foundation::ReasoningLevel::Low;
        for invalid in [
            stale,
            altered_scope,
            stale_runtime,
            altered_target_level,
            altered_source_level,
        ] {
            assert!(matches!(
                manage(&mut engine, Request::Confirm { preview: invalid }).await,
                Result::Rejected { message, .. } if message.ends_with(scope.command())
            ));
        }
        // A cancellation from another picker must not consume this preview.
        let (events, _) = session_event_channel(8);
        engine
            .handle_command(
                SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                    request_id: "other-picker".into(),
                    request: Request::Cancel,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(matches!(
            manage(
                &mut engine,
                Request::List {
                    scope,
                    mode: SessionMode::Build
                }
            )
            .await,
            Result::Rejected { .. }
        ));
        assert_eq!(
            manage(&mut engine, Request::Cancel).await,
            Result::Cancelled
        );
        assert!(matches!(
            manage(&mut engine, Request::Confirm { preview }).await,
            Result::Rejected { .. }
        ));
        assert_eq!(&engine.conversation().items()[1..], original);
        assert_eq!(engine.provider().initialized_profile_count(), 0);
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
    }
}
