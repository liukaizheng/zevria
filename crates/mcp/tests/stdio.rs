//! A real child process speaking JSON-RPC over pipes, without a shell or Node.
use std::{
    future::poll_fn,
    io::{BufRead as _, Write as _},
    path::Path,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use rig_agent::tool::server::ToolServer;
use rig_core::tool::ToolContext;
use serde_json::{Value, json};
use zevria_foundation::{
    MCP_LIST_RESOURCES_TOOL_NAME, MCP_READ_RESOURCE_TOOL_NAME, McpManagementRequest,
    McpManagementResult, McpManagementView, McpServerName, McpServerStatus, McpToolAccess,
};
use zevria_mcp::{McpConfigPersister, McpConfigSource, McpManager};
use zevria_session_api::mcp::McpSessionService;

fn serve() {
    eprintln!("fixture stderr");
    if std::env::args().any(|arg| arg == "--fail") {
        eprintln!(
            "invalid fixture settings: {}",
            std::env::var("FIXTURE_SECRET").unwrap_or_default()
        );
        std::process::exit(2);
    }
    if std::env::args().any(|arg| arg == "--count") {
        let mut launches = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("launches")
            .unwrap();
        writeln!(launches, "started").unwrap();
    }
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = msg.get("id") else {
            continue;
        };
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => {
                if std::env::args().any(|arg| arg == "--park") {
                    std::fs::write("handshake-marker", "initialize received").unwrap();
                    std::thread::sleep(Duration::from_secs(60));
                }
                let mut capabilities = json!({"tools":{}});
                if std::env::args().any(|arg| arg == "--resources") {
                    capabilities["resources"] = json!({});
                }
                json!({"protocolVersion":"2025-11-25","serverInfo":{"name":"stdio-fixture","version":"1"},"capabilities":capabilities,"instructions":"Use fixture"})
            }
            "tools/list" => {
                json!({"tools":[{"name":"env","inputSchema":{"type":"object"},"description":"Return sample env"}]})
            }
            "tools/call" => {
                if std::env::args().any(|arg| arg == "--exit-on-call") {
                    return;
                }
                json!({"content":[{"type":"text","text":std::env::var("MCP_FIXTURE_VALUE").unwrap()}]})
            }
            "resources/list" => json!({"resources":[]}),
            "resources/templates/list" => json!({"resourceTemplates":[]}),
            _ => {
                writeln!(output, "{}", json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"not implemented"}})).unwrap();
                output.flush().unwrap();
                continue;
            }
        };
        writeln!(
            output,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"result":result})
        )
        .unwrap();
        output.flush().unwrap();
    }
    std::fs::write("exit-marker", "stdin closed").unwrap();
    if std::env::args().any(|arg| arg == "--ignore-eof") {
        std::thread::sleep(Duration::from_secs(10));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().any(|arg| arg == "--serve") {
        serve();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("fixture")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let source = |required| {
        McpConfigSource { revision: "stdio".into(), config: serde_json::from_value(json!({
        "startup_timeout_seconds": 5,
        "servers": {
            "stdio": {"command":exe,"args":["--serve"],"cwd":"fixture","env":{"MCP_FIXTURE_VALUE":"piped correctly"},"required":true},
            "bad": {"command":exe,"args":["--serve","--fail"],"env":{"FIXTURE_SECRET":"private-test-secret"},"required":required}
        }
    })).unwrap() }
    };
    let tools = ToolServer::new().run();
    let manager = McpManager::start(
        source(false),
        vec![],
        dir.path().into(),
        tools.clone(),
        None,
    )
    .await
    .unwrap();
    let result = manager.query_view(&McpManagementRequest::Inspect {
        server: "bad".parse().unwrap(),
    });
    let McpManagementResult::View { view } = result else {
        panic!("bad server detail")
    };
    let status = view.selected.unwrap().summary.status;
    assert!(matches!(status, McpServerStatus::Failed { .. }));
    assert!(!format!("{status:?}").contains("private-test-secret"));
    let mut context = ToolContext::new();
    let tool = tools.snapshot().get("mcp__stdio__env").unwrap().clone();
    let result = tool.execute("{}".to_owned(), &mut context).await;
    assert!(result.is_success());
    assert_eq!(result.output().render(), "piped correctly");
    manager.shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("fixture/exit-marker")).unwrap(),
        "stdin closed"
    );
    assert!(!tools.snapshot().contains("mcp__stdio__env"));

    // Verify a stdio server that ignores stdin EOF is still reaped promptly
    // (SIGTERM on Unix, kill on Windows) without hanging for the full grace window.
    let stubborn = McpConfigSource {
        revision: "stubborn".into(),
        config: serde_json::from_value(json!({
            "startup_timeout_seconds": 5,
            "servers": {
                "stubborn": {"command":exe,"args":["--serve","--ignore-eof"],"cwd":"fixture","required":true}
            }
        }))
        .unwrap(),
    };
    let manager = McpManager::start(stubborn, vec![], dir.path().into(), tools.clone(), None)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    manager.shutdown(Duration::from_secs(5)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stubborn stdio server shutdown took {:?}",
        started.elapsed()
    );
    assert!(!tools.snapshot().contains("mcp__stubborn__env"));

    assert!(
        McpManager::start(source(true), vec![], dir.path().into(), tools, None)
            .await
            .is_err()
    );
    disconnected_resource_server_denies_dispatch_before_registry_sync().await;
    reload_preserves_unchanged_server().await;
    shutdown_interrupts_pending_handshake().await;
    shutdown_rejects_reload_and_enable_commits().await;
    aborted_disable_keeps_catalog_invalidation_pending().await;
}

