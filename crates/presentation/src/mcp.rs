//! Shared state and input for the MCP inspector. Renderers own navigation keys,
//! focus and layout; this model owns request correlation and prompt drafts.
use std::collections::BTreeMap;
use zevria_session_api::ManagementEvent;

use zevria_foundation::*;
use zevria_session_api::{ManagementCommand, SessionCommand, SessionEvent, TurnCommand};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpTab {
    #[default]
    Tools,
    Resources,
    Prompts,
}
impl McpTab {
    pub const ALL: [Self; 3] = [Self::Tools, Self::Resources, Self::Prompts];
    pub fn title(self) -> &'static str {
        match self {
            Self::Tools => "Tools",
            Self::Resources => "Resources",
            Self::Prompts => "Prompts",
        }
    }
}

#[derive(Debug)]
pub struct McpPromptDraft {
    pub server: McpServerName,
    pub prompt: McpPromptInfo,
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct McpInspector {
    pub open: bool,
    pub view: Option<McpManagementView>,
    pub server: Option<McpServerName>,
    pub tab: McpTab,
    pub draft: Option<McpPromptDraft>,
    pub notice: Option<String>,
    pub commands: crate::outbox::CommandOutbox,
    pub prompts: Vec<(McpServerName, McpPromptInfo)>,
    query: Option<String>,
    mutation: Option<String>,
}

impl McpInspector {
    pub fn show(&mut self) {
        self.open = true;
        self.server = None;
        self.draft = None;
        self.notice = None;
        self.refresh();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.draft = None;
    }
    pub fn loading(&self) -> bool {
        self.query.is_some() || self.mutation.is_some()
    }
    pub fn can_change(&self, busy: bool) -> bool {
        !busy && self.mutation.is_none() && self.view.is_some()
    }

    pub fn detail(&self) -> Option<&McpServerDetail> {
        let selected = self.view.as_ref()?.selected.as_ref()?;
        (Some(&selected.summary.name) == self.server.as_ref()).then_some(selected)
    }

    pub fn inspect(&mut self, server: McpServerName) {
        self.server = Some(server);
        self.tab = McpTab::Tools;
        self.draft = None;
        self.notice = None;
        self.refresh();
    }

    pub fn back(&mut self) {
        if self.draft.take().is_some() {
            self.notice = None;
        } else if self.server.take().is_some() {
            self.notice = None;
            self.refresh();
        } else {
            self.close();
        }
    }

    pub fn refresh(&mut self) {
        let request = self
            .server
            .as_ref()
            .map_or(McpManagementRequest::List, |server| {
                McpManagementRequest::Inspect {
                    server: server.clone(),
                }
            });
        self.query = Some(self.send(request));
    }

    pub fn reload(&mut self, busy: bool) {
        self.mutate(busy, McpManagementRequest::Reload);
    }
    pub fn reconnect(&mut self, busy: bool, server: McpServerName) {
        self.mutate(busy, McpManagementRequest::Reconnect { server });
    }
    pub fn toggle(&mut self, busy: bool, server: McpServerName) {
        let Some(view) = &self.view else {
            return;
        };
        let Some(summary) = view.servers.iter().find(|s| s.name == server) else {
            return;
        };
        if summary.origin != McpServerOrigin::Config {
            self.notice = Some("Edit this delegated server in the ACP client.".into());
            return;
        }
        self.mutate(
            busy,
            McpManagementRequest::SetEnabled {
                expected_revision: view.revision.clone(),
                server,
                enabled: !summary.enabled,
            },
        );
    }

    fn mutate(&mut self, busy: bool, request: McpManagementRequest) {
        if !self.can_change(busy) {
            if busy {
                self.notice = Some("Wait for the turn to finish before changing servers.".into());
            }
            return;
        }
        self.notice = None;
        self.query = None;
        self.mutation = Some(self.send(request));
    }

    fn send(&mut self, request: McpManagementRequest) -> String {
        let request_id = crate::outbox::RequestIdAllocator::allocate("mcp-");
        self.commands
            .push_back(SessionCommand::Manage(ManagementCommand::Mcp {
                request_id: request_id.clone(),
                request,
            }));
        request_id
    }

