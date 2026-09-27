use super::*;
use rig_agent::tool::Tool;
use zevria_tools::{LaunchSubtaskSpec, LaunchSubtaskType, LaunchSubtasksArgs, LaunchSubtasksTool};

#[cfg(unix)]
#[tokio::test]
async fn cancellation_and_supervisor_abort_quiesce_commands_before_workspace_reuse() {
    for abort in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (url, mut http, server) = fixture().await;
        let config = config(&url);
        let (events, _receiver) = session_event_channel(512);
        let channels = zevria_session_api::subtask_channels("root", events.clone());
        let supervisor = spawn_supervisor(
            channels.requests,
            supervisor_config(&config, root.path(), events, 1),
        );
        let tool = LaunchSubtasksTool::new(channels.launcher.clone(), root.path().to_path_buf());
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        let call = tokio::spawn(async move {
            let mut context = ToolContext::new();
            context.insert(
                TurnContext::new(TurnId::new(1), SessionMode::Build, token)
                    .with_build_subtasks(true),
            );
            let result = tool
                .call(
                    &mut context,
                    LaunchSubtasksArgs {
                        tasks: vec![LaunchSubtaskSpec {
                            title: "Build page".into(),
                            prompt: "create page".into(),
                            r#type: LaunchSubtaskType::Build,
                            workspace: Some("page".into()),
                        }],
                    },
                )
                .await;
            (result, context)
        });
        let provider_call = next(&mut http).await;
        assert_eq!(provider_call.request["model"], "builder-model");
        provider_call.reply.send(vec![function("command", "command", json!({"command":"printf '%s' $$ > pid; printf ready > ready; while [ ! -f release ]; do sleep 0.05; done; printf stale > result"}))]).unwrap();
        let path = root.path().join("page");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !path.join("ready").is_file() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(channels.launcher.reserve_workspace(&path).is_err());
        if abort {
            supervisor.abort();
        } else {
            cancellation.cancel();
        }
        let (result, context) = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert_eq!(
            context
                .result::<ToolResultDetail>()
                .and_then(|detail| detail.subtasks().first())
                .and_then(|entry| entry.launch.as_ref())
                .unwrap()
                .workspace
                .as_deref(),
            Some("page")
        );
        if !abort {
            assert!(
                context
                    .result::<zevria_foundation::ToolCancelled>()
                    .is_some()
            );
        }
        // Aborting the JoinSet drops its unresolved oneshot immediately, but
        // the separately owned execution keeps the reservation until cleanup.
        let reservation = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(guard) = channels.launcher.reserve_workspace(&path) {
                    break guard;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let pid = std::fs::read_to_string(path.join("pid")).unwrap();
        let process = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap();
        assert!(
            !process.status.success(),
            "old shell must be reaped before reuse"
        );
        let tools = build_isolated_build_tools(&path, CommandConfig::default()).unwrap();
        assert!(
            execute(
                &tools,
                "write",
                json!({"file_path":"result","content":"new owner"})
            )
            .await
            .0
            .is_success()
        );
        assert!(
            execute(
                &tools,
                "command",
                json!({"command":"touch release; sleep 0.15; cat result"})
            )
            .await
            .0
            .output()
            .render()
            .contains("new owner")
        );
        assert_eq!(
            std::fs::read_to_string(path.join("result")).unwrap(),
            "new owner"
        );
        drop(reservation);
        supervisor.abort();
        let _ = supervisor.await;
        server.abort();
    }
}

