//! Provider-neutral ensemble workflow contracts and durable ACP worker logs.

use rig_core::message::Message;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};
use uuid::Uuid;

use crate::{WorkerControlResult, WorkerReviewEvent};

mod decisions;
mod reconciliation;
mod synthesis;
mod worker;

pub(crate) use decisions::sha256_hex;
pub use decisions::{
    AgentElicitationOutcome, AgentUnavailableDecision, AgentUnavailableDecisionId,
    AgentUnavailableDecisionReason, AgentUserDecisionAnswer, AgentUserDecisionBatch,
    AgentUserDecisionId, AgentUserDecisionValue, MAX_NORMALIZED_USER_DECISION_BYTES,
    merge_decision_evidence, merge_decision_ids_from_batches, merge_unavailable_decisions,
};
pub use reconciliation::{
    ReconciliationNextStep, RecordedDecisionAccounting, RecordedDecisionDisposition,
    ReportBaseline, ReportDisagreement, ReportDisagreementClassification,
    ReportDisagreementResolution, ReportPosition, ReportReconciliation,
    ReportReconciliationCatalog, ReportReconciliationError, RepositoryEvidenceResolutionKind,
    UnavailableDecisionAccounting, UnavailableDecisionDisposition, ValidatedReportReconciliation,
};
pub use synthesis::{
    UNTRUSTED_EVIDENCE_PREAMBLE, build_synthesis_input, build_synthesis_prompt,
    build_synthesis_prompt_with_feedback, potential_synthesis_images, truncate_utf8,
    validate_synthesis_image_budget, validate_worker_synthesis_payload,
};
pub use worker::{
    AgentPlanEntry, AgentProtocolDirection, AgentRunDescriptor, AgentRunEvent, AgentRunLocation,
    AgentRunOutcome, AgentRunRepair, AgentRunStatus, AgentRunSummary, AgentStructuredPlan,
    AgentUsage, CLAUDE_PLAN_HANDOFF_PLAN_ID, NativePlanCapture, NativePlanSource,
    WORKER_ABANDONMENT_REASON, reduce_agent_plan_event,
};

pub const MAX_AGENT_RUN_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Which visible ensemble workflow the user requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsembleWorkflow {
    Plan,
    Review,
}

impl EnsembleWorkflow {
    pub const fn slash_command(self) -> &'static str {
        match self {
            Self::Plan => "/ensemble-plan",
            Self::Review => "/ensemble-review",
        }
    }
}

impl fmt::Display for EnsembleWorkflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Plan => "Ensemble Plan",
            Self::Review => "Ensemble Review",
        })
    }
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(EnsembleRunId);
string_id!(AgentRunId);

/// Durable payload of an ensemble start record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnsembleStart {
    pub run_id: EnsembleRunId,
    pub workflow: EnsembleWorkflow,
    pub prompt: crate::UserPrompt,
    pub agents: Vec<AgentRunDescriptor>,
}

impl EnsembleStart {
    /// The durable launch definition selects direct publication, never the
    /// number of survivors or the current worker configuration.
    pub fn publishes_confirmed_worker_plan(&self) -> bool {
        self.workflow == EnsembleWorkflow::Plan && self.agents.len() == 1
    }

    pub fn command(&self) -> String {
        format!(
            "{} {}",
            self.workflow.slash_command(),
            self.prompt.display_projection()
        )
    }
}

/// Engine-owned root ensemble state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnsembleRecord {
    Started {
        start: EnsembleStart,
    },
    ReviewStarted {
        run_id: EnsembleRunId,
        version: u32,
    },
    WorkerReview {
        run_id: EnsembleRunId,
        worker_id: AgentRunId,
        event: Box<WorkerReviewEvent>,
        result: Option<WorkerControlResult>,
    },
    ControlResult {
        run_id: EnsembleRunId,
        result: WorkerControlResult,
    },
    WorkersConfirmed {
        run_id: EnsembleRunId,
        /// Final accepted Confirm or Abandon, atomically sealing all workers.
        final_confirmation: WorkerControlResult,
        outcomes: Vec<AgentRunOutcome>,
    },
    ReportsReady {
        run_id: EnsembleRunId,
        synthesis_input: Message,
        agents: Vec<AgentRunSummary>,
    },
    Completed {
        run_id: EnsembleRunId,
    },
    Failed {
        run_id: EnsembleRunId,
        error: String,
    },
    Cancelled {
        run_id: EnsembleRunId,
    },
}

impl EnsembleRecord {
    pub fn run_id(&self) -> &EnsembleRunId {
        match self {
            Self::Started { start } => &start.run_id,
            Self::ReviewStarted { run_id, .. }
            | Self::WorkerReview { run_id, .. }
            | Self::ControlResult { run_id, .. }
            | Self::WorkersConfirmed { run_id, .. }
            | Self::ReportsReady { run_id, .. }
            | Self::Completed { run_id }
            | Self::Failed { run_id, .. }
            | Self::Cancelled { run_id } => run_id,
        }
    }

    /// Only the durable synthesis input enters model history. Child files are
    /// never consulted during root replay.
    pub fn model_message(&self) -> Option<&Message> {
        match self {
            Self::ReportsReady {
                synthesis_input, ..
            } => Some(synthesis_input),
            Self::Started { .. }
            | Self::ReviewStarted { .. }
            | Self::WorkerReview { .. }
            | Self::ControlResult { .. }
            | Self::WorkersConfirmed { .. }
            | Self::Completed { .. }
            | Self::Failed { .. }
            | Self::Cancelled { .. } => None,
        }
    }
}

/// Last unfinished ensemble reconstructed from root records.
#[derive(Debug, Clone, PartialEq)]
pub struct EnsembleRecovery {
    pub start: EnsembleStart,
    pub reports_ready: Option<(Message, Vec<AgentRunSummary>)>,
}

pub fn latest_ensemble_recovery<'a>(
    records: impl IntoIterator<Item = &'a EnsembleRecord>,
) -> Option<EnsembleRecovery> {
    let mut starts: HashMap<&EnsembleRunId, &EnsembleStart> = HashMap::new();
    let mut reports: HashMap<&EnsembleRunId, (&Message, &Vec<AgentRunSummary>)> = HashMap::new();
    let mut order = Vec::new();
    for record in records {
        match record {
            EnsembleRecord::Started { start } => {
                starts.insert(&start.run_id, start);
                order.push(&start.run_id);
            }
            EnsembleRecord::ReportsReady {
                run_id,
                synthesis_input,
                agents,
            } => {
                reports.insert(run_id, (synthesis_input, agents));
            }
            EnsembleRecord::ReviewStarted { .. }
            | EnsembleRecord::WorkerReview { .. }
            | EnsembleRecord::ControlResult { .. }
            | EnsembleRecord::WorkersConfirmed { .. } => {}
            EnsembleRecord::Completed { run_id }
            | EnsembleRecord::Failed { run_id, .. }
            | EnsembleRecord::Cancelled { run_id } => {
                starts.remove(run_id);
                reports.remove(run_id);
            }
        }
    }
    order.into_iter().rev().find_map(|run_id| {
        starts.remove(run_id).map(|start| EnsembleRecovery {
            start: start.clone(),
            reports_ready: reports
                .remove(run_id)
                .map(|(message, agents)| (message.clone(), agents.clone())),
        })
    })
}
