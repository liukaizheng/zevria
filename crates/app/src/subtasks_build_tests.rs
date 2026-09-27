use super::*;

#[path = "subtasks_cleanup_tests.rs"]
mod cleanup_tests;
use crate::{
    config::{CommandConfig, Config},
    runtime::{build_isolated_build_tools, build_session_policies, build_tools},
};
use rig_agent::tool::ToolContext;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use zevria_foundation::ToolResultDetail;
use zevria_foundation::TurnId;
use zevria_provider::ResponsesRouter;
use zevria_session_api::TurnContext;

async fn execute(
    tools: &ToolServerHandle,
    name: &str,
    args: Value,
) -> (rig_core::tool::ToolResult, ToolContext) {
    let mut context = ToolContext::new();
    let result = tools.execute(name, &args.to_string(), &mut context).await;
    (result, context)
}

#[tokio::test]
async fn private_registries_have_exact_tools_and_independent_structured_roots() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(root.path().join("input.txt"), "untouched").unwrap();
    for (path, text) in [(&a, "first"), (&b, "second")] {
        let marker = format!("private-root-{text}");
        std::fs::write(path.join(".root-marker"), &marker).unwrap();
        let tools = build_isolated_build_tools(path, CommandConfig::default()).unwrap();
        assert_eq!(
            tools
                .static_tool_defs()
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["command", "task", "edit", "write", "delete"]
        );
        let (result, _) = execute(
            &tools,
            "command",
            json!({"command":"pwd && cat .root-marker"}),
        )
        .await;
        let rendered = result.output().render();
        assert!(result.is_success(), "{rendered}");
        assert!(rendered.contains(&marker), "{rendered}");
        let (result, context) = execute(
            &tools,
            "write",
            json!({"file_path":"same.txt","content":text}),
        )
        .await;
        assert!(result.is_success());
        assert_eq!(
            context.result::<ToolResultDetail>().unwrap().file_changes()[0].path,
            std::fs::canonicalize(path).unwrap().join("same.txt")
        );
        let (result, _) = execute(&tools, "edit", json!({"file_path":"same.txt","replacements":[{"old_string":text,"new_string":"edited"}],"move_to":"moved.txt"})).await;
        assert!(result.is_success(), "{}", result.output().render());
        assert!(!path.join("same.txt").exists());
        assert_eq!(
            std::fs::read_to_string(path.join("moved.txt")).unwrap(),
            "edited"
        );
        for outside in [
            "../input.txt".to_string(),
            root.path().join("input.txt").display().to_string(),
            root.path()
                .join(if path == &a {
                    "b/escape.txt"
                } else {
                    "a/escape.txt"
                })
                .display()
                .to_string(),
        ] {
            assert!(
                !execute(
                    &tools,
                    "write",
                    json!({"file_path":outside,"content":"bad"})
                )
                .await
                .0
                .is_success()
            );
            assert!(
                !execute(&tools, "delete", json!({"file_path":outside}))
                    .await
                    .0
                    .is_success()
            );
            assert!(
                !execute(
                    &tools,
                    "edit",
                    json!({"file_path":"moved.txt","replacements":[],"move_to":outside})
                )
                .await
                .0
                .is_success()
            );
        }
        assert!(
            !execute(
                &tools,
                "write",
                json!({"file_path":"missing/file","content":"no implicit parents"})
            )
            .await
            .0
            .is_success()
        );
        assert!(
            execute(&tools, "delete", json!({"file_path":"moved.txt"}))
                .await
                .0
                .is_success()
        );
        for name in [
            "question",
            "launch_subtasks",
            "skill",
            "skill_read",
            "submit_plan",
            "reconcile_reports",
        ] {
            assert!(!execute(&tools, name, json!({})).await.0.is_success());
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
        "untouched"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&b, a.join("escape")).unwrap();
        let tools = build_isolated_build_tools(&a, CommandConfig::default()).unwrap();
        assert!(
            !execute(
                &tools,
                "write",
                json!({"file_path":"escape/denied","content":"bad"})
            )
            .await
            .0
            .is_success()
        );
        assert!(!b.join("denied").exists());
    }
}

