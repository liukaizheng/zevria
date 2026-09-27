//! Application-owned frame layout and lower-surface allocation.

use ratatui::layout::Rect;
use zevria_tui_widgets::chrome::{
    BLOCK_PAD_LEFT, BLOCK_PAD_RIGHT, CHROME_PAD_LEFT, CHROME_PAD_RIGHT, PROMPT_PREFIX_WIDTH,
};

pub(crate) const OUTER_HPAD: u16 = 2;
pub(crate) const OUTER_VPAD: u16 = 1;
pub(crate) const MIN_HPAD: u16 = 1;
pub(crate) const CONVERSATION_MIN_ROWS: u16 = 5;
pub(crate) const AUTO_COMPACT_MAX_ROWS: u16 = 20;
pub(crate) const SHORT_TERMINAL_ROWS: u16 = 16;
pub(crate) const ORDINARY_LOWER_MIN_ROWS: u16 = 2;
pub(crate) const PLAN_LOWER_MIN_ROWS: u16 = 3;
pub(crate) const WORKSPACE_HEADER_MIN_ROWS: u16 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Metrics {
    pub(crate) hpad_left: u16,
    pub(crate) hpad_right: u16,
    pub(crate) vpad: u16,
    pub(crate) gap: u16,
    pub(crate) hints_enabled: bool,
}

