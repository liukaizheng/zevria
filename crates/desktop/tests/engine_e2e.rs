//! Real host/provider/session transport, without a window or external service.
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use zevria_desktop::session::{InputRoute, WorkspaceModel};
use zevria_host::{SessionStart, start_session};
use zevria_session_api::{ManagementEvent, TurnEvent};
use zevria_session_api::{SessionEvent, SessionUpdate};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_thinking_effort_selection_shutdown_resume_and_new_through_desktop_model() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut first = None;
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let length = length.unwrap();
            assert!(length < 1_048_576);
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                request["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("Workflow policy")
            );
            assert_eq!(
                request["reasoning"]["effort"],
                if index == 0 { "medium" } else { "high" }
            );
            if let Some(first) = &first {
                let first: &serde_json::Value = first;
                for field in ["instructions", "tools", "prompt_cache_key"] {
                    assert_eq!(request[field], first[field], "effort must preserve {field}");
                }
                let input = request["input"].as_array().unwrap();
                assert!(
                    input
                        .iter()
                        .any(|item| item["encrypted_content"] == "saved-native-reasoning"
                            && item["signature"] == "saved-signature")
                );
            } else {
                first = Some(request.clone());
            }
            let response = serde_json::json!({"type":"response.completed","sequence_number":2,"response":{"id":format!("response_{index}"),"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":null,"tools":[],"output":[{"type":"reasoning","id":format!("reasoning_{index}"),"summary":[{"type":"summary_text","text":"Saved visible thinking."}],"encrypted_content":"saved-native-reasoning","signature":"saved-signature"},{"type":"message","id":format!("message_{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"Desktop fixture complete."}]}]}});
            let body = format!("data: {response}\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let source = format!(
        r#"
[providers.test]
base_url = "http://{address}/v1/responses"
api_key = "fixture"
supports_websockets = false
[providers.test.input_token_count]
enabled = false
[providers.test.models.test-model]
context_window_tokens = 272000
retained_user_tokens = 20000
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[modes]
build = {{ provider = "test", model = "test-model", reasoning_level = "medium" }}
plan = {{ provider = "test", model = "test-model", reasoning_level = "medium" }}
review = {{ provider = "test", model = "test-model", reasoning_level = "medium" }}
explore = {{ provider = "test", model = "test-model", reasoning_level = "medium" }}
builder = {{ provider = "test", model = "test-model", reasoning_level = "medium" }}
"#
    );
    let config =
        zevria_host::test_support::write_fixture(&workspace.join("config.toml"), &source).unwrap();
    let mut running = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let mut model = zevria_desktop::session::restore(running.restored);
    let path = model.transcript_path.clone();
    let mut events = running.running.take_event_receiver().unwrap();
    let sender = running.running.command_sender();
    let InputRoute::Command(command) = model.route_input("desktop smoke", false) else {
        panic!()
    };
    sender.send(*command).unwrap();
    complete(&mut model, &mut events).await;
    assert!(!model.root.is_busy());
    assert_eq!(model.root.conversation.history().len(), 2);
    use zevria_foundation::{ModelRole, ReasoningLevel as Level, SessionMode};
    use zevria_model::models::{ModelManagementResult as Result, ModelSelectionScope as Scope};
    let epoch = model.root.conversation.epoch();
    let history = format!("{:?}", model.root.conversation.history());
    let telemetry = model.root.session.telemetry(ModelRole::Build).clone();
    let settings_path = zevria_foundation::config::models_path_for(&workspace.join("config.toml"));
    let settings_before = std::fs::read(&settings_path).unwrap();
    assert!(model.open_model_picker(Scope::SessionOnly));
    while let Some(command) = model.controls.models.commands.pop_front() {
        sender.send(command).unwrap();
    }
    assert!(matches!(
        model_result(&mut model, &mut events).await,
        Result::Catalog { .. }
    ));
    assert!(model.controls.models.choose_model(0));
    let high = model.controls.models.profiles[0]
        .reasoning_levels
        .iter()
        .position(|level| *level == Level::High)
        .unwrap();
    assert!(model.controls.models.choose_effort(high));
    assert!(model.controls.models.apply());
    while let Some(command) = model.controls.models.commands.pop_front() {
        sender.send(command).unwrap();
    }
    assert!(matches!(
        model_result(&mut model, &mut events).await,
        Result::Changed {
            snapshot: None,
            reasoning_level: Level::High,
            ..
        }
    ));
    assert_eq!(model.root.conversation.epoch(), epoch);
    assert_eq!(format!("{:?}", model.root.conversation.history()), history);
    assert_eq!(model.root.session.telemetry(ModelRole::Build), &telemetry);
    assert_eq!(std::fs::read(&settings_path).unwrap(), settings_before);
    drop(events);
    drop(sender);
    running.running.shutdown().await.unwrap();
    let mut running = start_session(&config, &workspace, SessionStart::Resume(path.clone()))
        .await
        .unwrap();
    let mut model = zevria_desktop::session::restore(running.restored);
    assert_eq!(model.root.conversation.history().len(), 2);
    assert_eq!(model.transcript_path, path);
    assert_eq!(
        model
            .root
            .session_models()
            .unwrap()
            .reasoning_for_mode(SessionMode::Build),
        Level::High
    );
    assert_eq!(
        model
            .root
            .session_models()
            .unwrap()
            .reasoning_for_mode(SessionMode::Plan),
        Level::Medium
    );
    let mut events = running.running.take_event_receiver().unwrap();
    let sender = running.running.command_sender();
    let InputRoute::Command(command) = model.route_input("resumed smoke", false) else {
        panic!()
    };
    sender.send(*command).unwrap();
    complete(&mut model, &mut events).await;
    assert_eq!(model.root.conversation.history().len(), 4);
    let inherited_models = model.root.session_models().unwrap();
    drop(events);
    drop(sender);
    running.running.shutdown().await.unwrap();
    let fresh = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: Some(inherited_models),
        },
    )
    .await
    .unwrap();
    let restored = zevria_desktop::session::restore(fresh.restored);
    assert_eq!(
        restored
            .root
            .session_models()
            .unwrap()
            .reasoning_for_mode(SessionMode::Build),
        Level::High
    );
    assert_eq!(
        restored
            .root
            .session_models()
            .unwrap()
            .reasoning_for_mode(SessionMode::Plan),
        Level::Medium
    );
    fresh.running.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn restored_plan_uses_skill_policy_and_mode_switches_refresh_completions() {
    use zevria_content::UserPrompt;
    use zevria_foundation::{ModelRole, SessionMode};
    use zevria_model::models::SessionModels;
    use zevria_presentation::command::ClassifiedInput;
    use zevria_transcript::transcript::{TranscriptItem, TranscriptWriter, sessions_dir};

    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let skills = workspace.join(".zevria/skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("desktop-mode-fixture.md"),
        "---\ndescription: Check skill availability\n---\nReview the fixture.\n",
    )
    .unwrap();
    let config = zevria_host::test_support::write_fixture(
        &workspace.join("config.toml"),
        r#"
[session.plan]
allow_skills = false
[providers.test]
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "fixture"
supports_websockets = false
[providers.test.input_token_count]
enabled = false
[providers.test.models.test-model]
context_window_tokens = 272000
retained_user_tokens = 20000
reasoning_levels = ["medium"]
reasoning_summary_level = "detailed"
[modes]
build = { provider = "test", model = "test-model", reasoning_level = "medium" }
plan = { provider = "test", model = "test-model", reasoning_level = "medium" }
review = { provider = "test", model = "test-model", reasoning_level = "medium" }
explore = { provider = "test", model = "test-model", reasoning_level = "medium" }
builder = { provider = "test", model = "test-model", reasoning_level = "medium" }
"#,
    )
    .unwrap();
    let routing = config.routing();
    let models = SessionModels::new(
        routing.selection_for_role(ModelRole::Build).clone(),
        routing.selection_for_role(ModelRole::Plan).clone(),
    )
    .unwrap();
    let mut writer = TranscriptWriter::create_with_id(&sessions_dir(&workspace), "plan").unwrap();
    writer
        .rewrite(&[
            TranscriptItem::SessionModels(Box::new(models)),
            TranscriptItem::SessionMode(SessionMode::Plan),
        ])
        .unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);

    let mut started = start_session(&config, &workspace, SessionStart::Resume(path))
        .await
        .unwrap();
    let mut model = zevria_desktop::session::restore(started.restored);
    let has_skill = |model: &WorkspaceModel| {
        matches!(
            model
                .controls
                .registry
                .classify_prompt(&UserPrompt::from_text("$desktop-mode-fixture")),
            Ok(ClassifiedInput::Skill { .. })
        )
    };
    assert_eq!(model.root.next_mode(), SessionMode::Plan);
    assert!(!has_skill(&model));

    let mut events = started.running.take_event_receiver().unwrap();
    let sender = started.running.command_sender();
    for (slash, mode, expected) in [
        ("/build", SessionMode::Build, true),
        ("/plan", SessionMode::Plan, false),
    ] {
        let InputRoute::Command(command) = model.route_input(slash, false) else {
            panic!("expected a mode selection command for {slash}");
        };
        sender.send(*command).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let update = events.recv().await.unwrap();
                let refreshed = matches!(
                    &update,
                    SessionUpdate::Lifecycle(SessionEvent::Manage(
                        ManagementEvent::SkillsResult { .. }
                    ))
                );
                model.apply(update);
                while let Some(command) = model.controls.skills.commands.pop_front() {
                    sender.send(command).unwrap();
                }
                if refreshed {
                    break;
                }
            }
        })
        .await
        .expect("mode selection must request fresh skill completions");
        assert_eq!(model.root.next_mode(), mode);
        assert_eq!(has_skill(&model), expected);
    }
    drop(sender);
    drop(events);
    started.running.shutdown().await.unwrap();
}

