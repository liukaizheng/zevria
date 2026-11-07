//! Authoritative Plan workflow projection and modal decision state.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanDialogCommand {
    Close,
    Cancel,
    Up,
    Down,
    PlanCurrent,
    PlanFresh,
    PlanRevise,
    Copy,
    Confirm,
    Other,
}
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanResolution;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;

/// One actionable row of the Plan decision dialog.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlanChoice {
    Implement,
    ImplementFresh,
    #[default]
    Revise,
}

/// The mutually exclusive initial and recovery dialogs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanDialogState {
    Initial { choice: PlanChoice },
    Recovery { choice: PlanChoice },
}

impl PlanDialogState {
    pub const fn choice(self) -> PlanChoice {
        match self {
            Self::Initial { choice } | Self::Recovery { choice } => choice,
        }
    }

    pub const fn recovering(self) -> bool {
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
pub enum PlanIntent {
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
pub struct WorkflowState {
    snapshot: PlanWorkflowState,
    dialog: Option<PlanDialogState>,
    hidden: bool,
    choice: PlanChoice,
}

impl WorkflowState {
    pub const fn snapshot(&self) -> &PlanWorkflowState {
        &self.snapshot
    }

    pub const fn dialog(&self) -> Option<PlanDialogState> {
        self.dialog
    }

    pub fn dialog_artifact(&self) -> Option<&PlanArtifact> {
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

    pub fn submitted_artifact(&self) -> Option<&PlanArtifact> {
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

    pub fn fresh_retry_version(&self) -> Option<PlanVersion> {
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

    pub fn has_unsubmitted_draft(&self) -> bool {
        matches!(
            &self.snapshot,
            PlanWorkflowState::Planning {
                previous: Some(_),
                draft: Some(_),
                ..
            }
        )
    }

    /// Only a working-copy change in the same revision cycle is draft-only.
    /// Rewinds can replace Planning with a different retained artifact or id.
    pub fn is_draft_update(&self, snapshot: &PlanWorkflowState) -> bool {
        matches!((&self.snapshot, snapshot), (
            PlanWorkflowState::Planning { id: old_id, previous: old_previous, .. },
            PlanWorkflowState::Planning { id, previous, .. },
        ) if old_id == id && old_previous == previous)
    }

    /// Identical authority does not reset local visibility or selection.
    pub fn apply_snapshot(&mut self, snapshot: PlanWorkflowState) -> bool {
        if self.snapshot == snapshot {
            return false;
        }
        if self.is_draft_update(&snapshot) {
            self.snapshot = snapshot;
            return true;
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

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn hide(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            self.choice = dialog.choice();
        }
        self.hidden = true;
    }

    pub fn open_review(&mut self) -> bool {
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
    pub fn restore_dialog_after_failure(&mut self) {
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

    /// Reduce dialog-only keys. Conversation scrolling remains coordinated by
    /// `App`; `decision_pending` disables every confirmation/close path while
    /// retaining choice navigation and copy for review.
    pub fn handle_dialog_command(
        &mut self,
        action: PlanDialogCommand,
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

        if matches!(action, PlanDialogCommand::Close | PlanDialogCommand::Cancel) {
            self.hide();
            return PlanIntent::Close;
        }
        match action {
            PlanDialogCommand::Up => {
                dialog.set_choice(match dialog.choice() {
                    PlanChoice::Implement => PlanChoice::Implement,
                    PlanChoice::ImplementFresh => PlanChoice::Implement,
                    PlanChoice::Revise => PlanChoice::ImplementFresh,
                });
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            PlanDialogCommand::Down => {
                dialog.set_choice(match dialog.choice() {
                    PlanChoice::Implement => PlanChoice::ImplementFresh,
                    PlanChoice::ImplementFresh => PlanChoice::Revise,
                    PlanChoice::Revise => PlanChoice::Revise,
                });
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            PlanDialogCommand::PlanCurrent => {
                dialog.set_choice(PlanChoice::Implement);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            PlanDialogCommand::PlanFresh => {
                dialog.set_choice(PlanChoice::ImplementFresh);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            PlanDialogCommand::PlanRevise => {
                dialog.set_choice(PlanChoice::Revise);
                self.choice = dialog.choice();
                self.dialog = Some(dialog);
                PlanIntent::None
            }
            PlanDialogCommand::Copy => PlanIntent::Copy(
                self.dialog_artifact()
                    .expect("review artifact")
                    .markdown
                    .clone(),
            ),
            PlanDialogCommand::Confirm if !decision_pending => match dialog.choice() {
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
