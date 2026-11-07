//! Real host/provider/session transport, without a window or external service.
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use zevria_desktop::session::{InputRoute, WorkspaceModel};
use zevria_host::{SessionStart, start_session};
use zevria_session_api::{SessionEvent, SessionUpdate};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_complete_shutdown_and_resume_through_the_desktop_model() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
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
            let response = serde_json::json!({"type":"response.completed","sequence_number":2,"response":{"id":format!("response_{index}"),"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":null,"tools":[],"output":[{"type":"message","id":format!("message_{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"Desktop fixture complete."}]}]}});
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
    let mut model = WorkspaceModel::restore(running.take_restored()).unwrap();
    let path = model.transcript_path.clone();
    let mut events = running.take_event_receiver().unwrap();
    let sender = running.command_sender();
    let InputRoute::Command(command) = model.route_input("desktop smoke", false) else {
        panic!()
    };
    sender.send(*command).unwrap();
    complete(&mut model, &mut events).await;
    assert!(!model.root.is_busy());
    assert_eq!(model.root.conversation.history().len(), 2);
    drop(events);
    drop(sender);
    running.shutdown().await.unwrap();
    let mut running = start_session(&config, &workspace, SessionStart::Resume(path.clone()))
        .await
        .unwrap();
    let mut model = WorkspaceModel::restore(running.take_restored()).unwrap();
    assert_eq!(model.root.conversation.history().len(), 2);
    assert_eq!(model.transcript_path, path);
    let mut events = running.take_event_receiver().unwrap();
    let sender = running.command_sender();
    let InputRoute::Command(command) = model.route_input("resumed smoke", false) else {
        panic!()
    };
    sender.send(*command).unwrap();
    complete(&mut model, &mut events).await;
    assert_eq!(model.root.conversation.history().len(), 4);
    drop(events);
    drop(sender);
    running.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
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
                        SessionEvent::TurnFailed { .. }
                            | SessionEvent::TurnRejected { .. }
                            | SessionEvent::TurnCancelled { .. }
                    )
                ),
                "{update:?}"
            );
            let done = matches!(
                &update,
                SessionUpdate::Lifecycle(SessionEvent::TurnCompleted { .. })
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
