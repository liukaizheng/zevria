//! Render-only caches, shared viewports, follow intent, and composer measurements.

use ratatui::layout::Rect;

use crate::layout::ConversationCache;
use crate::viewport::{RowRange, Viewport};

pub(crate) struct ViewState {
    conversation: ConversationCache,
    conversation_viewport: Viewport,
    rendered_selection_window: Option<RowRange>,
    allocation_measured: bool,
    follow: bool,
    repin_on_bottom: bool,
    pending_anchor: Option<(usize, crate::presentation::PresentationBlockId, usize)>,
    composer_width: u16,
    composer_viewport: Viewport,
    completion_viewport: Viewport,
    plan_viewport: Viewport,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            conversation: ConversationCache::default(),
            conversation_viewport: Viewport::default(),
            rendered_selection_window: None,
            allocation_measured: false,
            follow: true,
            repin_on_bottom: false,
            pending_anchor: None,
            composer_width: 0,
            composer_viewport: Viewport::default(),
            completion_viewport: Viewport::default(),
            plan_viewport: Viewport::default(),
        }
    }
}

impl ViewState {
    pub(crate) fn capture_conversation_anchor(&mut self) {
        if !self.follow && self.pending_anchor.is_none() {
            self.pending_anchor = self
                .conversation
                .semantic_anchor(self.conversation_viewport.top());
        }
    }

    pub(crate) fn reconcile_anchor_identity(
        &mut self,
        conversation: &super::conversation::ConversationState,
    ) {
        if let Some((history, id, within)) = self.pending_anchor {
            let (history, id) = conversation.resolved_identity((history, id));
            self.pending_anchor = Some((history, id, within));
        }
    }

    pub(crate) fn conversation_cache(&self) -> &ConversationCache {
        &self.conversation
    }

    pub(crate) fn conversation_cache_mut(&mut self) -> &mut ConversationCache {
        &mut self.conversation
    }

    pub(crate) fn invalidate_from(&mut self, index: usize) {
        self.invalidate_rendered_geometry();
        self.conversation.invalidate_from(index);
    }

    /// Input may only query geometry established by a usable render of this
    /// pane. Invalidating the measurement need not discard reusable layouts.
    pub(crate) fn invalidate_rendered_geometry(&mut self) {
        self.allocation_measured = false;
        self.invalidate_selection_window();
    }

    fn invalidate_selection_window(&mut self) {
        self.rendered_selection_window = None;
    }

    fn page_rows(&self) -> usize {
        if self.allocation_measured {
            self.conversation_viewport.visible_rows().max(1)
        } else {
            1
        }
    }

    pub(crate) const fn rendered_selection_window(&self) -> Option<RowRange> {
        self.rendered_selection_window
    }

    pub(crate) fn record_rendered_selection_window(&mut self, content: Rect) {
        let window = self.conversation_viewport.visible_range();
        self.allocation_measured = content.width > 0 && content.height > 0;
        self.rendered_selection_window =
            (content.width > 0 && content.height > 0 && !window.is_empty()).then_some(window);
    }

    #[cfg(test)]
    pub(crate) const fn scroll(&self) -> usize {
        self.conversation_viewport.top()
    }

    pub(crate) const fn conversation_viewport(&self) -> &Viewport {
        &self.conversation_viewport
    }

    #[cfg(test)]
    pub(crate) const fn follow(&self) -> bool {
        self.follow
    }

    pub(crate) fn set_follow(&mut self, follow: bool) {
        self.follow = follow;
    }

    pub(crate) fn jump_top(&mut self) {
        self.invalidate_selection_window();
        self.follow = false;
        self.repin_on_bottom = false;
        self.pending_anchor = None;
        self.conversation_viewport.jump_start();
    }

    pub(crate) fn jump_bottom(&mut self) {
        self.invalidate_selection_window();
        self.follow = true;
        self.repin_on_bottom = false;
        self.pending_anchor = None;
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        self.invalidate_selection_window();
        self.follow = false;
        self.repin_on_bottom = false;
        self.pending_anchor = None;
        self.conversation_viewport.pan_up(amount);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.invalidate_selection_window();
        self.follow = false;
        self.repin_on_bottom = true;
        self.pending_anchor = None;
        self.conversation_viewport
            .set_top(self.conversation_viewport.top().saturating_add(amount));
    }

