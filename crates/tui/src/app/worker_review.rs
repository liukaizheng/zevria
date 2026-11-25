//! Pane-local worker request state. Workflow snapshots remain the authority;
//! frontend acknowledgements only settle their exact bound request/draft.

use super::draft_submission::DraftSubmissions;
use crate::composer::{ComposerDraft, ComposerState};
use zevria_workflow::ensemble_review::*;

pub(super) struct SubmissionHandoff {
    pub request_id: WorkerControlId,
    pub target: WorkerControlTarget,
    pub accepted_generation: u64,
}

#[derive(Default)]
pub(super) struct WorkerReviewUiState {
    pub bound: Option<(WorkerControlTarget, Box<WorkerReviewState>)>,
    pub pending: Option<(WorkerControl, ComposerDraft)>,
    pub urgent: Vec<(WorkerControl, ComposerDraft)>,
    pub handoff: Option<SubmissionHandoff>,
}

impl WorkerReviewUiState {
    pub(super) fn bind(
        &mut self,
        target: WorkerControlTarget,
        state: Box<WorkerReviewState>,
        composer: &mut ComposerState,
    ) -> bool {
        if self
            .bound
            .as_ref()
            .is_some_and(|(bound, _)| bound != &target)
        {
            self.retire();
            composer.cancel_paste();
        }
        if self.handoff.as_ref().is_some_and(|handoff| {
            handoff.target != target
                || state.inputs.accepted_generation >= handoff.accepted_generation
                || state.is_sealed()
                || state.is_abandoned()
        }) {
            self.handoff = None;
        }
        let frozen = state.is_sealed() || state.is_abandoned();
        if frozen {
            composer.cancel_paste();
        }
        self.bound = Some((target, state));
        frozen
    }

    pub(super) fn retire(&mut self) {
        self.pending = None;
        self.urgent.clear();
        self.handoff = None;
    }

    pub(super) fn busy(&self) -> bool {
        self.bound.as_ref().is_some_and(|(target, state)| {
            !state.quiescent()
                || self.pending.as_ref().is_some_and(|(control, _)| {
                    &control.target == target && is_work(&control.action)
                })
                || self
                    .handoff
                    .as_ref()
                    .is_some_and(|handoff| &handoff.target == target)
        })
    }

    pub(super) fn eligible(&self, action: &WorkerControlAction) -> bool {
        let Some((_, state)) = &self.bound else {
            return false;
        };
        if state.is_sealed() || state.is_abandoned() {
            return false;
        }
        let urgent = matches!(
            action,
            WorkerControlAction::Abandon | WorkerControlAction::CancelPrompt
        );
        if (!urgent && self.pending.is_some())
            || self
                .urgent
                .iter()
                .any(|(control, _)| &control.action == action)
        {
            return false;
        }
        match action {
            WorkerControlAction::SendFeedback { text } => !self.busy() && !text.is_blank(),
            WorkerControlAction::Retry => !self.busy(),
            WorkerControlAction::Confirm { expected_revision }
            | WorkerControlAction::Baseline { expected_revision } => {
                !self.busy()
                    && state
                        .eligible_snapshot()
                        .is_some_and(|plan| &plan.revision == expected_revision)
            }
            WorkerControlAction::Unconfirm { expected_revision } => state
                .proposals
                .confirmation
                .as_ref()
                .is_some_and(|receipt| &receipt.revision == expected_revision),
            WorkerControlAction::Unbaseline { expected_revision } => state
                .proposals
                .baseline
                .as_ref()
                .is_some_and(|receipt| &receipt.revision == expected_revision),
            // The pending submission covers cancellation before its generation
            // acknowledgement. Transport order binds it to this worker only.
            WorkerControlAction::CancelPrompt => {
                state.cancellable_generation().is_some() || self.handoff.is_some()
            }
            WorkerControlAction::Abandon => true,
        }
    }

    pub(super) fn admit(
        &mut self,
        action: WorkerControlAction,
        composer: &ComposerState,
    ) -> Option<WorkerControl> {
        if !self.eligible(&action) {
            return None;
        }
        let (target, state) = self.bound.as_ref()?;
        let control = WorkerControl {
            request_id: WorkerControlId::new(),
            target: target.clone(),
            action,
        };
        if is_work(&control.action) {
            self.handoff = Some(SubmissionHandoff {
                request_id: control.request_id.clone(),
                target: target.clone(),
                accepted_generation: state.inputs.accepted_generation.saturating_add(1),
            });
        }
        let draft = composer.snapshot();
        if matches!(
            control.action,
            WorkerControlAction::Abandon | WorkerControlAction::CancelPrompt
        ) {
            self.urgent.push((control.clone(), draft));
        } else {
            self.pending = Some((control.clone(), draft));
        }
        Some(control)
    }

    /// None means a stale/unrelated result, not permission to show its error.
    pub(super) fn settle(
        &mut self,
        result: &WorkerControlResult,
        composer: &mut ComposerState,
        drafts: &mut DraftSubmissions,
    ) -> Option<Option<String>> {
        // Accepted feedback may carry newly prepared image bytes. Correlate its
        // identity/target and ordered non-image content without decoding images
        // on the UI thread. All other receipts still require exact equality.
        let matches = |control: &WorkerControl| {
            control == &result.control
                || (result.accepted
                    && control.request_id == result.control.request_id
                    && control.target == result.control.target
                    && match (&control.action, &result.control.action) {
                        (
                            WorkerControlAction::SendFeedback { text: before },
                            WorkerControlAction::SendFeedback { text: after },
                        ) => {
                            before.blocks().len() == after.blocks().len()
                                && before.blocks().iter().zip(after.blocks()).all(|(a, b)| {
                                    match (a, b) {
                                        (
                                            zevria_content::PromptBlock::Text(a),
                                            zevria_content::PromptBlock::Text(b),
                                        ) => a == b,
                                        (
                                            zevria_content::PromptBlock::Image(_),
                                            zevria_content::PromptBlock::Image(_),
                                        ) => true,
                                        _ => false,
                                    }
                                })
                        }
                        _ => false,
                    })
        };
        let draft = if self
            .pending
            .as_ref()
            .is_some_and(|(control, _)| matches(control))
        {
            self.pending.take().map(|(_, draft)| draft)
        } else {
            self.urgent
                .iter()
                .position(|(control, _)| matches(control))
                .map(|index| self.urgent.remove(index).1)
        }?;
        if result.accepted {
            if composer.matches_draft(&draft) {
                composer.clear();
            }
            Some(None)
        } else {
            if !composer.matches_draft(&draft) {
                drafts.retain(draft);
            }
            if self.handoff.as_ref().is_some_and(|handoff| {
                handoff.request_id == result.control.request_id
                    && handoff.target == result.control.target
            }) {
                self.handoff = None;
            }
            Some(Some(result.detail.clone()))
        }
    }
}

fn is_work(action: &WorkerControlAction) -> bool {
    matches!(
        action,
        WorkerControlAction::SendFeedback { .. } | WorkerControlAction::Retry
    )
}
