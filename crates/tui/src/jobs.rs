//! Global shell-job overlay. The shared inspector owns the command lifecycle.
use crate::{
    hints::{Eligibility, hint_line},
    input::{Action, ChordState, KeyContext},
    status_icon::StatusIcon,
    text::{display_width, truncate_display_width},
    theme::theme,
};
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::time::Instant;
use zevria_foundation::{CommandJobId, CommandJobSnapshot};
use zevria_presentation::jobs::{
    JobRow, JobsInspector, JobsMessage, summary_title, tail_byte_label, terminal_tail_text,
};
use zevria_tui_widgets::overlay::{ListAction, ListNav, modal};

pub(crate) fn job_ink(icon: StatusIcon) -> Color {
    match icon {
        StatusIcon::Running => theme().feedback.info,
        StatusIcon::Done => theme().feedback.success,
        StatusIcon::Failed => theme().feedback.error,
        StatusIcon::Interrupted => theme().feedback.warning,
        _ => theme().text.muted,
    }
}

pub(crate) struct JobsOverlay {
    follow: bool,
    nav: ListNav,
    keys: ChordState,
    opened_at: Instant,
}

impl Default for JobsOverlay {
    fn default() -> Self {
        Self {
            follow: false,
            nav: ListNav::default(),
            keys: ChordState::default(),
            opened_at: Instant::now(),
        }
    }
}

impl JobsOverlay {
    pub fn show(&mut self, inspector: &mut JobsInspector) {
        if !inspector.is_open() {
            self.opened_at = Instant::now();
            self.follow = false;
            self.keys.clear();
            self.nav.request_reveal();
            self.invalidate_geometry();
            inspector.show();
        }
    }

