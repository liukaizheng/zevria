#[path = "session_history_tests.rs"]
mod history_tests;

#[path = "session_mode_tests.rs"]
mod mode_tests;
#[path = "session_reasoning_tests.rs"]
mod reasoning_tests;

use std::path::{Path, PathBuf};
use zevria_app::{
    Config,
    test_support::{load_config as load, model_revision as revision},
};
use zevria_foundation::{ModelProfileRef, ModelRole};

fn source() -> String {
    r#"# preserved comment
[providers.p]
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "untouched-secret"
supports_websockets = false
[providers.p.models."old"]
context_window_tokens = 100000
retained_user_tokens = 1000
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[providers.p.models."new/with:separators"]
context_window_tokens = 50000
input_token_limit = 40000
retained_user_tokens = 1000
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[modes]
build = { provider = "p", model = "old", reasoning_level = "medium" } # build comment
plan = { provider = "p", model = "old", reasoning_level = "medium" } # plan untouched
review = { provider = "p", model = "old", reasoning_level = "medium" }
explore = { provider = "p", model = "old", reasoning_level = "medium" }
builder = { provider = "p", model = "old", reasoning_level = "medium" }
"#
    .into()
}

use crate::runtime;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::mpsc,
};
use zevria_foundation::SessionMode;
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
use zevria_model::models::ModelSelectionScope as Scope;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionEventReceiver;
use zevria_session_api::SessionUpdate;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