#[test]
fn builder_directive_and_permissions_are_fixed_in_all_modes() {
    let policies = build_policies(Path::new("/startup/pages/book"), Path::new("/startup"));
    for mode in [SessionMode::Build, SessionMode::Build, SessionMode::Plan] {
        let policy = policies.policy(mode);
        assert_eq!(policy.model_role, ModelRole::Builder);
        assert!(!policy.skills_enabled);
        assert_eq!(
            policy.allowed_tool_names.as_ref().unwrap(),
            &["command", "task", "edit", "write", "delete", "web_search"]
        );
        for pin in [
            "only inside your workspace",
            "unsandboxed shell",
            "final report",
            "Git, package installation, parent-project builds, or network access",
            "Authorization does not relax",
            "Do not rename, remove, or replace the workspace root",
            "Task-relative input paths default",
            "Shared command and file-tool descriptions",
        ] {
            assert!(policy.instructions.contains(pin), "missing {pin}");
        }
        let rendered = crate::runtime::rendered_instructions(policy);
        let declaration: serde_json::Value = serde_json::from_str(
            rendered
                .split_once("## Workflow policy: builder\n")
                .unwrap()
                .1
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            declaration["workspace"],
            serde_json::json!({"root": "/startup/pages/book", "startup": "/startup"})
        );
        assert!(!policy.orchestration);
        for denied in [
            "question",
            "launch_subtasks",
            "skill",
            "skill_read",
            "submit_plan",
            "reconcile_reports",
        ] {
            assert!(!policy.allows_tool(denied));
        }
    }
}

struct HttpCall {
    request: Value,
    reply: oneshot::Sender<Vec<Value>>,
}

async fn fixture() -> (String, mpsc::UnboundedReceiver<HttpCall>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(&mut stream);
                let mut length = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let length = length.unwrap();
                assert!(length < 4 * 1024 * 1024);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let model = request["model"].clone();
                let (reply, receive) = oneshot::channel();
                if tx.send(HttpCall { request, reply }).is_err() {
                    return;
                }
                let Ok(output) = receive.await else {
                    return;
                };
                let id = SubtaskId::generate().to_string();
                let response = json!({"type":"response.completed","sequence_number":1,"response":{
                    "id":id,"object":"response","created_at":0,"status":"completed","error":null,"incomplete_details":null,"instructions":null,"model":model,"max_output_tokens":null,"usage":null,"tools":[],"output":output
                }});
                let body = format!("data: {response}\n\n");
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            });
        }
    });
    (format!("http://{address}/v1/responses"), rx, server)
}

fn message(text: &str) -> Value {
    json!({"type":"message","id":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]})
}

fn function(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":args.to_string(),"status":"completed"})
}

fn config(url: &str) -> Config {
    config_with_parallel(url, None)
}

fn config_with_parallel(url: &str, parallel: Option<bool>) -> Config {
    let mut text = format!(
        "[providers.test]\nbase_url = {url:?}\napi_key = 'fixture'\nsupports_websockets = false\n[providers.test.input_token_count]\nenabled = false\n"
    );
    if let Some(parallel) = parallel {
        text.push_str(&format!(
            "[providers.test.additional_params]\nparallel_tool_calls = {parallel}\n"
        ));
    }
    for model in ["root-model", "explore-model", "builder-model"] {
        text.push_str(&format!("[providers.test.models.{model}]\ncontext_window_tokens = 272000\nretained_user_tokens = 20000\nreasoning_levels = ['low','medium','high']\nreasoning_summary_level = 'detailed'\n"));
    }
    text.push_str("[modes]\nbuild = {provider='test',model='root-model',reasoning_level='medium'}\nplan = {provider='test',model='root-model',reasoning_level='medium'}\nreview = {provider='test',model='root-model',reasoning_level='medium'}\nexplore = {provider='test',model='explore-model',reasoning_level='medium'}\nbuilder = {provider='test',model='builder-model',reasoning_level='medium'}\n");
    crate::test_support::parse_fixture(&text).unwrap()
}