    pub fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    fn selected<'a>(&self, inspector: &'a JobsInspector) -> Option<&'a CommandJobSnapshot> {
        if let Some(id) = inspector.inspected() {
            inspector.jobs().iter().find(|job| &job.id == id)
        } else {
            inspector.jobs().get(self.nav.selected)
        }
    }

    pub fn context(&self, inspector: &JobsInspector) -> KeyContext {
        if inspector.inspected().is_some() {
            KeyContext::JobsInspect
        } else {
            KeyContext::JobsList
        }
    }

    pub fn hint_eligibility(&self, inspector: &JobsInspector) -> Eligibility {
        let mut hints = Eligibility::default();
        hints
            .labels
            .extend([(Action::Confirm, "inspect"), (Action::Back, "list")]);
        let job = self.selected(inspector);
        if job.is_none() {
            hints.disabled.push(Action::Confirm);
        }
        if !job.is_some_and(JobsInspector::can_terminate) {
            hints.disabled.push(Action::Terminate);
        }
        if !job.is_some_and(JobsInspector::can_background) {
            hints.disabled.push(Action::MoveToBackground);
        }
        hints
    }

    pub fn selection_anchor(&self, inspector: &JobsInspector) -> (bool, Option<CommandJobId>) {
        (
            inspector.inspected().is_some(),
            self.selected(inspector).map(|job| job.id.clone()),
        )
    }

    pub fn reconcile(
        &mut self,
        inspector: &JobsInspector,
        (inspecting, selected): (bool, Option<CommandJobId>),
    ) {
        if inspector.inspected().is_none() {
            if inspecting {
                self.return_to_list();
            }
            if let Some(id) = selected
                && let Some(index) = inspector.jobs().iter().position(|job| job.id == id)
                && index != self.nav.selected
            {
                self.nav.selected = index;
                self.nav.request_reveal();
            }
            self.nav.selected = self
                .nav
                .selected
                .min(inspector.jobs().len().saturating_sub(1));
        }
    }

    fn return_to_list(&mut self) {
        self.follow = false;
        self.nav.viewport.jump_start();
        self.nav.request_reveal();
    }

    pub fn handle_input(&mut self, inspector: &mut JobsInspector, key: KeyEvent) {
        let Some(action) = self.keys.resolve(self.context(inspector), key) else {
            return;
        };
        if self.hint_eligibility(inspector).disabled.contains(&action) {
            return;
        }
        match action {
            Action::Cancel | Action::Close => inspector.close(),
            Action::Back => {
                inspector.back();
                self.return_to_list();
            }
            Action::Confirm if inspector.inspected().is_none() => {
                if let Some(job) = self.selected(inspector) {
                    inspector.inspect(job.id.clone());
                    self.follow = true;
                }
            }
            Action::Terminate | Action::MoveToBackground => {
                if let Some(job) = self.selected(inspector) {
                    let id = job.id.clone();
                    if action == Action::Terminate {
                        inspector.terminate(id);
                    } else {
                        inspector.move_to_background(id);
                    }
                }
            }
            _ => {
                if let Some(action) = action.list_action() {
                    if inspector.inspected().is_some() {
                        self.nav.pan(action);
                        self.follow = action == ListAction::End
                            || self.nav.viewport.top() >= self.nav.viewport.max_top();
                    } else {
                        self.nav.handle(action, inspector.jobs().len());
                    }
                }
            }
        }
    }

    pub fn render(&mut self, inspector: &JobsInspector, frame: &mut Frame, area: Rect) {
        let spin = ((self.opened_at.elapsed().as_millis() / 250) % 4) as usize;
        let title_width = usize::from(area.width.saturating_sub(2));
        let inspected = inspector.inspected();
        let row = inspected
            .and_then(|_| self.selected(inspector))
            .map(JobRow::from);
        let title = row.as_ref().map_or_else(
            || {
                Line::raw(truncate_display_width(
                    &format!(" {} ", summary_title(inspector.jobs())),
                    title_width,
                ))
            },
            |row| inspect_title(row, title_width, spin),
        );
        let footer = hint_line(
            self.context(inspector),
            &self.hint_eligibility(inspector),
            title_width,
        );
        let mut inner = modal(frame, area, title, footer);
        let width = usize::from(inner.width);
        let muted = Style::new().fg(theme().text.muted);

        if let Some(id) = inspected
            && inner.height > 1
        {
            frame.render_widget(
                Line::styled(truncate_display_width(id.as_str(), width), muted),
                Rect { height: 1, ..inner },
            );
            inner.y = inner.y.saturating_add(1);
            inner.height -= 1;
        }
        if let Some(message) = inspector.message()
            && inner.height > 1
        {
            let (text, style) = match message {
                JobsMessage::Accepted => ("Control accepted".into(), muted),
                JobsMessage::Rejected(message) => (
                    terminal_tail_text(message)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    Style::new().fg(theme().feedback.error),
                ),
            };
            inner.height -= 1;
            frame.render_widget(
                Line::styled(truncate_display_width(&text, width), style),
                Rect {
                    y: inner.y.saturating_add(inner.height),
                    height: 1,
                    ..inner
                },
            );
        }
        let (lines, entries) = if inspected.is_some() {
            (self.detail_lines(inspector, row.as_ref()), Vec::new())
        } else if inspector.jobs().is_empty() {
            (
                vec![Line::styled("No commands in this session.", muted)],
                Vec::new(),
            )
        } else {
            let lines = list_lines(inspector.jobs(), width, spin);
            let entries = (0..lines.len())
                .map(|n| crate::viewport::RowRange::from_start_len(n, 1))
                .collect();
            (lines, entries)
        };
        let physical = if entries.is_empty() {
            crate::layout::prepare::wrapped_height(&lines, inner.width.max(1))
        } else {
            lines.len()
        };
        let has_list = !entries.is_empty();
        // Inspection pans the output only; keep the job identity pinned above it.
        if inspected.is_some() {
            self.nav
                .viewport
                .reconcile(physical, usize::from(inner.height));
        } else {
            self.nav
                .reconcile_rows(entries, physical, usize::from(inner.height));
        }
        if self.follow {
            self.nav.viewport.jump_end();
        }
        let body = Paragraph::new(lines)
            .style(crate::chrome::overlay_style())
            .scroll((crate::viewport::rows_to_u16(self.nav.viewport.top()), 0));
        frame.render_widget(
            if has_list {
                body
            } else {
                body.wrap(Wrap { trim: false })
            },
            inner,
        );
        if has_list {
            self.nav.paint_selected(frame, inner);
        }
        crate::viewport::render_scrollbar(frame, area, &self.nav.viewport);
    }

    fn detail_lines(&self, inspector: &JobsInspector, row: Option<&JobRow>) -> Vec<Line<'static>> {
        let t = theme();
        let mut lines = Vec::new();
        if let Some(detail) = inspector.detail() {
            for (name, tail, bytes, color) in [
                (
                    "stdout",
                    &detail.stdout_tail,
                    detail.stdout_bytes,
                    t.text.primary,
                ),
                (
                    "stderr",
                    &detail.stderr_tail,
                    detail.stderr_bytes,
                    t.feedback.warning,
                ),
            ] {
                if !tail.is_empty() {
                    lines.push(
                        Span::styled(
                            format!("{name} ({}):", tail_byte_label(bytes)),
                            Style::new().fg(t.text.secondary),
                        )
                        .into(),
                    );
                    lines.extend(
                        terminal_tail_text(tail)
                            .lines()
                            .map(|text| Line::styled(text.to_owned(), Style::new().fg(color))),
                    );
                }
            }
        }
        if lines.is_empty() {
            let text = if row.is_some_and(|row| row.icon != StatusIcon::Running) {
                "No output."
            } else if inspector.detail().is_some() {
                "No output yet."
            } else {
                "Loading output tails…"
            };
            lines.push(Line::styled(text, Style::new().fg(t.text.muted)));
        }
        lines
    }
}

