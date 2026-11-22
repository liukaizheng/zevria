//! Terminal MCP inspector, using the same connection and prompt state as Desktop.
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{Frame, layout::Rect, style::Style, text::Line, widgets::Paragraph};
use zevria_foundation::{McpServerStatus, SessionMode};
use zevria_presentation::mcp::{McpInspector, McpTab, status_label, tool_policy, transport_label};
use zevria_session_api::TurnCommand;

use crate::{input::UserInput, theme::theme};

#[derive(Default)]
pub(crate) struct McpOverlay {
    pub inspector: McpInspector,
    server_row: usize,
    row: usize,
    arg: usize,
}

impl McpOverlay {
    pub fn show(&mut self) {
        self.server_row = 0;
        self.row = 0;
        self.arg = 0;
        self.inspector.show();
    }

    fn selected(&self) -> Option<zevria_foundation::McpServerName> {
        self.inspector.server.clone().or_else(|| {
            let servers = &self.inspector.view.as_ref()?.servers;
            servers
                .get(self.server_row.min(servers.len().saturating_sub(1)))
                .map(|s| s.name.clone())
        })
    }

    pub fn input(
        &mut self,
        input: UserInput,
        busy: bool,
        mode: SessionMode,
    ) -> Option<TurnCommand> {
        if let Some(draft) = &mut self.inspector.draft {
            let n = draft.prompt.arguments.len();
            self.arg = self.arg.min(n.saturating_sub(1));
            let arg = draft
                .prompt
                .arguments
                .get(self.arg)
                .map(|arg| arg.name.clone());
            match input {
                UserInput::Paste(text) => {
                    if let Some(arg) = arg {
                        draft
                            .values
                            .entry(arg)
                            .or_default()
                            .extend(text.chars().filter(|c| !c.is_control()).take(16384));
                    }
                }
                UserInput::Key(key) => match key.code {
                    KeyCode::Esc => self.inspector.back(),
                    KeyCode::Enter => return self.inspector.run_prompt(mode, busy),
                    KeyCode::Tab | KeyCode::Down => {
                        self.arg = (self.arg + 1).min(n.saturating_sub(1))
                    }
                    KeyCode::BackTab | KeyCode::Up => self.arg = self.arg.saturating_sub(1),
                    KeyCode::Backspace => {
                        if let Some(arg) = arg {
                            draft.values.entry(arg).or_default().pop();
                        }
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        if let Some(arg) = arg {
                            let value = draft.values.entry(arg).or_default();
                            if value.len() < 16384 {
                                value.push(c);
                            }
                        }
                    }
                    _ => {}
                },
            }
            return None;
        }
        let UserInput::Key(key) = input else {
            return None;
        };
        let inspecting = self.inspector.server.is_some();
        let len = if inspecting {
            self.detail_rows().len()
        } else {
            self.inspector
                .view
                .as_ref()
                .map_or(0, |view| view.servers.len())
        };
        let row = if inspecting {
            &mut self.row
        } else {
            &mut self.server_row
        };
        *row = (*row).min(len.saturating_sub(1));
        match key.code {
            KeyCode::Esc => self.inspector.back(),
            KeyCode::Char('j') | KeyCode::Down => *row = (*row + 1).min(len.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *row = row.saturating_sub(1),
            KeyCode::Tab | KeyCode::BackTab if inspecting => {
                let index = McpTab::ALL
                    .iter()
                    .position(|tab| *tab == self.inspector.tab)
                    .unwrap();
                self.inspector.tab =
                    McpTab::ALL[(index + if key.code == KeyCode::Tab { 1 } else { 2 }) % 3];
                self.row = 0;
            }
            KeyCode::Enter if inspecting && self.inspector.tab == McpTab::Prompts => {
                self.arg = 0;
                self.inspector.draft_prompt(self.row);
            }
            KeyCode::Enter if !inspecting => {
                if let Some(server) = self.selected() {
                    self.row = 0;
                    self.inspector.inspect(server);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(server) = self.selected() {
                    self.inspector.toggle(busy, server);
                }
            }
            KeyCode::Char('r') => {
                if let Some(server) = self.selected() {
                    self.inspector.reconnect(busy, server);
                }
            }
            KeyCode::Char('R') => self.inspector.reload(busy),
            _ => {}
        }
        None
    }

    fn detail_rows(&self) -> Vec<String> {
        let Some(detail) = self.inspector.detail() else {
            return vec![];
        };
        match self.inspector.tab {
            McpTab::Tools => detail
                .tools
                .iter()
                .map(|tool| {
                    format!(
                        "{}  [{}]  {}",
                        tool.name,
                        tool_policy(tool),
                        tool.description
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                    )
                })
                .collect(),
            McpTab::Resources => {
                detail
                    .resources
                    .iter()
                    .map(|r| format!("{}  {}", r.name, terminal_text(&r.uri)))
                    .chain(detail.templates.iter().map(|r| {
                        format!("Template: {}  {}", r.name, terminal_text(&r.uri_template))
                    }))
                    .collect()
            }
            McpTab::Prompts => detail
                .prompts
                .iter()
                .map(|p| {
                    format!(
                        "{}  ({} arguments)  {}",
                        p.name,
                        p.arguments.len(),
                        p.description
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                    )
                })
                .collect(),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let i = &self.inspector;
        let title = i.server.as_ref().map_or_else(
            || " MCP servers ".into(),
            |server| format!(" MCP / {server} "),
        );
        let keys = if i.draft.is_some() {
            "Tab next argument   Enter run   Esc back"
        } else if i.server.is_some() {
            "j/k move   Tab section   Enter run prompt   Esc back"
        } else {
            "j/k move   Enter inspect   Space toggle   r reconnect   R reload   Esc close"
        };
        let inner =
            zevria_tui_widgets::overlay::modal(frame, area, Line::raw(title), Line::raw(keys));
        let ink = theme();
        let mut header = vec![];
        if let Some(notice) = &i.notice {
            header.push(Line::styled(
                notice.clone(),
                Style::default().fg(ink.feedback.warning),
            ));
        }
        if i.loading() {
            header.push(Line::styled(
                "Updating MCP servers…",
                Style::default().fg(ink.text.muted),
            ));
        }
        let mut rows: Vec<(String, Style)> = vec![];
        let selected;
        if let Some(draft) = &i.draft {
            header.push(Line::raw(format!(
                "Run {}:{}",
                draft.server, draft.prompt.name
            )));
            if draft.prompt.arguments.is_empty() {
                header.push(Line::raw("No arguments. Press Enter to run."));
            }
            rows.extend(draft.prompt.arguments.iter().map(|arg| {
                (
                    format!(
                        "{}{}: {}",
                        arg.name,
                        if arg.required { " *" } else { "" },
                        draft.values.get(&arg.name).map_or("", String::as_str)
                    ),
                    Style::default(),
                )
            }));
            selected = self.arg;
        } else if let Some(detail) = i.detail() {
            header.push(Line::raw(format!(
                "{}   {}",
                status_label(&detail.summary.status),
                transport_label(&detail.summary.transport)
            )));
            if let McpServerStatus::Failed { error, stderr_tail } = &detail.summary.status {
                header.push(Line::styled(
                    error.clone(),
                    Style::default().fg(ink.feedback.error),
                ));
                if let Some(tail) = stderr_tail {
                    header.extend(
                        tail.lines()
                            .rev()
                            .take(3)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .map(|line| Line::raw(line.to_owned())),
                    );
                }
            }
            if let Some(instructions) = &detail.instructions {
                header.push(Line::styled(
                    format!(
                        "Instructions: {}",
                        instructions.lines().next().unwrap_or("")
                    ),
                    Style::default().fg(ink.text.muted),
                ));
            }
            header.push(Line::from(
                McpTab::ALL
                    .map(|tab| {
                        if tab == i.tab {
                            format!("[{}]  ", tab.title())
                        } else {
                            format!(" {}   ", tab.title())
                        }
                    })
                    .join(""),
            ));
            rows.extend(
                self.detail_rows()
                    .into_iter()
                    .map(|row| (row, Style::default())),
            );
            if rows.is_empty() {
                header.push(Line::raw(format!(
                    "No {} from this server.",
                    i.tab.title().to_lowercase()
                )));
            }
            selected = self.row;
        } else {
            if let Some(view) = &i.view {
                if !view.enabled {
                    header.push(Line::raw("MCP is disabled in config.toml."));
                }
                for server in &view.servers {
                    let (glyph, color) = match server.status {
                        McpServerStatus::Connected => ("●", ink.feedback.success),
                        McpServerStatus::Failed { .. } => ("✗", ink.feedback.error),
                        _ => ("○", ink.text.muted),
                    };
                    let text = if let McpServerStatus::Failed { error, .. } = &server.status {
                        error.clone()
                    } else {
                        format!(
                            "{} tools / {} resources / {} prompts",
                            server.tool_count, server.resource_count, server.prompt_count
                        )
                    };
                    let transport = match server.transport {
                        zevria_foundation::McpTransportSummary::Stdio { .. } => "stdio",
                        _ => "HTTP",
                    };
                    rows.push((
                        format!(
                            "{glyph} {}  {} / {transport}   {text}",
                            server.name,
                            status_label(&server.status)
                        ),
                        Style::default().fg(color),
                    ));
                }
                if view.servers.is_empty() {
                    header.push(Line::raw("No servers. Add [mcp.servers] in config.toml."));
                }
            }
            selected = self.server_row;
        }
        let height = usize::from(inner.height).saturating_sub(header.len());
        let selected = selected.min(rows.len().saturating_sub(1));
        let top = (selected + 1).saturating_sub(height);
        header.extend(rows.into_iter().enumerate().skip(top).take(height).map(
            |(idx, (row, style))| {
                if idx == selected {
                    Line::styled(
                        format!("› {row}"),
                        style.add_modifier(ratatui::style::Modifier::REVERSED),
                    )
                } else {
                    Line::styled(format!("  {row}"), style)
                }
            },
        ));
        frame.render_widget(Paragraph::new(header), inner);
    }
}

fn terminal_text(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::{
        McpManagementRequest, McpManagementView, McpServerOrigin, McpServerSummary,
        McpTransportSummary, PlanMcpPolicy,
    };

    #[test]
    fn keys_select_servers_and_only_enqueue_idle_mutations() {
        let mut overlay = McpOverlay::default();
        overlay.show();
        overlay.inspector.commands.clear();
        overlay.inspector.view = Some(McpManagementView {
            revision: "view".into(),
            enabled: true,
            selected: None,
            diagnostics: vec![],
            prompts: vec![],
            servers: ["alpha", "beta"]
                .into_iter()
                .map(|name| McpServerSummary {
                    name: name.parse().unwrap(),
                    origin: McpServerOrigin::Config,
                    enabled: true,
                    status: McpServerStatus::Connected,
                    transport: McpTransportSummary::Stdio {
                        command: "fixture".into(),
                        args: vec![],
                    },
                    plan_mode: PlanMcpPolicy::ReadOnly,
                    tool_count: 1,
                    resource_count: 0,
                    prompt_count: 0,
                    connect_ms: Some(1),
                })
                .collect(),
        });
        let key = |code| UserInput::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE));
        overlay.input(key(KeyCode::Char('j')), false, SessionMode::Build);
        overlay.input(key(KeyCode::Char(' ')), true, SessionMode::Build);
        assert!(overlay.inspector.commands.is_empty());
        overlay.input(key(KeyCode::Enter), true, SessionMode::Build);
        let Some(zevria_session_api::SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Mcp {
                request: McpManagementRequest::Inspect { server },
                ..
            },
        )) = overlay.inspector.commands.pop_front()
        else {
            panic!("inspect request");
        };
        assert_eq!(server.as_str(), "beta");
        overlay.input(key(KeyCode::Tab), true, SessionMode::Build);
        assert_eq!(overlay.inspector.tab, McpTab::Resources);
    }
}
