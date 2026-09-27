//! Modal session picker for the `/resume` command.
//!
//! The picker owns its selected index and render-only viewport. The runtime
//! injects summaries listed from disk, key handling reduces to a
//! [`PickerAction`], and the chosen session is executed by the runtime — no IO
//! happens here.

use std::path::PathBuf;
use std::time::SystemTime;

use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem},
};
use zevria_transcript::transcript::SessionSummary;

use crate::hints::{Eligibility, hint_line};
use crate::input::{ChordState, KeyContext};
use crate::theme::theme;
use crate::viewport::{render_scrollbar, rows_to_u16};
use zevria_tui_widgets::overlay::{ListNav, ListOutcome, modal};

/// Widest relative-time label ("365d ago"), for column alignment.
const AGE_COLUMN_WIDTH: usize = 8;
/// Cap on the modal's outer height, borders included.
const MAX_PICKER_HEIGHT: usize = 14;

/// What one key press did to the picker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PickerAction {
    /// Consumed with no session chosen yet.
    Handled,
    /// Dismiss the picker without resuming anything.
    Close,
    /// Resume the session persisted at this path.
    Choose(PathBuf),
}

/// Modal list of resumable sessions, newest first.
pub(crate) struct SessionPicker {
    sessions: Vec<SessionSummary>,
    nav: ListNav,
    keys: ChordState,
}

impl SessionPicker {
    pub(crate) fn new(sessions: Vec<SessionSummary>) -> Self {
        Self {
            sessions,
            nav: ListNav::default(),
            keys: ChordState::default(),
        }
    }

    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    pub(crate) fn hint_eligibility(&self) -> Eligibility {
        Eligibility {
            labels: vec![(crate::input::Action::Confirm, "resume")],
            disabled: if self.sessions.is_empty() {
                vec![crate::input::Action::Confirm]
            } else {
                Vec::new()
            },
        }
    }

    /// Every key terminates at this modal, including unsupported modifiers.
    pub(crate) fn handle_input(&mut self, key: KeyEvent) -> PickerAction {
        let Some(action) = self
            .keys
            .resolve(KeyContext::OverlayList, key)
            .and_then(crate::input::Action::list_action)
        else {
            return PickerAction::Handled;
        };
        match self.nav.handle(action, self.sessions.len()) {
            ListOutcome::Close => PickerAction::Close,
            ListOutcome::Confirm => self
                .selected_session()
                .map_or(PickerAction::Handled, |summary| {
                    PickerAction::Choose(summary.path.clone())
                }),
            ListOutcome::Handled => PickerAction::Handled,
        }
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.nav.selected)
    }

    /// Draw the centered modal over whatever pane is behind it.
    pub(crate) fn render(&mut self, frame: &mut Frame, modal_body: Rect) {
        let content_rows = self.sessions.len().max(1);
        let popup_area = centered_area(modal_body, content_rows);
        let hints = hint_line(
            KeyContext::OverlayList,
            &self.hint_eligibility(),
            usize::from(popup_area.width.saturating_sub(2)),
        );
        let inner_area = modal(frame, popup_area, " Resume session ", hints);
        self.nav
            .reconcile(content_rows, usize::from(inner_area.height));
        let visible = self.nav.viewport.visible_range();
        let items: Vec<ListItem<'static>> = if self.sessions.is_empty() {
            if visible.is_empty() {
                Vec::new()
            } else {
                vec![ListItem::new(Line::styled(
                    "No previous sessions".to_string(),
                    crate::chrome::dim_style(),
                ))]
            }
        } else {
            self.sessions[visible.start()..visible.end()]
                .iter()
                .enumerate()
                .map(|(local_index, summary)| {
                    let global_index = visible.start().saturating_add(local_index);
                    let mut row = session_row(summary);
                    self.nav.style_line(global_index, &mut row);
                    ListItem::new(row)
                })
                .collect()
        };
        frame.render_widget(List::new(items), inner_area);
        if !self.sessions.is_empty() {
            self.nav.paint_selected(frame, inner_area);
        }
        render_scrollbar(frame, popup_area, &self.nav.viewport);
    }
}

/// One picker row: age, first-prompt preview, dimmed short id.
fn session_row(summary: &SessionSummary) -> Line<'static> {
    let age = relative_time(summary.modified);
    let preview = summary
        .preview
        .clone()
        .unwrap_or_else(|| "(no messages)".to_string());
    let short_id: String = summary.id.chars().take(8).collect();
    Line::from(vec![
        Span::styled(
            format!("{age:>AGE_COLUMN_WIDTH$}  "),
            Style::default().fg(theme().roles.tools),
        ),
        Span::raw(preview),
        Span::styled(format!("  {short_id}"), crate::chrome::dim_style()),
    ])
}

/// A centered modal rect: ~60% of the supplied body width, sized to the rows
/// but capped, and clamped to that body on tiny terminals.
fn centered_area(modal_body: Rect, row_count: usize) -> Rect {
    let width = (modal_body.width.saturating_mul(3) / 5)
        .max(20)
        .min(modal_body.width);
    let height = rows_to_u16(
        row_count
            .saturating_add(2)
            .min(MAX_PICKER_HEIGHT)
            .min(usize::from(modal_body.height)),
    );
    Rect {
        x: modal_body.x + modal_body.width.saturating_sub(width) / 2,
        y: modal_body.y + modal_body.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Coarse elapsed-time label for a session's last modification.
fn relative_time(modified: SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        // A future mtime (clock skew) still reads sensibly.
        return "just now".to_string();
    };
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}
