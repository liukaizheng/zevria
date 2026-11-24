//! Bounded, uncached server discovery. A refresh is published as one catalog.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use rmcp::{
    RoleClient,
    model::*,
    service::{Peer, ServiceError},
};
use serde::Serialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zevria_foundation::*;

use crate::{
    convert::display_text,
    transport::{RequestFailure, request},
};

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Catalog {
    pub tools: BTreeMap<String, Tool>,
    pub resources: Vec<McpResourceInfo>,
    pub templates: Vec<McpResourceTemplateInfo>,
    pub prompts: Vec<McpPromptInfo>,
    pub resource_capability: bool,
    pub instructions: Option<McpServerInstruction>,
    pub server_info: Option<String>,
}

impl Catalog {
    pub(crate) async fn fetch(
        name: &McpServerName,
        peer: &Peer<RoleClient>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Self> {
        let info = peer
            .peer_info()
            .ok_or_else(|| anyhow::anyhow!("MCP server did not initialize"))?;
        let deadline = Instant::now() + timeout;
        let (tools, resources, templates, prompts) = tokio::try_join!(
            list(
                peer,
                deadline,
                cancel,
                info.capabilities.tools.is_some(),
                "tools/list",
                |p| ListToolsRequest::with_param(p).into(),
                |r| match r {
                    ServerResult::ListToolsResult(r) => Some((r.tools, r.next_cursor)),
                    _ => None,
                }
            ),
            list(
                peer,
                deadline,
                cancel,
                info.capabilities.resources.is_some(),
                "resources/list",
                |p| ListResourcesRequest::with_param(p).into(),
                |r| match r {
                    ServerResult::ListResourcesResult(r) => Some((r.resources, r.next_cursor)),
                    _ => None,
                }
            ),
            list(
                peer,
                deadline,
                cancel,
                info.capabilities.resources.is_some(),
                "resources/templates/list",
                |p| ListResourceTemplatesRequest::with_param(p).into(),
                |r| match r {
                    ServerResult::ListResourceTemplatesResult(r) =>
                        Some((r.resource_templates, r.next_cursor)),
                    _ => None,
                }
            ),
            list(
                peer,
                deadline,
                cancel,
                info.capabilities.prompts.is_some(),
                "prompts/list",
                |p| ListPromptsRequest::with_param(p).into(),
                |r| match r {
                    ServerResult::ListPromptsResult(r) => Some((r.prompts, r.next_cursor)),
                    _ => None,
                }
            ),
        )?;
        let mut sorted_tools = BTreeMap::new();
        for tool in tools {
            anyhow::ensure!(valid_item_name(&tool.name), "invalid MCP tool name");
            let key = tool.name.to_string();
            anyhow::ensure!(
                sorted_tools.insert(key, tool).is_none(),
                "duplicate MCP tool name"
            );
        }
        let mut prompts: Vec<_> = prompts
            .into_iter()
            .map(|prompt| McpPromptInfo {
                name: prompt.name,
                description: preview(prompt.description),
                arguments: prompt
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|arg| McpPromptArgumentInfo {
                        name: arg.name,
                        description: preview(arg.description),
                        required: arg.required.unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect();
        prompts.sort_by(|a, b| a.name.cmp(&b.name));
        anyhow::ensure!(
            prompts.windows(2).all(|pair| pair[0].name != pair[1].name)
                && prompts.iter().all(|p| valid_item_name(&p.name)
                    && p.arguments.len() <= 64
                    && p.arguments.iter().all(|a| valid_item_name(&a.name))),
            "invalid or duplicate MCP prompt entries"
        );
        Ok(Self {
            tools: sorted_tools,
            resources: resources
                .into_iter()
                .map(|r| McpResourceInfo {
                    uri: r.uri,
                    name: display_text(&r.name, 256),
                    description: preview(r.description),
                    mime_type: preview(r.mime_type),
                })
                .collect(),
            templates: templates
                .into_iter()
                .map(|r| McpResourceTemplateInfo {
                    uri_template: r.uri_template,
                    name: display_text(&r.name, 256),
                    description: preview(r.description),
                    mime_type: preview(r.mime_type),
                })
                .collect(),
            prompts,
            resource_capability: info.capabilities.resources.is_some(),
            instructions: info
                .instructions
                .as_deref()
                .and_then(|body| McpServerInstruction::from_body(name.clone(), body)),
            server_info: info
                .server_info
                .as_ref()
                .map(|server| display_text(&format!("{} {}", server.name, server.version), 512)),
        })
    }
}

fn preview(value: Option<String>) -> Option<String> {
    value.map(|s| display_text(&s, 4096))
}

fn valid_item_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control)
}

async fn list<T: Serialize>(
    peer: &Peer<RoleClient>,
    deadline: Instant,
    cancel: &CancellationToken,
    enabled: bool,
    method: &str,
    make_request: impl Fn(PaginatedRequestParams) -> ClientRequest,
    unpack: impl Fn(ServerResult) -> Option<(Vec<T>, Option<String>)>,
) -> anyhow::Result<Vec<T>> {
    if !enabled {
        return Ok(vec![]);
    }
    let mut all = vec![];
    let mut cursor = None;
    let mut visited = BTreeSet::new();
    let mut bytes = 0usize;
    for _ in 0..32 {
        let response = request(
            peer,
            make_request(PaginatedRequestParams::default().with_cursor(cursor)),
            deadline,
            cancel,
        )
        .await;
        let response = match response {
            Err(RequestFailure::Service(ServiceError::McpError(error)))
                if method == "resources/templates/list"
                    && error.code == ErrorCode::METHOD_NOT_FOUND =>
            {
                return Ok(vec![]);
            }
            other => other.map_err(|error| anyhow::anyhow!("{method}: {error}"))?,
        };
        let (items, next) =
            unpack(response).ok_or_else(|| anyhow::anyhow!("unexpected {method} response"))?;
        anyhow::ensure!(
            all.len() + items.len() <= 4096,
            "{method} catalog exceeds 4096 entries"
        );
        const LIMIT: usize = 4 * 1024 * 1024;
        let mut count = zevria_foundation::BoundedCountingWriter::new(LIMIT - bytes);
        let encoded = serde_json::to_writer(&mut count, &items);
        anyhow::ensure!(
            count.written() <= LIMIT - bytes,
            "{method} catalog exceeds 4 MiB"
        );
        encoded?;
        bytes += count.written();
        all.extend(items);
        match next {
            None => return Ok(all),
            Some(next) if next.len() <= 8192 && visited.insert(next.clone()) => cursor = Some(next),
            Some(_) => anyhow::bail!("{method} returned an invalid or repeated cursor"),
        }
    }
    anyhow::bail!("{method} catalog exceeds 32 pages")
}

pub(crate) fn read_only(tool: &Tool) -> bool {
    tool.annotations
        .as_ref()
        .is_some_and(|a| a.read_only_hint == Some(true) && a.destructive_hint != Some(true))
}

pub(crate) fn wire_names(
    server: &McpServerName,
    catalog: &Catalog,
    previous: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut result: BTreeMap<_, _> = previous
        .iter()
        .filter(|(name, _)| catalog.tools.contains_key(*name))
        .map(|(name, wire)| (name.clone(), wire.clone()))
        .collect();
    let mut used: BTreeSet<_> = result.values().cloned().collect();
    for name in catalog.tools.keys() {
        if result.contains_key(name) {
            continue;
        }
        for n in 0.. {
            let wire = mcp_wire_tool_name_with_disambiguator(server, name, n);
            if used.insert(wire.clone()) {
                result.insert(name.clone(), wire);
                break;
            }
        }
    }
    result
}
