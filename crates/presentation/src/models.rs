//! Correlated, renderer-independent model/effort picker lifecycle.
//!
//! Frontends own filtering, focus, geometry and navigation. This state owns the
//! captured root mode/scope and all protocol identity, including cancellation.
use zevria_foundation::SessionMode;
use zevria_model::models::{
    ModelCandidate, ModelManagementRequest as Request, ModelManagementResult as Result,
    ModelSelection, ModelSelectionPreview, ModelSelectionScope, mode_role,
};
use zevria_session_api::{ManagementCommand, SessionCommand};

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionStep {
    Model,
    Reasoning { profile: usize, selected: usize },
}

#[derive(Default)]
pub enum ModelPhase {
    #[default]
    Settled,
    Loading,
    Selecting(SelectionStep),
    Conversion {
        preview: ModelSelectionPreview,
        back: SelectionStep,
    },
    PendingChange {
        target: ModelSelection,
        back: SelectionStep,
    },
    Cancelling {
        target: Option<ModelSelection>,
    },
    Error {
        message: String,
        back: SelectionStep,
    },
}
impl ModelPhase {
    pub fn step(&self) -> SelectionStep {
        match self {
            Self::Selecting(step) => *step,
            Self::Conversion { back, .. }
            | Self::PendingChange { back, .. }
            | Self::Error { back, .. } => *back,
            _ => SelectionStep::Model,
        }
    }
    fn target(&self) -> Option<&ModelSelection> {
        match self {
            Self::Conversion { preview, .. } => Some(&preview.target),
            Self::PendingChange { target, .. } => Some(target),
            Self::Cancelling { target } => target.as_ref(),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct ModelPicker {
    pub phase: ModelPhase,
    pub commands: crate::outbox::CommandOutbox,
    pub request_id: String,
    pub mode: Option<SessionMode>,
    pub scope: Option<ModelSelectionScope>,
    pub profiles: Vec<ModelCandidate>,
    pub current: Option<ModelSelection>,
    pub revision: String,
    settlement_notice: Option<String>,
}
impl ModelPicker {
    pub fn filter_indices(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.profiles
            .iter()
            .enumerate()
            .filter_map(|(index, profile)| {
                profile
                    .context
                    .profile
                    .to_string()
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect()
    }

    pub fn is_open(&self) -> bool {
        !matches!(self.phase, ModelPhase::Settled)
    }
    pub fn pending(&self) -> bool {
        matches!(
            self.phase,
            ModelPhase::Loading | ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
        )
    }
    pub fn preview(&self) -> Option<&ModelSelectionPreview> {
        match &self.phase {
            ModelPhase::Conversion { preview, .. } => Some(preview),
            _ => None,
        }
    }
    pub fn error(&self) -> Option<&str> {
        match &self.phase {
            ModelPhase::Error { message, .. } => Some(message),
            _ => None,
        }
    }
    /// Duplicate activation never replaces an outstanding request/cancellation.
    pub fn show(&mut self, mode: SessionMode, scope: ModelSelectionScope) -> bool {
        if self.is_open() {
            return false;
        }
        *self = Self {
            phase: ModelPhase::Loading,
            mode: Some(mode),
            scope: Some(scope),
            request_id: crate::outbox::RequestIdAllocator::allocate("model-"),
            ..Self::default()
        };
        self.send(Request::List { mode, scope });
        true
    }
    fn send(&mut self, request: Request) {
        self.commands
            .push_back(SessionCommand::Manage(ManagementCommand::Models {
                request_id: self.request_id.clone(),
                request,
            }));
    }
    pub fn cancel(&mut self) {
        if self.is_open() && !matches!(self.phase, ModelPhase::Cancelling { .. }) {
            self.phase = ModelPhase::Cancelling {
                target: self.phase.target().cloned(),
            };
            self.send(Request::Cancel);
        }
    }
    /// A cancellation can race a partially durable failure. The sheet may
    /// close, but the caller must surface this correlated outcome as a notice.
    pub fn take_settlement_notice(&mut self) -> Option<String> {
        self.settlement_notice.take()
    }
    /// Disconnect/session replacement retires request identity, not just the sheet.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn choose_model(&mut self, profile: usize) -> bool {
        if !self.is_open() || self.pending() || self.preview().is_some() {
            return false;
        }
        let Some(candidate) = self.profiles.get(profile) else {
            return false;
        };
        if candidate.reasoning_levels.is_empty() {
            return false;
        }
        let selected = self
            .current
            .as_ref()
            .and_then(|current| {
                candidate
                    .reasoning_levels
                    .iter()
                    .position(|level| *level == current.reasoning_level)
            })
            .unwrap_or(0);
        self.phase = ModelPhase::Selecting(SelectionStep::Reasoning { profile, selected });
        true
    }
    pub fn choose_effort(&mut self, selected: usize) -> bool {
        if self.pending() || self.preview().is_some() {
            return false;
        }
        let SelectionStep::Reasoning { profile, .. } = self.phase.step() else {
            return false;
        };
        if self
            .profiles
            .get(profile)
            .is_none_or(|p| selected >= p.reasoning_levels.len())
        {
            return false;
        }
        self.phase = ModelPhase::Selecting(SelectionStep::Reasoning { profile, selected });
        true
    }
    pub fn back(&mut self) {
        if self.is_open() && !self.pending() && self.preview().is_none() {
            self.phase = ModelPhase::Selecting(SelectionStep::Model);
        }
    }
    /// Effort selection is always explicit. A real conversion needs its own
    /// subsequent confirmation using the exact engine-issued preview.
    pub fn apply(&mut self) -> bool {
        if self.pending() || self.preview().is_some() {
            return false;
        }
        let SelectionStep::Reasoning { profile, selected } = self.phase.step() else {
            return false;
        };
        let Some(candidate) = self.profiles.get(profile) else {
            return false;
        };
        let Some(level) = candidate.reasoning_levels.get(selected) else {
            return false;
        };
        let target = ModelSelection::new(candidate.context.profile.clone(), *level);
        self.phase = ModelPhase::PendingChange {
            target: target.clone(),
            back: SelectionStep::Reasoning { profile, selected },
        };
        self.send(Request::Select {
            mode: self.mode.expect("open mode"),
            scope: self.scope.expect("open scope"),
            target,
            revision: self.revision.clone(),
        });
        true
    }
    pub fn confirm_conversion(&mut self) -> bool {
        let Some(preview) = self.preview().cloned() else {
            return false;
        };
        self.phase = ModelPhase::PendingChange {
            target: preview.target.clone(),
            back: self.phase.step(),
        };
        self.send(Request::Confirm { preview });
        true
    }
    /// Only a correlated result may authorize root metadata installation. Late
    /// catalog/preview results cannot reopen cancellation; a committed success
    /// racing cancellation must still be installed by the caller.
    pub fn accept(&mut self, request_id: &str, result: &Result) -> bool {
        if !self.is_open() || self.request_id != request_id {
            return false;
        }
        let correlated = match result {
            Result::Catalog { mode, scope, .. } => {
                matches!(
                    self.phase,
                    ModelPhase::Loading | ModelPhase::Cancelling { .. }
                ) && Some(*mode) == self.mode
                    && Some(*scope) == self.scope
            }
            Result::ConfirmationRequired(preview) => {
                matches!(
                    self.phase,
                    ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
                ) && preview.request_id == self.request_id
                    && Some(preview.mode) == self.mode
                    && Some(preview.scope) == self.scope
                    && preview.revision == self.revision
                    && self.phase.target() == Some(&preview.target)
            }
            Result::Changed {
                role,
                scope,
                context,
                reasoning_level,
                ..
            } => {
                matches!(
                    self.phase,
                    ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
                ) && self.mode.map(mode_role) == Some(*role)
                    && Some(*scope) == self.scope
                    && self.phase.target().is_some_and(|target| {
                        target.profile == context.profile
                            && target.reasoning_level == *reasoning_level
                    })
            }
            Result::Cancelled | Result::Rejected { .. } => true,
        };
        if !correlated {
            return false;
        }
        let cancelling = matches!(self.phase, ModelPhase::Cancelling { .. });
        match result {
            Result::Catalog {
                current,
                profiles,
                revision,
                ..
            } => {
                self.profiles = profiles.clone();
                self.current = Some(current.clone());
                self.revision = revision.clone();
                if !cancelling {
                    self.phase = ModelPhase::Selecting(SelectionStep::Model);
                }
            }
            Result::ConfirmationRequired(preview) => {
                if !cancelling {
                    self.phase = ModelPhase::Conversion {
                        preview: preview.clone(),
                        back: self.phase.step(),
                    };
                }
            }
            Result::Changed { .. } | Result::Cancelled => self.phase = ModelPhase::Settled,
            Result::Rejected {
                message,
                current_revision,
                checkpoint_installed,
                ..
            } => {
                if cancelling && (*checkpoint_installed || current_revision.is_some()) {
                    self.settlement_notice = Some(message.clone());
                }
                if let Some(revision) = current_revision
                    && self.scope == Some(ModelSelectionScope::SessionAndDefault)
                {
                    self.revision = revision.clone();
                }
                self.phase = if cancelling {
                    ModelPhase::Settled
                } else {
                    ModelPhase::Error {
                        message: message.clone(),
                        back: self.phase.step(),
                    }
                };
            }
        }
        true
    }
    pub fn scope_label(&self) -> &'static str {
        match self.scope {
            Some(ModelSelectionScope::SessionOnly) => {
                "This session only. Kept on /new, fresh handoff and resume; config unchanged."
            }
            Some(ModelSelectionScope::SessionAndDefault) => {
                "This session and global default. Kept on /new, fresh handoff and resume; updates config."
            }
            None => "",
        }
    }
}
