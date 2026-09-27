use super::*;

/// Immutable identity, mode, and cancellation scope shared by a turn's
/// provider requests and tool calls.
#[derive(Clone)]
pub struct TurnContext {
    pub id: TurnId,
    pub mode: SessionMode,
    /// Copied from the effective turn policy; nominal mode is not authorization.
    pub build_subtasks: bool,
    pub(super) cancellation: CancellationToken,
}

impl TurnContext {
    pub fn new(id: TurnId, mode: SessionMode, cancellation: CancellationToken) -> Self {
        Self {
            id,
            mode,
            build_subtasks: false,
            cancellation,
        }
    }

    pub fn with_build_subtasks(mut self, enabled: bool) -> Self {
        self.build_subtasks = enabled;
        self
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl fmt::Debug for TurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnContext")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("build_subtasks", &self.build_subtasks)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}
