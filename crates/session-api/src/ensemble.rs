use crate::*;
use std::{future::Future, pin::Pin};
/// Input passed to the provider-neutral launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsembleLaunchRequest {
    pub start: EnsembleStart,
    pub resume: bool,
}

pub type EnsembleLaunchFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Vec<AgentRunOutcome>>> + Send + 'a>>;

/// Provider-neutral launcher injected into the session engine.
pub trait EnsembleLauncher: Send + Sync + 'static {
    fn workers(&self, workflow: EnsembleWorkflow) -> anyhow::Result<Vec<AgentRunDescriptor>>;

    fn max_synthesis_bytes_per_agent(&self) -> usize;

    fn finalize_review<'a>(
        &'a self,
        _request: EnsembleLaunchRequest,
        _outcomes: Vec<AgentRunOutcome>,
        _events: SessionEventSender,
        _turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async { anyhow::bail!("launcher cannot persist confirmed review outcomes") })
    }

    /// Read-only cross-log reconciliation before restoring queued input.
    fn recover_review(
        &self,
        _start: &EnsembleStart,
        _history: &[(AgentRunId, WorkerReviewEvent)],
    ) -> anyhow::Result<Vec<crate::WorkerActorUpdate>> {
        Ok(Vec::new())
    }

    fn start_review(
        &self,
        _request: EnsembleLaunchRequest,
        _states: Vec<WorkerReviewState>,
        _events: SessionEventSender,
        _turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        anyhow::bail!("launcher does not support interactive Plan worker review")
    }

    fn launch<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a>;
}
