//! A real child process speaking JSON-RPC over pipes, without a shell or Node.
use std::{
    io::{BufRead as _, Write as _},
    time::Duration,
};

use rig_agent::tool::server::ToolServer;
use rig_core::tool::ToolContext;
use serde_json::{Value, json};
use zevria_foundation::{McpManagementRequest, McpManagementResult, McpServerStatus};
use zevria_mcp::{McpConfigSource, McpManager};
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
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = msg.get("id") else {
            continue;
        };
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => {
                json!({"protocolVersion":"2025-11-25","serverInfo":{"name":"stdio-fixture","version":"1"},"capabilities":{"tools":{}},"instructions":"Use fixture"})
            }
            "tools/list" => {
                json!({"tools":[{"name":"env","inputSchema":{"type":"object"},"description":"Return sample env"}]})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":std::env::var("MCP_FIXTURE_VALUE").unwrap()}]})
            }
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
    assert!(
        McpManager::start(source(true), vec![], dir.path().into(), tools, None)
            .await
            .is_err()
    );
}
