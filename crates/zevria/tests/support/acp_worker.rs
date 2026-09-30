//! Actual worker binary against a deterministic local Responses endpoint.
use super::acp_skills::Rpc;
#[path = "../../../tools/tests/support/inspection.rs"]
mod inspection;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead as _, Read as _, Write as _},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zevria_transcript::transcript;

const TITLE: &str = "Inspect The Worker Fixture";
const MARKDOWN: &str = "# Inspect The Worker Fixture\n\n## Goal\nInspect source.txt.  \n\n## Decisions\n- Preserve compatibility.\n\n## Implementation\n1. Leave the source unchanged.\n\n## Validation\n- Inspect the persisted proof.\n\n## Risks\n- Behavioral shell restrictions are not a sandbox.\n";

pub(super) struct ResponsesFixture {
    pub(super) url: String,
    pub(super) requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl ResponsesFixture {
    pub(super) fn start() -> Self {
        Self::with_inspection(None)
    }

    fn with_inspection(investigation: Option<(Arc<inspection::Fixture>, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = std::thread::spawn(move || {
            let mut steps = BTreeMap::<String, usize>::new();
            let mut scratch = BTreeMap::<String, inspection::Scratch>::new();
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                // Accepted sockets can inherit the listener's nonblocking
                // mode on BSD/macOS. Read the request with the bounded blocking
                // timeout rather than racing the client's first write.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(
                    line.starts_with("POST /v1/responses "),
                    "unexpected request {line}"
                );
                let mut length = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let length = length.expect("bounded content length");
                assert!(length < 1024 * 1024);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let model = request["model"].as_str().unwrap().to_string();
                let step = steps.entry(model.clone()).or_default();
                let output = match (model.as_str(), *step) {
                    ("inspection-plan-fixture" | "inspection-review-fixture", step) => {
                        let (fixture, url) = investigation.as_ref().expect("inspection fixture");
                        inspection_response(&request, &model, step, fixture, url, &mut scratch)
                    }
                    ("synthesis-review-fixture", 0) => {
                        let (files, _) = investigation.as_ref().unwrap();
                        assert!(
                            request["input"].to_string().contains(
                                "Investigated external diagnostic and scratch build/test"
                            )
                        );
                        function(
                            "parent-inspection",
                            "command",
                            json!({"command":files.read_command()}),
                        )
                    }
                    ("synthesis-review-fixture", 1) => {
                        assert!(
                            result_text(&request, "parent-inspection")
                                .contains(inspection::DIAGNOSTIC)
                        );
                        message(
                            "Parent synthesis independently verified source and external evidence; scratch validation passed.",
                        )
                    }
                    ("automatic-skill-fixture", 0 | 3 | 8) => {
                        function(&format!("apply-{step}"), "skill", json!({"skill":"commit"}))
                    }
                    ("automatic-skill-fixture", _) => {
                        message("Synthetic skill application test; no Git action performed.")
                    }
                    ("guidance-fixture", _)
                        if request["input"]
                            .as_array()
                            .and_then(|input| {
                                input.iter().rev().find(|item| {
                                    !item["content"][0]["text"]
                                        .as_str()
                                        .is_some_and(|text| text.starts_with("Request directive:"))
                                })
                            })
                            .is_some_and(|last| {
                                last["content"][0]["text"] == "LATE_EXPLORE_REQUEST"
                            }) =>
                    {
                        function(
                            "late-explore",
                            "launch_subtasks",
                            json!({"tasks":[{"title":"Late guidance child", "type":"explore", "prompt":"Report on the guidance fixture without tools."}]}),
                        )
                    }
                    ("guidance-fixture" | "explore-guidance-fixture", _) => {
                        message("Guidance fixture completed.")
                    }
                    ("plan-fixture" | "review-fixture", 0) => function(
                        "inspect",
                        "command",
                        json!({"command":"rtk sed -n '1,5p' source.txt"}),
                    ),
                    ("plan-fixture", 1) => function(
                        "question",
                        "question",
                        json!({"questions":[{"id":"compatibility", "header":"Compatibility", "question":"Which compatibility policy should be retained?", "options":[{"label":"Preserve", "description":"Keep the public API."},{"label":"Simplify", "description":"Remove legacy behavior."}]}]}),
                    ),
                    ("plan-fixture", 2) => function(
                        "submit",
                        "submit_plan",
                        json!({"title":TITLE,"markdown":MARKDOWN}),
                    ),
                    ("plan-fixture", 3) => message("Independent Plan proposal published."),
                    ("plan-fixture", 4) => {
                        message("Prose-only feedback discussion; no new plan publication.")
                    }
                    ("plan-fixture", 5) => function(
                        "resubmit",
                        "submit_plan",
                        json!({"title":TITLE,"markdown":MARKDOWN}),
                    ),
                    ("plan-fixture", 6) => message("Revised proposal published."),
                    ("review-fixture", 1) => message(
                        "No actionable defects found in source.txt. Validation gap: this limited source inspection did not run builds or tests.",
                    ),
                    other => panic!("unexpected model request {other:?}: {request}"),
                };
                *step += 1;
                recorded.lock().unwrap().push(request);
                let response = json!({"type":"response.completed","sequence_number":1,"response":{
                    "id":format!("response-{model}-{step}"), "object":"response", "created_at":0,
                    "status":"completed", "error":null,"incomplete_details":null,"instructions":null,
                    "model":model,"max_output_tokens":null,"usage":null,"output":[output],"tools":[]
                }});
                let body = format!("data: {response}\n\n");
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            task: Some(task),
        }
    }
}
impl Drop for ResponsesFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let result = task.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn inspection_response(
    request: &Value,
    model: &str,
    step: usize,
    fixture: &inspection::Fixture,
    url: &str,
    scratch: &mut BTreeMap<String, inspection::Scratch>,
) -> Value {
    let plan = model == "inspection-plan-fixture";
    let phase = step % if plan { 11 } else { 10 };
    let id = format!("investigate-{step}");
    let command = match phase {
        0 => fixture.read_command(),
        1 => fixture.home_read_command(),
        2 => fixture.missing_command(),
        3 => fixture.create_command(),
        4 => {
            let result = result_text(request, &format!("investigate-{}", step - 1));
            let owned = fixture.own_scratch(&result);
            let command = fixture.download_command(&owned.0, url);
            scratch.insert(model.into(), owned);
            command
        }
        5 => fixture.prepare_command(&scratch[model].0),
        6 => fixture.execute_command(&scratch[model].0),
        7 => {
            let result = result_text(request, &format!("investigate-{}", step - 1));
            assert!(result.contains(inspection::SCRIPT_RESULT), "{result}");
            assert!(result.contains(inspection::BUILD_RESULT), "{result}");
            scratch[model].assert_results(fixture);
            fixture.assert_unchanged();
            fixture.cleanup_command(&scratch[model].0)
        }
        8 => {
            assert!(!scratch[model].0.exists(), "cleanup before publication");
            // Scripted providers can request unadvertised tools. The existing
            // worker registry/policy must still reject structured mutation.
            return function(
                &id,
                "write",
                json!({"file_path":"source.txt", "content":"MUST NOT WRITE"}),
            );
        }
        9 if plan => {
            return function(
                &id,
                "submit_plan",
                json!({"title":TITLE,"markdown":MARKDOWN}),
            );
        }
        9 | 10 => {
            return message(
                "Investigated external diagnostic and scratch build/test; original source unchanged.",
            );
        }
        _ => unreachable!(),
    };
    function(&id, "command", json!({"command": command}))
}

fn result_text(request: &Value, id: &str) -> String {
    let output = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
        .unwrap_or_else(|| panic!("missing correlated result {id}: {request}"));
    output["output"]
        .as_str()
        .expect("text tool result")
        .to_string()
}

fn function(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":arguments.to_string(),"status":"completed"})
}
fn message(text: &str) -> Value {
    json!({"type":"message","id":"message-fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]})
}
fn config(path: &Path, url: &str) {
    config_with_models(path, url, "plan-fixture", "review-fixture");
}
fn config_with_models(path: &Path, url: &str, plan: &str, review: &str) {
    let mut source = format!(
        r#"
[providers.test]
base_url = "{url}"
api_key = "fixture-only"
supports_websockets = false
[providers.test.input_token_count]
enabled = false
"#
    );
    for model in [
        "build-must-not-run",
        plan,
        review,
        "explore-must-not-run",
        "builder-must-not-run",
    ] {
        source.push_str(&format!("\n[providers.test.models.\"{model}\"]\ncontext_window_tokens = 272000\nretained_user_tokens = 20000\nreasoning_levels = [\"low\", \"medium\", \"high\"]\nreasoning_summary_level = \"detailed\"\n"));
    }
    source.push_str(&format!(
        r#"
[modes]
build = {{ provider = "test", model = "build-must-not-run", reasoning_level = "medium" }}
plan = {{ provider = "test", model = "{plan}", reasoning_level = "medium" }}
review = {{ provider = "test", model = "{review}", reasoning_level = "medium" }}
explore = {{ provider = "test", model = "explore-must-not-run", reasoning_level = "medium" }}
builder = {{ provider = "test", model = "builder-must-not-run", reasoning_level = "medium" }}
"#,
    ));
    zevria_app::test_support::write_fixture(path, &source).unwrap();
}
fn successful(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

#[cfg(unix)]
#[test]
fn actual_worker_external_reads_and_private_scratch_survive_plan_review_load_and_feedback() {
    use zevria_instructions::prompts::{
        ENSEMBLE_WORKER_PLAN_INSTRUCTIONS, ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS,
        INSPECTION_POLICY_INSTRUCTIONS,
    };
    let files = Arc::new(inspection::Fixture::new());
    let download = inspection::HttpFixture::start();
    let provider = ResponsesFixture::with_inspection(Some((files.clone(), download.url.clone())));
    let configuration = files.home.join("worker-config.toml");
    config_with_models(
        &configuration,
        &provider.url,
        "inspection-plan-fixture",
        "inspection-review-fixture",
    );
    // Engine-owned logs are allowed writes, but must not append to the synthetic
    // home diagnostic that the investigative commands are proving unchanged.
    let mut config = std::fs::read_to_string(&configuration).unwrap();
    config.push_str(&format!(
        "\n[log]\ndirectory = {:?}\n",
        files.root.path().join("engine-logs").to_str().unwrap()
    ));
    std::fs::write(&configuration, &config).unwrap();
    let start_rpc = || {
        let mut rpc = Rpc::start_worker(&files.home, &files.workspace, &configuration);
        successful(rpc.request(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{"plan":{}}}),
        ));
        rpc
    };
    let mut rpc = start_rpc();
    let plan = successful(rpc.request(
        "session/new",
        json!({"cwd":files.workspace,"mcpServers":[]}),
    ))["sessionId"]
        .clone();
    successful(rpc.request(
        "session/set_mode",
        json!({"sessionId":plan,"modeId":"plan"}),
    ));
    const OLD: &str = "Older generic Zevria launch boilerplate: Do not modify files, create patches, write plan or artifact files, invoke mutating tools, or begin implementation.";
    successful(rpc.request("session/prompt", json!({"sessionId":plan,"prompt":[{"type":"text","text":format!("{OLD}\nInvestigate source.txt and the external diagnostic under the current native workflow policy.")}]})));
    assert_inspection_notifications(&rpc, &plan, 0);
    successful(rpc.request("session/close", json!({"sessionId":plan})));
    drop(rpc);
    for (round, method) in ["session/load", "session/resume"].into_iter().enumerate() {
        let mut rpc = start_rpc();
        let before = provider.requests.lock().unwrap().len();
        let loaded = successful(rpc.request(
            method,
            json!({"sessionId":plan,"cwd":files.workspace,"mcpServers":[]}),
        ));
        assert_eq!(loaded["modes"]["currentModeId"], "plan");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            before,
            "load/replay is not model work"
        );
        rpc.notifications.clear();
        successful(rpc.request("session/prompt", json!({"sessionId":plan,"prompt":[{"type":"text","text":format!("Feedback after {method}: independently recheck the external diagnostic, fetch the controlled fixture into newly owned scratch, rerun validation, and republish the complete plan.")}]})));
        assert_inspection_notifications(&rpc, &plan, (round + 1) * 11);
        successful(rpc.request("session/close", json!({"sessionId":plan})));
    }
    let mut rpc = start_rpc();
    let review = successful(rpc.request(
        "session/new",
        json!({"cwd":files.workspace,"mcpServers":[]}),
    ))["sessionId"]
        .clone();
    successful(rpc.request("session/prompt", json!({"sessionId":review,"prompt":[{"type":"text","text":"Review source.txt using the external diagnostic and a contained validation experiment."}]})));
    assert_inspection_notifications(&rpc, &review, 0);
    assert!(
        !rpc.notifications
            .iter()
            .any(|packet| packet["params"]["update"]["sessionUpdate"] == "plan_update")
    );
    successful(rpc.request("session/close", json!({"sessionId":review})));
    drop(rpc);
    let mut rpc = start_rpc();
    successful(rpc.request(
        "session/resume",
        json!({"sessionId":review,"cwd":files.workspace,"mcpServers":[]}),
    ));
    rpc.notifications.clear();
    successful(rpc.request("session/prompt", json!({"sessionId":review,"prompt":[{"type":"text","text":"Feedback: recheck the same evidence in fresh scratch after resume."}]})));
    assert_inspection_notifications(&rpc, &review, 10);
    successful(rpc.request("session/close", json!({"sessionId":review})));
    drop(rpc);
    files.assert_unchanged();
    assert_eq!(std::fs::read_to_string(configuration).unwrap(), config);
    assert!(!transcript::plans_dir(&files.workspace).exists());
    for session in [&plan, &review] {
        let records = transcript::load(
            &zevria_foundation::runtime_paths::workspace_state_root(&files.workspace).join(
                format!("ensemble-sessions/{}.jsonl", session.as_str().unwrap()),
            ),
        )
        .unwrap();
        assert!(
            !records
                .iter()
                .any(|item| matches!(item, transcript::TranscriptItem::Directive(_)))
        );
        assert!(records.iter().any(|item| matches!(item,
            transcript::TranscriptItem::ToolResults { metadata, .. }
                if metadata.iter().any(|tool| tool.tool_name == "write" && tool.outcome == zevria_foundation::ToolCallOutcome::Denied)
        )));
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 53);
    for request in requests.iter() {
        let authoritative = request["instructions"].as_str().unwrap();
        assert!(authoritative.starts_with("Zevria engine instructions."));
        assert!(
            !request["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| matches!(item["role"].as_str(), Some("developer" | "system")))
        );
        assert_eq!(
            authoritative
                .matches(ENSEMBLE_WORKER_PLAN_INSTRUCTIONS)
                .count()
                + authoritative
                    .matches(ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS)
                    .count(),
            1
        );
        assert_eq!(
            authoritative
                .matches(INSPECTION_POLICY_INSTRUCTIONS)
                .count(),
            1
        );
        assert!(
            authoritative.starts_with(zevria_instructions::prompts::ENGINE_PROTOCOL_INSTRUCTIONS)
        );
        assert!(
            !authoritative.contains("zevria-inspection."),
            "scratch paths are history data, not instructions"
        );
        assert!(
            !request["instructions"]
                .as_str()
                .unwrap()
                .contains("SCRATCH=")
        );
        let tools = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(tools.contains(&"command"));
        for absent in [
            "write",
            "edit",
            "delete",
            "skill",
            "skill_read",
            "launch_subtasks",
        ] {
            assert!(!tools.contains(&absent));
        }
    }
    assert!(requests[0]["prompt_cache_key"].is_string());
    for index in [11, 22] {
        assert_eq!(
            requests[index]["prompt_cache_key"], requests[0]["prompt_cache_key"],
            "same session/profile cache identity after resume"
        );
    }
    assert_eq!(
        requests[33]["prompt_cache_key"],
        requests[43]["prompt_cache_key"]
    );
    assert_ne!(
        requests[0]["prompt_cache_key"],
        requests[33]["prompt_cache_key"]
    );
    assert!(requests[11]["input"].to_string().contains(OLD));
    assert!(
        requests[11]["input"]
            .to_string()
            .contains("Feedback after session/load")
    );
    assert!(
        requests[22]["input"]
            .to_string()
            .contains("Feedback after session/resume")
    );
    assert!(result_text(&requests[7], "investigate-6").contains(inspection::BUILD_RESULT));
}

/// Opt-in live process smoke: real supervisor, freshly exec'd native ACP worker,
/// real command tool, loopback fixtures and root synthesis. The provider remains
/// scripted; this does not establish compliance by a production model/adapter.
#[cfg(unix)]
#[test]
#[ignore = "run explicitly after the workspace suite for a fresh native-worker/parent smoke"]
fn fresh_native_worker_and_parent_synthesis_inspection_smoke() {
    const CHILD_CONFIG: &str = "ZEVRIA_INSPECTION_SMOKE_CONFIG";
    if let Some(path) = std::env::var_os(CHILD_CONFIG) {
        let config = zevria_app::test_support::load_config(Path::new(&path)).unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut session = zevria_app::runtime::start_session(
                &config, &std::env::current_dir().unwrap(), zevria_app::runtime::SessionStart::New { inherited_models: None },
            ).await.unwrap();
            let mut events = session.take_event_receiver().unwrap();
            session.command_sender().send(zevria_session_api::SessionCommand::Turn(
                zevria_session_api::TurnCommand::RunEnsemble {
                    workflow: zevria_workflow::EnsembleWorkflow::Review,
                    prompt: "Inspect source.txt and the external synthetic home diagnostic; fetch the controlled loopback fixture into private OS-temp scratch and run the contained validation. Leave source unchanged, clean up owned scratch, then independently verify the evidence in parent synthesis.".into(),
                },
            )).unwrap();
            tokio::time::timeout(Duration::from_secs(60), async {
                while let Some(update) = events.recv().await {
                    if let zevria_session_api::SessionUpdate::Lifecycle(event) = update {
                        match event {
                            zevria_session_api::SessionEvent::TurnCompleted { message, .. } => {
                                assert!(format!("{message:?}").contains("Parent synthesis independently verified"));
                                return;
                            }
                            zevria_session_api::SessionEvent::TurnFailed { error, .. }
                            | zevria_session_api::SessionEvent::TurnRejected { error, .. } => panic!("smoke failed: {error}"),
                            _ => {}
                        }
                    }
                }
                panic!("smoke ended without parent synthesis");
            }).await.expect("native ensemble smoke deadline");
            session.shutdown().await.unwrap();
        });
        return;
    }
    let files = Arc::new(inspection::Fixture::new());
    let download = inspection::HttpFixture::start();
    let provider = ResponsesFixture::with_inspection(Some((files.clone(), download.url.clone())));
    let worker_config = files.home.join("worker.toml");
    config_with_models(
        &worker_config,
        &provider.url,
        "inspection-plan-fixture",
        "inspection-review-fixture",
    );
    let mut worker = std::fs::read_to_string(&worker_config).unwrap();
    worker.push_str(&format!(
        "\n[log]\ndirectory = {:?}\n",
        files.root.path().join("engine-logs").to_str().unwrap()
    ));
    std::fs::write(&worker_config, worker).unwrap();
    let parent_config = files.home.join("parent.toml");
    config_with_models(
        &parent_config,
        &provider.url,
        "plan-fixture",
        "synthesis-review-fixture",
    );
    let mut parent = std::fs::read_to_string(&parent_config).unwrap();
    parent.push_str(&format!(
        r#"
[ensemble]
plan_agents = ["native-fixture"]
review_agents = ["native-fixture"]
[ensemble.agents.native-fixture]
label = "Fresh native inspection worker"
command = {:?}
args = ["--acp", "--ensemble-worker"]
plan_mode = "plan"
review_mode = "review"
[ensemble.agents.native-fixture.env]
HOME = {:?}
ZEVRIA_CONFIG = {:?}
"#,
        env!("CARGO_BIN_EXE_zevria"),
        files.home.to_str().unwrap(),
        worker_config.to_str().unwrap()
    ));
    std::fs::write(&parent_config, parent).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "acp_worker::fresh_native_worker_and_parent_synthesis_inspection_smoke",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&files.workspace)
        .env("HOME", &files.home)
        .env("ZEVRIA_CONFIG", &parent_config)
        .env(CHILD_CONFIG, &parent_config)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    files.assert_unchanged();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 12);
    let worker = requests
        .iter()
        .find(|request| request["model"] == "inspection-review-fixture")
        .unwrap();
    let envelope = worker["input"].to_string();
    assert!(envelope.contains("only if your own active policy permits them"));
    assert!(envelope.contains("no additional capabilities or ACP mutation permissions"));
    let parent = requests
        .iter()
        .find(|request| request["model"] == "synthesis-review-fixture")
        .unwrap();
    assert!(
        parent["input"]
            .to_string()
            .contains("independently inspect the current repository")
    );
    assert!(
        parent["input"]
            .to_string()
            .contains("source-read-only with temporary investigative execution")
    );
    assert_eq!(parent["tools"].as_array().unwrap().len(), 1);
    println!(
        "Fresh native worker and parent synthesis passed: external reads, loopback download, scratch script/build/test, cleanup, and unchanged protected fixtures (scripted provider)."
    );
}

