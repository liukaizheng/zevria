//! Model-callable tool adapters. Resource URIs are sent back to their owning
//! server; Zevria never dereferences a URI itself.
use std::{sync::Weak, time::Instant};

use rig_core::{
    completion::ToolAnnotations,
    message::ToolName,
    tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput},
};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ReadResourceRequest, ReadResourceRequestParams,
    ServerResult, Tool,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use zevria_foundation::*;
use zevria_session_api::TurnContext;

use crate::{
    convert::{
        ConversionStats, call_tool_result_to_output, display_text, elide, normalize_input_schema,
        read_resource_result_to_output,
    },
    manager::{McpManager, Target},
    transport::{RequestFailure, request},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Restrict results to one configured server (null means all accessible servers).
    server: Option<String>,
    /// Whether to include URI templates (defaults to true).
    include_templates: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    server: String,
    uri: String,
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("JSON schema")
}

fn read_annotations() -> ToolAnnotations {
    ToolAnnotations {
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        open_world_hint: Some(true),
        ..ToolAnnotations::default()
    }
}

fn access(context: &ToolContext) -> McpToolAccess {
    context
        .scope::<McpToolAccess>()
        .map_or(McpToolAccess::None, |a| *a)
}
fn cancellation(context: &ToolContext) -> CancellationToken {
    context
        .scope::<TurnContext>()
        .map_or_else(CancellationToken::new, |turn| turn.cancellation().clone())
}

pub(crate) fn helpers(manager: Weak<McpManager>) -> [DynamicTool; 2] {
    let list_manager = manager.clone();
    let list = DynamicTool::new_with_context(
        ToolName::new(MCP_LIST_RESOURCES_TOOL_NAME).unwrap(),
        "List readable MCP resources and URI templates. Use the server name and URI with mcp_read_resource.",
        schema::<ListArgs>(),
        move |context: &mut ToolContext, args| {
            let manager = list_manager.upgrade();
            let access = access(context);
            Box::pin(async move { list_resources(manager, access, args) })
        },
    ).with_annotations(read_annotations());
    let read = DynamicTool::new_with_context(
        ToolName::new(MCP_READ_RESOURCE_TOOL_NAME).unwrap(),
        "Read a resource from its MCP server, using a concrete URI from a resource or URI template.",
        schema::<ReadArgs>(),
        move |context: &mut ToolContext, args| {
            let manager = manager.upgrade();
            Box::pin(async move { read_resource(manager, context, args).await })
        },
    ).with_annotations(read_annotations());
    [list, read]
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ToolExecutionError> {
    serde_json::from_value(value)
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))
}

fn server_name(name: &str) -> Result<McpServerName, ToolExecutionError> {
    name.parse().map_err(ToolExecutionError::invalid_args)
}

fn list_resources(
    manager: Option<std::sync::Arc<McpManager>>,
    access: McpToolAccess,
    value: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let args: ListArgs = args(value)?;
    let server = args.server.as_deref().map(server_name).transpose()?;
    let manager = manager.ok_or_else(|| ToolExecutionError::other("MCP session is closed"))?;
    let resources = manager.resource_view(access, server.as_ref());
    if resources.is_empty() && server.is_some() {
        return Err(ToolExecutionError::other(
            "the requested MCP server is not connected or does not allow resource access in this mode",
        ));
    }
    let with_templates = args.include_templates.unwrap_or(true);
    let entries: Vec<_> = resources
        .into_iter()
        .map(|(server, resources, templates)| {
            if with_templates {
                json!({"server": server, "resources": resources, "templates": templates})
            } else {
                json!({"server": server, "resources": resources})
            }
        })
        .collect();
    let text = serde_json::to_string_pretty(&entries).expect("resource list serializes");
    Ok(ToolOutput::text(elide(&text, manager.output_limit())))
}

async fn read_resource(
    manager: Option<std::sync::Arc<McpManager>>,
    context: &mut ToolContext,
    value: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let args: ReadArgs = args(value)?;
    let server = server_name(&args.server)?;
    if args.uri.trim().is_empty() || args.uri.len() > 8192 {
        return Err(ToolExecutionError::invalid_args(
            "MCP resource URI must be 1–8192 bytes",
        ));
    }
    let target = manager.and_then(|manager| manager.resource_target(access(context), &server))
        .ok_or_else(|| ToolExecutionError::other("the requested MCP server is not connected or does not allow resource access in this mode"))?;
    let start = Instant::now();
    let cancellation = cancellation(context);
    let result = request(
        &target.peer,
        ReadResourceRequest::new(ReadResourceRequestParams::new(args.uri)).into(),
        tokio::time::Instant::now() + target.timeout,
        &cancellation,
    )
    .await;
    match result {
        Ok(ServerResult::ReadResourceResult(result)) => {
            let limit = target.limit;
            let inline = result.contents.iter().all(|block| {
                matches!(
                    block,
                    rmcp::model::ResourceContents::TextResourceContents { .. }
                )
            });
            finish(
                context,
                &target,
                MCP_READ_RESOURCE_TOOL_NAME,
                start,
                &cancellation,
                inline,
                move || read_resource_result_to_output(result, limit),
            )
            .await
        }
        Ok(_) => Err(ToolExecutionError::other(
            "unexpected MCP resources/read response",
        )),
        Err(error) => fail(context, &target, MCP_READ_RESOURCE_TOOL_NAME, start, error),
    }
}

