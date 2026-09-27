//! Typed acceptance boundary and the sole rejection/failure terminal path.

use super::*;

pub(super) enum TurnWork {
    Command(TurnCommand),
    ResumeEnsemble(EnsembleRecovery),
}
impl TurnWork {
    pub(super) fn mode(&self) -> SessionMode {
        match self {
            Self::Command(command) => command.mode(),
            Self::ResumeEnsemble(recovery) => match recovery.start.workflow {
                EnsembleWorkflow::Plan => SessionMode::Plan,
                EnsembleWorkflow::Review => SessionMode::Build,
            },
        }
    }
}

#[derive(Debug)]
pub(super) enum Rejection {
    Rejected(String),
    Cancelled,
    Fatal(SessionReplayError),
}
impl From<SessionReplayError> for Rejection {
    fn from(error: SessionReplayError) -> Self {
        Self::Fatal(error)
    }
}
impl From<String> for Rejection {
    fn from(error: String) -> Self {
        Self::Rejected(error)
    }
}
impl From<anyhow::Error> for Rejection {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<SessionReplayError>() {
            Ok(error) => Self::Fatal(error),
            Err(error) => Self::Rejected(format!("{error:#}")),
        }
    }
}

#[derive(Debug)]
pub(super) enum Failure {
    Failed {
        error: anyhow::Error,
        retained: Vec<TranscriptItem>,
        usage: Option<TokenUsage>,
    },
    Cancelled,
    Fatal(SessionReplayError),
}
impl From<SessionReplayError> for Failure {
    fn from(error: SessionReplayError) -> Self {
        Self::Fatal(error)
    }
}
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<SessionReplayError>() {
            Ok(error) => Self::Fatal(error),
            Err(error) => Self::Failed {
                error,
                retained: Vec::new(),
                usage: None,
            },
        }
    }
}

impl Failure {
    /// Cancellation is resolved at the in-flight work boundary, not after
    /// finalization: a retained terminal provider response must never be lost.
    pub(super) fn during_work(self, turn: &TurnContext) -> Self {
        if !matches!(self, Self::Fatal(_)) && turn.is_cancelled() {
            Self::Cancelled
        } else {
            self
        }
    }
}

/// Proof of acceptance, carrying request-local (not engine-global) dispatch state.
pub(super) struct AcceptedTurn {
    pub(super) kind: AcceptedKind,
    pub(super) request: Option<zevria_foundation::RequestMetadata>,
    pub(super) compaction_attempted_for_first_dispatch: bool,
}
impl AcceptedTurn {
    pub(super) fn new(kind: AcceptedKind) -> Self {
        Self {
            kind,
            request: None,
            compaction_attempted_for_first_dispatch: false,
        }
    }
    pub(super) fn mode(&self) -> SessionMode {
        match self.kind {
            AcceptedKind::Prompt { mode } | AcceptedKind::ManualCompaction { mode } => mode,
            AcceptedKind::Ensemble {
                workflow: EnsembleWorkflow::Plan,
                ..
            } => SessionMode::Plan,
            _ => SessionMode::Build,
        }
    }
}
pub(super) enum AcceptedKind {
    Prompt {
        mode: SessionMode,
    },
    Handoff,
    Ensemble {
        run_id: EnsembleRunId,
        workflow: EnsembleWorkflow,
        resumed: bool,
    },
    ManualCompaction {
        mode: SessionMode,
    },
    PlanDecision,
}

pub(super) struct TurnCompletion;