struct Server {
    url: String,
    requests: mpsc::UnboundedReceiver<(String, Value)>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let (send, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut sequence = 0;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let boundary = loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break index + 4;
                    }
                    assert!(bytes.len() < 65536);
                };
                let headers = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse().unwrap())
                    })
                    .unwrap();
                while bytes.len() < boundary + size {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[boundary..boundary + size]).unwrap();
                send.send((headers, body.clone())).unwrap();
                sequence += 1;
                let event = json!({"type":"response.completed","sequence_number":1,"response":{
                    "id":format!("resp-{sequence}"),"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"max_output_tokens":null,"model":body["model"],
                    "usage":{"input_tokens":1200,"input_tokens_details":{"cached_tokens":1000},"output_tokens":40,"total_tokens":1240},
                    "output":[{"type":"message","id":format!("msg-{sequence}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]}],"tools":[]}});
                let response = format!("data: {event}\n\n");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    async fn expect(&mut self, provider: &str, model: &str) -> Value {
        self.expect_with_headers(provider, model).await.1
    }
    async fn expect_with_headers(&mut self, provider: &str, model: &str) -> (String, Value) {
        let (headers, request) =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.requests.recv())
                .await
                .unwrap()
                .unwrap();
        let path = headers.lines().next().unwrap();
        assert!(
            path.contains(if provider == "q" {
                "/q/responses"
            } else {
                "/v1/responses"
            }),
            "{path}"
        );
        assert_eq!(request["model"], model);
        (headers, request)
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    path: PathBuf,
    models_path: PathBuf,
    config: Config,
}
impl Fixture {
    fn new(url: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-captured.toml");
        let source = source().replace("http://127.0.0.1:1/v1/responses", url);
        let extra = source
            .split("[modes]")
            .next()
            .unwrap()
            .replace("providers.p", "providers.q")
            .replace(url, &url.replace("/v1/responses", "/q/responses"));
        let source = source.replace(
            "plan = { provider = \"p\", model = \"old\", reasoning_level = \"medium\" }",
            "plan = { provider = \"p\", model = \"new/with:separators\", reasoning_level = \"low\" }",
        );
        let config =
            zevria_app::test_support::write_fixture(&path, &format!("{source}\n{extra}")).unwrap();
        let models_path = zevria_foundation::config::models_path_for(&path);
        Self {
            directory,
            path,
            models_path,
            config,
        }
    }
    async fn start(&self, start: runtime::SessionStart) -> runtime::RunningSession {
        runtime::start_session(&self.config, self.directory.path(), start)
            .await
            .unwrap()
    }
    fn change_globals(&self) -> Vec<u8> {
        let mut document: Value =
            serde_json::from_str(&std::fs::read_to_string(&self.models_path).unwrap()).unwrap();
        let mut assignments: toml::Table =
            toml::from_str(&std::fs::read_to_string(&self.path).unwrap()).unwrap();
        for (role, provider, model) in [
            ("build", "p", "new/with:separators"),
            ("plan", "p", "old"),
            ("review", "q", "old"),
            ("explore", "q", "new/with:separators"),
            ("builder", "q", "old"),
        ] {
            assignments["modes"][role]["provider"] = provider.into();
            assignments["modes"][role]["model"] = model.into();
        }
        std::fs::write(&self.path, toml::to_string(&assignments).unwrap()).unwrap();
        document["providers"]["p"]["models"]["old"]["context_window_tokens"] = json!(88000);
        document["providers"]["p"]["models"]["old"]["input_token_limit"] = json!(80000);
        std::fs::write(
            &self.models_path,
            serde_json::to_string_pretty(&document).unwrap(),
        )
        .unwrap();
        std::fs::read(&self.models_path).unwrap()
    }
}

fn plan_handoff(source_session_id: &str) -> zevria_workflow::PlanHandoff {
    zevria_workflow::PlanHandoff::new(
        zevria_workflow::PlanArtifact {
            version: zevria_workflow::PlanVersion {
                id: zevria_workflow::PlanId::new(),
                revision: 1,
            },
            title: "Implement approved test plan".into(),
            markdown: "# Implement approved test plan\n\n## Goal\nDone\n\n## Decisions\nDone\n\n## Implementation\nDone\n\n## Validation\nDone\n\n## Risks\nNone".into(),
            source_turn_id: zevria_foundation::TurnId::new(1),
        },
        source_session_id,
    )
}

fn assert_persisted_handoff(path: &Path, expected: &zevria_workflow::PlanHandoff) {
    let items = transcript::load(path).unwrap();
    assert!(matches!(
        items.as_slice(),
        [
            TranscriptItem::SessionModels(_),
            TranscriptItem::SessionMode(SessionMode::Build),
            TranscriptItem::Plan(zevria_workflow::PlanRecord::Handoff { handoff }),
            ..
        ] if handoff == expected
    ));
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(
                item,
                TranscriptItem::Plan(zevria_workflow::PlanRecord::Handoff { .. })
            ))
            .count(),
        1,
        "exactly one typed opening handoff must be persisted"
    );
}

async fn event(receiver: &mut SessionEventReceiver) -> SessionEvent {
    loop {
        if let SessionUpdate::Lifecycle(event) =
            tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
                .await
                .unwrap()
                .unwrap()
        {
            return event;
        }
    }
}
async fn completed(
    receiver: &mut SessionEventReceiver,
    profile: &ModelProfileRef,
    mode: SessionMode,
    limit: u64,
) {
    let mut usage = false;
    loop {
        match event(receiver).await {
            SessionEvent::UsageUpdated {
                profile: actual,
                model_role,
                input_token_limit,
                ..
            } => {
                assert_eq!(actual, *profile);
                assert_eq!(model_role, zevria_model::models::mode_role(mode));
                assert_eq!(input_token_limit, limit);
                usage = true;
            }
            SessionEvent::TurnCompleted { .. } => {
                assert!(usage);
                return;
            }
            SessionEvent::TurnFailed { error, .. } => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}
async fn select(
    commands: &mpsc::UnboundedSender<SessionCommand>,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    target: ModelProfileRef,
    revision: String,
) -> Result {
    select_scoped(
        commands,
        events,
        mode,
        target,
        revision,
        Scope::SessionAndDefault,
    )
    .await
}

async fn select_scoped(
    commands: &mpsc::UnboundedSender<SessionCommand>,
    events: &mut SessionEventReceiver,
    mode: SessionMode,
    target: ModelProfileRef,
    revision: String,
    scope: Scope,
) -> Result {
    commands
        .send(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Models {
                request_id: "selection".into(),
                request: Request::Select {
                    scope,
                    mode,
                    target: zevria_model::models::ModelSelection::new(
                        target,
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    revision,
                },
            },
        ))
        .unwrap();
    loop {
        if let SessionEvent::ModelsResult { request_id, result } = event(events).await {
            assert_eq!(request_id, "selection");
            return result;
        }
    }
}

#[tokio::test]
async fn session_header_follows_persistent_identity_across_modes_resume_and_fresh_roots() {
    let mut server = Server::new().await;
    let mut fixture = Fixture::new(&server.url);
    let mut document: Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture.models_path).unwrap()).unwrap();
    document["providers"]["p"]["session_id_header"] = json!("X-Root-Conversation");
    std::fs::write(
        &fixture.models_path,
        serde_json::to_string_pretty(&document).unwrap(),
    )
    .unwrap();
    fixture.config = load(&fixture.path).unwrap();
    let config_bytes = std::fs::read(&fixture.models_path).unwrap();
    let mut original_path: Option<PathBuf> = None;
    let mut original_id = String::new();
    let mut original_cache_keys = Vec::new();
    for iteration in 0..3 {
        let mut running = fixture
            .start(if iteration == 1 {
                runtime::SessionStart::Resume(original_path.clone().unwrap())
            } else {
                runtime::SessionStart::New
            })
            .await;
        let id = running.restoration().session_id.clone();
        let path = running.restoration().transcript_path.clone();
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), id);
        if iteration == 0 {
            original_path = Some(path.clone());
            original_id.clone_from(&id);
        } else if iteration == 1 {
            assert_eq!(id, original_id, "resume must reuse the transcript identity");
        } else {
            assert_ne!(
                id, original_id,
                "a fresh root needs an independent identity"
            );
        }
        let mut events = running.take_event_receiver().unwrap();
        let mut cache_keys = Vec::new();
        for (mode, model, limit) in [
            (SessionMode::Build, "old", 100000),
            (SessionMode::Plan, "new/with:separators", 40000),
        ] {
            running
                .command_sender()
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: "no-tools diagnostic".into(),
                        mode,
                    },
                ))
                .unwrap();
            completed(&mut events, &ModelProfileRef::new("p", model), mode, limit).await;
            let (headers, request) = server.expect_with_headers("p", model).await;
            let values = headers
                .lines()
                .filter_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("x-root-conversation")
                        .then(|| value.trim())
                })
                .collect::<Vec<_>>();
            assert_eq!(values, vec![id.as_str()]);
            assert!(!headers.to_ascii_lowercase().contains("x-opencode-session:"));
            cache_keys.push(request["prompt_cache_key"].as_str().unwrap().to_string());
        }
        assert_ne!(
            cache_keys[0], cache_keys[1],
            "cache identity remains profile-specific"
        );
        match iteration {
            0 => original_cache_keys = cache_keys,
            1 => assert_eq!(cache_keys, original_cache_keys),
            _ => {
                assert_ne!(cache_keys[0], original_cache_keys[0]);
                assert_ne!(cache_keys[1], original_cache_keys[1]);
            }
        }
        running.shutdown().await.unwrap();
        let transcript = std::fs::read_to_string(path).unwrap();
        assert!(!transcript.contains("session_id_header"));
        assert!(!transcript.contains("X-Root-Conversation"));
        assert_eq!(std::fs::read(&fixture.models_path).unwrap(), config_bytes);
    }
}

