//! Semantic pane status views and width-safe one-row rendering.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use zevria_foundation::ModelProfileRef;
use zevria_model::ContextTokenSnapshot;
use zevria_model::TokenUsage;

use crate::text::{display_width, truncate_display_width};
use crate::theme::theme;

const SEPARATOR: &str = " · ";

/// Provider-neutral context accounting supplied by an external ACP worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExternalContextUsage {
    pub(crate) used: u64,
    pub(crate) size: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StatusAccent {
    #[default]
    Build,
    Plan,
    Review,
    Explore,
    Inspect,
}

impl StatusAccent {
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Build => theme().workflow.build,
            Self::Plan => theme().workflow.plan,
            Self::Review => theme().workflow.review,
            Self::Explore => theme().workflow.explore,
            Self::Inspect => theme().roles.tools,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StatusTone {
    #[default]
    Normal,
    Warning,
}

/// One semantic status snapshot. Rendering owns all adaptive compression.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StatusBarView {
    pub(crate) primary: String,
    pub(crate) accent: StatusAccent,
    pub(crate) tone: StatusTone,
    /// Optional secondary state, retained with inspect controls while space permits.
    pub(crate) detail: Option<String>,
    pub(crate) profile: Option<ModelProfileRef>,
    pub(crate) reasoning: Option<zevria_foundation::ReasoningLevel>,
    pub(crate) context: Option<ContextTokenSnapshot>,
    pub(crate) response: Option<TokenUsage>,
    pub(crate) external_context: Option<ExternalContextUsage>,
    pub(crate) controls: Option<String>,
    /// Essential controls retained before dropping telemetry or the whole hint.
    pub(crate) compact_controls: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseDetail {
    Full,
    Total,
    Omitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelDetail {
    Full,
    ModelOnly,
    Omitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderOptions {
    response: ResponseDetail,
    context_source: bool,
    optional_left: bool,
    compact_controls: bool,
    model: ModelDetail,
    context: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            response: ResponseDetail::Full,
            context_source: true,
            optional_left: true,
            compact_controls: false,
            model: ModelDetail::Full,
            context: true,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct StatusCandidate {
    primary: String,
    tail: String,
    right: String,
}

impl StatusCandidate {
    fn left_width(&self) -> usize {
        display_width(&self.primary).saturating_add(display_width(&self.tail))
    }

    fn right_width(&self) -> usize {
        display_width(&self.right)
    }

    fn minimum_width(&self) -> usize {
        self.left_width()
            .saturating_add(self.right_width())
            .saturating_add(usize::from(!self.right.is_empty()))
    }

    fn fits(&self, width: usize) -> bool {
        self.minimum_width() <= width
    }
}

/// Draw a compact status line on Zevria's explicit canvas without wrapping.
pub(crate) fn render_status_bar(frame: &mut Frame, area: Rect, view: &StatusBarView) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let width = usize::from(area.width);
    let candidate = select_candidate(view, width);
    let primary_color = match view.tone {
        StatusTone::Normal => view.accent.color(),
        StatusTone::Warning => theme().feedback.warning,
    };
    let primary = Style::default()
        .fg(primary_color)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default().fg(theme().text.muted);
    let left_width = candidate.left_width();
    let right_width = candidate.right_width();
    let padding = width.saturating_sub(left_width.saturating_add(right_width));
    let mut spans = vec![Span::styled(candidate.primary, primary)];
    if !candidate.tail.is_empty() {
        spans.push(Span::styled(candidate.tail, secondary));
    }
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    if !candidate.right.is_empty() {
        spans.push(Span::styled(candidate.right, secondary));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(
            Style::new()
                .fg(theme().text.primary)
                .bg(theme().surfaces.canvas),
        ),
        area,
    );
}

fn select_candidate(view: &StatusBarView, width: usize) -> StatusCandidate {
    let mut options = RenderOptions::default();
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.response = ResponseDetail::Total;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.response = ResponseDetail::Omitted;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.context_source = false;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.optional_left = false;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.model = ModelDetail::ModelOnly;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.model = ModelDetail::Omitted;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    options.context = false;
    if let Some(candidate) = fitting_candidate(view, options, width) {
        return candidate;
    }

    StatusCandidate {
        primary: truncate_display_width(&view.primary, width),
        tail: String::new(),
        right: String::new(),
    }
}

fn fitting_candidate(
    view: &StatusBarView,
    options: RenderOptions,
    width: usize,
) -> Option<StatusCandidate> {
    let candidate = build_candidate(view, options);
    candidate.fits(width).then_some(candidate)
}

fn build_candidate(view: &StatusBarView, options: RenderOptions) -> StatusCandidate {
    let mut left = Vec::new();
    if options.optional_left {
        left.extend(
            view.detail
                .iter()
                .filter(|value| !value.is_empty())
                .cloned(),
        );
    }
    if let Some(profile) = &view.profile {
        let model = match options.model {
            ModelDetail::Full => Some(profile.to_string()),
            ModelDetail::ModelOnly => Some(profile.model.clone()),
            ModelDetail::Omitted => None,
        };
        if let Some(mut model) = model {
            if let Some(level) = view.reasoning {
                model.push_str(&format!(" · {level}"));
            }
            left.push(model);
        }
    }
    if options.optional_left {
        let controls = if options.compact_controls {
            view.compact_controls.as_ref().or(view.controls.as_ref())
        } else {
            view.controls.as_ref()
        };
        left.extend(controls.filter(|value| !value.is_empty()).cloned());
    }

    let mut right = Vec::new();
    if options.context {
        if let Some(context) = view.external_context {
            right.push(format!(
                "context {}/{}",
                format_token_count(context.used),
                format_token_count(context.size)
            ));
        } else if let Some(context) = &view.context {
            let mut summary = format!(
                "next {}/{}",
                format_token_count(context.projected_input_tokens),
                format_token_count(context.input_token_limit)
            );
            if options.context_source {
                summary.push(' ');
                summary.push_str(context.source.label());
            }
            right.push(summary);
        }
    }
    if let Some(usage) = view.response {
        match options.response {
            ResponseDetail::Full => right.push(format!(
                "last in {} · cached {} ({:.0}%) · out {} · total {}",
                format_token_count(usage.input_tokens),
                format_token_count(usage.cached_tokens),
                usage.cached_percent(),
                format_token_count(usage.output_tokens),
                format_token_count(usage.total_tokens),
            )),
            ResponseDetail::Total => right.push(format!(
                "last total {}",
                format_token_count(usage.total_tokens)
            )),
            ResponseDetail::Omitted => {}
        }
    }

    StatusCandidate {
        primary: view.primary.clone(),
        tail: if left.is_empty() {
            String::new()
        } else {
            format!("{SEPARATOR}{}", left.join(SEPARATOR))
        },
        right: right.join(SEPARATOR),
    }
}

/// Compact token count: raw below one thousand, then `12.3k`, then `1.2M`.
pub(crate) fn format_token_count(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};
    use zevria_foundation::ModelRole;
    use zevria_model::ContextTokenSource;

    use super::*;

    fn complete_view() -> StatusBarView {
        StatusBarView {
            primary: "Explore · running 🧭".to_string(),
            accent: StatusAccent::Explore,
            detail: Some("secondary".to_string()),
            profile: Some(ModelProfileRef::new("provider", "model-wide")),
            context: Some(ContextTokenSnapshot {
                profile: ModelProfileRef::new("provider", "model-wide"),
                model_role: ModelRole::Explore,
                projected_input_tokens: 17_800,
                source: ContextTokenSource::UsagePlusDelta,
                automatic_trigger: 90_000,
                input_token_limit: 100_000,
                context_window_tokens: 128_000,
            }),
            response: Some(TokenUsage {
                input_tokens: 12_300,
                cached_tokens: 10_200,
                output_tokens: 1_400,
                total_tokens: 13_700,
            }),
            controls: Some("Ctrl+O back".to_string()),
            ..StatusBarView::default()
        }
    }

    fn compact_text(view: &StatusBarView, width: usize) -> String {
        let candidate = select_candidate(view, width);
        let padding = width.saturating_sub(
            candidate
                .left_width()
                .saturating_add(candidate.right_width()),
        );
        format!(
            "{}{}{}{}",
            candidate.primary,
            candidate.tail,
            " ".repeat(padding),
            candidate.right
        )
    }

    #[test]
    fn reasoning_stays_with_model_and_disappears_with_it_at_narrow_widths() {
        let mut view = complete_view();
        view.reasoning = Some(zevria_foundation::ReasoningLevel::High);
        for (detail, expected) in [
            (ModelDetail::Full, "provider/model-wide · high"),
            (ModelDetail::ModelOnly, "model-wide · high"),
        ] {
            let candidate = build_candidate(
                &view,
                RenderOptions {
                    model: detail,
                    ..RenderOptions::default()
                },
            );
            assert!(candidate.tail.contains(expected));
        }
        let omitted = build_candidate(
            &view,
            RenderOptions {
                model: ModelDetail::Omitted,
                ..RenderOptions::default()
            },
        );
        assert!(!omitted.tail.contains("high"));
        for width in 1..160 {
            let text = compact_text(&view, width);
            assert!(
                !text.contains(" · high") || text.contains("model-wide"),
                "{width}: {text}"
            );
        }
    }

    #[test]
    fn response_detail_is_compressed_before_controls() {
        let mut view = complete_view();
        view.compact_controls = Some("short".into());
        view.controls = Some("Ctrl+O back, Ctrl+B/F page, Ctrl+U/D half page".into());
        let full = build_candidate(&view, RenderOptions::default());
        let total = build_candidate(
            &view,
            RenderOptions {
                response: ResponseDetail::Total,
                ..RenderOptions::default()
            },
        );
        assert_eq!(select_candidate(&view, full.minimum_width()), full);
        assert_eq!(select_candidate(&view, total.minimum_width()), total);
        assert!(total.tail.contains("Ctrl+O back, Ctrl+B/F page"));
        assert!(total.right.contains("last total"));
        assert!(!total.right.contains("last in"));
    }

    #[test]
    fn compression_follows_the_fixed_priority_order() {
        let view = complete_view();
        let options = [
            RenderOptions::default(),
            RenderOptions {
                response: ResponseDetail::Total,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                context_source: false,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                context_source: false,
                optional_left: false,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                context_source: false,
                optional_left: false,
                model: ModelDetail::ModelOnly,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                context_source: false,
                optional_left: false,
                model: ModelDetail::Omitted,
                ..RenderOptions::default()
            },
            RenderOptions {
                response: ResponseDetail::Omitted,
                context_source: false,
                optional_left: false,
                compact_controls: true,
                model: ModelDetail::Omitted,
                context: false,
            },
        ];
        let candidates = options
            .into_iter()
            .map(|options| build_candidate(&view, options))
            .collect::<Vec<_>>();
        for pair in candidates.windows(2) {
            assert!(pair[0].minimum_width() > pair[1].minimum_width());
        }
        for (index, candidate) in candidates.iter().enumerate() {
            assert_eq!(
                select_candidate(&view, candidate.minimum_width()),
                *candidate
            );
            if let Some(next) = candidates.get(index + 1) {
                assert_eq!(
                    select_candidate(&view, candidate.minimum_width() - 1),
                    *next
                );
            }
        }

        assert!(candidates[0].right.contains("last in 12.3k"));
        assert!(candidates[1].right.contains("last total 13.7k"));
        assert!(!candidates[2].right.contains("last"));
        assert!(!candidates[3].right.contains("usage+delta"));
        assert!(!candidates[4].tail.contains("Ctrl+O back"));
        assert!(candidates[4].tail.contains("provider/model-wide"));
        assert!(candidates[5].tail.contains("model-wide"));
        assert!(!candidates[5].tail.contains("provider/"));
        assert!(!candidates[6].tail.contains("model-wide"));
        assert!(candidates[6].right.contains("next"));
        assert!(candidates[7].right.is_empty());
    }

    #[test]
    fn zero_one_cell_and_unicode_rows_never_overflow() {
        let mut view = complete_view();
        view.primary = "e\u{301}界🧑🏽‍💻 pane".to_string();
        for width in 0..=32 {
            let row = compact_text(&view, width);
            assert_eq!(display_width(&row), width, "width {width}");
        }
        assert_eq!(compact_text(&view, 0), "");
        assert_eq!(compact_text(&view, 1), "…");
        assert!(compact_text(&view, 6).starts_with("e\u{301}界"));
    }

    #[test]
    fn primary_uses_semantic_accents_and_warning_override_without_reverse() {
        for (accent, expected) in [
            (StatusAccent::Build, theme().workflow.build),
            (StatusAccent::Plan, theme().workflow.plan),
            (StatusAccent::Review, theme().workflow.review),
            (StatusAccent::Explore, theme().workflow.explore),
            (StatusAccent::Inspect, theme().roles.tools),
        ] {
            let view = StatusBarView {
                primary: "State".to_string(),
                accent,
                ..StatusBarView::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(10, 1)).expect("terminal");
            terminal
                .draw(|frame| render_status_bar(frame, frame.area(), &view))
                .expect("render status");
            let buffer = terminal.backend().buffer();
            for x in 0..5 {
                let cell = &buffer[(x, 0)];
                assert_eq!(cell.fg, expected, "accent {accent:?} at {x}");
                assert_eq!(cell.bg, theme().surfaces.canvas);
                assert!(cell.modifier.contains(Modifier::BOLD));
                assert!(!cell.modifier.contains(Modifier::REVERSED));
            }
            for x in 5..10 {
                let cell = &buffer[(x, 0)];
                assert_eq!(cell.fg, theme().text.primary);
                assert_eq!(cell.bg, theme().surfaces.canvas);
                assert_eq!(cell.modifier, Modifier::empty());
            }
        }

        let warning = StatusBarView {
            primary: "Warning".to_string(),
            accent: StatusAccent::Plan,
            tone: StatusTone::Warning,
            ..StatusBarView::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(12, 1)).expect("terminal");
        terminal
            .draw(|frame| render_status_bar(frame, frame.area(), &warning))
            .expect("render warning status");
        let cell = &terminal.backend().buffer()[(0, 0)];
        assert_eq!(cell.fg, theme().feedback.warning);
        assert_eq!(cell.bg, theme().surfaces.canvas);
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(!cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn secondary_and_telemetry_are_muted_with_default_padding_and_right_alignment() {
        let mut view = complete_view();
        view.primary = "Explore running".to_string();
        let candidate = build_candidate(&view, RenderOptions::default());
        let area_width = u16::try_from(candidate.minimum_width() + 7).expect("status width");
        let area = Rect::new(3, 1, area_width, 1);
        let mut terminal = Terminal::new(TestBackend::new(area_width + 6, 3)).expect("terminal");
        terminal
            .draw(|frame| {
                let sentinel = Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::ITALIC);
                for position in [
                    (area.x - 1, area.y),
                    (area.right(), area.y),
                    (area.x, area.y - 1),
                    (area.x, area.bottom()),
                ] {
                    frame
                        .buffer_mut()
                        .cell_mut(position)
                        .expect("sentinel cell")
                        .set_symbol("X")
                        .set_style(sentinel);
                }
                render_status_bar(frame, area, &view);
            })
            .expect("render offset status");
        let buffer = terminal.backend().buffer();

        let primary_end = area
            .x
            .saturating_add(u16::try_from(display_width(&candidate.primary)).expect("primary"));
        let secondary_end = primary_end
            .saturating_add(u16::try_from(display_width(&candidate.tail)).expect("secondary"));
        let telemetry_width = u16::try_from(candidate.right_width()).expect("telemetry");
        let telemetry_start = area.right().saturating_sub(telemetry_width);

        for x in area.x..primary_end {
            let cell = &buffer[(x, area.y)];
            assert_eq!(cell.fg, theme().workflow.explore);
            assert_eq!(cell.bg, theme().surfaces.canvas);
            assert!(cell.modifier.contains(Modifier::BOLD));
            assert!(!cell.modifier.contains(Modifier::REVERSED));
        }
        for x in primary_end..secondary_end {
            let cell = &buffer[(x, area.y)];
            assert_eq!(cell.fg, theme().text.muted);
            assert_eq!(cell.bg, theme().surfaces.canvas);
            assert_eq!(cell.modifier, Modifier::empty());
        }
        for x in secondary_end..telemetry_start {
            let cell = &buffer[(x, area.y)];
            assert_eq!(cell.fg, theme().text.primary);
            assert_eq!(cell.bg, theme().surfaces.canvas);
            assert_eq!(cell.modifier, Modifier::empty());
        }
        for x in telemetry_start..area.right() {
            let cell = &buffer[(x, area.y)];
            assert_eq!(cell.fg, theme().text.muted);
            assert_eq!(cell.bg, theme().surfaces.canvas);
            assert_eq!(cell.modifier, Modifier::empty());
        }
        let rendered_telemetry = (telemetry_start..area.right())
            .map(|x| buffer[(x, area.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_telemetry, candidate.right);

        for position in [
            (area.x - 1, area.y),
            (area.right(), area.y),
            (area.x, area.y - 1),
            (area.x, area.bottom()),
        ] {
            let cell = &buffer[position];
            assert_eq!(cell.symbol(), "X");
            assert_eq!(cell.fg, Color::Yellow);
            assert_eq!(cell.bg, Color::Blue);
            assert!(cell.modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn zero_sized_render_areas_are_ignored() {
        let view = complete_view();
        let mut terminal = Terminal::new(TestBackend::new(3, 2)).expect("terminal");
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .cell_mut((1, 1))
                    .expect("sentinel cell")
                    .set_symbol("X");
                render_status_bar(frame, Rect::new(1, 1, 0, 1), &view);
                render_status_bar(frame, Rect::new(1, 1, 1, 0), &view);
            })
            .expect("render zero-sized status");
        assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "X");
    }
}
