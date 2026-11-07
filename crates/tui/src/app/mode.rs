//! Terminal-only draft hooks for correlated shared mode selection.
use super::{App, UiAction};
use zevria_foundation::SessionMode;
use zevria_session_api::ManagementCommand;
#[cfg(test)]
use zevria_session_api::ModeSelectionResult;

impl App {
    pub fn apply_selected_mode(&mut self, mode: SessionMode) {
        if !self.pane.is_root() {
            return;
        }
        self.view.invalidate_rendered_geometry();
        self.composer.close_edit_group();
        self.model.apply_selected_mode(mode);
    }

    pub(crate) fn mode_selection_pending(&self) -> bool {
        self.model.session.pending_mode_selection.is_some()
    }

    pub(super) fn begin_mode_selection(
        &mut self,
        mode: SessionMode,
        command: bool,
    ) -> Option<UiAction> {
        if self.capabilities().manage_session.is_err()
            || self.model.workflow.dialog().is_some()
            || self.edit.is_recalling()
            || self.interaction.is_selecting()
            || self.mode_selection_pending()
        {
            return None;
        }
        let ManagementCommand::SetMode { request_id, mode } = self.model.select_mode(mode)? else {
            return None;
        };
        self.composer.close_edit_group();
        self.mode_command_draft = command.then(|| {
            let draft = self.composer.snapshot();
            self.composer.clear();
            (draft, self.composer.generation())
        });
        self.interaction.clear_chords();
        self.view.invalidate_rendered_geometry();
        Some(UiAction::SetMode { request_id, mode })
    }

    pub(crate) fn mode_selection_disconnected(&mut self) {
        // Ordinary lifecycle disconnect handling remains in the runtime; here
        // only a pending management command needs its terminal draft hook.
        if self.mode_selection_pending() {
            let reduction = self.model.disconnected();
            self.apply_reduction(reduction);
        }
    }

    #[cfg(test)]
    pub(super) fn accept_mode_selection(&mut self, request_id: &str, result: ModeSelectionResult) {
        let reduction = self
            .model
            .reduce(zevria_session_api::SessionEvent::ModeResult {
                request_id: request_id.into(),
                result,
            });
        self.apply_reduction(reduction);
    }
}
