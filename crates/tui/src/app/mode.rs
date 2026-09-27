//! Correlated, management-only workflow selection. No prompt or turn is staged.

use std::sync::atomic::{AtomicU64, Ordering};

use zevria_foundation::SessionMode;
use zevria_session_api::ModeSelectionResult;
use zevria_workflow::PlanWorkflowState;

use super::{App, OperationKind, UiAction, role_for_mode};
use crate::composer::ComposerDraft;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct PendingModeSelection {
    request_id: String,
    mode: SessionMode,
    command_draft: Option<(ComposerDraft, u64)>,
}

impl App {
    /// Restore the durable selection after `restore_plan_state`. A Ready Plan
    /// always remains in modal Plan approval, even if metadata disagrees.
    pub fn apply_selected_mode(&mut self, mode: SessionMode) {
        if !self.pane.is_root() {
            return;
        }
        self.view.invalidate_rendered_geometry();
        self.composer.close_edit_group();
        self.session.set_next_mode(
            if matches!(self.workflow.snapshot(), PlanWorkflowState::Ready { .. }) {
                SessionMode::Plan
            } else {
                mode
            },
        );
    }

    pub(crate) fn mode_selection_pending(&self) -> bool {
        self.session.pending_mode_selection.is_some()
    }

    pub(super) fn begin_mode_selection(
        &mut self,
        mode: SessionMode,
        command: bool,
    ) -> Option<UiAction> {
        if self.capabilities().manage_session.is_err()
            || self.workflow.dialog().is_some()
            || self.edit.is_recalling()
            || self.interaction.is_selecting()
            || self.mode_selection_pending()
        {
            return None;
        }
        let current = self.session.next_mode();
        if !self.session.begin_operation(
            OperationKind::ModeManagement,
            current,
            role_for_mode(current),
        ) {
            return None;
        }
        self.composer.close_edit_group();
        let command_draft = command.then(|| {
            let draft = self.composer.snapshot();
            self.composer.clear();
            (draft, self.composer.generation())
        });
        let request_id = format!("mode-{}", REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed));
        self.session.pending_mode_selection = Some(PendingModeSelection {
            request_id: request_id.clone(),
            mode,
            command_draft,
        });
        self.interaction.clear_chords();
        self.view.invalidate_rendered_geometry();
        Some(UiAction::SetMode { request_id, mode })
    }

    pub(crate) fn mode_selection_disconnected(&mut self) {
        if let Some(pending) = &self.session.pending_mode_selection {
            let request_id = pending.request_id.clone();
            self.accept_mode_selection(
                &request_id,
                ModeSelectionResult::Rejected {
                    code: "engine_stopped".into(),
                    message: "The session engine stopped before acknowledging mode selection."
                        .into(),
                },
            );
        }
    }

    pub(super) fn accept_mode_selection(&mut self, request_id: &str, result: ModeSelectionResult) {
        let Some(pending) = &self.session.pending_mode_selection else {
            return;
        };
        if !self.pane.is_root()
            || pending.request_id != request_id
            || matches!(&result, ModeSelectionResult::Accepted { mode, .. } if *mode != pending.mode)
        {
            return;
        }
        let pending = self
            .session
            .pending_mode_selection
            .take()
            .expect("correlated mode selection");
        self.session.finish_mode_management();
        match result {
            ModeSelectionResult::Accepted { mode, .. } => self.apply_selected_mode(mode),
            ModeSelectionResult::Rejected { code, message } => {
                if let Some((draft, generation)) = pending.command_draft {
                    self.drafts
                        .restore_or_retain(&mut self.composer, draft, generation);
                }
                self.push_error(format!("Mode selection rejected ({code}): {message}"));
            }
        }
    }
}