async fn disconnected_resource_server_denies_dispatch_before_registry_sync() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolServer::new().run();
    let source = McpConfigSource {
        revision: "disconnect".into(),
        config: serde_json::from_value(json!({
            "servers": {
                "fixture": {
                    "command":std::env::current_exe().unwrap(),
                    "args":["--serve","--resources","--exit-on-call"],
                    "required":true
                }
            }
        }))
        .unwrap(),
    };
    let manager = McpManager::start(source, vec![], dir.path().into(), tools.clone(), None)
        .await
        .unwrap();
    let mut changes = manager.changes();
    for helper in [MCP_LIST_RESOURCES_TOOL_NAME, MCP_READ_RESOURCE_TOOL_NAME] {
        assert!(manager.tool_allowed(McpToolAccess::PlanScoped, helper));
    }
    let wire = "mcp__fixture__env";
    let tool = tools.snapshot().get(wire).unwrap().clone();
    let result = tool.execute("{}".into(), &mut ToolContext::new()).await;
    assert!(!result.is_success());
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.tool_allowed(McpToolAccess::AllEnabled, wire) {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    for helper in [MCP_LIST_RESOURCES_TOOL_NAME, MCP_READ_RESOURCE_TOOL_NAME] {
        assert!(
            tools.snapshot().contains(helper),
            "declaration is still pinned this turn"
        );
        assert!(!manager.tool_allowed(McpToolAccess::PlanScoped, helper));
        assert!(!manager.tool_allowed(McpToolAccess::AllEnabled, helper));
    }
    assert!(manager.sync_pending_updates().await.is_some());
    assert!(!tools.snapshot().contains(MCP_LIST_RESOURCES_TOOL_NAME));
    assert!(!tools.snapshot().contains(wire));
    manager.shutdown(Duration::from_secs(1)).await;
}

struct Store(Mutex<McpConfigSource>);

impl McpConfigPersister for Store {
    fn load(&self) -> anyhow::Result<McpConfigSource> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn set_enabled(
        &self,
        revision: &str,
        server: &McpServerName,
        enabled: bool,
    ) -> anyhow::Result<McpConfigSource> {
        let mut source = self.0.lock().unwrap();
        assert_eq!(source.revision, revision);
        source.config.servers.get_mut(server).unwrap().enabled = enabled;
        source.revision = "persisted".into();
        Ok(source.clone())
    }
}

async fn reload_preserves_unchanged_server() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["same", "changed", "removed"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let source = McpConfigSource {
        revision: "original".into(),
        config: serde_json::from_value(json!({
            "servers": {
                "same": {"command":exe,"args":["--serve","--count"],"cwd":"same"},
                "changed": {"command":exe,"args":["--serve","--count"],"cwd":"changed"},
                "removed": {"command":exe,"args":["--serve","--count"],"cwd":"removed"}
            }
        }))
        .unwrap(),
    };
    let store = Arc::new(Store(Mutex::new(source.clone())));
    let manager = McpManager::start(
        source,
        vec![],
        dir.path().into(),
        ToolServer::new().run(),
        Some(store.clone()),
    )
    .await
    .unwrap();
    {
        let mut source = store.0.lock().unwrap();
        source.config.servers.remove(&"removed".parse().unwrap());
        source
            .config
            .servers
            .get_mut(&"changed".parse().unwrap())
            .unwrap()
            .args
            .push("new-arg".into());
        source.revision = "edited".into();
    }
    assert!(matches!(
        manager.manage(McpManagementRequest::Reload).await,
        McpManagementResult::Changed {
            connected: 2,
            total: 2,
            ..
        }
    ));
    for (name, count) in [("same", 1), ("changed", 2), ("removed", 1)] {
        let launches = std::fs::read_to_string(dir.path().join(name).join("launches")).unwrap();
        assert_eq!(launches.lines().count(), count, "{name}");
    }
    manager.shutdown(Duration::from_secs(2)).await;
}

async fn wait_for_handshake(dir: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.join("handshake-marker").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child received initialize");
}

fn list_view(manager: &McpManager) -> McpManagementView {
    let McpManagementResult::View { view } = manager.query_view(&McpManagementRequest::List) else {
        panic!("manager list");
    };
    view
}

