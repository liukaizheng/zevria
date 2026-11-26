//! Transcript recall and the engine's durable edit acceptance boundary.

use zevria_session_api::TranscriptEditTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallEdit<D> {
    pub saved_input: D,
    pub target: TranscriptEditTarget,
    pub target_index: usize,
}

#[derive(Debug, Default)]
pub enum EditState<D> {
    #[default]
    None,
    Recalling(RecallEdit<D>),
    AwaitingAcceptance {
        recall: RecallEdit<D>,
    },
}

impl<D> EditState<D> {
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub const fn is_recalling(&self) -> bool {
        matches!(self, Self::Recalling(_))
    }

    pub const fn is_awaiting_acceptance(&self) -> bool {
        matches!(self, Self::AwaitingAcceptance { .. })
    }

    pub const fn is_active(&self) -> bool {
        !self.is_none()
    }

    pub fn recall(&mut self, edit: RecallEdit<D>) -> bool {
        if !self.is_none() {
            return false;
        }
        *self = Self::Recalling(edit);
        true
    }

    /// Keep the pre-recall draft, even when the user changes targets.
    pub fn switch_target(&mut self, target: TranscriptEditTarget, target_index: usize) -> bool {
        let Self::Recalling(recall) = self else {
            return false;
        };
        recall.target = target;
        recall.target_index = target_index;
        true
    }

    pub fn recalled_target(&self) -> Option<(TranscriptEditTarget, usize)> {
        match self {
            Self::Recalling(recall) => Some((recall.target.clone(), recall.target_index)),
            _ => None,
        }
    }

    pub const fn active_target_index(&self) -> Option<usize> {
        match self {
            Self::Recalling(recall) | Self::AwaitingAcceptance { recall } => {
                Some(recall.target_index)
            }
            Self::None => None,
        }
    }

    pub fn cancel_recall(&mut self) -> Option<RecallEdit<D>> {
        if !self.is_recalling() {
            return None;
        }
        let Self::Recalling(recall) = std::mem::take(self) else {
            unreachable!()
        };
        Some(recall)
    }

    /// Lock the recall until the engine acknowledges or rejects the edit.
    pub fn stage_submission(&mut self) -> Option<TranscriptEditTarget> {
        let recall = self.cancel_recall()?;
        let target = recall.target.clone();
        *self = Self::AwaitingAcceptance { recall };
        Some(target)
    }

    pub fn accept(&mut self) {
        if self.is_awaiting_acceptance() {
            self.reset();
        }
    }

    pub fn reject(&mut self) -> bool {
        if !self.is_awaiting_acceptance() {
            return false;
        }
        let Self::AwaitingAcceptance { recall } = std::mem::take(self) else {
            unreachable!()
        };
        *self = Self::Recalling(recall);
        true
    }

    pub fn reset(&mut self) {
        *self = Self::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_and_rejecting_preserve_the_original_draft() {
        let first = TranscriptEditTarget::PromptOrdinal(0);
        let second = TranscriptEditTarget::PromptOrdinal(1);
        let mut state = EditState::default();
        assert!(state.recall(RecallEdit {
            saved_input: "unsent draft",
            target: first.clone(),
            target_index: 1,
        }));
        assert!(state.switch_target(second.clone(), 3));
        assert_eq!(state.recalled_target(), Some((second.clone(), 3)));
        assert_eq!(state.stage_submission(), Some(second.clone()));
        assert!(state.is_awaiting_acceptance());
        assert_eq!(state.active_target_index(), Some(3));
        assert!(state.recalled_target().is_none());
        assert!(state.cancel_recall().is_none());
        assert!(!state.switch_target(first, 1));
        assert!(state.reject());
        assert_eq!(
            state.cancel_recall(),
            Some(RecallEdit {
                saved_input: "unsent draft",
                target: second,
                target_index: 3,
            })
        );
        assert!(state.is_none());
    }

    #[test]
    fn only_durable_acceptance_consumes_a_staged_recall() {
        let recall = RecallEdit {
            saved_input: 42,
            target: TranscriptEditTarget::PromptOrdinal(0),
            target_index: 1,
        };
        let mut state = EditState::default();
        assert!(state.recall(recall.clone()));
        state.accept();
        assert!(state.is_recalling());
        assert!(!state.recall(recall.clone()));
        assert_eq!(state.stage_submission(), Some(recall.target.clone()));
        assert!(state.stage_submission().is_none());
        state.accept();
        assert!(state.is_none() && !state.reject());
        assert!(state.recall(recall));
        state.reset();
        assert!(state.is_none());
    }
}