fn glyph(icon: StatusIcon, spin: usize) -> Span<'static> {
    Span::styled(icon.glyph(spin), Style::new().fg(job_ink(icon)))
}

fn shell_spans(command: &str, width: usize) -> Vec<Span<'static>> {
    crate::syntax::highlighted_code_lines(
        &truncate_display_width(command, width),
        Some("bash"),
        Style::new().fg(theme().text.primary),
    )
    .into_iter()
    .next()
    .unwrap_or_default()
    .spans
}

fn inspect_title(row: &JobRow, width: usize, spin: usize) -> Line<'static> {
    let meta = format!(" · {} · {} · {} ", row.mode, row.status, row.duration);
    let meta_width = display_width(&meta).min(width.saturating_sub(10) / 2);
    let mut spans = vec![Span::raw(" "), glyph(row.icon, spin), Span::raw(" ")];
    spans.extend(shell_spans(
        &row.command.text,
        width.saturating_sub(meta_width + 3),
    ));
    spans.push(Span::styled(
        truncate_display_width(&meta, meta_width),
        Style::new().fg(theme().text.muted),
    ));
    Line::from(spans)
}

fn list_lines(jobs: &[CommandJobSnapshot], width: usize, spin: usize) -> Vec<Line<'static>> {
    let rows: Vec<_> = jobs.iter().map(JobRow::from).collect();
    let duration = rows
        .iter()
        .map(|row| display_width(&row.duration))
        .max()
        .unwrap_or(0);
    let status = rows
        .iter()
        .map(|row| display_width(&row.status))
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(duration + 12) / 2)
        .min(24);
    let meta_width = if status > 0 { 8 + duration + status } else { 0 };
    let command_width = width.saturating_sub(meta_width + 2);
    let muted = Style::new().fg(theme().text.muted);
    rows.into_iter()
        .map(|row| {
            let mut spans = vec![glyph(row.icon, spin), Span::raw(" ")];
            let command = shell_spans(&row.command.text, command_width);
            let used = command.iter().map(|span| span.width()).sum::<usize>();
            spans.extend(command);
            if status > 0 {
                let pad = command_width.saturating_sub(used);
                let phrase = truncate_display_width(&row.status, status);
                spans.push(Span::styled(
                    format!(
                        "{}  {}  {:>duration$}  {phrase}",
                        " ".repeat(pad),
                        row.mode,
                        row.duration
                    ),
                    muted,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
#[path = "job_view_tests.rs"]
mod tests;