    // Page distances use the last reconciled pane height, even when navigation
    // has invalidated the selection window or content is shorter than the pane.
    pub(crate) fn page_up(&mut self) {
        self.scroll_up(self.page_rows());
    }

    pub(crate) fn page_down(&mut self) {
        self.scroll_down(self.page_rows());
    }

    pub(crate) fn half_page_up(&mut self) {
        self.scroll_up((self.page_rows() / 2).max(1));
    }

    pub(crate) fn half_page_down(&mut self) {
        self.scroll_down((self.page_rows() / 2).max(1));
    }

    /// Every key supersedes any downward intent that has not reached a render
    /// boundary yet. A new downward key may arm it again afterward.
    pub(crate) fn clear_pending_repin(&mut self) {
        self.repin_on_bottom = false;
    }

    pub(crate) fn reset_conversation(&mut self) {
        self.invalidate_rendered_geometry();
        self.conversation_viewport = Viewport::default();
        self.follow = true;
        self.repin_on_bottom = false;
        self.pending_anchor = None;
        self.conversation.invalidate_from(0);
    }

    /// Reconcile the conversation viewport while preserving the
    /// rendered-bottom re-pin rule and selected-item visibility behavior.
    pub(crate) fn reconcile_conversation_viewport(
        &mut self,
        total_rows: usize,
        visible_rows: usize,
        selection: Option<RowRange>,
        selecting: bool,
    ) {
        self.allocation_measured = visible_rows > 0;
        let attempted_top = self.conversation_viewport.top();
        let repin_on_bottom = std::mem::take(&mut self.repin_on_bottom);
        self.conversation_viewport
            .reconcile(total_rows, visible_rows);
        if !self.follow
            && let Some(anchor) = self.pending_anchor.take()
            && let Some(row) = self.conversation.anchor_row(anchor)
        {
            // The anchor is bounded within its entry, but folding or resizing
            // can put that row beyond the newly reconciled viewport's last start.
            self.conversation_viewport
                .set_top(row.min(self.conversation_viewport.max_top()));
        }
        if repin_on_bottom && !selecting && attempted_top >= self.conversation_viewport.max_top() {
            self.follow = true;
        }
        if self.follow {
            self.conversation_viewport.jump_end();
        }
        if let Some(selection) = selection {
            if selection.len() <= visible_rows {
                self.conversation_viewport.reveal(selection);
            } else if selection.start() < self.conversation_viewport.top() {
                // Preserve the established transcript directionality for an
                // item taller than the pane: moving upward reveals its start.
                self.conversation_viewport.reveal_start(selection);
            } else {
                // Moving downward reveals the oversized item's trailing rows.
                self.conversation_viewport.reveal_end(selection);
            }
        }
        debug_assert!(self.conversation_viewport.top() <= self.conversation_viewport.max_top());
    }

    pub(crate) const fn composer_width(&self) -> u16 {
        self.composer_width
    }

    pub(crate) fn set_composer_width(&mut self, width: u16) {
        self.composer_width = width;
    }

    #[cfg(test)]
    pub(crate) const fn composer_scroll(&self) -> usize {
        self.composer_viewport.top()
    }

    pub(crate) const fn composer_viewport(&self) -> &Viewport {
        &self.composer_viewport
    }

    pub(crate) fn set_composer_scroll(&mut self, scroll: usize) {
        self.composer_viewport.set_top(scroll);
    }

    pub(crate) fn reconcile_composer_viewport(
        &mut self,
        total_rows: usize,
        visible_rows: usize,
        caret_row: usize,
    ) {
        self.composer_viewport.reconcile(total_rows, visible_rows);
        self.composer_viewport
            .reveal(RowRange::from_start_len(caret_row, 1));
    }

    pub(crate) fn completion_page_rows(&self) -> usize {
        if self.allocation_measured {
            self.completion_viewport.visible_rows().max(1)
        } else {
            1
        }
    }

    pub(crate) fn composer_page_rows(&self) -> usize {
        self.composer_viewport.visible_rows().max(1)
    }

    pub(crate) const fn completion_viewport_mut(&mut self) -> &mut Viewport {
        &mut self.completion_viewport
    }

    pub(crate) const fn plan_viewport_mut(&mut self) -> &mut Viewport {
        &mut self.plan_viewport
    }

