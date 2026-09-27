//! Shared terminal-row viewport state and overflow-only scrollbar rendering.
//!
//! The primitive deliberately owns only geometry: each caller retains its own
//! follow, manual-pan, and focus-reveal policy.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use crate::theme::theme;

/// A half-open range in wrapped terminal-row coordinates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RowRange {
    start: usize,
    end: usize,
}

impl RowRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self {
            start,
            end: if end < start { start } else { end },
        }
    }

    pub const fn from_start_len(start: usize, len: usize) -> Self {
        Self::new(start, start.saturating_add(len))
    }

    pub const fn start(self) -> usize {
        self.start
    }

    pub const fn end(self) -> usize {
        self.end
    }

    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub const fn shifted(self, rows: usize) -> Self {
        Self::new(
            self.start.saturating_add(rows),
            self.end.saturating_add(rows),
        )
    }

    #[cfg(test)]
    pub const fn relative_to(self, origin: usize) -> Self {
        Self::new(
            self.start.saturating_sub(origin),
            self.end.saturating_sub(origin),
        )
    }

    pub const fn clamped(self, content_rows: usize) -> Self {
        Self::new(
            if self.start > content_rows {
                content_rows
            } else {
                self.start
            },
            if self.end > content_rows {
                content_rows
            } else {
                self.end
            },
        )
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// A reconciled vertical viewport over wrapped terminal rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    top: usize,
    content_rows: usize,
    visible_rows: usize,
}

impl Viewport {
    pub const fn top(&self) -> usize {
        self.top
    }

    pub const fn visible_rows(&self) -> usize {
        self.visible_rows
    }

    pub const fn max_top(&self) -> usize {
        self.content_rows.saturating_sub(self.visible_rows)
    }

    /// Replace the logical top without consulting stale extents. Rendering
    /// reconciles it against current content and terminal geometry.
    pub const fn set_top(&mut self, top: usize) {
        self.top = top;
    }

    pub const fn pan_up(&mut self, rows: usize) {
        self.top = self.top.saturating_sub(rows);
    }

    pub const fn pan_down(&mut self, rows: usize) {
        self.top = self.top.saturating_add(rows);
        if self.top > self.max_top() {
            self.top = self.max_top();
        }
    }

    pub const fn page_up(&mut self, page_rows: usize) {
        self.pan_up(page_rows);
    }

    pub const fn page_down(&mut self, page_rows: usize) {
        self.pan_down(page_rows);
    }

    pub const fn jump_start(&mut self) {
        self.top = 0;
    }

    pub const fn jump_end(&mut self) {
        self.top = self.max_top();
    }

    /// Retire stale measurements after a resize or surface reallocation without
    /// discarding the logical scroll position. Paging falls back to one row.
    pub const fn invalidate_geometry(&mut self) {
        self.visible_rows = 0;
    }

    /// Update content and visible extents, then clamp the logical top.
    pub fn reconcile(&mut self, content_rows: usize, visible_rows: usize) {
        self.content_rows = content_rows;
        self.visible_rows = visible_rows;
        self.top = self.top.min(self.max_top());
    }

    /// Reveal the full target when it fits. Oversized targets use their start
    /// as the conservative default anchor; callers that need the end use
    /// [`Self::reveal_end`].
    pub fn reveal(&mut self, target: RowRange) {
        let target = target.clamped(self.content_rows);
        if target.is_empty() || self.visible_rows == 0 {
            return;
        }
        if target.len() > self.visible_rows {
            self.reveal_start(target);
            return;
        }
        if target.start() < self.top {
            self.top = target.start();
        } else if target.end() > self.top.saturating_add(self.visible_rows) {
            self.top = target.end().saturating_sub(self.visible_rows);
        }
        self.top = self.top.min(self.max_top());
    }

    /// Ensure the target's first row is visible, anchoring it at the top only
    /// when it lies outside the current viewport.
    pub fn reveal_start(&mut self, target: RowRange) {
        let target = target.clamped(self.content_rows);
        if target.is_empty() || self.visible_rows == 0 {
            return;
        }
        let anchor = target.start();
        let visible = self.visible_range();
        if anchor < visible.start() || anchor >= visible.end() {
            self.top = anchor.min(self.max_top());
        }
    }

    /// Ensure the target's last row is visible, anchoring it at the bottom only
    /// when it lies outside the current viewport.
    pub fn reveal_end(&mut self, target: RowRange) {
        let target = target.clamped(self.content_rows);
        if target.is_empty() || self.visible_rows == 0 {
            return;
        }
        let anchor = target.end().saturating_sub(1);
        let visible = self.visible_range();
        if anchor < visible.start() {
            self.top = anchor.min(self.max_top());
        } else if anchor >= visible.end() {
            self.top = target
                .end()
                .saturating_sub(self.visible_rows)
                .min(self.max_top());
        }
    }

