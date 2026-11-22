//! Session-global pane switcher. Confirmation opens the child behind the modal.
use crate::{
    hints::{Eligibility, hint_line},
    input::{Action, ChordState, KeyContext},
    jobs::job_ink,
    status_icon::StatusIcon,
    text::{display_width, truncate_display_width, truncate_leading_display_width},
    theme::theme,
    viewport::{render_scrollbar, rows_to_u16},
};
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem},
};
use std::time::Instant;
use zevria_foundation::{SubtaskId, SubtaskKind, SubtaskStatus};
use zevria_presentation::{
    blocks::subtask_icon,
    subtasks::{active_count, summary_title},
};
use zevria_tui_widgets::overlay::{ListNav, ListOutcome, modal};

const MAX_HEIGHT: usize = 14;
const CONTEXT: KeyContext = KeyContext::SubtasksList;
const SEPARATOR: &str = " · ";

pub(crate) struct SubtaskRow {
    pub id: SubtaskId,
    pub kind: SubtaskKind,
    pub title: String,
    pub workspace: Option<String>,
    pub status: Option<SubtaskStatus>,
    pub historical: bool,
    pub visible: bool,
}

pub(crate) struct SubtasksOverlay {
    open: bool,
    rows: Vec<SubtaskRow>,
    nav: ListNav,
    keys: ChordState,
    opened_at: Instant,
}

impl Default for SubtasksOverlay {
    fn default() -> Self {
        Self {
            open: false,
            rows: Vec::new(),
            nav: ListNav::default(),
            keys: ChordState::default(),
            opened_at: Instant::now(),
        }
    }
}

impl SubtasksOverlay {
    pub fn show(&mut self, rows: Vec<SubtaskRow>) {
        self.open = true;
        self.opened_at = Instant::now();
        self.nav = ListNav::default();
        self.keys.clear();
        self.nav.selected = default_selection(&rows);
        self.rows = rows;
    }

    pub fn replace_rows(&mut self, rows: Vec<SubtaskRow>) {
        // Live status changes and out-of-order arrivals must not move the
        // reader's selection to a different child.
        self.nav.selected = self
            .rows
            .get(self.nav.selected)
            .and_then(|old| rows.iter().position(|row| row.id == old.id))
            .unwrap_or_else(|| default_selection(&rows));
        self.rows = rows;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.rows.clear();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn has_active_rows(&self) -> bool {
        active_count(self.rows.iter().map(|row| row.status)) > 0
    }

    pub fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    pub fn hint_eligibility(&self) -> Eligibility {
        Eligibility {
            labels: vec![(Action::Confirm, "inspect")],
            disabled: if self.rows.is_empty() {
                vec![Action::Confirm]
            } else {
                Vec::new()
            },
        }
    }

    pub fn handle_input(&mut self, key: KeyEvent) -> Option<SubtaskId> {
        let action = self.keys.resolve(CONTEXT, key)?.list_action()?;
        match self.nav.handle(action, self.rows.len()) {
            ListOutcome::Close => {
                self.close();
                None
            }
            ListOutcome::Confirm => {
                let id = self.rows.get(self.nav.selected)?.id.clone();
                self.close();
                Some(id)
            }
            ListOutcome::Handled => None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, body: Rect) {
        let row_count = self.rows.len().max(1);
        // Paths need more breathing room than in the session picker, but cap
        // the reading width on very wide terminals.
        let width = (body.width - body.width / 5).clamp(24, 120).min(body.width);
        let height = rows_to_u16((row_count.saturating_add(2)).min(MAX_HEIGHT)).min(body.height);
        let area = Rect::new(
            body.x + (body.width - width) / 2,
            body.y + (body.height - height) / 2,
            width,
            height,
        );
        let title_width = usize::from(area.width.saturating_sub(2));
        let title = summary_title(self.rows.iter().map(|row| row.status));
        let footer = hint_line(CONTEXT, &self.hint_eligibility(), title_width);
        let inner = modal(
            frame,
            area,
            truncate_display_width(&format!(" {title} "), title_width),
            footer,
        );
        self.nav.reconcile(row_count, usize::from(inner.height));
        let visible = self.nav.viewport.visible_range();
        let spin = ((self.opened_at.elapsed().as_millis() / 250) % 4) as usize;
        let items = if self.rows.is_empty() {
            vec![ListItem::new(Line::styled(
                "No subtasks in this session.",
                crate::chrome::dim_style(),
            ))]
        } else {
            (visible.start()..visible.end())
                .map(|index| {
                    let mut line = subtask_line(&self.rows[index], usize::from(inner.width), spin);
                    self.nav.style_line(index, &mut line);
                    ListItem::new(line)
                })
                .collect()
        };
        frame.render_widget(List::new(items), inner);
        if !self.rows.is_empty() {
            self.nav.paint_selected(frame, inner);
        }
        render_scrollbar(frame, area, &self.nav.viewport);
    }
}

fn default_selection(rows: &[SubtaskRow]) -> usize {
    rows.iter()
        .position(|row| row.visible)
        .or_else(|| {
            rows.iter()
                .rposition(|row| row.status.is_none_or(|status| !status.is_terminal()))
        })
        .unwrap_or(0)
}

fn subtask_line(row: &SubtaskRow, width: usize, spin: usize) -> Line<'static> {
    let marker = if row.visible { "▸ " } else { "  " };
    let icon = row.status.map_or(StatusIcon::Pending, subtask_icon);
    let glyph = icon.glyph(spin);
    let (kind, ink) = match row.kind {
        SubtaskKind::Explore => ("Explore ", theme().workflow.explore),
        SubtaskKind::Build => ("Build   ", theme().workflow.build),
    };
    let content = width
        .saturating_sub(display_width(marker) + display_width(glyph) + 1 + display_width(kind));
    let meta = subtask_meta(row, (content / 2).saturating_sub(1));
    let meta_width = display_width(&meta);
    let title_width = if meta.is_empty() {
        content
    } else {
        content.saturating_sub(meta_width + 2)
    };
    let title = truncate_display_width(&row.title.replace(char::is_control, " "), title_width);
    let padding = if meta.is_empty() {
        0
    } else {
        content.saturating_sub(display_width(&title) + meta_width)
    };
    Line::from(vec![
        Span::raw(marker),
        Span::styled(format!("{glyph} "), Style::new().fg(job_ink(icon))),
        Span::styled(kind, Style::new().fg(ink)),
        Span::raw(title),
        Span::raw(" ".repeat(padding)),
        Span::styled(meta, crate::chrome::dim_style()),
    ])
}

fn subtask_meta(row: &SubtaskRow, width: usize) -> String {
    let mut text = row
        .status
        .map_or_else(|| "pending".into(), |status| status.to_string());
    let historical = if row.historical { " · historical" } else { "" };
    // Shorten the path first so status and historical provenance stay legible.
    let available = width.saturating_sub(
        display_width(&text) + display_width(historical) + display_width(SEPARATOR),
    );
    if let Some(workspace) = &row.workspace
        && available > 0
    {
        text.push_str(SEPARATOR);
        text.push_str(&truncate_leading_display_width(
            &workspace.replace(char::is_control, " "),
            available,
        ));
    }
    text.push_str(historical);
    truncate_display_width(&text, width)
}

#[cfg(test)]
#[path = "subtask_view_tests.rs"]
mod tests;