fn supervisor_config(
    config: &Config,
    root: &Path,
    events: SessionEventSender,
    limit: usize,
) -> SubtaskSupervisorConfig {
    let models = &config;
    SubtaskSupervisorConfig {
        explore_factory: ResponsesRouterFactory::new(
            ModelRole::Explore,
            models.routing().for_role(ModelRole::Explore).clone(),
            models
                .routing()
                .selection_for_role(ModelRole::Explore)
                .reasoning_level,
            "fixture preamble",
        ),
        builder_factory: ResponsesRouterFactory::new(
            ModelRole::Builder,
            models.routing().for_role(ModelRole::Builder).clone(),
            models
                .routing()
                .selection_for_role(ModelRole::Builder)
                .reasoning_level,
            "fixture preamble",
        ),
        explore_tools: crate::runtime::build_explore_tools(root, config.command).unwrap(),
        startup_workspace: std::fs::canonicalize(root).unwrap(),
        command_config: config.command,
        events,
        subsessions_dir: root.join(".zevria/subsessions/root"),
        compaction: config.compaction_policy().clone(),
        max_concurrent_subtasks: limit,
        guidance: zevria_instructions::GuidanceSnapshot::default(),
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<HttpCall>) -> HttpCall {
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn production_build_rejects_builder_launch_before_workspace_or_child_provider() {
    let root = tempfile::tempdir().unwrap();
    let (url, mut http, server) = fixture().await;
    let config = config(&url);
    let (events, mut receiver) = session_event_channel(128);
    let channels = zevria_session_api::subtask_channels("root", events.clone());
    let supervisor = spawn_supervisor(
        channels.requests,
        supervisor_config(&config, root.path(), events.clone(), 1),
    );
    let tools = build_tools(
        root.path(),
        channels.launcher,
        zevria_session_api::question_channels(events.clone()).requester,
        config.command,
        config.session.plan.max_artifact_bytes,
    )
    .unwrap();
    let provider =
        ResponsesRouter::root(config.routing(), "fixture", tools.clone(), "root").unwrap();
    let transcript = TranscriptWriter::create_with_id(root.path(), "root").unwrap();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        build_session_policies(config.session.plan),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_mode_management()
    .with_compaction_policy(config.compaction_policy().clone());
    let engine_task = tokio::spawn(async move {
        engine
            .handle_turn(
                zevria_session_api::TurnCommand::Submit {
                    text: "implement one page".into(),
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    mode: SessionMode::Build,
                },
                &TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
                &events,
            )
            .await
    });
    let opening = next(&mut http).await;
    assert_eq!(opening.request["model"], "root-model");
    assert!(
        opening.request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "launch_subtasks"),
        "Build retains launch_subtasks for Explore"
    );
    opening
        .reply
        .send(vec![function(
            "denied-builder",
            "launch_subtasks",
            json!({"tasks":[{"title":"Build page","prompt":"write index.html","type":"build","workspace":"pages/book"}]}),
        )])
        .unwrap();
    let continuation = next(&mut http).await;
    assert_eq!(
        continuation.request["model"], "root-model",
        "a denied Build request must never call a child provider"
    );
    let outputs = continuation.request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["call_id"], "denied-builder");
    assert!(
        outputs[0]["output"]
            .to_string()
            .contains("/orchestrate <prompt>")
    );
    continuation
        .reply
        .send(vec![message("implemented directly")])
        .unwrap();
    engine_task.await.unwrap().unwrap();
    assert!(http.try_recv().is_err());
    assert!(!root.path().join("pages").exists());
    assert!(!root.path().join(".zevria/subsessions/root").exists());
    assert!(
        zevria_transcript::subtask_launch_metadata(
            &zevria_transcript::transcript::load(&path).unwrap()
        )
        .is_empty()
    );
    while let Ok(update) = receiver.try_recv() {
        assert!(
            !matches!(
                update,
                SessionUpdate::Lifecycle(
                    SessionEvent::SubtaskLaunched { .. }
                        | SessionEvent::SubtaskStatus { .. }
                        | SessionEvent::SubtaskSession { .. }
                )
            ),
            "denied launches must not publish a child lifecycle"
        );
    }
    supervisor.abort();
    let _ = supervisor.await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn ten_real_response_launches_share_limits_and_return_original_slots() {
    for (limit, mixed, separate, parallel) in [
        (
            crate::session_config::SessionConfig::default().max_concurrent_subtasks,
            false,
            false,
            None,
        ),
        (4, false, true, None),
        (4, true, false, Some(false)),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (url, mut http, server) = fixture().await;
        let config = config_with_parallel(&url, parallel);
        let (events, _receiver) = session_event_channel(512);
        let channels =
            zevria_session_api::subtask_channels_with_capacity("root", events.clone(), 2);
        let supervisor = spawn_supervisor(
            channels.requests,
            supervisor_config(&config, root.path(), events.clone(), limit),
        );
        let tools = build_tools(
            root.path(),
            channels.launcher.clone(),
            zevria_session_api::question_channels(events.clone()).requester,
            config.command,
            config.session.plan.max_artifact_bytes,
        )
        .unwrap();
        let models = &config;
        let provider =
            ResponsesRouter::root(models.routing(), "fixture", tools.clone(), "root").unwrap();
        let transcript = TranscriptWriter::create_with_id(root.path(), "root").unwrap();
        let path = transcript.path().to_path_buf();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            build_session_policies(config.session.plan),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_mode_management()
        .with_subtask_concurrency(limit)
        .with_compaction_policy(config.compaction_policy().clone());
        let engine_task = tokio::spawn(async move {
            engine
                .handle_turn(
                    zevria_session_api::TurnCommand::Submit {
                        text: "ten books".into(),
                        behavior: zevria_foundation::RequestBehavior::Orchestrate,
                        mode: SessionMode::Build,
                    },
                    &TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new())
                        .with_build_subtasks(true),
                    &events,
                )
                .await
        });
        let call = next(&mut http).await;
        let launch_schema = call.request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "launch_subtasks")
            .unwrap();
        assert_eq!(launch_schema["strict"], true);
        assert!(
            launch_schema["parameters"]["properties"]["tasks"]["items"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("workspace")),
            "strict Responses makes optional fields required-nullable"
        );
        assert_eq!(
            call.request.get("parallel_tool_calls"),
            parallel.as_ref().map(|_| &Value::Bool(false))
        );
        let tasks = (0..10).map(|i| {
            let mut args = json!({"title":format!("Book {i}"),"prompt":format!("BOOK_TASK_{i}"),"type":if !mixed || i % 2 == 0 { "build" } else { "explore" }});
            if !mixed || i % 2 == 0 { args["workspace"] = json!(format!("pages/book-{i}")); }
            else if i % 4 == 1 { args["workspace"] = Value::Null; }
            args
        }).collect::<Vec<_>>();
        let output = if separate {
            tasks
                .into_iter()
                .enumerate()
                .map(|(i, task)| {
                    function(
                        &format!("launch-{i}"),
                        "launch_subtasks",
                        json!({"tasks":[task]}),
                    )
                })
                .collect()
        } else {
            vec![function(
                "launch-batch",
                "launch_subtasks",
                json!({"tasks": tasks}),
            )]
        };
        call.reply.send(output).unwrap();
        let mut seen = 0;
        let mut caches = std::collections::BTreeSet::new();
        while seen < 10 {
            let mut wave = Vec::new();
            for _ in 0..limit.min(10 - seen) {
                wave.push(next(&mut http).await);
            }
            assert!(
                http.try_recv().is_err(),
                "no child beyond execution limit may call its provider"
            );
            for call in wave.into_iter().rev() {
                let input = call.request["input"].to_string();
                let i = (0..10)
                    .find(|i| input.contains(&format!("BOOK_TASK_{i}")))
                    .unwrap();
                assert_eq!(
                    call.request["model"],
                    if !mixed || i % 2 == 0 {
                        "builder-model"
                    } else {
                        "explore-model"
                    }
                );
                let names = call.request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["name"].as_str().unwrap())
                    .collect::<Vec<_>>();
                if !mixed || i % 2 == 0 {
                    assert_eq!(names, ["command", "task", "edit", "write", "delete"]);
                    let declaration: serde_json::Value = serde_json::from_str(
                        call.request["instructions"]
                            .as_str()
                            .unwrap()
                            .split_once("## Workflow policy: builder\n")
                            .unwrap()
                            .1
                            .lines()
                            .next()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        declaration["workspace"],
                        serde_json::json!({
                            "root": std::fs::canonicalize(root.path().join(format!("pages/book-{i}"))).unwrap(),
                            "startup": std::fs::canonicalize(root.path()).unwrap(),
                        })
                    );
                } else {
                    assert_eq!(names, ["command"]);
                }
                assert!(call.request.get("previous_response_id").is_none());
                assert!(
                    caches.insert(
                        call.request["prompt_cache_key"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    )
                );
                call.reply
                    .send(vec![message(&format!("REPORT_{i}"))])
                    .unwrap();
                seen += 1;
            }
        }
        let root_continuation = next(&mut http).await;
        assert_eq!(root_continuation.request["model"], "root-model");
        let input = root_continuation.request["input"].as_array().unwrap();
        let results = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(
            call.request["instructions"],
            root_continuation.request["instructions"]
        );
        assert_eq!(call.request["tools"], root_continuation.request["tools"]);
        assert_eq!(
            call.request["prompt_cache_key"],
            root_continuation.request["prompt_cache_key"]
        );
        assert_eq!(results.len(), if separate { 10 } else { 1 });
        for i in 0..10 {
            let result = results[if separate { i } else { 0 }];
            assert_eq!(
                result["call_id"],
                if separate {
                    format!("launch-{i}")
                } else {
                    "launch-batch".into()
                }
            );
            assert!(
                result["output"]
                    .to_string()
                    .contains(&format!("REPORT_{i}"))
            );
            assert!(!result["output"].to_string().contains("queue is full"));
        }
        root_continuation
            .reply
            .send(vec![message("all done")])
            .unwrap();
        if separate {
            let correction = next(&mut http).await;
            assert!(
                correction.request["input"]
                    .to_string()
                    .contains("one engine correction")
            );
            correction
                .reply
                .send(vec![message("cannot decompose further")])
                .unwrap();
        }
        engine_task.await.unwrap().unwrap();
        let items = zevria_transcript::transcript::load(&path).unwrap();
        let metadata = zevria_transcript::subtask_launch_metadata(&items);
        assert_eq!(metadata.len(), 10);
        for child in metadata.values() {
            assert!(
                root.path()
                    .join(format!(".zevria/subsessions/root/{}.jsonl", child.id))
                    .is_file()
            );
            if let Some(path) = &child.workspace {
                assert!(
                    channels
                        .launcher
                        .reserve_workspace(root.path().join(path))
                        .is_ok()
                );
            }
        }
        supervisor.abort();
        let _ = supervisor.await;
        server.abort();
    }
}
