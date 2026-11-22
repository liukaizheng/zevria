//! Provider-independent MCP diagnostics and configuration commands.
use std::path::Path;

use zevria_foundation::{McpManagementView, McpServerName, McpServerStatus, McpTransportSummary};
use zevria_host::mcp::{LocalMcpConfigStore, McpConfigPersister as _, offline_view, probe};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpCli {
    List {
        json: bool,
    },
    Inspect {
        name: McpServerName,
        json: bool,
    },
    Test {
        name: Option<McpServerName>,
        json: bool,
    },
    SetEnabled {
        name: McpServerName,
        enabled: bool,
    },
}

impl McpCli {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        const USAGE: &str = "usage: zevria mcp list [--json] | inspect <name> [--json] | test [<name>] [--json] | enable <name> | disable <name>";
        let mut json = false;
        let mut positional = Vec::new();
        for arg in args {
            if arg == "--json" && !json {
                json = true;
            } else {
                anyhow::ensure!(!arg.starts_with('-'), "unsupported MCP option; {USAGE}");
                positional.push(arg.as_str());
            }
        }
        let name = |s: &str| s.parse::<McpServerName>().map_err(anyhow::Error::msg);
        Ok(match positional.as_slice() {
            ["list"] => Self::List { json },
            ["inspect", server] => Self::Inspect {
                name: name(server)?,
                json,
            },
            ["test"] => Self::Test { name: None, json },
            ["test", server] => Self::Test {
                name: Some(name(server)?),
                json,
            },
            [action @ ("enable" | "disable"), server] if !json => Self::SetEnabled {
                name: name(server)?,
                enabled: *action == "enable",
            },
            _ => anyhow::bail!("{USAGE}"),
        })
    }

    pub async fn run(self, workspace: &Path) -> anyhow::Result<i32> {
        let store = LocalMcpConfigStore::new(zevria_foundation::config::config_path()?);
        let source = store.load()?;
        match self {
            Self::List { json } => {
                print_view(&offline_view(&source), json)?;
                Ok(0)
            }
            Self::Inspect { name, json } => {
                let view = probe(source, workspace.into(), Some(name), true).await?;
                print_view(&view, json)?;
                Ok(i32::from(view.servers.iter().any(|server| {
                    matches!(
                        server.status,
                        McpServerStatus::Failed { .. } | McpServerStatus::Disconnected
                    )
                })))
            }
            Self::Test { name, json } => {
                let view = probe(source, workspace.into(), name, false).await?;
                print_view(&view, json)?;
                Ok(i32::from(
                    view.servers
                        .iter()
                        .any(|s| !matches!(s.status, McpServerStatus::Connected)),
                ))
            }
            Self::SetEnabled { name, enabled } => {
                let updated = store.set_enabled(&source.revision, &name, enabled)?;
                println!(
                    "MCP server {name}: {}{}",
                    if enabled { "enabled" } else { "disabled" },
                    if source.revision == updated.revision {
                        " (unchanged)"
                    } else {
                        ""
                    }
                );
                Ok(0)
            }
        }
    }
}

fn print_view(view: &McpManagementView, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(view)?);
        return Ok(());
    }
    if !view.enabled {
        println!("MCP is disabled in config.toml.");
    }
    if view.servers.is_empty() {
        println!("No MCP servers match. Add a server under [mcp.servers] in config.toml.");
    }
    for server in &view.servers {
        let transport = match &server.transport {
            McpTransportSummary::Stdio { command, .. } => format!("stdio {command}"),
            McpTransportSummary::Http { redacted_url } => format!("HTTP {redacted_url}"),
        };
        let status = match &server.status {
            McpServerStatus::Connected => "connected",
            McpServerStatus::Connecting => "connecting",
            McpServerStatus::Disabled => "disabled",
            McpServerStatus::Disconnected => "not connected",
            McpServerStatus::Failed { .. } => "failed",
        };
        print!("{}  {status}  {transport}", server.name);
        if let Some(ms) = server.connect_ms {
            print!(
                "  {ms} ms  {} tools, {} resources/templates, {} prompts",
                server.tool_count, server.resource_count, server.prompt_count
            );
        }
        println!();
        if let McpServerStatus::Failed { error, stderr_tail } = &server.status {
            println!("  {error}");
            if let Some(tail) = stderr_tail {
                println!("  stderr: {tail}");
            }
        }
    }
    if let Some(detail) = &view.selected {
        if let Some(info) = &detail.server_info {
            println!("Server: {info}");
        }
        if let Some(instructions) = &detail.instructions {
            println!("Instructions:\n{instructions}");
        }
        for tool in &detail.tools {
            let policy = if !tool.enabled {
                " [disabled]"
            } else if tool.plan_allowed {
                " [Plan allowed]"
            } else {
                " [Build only]"
            };
            println!("Tool: {}{policy} ({})", tool.name, tool.wire_name);
            if let Some(description) = &tool.description {
                println!("  {description}");
            }
        }
        for resource in &detail.resources {
            println!(
                "Resource: {} ({})",
                resource.name,
                resource.uri.escape_default()
            );
        }
        for template in &detail.templates {
            println!(
                "Template: {} ({})",
                template.name,
                template.uri_template.escape_default()
            );
        }
        for prompt in &detail.prompts {
            let args = prompt
                .arguments
                .iter()
                .map(|a| format!("{}{}", a.name, if a.required { "*" } else { "" }))
                .collect::<Vec<_>>()
                .join(", ");
            println!("Prompt: {}({args})", prompt.name);
        }
    }
    Ok(())
}