impl Metrics {
    pub(crate) const fn for_height(height: u16) -> Self {
        if height <= AUTO_COMPACT_MAX_ROWS {
            Self {
                hpad_left: MIN_HPAD,
                hpad_right: MIN_HPAD,
                vpad: 0,
                gap: 0,
                hints_enabled: height > SHORT_TERMINAL_ROWS,
            }
        } else {
            Self {
                hpad_left: OUTER_HPAD,
                hpad_right: OUTER_HPAD,
                vpad: OUTER_VPAD,
                gap: 1,
                hints_enabled: true,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LowerSurface {
    None,
    Composer { requested_height: u16 },
    Plan { requested_height: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameLayout {
    pub(crate) metrics: Metrics,
    pub(crate) workspace_header_enabled: bool,
    pub(crate) workspace_header: Rect,
    pub(crate) workspace_header_rule: Rect,
    pub(crate) conversation: Rect,
    pub(crate) conversation_content: Rect,
    pub(crate) conversation_scrollbar: Rect,
    pub(crate) selection_band: Rect,
    pub(crate) prompt: Rect,
    pub(crate) hints: Rect,
    pub(crate) status: Rect,
    pub(crate) modal_body: Rect,
}

impl FrameLayout {
    pub(crate) fn compute(
        frame: Rect,
        inspect: bool,
        lower: LowerSurface,
        workspace_header_required: bool,
    ) -> Self {
        let metrics = Metrics::for_height(frame.height);
        let workspace_header_enabled =
            workspace_header_required && frame.height >= WORKSPACE_HEADER_MIN_ROWS;
        let workspace_header_height = u16::from(workspace_header_enabled).min(frame.height);
        let workspace_header_gap = if workspace_header_enabled {
            metrics.gap
        } else {
            0
        };
        let workspace_header = inset_passive_row(
            Rect {
                x: frame.x,
                y: frame.y,
                width: frame.width,
                height: workspace_header_height,
            },
            metrics,
        );
        let workspace_header_rule = inset_passive_row(
            Rect {
                x: frame.x,
                y: workspace_header.bottom(),
                width: frame.width,
                height: workspace_header_gap,
            },
            metrics,
        );
        let status_enabled = if inspect {
            frame.height >= 4
        } else {
            frame.height >= 5
        };
        let status_height = u16::from(status_enabled).min(frame.height);
        let body_height = frame.height.saturating_sub(status_height);
        let body = Rect {
            x: frame.x,
            y: frame.y,
            width: frame.width,
            height: body_height,
        };
        let status = inset_passive_row(
            Rect {
                x: frame.x,
                y: frame.y.saturating_add(body_height),
                width: frame.width,
                height: status_height,
            },
            metrics,
        );
        let padded = inset_rect(
            body,
            metrics.hpad_left,
            metrics.hpad_right,
            metrics
                .vpad
                .max(workspace_header_height.saturating_add(workspace_header_gap)),
            metrics.vpad,
        );

        let has_lower = !matches!(lower, LowerSurface::None);
        let hints_height =
            u16::from(matches!(lower, LowerSurface::Composer { .. }) && metrics.hints_enabled)
                .min(padded.height);
        let transcript_gap = if has_lower { metrics.gap } else { 0 };
        let hints_gap = if hints_height > 0 { metrics.gap } else { 0 };
        let fixed_height = hints_height
            .saturating_add(transcript_gap)
            .saturating_add(hints_gap)
            .min(padded.height);
        let flexible_budget = padded.height.saturating_sub(fixed_height);
        let (requested_height, lower_minimum) = match lower {
            LowerSurface::None => (0, 0),
            LowerSurface::Composer { requested_height } => {
                (requested_height, ORDINARY_LOWER_MIN_ROWS)
            }
            LowerSurface::Plan { requested_height } => (requested_height, PLAN_LOWER_MIN_ROWS),
        };
        let lower_height = if !has_lower {
            0
        } else if flexible_budget >= CONVERSATION_MIN_ROWS.saturating_add(lower_minimum) {
            requested_height
                .max(lower_minimum)
                .min(flexible_budget.saturating_sub(CONVERSATION_MIN_ROWS))
        } else {
            lower_minimum.min(flexible_budget.saturating_sub(1))
        };
        let conversation_height = flexible_budget.saturating_sub(lower_height);

        let scrollbar_x = if frame.width > 0 {
            frame.right().saturating_sub(1)
        } else {
            frame.x
        };
        let right_selection_available = frame.width >= 2;
        let selection_right_x = if right_selection_available {
            frame.right().saturating_sub(2)
        } else {
            frame.x
        };
        let conversation_right = if right_selection_available {
            padded.right().min(selection_right_x)
        } else {
            padded.x
        };
        let conversation = Rect {
            x: padded.x,
            y: padded.y,
            width: conversation_right.saturating_sub(padded.x),
            height: conversation_height,
        };
        let conversation_content = inset_rect(
            conversation,
            1_u16.saturating_add(BLOCK_PAD_LEFT),
            BLOCK_PAD_RIGHT,
            0,
            0,
        );
        let conversation_scrollbar = Rect {
            x: scrollbar_x,
            y: conversation.y,
            width: u16::from(frame.width > 0),
            height: conversation.height,
        };
        let selection_left_x = if conversation.x > frame.x {
            conversation.x - 1
        } else {
            conversation.x
        };
        let selection_band = Rect {
            x: selection_left_x,
            y: conversation.y,
            width: if selection_left_x < selection_right_x {
                selection_right_x
                    .saturating_sub(selection_left_x)
                    .saturating_add(1)
            } else {
                0
            },
            height: conversation.height,
        };

        let prompt_y = conversation.bottom().saturating_add(
            transcript_gap.min(padded.bottom().saturating_sub(conversation.bottom())),
        );
        let prompt = Rect {
            x: padded.x,
            y: prompt_y.min(padded.bottom()),
            width: padded.width,
            height: lower_height.min(
                padded
                    .bottom()
                    .saturating_sub(prompt_y.min(padded.bottom())),
            ),
        };
        let hints_y = prompt
            .bottom()
            .saturating_add(hints_gap.min(padded.bottom().saturating_sub(prompt.bottom())));
        let hints = Rect {
            x: padded.x,
            y: hints_y.min(padded.bottom()),
            width: padded.width,
            height: hints_height.min(padded.bottom().saturating_sub(hints_y.min(padded.bottom()))),
        };
        let modal_end = if has_lower {
            prompt.bottom()
        } else {
            conversation.bottom()
        };
        let modal_body = Rect {
            x: padded.x,
            y: padded.y,
            width: padded.width,
            height: modal_end.min(padded.bottom()).saturating_sub(padded.y),
        };

        Self {
            metrics,
            workspace_header_enabled,
            workspace_header,
            workspace_header_rule,
            conversation,
            conversation_content,
            conversation_scrollbar,
            selection_band,
            prompt,
            hints,
            status,
            modal_body,
        }
    }

    pub(crate) fn padded_width(frame: Rect) -> u16 {
        let metrics = Metrics::for_height(frame.height);
        frame
            .width
            .saturating_sub(metrics.hpad_left)
            .saturating_sub(metrics.hpad_right)
    }

    pub(crate) const fn composer_text_width(prompt_width: u16) -> u16 {
        prompt_width
            .saturating_sub(CHROME_PAD_LEFT)
            .saturating_sub(CHROME_PAD_RIGHT)
            .saturating_sub(PROMPT_PREFIX_WIDTH)
    }
}

fn inset_passive_row(rect: Rect, metrics: Metrics) -> Rect {
    inset_rect(rect, metrics.hpad_left, metrics.hpad_right, 0, 0)
}

fn inset_rect(rect: Rect, left: u16, right: u16, top: u16, bottom: u16) -> Rect {
    let left = left.min(rect.width);
    let right = right.min(rect.width.saturating_sub(left));
    let top = top.min(rect.height);
    let bottom = bottom.min(rect.height.saturating_sub(top));
    Rect {
        x: rect.x.saturating_add(left),
        y: rect.y.saturating_add(top),
        width: rect.width.saturating_sub(left).saturating_sub(right),
        height: rect.height.saturating_sub(top).saturating_sub(bottom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn composer_layout(width: u16, height: u16) -> FrameLayout {
        FrameLayout::compute(
            Rect::new(0, 0, width, height),
            false,
            LowerSurface::Composer {
                requested_height: 3,
            },
            false,
        )
    }

    #[test]
    fn empty_composer_height_checkpoints_match_the_surface_contract() {
        let spacious = composer_layout(80, 21);
        assert_eq!(spacious.metrics, Metrics::for_height(21));
        assert_eq!(spacious.conversation, Rect::new(2, 1, 76, 12));
        assert_eq!(spacious.prompt, Rect::new(2, 14, 76, 3));
        assert_eq!(spacious.hints, Rect::new(2, 18, 76, 1));
        assert_eq!(spacious.status, Rect::new(2, 20, 76, 1));

        let compact = composer_layout(80, 20);
        assert_eq!(compact.conversation.height, 15);
        assert_eq!(compact.prompt, Rect::new(1, 15, 78, 3));
        assert_eq!(compact.hints, Rect::new(1, 18, 78, 1));
        assert_eq!(compact.status, Rect::new(1, 19, 78, 1));

        let short = composer_layout(80, 16);
        assert_eq!(short.conversation.height, 12);
        assert_eq!(short.prompt, Rect::new(1, 12, 78, 3));
        assert_eq!(short.hints.height, 0);
        assert_eq!(short.status, Rect::new(1, 15, 78, 1));

        let tiny = composer_layout(80, 8);
        assert_eq!(tiny.conversation.height, 5);
        assert_eq!(tiny.prompt, Rect::new(1, 5, 78, 2));
        assert_eq!(tiny.status, Rect::new(1, 7, 78, 1));
    }

    #[test]
    fn global_header_adds_spacious_gap_without_moving_lower_surfaces() {
        let baseline = composer_layout(80, 21);
        let layout = FrameLayout::compute(
            Rect::new(0, 0, 80, 21),
            false,
            LowerSurface::Composer {
                requested_height: 3,
            },
            true,
        );

        assert!(layout.workspace_header_enabled);
        assert_eq!(layout.workspace_header, Rect::new(2, 0, 76, 1));
        assert_eq!(layout.workspace_header_rule, Rect::new(2, 1, 76, 1));
        assert_eq!(layout.workspace_header.x, layout.prompt.x);
        assert_eq!(layout.workspace_header.right(), layout.prompt.right());
        assert_eq!(layout.workspace_header.x, layout.status.x);
        assert_eq!(layout.workspace_header.right(), layout.status.right());
        assert_eq!(layout.metrics, baseline.metrics);
        assert_eq!(
            layout.conversation.y,
            layout.workspace_header.bottom() + layout.metrics.gap
        );
        assert_eq!(layout.conversation.y, baseline.conversation.y + 1);
        assert_eq!(layout.conversation.height + 1, baseline.conversation.height);
        assert_eq!(
            layout.conversation_content.y,
            baseline.conversation_content.y + 1
        );
        assert_eq!(
            layout.conversation_content.height + 1,
            baseline.conversation_content.height
        );
        assert_eq!(
            layout.conversation_scrollbar.y,
            baseline.conversation_scrollbar.y + 1
        );
        assert_eq!(
            layout.conversation_scrollbar.height + 1,
            baseline.conversation_scrollbar.height
        );
        assert_eq!(layout.selection_band.y, baseline.selection_band.y + 1);
        assert_eq!(
            layout.selection_band.height + 1,
            baseline.selection_band.height
        );
        assert_eq!(layout.prompt, baseline.prompt);
        assert_eq!(layout.hints, baseline.hints);
        assert_eq!(layout.status, baseline.status);
        assert_eq!(layout.modal_body.y, baseline.modal_body.y + 1);
        assert_eq!(layout.modal_body.bottom(), baseline.modal_body.bottom());
    }

    #[test]
    fn global_header_takes_only_one_compact_transcript_row() {
        let baseline = composer_layout(80, 20);
        let layout = FrameLayout::compute(
            Rect::new(0, 0, 80, 20),
            false,
            LowerSurface::Composer {
                requested_height: 3,
            },
            true,
        );

        assert!(layout.workspace_header_enabled);
        assert_eq!(layout.workspace_header, Rect::new(1, 0, 78, 1));
        assert_eq!(layout.workspace_header_rule.height, 0);
        assert_eq!(layout.workspace_header.x, layout.prompt.x);
        assert_eq!(layout.workspace_header.right(), layout.prompt.right());
        assert_eq!(layout.workspace_header.x, layout.status.x);
        assert_eq!(layout.workspace_header.right(), layout.status.right());
        assert_eq!(layout.metrics.gap, 0);
        assert_eq!(layout.conversation.y, layout.workspace_header.bottom());
        assert_eq!(layout.conversation.y, baseline.conversation.y + 1);
        assert_eq!(layout.conversation.height + 1, baseline.conversation.height);
        assert_eq!(
            layout.conversation_content.y,
            baseline.conversation_content.y + 1
        );
        assert_eq!(
            layout.conversation_content.height + 1,
            baseline.conversation_content.height
        );
        assert_eq!(
            layout.conversation_scrollbar.y,
            baseline.conversation_scrollbar.y + 1
        );
        assert_eq!(
            layout.conversation_scrollbar.height + 1,
            baseline.conversation_scrollbar.height
        );
        assert_eq!(layout.selection_band.y, baseline.selection_band.y + 1);
        assert_eq!(
            layout.selection_band.height + 1,
            baseline.selection_band.height
        );
        assert_eq!(layout.prompt, baseline.prompt);
        assert_eq!(layout.hints, baseline.hints);
        assert_eq!(layout.status, baseline.status);
        assert_eq!(layout.modal_body.y, baseline.modal_body.y + 1);
        assert_eq!(layout.modal_body.bottom(), baseline.modal_body.bottom());
    }

    #[test]
    fn global_header_is_suppressed_below_six_rows() {
        for height in 0..WORKSPACE_HEADER_MIN_ROWS {
            let frame = Rect::new(3, 4, 40, height);
            let baseline = FrameLayout::compute(
                frame,
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                false,
            );
            let requested = FrameLayout::compute(
                frame,
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                true,
            );
            assert_eq!(requested, baseline);
            assert!(!requested.workspace_header_enabled);
            assert_eq!(requested.workspace_header.height, 0);
        }
    }

    #[test]
    fn protected_header_and_all_surfaces_remain_clamped_and_separate() {
        for (width, height) in [(0, 0), (1, 6), (2, 8), (8, 16), (40, 20), (80, 21)] {
            let frame = Rect::new(5, 7, width, height);
            let layout = FrameLayout::compute(
                frame,
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                true,
            );
            for area in [
                layout.workspace_header,
                layout.workspace_header_rule,
                layout.conversation,
                layout.conversation_content,
                layout.conversation_scrollbar,
                layout.selection_band,
                layout.prompt,
                layout.hints,
                layout.status,
                layout.modal_body,
            ] {
                assert!(area.x >= frame.x);
                assert!(area.y >= frame.y);
                assert!(area.right() <= frame.right());
                assert!(area.bottom() <= frame.bottom());
            }

            if layout.workspace_header_enabled {
                let protected_bottom = layout
                    .workspace_header
                    .bottom()
                    .saturating_add(layout.metrics.gap);
                for area in [
                    layout.conversation,
                    layout.conversation_content,
                    layout.conversation_scrollbar,
                    layout.selection_band,
                    layout.prompt,
                    layout.hints,
                    layout.status,
                    layout.modal_body,
                ] {
                    assert!(area.height == 0 || area.y >= protected_bottom);
                }
            }
            assert_eq!(
                layout
                    .selection_band
                    .intersection(layout.conversation_scrollbar)
                    .width,
                0
            );
            assert!(layout.conversation.bottom() <= layout.prompt.y || layout.prompt.height == 0);
            assert!(layout.prompt.bottom() <= layout.hints.y || layout.hints.height == 0);
            assert!(layout.prompt.bottom() <= layout.status.y || layout.status.height == 0);
            assert!(layout.hints.bottom() <= layout.status.y || layout.status.height == 0);
        }
    }

    #[test]
    fn plan_keeps_one_choice_on_a_four_row_terminal() {
        let layout = FrameLayout::compute(
            Rect::new(0, 0, 80, 4),
            false,
            LowerSurface::Plan {
                requested_height: 5,
            },
            false,
        );
        assert_eq!(layout.status.height, 0);
        assert_eq!(layout.conversation.height, 1);
        assert_eq!(layout.prompt.height, 3);
    }

    #[test]
    fn status_thresholds_differ_for_interactive_and_inspect_panes() {
        let interactive = FrameLayout::compute(
            Rect::new(0, 0, 40, 4),
            false,
            LowerSurface::Composer {
                requested_height: 3,
            },
            false,
        );
        let inspect = FrameLayout::compute(Rect::new(0, 0, 40, 4), true, LowerSurface::None, false);
        assert_eq!(interactive.status.height, 0);
        assert_eq!(inspect.status.height, 1);
        assert_eq!(
            FrameLayout::compute(Rect::new(0, 0, 40, 3), true, LowerSurface::None, false,)
                .status
                .height,
            0
        );
        assert_eq!(
            FrameLayout::compute(
                Rect::new(0, 0, 40, 5),
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                false,
            )
            .status
            .height,
            1
        );
    }

    #[test]
    fn modal_body_excludes_shared_hints_and_status() {
        let layout = composer_layout(80, 21);
        assert_eq!(layout.modal_body, Rect::new(2, 1, 76, 16));
        assert!(layout.modal_body.bottom() < layout.hints.y);
        assert!(layout.modal_body.bottom() < layout.status.y);
    }

    #[test]
    fn selection_band_and_scrollbar_columns_remain_separate() {
        let spacious = composer_layout(80, 21);
        assert_eq!(spacious.selection_band, Rect::new(1, 1, 78, 12));
        assert_eq!(spacious.conversation, Rect::new(2, 1, 76, 12));
        assert_eq!(spacious.conversation_content, Rect::new(5, 1, 71, 12));
        assert_eq!(spacious.conversation_scrollbar, Rect::new(79, 1, 1, 12));

        let compact = composer_layout(80, 20);
        assert_eq!(compact.selection_band, Rect::new(0, 0, 79, 15));
        assert_eq!(compact.conversation, Rect::new(1, 0, 77, 15));
        assert_eq!(compact.conversation_content, Rect::new(4, 0, 72, 15));
        assert_eq!(compact.conversation_scrollbar, Rect::new(79, 0, 1, 15));
    }

    #[test]
    fn narrow_and_empty_frames_are_parent_clamped() {
        for width in [0, 1, 2, 8] {
            let frame = Rect::new(5, 7, width, 8);
            let layout = FrameLayout::compute(
                frame,
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                true,
            );
            for area in [
                layout.workspace_header,
                layout.workspace_header_rule,
                layout.conversation,
                layout.conversation_content,
                layout.conversation_scrollbar,
                layout.selection_band,
                layout.prompt,
                layout.hints,
                layout.status,
                layout.modal_body,
            ] {
                assert!(area.x >= frame.x);
                assert!(area.y >= frame.y);
                assert!(area.right() <= frame.right());
                assert!(area.bottom() <= frame.bottom());
            }
            if width <= 2 {
                assert_eq!(layout.selection_band.width, 0);
            }
        }
    }

    #[test]
    fn workspace_header_gutters_clamp_safely_in_empty_and_narrow_frames() {
        for (width, height, expected) in [
            (0, 8, Rect::new(5, 7, 0, 1)),
            (1, 8, Rect::new(6, 7, 0, 1)),
            (2, 8, Rect::new(6, 7, 0, 1)),
            (3, 8, Rect::new(6, 7, 1, 1)),
            (0, 21, Rect::new(5, 7, 0, 1)),
            (1, 21, Rect::new(6, 7, 0, 1)),
            (2, 21, Rect::new(7, 7, 0, 1)),
            (3, 21, Rect::new(7, 7, 0, 1)),
        ] {
            let layout = FrameLayout::compute(
                Rect::new(5, 7, width, height),
                false,
                LowerSurface::Composer {
                    requested_height: 3,
                },
                true,
            );

            assert_eq!(layout.workspace_header, expected);
            assert_eq!(layout.workspace_header.x, layout.prompt.x);
            assert_eq!(layout.workspace_header.right(), layout.prompt.right());
            assert_eq!(layout.workspace_header.x, layout.status.x);
            assert_eq!(layout.workspace_header.right(), layout.status.right());
        }
    }
}