    pub fn event(&mut self, event: &SessionEvent) -> bool {
        match event {
            SessionEvent::Manage(ManagementEvent::McpChanged { .. }) => {
                if self.mutation.is_none() {
                    self.refresh();
                }
                true
            }
            SessionEvent::Manage(ManagementEvent::McpResult { request_id, result })
                if self.mutation.as_ref() == Some(request_id) =>
            {
                self.mutation = None;
                self.notice = match &**result {
                    McpManagementResult::Changed {
                        unchanged: true, ..
                    } => Some("No configuration change.".into()),
                    McpManagementResult::Changed {
                        connected, total, ..
                    } => Some(format!("{connected} of {total} servers connected.")),
                    McpManagementResult::Error { message, .. } => Some(message.clone()),
                    _ => None,
                };
                self.refresh();
                true
            }
            SessionEvent::Manage(ManagementEvent::McpResult { request_id, result })
                if self.query.as_ref() == Some(request_id) =>
            {
                self.query = None;
                match &**result {
                    McpManagementResult::View { view } => {
                        self.view = Some(view.clone());
                        self.prompts.clone_from(&view.prompts);
                        if self
                            .server
                            .as_ref()
                            .is_some_and(|name| !view.servers.iter().any(|s| &s.name == name))
                        {
                            self.server = None;
                            self.draft = None;
                        }
                    }
                    McpManagementResult::Error { code, message } => {
                        self.notice = Some(message.clone());
                        if code == "not_found" && self.server.take().is_some() {
                            self.draft = None;
                            self.refresh();
                        }
                        if code == "inactive" {
                            self.view = None;
                            self.prompts.clear();
                        }
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    pub fn draft_prompt(&mut self, index: usize) -> bool {
        let Some(detail) = self.detail() else {
            return false;
        };
        let Some(prompt) = detail.prompts.get(index) else {
            return false;
        };
        self.draft = Some(McpPromptDraft {
            server: detail.summary.name.clone(),
            prompt: prompt.clone(),
            values: BTreeMap::new(),
        });
        self.notice = None;
        true
    }

    pub fn run_prompt(&mut self, mode: SessionMode, busy: bool) -> Option<TurnCommand> {
        if busy {
            self.notice = Some("Wait for the turn to finish before running a prompt.".into());
            return None;
        }
        let draft = self.draft.as_ref()?;
        if let Some(arg) = draft.prompt.arguments.iter().find(|arg| {
            arg.required
                && draft
                    .values
                    .get(&arg.name)
                    .is_none_or(|value| value.trim().is_empty())
        }) {
            self.notice = Some(format!("Enter a value for {}.", arg.name));
            return None;
        }
        let draft = self.draft.take().unwrap();
        self.close();
        Some(TurnCommand::InvokeMcpPrompt {
            server: draft.server,
            prompt: draft.prompt.name,
            arguments: draft.values,
            mode,
        })
    }
}

pub fn status_label(status: &McpServerStatus) -> &'static str {
    match status {
        McpServerStatus::Connected => "Connected",
        McpServerStatus::Connecting => "Connecting",
        McpServerStatus::Disabled => "Disabled",
        McpServerStatus::Disconnected => "Disconnected",
        McpServerStatus::Failed { .. } => "Failed",
    }
}

pub fn transport_label(summary: &McpTransportSummary) -> String {
    match summary {
        McpTransportSummary::Stdio { command, .. } => format!("stdio: {command}"),
        McpTransportSummary::Http { redacted_url } => format!("HTTP: {redacted_url}"),
    }
}

pub fn tool_policy(tool: &McpToolInfo) -> &'static str {
    if !tool.enabled {
        "Disabled"
    } else if tool.plan_allowed && tool.read_only {
        "Plan / read-only hint"
    } else if tool.plan_allowed {
        "Plan allowed"
    } else if tool.read_only {
        "Build only / read-only hint"
    } else {
        "Build only"
    }
}

pub(crate) fn parse_prompt(
    input: &str,
) -> Result<(McpServerName, String, BTreeMap<String, String>), String> {
    let input = input.trim();
    let (target, mut rest) = input.split_at(input.find(char::is_whitespace).unwrap_or(input.len()));
    let (server, prompt) = target
        .split_once(':')
        .ok_or("Use /prompt <server>:<name> [key=value ...].")?;
    let server = server.parse::<McpServerName>().map_err(str::to_owned)?;
    if prompt.is_empty() || prompt.chars().any(char::is_control) {
        return Err("Enter a valid MCP prompt name.".into());
    }
    if input.len() > 256 * 1024 {
        return Err("MCP prompt arguments exceed 256 KiB.".into());
    }
    let mut arguments = BTreeMap::new();
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let eq = rest.find('=').ok_or("Prompt arguments use key=value.")?;
        let key = &rest[..eq];
        if key.is_empty()
            || key
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\'))
        {
            return Err("Prompt arguments use key=value.".into());
        }
        let value;
        let after = &rest[eq + 1..];
        if let Some(quoted) = after.strip_prefix('"') {
            let mut parsed = String::new();
            let mut chars = quoted.char_indices();
            rest = loop {
                match chars.next() {
                    Some((i, '"')) => break &quoted[i + 1..],
                    Some((_, '\\')) => match chars.next() {
                        Some((_, ch @ ('"' | '\\'))) => parsed.push(ch),
                        _ => {
                            return Err(
                                "Quoted values escape a quote or backslash with '\\'.".into()
                            );
                        }
                    },
                    Some((_, ch)) => parsed.push(ch),
                    None => return Err("Close the quoted prompt argument.".into()),
                }
            };
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                return Err("Separate prompt arguments with spaces.".into());
            }
            value = parsed;
        } else {
            let (word, remainder) =
                after.split_at(after.find(char::is_whitespace).unwrap_or(after.len()));
            if word.is_empty() || word.contains('"') {
                return Err("Use a quoted argument value for spaces or an empty value.".into());
            }
            value = word.to_owned();
            rest = remainder;
        }
        if arguments.insert(key.to_owned(), value).is_some() {
            return Err(format!("Argument '{key}' appears more than once."));
        }
        if arguments.len() > 64 {
            return Err("Use at most 64 prompt arguments.".into());
        }
    }
    Ok((server, prompt.to_owned(), arguments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{ClassificationError, ClassifiedInput, CommandRegistry};

    #[test]
    fn slash_prompt_parses_quotes_and_completes_a_session_catalog() {
        let mut registry = CommandRegistry::default();
        registry.set_mcp_prompts(&[(
            "catalog".parse().unwrap(),
            McpPromptInfo {
                name: "summary".into(),
                description: None,
                arguments: vec![],
            },
        )]);
        assert_eq!(
            registry.matches("/prompt cat", 11)[0].name(),
            "prompt catalog:summary"
        );
        let ClassifiedInput::McpPrompt {
            server,
            prompt,
            arguments,
        } = registry
            .classify(
                r#"/prompt catalog:summary topic="release notes" path="a\\b\"c" format=short"#,
            )
            .unwrap()
        else {
            panic!("prompt route")
        };
        assert_eq!(server.as_str(), "catalog");
        assert_eq!(prompt, "summary");
        assert_eq!(arguments["topic"], "release notes");
        assert_eq!(arguments["path"], "a\\b\"c");
        for invalid in [
            "/prompt",
            "/prompt catalog:",
            "/prompt catalog:summary duplicate=one duplicate=two",
            "/prompt catalog:summary broken=\"open",
        ] {
            assert!(
                matches!(
                    registry.classify(invalid),
                    Err(ClassificationError::InvalidMcpPrompt(_))
                ),
                "{invalid}"
            );
        }
    }

    #[test]
    fn inspector_correlates_and_rejects_busy_writes() {
        let mut i = McpInspector::default();
        i.show();
        let SessionCommand::Manage(ManagementCommand::Mcp {
            request_id: first, ..
        }) = i.commands.pop_front().unwrap()
        else {
            panic!("query")
        };
        i.refresh();
        let SessionCommand::Manage(ManagementCommand::Mcp {
            request_id: second, ..
        }) = i.commands.pop_front().unwrap()
        else {
            panic!("query")
        };
        let view = McpManagementResult::View {
            view: McpManagementView {
                revision: "a".into(),
                enabled: true,
                servers: vec![],
                selected: None,
                diagnostics: vec![],
                prompts: vec![],
            },
        };
        assert!(!i.event(&SessionEvent::Manage(ManagementEvent::McpResult {
            request_id: first,
            result: Box::new(view.clone())
        })));
        assert!(i.event(&SessionEvent::Manage(ManagementEvent::McpResult {
            request_id: second,
            result: Box::new(view)
        })));
        i.reload(true);
        assert!(i.commands.is_empty());
        i.reload(false);
        assert!(matches!(
            i.commands.pop_front(),
            Some(SessionCommand::Manage(ManagementCommand::Mcp {
                request: McpManagementRequest::Reload,
                ..
            }))
        ));
    }
}
