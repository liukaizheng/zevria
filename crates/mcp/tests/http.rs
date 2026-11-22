use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::Request,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use http::{Method, StatusCode};
use rig_agent::tool::server::ToolServer;
use rig_core::{
    message::ToolResultContent,
    tool::{ToolContext, ToolResult},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::{Peer, RequestContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zevria_foundation::*;
use zevria_mcp::{McpConfigSource, McpManager};
use zevria_session_api::{TurnContext, mcp::McpSessionService};

#[derive(Clone, Default)]
struct Fixture {
    peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
    extra: Arc<AtomicBool>,
    started: Arc<Notify>,
    stopped: Arc<Notify>,
    deletes: Arc<AtomicUsize>,
}

fn image() -> String {
    zevria_content::PromptImage::from_rgba(1, 1, &[0, 0, 0, 255])
        .unwrap()
        .base64()
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .with_instructions("  Use the sample catalog.  ")
        .with_server_info(Implementation::new("mcp-fixture", "1"))
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        *self.peer.lock().unwrap() = Some(context.peer.clone());
        let tool = |name| {
            Tool::new(
                name,
                "An example tool",
                Arc::new(
                    json!({"type":"object","properties":{"option":{"type":"string"}}})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
        };
        let mut tools = vec![
            tool("read").with_annotations(ToolAnnotations::new().read_only(true)),
            tool("fail"),
            tool("slow"),
        ];
        if self.extra.load(Ordering::Relaxed) {
            tools.push(tool("added"));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = match params.name.as_ref() {
            "slow" => {
                self.started.notify_one();
                context.ct.cancelled().await;
                self.stopped.notify_one();
                CallToolResult::success(vec![ContentBlock::text("cancelled")])
            }
            "fail" => CallToolResult::error(vec![ContentBlock::text("fixture tool error")]),
            _ => {
                let mut result = CallToolResult::success(vec![
                    ContentBlock::text(format!("start {} end", "abc".repeat(1000))),
                    ContentBlock::image(image(), "image/png"),
                ]);
                result.structured_content = Some(json!({ "fixture": true }));
                result
            }
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![Resource::new(
            "fixture://index",
            "Index",
        )]))
    }

    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("fixture://{id}", "By id"),
        ]))
    }

    async fn read_resource(
        &self,
        p: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text("Fixture content", p.uri),
            ResourceContents::blob(image(), "fixture://image").with_mime_type("image/png"),
        ])
        .into())
    }

    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "summarize",
            Some("Summarize an entry"),
            None,
        )]))
    }

    async fn get_prompt(
        &self,
        _: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        Ok(GetPromptResult::new(vec![
            PromptMessage::new_text(Role::User, "Summarize this."),
            PromptMessage::new(Role::Assistant, ContentBlock::image(image(), "image/png")),
        ])
        .into())
    }
}

