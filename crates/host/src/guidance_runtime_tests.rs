use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn fresh_plan_handoff_captures_guidance_before_first_generation() {
    let workspace = tempfile::tempdir().unwrap();
    let global = workspace.path().join(".test-global-guidance");
    std::fs::create_dir(&global).unwrap();
    std::fs::write(global.join("AGENTS.md"), "FRESH_GLOBAL_OPENING").unwrap();
    std::fs::write(workspace.path().join("AGENTS.md"), "FRESH_PROJECT_OPENING").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
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
        let response = json!({"type":"response.completed","sequence_number":1,"response":{
            "id":"fresh-response","object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":null,"tools":[],
            "output":[{"type":"message","id":"fresh-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Fresh fixture complete."}]}]
        }});
        let body = format!("data: {response}\n\n");
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        request
    });
    let source = format!(
        r#"
[session]
preamble = "FRESH_CUSTOM_PREAMBLE"
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
    let config_path = workspace.path().join("config.toml");
    let config = crate::test_support::write_fixture(&config_path, &source).unwrap();
    let handoff = zevria_workflow::PlanHandoff::new(
        zevria_workflow::PlanArtifact {
            version: zevria_workflow::PlanVersion {
                id: zevria_workflow::PlanId::new(),
                revision: 1,
            },
            title: "Approved fixture".into(),
            markdown: "# Approved fixture\n\nImplement the fixture.".into(),
            source_turn_id: zevria_foundation::TurnId::new(1),
        },
        "source-session",
    );
    let mut running = start_session(
        &config,
        workspace.path(),
        SessionStart::FromPlan {
            handoff,
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    assert!(running.restoration.startup_notices.is_empty());
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "LATE_CHANGE_MUST_NOT_APPEAR",
    )
    .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let input = request["instructions"].to_string();
    for text in [
        "FRESH_GLOBAL_OPENING",
        "FRESH_PROJECT_OPENING",
        "FRESH_CUSTOM_PREAMBLE",
    ] {
        assert!(input.contains(text));
    }
    assert!(!input.contains("LATE_CHANGE_MUST_NOT_APPEAR"));
    assert!(
        request["instructions"]
            .as_str()
            .unwrap()
            .contains("## File guidance")
    );
    assert!(
        !request["input"]
            .to_string()
            .contains("FRESH_PROJECT_OPENING")
    );
    let mut events = running.take_event_receiver().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(zevria_session_api::SessionUpdate::Lifecycle(
                zevria_session_api::SessionEvent::TurnCompleted { .. },
            )) = events.recv().await
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let items = transcript::load(&running.restoration.transcript_path).unwrap();
    assert_eq!(
        zevria_transcript::replay_directives(&items)
            .unwrap()
            .snapshot()
            .directives
            .len(),
        0
    );
    let bytes = std::fs::read_to_string(&running.restoration.transcript_path).unwrap();
    for text in [
        "FRESH_GLOBAL_OPENING",
        "FRESH_PROJECT_OPENING",
        "FRESH_CUSTOM_PREAMBLE",
        "zevria_directive",
        "zevria_instruction_prefix",
    ] {
        assert!(!bytes.contains(text));
    }
    running.shutdown().await.unwrap();
}