#[tokio::test]
async fn permit_wait_cancel_and_execution_failure_release_before_backpressured_terminal_delivery() {
    for preparation_failure in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (events, mut receiver) = session_event_channel(1);
        let mut channels = zevria_session_api::subtask_channels("root", events.clone());
        let config = config("http://127.0.0.1:1/v1/responses");
        let mut config = supervisor_config(&config, root.path(), events, 1);
        if preparation_failure {
            config.subsessions_dir = root.path().join("not-directory");
            std::fs::write(&config.subsessions_dir, "block transcript creation").unwrap();
        }
        let path = root.path().join("child");
        let reservation = channels.launcher.reserve_workspace(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let workspace = ChildWorkspace::new(
            std::fs::canonicalize(&path).unwrap(),
            "child".into(),
            reservation,
        )
        .unwrap();
        let (_, outcome) = channels
            .launcher
            .launch(
                "call",
                0,
                "child",
                SubtaskKind::Build,
                "prompt",
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new())
                    .with_build_subtasks(true),
                Some(workspace),
            )
            .await
            .unwrap();
        // The accepted launch fills the event channel. finish_cancelled must send
        // its outcome and release ownership without room for the status event.
        let request = channels.requests.recv().await.unwrap();
        let terminal = tokio::spawn(async move {
            if preparation_failure {
                let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
                run_child(request, config, permit).await;
            } else {
                finish_cancelled(request, &config).await;
            }
        });
        let outcome = outcome.await.unwrap();
        if preparation_failure {
            assert!(matches!(outcome, SubtaskOutcome::Failed { .. }));
        } else {
            assert_eq!(outcome, SubtaskOutcome::Cancelled);
        }
        assert!(channels.launcher.reserve_workspace(&path).is_ok());
        assert!(!terminal.is_finished());
        receiver.recv().await.unwrap();
        terminal.await.unwrap();
    }
}

