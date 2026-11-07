use crate::input::Action;
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
pub(crate) use zevria_presentation::workflow::*;
#[cfg(test)]
use zevria_workflow::{PlanArtifact, PlanVersion, PlanWorkflowState};
pub(crate) trait WorkflowKeys {
    fn handle_dialog_action(&mut self, action: Action, decision_pending: bool) -> PlanIntent;
    #[cfg(test)]
    fn handle_dialog_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        pending: bool,
    ) -> PlanIntent {
        crate::input::ChordState::default()
            .resolve(
                crate::input::KeyContext::PlanDecision,
                ratatui::crossterm::event::KeyEvent::new(code, modifiers),
            )
            .map_or(PlanIntent::None, |action| {
                self.handle_dialog_action(action, pending)
            })
    }
}
impl WorkflowKeys for WorkflowState {
    fn handle_dialog_action(&mut self, action: Action, decision_pending: bool) -> PlanIntent {
        let command = match action {
            Action::Close => PlanDialogCommand::Close,
            Action::Cancel => PlanDialogCommand::Cancel,
            Action::Up => PlanDialogCommand::Up,
            Action::Down => PlanDialogCommand::Down,
            Action::PlanCurrent => PlanDialogCommand::PlanCurrent,
            Action::PlanFresh => PlanDialogCommand::PlanFresh,
            Action::PlanRevise => PlanDialogCommand::PlanRevise,
            Action::Copy => PlanDialogCommand::Copy,
            Action::Confirm => PlanDialogCommand::Confirm,
            _ => PlanDialogCommand::Other,
        };
        self.handle_dialog_command(command, decision_pending)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::TurnId;
    use zevria_workflow::PlanId;

    fn artifact() -> PlanArtifact {
        PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 2,
            },
            title: "Plan".to_string(),
            markdown: "# Plan".to_string(),
            source_turn_id: TurnId::new(1),
        }
    }

    #[test]
    fn ready_and_recovery_dialogs_are_exclusive() {
        let artifact = artifact();
        let mut state = WorkflowState::default();
        state.apply_snapshot(PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        });
        assert!(matches!(
            state.dialog(),
            Some(PlanDialogState::Initial { .. })
        ));
        state.apply_snapshot(PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact),
            draft: None,
        });
        assert_eq!(state.dialog(), None);
        assert!(state.open_review());
        assert!(matches!(
            state.dialog(),
            Some(PlanDialogState::Recovery { .. })
        ));
    }

    #[test]
    fn pending_decision_cannot_be_confirmed_twice() {
        let artifact = artifact();
        let mut state = WorkflowState::default();
        state.apply_snapshot(PlanWorkflowState::Ready { artifact });
        assert_eq!(
            state.handle_dialog_key(KeyCode::Enter, KeyModifiers::empty(), true),
            PlanIntent::None
        );
        assert!(matches!(
            state.dialog(),
            Some(PlanDialogState::Initial { .. })
        ));
    }
}