async fn parked_manager(dir: &Path) -> (Arc<McpManager>, Arc<Store>) {
    let source = McpConfigSource {
        revision: "original".into(),
        config: serde_json::from_value(json!({
            "servers": {
                "parked": {"command":std::env::current_exe().unwrap(),"args":["--serve","--park"],"enabled":false}
            }
        }))
        .unwrap(),
    };
    let store = Arc::new(Store(Mutex::new(source.clone())));
    let manager = McpManager::start(
        source,
        vec![],
        dir.into(),
        ToolServer::new().run(),
        Some(store.clone()),
    )
    .await
    .unwrap();
    (manager, store)
}

async fn shutdown_interrupts_pending_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = parked_manager(dir.path()).await;
    let server: McpServerName = "parked".parse().unwrap();
    let view = list_view(&manager);
    let change = tokio::spawn({
        let manager = manager.clone();
        let server = server.clone();
        async move {
            manager
                .manage(McpManagementRequest::SetEnabled {
                    expected_revision: view.revision,
                    server,
                    enabled: true,
                })
                .await
        }
    });
    wait_for_handshake(dir.path()).await;
    change.abort();
    assert!(change.await.unwrap_err().is_cancelled());
    let view = list_view(&manager);
    assert!(view.revision.starts_with("persisted:"));
    assert!(view.servers[0].enabled);

    std::fs::remove_file(dir.path().join("handshake-marker")).unwrap();
    assert_shutdown_interrupts_handshake(
        &manager,
        dir.path(),
        McpManagementRequest::Reconnect { server },
    )
    .await;
    assert_eq!(list_view(&manager).revision, view.revision);
}

async fn assert_shutdown_interrupts_handshake(
    manager: &Arc<McpManager>,
    dir: &Path,
    request: McpManagementRequest,
) {
    let mutation = tokio::spawn({
        let manager = manager.clone();
        async move { manager.manage(request).await }
    });
    wait_for_handshake(dir).await;
    tokio::time::timeout(
        Duration::from_secs(2),
        manager.shutdown(Duration::from_secs(1)),
    )
    .await
    .expect("shutdown must not wait for the 30-second handshake deadline");
    let result = tokio::time::timeout(Duration::from_secs(2), mutation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, McpManagementResult::Error { ref code, .. } if code == "closed"),
        "{result:?}"
    );
}

async fn shutdown_rejects_reload_and_enable_commits() {
    for reload in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (manager, store) = parked_manager(dir.path()).await;
        let server = "parked".parse().unwrap();
        let before = list_view(&manager);
        let request = if reload {
            let mut source = store.0.lock().unwrap();
            source.config.servers.get_mut(&server).unwrap().enabled = true;
            source.revision = "edited".into();
            McpManagementRequest::Reload
        } else {
            McpManagementRequest::SetEnabled {
                expected_revision: before.revision.clone(),
                server,
                enabled: true,
            }
        };
        assert_shutdown_interrupts_handshake(&manager, dir.path(), request).await;
        let after = list_view(&manager);
        if reload {
            assert_eq!(after.revision, before.revision);
            assert!(!after.servers[0].enabled);
        } else {
            assert!(after.revision.starts_with("persisted:"));
            assert!(after.servers[0].enabled);
            assert_eq!(after.servers[0].status, McpServerStatus::Disconnected);
        }
    }
}

async fn aborted_disable_keeps_catalog_invalidation_pending() {
    let dir = tempfile::tempdir().unwrap();
    let source = McpConfigSource {
        revision: "original".into(),
        config: serde_json::from_value(json!({
            "servers": {
                "fixture": {"command":std::env::current_exe().unwrap(),"args":["--serve"],"required":true}
            }
        })).unwrap(),
    };
    let store = Arc::new(Store(Mutex::new(source.clone())));
    let tools = ToolServer::new().run();
    let manager = McpManager::start(
        source,
        vec![],
        dir.path().into(),
        tools.clone(),
        Some(store),
    )
    .await
    .unwrap();
    let tool = "mcp__fixture__env";
    assert!(tools.snapshot().contains(tool));
    let mut changes = manager.changes();
    let mut disable = manager.manage(McpManagementRequest::SetEnabled {
        expected_revision: list_view(&manager).revision,
        server: "fixture".parse().unwrap(),
        enabled: false,
    });
    // On this current-thread runtime, observe retirement in the same poll
    // that removes the tool, before the connection supervisor can finish.
    tokio::time::timeout(
        Duration::from_secs(5),
        poll_fn(|cx| match disable.as_mut().poll(cx) {
            Poll::Ready(result) => panic!("mutation completed before cancellation: {result:?}"),
            Poll::Pending if !tools.snapshot().contains(tool) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }),
    )
    .await
    .unwrap();
    drop(disable);
    assert!(
        changes.has_changed().unwrap(),
        "retirement must wake the engine"
    );
    changes.borrow_and_update();
    assert_eq!(list_view(&manager).revision, "persisted:1");
    assert!(
        manager.sync_pending_updates().await.is_some(),
        "retirement still invalidates the request plan"
    );
    assert!(
        manager.sync_pending_updates().await.is_none(),
        "invalidation is consumed once"
    );
    tokio::time::timeout(Duration::from_secs(5), changes.changed())
        .await
        .unwrap()
        .unwrap();
    manager.shutdown(Duration::from_secs(1)).await;
}