    pub const fn visible_range(&self) -> RowRange {
        let top = if self.top > self.max_top() {
            self.max_top()
        } else {
            self.top
        };
        let end = top.saturating_add(self.visible_rows);
        RowRange::new(
            top,
            if end > self.content_rows {
                self.content_rows
            } else {
                end
            },
        )
    }

    pub const fn overflows(&self) -> bool {
        self.content_rows > self.visible_rows
    }

    pub fn scrollbar_state(&self) -> Option<ScrollbarState> {
        if !self.overflows() || self.visible_rows == 0 {
            return None;
        }
        let max_top = self.max_top();
        let reachable_positions = max_top.saturating_add(1);
        // With `viewport_content_length` set, Ratatui maps endpoints correctly
        // when `content_length` is the number of valid viewport starts.
        Some(
            ScrollbarState::new(reachable_positions)
                .position(self.top.min(max_top))
                .viewport_content_length(self.visible_rows),
        )
    }

    /// Ratatui's paragraph offset remains `u16`; make the boundary conversion
    /// explicit and saturating rather than relying on a lossy cast.
    pub fn paragraph_offset(&self) -> u16 {
        u16::try_from(self.top).unwrap_or(u16::MAX)
    }
}

/// Draw the shared vertical scrollbar over the area's right edge without
/// reducing the content width used for wrapping.
pub fn render_scrollbar(frame: &mut Frame, area: Rect, viewport: &Viewport) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(mut state) = viewport.scrollbar_state() else {
        return;
    };
    let muted = Style::default().fg(theme().surfaces.border);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .thumb_style(Style::default().fg(theme().roles.tools))
        .track_symbol(Some("│"))
        .track_style(muted);
    let scrollbar = if area.height < 3 {
        scrollbar.begin_symbol(None).end_symbol(None)
    } else {
        scrollbar
            .begin_symbol(Some("↑"))
            .begin_style(muted)
            .end_symbol(Some("↓"))
            .end_style(muted)
    };
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