async fn model_result(
    model: &mut WorkspaceModel,
    events: &mut zevria_session_api::SessionEventReceiver,
) -> zevria_model::models::ModelManagementResult {
    *tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(update) = events.recv().await {
            assert!(
                !matches!(
                    &update,
                    SessionUpdate::Lifecycle(
                        SessionEvent::Turn(TurnEvent::Started { .. })
                            | SessionEvent::Turn(TurnEvent::ModelCallStarted { .. })
                            | SessionEvent::Turn(TurnEvent::CompactionStarted { .. })
                    )
                ),
                "management is not a conversation turn"
            );
            let result = if let SessionUpdate::Lifecycle(SessionEvent::Manage(
                ManagementEvent::ModelsResult { result, .. },
            )) = &update
            {
                Some(result.clone())
            } else {
                None
            };
            model.apply(update);
            if let Some(result) = result {
                return result;
            }
        }
        panic!("management stream closed")
    })
    .await
    .unwrap()
}
async fn complete(
    model: &mut WorkspaceModel,
    events: &mut zevria_session_api::SessionEventReceiver,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(update) = events.recv().await {
            assert!(
                !matches!(
                    &update,
                    SessionUpdate::Lifecycle(
                        SessionEvent::Turn(TurnEvent::Failed { .. })
                            | SessionEvent::Turn(TurnEvent::Rejected { .. })
                            | SessionEvent::Turn(TurnEvent::Cancelled { .. })
                    )
                ),
                "{update:?}"
            );
            let done = matches!(
                &update,
                SessionUpdate::Lifecycle(SessionEvent::Turn(TurnEvent::Completed { .. }))
            );
            model.apply(update);
            if done {
                return;
            }
        }
        panic!("event stream closed");
    })
    .await
    .unwrap();
}