    #[cfg(test)]
    pub(crate) fn set_scroll_for_test(&mut self, scroll: usize, follow: bool) {
        self.invalidate_selection_window();
        self.conversation_viewport.set_top(scroll);
        self.follow = follow;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{FoldKey, FoldState, HistoryEntry, ToolCallStatus};
    use rig_core::message::Message;

    fn cached_view() -> (ViewState, Vec<HistoryEntry>) {
        let history = (0..3)
            .map(|_| {
                HistoryEntry::from_message(
                    Message::user("body row\n".repeat(30)),
                    ToolCallStatus::Finished,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let mut view = ViewState::default();
        view.conversation
            .refresh(&history, None, 80, false, &FoldState::default());
        (view, history)
    }

    fn cached_rows(view: &ViewState) -> usize {
        view.conversation
            .entries()
            .iter()
            .map(|entry| entry.extent())
            .sum()
    }

    fn fold_cached_history(view: &mut ViewState, history: &[HistoryEntry]) -> usize {
        let mut folds = FoldState::default();
        for history_index in 0..history.len() {
            folds.fold(FoldKey::Message { history_index });
        }
        view.conversation.refresh(history, None, 80, false, &folds);
        cached_rows(view)
    }

    #[test]
    fn folded_anchor_restoration_respects_new_viewport_bounds() {
        // Fits entirely, overflows with an anchor past the bottom, preserves an
        // in-range anchor, and permits a zero-height viewport, respectively.
        for (visible, clamped) in [(20, true), (5, true), (1, false), (0, false)] {
            let (mut view, history) = cached_view();
            let top = view.conversation.entries()[..2]
                .iter()
                .map(|entry| entry.extent())
                .sum::<usize>()
                + 8;
            view.set_scroll_for_test(top, false);
            view.reconcile_conversation_viewport(cached_rows(&view), visible, None, false);
            assert_eq!(view.scroll(), top);
            view.capture_conversation_anchor();
            let anchor = view.pending_anchor.expect("top is inside the last message");

            let total = fold_cached_history(&mut view, &history);
            let row = view.conversation.anchor_row(anchor).unwrap();
            let max_top = total.saturating_sub(visible);
            assert_eq!(row > max_top, clamped);
            assert_eq!(max_top == 0, visible == 20);
            view.reconcile_conversation_viewport(total, visible, None, false);
            assert_eq!(view.scroll(), row.min(max_top));
            assert_eq!(
                view.scroll(),
                view.conversation_viewport.visible_range().start()
            );
            assert_eq!(view.pending_anchor, None);
            assert!(!view.follow(), "geometry clamping is not downward intent");
        }
    }

    #[test]
    fn unresolved_anchors_and_inter_entry_gaps_use_row_clamping() {
        for remove_anchor in [false, true] {
            let (mut view, mut history) = cached_view();
            let top = if remove_anchor {
                view.conversation.entries()[..2]
                    .iter()
                    .map(|entry| entry.extent())
                    .sum::<usize>()
                    + 8
            } else {
                view.conversation.entries()[0].height
            };
            view.set_scroll_for_test(top, false);
            view.reconcile_conversation_viewport(cached_rows(&view), 5, None, false);
            view.capture_conversation_anchor();
            assert_eq!(
                view.pending_anchor.is_some(),
                remove_anchor,
                "gaps have no anchor"
            );
            if remove_anchor {
                history.pop();
            }
            let total = fold_cached_history(&mut view, &history);
            if let Some(anchor) = view.pending_anchor {
                assert_eq!(view.conversation.anchor_row(anchor), None);
            }
            view.reconcile_conversation_viewport(total, 5, None, false);
            assert_eq!(view.scroll(), top.min(total.saturating_sub(5)));
            assert_eq!(view.pending_anchor, None);
            assert!(!view.follow());
        }
    }

    #[test]
    fn empty_content_clamps_pending_anchors_even_at_zero_height() {
        for visible in [0, 1, 20] {
            let (mut view, _) = cached_view();
            view.set_scroll_for_test(8, false);
            view.reconcile_conversation_viewport(cached_rows(&view), visible, None, false);
            view.capture_conversation_anchor();
            assert!(view.pending_anchor.is_some());
            view.conversation
                .refresh(&[], None, 80, false, &FoldState::default());
            view.reconcile_conversation_viewport(0, visible, None, false);
            assert_eq!(view.scroll(), 0);
            assert_eq!(view.conversation_viewport.max_top(), 0);
            assert!(view.conversation_viewport.visible_range().is_empty());
            assert_eq!(view.pending_anchor, None);
            assert!(!view.follow());
        }
    }

    #[test]
    fn follow_and_selection_reveal_take_precedence_over_restored_anchors() {
        for (last_entry, follow, selection, expected) in [
            (false, true, None, 6),
            (false, true, Some(RowRange::new(0, 2)), 0),
            (true, false, Some(RowRange::new(0, 2)), 0),
            (false, false, Some(RowRange::new(4, 6)), 3),
            // Oversized selections still reveal the start when moving up and
            // the end when moving down, after bounding the restored anchor.
            (true, false, Some(RowRange::new(0, 5)), 0),
            (false, false, Some(RowRange::new(3, 9)), 6),
        ] {
            let (mut view, history) = cached_view();
            let top = if last_entry {
                view.conversation.entries()[..2]
                    .iter()
                    .map(|entry| entry.extent())
                    .sum::<usize>()
                    + 8
            } else {
                8
            };
            view.set_scroll_for_test(top, false);
            view.reconcile_conversation_viewport(cached_rows(&view), 3, None, false);
            view.capture_conversation_anchor();
            assert!(view.pending_anchor.is_some());
            let total = fold_cached_history(&mut view, &history);
            assert_eq!(total, 9);
            view.set_follow(follow);
            view.reconcile_conversation_viewport(total, 3, selection, selection.is_some());
            assert_eq!(view.scroll(), expected);
            assert_eq!(view.follow(), follow);
        }
    }

    #[test]
    fn downward_repin_uses_attempted_top_before_anchor_restoration() {
        for selecting in [false, true] {
            let (mut view, history) = cached_view();
            view.reconcile_conversation_viewport(cached_rows(&view), 5, None, false);
            view.set_scroll_for_test(6, false);
            view.scroll_down(1);
            let attempted_top = view.scroll();
            view.capture_conversation_anchor();
            let anchor = view.pending_anchor.unwrap();
            let total = fold_cached_history(&mut view, &history);
            let restored = view.conversation.anchor_row(anchor).unwrap();
            let max_top = total.saturating_sub(5);
            assert!(restored < max_top && attempted_top >= max_top);
            view.reconcile_conversation_viewport(total, 5, None, selecting);
            assert_eq!(view.follow(), !selecting);
            assert_eq!(view.scroll(), if selecting { restored } else { max_top });
            assert!(!view.repin_on_bottom);
        }
    }

    #[test]
    fn page_distances_use_reconciled_pane_height_even_without_selection_geometry() {
        for visible in [0, 1, 2, 3, 9, 12] {
            let mut view = ViewState::default();
            view.reconcile_conversation_viewport(200, visible, None, false);
            view.set_scroll_for_test(100, false);
            view.record_rendered_selection_window(Rect::new(0, 0, 80, visible as u16));
            let full = visible.max(1);
            let half = (visible / 2).max(1);
            view.page_up();
            assert_eq!(view.scroll(), 100 - full);
            assert_eq!(view.rendered_selection_window(), None);
            view.half_page_up();
            view.half_page_up();
            assert_eq!(view.scroll(), 100 - full - 2 * half);
            view.page_down();
            view.half_page_down();
            view.half_page_down();
            assert_eq!(view.scroll(), 100);
            assert!(!view.follow(), "only rendering may re-pin");
        }

        for total in [0, 2] {
            let mut view = ViewState::default();
            view.reconcile_conversation_viewport(total, 20, None, false);
            view.record_rendered_selection_window(Rect::new(0, 0, 80, 20));
            view.page_down();
            assert_eq!(view.scroll(), 20, "page height is not content height");
            view.reconcile_conversation_viewport(total, 20, None, false);
            assert_eq!(view.scroll(), 0);
            assert!(view.follow());
        }
    }

    #[test]
    fn unmeasured_pages_have_a_one_row_minimum_and_saturate() {
        let mut view = ViewState::default();
        view.page_down();
        view.half_page_down();
        assert_eq!(view.scroll(), 2);
        view.page_up();
        view.half_page_up();
        view.half_page_up();
        assert_eq!(view.scroll(), 0);
        assert!(!view.follow());

        view.set_scroll_for_test(usize::MAX, false);
        view.page_down();
        view.half_page_down();
        assert_eq!(view.scroll(), usize::MAX);
        view.reconcile_conversation_viewport(0, 0, None, false);
        assert_eq!(view.scroll(), 0);
        assert!(view.follow());
    }

    #[test]
    fn resized_pages_repin_against_newly_rendered_content_not_stale_extents() {
        let mut view = ViewState::default();
        view.reconcile_conversation_viewport(100, 9, None, false);
        view.half_page_up();
        assert_eq!(view.scroll(), 87);
        view.half_page_down();
        assert_eq!(view.scroll(), 91);
        // Streaming grew before the next frame: reaching the old bottom is
        // insufficient, even with a simultaneous resize.
        view.reconcile_conversation_viewport(120, 7, None, false);
        assert_eq!(view.scroll(), 91);
        assert!(!view.follow());
        view.half_page_down();
        assert_eq!(view.scroll(), 94);
        view.page_down();
        view.page_down();
        view.page_down();
        assert_eq!(view.scroll(), 115);
        assert!(!view.follow());
        view.reconcile_conversation_viewport(120, 7, None, false);
        assert_eq!(view.scroll(), 113);
        assert!(view.follow());
        view.reconcile_conversation_viewport(130, 7, None, false);
        assert_eq!(view.scroll(), 123);
    }

    #[test]
    fn downward_scroll_repins_only_at_rendered_bottom() {
        let mut view = ViewState::default();
        view.set_scroll_for_test(7, false);
        view.scroll_down(1);
        view.reconcile_conversation_viewport(12, 4, None, false);
        assert!(view.follow());
        assert_eq!(view.scroll(), 8);

        view.set_scroll_for_test(2, false);
        view.scroll_down(1);
        view.reconcile_conversation_viewport(12, 4, None, false);
        assert!(!view.follow());
        assert_eq!(view.scroll(), 3);
    }

    #[test]
    fn rendered_measurements_require_visible_content_and_are_invalidated_by_view_changes() {
        let mut view = ViewState::default();
        assert_eq!(view.rendered_selection_window(), None);
        view.reconcile_conversation_viewport(20, 5, None, false);
        view.record_rendered_selection_window(Rect::new(2, 1, 0, 5));
        assert_eq!(view.rendered_selection_window(), None);
        view.record_rendered_selection_window(Rect::new(2, 1, 80, 0));
        assert_eq!(view.rendered_selection_window(), None);
        let content = Rect::new(2, 1, 80, 5);
        view.record_rendered_selection_window(content);
        assert_eq!(
            view.rendered_selection_window(),
            Some(RowRange::new(15, 20))
        );
        for change in 0..6 {
            view.record_rendered_selection_window(content);
            match change {
                0 => view.scroll_up(1),
                1 => view.scroll_down(1),
                2 => view.jump_top(),
                3 => view.jump_bottom(),
                4 => view.invalidate_from(0),
                5 => view.reset_conversation(),
                _ => unreachable!(),
            }
            assert_eq!(view.rendered_selection_window(), None);
        }
    }

    #[test]
    fn suppressed_reveal_keeps_selection_separate_from_repin_and_geometry_clamping() {
        let mut view = ViewState::default();
        view.set_scroll_for_test(20, false);
        view.scroll_down(1);
        view.reconcile_conversation_viewport(12, 4, None, true);
        assert_eq!(view.scroll(), 8, "ordinary content shrinkage still clamps");
        assert!(
            !view.follow(),
            "selecting must inhibit the downward re-pin rule"
        );
    }

    #[test]
    fn selection_is_kept_visible() {
        let mut view = ViewState::default();
        view.set_scroll_for_test(0, false);
        view.reconcile_conversation_viewport(20, 5, Some(RowRange::new(10, 12)), true);
        assert_eq!(view.scroll(), 7);
    }

    #[test]
    fn oversized_selection_preserves_navigation_direction() {
        let mut view = ViewState::default();
        view.set_scroll_for_test(15, false);
        view.reconcile_conversation_viewport(30, 5, Some(RowRange::new(5, 15)), true);
        assert_eq!(view.scroll(), 5, "upward movement reveals the start");

        view.set_scroll_for_test(0, false);
        view.reconcile_conversation_viewport(30, 5, Some(RowRange::new(10, 20)), true);
        assert_eq!(view.scroll(), 15, "downward movement reveals the end");
    }
}