/// Checked, saturating conversion at a Ratatui widget boundary.
pub fn rows_to_u16(rows: usize) -> u16 {
    u16::try_from(rows).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn row_ranges_use_saturating_half_open_coordinates() {
        assert_eq!(RowRange::new(4, 2), RowRange::new(4, 4));
        assert_eq!(
            RowRange::from_start_len(usize::MAX - 1, 8),
            RowRange::new(usize::MAX - 1, usize::MAX)
        );
        assert_eq!(RowRange::new(3, 8).relative_to(5), RowRange::new(0, 3));
        assert_eq!(
            RowRange::new(usize::MAX - 1, usize::MAX).shifted(9),
            RowRange::new(usize::MAX, usize::MAX)
        );
    }

    #[test]
    fn empty_content_and_zero_visible_rows_are_safe() {
        let mut viewport = Viewport::default();
        viewport.set_top(10);
        viewport.reconcile(0, 0);
        assert_eq!(viewport.top(), 0);
        assert_eq!(viewport.visible_range(), RowRange::default());
        assert!(!viewport.overflows());
        assert_eq!(viewport.scrollbar_state(), None);

        viewport.reconcile(5, 0);
        assert_eq!(viewport.visible_range(), RowRange::new(0, 0));
        assert!(viewport.overflows());
        assert_eq!(viewport.scrollbar_state(), None);
    }

    #[test]
    fn invalidation_retires_page_measurements_but_preserves_scroll_intent() {
        let mut viewport = Viewport::default();
        viewport.reconcile(100, 20);
        viewport.set_top(40);
        viewport.invalidate_geometry();
        assert_eq!(viewport.visible_rows(), 0);
        assert_eq!(viewport.top(), 40);
        viewport.page_down(viewport.visible_rows().max(1));
        assert_eq!(viewport.top(), 41);
        assert_eq!(viewport.scrollbar_state(), None);
        viewport.reconcile(100, 10);
        assert_eq!(viewport.visible_range(), RowRange::new(41, 51));
    }

    #[test]
    fn pans_saturate_and_stay_clamped_to_reconciled_extents() {
        let mut viewport = Viewport::default();
        viewport.reconcile(usize::MAX, 0);
        viewport.pan_down(usize::MAX);
        viewport.pan_down(1);
        assert_eq!(viewport.top(), usize::MAX);
        viewport.pan_up(3);
        assert_eq!(viewport.top(), usize::MAX - 3);
        viewport.reconcile(20, 5);
        assert_eq!(viewport.top(), 15);
        viewport.page_up(6);
        assert_eq!(viewport.top(), 9);
        viewport.page_down(usize::MAX);
        assert_eq!(viewport.top(), 15);
    }

    #[test]
    fn start_and_end_jumps_use_reconciled_extents() {
        let mut viewport = Viewport::default();
        viewport.reconcile(12, 5);
        viewport.jump_end();
        assert_eq!(viewport.top(), 7);
        viewport.jump_start();
        assert_eq!(viewport.top(), 0);
    }

    #[test]
    fn normal_targets_are_fully_revealed() {
        let mut viewport = Viewport::default();
        viewport.reconcile(30, 6);
        viewport.reveal(RowRange::new(10, 13));
        assert_eq!(viewport.top(), 7);
        viewport.reveal(RowRange::new(4, 6));
        assert_eq!(viewport.top(), 4);
        assert_eq!(viewport.visible_range(), RowRange::new(4, 10));
    }

    #[test]
    fn oversized_targets_support_start_and_end_anchors() {
        let mut viewport = Viewport::default();
        viewport.reconcile(40, 5);
        let target = RowRange::new(12, 24);
        viewport.reveal_start(target);
        assert_eq!(viewport.top(), 12);
        viewport.reveal_end(target);
        assert_eq!(viewport.top(), 19);
        viewport.reveal(target);
        assert_eq!(viewport.top(), 12);
    }

    #[test]
    fn growth_shrink_and_resize_reconcile_the_visible_range() {
        let mut viewport = Viewport::default();
        viewport.reconcile(10, 4);
        viewport.jump_end();
        assert_eq!(viewport.visible_range(), RowRange::new(6, 10));

        viewport.reconcile(18, 4);
        assert_eq!(viewport.top(), 6, "growth does not imply follow policy");
        viewport.reconcile(5, 4);
        assert_eq!(viewport.visible_range(), RowRange::new(1, 5));
        viewport.reconcile(5, 8);
        assert_eq!(viewport.visible_range(), RowRange::new(0, 5));
    }

    #[test]
    fn overflow_and_scrollbar_projection_track_extents() {
        let mut viewport = Viewport::default();
        viewport.reconcile(4, 4);
        assert!(!viewport.overflows());
        assert_eq!(viewport.scrollbar_state(), None);

        viewport.reconcile(10, 4);
        assert!(viewport.overflows());
        let expected = |position| {
            Some(
                ScrollbarState::new(7)
                    .position(position)
                    .viewport_content_length(4),
            )
        };

        viewport.jump_start();
        assert_eq!(viewport.scrollbar_state(), expected(0));
        viewport.set_top(3);
        assert_eq!(viewport.scrollbar_state(), expected(3));
        viewport.jump_end();
        assert_eq!(viewport.scrollbar_state(), expected(6));
        viewport.set_top(usize::MAX);
        assert_eq!(viewport.scrollbar_state(), expected(6));
    }

    #[test]
    fn scrollbar_renderer_maps_proportional_thumb_to_both_track_endpoints() {
        fn rendered_column(viewport: &Viewport) -> String {
            const WIDTH: u16 = 2;
            const HEIGHT: u16 = 10;
            let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
            terminal
                .draw(|frame| render_scrollbar(frame, frame.area(), viewport))
                .expect("render scrollbar");
            let buffer = terminal.backend().buffer();
            (0..HEIGHT)
                .map(|y| buffer[(WIDTH - 1, y)].symbol())
                .collect()
        }

        let mut viewport = Viewport::default();
        viewport.reconcile(8, 4);

        viewport.jump_start();
        let at_start = rendered_column(&viewport);
        assert_eq!(at_start.chars().next(), Some('↑'));
        assert_eq!(at_start.chars().last(), Some('↓'));
        assert!(at_start.starts_with("↑█"));
        assert_eq!(
            at_start.matches('█').count(),
            4,
            "the half-height viewport should render a four-cell proportional thumb"
        );
        assert_eq!(at_start, "↑████││││↓");

        viewport.jump_end();
        let at_end = rendered_column(&viewport);
        assert_eq!(at_end.chars().next(), Some('↑'));
        assert_eq!(at_end.chars().last(), Some('↓'));
        assert_eq!(
            at_end, "↑││││████↓",
            "the final thumb cell should sit immediately above the end marker"
        );
    }

    #[test]
    fn one_and_two_row_scrollbar_bands_still_render_a_thumb() {
        for height in [1, 2] {
            let mut viewport = Viewport::default();
            viewport.reconcile(10, usize::from(height));
            let mut terminal = Terminal::new(TestBackend::new(2, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_scrollbar(frame, area, &viewport);
                })
                .expect("render tiny scrollbar");
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .any(|cell| cell.symbol() == "█"),
                "height {height} should retain a thumb"
            );
        }
    }
}
