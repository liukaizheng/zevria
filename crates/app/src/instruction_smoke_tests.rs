//! Production Responses smoke: mode switching, tool activation, shutdown, and resume.
use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use zevria_session_api::{
    ManagementCommand, ModeSelectionResult, SessionEvent, SessionUpdate, TurnCommand,
};

async fn completed(events: &mut zevria_session_api::SessionEventReceiver) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(update) = events.recv().await {
            if let SessionUpdate::Lifecycle(event) = update {
                assert!(
                    !matches!(
                        event,
                        SessionEvent::TurnFailed { .. }
                            | SessionEvent::TurnRejected { .. }
                            | SessionEvent::TurnCancelled { .. }
                    ),
                    "{event:?}"
                );
                if matches!(event, SessionEvent::TurnCompleted { .. }) {
                    return;
                }
            }
        }
        panic!("event channel closed before completion");
    })
    .await
    .unwrap();
}

async fn select_mode(
    running: &RunningSession,
    events: &mut zevria_session_api::SessionEventReceiver,
    mode: SessionMode,
) {
    running
        .command_sender()
        .send(SessionCommand::Manage(ManagementCommand::SetMode {
            request_id: "smoke-mode".into(),
            mode,
        }))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(update) = events.recv().await {
            if let SessionUpdate::Lifecycle(SessionEvent::ModeResult { request_id, result }) =
                update
            {
                assert_eq!(request_id, "smoke-mode");
                assert_eq!(
                    result,
                    ModeSelectionResult::Accepted {
                        mode,
                        changed: true
                    }
                );
                return;
            }
        }
        panic!("event channel closed before selection");
    })
    .await
    .unwrap();
}

fn policy(text: &str) -> Value {
    serde_json::from_str(
        text.split_once("## Workflow policy: ")
            .unwrap()
            .1
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn responses_instruction_modules_survive_modes_skill_activation_and_resume() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temporary.path()).unwrap();
    std::fs::create_dir_all(workspace.join(".zevria/skills")).unwrap();
    std::fs::write(
        workspace.join(".zevria/skills/module-smoke.md"),
        "---\ndescription: Check the module smoke fixture\n---\nPINNED_SMOKE_BODY\n",
    )
    .unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "STABLE_PROJECT_GUIDANCE").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..6 {
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
            requests.push(serde_json::from_slice::<Value>(&bytes).unwrap());
            let output = if index == 2 {
                json!([{"type":"function_call","id":"fc-smoke-skill","call_id":"call-smoke-skill","name":"skill","arguments":"{\"skill\":\"module-smoke\"}","status":"completed"}])
            } else {
                json!([{"type":"message","id":format!("message_{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"Smoke fixture complete."}]}])
            };
            let response = json!({"type":"response.completed","sequence_number":1,"response":{
                "id":format!("response_{index}"),"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":null,"tools":[],"output":output
            }});
            let body = format!("data: {response}\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let source = format!(
        r#"
[session]
preamble = "STABLE_APPLICATION_GUIDANCE"
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
    let mut running = start_session(&config, &workspace, SessionStart::New)
        .await
        .unwrap();
    let path = running.restoration.transcript_path.clone();
    let mut events = running.take_event_receiver().unwrap();
    for (index, mode) in [
        SessionMode::Build,
        SessionMode::Plan,
        SessionMode::Build,
        SessionMode::Build,
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 || index == 2 {
            select_mode(&running, &mut events, mode).await;
        }
        running
            .command_sender()
            .send(SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: format!("smoke turn {index}").into(),
                mode,
            }))
            .unwrap();
        completed(&mut events).await;
    }
    running.shutdown().await.unwrap();
    let mut resumed = start_session(&config, &workspace, SessionStart::Resume(path.clone()))
        .await
        .unwrap();
    assert_eq!(resumed.restoration.selected_mode, SessionMode::Build);
    let mut events = resumed.take_event_receiver().unwrap();
    resumed
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "resume smoke".into(),
            mode: SessionMode::Build,
        }))
        .unwrap();
    completed(&mut events).await;
    resumed.shutdown().await.unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let prefix = requests[0]["instructions"]
        .as_str()
        .unwrap()
        .split_once("## Workflow policy:")
        .unwrap()
        .0;
    for (index, request) in requests.iter().enumerate() {
        let instructions = request["instructions"].as_str().unwrap();
        assert_eq!(
            instructions
                .split_once("## Workflow policy:")
                .unwrap()
                .0
                .as_bytes(),
            prefix.as_bytes()
        );
        assert!(
            prefix.contains("STABLE_PROJECT_GUIDANCE")
                && prefix.contains("STABLE_APPLICATION_GUIDANCE")
        );
        assert!(
            instructions.contains(zevria_instructions::prompts::COMMAND_CONVENTIONS_INSTRUCTIONS)
        );
        assert!(!instructions.contains("PINNED_SMOKE_BODY"));
        let policy = policy(instructions);
        assert_eq!(
            policy["scope"],
            if index == 0 {
                "build"
            } else if index == 1 {
                "plan"
            } else {
                "build"
            }
        );
        assert_eq!(policy["subtasks"], json!(["explore"]));
        assert_eq!(policy.get("inspection").is_some(), index == 1);
        assert_eq!(
            request["input"]
                .to_string()
                .matches("Skill directive: enable")
                .count(),
            usize::from(index >= 3)
        );
        assert_eq!(request["prompt_cache_key"], requests[0]["prompt_cache_key"]);
        if index >= 3 {
            assert_eq!(request["instructions"], requests[2]["instructions"]);
        }
        if index > 0 {
            assert!(
                request["input"]
                    .as_array()
                    .unwrap()
                    .starts_with(requests[index - 1]["input"].as_array().unwrap())
            );
        }
    }
    assert_eq!(
        transcript::load(&path)
            .unwrap()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Directive(_)))
            .count(),
        1
    );
}
