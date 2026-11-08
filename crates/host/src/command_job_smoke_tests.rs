//! Production tool/prompt-cache/lifecycle regression, including a fresh runtime on resume.
use super::*;
#[tokio::test]
async fn background_jobs_keep_static_prefix_append_only_input_and_resume_without_processes() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temporary.path()).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "STABLE_JOB_GUIDANCE").unwrap();
    let outputs = (0..6)
        .map(|index| match index {
                0 => {
                    json!([{"type":"function_call", "id":"fc-job", "call_id":"call-job", "name":"command", "arguments":json!({"command":"printf cache-job", "background":true}).to_string(), "status":"completed"}])
                }
                2 => {
                    json!([{"type":"function_call", "id":"fc-read", "call_id":"call-read", "name":"command", "arguments":json!({"command":"printf inspection", "background":null}).to_string(), "status":"completed"}])
                }
                4 => {
                    json!([{"type":"function_call", "id":"fc-status", "call_id":"call-status", "name":"command_status", "arguments":"{\"id\":null,\"wait_seconds\":null}", "status":"completed"}])
                }
                _ => {
                    json!([{"type":"message", "id":format!("msg-{index}"), "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"Job fixture complete."}]}])
                }
        })
        .collect();
    let (address, server) = command_server(outputs).await;
    let config = crate::test_support::write_fixture(
        &workspace.join("config.toml"),
        &responses_config(address),
    )
    .unwrap();
    let mut running = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let path = running.restoration.transcript_path.clone();
    let mut events = running.take_event_receiver().unwrap();
    running
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: "start a background job".into(),
            mode: SessionMode::Build,
            behavior: zevria_foundation::RequestBehavior::Standard,
        }))
        .unwrap();
    completed(&mut events).await;
    // A frontend inspection is an ordering barrier, not model input. Poll until
    // the supervisor has recorded the terminal transition before the next tool.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            running
                .command_sender()
                .send(SessionCommand::Control(
                    zevria_session_api::ControlCommand::Command(
                        zevria_session_api::CommandControl {
                            request_id: "job-barrier".into(),
                            action: zevria_session_api::CommandControlAction::Inspect { id: None },
                        },
                    ),
                ))
                .unwrap();
            while let Some(update) = events.recv().await {
                if let SessionUpdate::Lifecycle(SessionEvent::CommandControlResult {
                    result: zevria_session_api::CommandControlResult::Inspected(jobs),
                    ..
                }) = update
                {
                    if jobs.iter().any(|j| j.snapshot.status.is_terminal()) {
                        return;
                    }
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    running
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: "inspect files".into(),
            mode: SessionMode::Build,
            behavior: zevria_foundation::RequestBehavior::Standard,
        }))
        .unwrap();
    completed(&mut events).await;
    running.shutdown().await.unwrap();
    let mut resumed = start_session(&config, &workspace, SessionStart::Resume(path.clone()))
        .await
        .unwrap();
    let mut events = resumed.take_event_receiver().unwrap();
    resumed
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: "list jobs after resume".into(),
            mode: SessionMode::Build,
            behavior: zevria_foundation::RequestBehavior::Standard,
        }))
        .unwrap();
    completed(&mut events).await;
    resumed.shutdown().await.unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request["instructions"], requests[0]["instructions"]);
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(request["prompt_cache_key"], requests[0]["prompt_cache_key"]);
        if index > 0 {
            assert!(
                request["input"]
                    .as_array()
                    .unwrap()
                    .starts_with(requests[index - 1]["input"].as_array().unwrap())
            );
        }
    }
    let tools = requests[0]["tools"].as_array().unwrap();
    let command = tools.iter().find(|t| t["name"] == "command").unwrap();
    assert_eq!(
        command["parameters"]["required"],
        json!(["command", "background"])
    );
    assert_eq!(
        command["parameters"]["properties"]["background"]["type"],
        json!(["boolean", "null"])
    );
    let status = tools
        .iter()
        .find(|t| t["name"] == "command_status")
        .unwrap();
    assert_eq!(
        status["parameters"]["required"],
        json!(["id", "wait_seconds"])
    );
    assert_eq!(
        status["parameters"]["properties"]["id"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        status["parameters"]["properties"]["wait_seconds"]["type"],
        json!(["integer", "null"])
    );
    assert!(
        status["parameters"]["properties"]["wait_seconds"]
            .get("format")
            .is_none()
    );
    let started = command_result_text(&requests[1], "call-job");
    let id = started
        .strip_prefix("id: ")
        .unwrap()
        .lines()
        .next()
        .unwrap();
    assert_eq!(
        started,
        format!(
            "id: {id}\nstatus: running\nnote: started in the background; use command_status to read output or wait, and command_terminate to stop it"
        )
    );
    let read = command_result_text(&requests[3], "call-read");
    assert!(read.starts_with("exit_code: 0\nduration: "));
    assert!(read.contains("\n<stdout>\ninspection\n</stdout>\njob updates:\n"));
    let (_, footer) = read.split_once("\njob updates:\n").unwrap();
    assert!(footer.starts_with(&format!("{id} · exited 0 · ")));
    assert!(footer.ends_with(" · printf cache-job"));
    assert_eq!(
        command_result_text(&requests[5], "call-read"),
        read,
        "resume must preserve the exact result bytes"
    );
    assert!(requests[3]["input"].to_string().contains("job updates:"));
    assert_eq!(
        requests[3]["input"]
            .to_string()
            .matches("job updates:")
            .count(),
        1
    );
    assert_eq!(
        command_result_text(&requests[5], "call-status"),
        "No command jobs in this session. Jobs do not survive restarts or session replacement."
    );
    let items = transcript::load(&path).unwrap();
    assert!(format!("{items:?}").contains("StartedInBackground"));
}

