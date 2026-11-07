//! Memory-only, pane-local follow-ups. Items acquire engine identity only when
//! admitted through the ordinary presentation APIs.
use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;
use zevria_content::{UserPrompt, readable::sanitize_readable};
use zevria_foundation::{RequestBehavior, SessionMode};
use zevria_instructions::SkillName;

use crate::composer::ComposerDraft;

pub(super) const MAX_QUEUED_FOLLOW_UPS: usize = 20;
const MAX_PREVIEW_GRAPHEMES: usize = 160;
const VISIBLE_ROWS: usize = 5;

#[derive(Debug, Clone)]
pub(super) enum QueuedWork {
    Message {
        text: UserPrompt,
        behavior: RequestBehavior,
    },
    Skill {
        name: SkillName,
        args: UserPrompt,
    },
}

impl QueuedWork {
    fn preview(&self) -> String {
        let text = match self {
            Self::Message { text, behavior } => format!(
                "{}{}",
                if *behavior == RequestBehavior::Orchestrate {
                    "/orchestrate "
                } else {
                    ""
                },
                text.display_projection()
            ),
            Self::Skill { name, args } => format!("${name} {}", args.display_projection()),
        };
        let readable = sanitize_readable(&text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut graphemes = readable.graphemes(true);
        let mut preview = graphemes
            .by_ref()
            .take(MAX_PREVIEW_GRAPHEMES)
            .collect::<String>();
        if graphemes.next().is_some() {
            preview.push('…');
        }
        preview
    }
}

#[derive(Debug)]
pub(super) struct QueuedFollowUp {
    pub draft: ComposerDraft,
    pub work: QueuedWork,
    pub mode: SessionMode,
    pub preview: String,
}

impl QueuedFollowUp {
    pub(super) fn new(draft: ComposerDraft, work: QueuedWork, mode: SessionMode) -> Self {
        let preview = work.preview();
        Self {
            draft,
            work,
            mode,
            preview,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueuePause {
    Cancelled,
    Failed,
    Rejected,
    PlanDecision,
    ModeChanged,
}

impl QueuePause {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Cancelled => "cancellation requested",
            Self::Failed => "turn failed",
            Self::Rejected => "turn rejected",
            Self::PlanDecision => "Plan decision required",
            Self::ModeChanged => "mode changed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueueRow {
    pub mode: SessionMode,
    pub preview: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct QueueView {
    pub rows: Vec<QueueRow>,
    pub hidden: usize,
    pub paused: Option<QueuePause>,
    pub len: usize,
}

#[derive(Default)]
pub(super) struct FollowUpQueue {
    items: VecDeque<QueuedFollowUp>,
    paused: Option<QueuePause>,
}

impl FollowUpQueue {
    pub fn push(&mut self, item: QueuedFollowUp) -> Result<(), QueuedFollowUp> {
        if self.len() == MAX_QUEUED_FOLLOW_UPS {
            return Err(item);
        }
        self.items.push_back(item);
        Ok(())
    }
    pub fn front(&self) -> Option<&QueuedFollowUp> {
        self.items.front()
    }
    pub fn pop_front(&mut self) -> Option<QueuedFollowUp> {
        let item = self.items.pop_front();
        if self.items.is_empty() {
            self.paused = None;
        }
        item
    }
    pub fn push_front(&mut self, item: QueuedFollowUp) {
        self.items.push_front(item);
    }
    pub fn pop_back(&mut self) -> Option<QueuedFollowUp> {
        let item = self.items.pop_back();
        if self.items.is_empty() {
            self.paused = None;
        }
        item
    }
    pub fn clear(&mut self) -> usize {
        let count = self.len();
        self.items.clear();
        self.resume();
        count
    }
    pub fn pause(&mut self, reason: QueuePause) {
        if !self.items.is_empty() {
            self.paused = Some(reason);
        }
    }
    pub fn resume(&mut self) {
        self.paused = None;
    }
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }
    pub fn is_running_nonempty(&self) -> bool {
        !self.items.is_empty() && !self.is_paused()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn view(&self) -> QueueView {
        QueueView {
            rows: self
                .items
                .iter()
                .take(VISIBLE_ROWS)
                .map(|item| QueueRow {
                    mode: item.mode,
                    preview: item.preview.clone(),
                })
                .collect(),
            hidden: self.len().saturating_sub(VISIBLE_ROWS),
            paused: self.paused,
            len: self.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> QueuedFollowUp {
        QueuedFollowUp::new(
            ComposerDraft::default(),
            QueuedWork::Message {
                text: text.into(),
                behavior: RequestBehavior::Standard,
            },
            SessionMode::Build,
        )
    }

    #[test]
    fn fifo_lifo_edit_cap_and_pause_lifecycle() {
        let mut queue = FollowUpQueue::default();
        queue.pause(QueuePause::Failed);
        assert!(!queue.is_paused());
        for index in 0..MAX_QUEUED_FOLLOW_UPS {
            queue.push(item(&index.to_string())).unwrap();
        }
        assert!(queue.push(item("overflow")).is_err());
        assert_eq!(queue.pop_back().unwrap().preview, "19");
        assert_eq!(queue.pop_front().unwrap().preview, "0");
        let head = queue.pop_front().unwrap();
        queue.push_front(head);
        assert_eq!(queue.front().unwrap().preview, "1");
        queue.pause(QueuePause::Cancelled);
        assert!(!queue.is_running_nonempty());
        assert_eq!(queue.view().hidden, 13);
        queue.resume();
        assert!(queue.is_running_nonempty());
        assert_eq!(queue.clear(), 18);
        assert!(!queue.is_paused());
    }

    #[test]
    fn previews_are_sanitized_collapsed_bounded_and_do_not_change_prompts() {
        let text: UserPrompt =
            "before\u{1b}[31mred\u{1b}[0m\u{202e}\u{2066}after\u{7}\n\t next".into();
        let work = QueuedWork::Message {
            text: text.clone(),
            behavior: RequestBehavior::Orchestrate,
        };
        assert_eq!(work.preview(), "/orchestrate beforeredafter next");
        let QueuedWork::Message {
            text: unchanged, ..
        } = work
        else {
            unreachable!()
        };
        assert_eq!(text, unchanged);
        let skill = QueuedWork::Skill {
            name: SkillName::parse("review").unwrap(),
            args: "one\ntwo".into(),
        };
        assert_eq!(skill.preview(), "$review one two");
        let long = item(&"👩‍💻".repeat(200));
        assert_eq!(
            long.preview.graphemes(true).count(),
            MAX_PREVIEW_GRAPHEMES + 1
        );
    }
}