pub(crate) fn external_tool(target: Target, tool: &Tool, wire: &str) -> DynamicTool {
    let name = tool.name.to_string();
    let description = tool
        .description
        .as_deref()
        .map(|s| display_text(s, 4096))
        .unwrap_or_default();
    let schema = normalize_input_schema(&tool.input_schema);
    let annotations = tool
        .annotations
        .as_ref()
        .map(|a| ToolAnnotations {
            title: a.title.as_deref().map(|title| display_text(title, 256)),
            read_only_hint: a.read_only_hint,
            destructive_hint: a.destructive_hint,
            idempotent_hint: a.idempotent_hint,
            open_world_hint: a.open_world_hint,
        })
        .unwrap_or_default();
    let check_target = target.clone();
    DynamicTool::new_with_context(
        ToolName::new(wire).expect("valid MCP name"),
        description,
        schema,
        move |context: &mut ToolContext, arguments| {
            let target = target.clone();
            let name = name.clone();
            Box::pin(async move { call_tool(target, name, context, arguments).await })
        },
    )
    .with_annotations(annotations)
    .with_liveness(move || {
        !check_target.connection_stop.is_cancelled() && !check_target.peer.is_transport_closed()
    })
}

async fn call_tool(
    target: Target,
    name: String,
    context: &mut ToolContext,
    arguments: Value,
) -> Result<ToolOutput, ToolExecutionError> {
    let mut params = CallToolRequestParams::new(name.clone());
    match arguments {
        Value::Null => {}
        Value::Object(arguments) => {
            params = params.with_arguments(arguments);
        }
        _ => {
            return Err(ToolExecutionError::invalid_args(
                "MCP tools require an object argument",
            ));
        }
    }
    let start = Instant::now();
    let cancellation = cancellation(context);
    let result = request(
        &target.peer,
        CallToolRequest::new(params).into(),
        tokio::time::Instant::now() + target.timeout,
        &cancellation,
    )
    .await;
    match result {
        Ok(ServerResult::CallToolResult(result)) => {
            let server = target.server.clone();
            let limit = target.limit;
            let inline = result.structured_content.is_none()
                && result
                    .content
                    .iter()
                    .all(|block| matches!(block, rmcp::model::ContentBlock::Text(_)));
            finish(
                context,
                &target,
                &name,
                start,
                &cancellation,
                inline,
                move || call_tool_result_to_output(result, &server, limit),
            )
            .await
        }
        Ok(_) => Err(ToolExecutionError::other(
            "unexpected MCP tools/call response",
        )),
        Err(error) => fail(context, &target, &name, start, error),
    }
}

async fn finish(
    context: &mut ToolContext,
    target: &Target,
    name: &str,
    start: Instant,
    cancellation: &CancellationToken,
    inline: bool,
    convert: impl FnOnce() -> (Result<ToolOutput, ToolExecutionError>, ConversionStats) + Send + 'static,
) -> Result<ToolOutput, ToolExecutionError> {
    if cancellation.is_cancelled() {
        return fail(context, target, name, start, RequestFailure::Cancelled);
    }
    if inline {
        let (output, stats) = convert();
        record(context, target, name, start, stats);
        return output;
    }
    let conversion = tokio::task::spawn_blocking(convert);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => fail(context, target, name, start, RequestFailure::Cancelled),
        result = conversion => {
            let (output, stats) = result.map_err(|_| ToolExecutionError::other("MCP response preparation failed"))?;
            record(context, target, name, start, stats);
            output
        }
    }
}

fn fail(
    context: &mut ToolContext,
    target: &Target,
    name: &str,
    start: Instant,
    error: RequestFailure,
) -> Result<ToolOutput, ToolExecutionError> {
    if matches!(error, RequestFailure::Cancelled) {
        let _ = context.insert_result(ToolCancelled);
    }
    record(
        context,
        target,
        name,
        start,
        ConversionStats {
            blocks: 0,
            truncated: false,
        },
    );
    Err(ToolExecutionError::other(error.to_string()))
}

fn record(
    context: &mut ToolContext,
    target: &Target,
    name: &str,
    start: Instant,
    stats: ConversionStats,
) {
    let _ = context.insert_result(ToolResultDetail::Mcp(McpCallDetail {
        server: target.server.clone(),
        tool: name.into(),
        duration_ms: start.elapsed().as_millis() as u64,
        content_blocks: stats.blocks,
        truncated: stats.truncated,
    }));
}