#[tokio::test]
async fn command_error_envelope_and_unix_signal_reach_provider_unchanged() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temporary.path()).unwrap();
    let outputs = vec![
        json!([
            {"type":"function_call", "id":"fc-timeout", "call_id":"call-timeout", "name":"command", "arguments":json!({"command":"printf partial; sleep 10", "background":null}).to_string(), "status":"completed"},
            {"type":"function_call", "id":"fc-signal", "call_id":"call-signal", "name":"command", "arguments":json!({"command":"kill -9 $$", "background":null}).to_string(), "status":"completed"}
        ]),
        json!([{"type":"message", "id":"msg-done", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"Checked command outcomes."}]}]),
    ];
    let (address, server) = command_server(outputs).await;
    let config = crate::test_support::write_fixture(
        &workspace.join("config.toml"),
        &format!(
            "{}\n[command]\ntimeout_seconds = 1\n",
            responses_config(address)
        ),
    )
    .unwrap();
    let mut running = start_session(
        &config,
        &workspace,
        SessionStart::New {
            inherited_models: None,
        },
    )
    .await
    .unwrap();
    let mut events = running.take_event_receiver().unwrap();
    running
        .command_sender()
        .send(SessionCommand::Turn(TurnCommand::Submit {
            text: "check interrupted and signalled commands".into(),
            mode: SessionMode::Build,
            behavior: zevria_foundation::RequestBehavior::Standard,
        }))
        .unwrap();
    completed(&mut events).await;
    running.shutdown().await.unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let timeout = command_result_text(&requests[1], "call-timeout");
    assert!(timeout.starts_with("status: error\nerror: command timed out after 1s; its process tree was terminated\nduration: "), "{timeout}");
    assert!(timeout.ends_with("\n<stdout>\npartial\n</stdout>"));
    assert_eq!(timeout.matches("status:").count(), 1);
    assert!(!timeout.contains("exit_code:"));
    assert!(!timeout.contains("<stderr>"));
    let signal = command_result_text(&requests[1], "call-signal");
    assert!(
        signal.starts_with("signal: 9 (SIGKILL)\nduration: "),
        "{signal}"
    );
    assert_eq!(signal.lines().count(), 2);
    assert!(!signal.contains("status:"));
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_eq!(requests[0]["instructions"], requests[1]["instructions"]);
}

fn command_result_text<'a>(request: &'a Value, call_id: &str) -> &'a str {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .unwrap()["output"]
        .as_str()
        .unwrap()
}

async fn command_server(
    outputs: Vec<Value>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, output) in outputs.into_iter().enumerate() {
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
            let mut bytes = vec![0; length.unwrap()];
            reader.read_exact(&mut bytes).await.unwrap();
            requests.push(serde_json::from_slice::<Value>(&bytes).unwrap());
            let response = json!({"type":"response.completed", "sequence_number":1, "response":{"id":format!("response-job-{index}"), "object":"response", "created_at":0,"status":"completed", "error":null, "incomplete_details":null,"instructions":null,"model":"test-model","max_output_tokens":null,"usage":null,"tools":[],"output":output}});
            let body = format!("data: {response}\n\n");
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (address, server)
}
