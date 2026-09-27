//! Reusable direct chrome painters.

use ratatui::{buffer::Buffer, layout::Rect, style::Style, text::Line};

use crate::text::{display_width, truncate_display_width};
use crate::theme::theme;

pub const BLOCK_PAD_LEFT: u16 = 2;
pub const BLOCK_PAD_RIGHT: u16 = 2;
pub const CHROME_PAD_LEFT: u16 = 2;
pub const CHROME_PAD_RIGHT: u16 = 2;
pub const PROMPT_PREFIX: &str = "❯ ";
pub const PROMPT_PREFIX_WIDTH: u16 = 2;
pub const ACCENT_BAR: char = '┃';

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenRows {
    pub start: u16,
    pub end: u16,
}

impl ScreenRows {
    pub const fn new(start: u16, end: u16) -> Self {
        Self {
            start,
            end: if end < start { start } else { end },
        }
    }

    const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptInfo<'a> {
    pub left: &'a str,
    pub right: &'a str,
}

pub fn paint_accent(buffer: &mut Buffer, x: u16, visible_rows: ScreenRows, style: Style) {
    for y in visible_rows.start..visible_rows.end {
        paint_cell(buffer, x, y, ACCENT_BAR, style);
    }
}

/// Fill a compositing layer before its widgets are drawn.
pub fn paint_surface(buffer: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buffer.area);
    if area.width > 0 && area.height > 0 {
        buffer.set_style(area, style);
    }
}

/// Paint a single horizontal rule within the buffer bounds.
pub fn paint_rule(buffer: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buffer.area);
    if area.height == 0 {
        return;
    }
    for x in area.x..area.right() {
        paint_cell(buffer, x, area.y, '─', style);
    }
}

pub fn dim_style() -> Style {
    Style::new().fg(theme().text.muted)
}

pub fn secondary_style() -> Style {
    Style::new().fg(theme().text.secondary)
}

pub fn hint_key_style() -> Style {
    Style::new().fg(theme().text.primary)
}

pub fn hint_description_style() -> Style {
    secondary_style()
}

pub fn error_style() -> Style {
    Style::new().fg(theme().feedback.error)
}

pub fn overlay_style() -> Style {
    Style::new()
        .fg(theme().text.primary)
        .bg(theme().surfaces.overlay)
}

pub fn selection_style() -> Style {
    Style::new()
        .fg(theme().surfaces.selection_foreground)
        .bg(theme().surfaces.selection_background)
}

/// Apply selection to both line and nested span styles. This is done after
/// semantic styling so syntax, Markdown, diff, and role foregrounds cannot
/// leak through selected text; modifiers and symbols are left untouched.
pub fn style_selected_line(line: &mut Line<'static>) {
    let selected = selection_style();
    line.style = line.style.patch(selected);
    for span in &mut line.spans {
        span.style = span.style.patch(selected);
    }
}

pub fn paint_selection(
    buffer: &mut Buffer,
    selection_band: Rect,
    visible_rows: ScreenRows,
    style: Style,
) {
    let (Some(foreground), Some(background)) = (style.fg, style.bg) else {
        return;
    };
    let area = selection_band.intersection(buffer.area);
    let start = visible_rows.start.max(area.y);
    let end = visible_rows.end.min(area.bottom());
    if visible_rows.is_empty() || area.width == 0 || start >= end {
        return;
    }
    for y in start..end {
        for x in area.x..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_fg(foreground).set_bg(background);
            }
        }
    }
}

pub fn paint_rounded_prompt(
    buffer: &mut Buffer,
    area: Rect,
    style: Style,
    caption: &str,
    info_line: PromptInfo<'_>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let left = area.x;
    let right = area.right().saturating_sub(1);
    let top = area.y;
    let bottom = area.bottom().saturating_sub(1);

    paint_horizontal_border(buffer, left, right, top, '╭', '╮', style);
    if area.height > 1 {
        paint_horizontal_border(buffer, left, right, bottom, '╰', '╯', style);
        for y in top.saturating_add(1)..bottom {
            paint_cell(buffer, left, y, '│', style);
            if right > left {
                paint_cell(buffer, right, y, '│', style);
            }
        }
    }

    if right <= left.saturating_add(1) {
        return;
    }
    let interior_start = left.saturating_add(1);
    let interior_end = right;
    let interior_width = usize::from(interior_end.saturating_sub(interior_start));
    if let Some(label) = border_label(caption, interior_width) {
        let width = display_width(&label);
        let x = interior_end.saturating_sub(u16::try_from(width).unwrap_or(u16::MAX));
        paint_string(buffer, x, top, &label, style);
    }
    if area.height < 2 {
        return;
    }

    let right_label = border_label(info_line.right, interior_width);
    let right_width = right_label.as_deref().map_or(0, display_width);
    if let Some(label) = right_label.as_deref() {
        let x = interior_end.saturating_sub(u16::try_from(right_width).unwrap_or(u16::MAX));
        paint_string(buffer, x, bottom, label, style);
    }
    let left_budget = interior_width
        .saturating_sub(right_width)
        .saturating_sub(usize::from(right_width > 0));
    if let Some(label) = border_label(info_line.left, left_budget) {
        paint_string(buffer, interior_start, bottom, &label, style);
    }
}

fn border_label(value: &str, max_width: usize) -> Option<String> {
    if value.is_empty() || max_width < 3 {
        return None;
    }
    let value = truncate_display_width(value, max_width.saturating_sub(2));
    (!value.is_empty()).then(|| format!(" {value} "))
}