#[cfg(unix)]
fn assert_inspection_notifications(rpc: &Rpc, session: &Value, start: usize) {
    for (offset, expected) in [
        (0, inspection::DIAGNOSTIC.trim()),
        (1, inspection::DIAGNOSTIC.trim()),
        (2, "missing-diagnostic"),
        (3, "SCRATCH="),
        (4, "DOWNLOADED=controlled fixture: 41"),
        (5, "INSPECTED=controlled fixture: 41"),
        (6, inspection::SCRIPT_RESULT),
        (6, inspection::BUILD_RESULT),
        (7, "owned scratch cleaned"),
    ] {
        let id = format!("investigate-{}", start + offset);
        let packets = rpc
            .notifications
            .iter()
            .filter(|packet| packet["params"]["sessionId"] == *session)
            .map(|packet| &packet["params"]["update"])
            .collect::<Vec<_>>();
        assert!(
            packets
                .iter()
                .any(|update| update["toolCallId"] == id
                    && update["rawInput"]["command"].is_string()),
            "missing correlated command {id}"
        );
        assert!(
            packets.iter().any(|update| update["toolCallId"] == id
                && update["rawOutput"]
                    .as_str()
                    .is_some_and(|text| text.contains(expected))),
            "missing correlated result {id}: {expected}"
        );
    }
}

