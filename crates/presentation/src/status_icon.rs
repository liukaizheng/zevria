//! Shared, single-cell status glyphs for TUI presentation.
//!
//! Clock-owned rendering advances the running frame every 250 ms. Cached
//! transcript rows have no clock and use frame zero to keep layout reusable.

const RUNNING_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusIcon {
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
    pub const fn agent(status: zevria_workflow::AgentRunStatus) -> Self {
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

    pub fn glyph(self, frame: usize) -> &'static str {
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