#[tokio::test]
async fn a_cancelled_permit_waiter_never_starts_and_releases_its_workspace() {
    let root = tempfile::tempdir().unwrap();
    let (url, mut http, server) = fixture().await;
    let config = config(&url);
    let (events, _receiver) = session_event_channel(128);
    let channels = zevria_session_api::subtask_channels("root", events.clone());
    let supervisor = spawn_supervisor(
        channels.requests,
        supervisor_config(&config, root.path(), events, 1),
    );
    let workspace = |name: &str| {
        let path = root.path().join(name);
        let guard = channels.launcher.reserve_workspace(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        ChildWorkspace::new(std::fs::canonicalize(path).unwrap(), name.into(), guard).unwrap()
    };
    let running = CancellationToken::new();
    let (_, active) = channels
        .launcher
        .launch(
            "active",
            0,
            "active",
            SubtaskKind::Build,
            "hold the permit",
            TurnContext::new(TurnId::new(1), SessionMode::Build, running.clone())
                .with_build_subtasks(true),
            Some(workspace("active")),
        )
        .await
        .unwrap();
    let parked = next(&mut http).await;
    let waiting = CancellationToken::new();
    let (metadata, waiter) = channels
        .launcher
        .launch(
            "waiter",
            0,
            "waiter",
            SubtaskKind::Build,
            "wait",
            TurnContext::new(TurnId::new(2), SessionMode::Build, waiting.clone())
                .with_build_subtasks(true),
            Some(workspace("waiter")),
        )
        .await
        .unwrap();
    waiting.cancel();
    assert_eq!(waiter.await.unwrap(), SubtaskOutcome::Cancelled);
    assert!(
        channels
            .launcher
            .reserve_workspace(root.path().join("waiter"))
            .is_ok()
    );
    assert!(
        root.path().join("waiter").is_dir(),
        "partial directory is retained"
    );
    assert!(
        !root
            .path()
            .join(format!(".zevria/subsessions/root/{}.jsonl", metadata.id))
            .exists()
    );
    assert!(http.try_recv().is_err());
    running.cancel();
    assert_eq!(active.await.unwrap(), SubtaskOutcome::Cancelled);
    drop(parked);
    supervisor.abort();
    let _ = supervisor.await;
    server.abort();
}

#[tokio::test]
async fn build_tools_execute_with_inherited_guidance_and_persist_artifacts_and_builder_role() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input.txt"), "startup input").unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "PARENT_SNAPSHOT").unwrap();
    std::fs::create_dir(root.path().join("page")).unwrap();
    std::fs::write(
        root.path().join("page/AGENTS.md"),
        "CHILD_GUIDANCE_MUST_NOT_LOAD",
    )
    .unwrap();
    let snapshot = zevria_instructions::load_guidance(
        &zevria_instructions::GuidanceRoots::fixture(None, root.path()),
    );
    let (url, mut http, server) = fixture().await;
    let config = config(&url);
    let (events, _receiver) = session_event_channel(512);
    let channels = zevria_session_api::subtask_channels("root", events.clone());
    let mut child_config = supervisor_config(&config, root.path(), events, 1);
    child_config.guidance = snapshot;
    let supervisor = spawn_supervisor(channels.requests, child_config);
    std::fs::write(root.path().join("AGENTS.md"), "LATE_GUIDANCE_MUST_NOT_LOAD").unwrap();
    let tool = LaunchSubtasksTool::new(channels.launcher.clone(), root.path().to_path_buf());
    let call = tokio::spawn(async move {
        let mut context = ToolContext::new();
        context.insert(
            TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new())
                .with_build_subtasks(true),
        );
        let result = tool
            .call(
                &mut context,
                LaunchSubtasksArgs {
                    tasks: vec![LaunchSubtaskSpec {
                        title: "Build page".into(),
                        prompt: "Read input.txt from startup; create index.html here.".into(),
                        r#type: LaunchSubtaskType::Build,
                        workspace: Some("page".into()),
                    }],
                },
            )
            .await;
        (result, context)
    });
    let first = next(&mut http).await;
    let input = first.request["instructions"].to_string();
    assert!(
        !first.request["input"]
            .to_string()
            .contains("PARENT_SNAPSHOT")
    );
    assert!(input.contains("PARENT_SNAPSHOT"));
    assert!(!input.contains("CHILD_GUIDANCE_MUST_NOT_LOAD"));
    assert!(!input.contains("LATE_GUIDANCE_MUST_NOT_LOAD"));
    assert_eq!(first.request["model"], "builder-model");
    assert!(input.contains("builder"));
    first
        .reply
        .send(vec![
            function("cwd", "command", json!({"command":"pwd"})),
            function(
                "write",
                "write",
                json!({"file_path":"index.html","content":"<h1>Book</h1>"}),
            ),
        ])
        .unwrap();
    let second = next(&mut http).await;
    assert_eq!(second.request["model"], "builder-model");
    assert_eq!(
        first.request["prompt_cache_key"],
        second.request["prompt_cache_key"]
    );
    assert!(
        second.request["input"]
            .to_string()
            .contains("wrote 13 bytes")
    );
    second
        .reply
        .send(vec![message(
            "Created index.html; validated with write output and cwd, no browser test.",
        )])
        .unwrap();
    let (result, context) = call.await.unwrap();
    assert!(result.unwrap().contains("workspace: page"));
    let metadata = context
        .result::<ToolResultDetail>()
        .and_then(|detail| detail.subtasks().first())
        .and_then(|entry| entry.launch.as_ref())
        .unwrap();
    let items = zevria_transcript::transcript::load(
        &root
            .path()
            .join(format!(".zevria/subsessions/root/{}.jsonl", metadata.id)),
    )
    .unwrap();
    let state = zevria_transcript::replay_directives(&items)
        .unwrap()
        .snapshot();
    assert!(state.directives.is_empty());
    assert!(!format!("{items:?}").contains("PARENT_SNAPSHOT"));
    assert!(items.iter().any(|item| matches!(item, zevria_transcript::transcript::TranscriptItem::ToolResults { metadata, .. } if metadata.iter().any(|m| !m.file_changes().is_empty()))));
    assert_eq!(
        std::fs::read_to_string(root.path().join("input.txt")).unwrap(),
        "startup input"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("page/index.html")).unwrap(),
        "<h1>Book</h1>"
    );
    assert!(
        channels
            .launcher
            .reserve_workspace(root.path().join("page"))
            .is_ok()
    );
    supervisor.abort();
    let _ = supervisor.await;
    server.abort();
}