#[test]
fn actual_worker_routes_models_questions_and_proof_and_recovers_without_mutating_project() {
    let fixture = ResponsesFixture::start();
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let configuration = home.path().join("inherited-config.toml");
    config(&configuration, &fixture.url);
    let source = workspace.path().join("source.txt");
    std::fs::write(&source, "source remains unchanged\n").unwrap();
    let skills = workspace.path().join(".zevria/skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("unsafe.md"),
        "---\ndescription: Unsafe fixture\n---\nPRIVATE_SKILL_MUST_NOT_MATERIALIZE\n",
    )
    .unwrap();
    let mut rpc = Rpc::start_worker(home.path(), workspace.path(), &configuration);
    let initialized = successful(rpc.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{"plan":{},"elicitation":{"form":{}}}}),
    ));
    assert!(!initialized.to_string().contains("zevria.skills"));
    assert!(!initialized.to_string().contains("zevria.orchestration"));
    let created = successful(rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    ));
    let plan_id = created["sessionId"].clone();
    assert_eq!(created["modes"]["currentModeId"], "review");
    assert!(!created["modes"].to_string().contains("orchestrate"));
    for mode in ["build", "orchestrate"] {
        assert!(
            rpc.request(
                "session/set_mode",
                json!({"sessionId":plan_id,"modeId":mode})
            )
            .get("error")
            .is_some()
        );
    }
    assert!(
        rpc.request(
            "session/prompt",
            json!({"sessionId":plan_id,"prompt":[{"type":"text","text":"delegate independent work"}],"_meta":{"zevria.orchestration":{"version":1,"enabled":true}}})
        )
        .get("error")
        .is_some()
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    let reload = rpc.request(
        "_zevria/skills/reload",
        json!({"version":1,"sessionId":plan_id,"expectedRevision":"any"}),
    );
    assert_eq!(reload["error"]["code"], -32602);
    assert!(
        reload["error"]
            .to_string()
            .contains("skill management is disabled for ensemble workers"),
        "{reload}"
    );
    successful(rpc.request(
        "session/set_mode",
        json!({"sessionId":plan_id,"modeId":"plan"}),
    ));
    let prompted = successful(rpc.request("session/prompt", json!({"sessionId":plan_id,"prompt":[{"type":"text","text":"Inspect source.txt and plan the change."}]})));
    assert_eq!(prompted["stopReason"], "end_turn");
    assert_eq!(rpc.questions.len(), 1);
    let proof = rpc
        .notifications
        .iter()
        .find(|packet| packet["params"]["update"]["sessionUpdate"] == "plan_update")
        .unwrap()["params"]["update"]["plan"]
        .clone();
    assert_eq!(proof["type"], "markdown");
    assert_eq!(proof["content"], MARKDOWN);
    assert!(
        rpc.request(
            "session/prompt",
            json!({"sessionId":plan_id,"prompt":[{"type":"text","text":"/implement"}]})
        )
        .get("error")
        .is_some()
    );
    successful(rpc.request("session/close", json!({"sessionId":plan_id})));
    drop(rpc);

    // Restoration replays retained proof without generation. Genuine follow-up
    // runs another model request in this exact session, including prior context.
    let count = fixture.requests.lock().unwrap().len();
    let mut rpc = Rpc::start_worker(home.path(), workspace.path(), &configuration);
    successful(rpc.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{"plan":{}}}),
    ));
    for method in ["session/load", "session/resume"] {
        rpc.notifications.clear();
        let restored = successful(rpc.request(
            method,
            json!({"sessionId":plan_id,"cwd":workspace.path(),"mcpServers":[]}),
        ));
        assert_eq!(restored["modes"]["currentModeId"], "plan");
        assert!(
            rpc.notifications
                .iter()
                .any(|packet| packet["params"]["update"]["plan"] == proof)
        );
        let before_feedback = fixture.requests.lock().unwrap().len();
        assert_eq!(
            before_feedback,
            count + usize::from(method == "session/resume")
        );
        successful(rpc.request(
            "session/set_mode",
            json!({"sessionId":plan_id,"modeId":"plan"}),
        ));
        successful(rpc.request(
            "session/prompt",
            json!({"sessionId":plan_id,"prompt":[{"type":"text","text":"continue"}]}),
        ));
        successful(rpc.request("session/close", json!({"sessionId":plan_id})));
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), count + 3);
    let review = successful(rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    ))["sessionId"]
        .clone();
    rpc.notifications.clear();
    successful(rpc.request(
        "session/prompt",
        json!({"sessionId":review,"prompt":[{"type":"text","text":"Review source.txt."}]}),
    ));
    assert!(rpc.notifications.iter().any(|packet| {
        packet["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("No actionable defects"))
    }));
    assert!(
        !rpc.notifications
            .iter()
            .any(|packet| packet["params"]["update"]["sessionUpdate"] == "plan_update")
    );
    successful(rpc.request("session/close", json!({"sessionId":review})));
    drop(rpc);
    assert_eq!(
        std::fs::read_to_string(source).unwrap(),
        "source remains unchanged\n"
    );
    assert!(
        transcript::list_sessions(&transcript::sessions_dir(workspace.path()))
            .unwrap()
            .is_empty()
    );
    assert!(
        transcript::latest_session_file(&transcript::sessions_dir(workspace.path()))
            .unwrap()
            .is_none()
    );
    let worker_dir = zevria_foundation::runtime_paths::workspace_state_root(workspace.path())
        .join("ensemble-sessions");
    assert_eq!(transcript::list_sessions(&worker_dir).unwrap().len(), 2);
    let records =
        transcript::load(&worker_dir.join(format!("{}.jsonl", plan_id.as_str().unwrap()))).unwrap();
    assert!(records.iter().any(|item| matches!(item, transcript::TranscriptItem::Plan(zevria_workflow::PlanRecord::Ready { artifact }) if artifact.markdown == MARKDOWN)));
    for tool in ["command", "question", "submit_plan"] {
        assert!(records.iter().any(|item| matches!(item,
            transcript::TranscriptItem::ToolResults { metadata, .. }
                if metadata.iter().any(|result| result.tool_name == tool && result.outcome == zevria_foundation::ToolCallOutcome::Success)
        )), "{tool} must actually complete successfully");
    }
    assert!(!transcript::plans_dir(workspace.path()).exists());
    assert!(
        !home.path().join(".zevria/config.toml").exists(),
        "ZEVRIA_CONFIG is inherited without credential copying"
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 9);
    assert!(requests[4]["input"].to_string().contains("continue"));
    assert!(requests[4]["input"].to_string().contains("Preserve"));
    assert!(
        requests[1]["input"]
            .to_string()
            .contains("source remains unchanged")
    );
    assert!(requests[2]["input"].to_string().contains("Preserve"));
    for request in requests.iter() {
        let mut tools = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        tools.sort();
        let expected = if request["model"] == "plan-fixture" {
            vec!["command", "question", "submit_plan"]
        } else {
            vec!["command", "question"]
        };
        assert_eq!(tools, expected, "{request}");
        assert!(
            !request
                .to_string()
                .contains("PRIVATE_SKILL_MUST_NOT_MATERIALIZE")
        );
    }
}
