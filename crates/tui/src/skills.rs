//! Metadata-only root skill manager. No filesystem access and no root editor.
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use zevria_instructions::skill::SkillMeta;
use zevria_presentation::skills::{SkillChange, SkillInspector};
use zevria_session_api::SessionEvent;

use crate::hints::{Eligibility, hint_line};
use crate::input::{Action, ChordState, KeyContext};
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use zevria_tui_widgets::overlay::{ListNav, modal};

#[derive(Default)]
pub(crate) struct SkillManager {
    pub(crate) inspector: SkillInspector,
    nav: ListNav,
    keys: ChordState,
    filter: Option<crate::composer::ComposerState>,
}

impl SkillManager {
    pub(crate) fn is_open(&self) -> bool {
        self.inspector.is_open()
    }

    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    pub fn show(&mut self) {
        self.filter = None;
        self.nav.request_reveal();
        self.invalidate_geometry();
        self.inspector.show(String::new());
    }

    pub fn event(&mut self, event: &SessionEvent) -> Option<Vec<SkillMeta>> {
        match self.inspector.event(event, self.nav.selected)? {
            SkillChange::Completions(entries) => Some(entries),
            SkillChange::Page { selected } => {
                self.nav.selected = selected;
                self.nav.viewport.jump_start();
                self.invalidate_geometry();
                self.nav.request_reveal();
                None
            }
        }
    }

    pub(crate) fn context(&self) -> KeyContext {
        if self.filter.is_some() {
            KeyContext::TextEntry
        } else if self.inspector.inspecting() {
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
            if self
                .inspector
                .page()
                .is_none_or(|page| page.entries.is_empty())
                || self.inspector.loading()
            {
                hints
                    .disabled
                    .extend([Action::Confirm, Action::Toggle, Action::Reload]);
            }
        }
        if self.inspector.inspecting() {
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
            self.inspector.close();
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
                    let query = self
                        .filter
                        .take()
                        .map(|filter| filter.text().to_string())
                        .unwrap_or_default();
                    self.inspector.show(query);
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
            && self.inspector.loading()
        {
            return;
        }
        match action {
            Action::Close => self.inspector.close(),
            Action::Filter => self.filter = Some(crate::composer::ComposerState::default()),
            Action::Confirm if !self.inspector.inspecting() => {
                self.inspector.inspect(self.nav.selected)
            }
            Action::Back => self.inspector.show(String::new()),
            Action::Reload => self.inspector.reload(),
            Action::Toggle => self.inspector.toggle(self.nav.selected),
            _ => {
                if let Some(action) = action.list_action() {
                    if self.inspector.inspecting() {
                        self.nav.pan(action);
                    } else {
                        self.nav.handle(
                            action,
                            self.inspector.page().map_or(0, |page| page.entries.len()),
                        );
                    }
                }
            }
        }
    }
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        let mut entry_lines = Vec::new();
        if let Some(page) = self.inspector.page() {
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
                if self.inspector.inspecting() {
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
        lines.push(Line::from(self.inspector.message().to_owned()));
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
            .inspector
            .page()
            .is_some_and(|page| !page.entries.is_empty())
            && self.filter.is_none()
        {
            self.nav.paint_selected(frame, inner);
        }
        crate::viewport::render_scrollbar(frame, area, &self.nav.viewport);
    }
}
