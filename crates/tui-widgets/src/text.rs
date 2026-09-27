//! Shared terminal text formatting and display-width helpers.

use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// Format completed whole-operation seconds, without rounding elapsed time up.
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Return the number of terminal display cells occupied by `value`.
pub fn display_width(value: &str) -> usize {
    value.width()
}

/// Truncate to at most `max_width` terminal cells at a grapheme boundary.
///
/// Truncated values reserve one cell for an ellipsis. Combining sequences,
/// emoji ZWJ clusters, and double-width graphemes are never split.
pub fn truncate_display_width(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let content_width = max_width - 1;
    let mut width = 0_usize;
    let mut truncated = String::new();
    for grapheme in value.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if width.saturating_add(grapheme_width) > content_width {
            break;
        }
        truncated.push_str(grapheme);
        width = width.saturating_add(grapheme_width);
    }
    truncated.push('…');
    truncated
}

/// Truncate to at most `max_width` terminal cells while preserving the end.
///
/// Truncated values reserve one cell for a leading ellipsis. This is useful
/// for paths, whose trailing components are generally more informative than
/// their root or earliest ancestors.
pub fn truncate_leading_display_width(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let content_width = max_width - 1;
    let mut width = 0_usize;
    let mut suffix = Vec::new();
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = display_width(grapheme);
        if width.saturating_add(grapheme_width) > content_width {
            break;
        }
        suffix.push(grapheme);
        width = width.saturating_add(grapheme_width);
    }

    let mut truncated = String::from('…');
    for grapheme in suffix.into_iter().rev() {
        truncated.push_str(grapheme);
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_uses_completed_seconds_minutes_and_hours() {
        for (seconds, expected) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m 00s"),
            (65, "1m 05s"),
            (3599, "59m 59s"),
            (3600, "1h 00m 00s"),
            (3723, "1h 02m 03s"),
        ] {
            assert_eq!(format_elapsed(Duration::from_secs(seconds)), expected);
            assert_eq!(
                format_elapsed(Duration::from_secs(seconds) + Duration::from_millis(999)),
                expected
            );
        }
    }

    #[test]
    fn truncation_respects_terminal_cells_and_graphemes() {
        assert_eq!(truncate_display_width("plain", 4), "pla…");
        assert_eq!(truncate_display_width("e\u{301}cho", 3), "e\u{301}c…");
        assert_eq!(truncate_display_width("界面", 3), "界…");
        assert_eq!(truncate_display_width("🧑🏽‍💻x", 2), "…");
        assert_eq!(display_width(&truncate_display_width("界面", 3)), 3);
        assert_eq!(truncate_display_width("anything", 1), "…");
        assert_eq!(truncate_display_width("anything", 0), "");
    }

    #[test]
    fn leading_truncation_respects_terminal_cells_and_graphemes() {
        assert_eq!(truncate_leading_display_width("plain", 4), "…ain");
        assert_eq!(truncate_leading_display_width("e\u{301}cho", 3), "…ho");
        assert_eq!(truncate_leading_display_width("界面", 3), "…面");
        assert_eq!(truncate_leading_display_width("x🧑🏽‍💻", 2), "…");
        assert_eq!(display_width(&truncate_leading_display_width("界面", 3)), 3);
        assert_eq!(truncate_leading_display_width("anything", 1), "…");
        assert_eq!(truncate_leading_display_width("anything", 0), "");
    }
}
