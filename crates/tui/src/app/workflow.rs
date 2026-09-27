//! Authoritative Plan workflow projection and modal decision state.

use crate::input::Action;
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanResolution;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;

/// One actionable row of the Plan decision dialog.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PlanChoice {
    Implement,
    ImplementFresh,
    #[default]
    Revise,
}

/// The mutually exclusive initial and recovery dialogs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanDialogState {
    Initial { choice: PlanChoice },
    Recovery { choice: PlanChoice },
}

impl PlanDialogState {
    pub(crate) const fn choice(self) -> PlanChoice {
        match self {
            Self::Initial { choice } | Self::Recovery { choice } => choice,
        }
    }

    pub(crate) const fn recovering(self) -> bool {
        matches!(self, Self::Recovery { .. })
    }

    fn set_choice(&mut self, choice: PlanChoice) {
        match self {
            Self::Initial {
                choice: current_choice,
            }
            | Self::Recovery {
                choice: current_choice,
            } => *current_choice = choice,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlanIntent {
    None,
    Copy(String),
    Close,
    Decide {
        expected: PlanVersion,
        decision: PlanDecision,
    },
}

/// Engine-owned workflow snapshot plus the optional local dialog projection.
#[derive(Debug, Default)]
pub(crate) struct WorkflowState {
    snapshot: PlanWorkflowState,
    dialog: Option<PlanDialogState>,
    hidden: bool,
    choice: PlanChoice,
}

impl WorkflowState {
    pub(crate) const fn snapshot(&self) -> &PlanWorkflowState {
        &self.snapshot
    }

    pub(crate) const fn dialog(&self) -> Option<PlanDialogState> {
        self.dialog
    }

    pub(crate) fn dialog_artifact(&self) -> Option<&PlanArtifact> {
        match (self.dialog, &self.snapshot) {
            (Some(PlanDialogState::Initial { .. }), PlanWorkflowState::Ready { artifact }) => {
                Some(artifact)
            }
            (
                Some(PlanDialogState::Recovery { .. }),
                PlanWorkflowState::Planning {
                    previous: Some(artifact),
                    ..
                },
            ) => Some(artifact),
            _ => None,
        }
    }

    pub(crate) fn submitted_artifact(&self) -> Option<&PlanArtifact> {
        match &self.snapshot {
            PlanWorkflowState::Ready { artifact }
            | PlanWorkflowState::Published { artifact }
            | PlanWorkflowState::Planning {
                previous: Some(artifact),
                ..
            } => Some(artifact),
            PlanWorkflowState::Idle
            | PlanWorkflowState::Planning { previous: None, .. }
            | PlanWorkflowState::Resolved { .. } => None,
        }
    }

    pub(crate) fn fresh_retry_version(&self) -> Option<PlanVersion> {
        match &self.snapshot {
            PlanWorkflowState::Resolved {
                artifact,
                resolution: PlanResolution::ImplementedFresh,
            } => Some(artifact.version),
            PlanWorkflowState::Idle
            | PlanWorkflowState::Planning { .. }
            | PlanWorkflowState::Ready { .. }
            | PlanWorkflowState::Published { .. }
            | PlanWorkflowState::Resolved { .. } => None,
        }
    }

    /// Identical authority does not reset local visibility or selection.
    pub(crate) fn apply_snapshot(&mut self, snapshot: PlanWorkflowState) -> bool {
        if self.snapshot == snapshot {
            return false;
        }
        let old_version = self.submitted_artifact().map(|artifact| artifact.version);
        let new_version = match &snapshot {
            PlanWorkflowState::Ready { artifact }
            | PlanWorkflowState::Published { artifact }
            | PlanWorkflowState::Planning {
                previous: Some(artifact),
                ..
            } => Some(artifact.version),
            _ => None,
        };
        if new_version != old_version {
            self.hidden = false;
            self.choice = PlanChoice::Revise;
        }
        self.dialog = if matches!(snapshot, PlanWorkflowState::Ready { .. }) && !self.hidden {
            Some(PlanDialogState::Initial {
                choice: self.choice,
            })
        } else {
            None
        };
        self.snapshot = snapshot;
        true
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn hide(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            self.choice = dialog.choice();
        }
        self.hidden = true;
    }

    pub(crate) fn open_review(&mut self) -> bool {
        let dialog = match &self.snapshot {
            PlanWorkflowState::Ready { .. } => PlanDialogState::Initial {
                choice: self.choice,
            },
            PlanWorkflowState::Planning {
                previous: Some(_), ..
            } => PlanDialogState::Recovery {
                choice: self.choice,
            },
            _ => return false,
        };
        self.hidden = false;
        self.dialog = Some(dialog);
        true
    }

    /// Restore the dialog appropriate to the still-authoritative snapshot
    /// after a Plan decision fails before a replacing snapshot arrives.
    pub(crate) fn restore_dialog_after_failure(&mut self) {
        if self.hidden {
            return;
        }
        match &self.snapshot {
            PlanWorkflowState::Ready { .. } => {
                self.dialog.get_or_insert(PlanDialogState::Initial {
                    choice: self.choice,
                });
            }
            PlanWorkflowState::Planning {
                previous: Some(_), ..
            } => {
                self.dialog.get_or_insert(PlanDialogState::Recovery {
                    choice: self.choice,
                });
            }
            PlanWorkflowState::Idle
            | PlanWorkflowState::Planning { previous: None, .. }
            | PlanWorkflowState::Published { .. }
            | PlanWorkflowState::Resolved { .. } => {}
        }
    }

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

    /// Reduce dialog-only keys. Conversation scrolling remains coordinated by
    /// `App`; `decision_pending` disables every confirmation/close path while
    /// retaining choice navigation and copy for review.
    pub(crate) fn handle_dialog_action(
        &mut self,
        action: Action,
        decision_pending: bool,
    ) -> PlanIntent {
        let Some(mut dialog) = self.dialog else {
            return PlanIntent::None;
        };
        let artifact = match self.dialog_artifact() {
            Some(artifact) => artifact,
            None => {
                self.dialog = None;
                return PlanIntent::None;
            }
        };
        let expected = artifact.version;

        if matches!(action, Action::Close | Action::Cancel) {
            self.hide();
            return PlanIntent::Close;
        }
        match action {
            Action::Up => {
                dialog.set_choice(match dialog.choice() {
                    PlanChoice::Implement => PlanChoice::Implement,
                    PlanChoice::ImplementFresh => PlanChoice::Implement,
                    PlanChoice::Revise => PlanChoice::ImplementFresh,
                });
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            Action::Down => {
                dialog.set_choice(match dialog.choice() {
                    PlanChoice::Implement => PlanChoice::ImplementFresh,
                    PlanChoice::ImplementFresh => PlanChoice::Revise,
                    PlanChoice::Revise => PlanChoice::Revise,
                });
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            Action::PlanCurrent => {
                dialog.set_choice(PlanChoice::Implement);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            Action::PlanFresh => {
                dialog.set_choice(PlanChoice::ImplementFresh);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            Action::PlanRevise => {
                dialog.set_choice(PlanChoice::Revise);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            Action::Copy => PlanIntent::Copy(
                self.dialog_artifact()
                    .expect("review artifact")
                    .markdown
                    .clone(),
            ),
            Action::Confirm if !decision_pending => match dialog.choice() {
                PlanChoice::Implement => PlanIntent::Decide {
                    expected,
                    decision: PlanDecision::ImplementCurrent,
                },
                PlanChoice::ImplementFresh => PlanIntent::Decide {
                    expected,
                    decision: PlanDecision::ImplementFresh,
                },
                PlanChoice::Revise if dialog.recovering() => {
                    self.hide();
                    PlanIntent::Close
                }
                PlanChoice::Revise => PlanIntent::Decide {
                    expected,
                    decision: PlanDecision::Revise,
                },
            },
            _ => PlanIntent::None,
        }
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
