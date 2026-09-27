//! Application-facing hints and full context help share the reusable catalogue.
use crate::input::{Action, ChordState, KeyContext};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};
pub(crate) use zevria_tui_input::hints::{Eligibility, hint_line};
use zevria_tui_widgets::{
    overlay::{ListNav, ListOutcome, modal},
    viewport::render_scrollbar,
};

pub(crate) struct HelpOverlay {
    context: KeyContext,
    lines: Vec<Line<'static>>,
    nav: ListNav,
    keys: ChordState,
}
impl HelpOverlay {
    pub(crate) fn new(context: KeyContext, eligibility: &Eligibility) -> Self {
        Self {
            context,
            lines: zevria_tui_input::hints::help_lines(context, eligibility),
            nav: ListNav::default(),
            keys: ChordState::default(),
        }
    }
    pub(crate) fn handle_input(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        let action = self.keys.resolve(KeyContext::Help, key);
        if action == Some(Action::Help) {
            return true;
        }
        action
            .and_then(Action::list_action)
            .is_some_and(|action| self.nav.pan(action) == ListOutcome::Close)
    }
    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }
    pub(crate) fn render(&mut self, frame: &mut Frame, bounds: Rect) {
        let width = bounds.width.min(88);
        let height = bounds.height.min(24);
        let area = Rect::new(
            bounds.x + bounds.width.saturating_sub(width) / 2,
            bounds.y + bounds.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let hints = hint_line(
            KeyContext::Help,
            &Eligibility::default(),
            usize::from(area.width.saturating_sub(2)),
        );
        let inner = modal(
            frame,
            area,
            format!(" {} help ", self.context.title()),
            hints,
        );
        let paragraph = Paragraph::new(self.lines.clone()).wrap(Wrap { trim: false });
        self.nav
            .viewport
            .reconcile(paragraph.line_count(inner.width), usize::from(inner.height));
        frame.render_widget(
            paragraph.scroll((self.nav.viewport.paragraph_offset(), 0)),
            inner,
        );
        render_scrollbar(frame, area, &self.nav.viewport);
    }
}
