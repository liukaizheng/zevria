pub(crate) use zevria_presentation::status_icon::StatusIcon;
pub(crate) trait StatusIconColor {
    fn color(self) -> ratatui::style::Color;
}
impl StatusIconColor for StatusIcon {
    /// Semantic foreground for unselected content. Final selection styling wins.
    fn color(self) -> ratatui::style::Color {
        let theme = crate::theme::theme();
        match self {
            Self::Pending | Self::Dismissed | Self::Updated | Self::Unknown => theme.text.muted,
            Self::Running => theme.feedback.info,
            Self::Done => theme.feedback.success,
            Self::Failed | Self::Denied => theme.feedback.error,
            Self::Interrupted => theme.feedback.warning,
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
