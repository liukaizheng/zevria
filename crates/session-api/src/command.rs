//! Command orchestration.

use super::*;
use crate::UserPrompt;

/// Stable address of a transcript row whose complete tail can be revised.
///
/// Prompt ordinals are zero-based over exactly
/// `TranscriptItem::Message` values containing a user message and
/// `TranscriptItem::SkillInvocation` values. Ensemble starts never enter
/// prompt numbering; they are addressed by their durable run ID instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEditTarget {
    PromptOrdinal(usize),
    EnsembleRun(EnsembleRunId),
}

/// Typed replacement installed at a resolved transcript edit target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEditReplacement {
    Message {
        text: UserPrompt,
        mode: SessionMode,
        behavior: zevria_foundation::RequestBehavior,
    },
    Skill {
        name: SkillName,
        args: UserPrompt,
        mode: SessionMode,
    },
    Ensemble {
        workflow: EnsembleWorkflow,
        prompt: UserPrompt,
    },
}

impl TranscriptEditReplacement {
    pub const fn mode(&self) -> SessionMode {
        match self {
            Self::Message { mode, .. } | Self::Skill { mode, .. } => *mode,
            Self::Ensemble { workflow, .. } => match workflow {
                EnsembleWorkflow::Plan => SessionMode::Plan,
                EnsembleWorkflow::Review => SessionMode::Build,
            },
        }
    }
}

/// One target-independent transcript-tail revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEdit {
    pub target: TranscriptEditTarget,
    pub replacement: TranscriptEditReplacement,
}

/// Commands classified by lifecycle rather than an optional mode.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    Turn(TurnCommand),
    Control(ControlCommand),
    Manage(ManagementCommand),
}

/// Work that allocates a turn identity and queues FIFO while busy.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnCommand {
    Submit {
        text: UserPrompt,
        mode: SessionMode,
        behavior: zevria_foundation::RequestBehavior,
    },
    /// Atomically replace a recalled prompt row or ensemble start and its
    /// complete tail with an ordinary message, typed skill invocation, or
    /// fresh ensemble run.
    EditTranscript(TranscriptEdit),
    /// Apply a named skill with optional arguments. First use commits a typed
    /// activation plus a bodyless invocation; later uses commit only another
    /// invocation. [`SessionEvent::TurnStarted`] carries the compact
    /// `$name [args]` display form.
    InvokeSkill {
        name: SkillName,
        args: UserPrompt,
        mode: SessionMode,
    },
    /// Validate name resolution and capacity before changing a Ready Plan, then run the typed
    /// revision input under the same tracked turn and cancellation scope.
    RevisePlanWithSkill {
        expected: PlanVersion,
        name: SkillName,
        args: UserPrompt,
    },
    /// Create a context checkpoint without adding a synthetic user command
    /// to the transcript.
    Compact { mode: SessionMode },
    /// Run independent ACP workers and synthesize their reports through the
    /// visible root model. This command is frontend-only and is never exposed
    /// as a model-callable tool.
    RunEnsemble {
        workflow: EnsembleWorkflow,
        prompt: UserPrompt,
    },
    /// Resolve the current Ready artifact, or explicitly implement the last
    /// submitted artifact retained while revising. `expected` prevents a
    /// delayed frontend action from applying to a newer revision.
    ResolvePlan {
        expected: PlanVersion,
        decision: PlanDecision,
    },
    /// Begin a fresh session from an engine-generated typed handoff.
    StartFromPlan { handoff: PlanHandoff },
}

impl TurnCommand {
    pub const fn mode(&self) -> SessionMode {
        match self {
            Self::Submit { mode, .. } | Self::InvokeSkill { mode, .. } | Self::Compact { mode } => {
                *mode
            }
            Self::RevisePlanWithSkill { .. } => SessionMode::Plan,
            Self::EditTranscript(edit) => edit.replacement.mode(),
            Self::RunEnsemble { workflow, .. } => match workflow {
                EnsembleWorkflow::Plan => SessionMode::Plan,
                EnsembleWorkflow::Review => SessionMode::Build,
            },
            Self::ResolvePlan { .. } | Self::StartFromPlan { .. } => SessionMode::Build,
        }
    }
}

/// Interaction traffic never allocates a turn and is never queued.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlCommand {
    Worker(WorkerControl),
    AnswerQuestion {
        request_id: QuestionRequestId,
        response: QuestionResponse,
    },
    CancelTurn {
        turn_id: Option<TurnId>,
    },
    Shutdown,
}

/// Idle-only correlated maintenance. Queries may be serviced while busy.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagementCommand {
    /// Durable idle-only ordinary-root selection; never allocates a turn.
    SetMode {
        request_id: String,
        mode: SessionMode,
    },
    Models {
        request_id: String,
        request: crate::models::ModelManagementRequest,
    },
    Skills {
        request_id: String,
        request: crate::skill::SkillManagementRequest,
    },
}
