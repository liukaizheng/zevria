//! Metadata-only root skill manager. No filesystem access and no root editor.
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use std::collections::{HashMap, VecDeque};
use zevria_instructions::skill::SkillManagementRequest as Request;
use zevria_instructions::skill::SkillManagementResult as Result;
use zevria_instructions::skill::SkillManagementView;
use zevria_instructions::skill::SkillMeta;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;

use crate::hints::{Eligibility, hint_line};
use crate::input::{Action, ChordState, KeyContext};
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use zevria_tui_widgets::overlay::{ListNav, modal};

#[derive(Clone)]
enum Purpose {
    Completion,
    Browser(u64),
    Mutation,
}

#[derive(Default)]
enum Browser {
    #[default]
    Closed,
    List,
    Inspect,
}

#[derive(Default)]
pub(crate) struct SkillManager {
    browser: Browser,
    page: Option<SkillManagementView>,
    nav: ListNav,
    keys: ChordState,
    filter: Option<crate::composer::ComposerState>,
    query: String,
    message: String,
    next_id: u64,
    pending: HashMap<String, Purpose>,
    pub commands: VecDeque<SessionCommand>,
    refresh_again: bool,
    browser_generation: u64,
}

impl SkillManager {
    pub(crate) fn is_open(&self) -> bool {
        !matches!(self.browser, Browser::Closed)
    }
    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    fn send(&mut self, request: Request, purpose: Purpose) {
        if self.pending.len() >= 32 {
            self.message = "Too many pending skill requests; wait for their results".into();
            return;
        }
        self.next_id += 1;
        let id = format!("tui.skills.{}", self.next_id);
        self.pending.insert(id.clone(), purpose);
        self.commands.push_back(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Skills {
                request_id: id,
                request,
            },
        ));
    }
    pub fn refresh(&mut self) {
        if self
            .pending
            .values()
            .any(|purpose| matches!(purpose, Purpose::Completion))
        {
            self.refresh_again = true;
            return;
        }
        self.send(
            Request::List {
                query: String::new(),
            },
            Purpose::Completion,
        );
    }
    pub fn show(&mut self) {
        self.browser = Browser::List;
        self.filter = None;
        self.nav.request_reveal();
        self.invalidate_geometry();
        self.query.clear();
        self.browse();
    }
    fn browse(&mut self) {
        self.message = "Loading installed metadata…".into();
        let request = if matches!(self.browser, Browser::Inspect) {
            Request::Inspect {
                name: self
                    .query
                    .parse()
                    .expect("inspect query is a validated candidate name"),
            }
        } else {
            Request::List {
                query: self.query.clone(),
            }
        };
        self.browser_generation += 1;
        self.send(request, Purpose::Browser(self.browser_generation));
    }
    pub fn event(&mut self, event: &SessionEvent) -> Option<Vec<SkillMeta>> {
        match event {
            SessionEvent::SkillsChanged { .. } => {
                self.refresh();
                if self.is_open() {
                    self.browse();
                }
            }
            SessionEvent::ModeChanged { .. }
            | SessionEvent::ModeResult { .. }
            | SessionEvent::TurnCompleted { .. }
            | SessionEvent::TurnRecovered { .. }
            | SessionEvent::TurnFailed { .. }
            | SessionEvent::TurnRejected { .. }
            | SessionEvent::TurnCancelled { .. } => self.refresh(),
            SessionEvent::SkillsResult { request_id, result } => {
                let purpose = self.pending.remove(request_id)?;
                if matches!(purpose, Purpose::Browser(generation) if generation != self.browser_generation)
                {
                    return None;
                }
                match (purpose, result) {
                    (Purpose::Completion, Result::View { view }) => {
                        if std::mem::take(&mut self.refresh_again) {
                            self.refresh();
                        } else {
                            return Some(view.completions.clone());
                        }
                    }
                    (Purpose::Browser(_), Result::View { view: page }) => {
                        let previous = self
                            .page
                            .as_ref()
                            .and_then(|view| view.entries.get(self.nav.selected));
                        self.nav.selected = previous
                            .and_then(|selected| {
                                page.entries
                                    .iter()
                                    .position(|entry| {
                                        entry.name == selected.name
                                            && entry.scope == selected.scope
                                            && entry.manifest == selected.manifest
                                    })
                                    .or_else(|| {
                                        page.entries
                                            .iter()
                                            .position(|entry| entry.name == selected.name)
                                    })
                            })
                            .unwrap_or(self.nav.selected.min(page.entries.len().saturating_sub(1)));
                        self.page = Some(page.clone());
                        self.nav.viewport.jump_start();
                        self.invalidate_geometry();
                        self.nav.request_reveal();
                        self.message.clear();
                    }
                    (Purpose::Mutation, Result::Changed { unchanged, .. }) => {
                        self.message = if *unchanged {
                            "Catalog unchanged"
                        } else {
                            "Saved and installed; other sessions require their own reload"
                        }
                        .into();
                        self.refresh();
                        if self.is_open() {
                            self.browse();
                        }
                    }
                    (Purpose::Completion, Result::Error { .. }) => {
                        if std::mem::take(&mut self.refresh_again) {
                            self.refresh();
                        }
                    }
                    (_, Result::Error { code, message }) => {
                        self.message = format!("{code}: {message}")
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }
    pub(crate) fn context(&self) -> KeyContext {
        if self.filter.is_some() {
            KeyContext::TextEntry
        } else if matches!(self.browser, Browser::Inspect) {
            KeyContext::SkillsInspect
        } else {
            KeyContext::SkillsList
        }
    }
    pub(crate) fn hint_eligibility(&self) -> Eligibility {
        let mut hints = Eligibility::default();
        if self.filter.is_none() {
            hints
                .labels
                .extend([(Action::Confirm, "inspect"), (Action::Back, "list")]);
            if self.page.as_ref().is_none_or(|page| page.entries.is_empty())
                || self.pending.values().any(|purpose| matches!(purpose, Purpose::Browser(generation) if *generation == self.browser_generation))
            {
                hints.disabled.extend([Action::Confirm, Action::Toggle, Action::Reload]);
            }
        }
        if matches!(self.browser, Browser::Inspect) {
            hints
                .labels
                .extend([(Action::Down, "scroll down"), (Action::Up, "scroll up")]);
        }
        hints
    }

    pub fn handle_input(&mut self, key: KeyEvent) {
        let Some(action) = self.keys.resolve(self.context(), key) else {
            return;
        };
        if action == Action::Cancel {
            self.browser = Browser::Closed;
            return;
        }
        self.handle_action(action);
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(filter) = &mut self.filter {
            let text: String = text
                .chars()
                .filter(|ch| !ch.is_control())
                .take(256usize.saturating_sub(filter.text().chars().count()))
                .collect();
            filter.insert_text(&text);
        }
    }

    #[cfg(test)]
    pub(super) fn key(&mut self, key: KeyCode) {
        self.handle_input(KeyEvent::new(key, KeyModifiers::NONE));
    }

    fn handle_action(&mut self, action: Action) {
        if let Some(filter) = &mut self.filter {
            match action {
                Action::Close => {
                    self.filter = None;
                    self.nav.request_reveal();
                }
                Action::Confirm => {
                    self.query = self
                        .filter
                        .take()
                        .map(|filter| filter.text().to_string())
                        .unwrap_or_default();
                    self.browser = Browser::List;
                    self.browse();
                }
                Action::Backspace => {
                    filter.backspace();
                }
                Action::Type(ch) if filter.text().len() < 256 => filter.insert_character(ch),
                _ => {}
            }
            return;
        }
        if matches!(action, Action::Confirm | Action::Toggle | Action::Reload)
            && self.pending.values().any(|purpose| matches!(purpose, Purpose::Browser(generation) if *generation == self.browser_generation)) {
            return;
        }
        match action {
            Action::Close => self.browser = Browser::Closed,
            Action::Filter => self.filter = Some(crate::composer::ComposerState::default()),
            Action::Confirm if !matches!(self.browser, Browser::Inspect) => {
                if let Some(entry) = self
                    .page
                    .as_ref()
                    .and_then(|page| page.entries.get(self.nav.selected))
                {
                    self.query = entry.name.to_string();
                    self.browser = Browser::Inspect;
                    self.browse();
                }
            }
            Action::Back => {
                self.query.clear();
                self.browser = Browser::List;
                self.browse();
            }
            Action::Reload => {
                if let Some(page) = &self.page {
                    self.send(
                        Request::Reload {
                            expected_revision: page.revision.clone(),
                        },
                        Purpose::Mutation,
                    );
                }
            }
            Action::Toggle => {
                if let Some(page) = &self.page
                    && let Some(entry) = page.entries.get(self.nav.selected)
                {
                    self.send(
                        Request::SetEnabled {
                            expected_revision: page.revision.clone(),
                            name: entry.name.clone(),
                            enabled: !entry.enabled,
                        },
                        Purpose::Mutation,
                    );
                }
            }
            _ => {
                if let Some(action) = action.list_action() {
                    if matches!(self.browser, Browser::Inspect) {
                        self.nav.pan(action);
                    } else {
                        self.nav.handle(
                            action,
                            self.page.as_ref().map_or(0, |page| page.entries.len()),
                        );
                    }
                }
            }
        }
    }
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        let mut entry_lines = Vec::new();
        if let Some(page) = &self.page {
            lines.push(Line::from(format!(
                "Global: {}",
                page.global_location.as_deref().unwrap_or("unavailable")
            )));
            lines.push(Line::from(format!(
                "Project: {}",
                page.project_location.as_deref().unwrap_or("unavailable")
            )));
            lines.push(Line::from(format!(
                "Revision {} · {} candidates · {} active · {} diagnostics",
                &page.revision[..page.revision.len().min(12)],
                page.counts.candidates,
                page.counts.active,
                page.counts.diagnostics
            )));
            if !page.globally_enabled {
                lines.push(Line::from("Skills are globally disabled. Name rules cannot override [skills].enabled; edit the config and reload."));
            }
            for (index, entry) in page.entries.iter().enumerate() {
                entry_lines.push(lines.len());
                lines.push(Line::from(format!(
                    "{} {} [{}{}{}] {}",
                    if index == self.nav.selected { ">" } else { " " },
                    entry.name,
                    entry.status,
                    if entry.enabled { "" } else { ", disabled" },
                    if entry.pinned { ", pinned" } else { "" },
                    entry.metadata.description
                )));
                if matches!(self.browser, Browser::Inspect) {
                    lines.push(Line::from(format!(
                        "  Source: {:?} {} · resources: {} · policy: {:?}",
                        entry.scope,
                        entry
                            .manifest
                            .as_ref()
                            .map(|p| p.as_str())
                            .unwrap_or("body-only"),
                        entry.resources,
                        entry.metadata.invocation_policy
                    )));
                    lines.push(Line::from(format!(
                        "  Interface: {:?}",
                        entry.metadata.interface
                    )));
                    for dependency in &entry.metadata.dependencies {
                        lines.push(Line::from(format!(
                            "  Declaration (inert): {} = {}",
                            dependency.kind, dependency.value
                        )));
                    }
                    if entry.metadata_shortened {
                        lines.push(Line::from("  Metadata shortened to its per-field limits"));
                    }
                }
            }
            for invalid in &page.invalid_entries {
                lines.push(Line::from(format!(
                    "! [invalid] {:?}: {} · {}",
                    invalid.scope,
                    invalid.manifest.as_str(),
                    invalid.diagnostic
                )));
            }
            for diagnostic in &page.diagnostics {
                lines.push(Line::from(format!("! {diagnostic}")));
            }
            if page.omitted_diagnostics > 0 {
                lines.push(Line::from(format!(
                    "{} additional diagnostics omitted",
                    page.omitted_diagnostics
                )));
            }
        }
        lines.push(Line::from(self.message.clone()));
        if let Some(filter) = &self.filter {
            lines.push(Line::from(format!("Filter: {}_", filter.text())));
        }
        let style = crate::chrome::overlay_style();
        let footer = hint_line(
            self.context(),
            &self.hint_eligibility(),
            usize::from(area.width.saturating_sub(2)),
        );
        let inner = modal(frame, area, " Skills ", footer);
        let mut physical_row = 0;
        let line_rows = lines
            .iter()
            .map(|line| {
                let start = physical_row;
                physical_row += Paragraph::new(line.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width);
                crate::viewport::RowRange::new(start, physical_row)
            })
            .collect::<Vec<_>>();
        let entry_rows = entry_lines
            .into_iter()
            .map(|index| line_rows[index])
            .collect();
        let paragraph = Paragraph::new(lines)
            .style(style)
            .wrap(Wrap { trim: false });
        self.nav.reconcile_rows(
            entry_rows,
            paragraph.line_count(inner.width),
            usize::from(inner.height),
        );
        if self.filter.is_some()
            && let Some(rows) = line_rows.last()
        {
            self.nav.viewport.reveal_end(*rows);
        }
        frame.render_widget(
            paragraph.scroll((crate::viewport::rows_to_u16(self.nav.viewport.top()), 0)),
            inner,
        );
        if self
            .page
            .as_ref()
            .is_some_and(|page| !page.entries.is_empty())
            && self.filter.is_none()
        {
            self.nav.paint_selected(frame, inner);
        }
        crate::viewport::render_scrollbar(frame, area, &self.nav.viewport);
    }
}