#[tokio::test]
async fn resumed_roots_pin_build_plan_but_refresh_other_roles_and_current_limits() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let a = ModelProfileRef::new("p", "old");
    let b = ModelProfileRef::new("p", "new/with:separators");
    let mut first = fixture.start(runtime::SessionStart::New).await;
    let path = first.restoration().transcript_path.clone();
    let mut events = first.take_event_receiver().unwrap();
    first
        .command_sender()
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "retained prompt".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    completed(&mut events, &a, SessionMode::Build, 100000).await;
    server.expect("p", "old").await;
    first.shutdown().await.unwrap();
    let originals = transcript::load(&path).unwrap();
    let original_replay = originals
        .iter()
        .find_map(TranscriptItem::provider_replay)
        .unwrap()
        .clone();
    let config_bytes = fixture.change_globals();
    for iteration in 0..2 {
        let mut running = fixture
            .start(runtime::SessionStart::Resume(path.clone()))
            .await;
        let contexts = &running.restoration().model_contexts;
        assert_eq!(contexts[ModelRole::Build.index()].profile, a);
        assert_eq!(contexts[ModelRole::Plan.index()].profile, b);
        assert_eq!(
            contexts[ModelRole::Review.index()].profile,
            ModelProfileRef::new("q", "old")
        );
        assert_eq!(
            contexts[ModelRole::Explore.index()].profile,
            ModelProfileRef::new("q", "new/with:separators")
        );
        assert_eq!(
            contexts[ModelRole::Builder.index()].profile,
            ModelProfileRef::new("q", "old")
        );
        assert_eq!(contexts[ModelRole::Build.index()].input_token_limit, 80000);
        assert_eq!(
            contexts[ModelRole::Build.index()].context_window_tokens,
            88000
        );
        for snapshot in &running.restoration().model_snapshots {
            assert_eq!(
                snapshot.profile,
                contexts[snapshot.model_role.index()].profile
            );
            assert_eq!(
                snapshot.input_token_limit,
                contexts[snapshot.model_role.index()].input_token_limit
            );
        }
        assert_eq!(running.restoration().model_snapshots.len(), 2);
        assert!(
            server.requests.try_recv().is_err(),
            "restoration must not call/count/convert"
        );
        if iteration == 0 {
            let mut events = running.take_event_receiver().unwrap();
            for (mode, profile, limit) in [
                (SessionMode::Build, &a, 80000),
                (SessionMode::Plan, &b, 40000),
            ] {
                running
                    .command_sender()
                    .send(SessionCommand::Turn(
                        zevria_session_api::TurnCommand::Submit {
                            behavior: zevria_foundation::RequestBehavior::Standard,
                            text: "continue".into(),
                            mode,
                        },
                    ))
                    .unwrap();
                completed(&mut events, profile, mode, limit).await;
                server.expect(&profile.provider, &profile.model).await;
            }
        }
        running.shutdown().await.unwrap();
        assert_eq!(std::fs::read(&fixture.models_path).unwrap(), config_bytes);
        assert_eq!(
            transcript::load(&path)
                .unwrap()
                .iter()
                .find_map(TranscriptItem::provider_replay)
                .unwrap(),
            &original_replay
        );
    }
    let fresh = fixture.start(runtime::SessionStart::New).await;
    assert_eq!(
        fresh.restoration().model_contexts[ModelRole::Build.index()].profile,
        b
    );
    assert_eq!(
        fresh.restoration().model_contexts[ModelRole::Plan.index()].profile,
        a
    );
    let fresh_path = fresh.restoration().transcript_path.clone();
    let fresh_bytes = std::fs::read(&fresh_path).unwrap();
    assert_eq!(fresh.restoration().selected_mode, SessionMode::Build);
    assert!(!transcript::is_abandoned_root(&fresh_path));
    fresh.shutdown().await.unwrap();
    assert_eq!(std::fs::read(&fresh_path).unwrap(), fresh_bytes);
    let source_session_id = path.file_stem().unwrap().to_str().unwrap();
    let source_before = std::fs::read(&path).unwrap();
    let opening_handoff = plan_handoff(source_session_id);
    let mut handoff = fixture
        .start(runtime::SessionStart::FromPlan(opening_handoff.clone()))
        .await;
    let handoff_path = handoff.restoration().transcript_path.clone();
    assert_ne!(handoff.restoration().session_id, source_session_id);
    assert_ne!(handoff_path, path);
    assert_eq!(
        handoff_path.file_stem().unwrap().to_str().unwrap(),
        handoff.restoration().session_id
    );
    assert!(matches!(
        handoff.restoration().transcript_items.as_slice(),
        [
            TranscriptItem::SessionModels(_),
            TranscriptItem::SessionMode(SessionMode::Build)
        ]
    ));
    assert_eq!(
        handoff.restoration().model_contexts[ModelRole::Build.index()].profile,
        b
    );
    assert_eq!(
        handoff.restoration().model_contexts[ModelRole::Plan.index()].profile,
        a
    );
    let mut events = handoff.take_event_receiver().unwrap();
    completed(&mut events, &b, SessionMode::Build, 40000).await;
    server.expect("p", "new/with:separators").await;
    handoff.shutdown().await.unwrap();
    assert_persisted_handoff(&handoff_path, &opening_handoff);
    assert_eq!(std::fs::read(&path).unwrap(), source_before);
    assert_eq!(std::fs::read(&fixture.models_path).unwrap(), config_bytes);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn immediate_model_change_resume_and_global_session_divergence_are_durable() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let mut running = fixture.start(runtime::SessionStart::New).await;
    let path = running.restoration().transcript_path.clone();
    let commands = running.command_sender();
    let mut events = running.take_event_receiver().unwrap();
    commands
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "before selection".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    completed(
        &mut events,
        &ModelProfileRef::new("p", "old"),
        SessionMode::Build,
        100000,
    )
    .await;
    server.expect("p", "old").await;
    let selected = ModelProfileRef::new("q", "old");
    assert!(matches!(
        select(
            &commands,
            &mut events,
            SessionMode::Build,
            selected.clone(),
            revision(&load(&fixture.path).unwrap()).unwrap()
        )
        .await,
        Result::Changed {
            unchanged: false,
            ..
        }
    ));
    // No assistant response after selection. Acknowledgement already means durable.
    running.shutdown().await.unwrap();
    assert_eq!(
        transcript::load_report(&path)
            .unwrap()
            .session_models()
            .unwrap()
            .unwrap()
            .for_mode(SessionMode::Build)
            .profile,
        selected
    );
    fixture.change_globals();
    let mut resumed = fixture
        .start(runtime::SessionStart::Resume(path.clone()))
        .await;
    assert_eq!(
        resumed.restoration().model_contexts[ModelRole::Build.index()].profile,
        selected
    );
    let unchanged_bytes = std::fs::read(&path).unwrap();
    let commands = resumed.command_sender();
    let mut events = resumed.take_event_receiver().unwrap();
    assert!(matches!(
        select(
            &commands,
            &mut events,
            SessionMode::Build,
            selected.clone(),
            revision(&load(&fixture.path).unwrap()).unwrap()
        )
        .await,
        Result::Changed {
            unchanged: true,
            ..
        }
    ));
    assert_eq!(std::fs::read(&path).unwrap(), unchanged_bytes);
    assert_eq!(
        load(&fixture.path).unwrap().modes().build.profile_ref(),
        selected
    );
    commands
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "after resume".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    completed(&mut events, &selected, SessionMode::Build, 100000).await;
    server.expect("q", "old").await;

    // The real global settings transaction can commit while the header fails.
    let old_bytes = std::fs::read(&path).unwrap();
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    let plan = ModelProfileRef::new("q", "new/with:separators");
    let result = select(
        &commands,
        &mut events,
        SessionMode::Plan,
        plan.clone(),
        revision(&load(&fixture.path).unwrap()).unwrap(),
    )
    .await;
    let Result::Rejected {
        code,
        message,
        current_revision: Some(next_revision),
        checkpoint_installed: false,
    } = result
    else {
        panic!("expected explicit partial save: {result:?}");
    };
    assert_eq!(code, "session_save_failed");
    assert!(message.contains("global default was saved"));
    assert_eq!(
        revision(&load(&fixture.path).unwrap()).unwrap(),
        next_revision
    );
    assert_eq!(
        load(&fixture.path).unwrap().modes().plan.profile_ref(),
        plan
    );
    assert_eq!(std::fs::read(&saved).unwrap(), old_bytes);
    commands
        .send(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Models {
                request_id: "list".into(),
                request: Request::List {
                    scope: Scope::SessionAndDefault,
                    mode: SessionMode::Plan,
                },
            },
        ))
        .unwrap();
    loop {
        if let SessionEvent::ModelsResult { result, .. } = event(&mut events).await {
            assert!(
                matches!(result, Result::Catalog { current, revision, .. } if current.profile == ModelProfileRef::new("p", "new/with:separators") && revision == next_revision)
            );
            break;
        }
    }
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&saved, &path).unwrap();
    assert!(matches!(
        select(
            &commands,
            &mut events,
            SessionMode::Plan,
            plan.clone(),
            next_revision
        )
        .await,
        Result::Changed {
            unchanged: false,
            ..
        }
    ));
    resumed.shutdown().await.unwrap();
    let mut retried = fixture.start(runtime::SessionStart::Resume(path)).await;
    assert_eq!(
        retried.restoration().model_contexts[ModelRole::Build.index()].profile,
        selected
    );
    assert_eq!(
        retried.restoration().model_contexts[ModelRole::Plan.index()].profile,
        plan
    );
    let mut events = retried.take_event_receiver().unwrap();
    retried
        .command_sender()
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "Plan after durable selection".into(),
                mode: SessionMode::Plan,
            },
        ))
        .unwrap();
    completed(&mut events, &plan, SessionMode::Plan, 40000).await;
    server.expect("q", "new/with:separators").await;
    retried.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_only_switches_preserve_config_and_resume_without_affecting_fresh_or_active_roots()
{
    for read_only in [false, true] {
        for mode in [SessionMode::Build, SessionMode::Plan] {
            let mut server = Server::new().await;
            let fixture = Fixture::new(&server.url);
            if read_only {
                let mut permissions = std::fs::metadata(&fixture.models_path)
                    .unwrap()
                    .permissions();
                permissions.set_readonly(true);
                std::fs::set_permissions(&fixture.models_path, permissions).unwrap();
            }
            let bytes = std::fs::read(&fixture.models_path).unwrap();
            let metadata = std::fs::metadata(&fixture.models_path).unwrap();
            let config_revision = revision(&load(&fixture.path).unwrap()).unwrap();
            let mut lock_name = fixture.path.as_os_str().to_os_string();
            lock_name.push(".skills.lock");
            let lock_path = PathBuf::from(lock_name);
            let assert_config_unchanged = || {
                assert_eq!(std::fs::read(&fixture.models_path).unwrap(), bytes);
                let after = std::fs::metadata(&fixture.models_path).unwrap();
                assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
                assert_eq!(after.permissions(), metadata.permissions());
                assert_eq!(
                    revision(&load(&fixture.path).unwrap()).unwrap(),
                    config_revision
                );
                // The global writer creates this lock even for a no-op save.
                assert!(!lock_path.exists());
            };
            let defaults = fixture.config.routing().context_policies();
            let mut running = fixture.start(runtime::SessionStart::New).await;
            let mut other = fixture.start(runtime::SessionStart::New).await;
            let path = running.restoration().transcript_path.clone();
            let commands = running.command_sender();
            let mut events = running.take_event_receiver().unwrap();
            commands
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: "established session before local selection".into(),
                        mode,
                    },
                ))
                .unwrap();
            let original_context = &defaults[zevria_model::models::mode_role(mode).index()];
            completed(
                &mut events,
                &original_context.profile,
                mode,
                original_context.input_token_limit,
            )
            .await;
            server
                .expect(
                    &original_context.profile.provider,
                    &original_context.profile.model,
                )
                .await;
            let original_history = transcript::load(&path).unwrap();
            let selected = ModelProfileRef::new("q", "old");
            // Header failure is independent of config writability and has no global revision.
            let saved = path.with_extension("saved");
            std::fs::rename(&path, &saved).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(
                matches!(select_scoped(&commands, &mut events, mode, selected.clone(), config_revision.clone(), Scope::SessionOnly).await,
                Result::Rejected { current_revision: None, checkpoint_installed: false, message, .. } if message.contains("config was not modified"))
            );
            assert_eq!(transcript::load(&saved).unwrap(), original_history);
            assert_config_unchanged();
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&saved, &path).unwrap();
            let result = select_scoped(
                &commands,
                &mut events,
                mode,
                selected.clone(),
                config_revision.clone(),
                Scope::SessionOnly,
            )
            .await;
            assert!(
                matches!(result, Result::Changed { scope: Scope::SessionOnly, revision, unchanged: false, .. } if revision == config_revision)
            );
            assert_config_unchanged();
            // Close immediately after acknowledgement: no new assistant turn.
            running.shutdown().await.unwrap();
            let durable = transcript::load_report(&path).unwrap();
            assert_eq!(&durable.items[1..], &original_history[1..]);
            assert_eq!(
                durable
                    .session_models()
                    .unwrap()
                    .unwrap()
                    .for_mode(mode)
                    .profile,
                selected
            );
            let mut resumed = fixture
                .start(runtime::SessionStart::Resume(path.clone()))
                .await;
            assert_eq!(resumed.restoration().selected_mode, mode);
            for role in [
                ModelRole::Build,
                ModelRole::Plan,
                ModelRole::Review,
                ModelRole::Explore,
                ModelRole::Builder,
            ] {
                let expected = if role == zevria_model::models::mode_role(mode) {
                    &selected
                } else {
                    &defaults[role.index()].profile
                };
                assert_eq!(
                    &resumed.restoration().model_contexts[role.index()].profile,
                    expected
                );
            }
            let before_noop = std::fs::read(&path).unwrap();
            let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let mut events = resumed.take_event_receiver().unwrap();
            assert!(matches!(
                select_scoped(
                    &resumed.command_sender(),
                    &mut events,
                    mode,
                    selected.clone(),
                    config_revision.clone(),
                    Scope::SessionOnly
                )
                .await,
                Result::Changed {
                    unchanged: true,
                    ..
                }
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before_noop);
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                before_mtime
            );
            resumed.shutdown().await.unwrap();
            // A separately running root keeps both original routes.
            let mut other_events = other.take_event_receiver().unwrap();
            for other_mode in [SessionMode::Build, SessionMode::Plan] {
                let context = &defaults[zevria_model::models::mode_role(other_mode).index()];
                other
                    .command_sender()
                    .send(SessionCommand::Turn(
                        zevria_session_api::TurnCommand::Submit {
                            behavior: zevria_foundation::RequestBehavior::Standard,
                            text: "independent root".into(),
                            mode: other_mode,
                        },
                    ))
                    .unwrap();
                completed(
                    &mut other_events,
                    &context.profile,
                    other_mode,
                    context.input_token_limit,
                )
                .await;
                server
                    .expect(&context.profile.provider, &context.profile.model)
                    .await;
            }
            other.shutdown().await.unwrap();
            let fresh = fixture.start(runtime::SessionStart::New).await;
            assert_eq!(fresh.restoration().model_contexts, defaults);
            fresh.shutdown().await.unwrap();
            // Production /implement-fresh construction carries only the Plan,
            // not this root's local selections, and resolves globals anew.
            let source_session_id = path.file_stem().unwrap().to_str().unwrap();
            let source_before = std::fs::read(&path).unwrap();
            let opening_handoff = plan_handoff(source_session_id);
            let mut handoff = fixture
                .start(runtime::SessionStart::FromPlan(opening_handoff.clone()))
                .await;
            let handoff_path = handoff.restoration().transcript_path.clone();
            assert_ne!(handoff.restoration().session_id, source_session_id);
            assert_ne!(handoff_path, path);
            assert_eq!(
                handoff_path.file_stem().unwrap().to_str().unwrap(),
                handoff.restoration().session_id
            );
            assert!(matches!(
                handoff.restoration().transcript_items.as_slice(),
                [
                    TranscriptItem::SessionModels(_),
                    TranscriptItem::SessionMode(SessionMode::Build)
                ]
            ));
            assert_eq!(handoff.restoration().model_contexts, defaults);
            let mut events = handoff.take_event_receiver().unwrap();
            completed(
                &mut events,
                &defaults[ModelRole::Build.index()].profile,
                SessionMode::Build,
                defaults[ModelRole::Build.index()].input_token_limit,
            )
            .await;
            server.expect("p", "old").await;
            handoff.shutdown().await.unwrap();
            assert_persisted_handoff(&handoff_path, &opening_handoff);
            assert_eq!(std::fs::read(&path).unwrap(), source_before);
            assert_config_unchanged();
            assert!(
                server.requests.try_recv().is_err(),
                "no inference during management/restoration"
            );
        }
    }
}

