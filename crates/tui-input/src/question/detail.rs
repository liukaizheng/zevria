//! Measurement, scrolling and chrome for the Markdown preview.

use super::{QuestionLayoutMode, WholeBody, measured_rows};
use crate::{text::truncate_display_width, theme::theme, viewport::Viewport};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use zevria_foundation::{QuestionPrompt, QuestionPromptKind};
use zevria_tui_widgets::markdown::markdown_lines;

#[derive(Default)]
pub(super) struct DetailPane {
    pub(super) viewport: Viewport,
    pub(super) scrollable: bool,
    owner: Option<usize>,
    cache: Option<DetailCache>,
}

struct DetailCache {
    width: u16,
    options: Vec<Option<DetailBody>>,
    placeholder: DetailBody,
    tallest: usize,
}

struct DetailBody {
    lines: Vec<Line<'static>>,
    rows: usize,
}

impl DetailBody {
    fn new(lines: Vec<Line<'static>>, width: u16) -> Self {
        let rows = measured_rows(&lines, width);
        Self { lines, rows }
    }
}

impl DetailPane {
    pub(super) fn set_owner(&mut self, cursor: usize) {
        if self.owner != Some(cursor) {
            self.owner = Some(cursor);
            self.viewport.jump_start();
            self.scrollable = false;
        }
    }

    pub(super) fn invalidate_geometry(&mut self) {
        self.viewport.invalidate_geometry();
        self.scrollable = false;
    }

    /// Cache the bodies at the active wrap width, rather than re-parsing them
    /// on focus moves or accumulating them for every intermediate resize width.
    pub(super) fn prepare(&mut self, prompt: &QuestionPrompt, cursor: usize, width: u16) -> usize {
        self.set_owner(cursor);
        if self.cache.as_ref().is_none_or(|cache| cache.width != width) {
            let options = prompt
                .options
                .iter()
                .map(|option| {
                    let detail = option.detail.as_deref()?;
                    let lines = markdown_lines(detail, Style::default(), usize::from(width));
                    (!lines.is_empty()).then(|| DetailBody::new(lines, width))
                })
                .collect::<Vec<_>>();
            let placeholder = DetailBody::new(
                vec![Line::styled(
                    "No additional detail.",
                    Style::default().fg(theme().text.muted),
                )],
                width,
            );
            let tallest = options
                .iter()
                .flatten()
                .map(|body| body.rows)
                .fold(placeholder.rows, usize::max);
            self.cache = Some(DetailCache {
                width,
                options,
                placeholder,
                tallest,
            });
        }
        self.cache.as_ref().expect("prepared detail cache").tallest
    }

    fn body(&self) -> &DetailBody {
        let cache = self.cache.as_ref().expect("prepared detail cache");
        self.owner
            .and_then(|owner| cache.options.get(owner)?.as_ref())
            .unwrap_or(&cache.placeholder)
    }

    pub(super) fn reconcile(&mut self, area: Option<Rect>) {
        let Some(area) = area.filter(|area| !area.is_empty()) else {
            // In whole-body mode, the question viewport scrolls both regions.
            self.invalidate_geometry();
            return;
        };
        self.viewport
            .reconcile(self.body().rows, usize::from(area.height));
        self.scrollable = self.viewport.overflows();
    }

    pub(super) fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(self.body().lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((self.viewport.paragraph_offset(), 0)),
            area,
        );
    }

    pub(super) fn append_to(&self, body: &mut WholeBody, label: &str, width: u16) {
        let rule = titled_rule(label, width);
        body.rows = body
            .rows
            .saturating_add(measured_rows(std::slice::from_ref(&rule), width))
            .saturating_add(self.body().rows);
        body.lines.push(rule);
        body.lines.extend(self.body().lines.iter().cloned());
    }
}

pub(super) struct DetailLayout {
    pub(super) list_width: u16,
    pub(super) detail_width: u16,
    pub(super) mode: QuestionLayoutMode,
}

impl DetailLayout {
    pub(super) fn new(width: u16) -> Self {
        if width >= 84 {
            let list_width = (width.saturating_mul(2) / 5).clamp(28, 44);
            Self {
                list_width,
                detail_width: width - list_width - 1,
                mode: QuestionLayoutMode::SideBySide,
            }
        } else {
            Self {
                list_width: width,
                detail_width: width,
                mode: QuestionLayoutMode::Stacked,
            }
        }
    }

    pub(super) fn fits(&self, body_rows: usize) -> bool {
        let minimum = if self.mode == QuestionLayoutMode::SideBySide {
            1
        } else {
            3
        };
        self.detail_width > 0 && body_rows >= minimum
    }

    pub(super) fn split(&self, body: Rect, answer_rows: usize) -> PaneAreas {
        if self.mode == QuestionLayoutMode::SideBySide {
            let list = Rect {
                width: self.list_width,
                ..body
            };
            let divider = Rect {
                x: list.right(),
                width: 1,
                ..body
            };
            let preview = Rect {
                x: divider.right(),
                width: self.detail_width,
                ..body
            };
            PaneAreas {
                list,
                divider,
                preview,
            }
        } else {
            let list_height = (body.height / 2)
                .min(super::rows_to_u16(answer_rows))
                .max(1);
            let list = Rect {
                height: list_height,
                ..body
            };
            let divider = Rect {
                y: list.bottom(),
                height: 1,
                ..body
            };
            let preview = Rect {
                y: divider.bottom(),
                height: body.height - list_height - 1,
                ..body
            };
            PaneAreas {
                list,
                divider,
                preview,
            }
        }
    }
}

pub(super) struct PaneAreas {
    pub(super) list: Rect,
    pub(super) divider: Rect,
    pub(super) preview: Rect,
}

impl PaneAreas {
    pub(super) fn render_divider(&self, frame: &mut Frame, mode: QuestionLayoutMode, label: &str) {
        if mode == QuestionLayoutMode::SideBySide {
            let style = Style::default().fg(theme().surfaces.border);
            for y in self.divider.top()..self.divider.bottom() {
                frame.buffer_mut()[(self.divider.x, y)]
                    .set_symbol("│")
                    .set_style(style);
            }
        } else {
            frame.render_widget(titled_rule(label, self.divider.width), self.divider);
        }
    }
}

pub(super) fn focused_label(prompt: &QuestionPrompt, cursor: usize) -> &str {
    if let Some(option) = prompt.options.get(cursor) {
        &option.label
    } else if cursor == prompt.options.len()
        && matches!(
            prompt.kind,
            QuestionPromptKind::SingleSelect { allow_other: true }
                | QuestionPromptKind::MultiSelect {
                    allow_other: true,
                    ..
                }
        )
    {
        "Other"
    } else {
        "Skip"
    }
}

fn titled_rule(label: &str, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let rule = Style::default().fg(theme().surfaces.border);
    if width < 6 {
        return Line::styled("─".repeat(width), rule);
    }
    let label = truncate_display_width(&label.replace(['\r', '\n'], " "), width - 5);
    let trailing = width.saturating_sub(4 + crate::text::display_width(&label));
    Line::from(vec![
        Span::styled("── ", rule),
        Span::styled(label, Style::default().fg(theme().text.muted)),
        Span::styled(format!(" {}", "─".repeat(trailing)), rule),
    ])
}
