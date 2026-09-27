//! Transcript recall/edit lifecycle and durable acceptance boundary.

use zevria_session_api::TranscriptEditTarget;

#[derive(Debug, Clone)]
pub(crate) struct RecallEdit {
    pub(crate) saved_input: crate::composer::ComposerDraft,
    pub(crate) target: TranscriptEditTarget,
    pub(crate) target_index: usize,
}

#[derive(Debug, Default)]
pub(crate) enum EditState {
    #[default]
    None,
    Recalling(RecallEdit),
    AwaitingAcceptance {
        recall: RecallEdit,
        target_index: usize,
        compacted: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedEdit {
    pub(crate) target_index: usize,
    pub(crate) compacted: bool,
}

impl EditState {
    pub(crate) const fn is_recalling(&self) -> bool {
        matches!(self, Self::Recalling(_))
    }

    pub(crate) const fn is_awaiting_acceptance(&self) -> bool {
        matches!(self, Self::AwaitingAcceptance { .. })
    }

    pub(crate) const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub(crate) fn recall(&mut self, edit: RecallEdit) -> bool {
        if !self.is_none() {
            return false;
        }
        *self = Self::Recalling(edit);
        true
    }

    #[cfg(test)]
    pub(crate) fn recalling(&self) -> Option<&RecallEdit> {
        match self {
            Self::Recalling(edit) => Some(edit),
            Self::None | Self::AwaitingAcceptance { .. } => None,
        }
    }

    pub(crate) fn cancel_recall(&mut self) -> Option<RecallEdit> {
        let state = std::mem::take(self);
        match state {
            Self::Recalling(edit) => Some(edit),
            state @ (Self::None | Self::AwaitingAcceptance { .. }) => {
                *self = state;
                None
            }
        }
    }

    /// Move a recalled target behind the engine's durable acceptance
    /// boundary. The conversation remains unchanged until `accept`.
    pub(crate) fn stage_submission(&mut self) -> Option<TranscriptEditTarget> {
        let state = std::mem::take(self);
        let Self::Recalling(edit) = state else {
            *self = state;
            return None;
        };
        let target = edit.target.clone();
        *self = Self::AwaitingAcceptance {
            target_index: edit.target_index,
            compacted: false,
            recall: edit,
        };
        Some(target)
    }

    pub(crate) fn mark_automatic_pre_turn_compaction(&mut self) -> bool {
        let Self::AwaitingAcceptance { compacted, .. } = self else {
            return false;
        };
        *compacted = true;
        true
    }

    pub(crate) fn accept(&mut self) -> Option<AcceptedEdit> {
        let state = std::mem::take(self);
        match state {
            Self::AwaitingAcceptance {
                target_index,
                compacted,
                ..
            } => Some(AcceptedEdit {
                target_index,
                compacted,
            }),
            state @ (Self::None | Self::Recalling(_)) => {
                *self = state;
                None
            }
        }
    }

    /// Reject or cancel an edit that never crossed durable acceptance.
    pub(crate) fn reject(&mut self) -> bool {
        if !self.is_awaiting_acceptance() {
            return false;
        }
        let Self::AwaitingAcceptance { recall, .. } = std::mem::take(self) else {
            unreachable!()
        };
        *self = Self::Recalling(recall);
        true
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_and_pending_edit_cannot_coexist() {
        let target = TranscriptEditTarget::PromptOrdinal(0);
        let mut state = EditState::default();
        assert!(state.recall(RecallEdit {
            saved_input: {
                let mut composer = crate::composer::ComposerState::default();
                composer.insert_text("draft");
                composer.snapshot()
            },
            target: target.clone(),
            target_index: 2,
        }));
        assert_eq!(state.stage_submission(), Some(target));
        assert!(state.is_awaiting_acceptance());
        assert!(state.mark_automatic_pre_turn_compaction());
        assert_eq!(
            state.accept(),
            Some(AcceptedEdit {
                target_index: 2,
                compacted: true,
            })
        );
        assert!(state.is_none());
    }
}
