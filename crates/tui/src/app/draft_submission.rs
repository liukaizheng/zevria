//! Submission slots are independent of the live editor. A rejected submission
//! never replaces a newer draft and remains explicitly recoverable.

use crate::composer::{ComposerDraft, ComposerState};

#[derive(Default)]
pub(super) struct DraftSubmissions {
    pending: Option<StagedDraft>,
    rejected: Vec<ComposerDraft>,
}

struct StagedDraft {
    draft: ComposerDraft,
    replacement_generation: u64,
}

impl DraftSubmissions {
    pub(super) fn stage(&mut self, composer: &mut ComposerState) {
        assert!(
            self.pending.is_none(),
            "work admission must precede draft staging"
        );
        let draft = composer.snapshot();
        composer.clear();
        self.pending = Some(StagedDraft {
            draft,
            replacement_generation: composer.generation(),
        });
    }

    pub(super) fn accept(&mut self) {
        // Staging already created an independent replacement slot. Acceptance
        // has no right to mutate that slot, including its cursor and undo stack.
        self.pending = None;
    }

    pub(super) fn reject(&mut self, composer: &mut ComposerState) {
        if let Some(staged) = self.pending.take() {
            self.restore_or_retain(composer, staged.draft, staged.replacement_generation);
        }
    }

    pub(super) fn restore_or_retain(
        &mut self,
        composer: &mut ComposerState,
        draft: ComposerDraft,
        generation: u64,
    ) {
        if composer.generation() == generation && composer.is_empty() {
            composer.restore(draft);
        } else {
            self.rejected.push(draft);
        }
    }

    pub(super) fn retain(&mut self, draft: ComposerDraft) {
        self.rejected.push(draft);
    }

    pub(super) fn recover(&mut self, composer: &mut ComposerState) -> bool {
        if !composer.is_empty() {
            return false;
        }
        let Some(draft) = self.rejected.pop() else {
            return false;
        };
        composer.restore(draft);
        true
    }

    pub(super) fn has_recovery(&self) -> bool {
        !self.rejected.is_empty()
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
