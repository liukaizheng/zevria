//! Startup must use the writer's real path, including worker and nondefault resumes.
use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn normal_and_worker_resume_store_diagnostics_adjacent_to_actual_transcript() {
    for execution in [
        ExecutionProfile::Interactive,
        ExecutionProfile::EnsembleWorker,
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(temporary.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::<Value>::new();
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
                assert!(length < 1024 * 1024);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                assert!(request.get("previous_response_id").is_none());
                if let Some(first) = requests.first() {
                    assert_eq!(request["instructions"], first["instructions"]);
                    assert_eq!(request["tools"], first["tools"]);
                    assert_eq!(request["prompt_cache_key"], first["prompt_cache_key"]);
                    assert!(
                        request["input"]
                            .as_array()
                            .unwrap()
                            .starts_with(first["input"].as_array().unwrap())
                    );
                }
                let response = json!({"type":"response.completed","sequence_number":1,"response":{
                    "id":format!("response_{index}"),"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":{"input_tokens":15979,"input_tokens_details":{"cached_tokens":if index == 0 {14848} else {0}},"output_tokens":100,"total_tokens":16079},"tools":[],
                    "output":[{"type":"message","id":format!("message_{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"PRIVATE_REPLY"}]}]
                }});
                let body = format!("data: {response}\n\n");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                requests.push(request);
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
        let config_path = workspace.join("config.toml");
        let config = crate::test_support::write_fixture(&config_path, &source).unwrap();
        let directory = if execution.is_worker() {
            sessions_dir(&workspace, execution)
        } else {
            workspace.join("nondefault-resume-location")
        };
        std::fs::create_dir_all(&directory).unwrap();
        let mut writer =
            transcript::TranscriptWriter::create_with_id(&directory, "resume-location").unwrap();
        let profile = zevria_foundation::ModelProfileRef::new("test", "test-model");
        writer
            .append(&TranscriptItem::SessionModels(
                zevria_model::models::SessionModels::new(
                    zevria_model::models::ModelSelection::new(
                        profile.clone(),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    zevria_model::models::ModelSelection::new(
                        profile,
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                )
                .unwrap(),
            ))
            .unwrap();
        writer
            .append(&TranscriptItem::Message(rig_core::message::Message::user(
                "PRIVATE_SEED",
            )))
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        let mut previous_runtime = None;
        for text in ["establish baseline", "ok"] {
            let mut running = start_session_with_profile(
                &config,
                &workspace,
                SessionStart::Resume(path.clone()),
                execution,
            )
            .await
            .unwrap();
            let mut events = running.take_event_receiver().unwrap();
            running
                .command_sender()
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: text.into(),
                        mode: SessionMode::Build,
                    },
                ))
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(update) = events.recv().await {
                    if let zevria_session_api::SessionUpdate::Lifecycle(event) = update {
                        assert!(
                            !matches!(
                                event,
                                zevria_session_api::SessionEvent::TurnFailed { .. }
                                    | zevria_session_api::SessionEvent::TurnRejected { .. }
                            ),
                            "{event:?}"
                        );
                        if matches!(
                            event,
                            zevria_session_api::SessionEvent::TurnCompleted { .. }
                        ) {
                            break;
                        }
                    }
                }
            })
            .await
            .unwrap();
            running.shutdown().await.unwrap();
            let snapshots = std::fs::read_dir(directory.join(".cache-diagnostics"))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(snapshots.len(), 1);
            let bytes = std::fs::read_to_string(snapshots[0].path()).unwrap();
            assert!(!bytes.contains("PRIVATE_"));
            let snapshot: Value = serde_json::from_str(&bytes).unwrap();
            let runtime = snapshot["baseline"]["meta"]["runtime"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_ne!(previous_runtime.as_ref(), Some(&runtime));
            previous_runtime = Some(runtime);
        }
        assert!(
            !transcript::sessions_dir(&workspace)
                .join(".cache-diagnostics")
                .exists()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
