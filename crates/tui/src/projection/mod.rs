//! Source adapters for semantic projection evidence. This layer never starts
//! operations, changes drafts, or emits engine commands.

use zevria_workflow::{
    AgentRunId, AgentRunOutcome, AgentRunStatus, AgentRunSummary, EnsembleRunId,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProjectionMode {
    Live,
    Restored,
}

/// A completion is fallback evidence, not a replacement for authoritative
/// review transitions. Live outcomes and durable ReportsReady use this same
/// normalization, including exact-run confirmation/baseline validation.
pub(crate) struct WorkerCompletion {
    pub worker_id: AgentRunId,
    pub status: AgentRunStatus,
    pub partial: bool,
    pub has_plan_proof: bool,
    pub baseline: bool,
    pub failure: Option<String>,
}

impl WorkerCompletion {
    pub(crate) fn outcome(run: &EnsembleRunId, outcome: AgentRunOutcome) -> Self {
        let has_plan_proof = outcome.has_plan_proof();
        let baseline = outcome.confirmation.as_ref().is_some_and(|plan| {
            plan.validate(&outcome.descriptor.id)
                && &plan.receipt.target.run_id == run
                && plan.baseline.is_some()
        });
        Self {
            worker_id: outcome.descriptor.id,
            status: outcome.status,
            partial: outcome.partial,
            has_plan_proof,
            baseline,
            failure: outcome.failure,
        }
    }

    pub(crate) fn summary(run: &EnsembleRunId, summary: AgentRunSummary) -> Self {
        let baseline = summary.confirmation.as_ref().is_some_and(|plan| {
            plan.validate(&summary.descriptor.id)
                && &plan.receipt.target.run_id == run
                && plan.baseline.is_some()
        });
        Self {
            worker_id: summary.descriptor.id,
            status: summary.status,
            partial: summary.partial,
            has_plan_proof: summary.has_plan_proof,
            baseline,
            failure: summary.failure,
        }
    }
}