async fn server(
    fixture: Fixture,
    require_auth: bool,
    legacy: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let service = StreamableHttpService::<_, LocalSessionManager>::new(
        {
            let fixture = fixture.clone();
            move || Ok(fixture.clone())
        },
        Default::default(),
        StreamableHttpServerConfig::default().with_legacy_session_mode(legacy),
    );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let fixture = fixture.clone();
            async move {
                if require_auth
                    && req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        != Some("Bearer xx")
                {
                    return (StatusCode::UNAUTHORIZED, "should not appear in diagnostics")
                        .into_response();
                }
                if req.method() == Method::DELETE {
                    fixture.deletes.fetch_add(1, Ordering::Relaxed);
                }
                let result: Response = next.run(req).await;
                result
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}

async fn execute(
    tools: &rig_agent::tool::server::ToolServerHandle,
    name: &str,
    args: &str,
    cancel: CancellationToken,
) -> (ToolResult, ToolContext) {
    let mut context = ToolContext::new();
    context.insert_scope(TurnContext::new(TurnId::new(1), SessionMode::Build, cancel));
    context.insert_scope(McpToolAccess::AllEnabled);
    let tool = tools.snapshot().get(name).unwrap().clone();
    let result = tool.execute(args.to_owned(), &mut context).await;
    (result, context)
}

#[tokio::test]
async fn http_discovery_calls_policy_notifications_and_cleanup() {
    let fixture = Fixture::default();
    let (url, server) = server(fixture.clone(), false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolServer::new().run();
    let source = McpConfigSource {
        revision: "fixture".into(),
        config: serde_json::from_value(json!({
            "max_output_bytes":1024, "startup_timeout_seconds":10, "tool_timeout_seconds":1,
            "servers":{"fixture":{"url":url,"required":true}}
        }))
        .unwrap(),
    };
    let manager = McpManager::start(source, vec![], dir.path().into(), tools.clone(), None)
        .await
        .unwrap();
    let name: McpServerName = "fixture".parse().unwrap();
    assert!(manager.tool_allowed(McpToolAccess::PlanScoped, "mcp__fixture__read"));
    assert!(!manager.tool_allowed(McpToolAccess::PlanScoped, "mcp__fixture__fail"));
    assert!(manager.tool_allowed(McpToolAccess::PlanScoped, MCP_READ_RESOURCE_TOOL_NAME));
    assert_eq!(
        manager.active_instructions(McpToolAccess::AllEnabled)[0].body,
        "Use the sample catalog."
    );
    let McpManagementResult::View { view } = manager.query_view(&McpManagementRequest::Inspect {
        server: name.clone(),
    }) else {
        panic!("inspect")
    };
    assert_eq!(
        view.selected.unwrap().templates[0].uri_template,
        "fixture://{id}"
    );
    let cancel = CancellationToken::new();
    let (result, context) = execute(&tools, "mcp__fixture__read", "{}", cancel.clone()).await;
    assert!(result.is_success());
    assert!(result.output().render().contains("MCP output truncated"));
    assert!(
        result
            .output()
            .as_content()
            .iter()
            .any(|block| matches!(block, ToolResultContent::Image(_)))
    );
    assert!(
        matches!(context.result::<ToolResultDetail>().unwrap(), Some(ToolResultDetail::Mcp(detail)) if detail.truncated)
    );
    let (result, _) = execute(&tools, "mcp__fixture__fail", "{}", cancel.clone()).await;
    assert!(!result.is_success());
    assert!(result.output().render().contains("fixture tool error"));
    let (result, _) = execute(&tools, MCP_LIST_RESOURCES_TOOL_NAME, "{}", cancel.clone()).await;
    assert!(result.output().render().contains("fixture://index"));
    let (result, _) = execute(
        &tools,
        MCP_READ_RESOURCE_TOOL_NAME,
        r#"{"server":"fixture","uri":"fixture://index"}"#,
        cancel.clone(),
    )
    .await;
    assert!(result.output().render().contains("Fixture content"));
    assert!(
        result
            .output()
            .as_content()
            .iter()
            .any(|block| matches!(block, ToolResultContent::Image(_)))
    );
    let prompt = manager
        .get_prompt(
            McpToolAccess::PlanScoped,
            name,
            "summarize".into(),
            Default::default(),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert!(
        prompt
            .text_projection()
            .starts_with("[MCP prompt: fixture:summarize]")
    );
    assert_eq!(prompt.images().count(), 1);

    let cancellation = async {
        fixture.started.notified().await;
        cancel.cancel();
    };
    let ((result, context), ()) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(
            execute(&tools, "mcp__fixture__slow", "{}", cancel.clone()),
            cancellation
        )
    })
    .await
    .expect("slow request reached the fixture and cancelled");
    assert!(!result.is_success());
    assert!(context.result::<ToolCancelled>().unwrap().is_some());
    tokio::time::timeout(Duration::from_secs(4), fixture.stopped.notified())
        .await
        .unwrap();
    let (result, _) = execute(&tools, "mcp__fixture__slow", "{}", CancellationToken::new()).await;
    assert!(result.output().render().contains("timed out"));
    tokio::time::timeout(Duration::from_secs(4), fixture.stopped.notified())
        .await
        .unwrap();

    let mut changes = manager.changes();
    fixture.extra.store(true, Ordering::Relaxed);
    let peer = fixture.peer.lock().unwrap().clone().unwrap();
    peer.notify_tool_list_changed().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(!tools.snapshot().contains("mcp__fixture__added"));
    assert!(manager.sync_pending_updates().await);
    assert!(tools.snapshot().contains("mcp__fixture__added"));
    manager.shutdown(Duration::from_secs(5)).await;
    assert!(!tools.snapshot().contains("mcp__fixture__read"));
    assert_eq!(fixture.deletes.load(Ordering::Relaxed), 1);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn http_refreshes_command_credentials_after_401_and_hides_url_query() {
    let fixture = Fixture::default();
    let (url, server) = server(fixture, true, false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut source = McpConfigSource { revision: "fixture".into(), config: serde_json::from_value(json!({
        "startup_timeout_seconds": 10,
        "servers":{"fixture":{"url":format!("{url}?secret=url-private"),"required":true,"bearer_token":{
            "command":"/bin/sh", "args":["-c","printf x >> count; cat count"]
        }}}
    })).unwrap() };
    source.config.bind_config_directory(dir.path());
    assert!(!format!("{source:?}").contains("url-private"));
    let manager = McpManager::start(
        source,
        vec![],
        dir.path().into(),
        ToolServer::new().run(),
        None,
    )
    .await
    .unwrap();
    let view = serde_json::to_string(&manager.query_view(&McpManagementRequest::List)).unwrap();
    assert!(!view.contains("url-private") && !view.contains("printf"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("count")).unwrap(),
        "xx"
    );
    manager.shutdown(Duration::from_secs(5)).await;
    server.abort();
}
