//! Shared, single-cell status glyphs for TUI presentation.
//!
//! Clock-owned rendering advances the running frame every 250 ms. Cached
//! transcript rows have no clock and use frame zero to keep layout reusable.

const RUNNING_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatusIcon {
    Pending,
    Running,
    Done,
    Failed,
    Denied,
    Interrupted,
    Dismissed,
    Updated,
    Unknown,
}

impl StatusIcon {
    pub(crate) const fn agent(status: zevria_workflow::AgentRunStatus) -> Self {
        use zevria_workflow::AgentRunStatus::*;
        match status {
            Queued | AwaitingFeedback | AwaitingConfirmation => Self::Pending,
            Starting | Running | Resuming => Self::Running,
            Completed | Confirmed => Self::Done,
            Failed | TimedOut | Blocked => Self::Failed,
            Cancelled | Interrupted => Self::Interrupted,
            Abandoned => Self::Dismissed,
        }
    }

    /// Semantic foreground for unselected content. Final selection styling wins.
    pub(crate) fn color(self) -> ratatui::style::Color {
        let theme = crate::theme::theme();
        match self {
            Self::Pending | Self::Dismissed | Self::Updated | Self::Unknown => theme.text.muted,
            Self::Running => theme.feedback.info,
            Self::Done => theme.feedback.success,
            Self::Failed | Self::Denied => theme.feedback.error,
            Self::Interrupted => theme.feedback.warning,
        }
    }

    pub(crate) fn glyph(self, frame: usize) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::Running => RUNNING_FRAMES[frame % RUNNING_FRAMES.len()],
            Self::Done => "✓",
            Self::Failed => "✗",
            Self::Denied => "⊘",
            Self::Interrupted => "◼",
            Self::Dismissed => "–",
            Self::Updated => "•",
            Self::Unknown => "?",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StatusIcon;
    use crate::text::display_width;

    #[test]
    fn status_glyphs_match_the_vocabulary_and_are_single_width() {
        for (status, expected) in [
            (StatusIcon::Pending, "○"),
            (StatusIcon::Done, "✓"),
            (StatusIcon::Failed, "✗"),
            (StatusIcon::Denied, "⊘"),
            (StatusIcon::Interrupted, "◼"),
            (StatusIcon::Dismissed, "–"),
            (StatusIcon::Updated, "•"),
            (StatusIcon::Unknown, "?"),
        ] {
            for frame in [0, 1, 4, usize::MAX] {
                let glyph = status.glyph(frame);
                assert_eq!(glyph, expected, "{status:?}, frame {frame}");
                assert_eq!(display_width(glyph), 1, "{status:?}");
            }
        }
        for (frame, expected) in ["◐", "◓", "◑", "◒"].into_iter().enumerate() {
            let glyph = StatusIcon::Running.glyph(frame);
            assert_eq!(glyph, expected);
            assert_eq!(display_width(glyph), 1, "running frame {frame}");
            assert_eq!(StatusIcon::Running.glyph(frame + 4), expected);
        }
        assert_eq!(StatusIcon::Running.glyph(usize::MAX), "◒");
    }
}
