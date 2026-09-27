//! Shared modal chrome and geometry-aware list/inspect navigation.
use crate::{
    chrome::{self, ScreenRows},
    theme::theme,
    viewport::{RowRange, Viewport, rows_to_u16},
};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, BorderType, Clear, Padding},
};

pub fn modal_block(title: impl Into<Line<'static>>, footer: Line<'static>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::new(
            chrome::CHROME_PAD_LEFT.saturating_sub(1),
            chrome::CHROME_PAD_RIGHT.saturating_sub(1),
            0,
            0,
        ))
        .style(chrome::overlay_style())
        .border_style(ratatui::style::Style::new().fg(theme().surfaces.border_strong))
        .title_top(title)
        .title_bottom(footer)
}

pub fn modal(
    frame: &mut Frame,
    area: Rect,
    title: impl Into<Line<'static>>,
    footer: Line<'static>,
) -> Rect {
    let block = modal_block(title, footer);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    chrome::paint_surface(frame.buffer_mut(), area, chrome::overlay_style());
    frame.render_widget(block, area);
    inner
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListAction {
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Close,
    Confirm,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListOutcome {
    Handled,
    Close,
    Confirm,
}

#[derive(Debug, Default)]
pub struct ListNav {
    pub selected: usize,
    pub viewport: Viewport,
    rows: Vec<RowRange>,
    reveal: bool,
}
impl ListNav {
    pub fn invalidate_geometry(&mut self) {
        self.viewport.invalidate_geometry();
        self.rows.clear();
    }
    pub fn request_reveal(&mut self) {
        self.reveal = true;
    }
    pub fn reconcile(&mut self, count: usize, height: usize) {
        self.selected = self.selected.min(count.saturating_sub(1));
        self.viewport.reconcile(count, height);
        self.viewport.reveal(RowRange::from_start_len(
            self.selected,
            usize::from(count > 0),
        ));
    }
    /// Wrapped list rows (skills) navigate in physical rows, not item counts.
    pub fn reconcile_rows(&mut self, rows: Vec<RowRange>, total: usize, height: usize) {
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.viewport.reconcile(total, height);
        if std::mem::take(&mut self.reveal)
            && let Some(row) = self.rows.get(self.selected)
        {
            self.viewport.reveal_start(*row);
        }
    }
    pub fn handle(&mut self, action: ListAction, count: usize) -> ListOutcome {
        use ListAction::*;
        let last = count.saturating_sub(1);
        let page = self.viewport.visible_rows().max(1);
        let step = if matches!(action, HalfPageUp | HalfPageDown) {
            (page / 2).max(1)
        } else {
            page
        };
        let down = matches!(action, PageDown | HalfPageDown);
        self.selected = match action {
            Close => return ListOutcome::Close,
            Confirm => return ListOutcome::Confirm,
            Up => self.selected.saturating_sub(1),
            Down => self.selected.saturating_add(1).min(last),
            Home => 0,
            End => last,
            PageUp | PageDown | HalfPageUp | HalfPageDown => {
                if let Some(current) = self.rows.get(self.selected) {
                    let target = if down {
                        current.start().saturating_add(step)
                    } else {
                        current.start().saturating_sub(step)
                    };
                    if down {
                        self.rows
                            .iter()
                            .position(|r| r.start() >= target)
                            .unwrap_or(last)
                    } else {
                        self.rows
                            .iter()
                            .rposition(|r| r.start() <= target)
                            .unwrap_or(0)
                    }
                } else if down {
                    self.selected.saturating_add(step).min(last)
                } else {
                    self.selected.saturating_sub(step)
                }
            }
        }
        .min(last);
        self.reveal = true;
        ListOutcome::Handled
    }
    /// Inspect mode pans without changing a selected entity or triggering reveal.
    pub fn pan(&mut self, action: ListAction) -> ListOutcome {
        use ListAction::*;
        let page = self.viewport.visible_rows().max(1);
        match action {
            Close => return ListOutcome::Close,
            Confirm => return ListOutcome::Confirm,
            Up => self.viewport.pan_up(1),
            Down => self.viewport.pan_down(1),
            PageUp => self.viewport.page_up(page),
            PageDown => self.viewport.page_down(page),
            HalfPageUp => self.viewport.page_up((page / 2).max(1)),
            HalfPageDown => self.viewport.page_down((page / 2).max(1)),
            Home => self.viewport.jump_start(),
            End => self.viewport.jump_end(),
        }
        self.reveal = false;
        ListOutcome::Handled
    }
    pub fn style_line(&self, index: usize, line: &mut Line<'static>) {
        if index == self.selected {
            chrome::style_selected_line(line);
        }
    }
    pub fn paint_selected(&self, frame: &mut Frame, area: Rect) {
        let visible = self.viewport.visible_range();
        let row = self
            .rows
            .get(self.selected)
            .copied()
            .unwrap_or(RowRange::from_start_len(self.selected, 1));
        if row.intersects(visible) {
            let start = area
                .y
                .saturating_add(rows_to_u16(row.start().saturating_sub(visible.start())));
            let end = area.y.saturating_add(rows_to_u16(
                row.end().min(visible.end()).saturating_sub(visible.start()),
            ));
            chrome::paint_selection(
                frame.buffer_mut(),
                area,
                ScreenRows::new(start, end),
                chrome::selection_style(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navigation_clamps_pages_and_empty_lists() {
        let mut nav = ListNav::default();
        nav.reconcile(50, 10);
        for (action, selected) in [
            (ListAction::Down, 1),
            (ListAction::PageDown, 11),
            (ListAction::HalfPageDown, 16),
            (ListAction::HalfPageUp, 11),
            (ListAction::PageUp, 1),
            (ListAction::Up, 0),
            (ListAction::End, 49),
            (ListAction::Home, 0),
        ] {
            assert_eq!(nav.handle(action, 50), ListOutcome::Handled);
            assert_eq!(nav.selected, selected);
        }
        assert_eq!(nav.handle(ListAction::Close, 0), ListOutcome::Close);
        assert_eq!(nav.handle(ListAction::Confirm, 0), ListOutcome::Confirm);
        nav.handle(ListAction::End, 0);
        nav.handle(ListAction::Down, 0);
        assert_eq!(nav.selected, 0);
    }
    #[test]
    fn wrapped_pages_use_measured_rows_and_inspect_does_not_reveal() {
        let mut nav = ListNav::default();
        nav.reconcile_rows(
            vec![
                RowRange::new(3, 6),
                RowRange::new(6, 10),
                RowRange::new(10, 16),
            ],
            25,
            6,
        );
        nav.handle(ListAction::PageDown, 3);
        assert_eq!(nav.selected, 2);
        nav.pan(ListAction::End);
        assert_eq!(nav.viewport.top(), 19);
        nav.reconcile_rows(
            vec![
                RowRange::new(3, 6),
                RowRange::new(6, 10),
                RowRange::new(10, 16),
            ],
            25,
            6,
        );
        assert_eq!(nav.viewport.top(), 19);
    }
}
