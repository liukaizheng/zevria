//! Durable ordinary-root selection. This operation never contacts a provider.
use super::*;

impl<P: ModelProvider> SessionEngine<P> {
    /// Explicit composition-root grant, never installed on children or workers.
    pub fn with_mode_management(mut self) -> Self {
        self.capabilities.mode_management = true;
        self
    }

    /// Saved selection is authoritative except for the modal Ready lock.
    /// Supported metadata-free histories use the Plan reducer, never prose or
    /// the last workflow directive, for their startup fallback.
    pub fn selected_mode(&self) -> SessionMode {
        if matches!(self.plan_state(), Ok(PlanWorkflowState::Ready { .. })) {
            return SessionMode::Plan;
        }
        if let Some(mode) = self
            .instruction_replay()
            .ok()
            .and_then(|state| state.persisted_mode())
        {
            return mode;
        }
        match self.plan_state() {
            Ok(PlanWorkflowState::Planning { .. } | PlanWorkflowState::Published { .. }) => {
                SessionMode::Plan
            }
            _ => SessionMode::Build,
        }
    }

    pub(super) fn invalidate_mode_accounting(&mut self) {
        self.invalidate_model_preview();
        self.context.invalidate_prepared_count();
        self.context.last_snapshots.clear();
        self.reestimate_context_usage();
    }

    pub(super) async fn manage_mode(
        &mut self,
        request_id: String,
        mode: SessionMode,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        let result = if !matches!(self.phase, EnginePhase::Idle) {
            ModeSelectionResult::rejected(
                "busy",
                "mode changes require an idle session; request was not queued",
            )
        } else if !self.capabilities.mode_management {
            ModeSelectionResult::rejected(
                "unsupported_profile",
                "mode changes are only available in ordinary root sessions",
            )
        } else if self.conversation.is_read_only() {
            ModeSelectionResult::rejected(
                "read_only",
                "mode changes are unavailable in a read-only session",
            )
        } else if self.conversation.persistence_error().is_some() {
            ModeSelectionResult::rejected(
                "persistence_degraded",
                "repair session transcript persistence before selecting a mode",
            )
        } else if matches!(self.plan_state()?, PlanWorkflowState::Ready { .. }) {
            ModeSelectionResult::rejected(
                "plan_ready",
                "the Ready Plan requires an explicit revision or implementation decision; selecting a mode cannot approve it",
            )
        } else if self.selected_mode() == mode {
            ModeSelectionResult::Accepted {
                mode,
                changed: false,
            }
        } else {
            match self.conversation.replace_session_mode(mode) {
                Ok(changed) => {
                    self.refresh_replay()?;
                    self.invalidate_mode_accounting();
                    ModeSelectionResult::Accepted { mode, changed }
                }
                Err(error) => ModeSelectionResult::rejected(
                    "save_failed",
                    format!("could not save {mode} selection; mode was not changed: {error:#}"),
                ),
            }
        };
        let _ = events
            .send(SessionEvent::ModeResult { request_id, result })
            .await;
        Ok(())
    }

    pub(super) async fn emit_selected_mode(&mut self, events: &SessionEventSender) {
        if self.capabilities.mode_management {
            let _ = events
                .send(SessionEvent::ModeChanged {
                    mode: self.selected_mode(),
                })
                .await;
        }
    }
}
