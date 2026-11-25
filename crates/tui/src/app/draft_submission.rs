//! Submission slots are independent of the live editor. A rejected submission
//! never replaces a newer draft and remains explicitly recoverable.

use crate::composer::{ComposerDraft, ComposerState};

use zevria_presentation::draft::DraftSlot;

#[derive(Default)]
pub(super) struct DraftSubmissions(DraftSlot<ComposerDraft>);

impl DraftSubmissions {
    pub(super) fn stage(&mut self, composer: &mut ComposerState) {
        assert!(
            !self.0.has_pending(),
            "work admission must precede draft staging"
        );
        let draft = composer.snapshot();
        composer.clear();
        self.stage_detached(draft, composer.generation());
    }

    pub(super) fn stage_detached(&mut self, draft: ComposerDraft, replacement_generation: u64) {
        self.0.stage(draft, replacement_generation);
    }

    pub(super) fn has_pending(&self) -> bool {
        self.0.has_pending()
    }

    pub(super) fn accept(&mut self) {
        // Staging already created an independent replacement slot. Acceptance
        // has no right to mutate that slot, including its cursor and undo stack.
        self.0.accept();
    }

    pub(super) fn reject(&mut self, composer: &mut ComposerState) {
        if let Some(staged) = self.0.take() {
            self.restore_or_retain(composer, staged.draft, staged.generation);
        }
    }

    pub(super) fn restore_or_retain(
        &mut self,
        composer: &mut ComposerState,
        draft: ComposerDraft,
        generation: u64,
    ) {
        if let Some(draft) = self.0.restore_or_retain(
            draft,
            generation,
            composer.generation(),
            composer.is_empty(),
        ) {
            composer.restore(draft);
        }
    }

    pub(super) fn retain(&mut self, draft: ComposerDraft) {
        self.0.retain(draft);
    }

    pub(super) fn recover(&mut self, composer: &mut ComposerState) -> bool {
        if !composer.is_empty() {
            return false;
        }
        let Some(draft) = self.0.recover() else {
            return false;
        };
        composer.restore(draft);
        true
    }

    pub(super) fn has_recovery(&self) -> bool {
        !self.0.recovery().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_submission_retains_both_drafts_and_history() {
        let mut composer = ComposerState::default();
        composer.insert_text("submitted");
        let mut submissions = DraftSubmissions::default();
        submissions.stage(&mut composer);
        composer.insert_text("newer");
        submissions.reject(&mut composer);
        assert_eq!(composer.text(), "newer");
        assert!(!submissions.recover(&mut composer));
        composer.clear();
        assert!(submissions.recover(&mut composer));
        assert_eq!(composer.text(), "submitted");
        assert!(composer.undo());
        assert!(composer.is_empty());
    }

    #[test]
    fn editing_and_undoing_does_not_revive_replacement_token() {
        let mut composer = ComposerState::default();
        composer.insert_text("submitted");
        let mut submissions = DraftSubmissions::default();
        submissions.stage(&mut composer);
        composer.insert_text("later");
        composer.undo();
        submissions.reject(&mut composer);
        assert!(composer.is_empty());
        assert!(submissions.has_recovery());
    }
}