pub(super) fn terminal_records(kind: &AcceptedKind, failure: &Failure) -> Vec<TranscriptItem> {
    let ensemble = match kind {
        AcceptedKind::Ensemble { run_id, .. } => Some(run_id),
        _ => None,
    };
    let error = match failure {
        Failure::Failed { error, .. } => error.to_string(),
        Failure::Cancelled if ensemble.is_some() => {
            "ensemble turn cancelled by the user".to_string()
        }
        Failure::Cancelled => "turn cancelled by the user".to_string(),
        Failure::Fatal(_) => return Vec::new(),
    };
    let mut records = Vec::new();
    if let Some(run_id) = ensemble {
        records.push(TranscriptItem::Ensemble(match failure {
            Failure::Cancelled => EnsembleRecord::Cancelled {
                run_id: run_id.clone(),
            },
            _ => EnsembleRecord::Failed {
                run_id: run_id.clone(),
                error: error.clone(),
            },
        }));
    }
    records.push(TranscriptItem::Error { error });
    records
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) async fn execute_turn(
        &mut self,
        work: TurnWork,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        if turn.is_cancelled() && matches!(work, TurnWork::Command(_)) {
            return self
                .finish_rejected(turn, events, Rejection::Cancelled)
                .await;
        }
        let command = match work {
            TurnWork::Command(command) => command,
            TurnWork::ResumeEnsemble(recovery) => {
                return self.resume_ensemble(recovery, events, turn).await;
            }
        };
        match command {
            TurnCommand::Submit {
                text,
                mode,
                behavior,
            } => {
                self.run_prompt_turn(
                    TurnAnchor::Append,
                    PromptTurnInput::Message { text, behavior },
                    mode,
                    events,
                    turn,
                )
                .await
            }
            TurnCommand::InvokeSkill { name, args, mode } => {
                self.run_prompt_turn(
                    TurnAnchor::Append,
                    PromptTurnInput::Skill { name, args },
                    mode,
                    events,
                    turn,
                )
                .await
            }
            TurnCommand::RevisePlanWithSkill {
                expected,
                name,
                args,
            } => {
                self.run_prompt_turn(
                    TurnAnchor::Append,
                    PromptTurnInput::RevisionSkill {
                        expected,
                        name,
                        args,
                    },
                    SessionMode::Plan,
                    events,
                    turn,
                )
                .await
            }
            TurnCommand::EditTranscript(edit) => self.edit_transcript(edit, events, turn).await,
            TurnCommand::Compact { mode } => self.compact_command(mode, events, turn).await,
            TurnCommand::RunEnsemble { workflow, prompt } => {
                self.run_ensemble(TurnAnchor::Append, workflow, prompt, events, turn)
                    .await
            }
            TurnCommand::ResolvePlan { expected, decision } => {
                self.resolve_plan(expected, decision, events, turn).await
            }
            TurnCommand::StartFromPlan { handoff } => {
                self.start_from_plan(handoff, events, turn).await
            }
        }
    }

    pub(super) async fn finish_rejected(
        &mut self,
        turn: &TurnContext,
        events: &SessionEventSender,
        rejection: Rejection,
    ) -> Result<(), SessionReplayError> {
        // A committed replay failure always wins a simultaneous cancellation.
        self.ensure_replay_valid()?;
        match rejection {
            Rejection::Fatal(error) => return Err(error),
            Rejection::Cancelled => {
                self.provider.reset();
                let _ = events
                    .send(SessionEvent::TurnCancelled { turn_id: turn.id })
                    .await;
            }
            Rejection::Rejected(error) => {
                tracing::warn!(%error, "turn rejected before acceptance");
                let _ = events
                    .send(SessionEvent::TurnRejected {
                        turn_id: turn.id,
                        error,
                    })
                    .await;
            }
        }
        Ok(())
    }

    pub(super) async fn finish_accepted(
        &mut self,
        accepted: &AcceptedTurn,
        turn: &TurnContext,
        events: &SessionEventSender,
        outcome: Result<TurnCompletion, Failure>,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        let Err(failure) = outcome else {
            return Ok(());
        };
        if let Failure::Fatal(error) = failure {
            return Err(error);
        }
        self.provider.reset();
        events.stream_cleared(turn.id);
        let terminal = terminal_records(&accepted.kind, &failure);
        let event = match failure {
            Failure::Failed {
                error,
                mut retained,
                usage,
            } => {
                retained.extend(terminal);
                let persistence = self.record_completed_items(retained);
                if let Some(usage) = usage {
                    let policy = match &accepted.kind {
                        AcceptedKind::Ensemble { workflow, .. } => self.ensemble_policy(*workflow),
                        _ => self.policies.policy(accepted.mode()).clone(),
                    };
                    self.report_provider_usage(&policy, usage.total_tokens)?;
                }
                if let Err(error) = persistence {
                    self.persistence_failed(&error, events).await?;
                }
                SessionEvent::TurnFailed {
                    turn_id: turn.id,
                    error: error.to_string(),
                }
            }
            Failure::Cancelled => {
                self.record_recoverable_items(terminal, events).await?;
                SessionEvent::TurnCancelled { turn_id: turn.id }
            }
            Failure::Fatal(_) => unreachable!(),
        };
        let _ = events.send(event).await;
        Ok(())
    }
}
