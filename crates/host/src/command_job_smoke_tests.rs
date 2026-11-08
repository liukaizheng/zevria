//! Production tool/prompt-cache/lifecycle regression, including a fresh runtime on resume.
use super::*;
#[tokio::test]
async fn background_jobs_keep_static_prefix_append_only_input_and_resume_without_processes() {
    background_completion_cache_fixture(true).await;
}

#[tokio::test]
async fn passive_job_delivery_keeps_static_prefix_and_verbatim_resume() {
    background_completion_cache_fixture(false).await;
}

async fn background_completion_cache_fixture(automatic: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temporary.path()).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "STABLE_JOB_GUIDANCE").unwrap();
    let offset = usize::from(automatic);
    let outputs = (0..6 + offset).map(|index| {
        if index == 0 {
            json!([{"type":"function_call", "id":"fc-job", "call_id":"call-job", "name":"command", "arguments":json!({"command":"while [ ! -f release-job ]; do sleep 0.02; done; printf cache-job", "background":true}).to_string(), "status":"completed"}])
        } else if index == 2 + offset {
            json!([{"type":"function_call", "id":"fc-read", "call_id":"call-read", "name":"command", "arguments":json!({"command":"printf inspection", "background":null}).to_string(), "status":"completed"}])
        } else if index == 4 + offset {
            json!([{"type":"function_call", "id":"fc-status", "call_id":"call-status", "name":"command_status", "arguments":"{\"id\":null,\"wait_seconds\":null}", "status":"completed"}])
        } else {
            json!([{"type":"message", "id":format!("msg-{index}"), "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"Job fixture complete."}]}])
        }
    }).collect();
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
    // completed() includes the post-turn EngineIdle barrier.
    completed(&mut events).await;
    std::fs::write(workspace.join("release-job"), "go").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(update) = events.recv().await {
            if matches!(
                update,
                SessionUpdate::Lifecycle(SessionEvent::JobContinuationChanged {
                    mode: Some(SessionMode::Build)
                })
            ) {
                return;
            }
        }
        panic!("runtime ended before job continuation offer");
    })
    .await
    .unwrap();
    if automatic {
        running
            .command_sender()
            .send(SessionCommand::Turn(TurnCommand::ContinueAfterJobs {
                mode: SessionMode::Build,
            }))
            .unwrap();
        completed(&mut events).await;
    }
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
    let saved = transcript::load(&path)
        .unwrap()
        .into_iter()
        .find_map(|item| match item {
            zevria_transcript::TranscriptItem::CommandJobNotice(notice) => Some(notice),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        saved.kind(),
        if automatic {
            zevria_foundation::CommandJobNoticeKind::Continuation
        } else {
            zevria_foundation::CommandJobNoticeKind::Update
        }
    );
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
        assert!(!request["input"].to_string().contains("job updates:"));
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
    let read = command_result_text(&requests[3 + offset], "call-read");
    assert!(read.starts_with("exit_code: 0\nduration: "));
    assert!(read.ends_with("\n<stdout>\ninspection\n</stdout>"));
    assert_eq!(
        command_result_text(requests.last().unwrap(), "call-read"),
        read
    );
    assert!(request_notice_text(&requests[2]).contains("<stdout>\ncache-job\n</stdout>"));
    assert_eq!(request_notice_text(&requests[2]), saved.text());
    assert_eq!(request_notice_text(requests.last().unwrap()), saved.text());
    assert_eq!(
        requests.last().unwrap()["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item.to_string().contains("Command job update:"))
            .count(),
        1
    );
    assert_eq!(
        command_result_text(requests.last().unwrap(), "call-status"),
        "No command jobs in this session. Jobs do not survive restarts or session replacement."
    );
    let loaded = transcript::load(&path)
        .unwrap()
        .into_iter()
        .find_map(|item| match item {
            zevria_transcript::TranscriptItem::CommandJobNotice(notice) => Some(notice),
            _ => None,
        })
        .unwrap();
    assert_eq!(loaded.text().as_bytes(), saved.text().as_bytes());
}

fn request_notice_text(request: &Value) -> &str {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flat_map(|content| content.iter())
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .find(|text| text.starts_with("Command job update:"))
        .unwrap()
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