fn paint_horizontal_border(
    buffer: &mut Buffer,
    left: u16,
    right: u16,
    y: u16,
    left_corner: char,
    right_corner: char,
    style: Style,
) {
    paint_cell(buffer, left, y, left_corner, style);
    if right <= left {
        return;
    }
    for x in left.saturating_add(1)..right {
        paint_cell(buffer, x, y, '─', style);
    }
    paint_cell(buffer, right, y, right_corner, style);
}

fn paint_cell(buffer: &mut Buffer, x: u16, y: u16, symbol: char, style: Style) {
    if let Some(cell) = buffer.cell_mut((x, y)) {
        cell.set_char(symbol).set_style(style);
    }
}

fn paint_string(buffer: &mut Buffer, x: u16, y: u16, value: &str, style: Style) {
    if value.is_empty() || buffer.cell((x, y)).is_none() {
        return;
    }
    let available = usize::from(buffer.area.right().saturating_sub(x));
    let value = truncate_display_width(value, available);
    buffer.set_stringn(x, y, value, available, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };

    #[test]
    fn rounded_prompt_truncates_unicode_labels_safely() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let area = buffer.area;
        paint_rounded_prompt(
            &mut buffer,
            area,
            Style::default().fg(Color::Blue),
            "界面模式中",
            PromptInfo {
                left: "e\u{301}cho flag",
                right: "multiline",
            },
        );
        let top = (0..12).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        let bottom = (0..12).map(|x| buffer[(x, 2)].symbol()).collect::<String>();
        assert_eq!(buffer.area.width, 12);
        assert!(top.contains('…'));
        assert!(bottom.contains('…'));
    }

    #[test]
    fn one_and_two_row_prompts_degrade_to_valid_rounded_borders() {
        let mut one = Buffer::empty(Rect::new(0, 0, 5, 1));
        let area = one.area;
        paint_rounded_prompt(
            &mut one,
            area,
            Style::default(),
            "Build",
            PromptInfo {
                left: "",
                right: "",
            },
        );
        assert_eq!(one[(0, 0)].symbol(), "╭");
        assert_eq!(one[(4, 0)].symbol(), "╮");

        let mut two = Buffer::empty(Rect::new(0, 0, 5, 2));
        let area = two.area;
        paint_rounded_prompt(
            &mut two,
            area,
            Style::default(),
            "Build",
            PromptInfo {
                left: "",
                right: "",
            },
        );
        assert_eq!(two[(0, 0)].symbol(), "╭");
        assert_eq!(two[(4, 0)].symbol(), "╮");
        assert_eq!(two[(0, 1)].symbol(), "╰");
        assert_eq!(two[(4, 1)].symbol(), "╯");
    }

    #[test]
    fn selection_painter_overrides_colors_but_preserves_symbols_and_modifiers() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 5));
        buffer.set_string(0, 2, "abcdefgh", Style::default());
        buffer.set_string(0, 3, "ijklmnop", Style::default());
        buffer
            .cell_mut((3, 2))
            .expect("styled cell")
            .set_char('X')
            .set_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            );
        let symbols_before = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect::<Vec<_>>();

        paint_selection(
            &mut buffer,
            Rect::new(1, 1, 5, 3),
            ScreenRows::new(2, 4),
            selection_style(),
        );

        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let selected = (1..6).contains(&x) && (2..4).contains(&y);
                assert_eq!(
                    buffer[(x, y)].bg == theme().surfaces.selection_background,
                    selected
                );
            }
        }
        let symbols_after = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect::<Vec<_>>();
        assert_eq!(symbols_after, symbols_before);
        assert_eq!(buffer[(3, 2)].fg, theme().surfaces.selection_foreground);
        assert!(buffer[(3, 2)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(3, 2)].modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn rule_painter_clamps_to_the_buffer_and_touches_only_one_row() {
        let bounds = Rect::new(3, 2, 5, 3);
        let style = Style::new()
            .fg(theme().surfaces.border)
            .bg(theme().surfaces.canvas);
        for area in [
            Rect::new(4, 3, 2, 2),
            Rect::new(1, 1, 10, 5),
            Rect::new(4, 3, 0, 1),
            Rect::new(4, 3, 2, 0),
            Rect::new(10, 8, 2, 1),
        ] {
            let mut buffer = Buffer::empty(bounds);
            let painted = area.intersection(bounds);
            paint_rule(&mut buffer, area, style);
            for y in bounds.y..bounds.bottom() {
                for x in bounds.x..bounds.right() {
                    let cell = &buffer[(x, y)];
                    if painted.height > 0
                        && y == painted.y
                        && (painted.x..painted.right()).contains(&x)
                    {
                        assert_eq!(cell.symbol(), "─");
                        assert_eq!(cell.fg, theme().surfaces.border);
                        assert_eq!(cell.bg, theme().surfaces.canvas);
                    } else {
                        assert_eq!(cell, &ratatui::buffer::Cell::default());
                    }
                }
            }
        }
    }

    #[test]
    fn accent_painter_leaves_separator_rows_untouched() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 5));
        paint_accent(
            &mut buffer,
            1,
            ScreenRows::new(1, 3),
            Style::default().fg(Color::Green),
        );
        assert_eq!(buffer[(1, 1)].symbol(), "┃");
        assert_eq!(buffer[(1, 2)].symbol(), "┃");
        assert_eq!(buffer[(1, 3)].symbol(), " ");
    }
}