#[tokio::test]
async fn opaque_preflight_names_restored_roles_and_global_review_without_conversion() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let mut writer = TranscriptWriter::create_with_id(
        &transcript::sessions_dir(fixture.directory.path()),
        "opaque",
    )
    .unwrap();
    let selections = zevria_model::models::SessionModels::new(
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("p", "old"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("p", "new/with:separators"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap();
    let checkpoint = zevria_model::CompactionCheckpoint::new(
        zevria_model::CompactionTrigger::Manual,
        zevria_model::CompactionBackend::OpenaiResponsesCompact,
        vec![
            zevria_model::OwnedModelRequestItem::replay_only(
                zevria_model::ProviderReplay::openai_responses(
                    ModelProfileRef::new("p", "old"),
                    vec![json!({"type":"compaction", "encrypted_content":"source-only"})],
                ),
            )
            .unwrap(),
        ],
        vec![],
    )
    .unwrap();
    writer
        .rewrite(&[
            TranscriptItem::SessionModels(selections),
            TranscriptItem::Compaction(checkpoint),
        ])
        .unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);
    let original = std::fs::read(&path).unwrap();
    fixture.change_globals();
    let resumed = fixture
        .start(runtime::SessionStart::Resume(path.clone()))
        .await;
    assert!(
        resumed
            .restoration()
            .startup_notices
            .iter()
            .any(|notice| notice.contains("plan restored selection p/new/with:separators"))
    );
    assert!(
        resumed
            .restoration()
            .startup_notices
            .iter()
            .any(|notice| notice.contains("review global default q/old"))
    );
    assert!(
        !resumed
            .restoration()
            .startup_notices
            .iter()
            .any(|notice| notice.starts_with("build"))
    );
    resumed.shutdown().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn restoration_failures_reject_the_whole_session_without_model_calls() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let good = zevria_model::models::SessionModels::new(
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("p", "old"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("p", "new/with:separators"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap();
    let header = serde_json::to_string(&TranscriptItem::SessionModels(good)).unwrap();
    let missing = header.replace("\"old\"", "\"Unavailable/Exact\"");
    let config_before = std::fs::read(&fixture.models_path).unwrap();
    for (index, original, expected) in [
        (
            0,
            "{\"error\":\"legacy\"}\n".into(),
            "resolve the reported history issue",
        ),
        (1, format!("{missing}\n{{\"partial\":"), "Unavailable/Exact"),
        (
            3,
            header.replace("new/with:separators", "Missing/Plan"),
            "Missing/Plan",
        ),
        (4, header.replace("\"p\"", "\"P\""), "provider \"P\""),
        (
            2,
            "{\"zevria_session_models\":{}}\n{\"partial\":".into(),
            "unsupported history",
        ),
    ] {
        let writer = TranscriptWriter::create_with_id(
            &transcript::sessions_dir(fixture.directory.path()),
            &format!("bad-{index}"),
        )
        .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        std::fs::write(&path, &original).unwrap();
        let error = match runtime::start_session(
            &fixture.config,
            fixture.directory.path(),
            runtime::SessionStart::Resume(path.clone()),
        )
        .await
        {
            Ok(_) => panic!("invalid restoration succeeded"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(expected), "{error}");
        assert!(error.contains(&format!("bad-{index}")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read(&fixture.models_path).unwrap(), config_before);
    }
    let writer = TranscriptWriter::create_with_id(
        &transcript::sessions_dir(fixture.directory.path()),
        "inspect",
    )
    .unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);
    let original = format!("{header}\n{{\"zevria_skill_activation_v999\":{{}}}}\n");
    std::fs::write(&path, &original).unwrap();
    assert!(
        runtime::start_session(
            &fixture.config,
            fixture.directory.path(),
            runtime::SessionStart::Resume(path.clone()),
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn production_acp_factory_restores_models_and_reports_legacy_errors() {
    use zevria_acp::{SessionRuntimeFactory as _, SessionStart, StartSessionRequest};
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let factory = crate::acp_host::AcpHostFactory::new(Arc::new(load(&fixture.path).unwrap()));
    let request = |start| StartSessionRequest {
        workspace: fixture.directory.path().to_path_buf(),
        start,
    };
    let mut fresh = factory.start(request(SessionStart::New)).await.unwrap();
    fresh
        .commands
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "ACP durable turn".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    completed(
        &mut fresh.events,
        &ModelProfileRef::new("p", "old"),
        SessionMode::Build,
        100000,
    )
    .await;
    server.expect("p", "old").await;
    let id = fresh.session_id.clone();
    fresh.lifecycle.shutdown().await.unwrap();
    fixture.change_globals();
    let mut loaded = factory
        .start(request(SessionStart::Existing {
            session_id: id.clone(),
        }))
        .await
        .unwrap();
    assert!(matches!(
        loaded.transcript_items.first(),
        Some(TranscriptItem::SessionModels(_))
    ));
    loaded
        .commands
        .send(SessionCommand::Turn(
            zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "ACP restored Plan".into(),
                mode: SessionMode::Plan,
            },
        ))
        .unwrap();
    completed(
        &mut loaded.events,
        &ModelProfileRef::new("p", "new/with:separators"),
        SessionMode::Plan,
        40000,
    )
    .await;
    server.expect("p", "new/with:separators").await;
    loaded.lifecycle.shutdown().await.unwrap();
    let sessions = transcript::sessions_dir(fixture.directory.path());
    let mut legacy = TranscriptWriter::create_with_id(&sessions, "legacy").unwrap();
    legacy
        .append(&TranscriptItem::Error {
            error: "legacy history".into(),
        })
        .unwrap();
    let error = match factory
        .start(request(SessionStart::Existing {
            session_id: "legacy".into(),
        }))
        .await
    {
        Ok(_) => panic!("legacy must fail"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("resolve the reported history issue"),
        "{error}"
    );
    assert!(!error.contains("fresh session"), "{error}");
    let selected_build = factory.start(request(SessionStart::New)).await.unwrap();
    let selected_build_id = selected_build.session_id.clone();
    assert_eq!(selected_build.selected_mode, SessionMode::Build);
    let listed = factory
        .list(fixture.directory.path().to_path_buf())
        .await
        .unwrap();
    assert!(listed.iter().any(|session| session.id == id));
    assert!(listed.iter().any(|session| session.id == selected_build_id));
    selected_build.lifecycle.shutdown().await.unwrap();
    assert!(
        factory
            .list(fixture.directory.path().to_path_buf())
            .await
            .unwrap()
            .iter()
            .any(|session| session.id == selected_build_id)
    );
    assert!(server.requests.try_recv().is_err());
}
