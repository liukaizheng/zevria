//! Worker run events, plan evidence, and terminal outcomes.

use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

use super::{
    AgentRunId,
    decisions::{
        AgentElicitationOutcome, AgentUnavailableDecision, AgentUserDecisionBatch,
        AgentUserDecisionId, merge_decision_ids_from_batches, sha256_hex,
    },
};
use crate::{ConfirmedWorkerPlan, WorkerReviewEvent};

/// Stable synthetic plan identifier for validated Claude Code native handoffs.
pub const CLAUDE_PLAN_HANDOFF_PLAN_ID: &str = "zevria-claude-code-exit-plan-mode";

/// Stable root-visible identity for one configured worker process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunDescriptor {
    pub id: AgentRunId,
    pub agent: String,
    pub label: String,
    pub safe_mode: String,
}

/// Lifecycle state for one independent worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Queued,
    Starting,
    Running,
    Resuming,
    AwaitingFeedback,
    AwaitingConfirmation,
    Confirmed,
    Blocked,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Abandoned,
    Interrupted,
}

impl AgentRunStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::TimedOut
                | Self::Cancelled
                | Self::Abandoned
                | Self::Interrupted
        )
    }
}

impl fmt::Display for AgentRunStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Resuming => "resuming",
            Self::AwaitingFeedback => "awaiting feedback / fresh publication",
            Self::AwaitingConfirmation => "awaiting confirmation",
            Self::Confirmed => "confirmed (reopenable)",
            Self::Blocked => "blocked; retry available",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned · excluded",
            Self::Interrupted => "interrupted",
        })
    }
}

/// A file location associated with an ACP tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunLocation {
    pub path: PathBuf,
    pub line: Option<u32>,
}

/// One item in the latest structured plan reported by a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

/// Latest complete structured ACP plan for a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStructuredPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    pub entries: Vec<AgentPlanEntry>,
}

impl AgentStructuredPlan {
    /// Whether this plan carries the mandatory inline Markdown completion
    /// proof. Trimming is used only for validation; the original payload is
    /// retained byte-for-byte.
    pub fn has_markdown_proof(&self) -> bool {
        self.markdown
            .as_deref()
            .is_some_and(|markdown| !markdown.trim().is_empty())
    }
}

/// Host-validated origin of a frozen native proposal. ACP metadata cannot create
/// this event; only the host's rejection-and-snapshot path may publish it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlanCapture {
    pub generation: u64,
    pub exit_tool_id: String,
    pub source: NativePlanSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativePlanSource {
    Explicit,
    Artifact {
        artifact_tool_id: String,
        path: PathBuf,
        content_digest: String,
    },
}

impl NativePlanCapture {
    pub fn content_digest(bytes: &[u8]) -> String {
        sha256_hex(bytes)
    }

    pub fn validate(&self, plan: &AgentStructuredPlan) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.generation > 0 && !self.exit_tool_id.trim().is_empty(),
            "native capture requires a generation and exit tool identity"
        );
        anyhow::ensure!(
            plan.plan_id.as_deref() == Some(CLAUDE_PLAN_HANDOFF_PLAN_ID)
                && plan.entries.is_empty()
                && plan.has_markdown_proof(),
            "native capture requires an exact Markdown snapshot"
        );
        if let NativePlanSource::Artifact {
            artifact_tool_id,
            path,
            content_digest,
        } = &self.source
        {
            let markdown = plan.markdown.as_deref().expect("proof checked above");
            anyhow::ensure!(
                !artifact_tool_id.trim().is_empty()
                    && path.is_absolute()
                    && path.extension().is_some_and(|extension| extension == "md"),
                "native artifact capture requires a validated Markdown path and tool identity"
            );
            anyhow::ensure!(
                markdown.len() <= crate::MAX_PLAN_ARTIFACT_BYTES
                    && *content_digest == Self::content_digest(markdown.as_bytes()),
                "native artifact snapshot size or digest is invalid"
            );
        }
        Ok(())
    }
}

/// Durable reason why Zevria spent a worker's one semantic repair prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRunRepair {
    HostDeniedPermission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
    },
    EarlyStop {
        stop_reason: String,
    },
}

/// ACP context/cost accounting. This is intentionally distinct from root
/// OpenAI token accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUsage {
    pub used: u64,
    pub size: u64,
    pub cost: Option<serde_json::Value>,
}

/// Direction of inspectable ACP transport traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProtocolDirection {
    ClientToAgent,
    AgentToClient,
}

/// Every provider-neutral update retained in a worker pane and JSONL log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRunEvent {
    /// Validated display-only ordering; inert for reports, choices and workflow.
    ResponseDisplay {
        display: Box<crate::response::ResponseDisplay>,
    },
    Review {
        event: Box<WorkerReviewEvent>,
    },
    Status {
        status: AgentRunStatus,
        detail: Option<String>,
    },
    /// Persist session identity before safety/bootstrap can fail. This does
    /// not claim the connection is safe or ready for a model prompt.
    SessionAllocated {
        session_id: String,
    },
    SessionEstablished {
        session_id: String,
        capabilities: serde_json::Value,
        safe_mode: String,
        recovered: bool,
    },
    /// An explicit prompt boundary. Reports are projected only from agent
    /// messages following the latest boundary.
    Prompt {
        text: String,
        continuation: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repair: Option<AgentRunRepair>,
    },
    UserImage {
        image: crate::PromptImage,
        message_id: Option<String>,
    },
    UserMessage {
        text: String,
        message_id: Option<String>,
    },
    AgentMessage {
        text: String,
        message_id: Option<String>,
    },
    Thought {
        text: String,
        message_id: Option<String>,
    },
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        content: Vec<String>,
        locations: Vec<AgentRunLocation>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
        content: Option<Vec<String>>,
        locations: Option<Vec<AgentRunLocation>>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
    },
    /// Display-only typed evidence from a correlated ACP tool-result sidecar.
    /// It never contributes text to prompts or workflow confirmation evidence.
    ToolResultMetadata {
        metadata: Box<zevria_foundation::ToolResultMetadata>,
    },
    Plan {
        plan: AgentStructuredPlan,
    },
    /// Plan bytes and host provenance are persisted atomically before publication.
    NativePlanCaptured {
        plan: AgentStructuredPlan,
        capture: NativePlanCapture,
    },
    PlanRemoved {
        plan_id: String,
    },
    ModeChanged {
        mode: String,
    },
    ConfigOptionsChanged {
        options: serde_json::Value,
    },
    SessionInfo {
        title: Option<String>,
        updated_at: Option<String>,
        metadata: Option<serde_json::Value>,
    },
    Usage {
        usage: AgentUsage,
    },
    Permission {
        tool_kind: Option<String>,
        decision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
    },
    Elicitation {
        field_count: usize,
        outcome: AgentElicitationOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<AgentUserDecisionBatch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision_unavailable: Option<AgentUnavailableDecision>,
    },
    Stderr {
        text: String,
    },
    Protocol {
        direction: AgentProtocolDirection,
        json: String,
    },
    /// `session/load` can replay prior history. A pane replaces everything
    /// before this boundary rather than showing duplicates.
    ReplayBoundary,
    Unsupported {
        context: String,
        placeholder: String,
    },
    Failure {
        error: String,
    },
}

impl_wire_types! {
    AgentRunEvent, KNOWN_WIRE_TYPES;
    Self::ResponseDisplay { .. } => "response_display",
    Self::Review { .. } => "review",
    Self::Status { .. } => "status",
    Self::SessionAllocated { .. } => "session_allocated",
    Self::SessionEstablished { .. } => "session_established",
    Self::Prompt { .. } => "prompt",
    Self::UserImage { .. } => "user_image",
    Self::UserMessage { .. } => "user_message",
    Self::AgentMessage { .. } => "agent_message",
    Self::Thought { .. } => "thought",
    Self::ToolCall { .. } => "tool_call",
    Self::ToolCallUpdate { .. } => "tool_call_update",
    Self::ToolResultMetadata { .. } => "tool_result_metadata",
    Self::Plan { .. } => "plan",
    Self::NativePlanCaptured { .. } => "native_plan_captured",
    Self::PlanRemoved { .. } => "plan_removed",
    Self::ModeChanged { .. } => "mode_changed",
    Self::ConfigOptionsChanged { .. } => "config_options_changed",
    Self::SessionInfo { .. } => "session_info",
    Self::Usage { .. } => "usage",
    Self::Permission { .. } => "permission",
    Self::Elicitation { .. } => "elicitation",
    Self::Stderr { .. } => "stderr",
    Self::Protocol { .. } => "protocol",
    Self::ReplayBoundary => "replay_boundary",
    Self::Unsupported { .. } => "unsupported",
    Self::Failure { .. } => "failure",
}

impl AgentRunEvent {
    /// Native ACP projection supplies both an attempt-scoped ID and origin.
    /// External agents continue to own their capabilities and correlations.
    pub fn is_hosted_search_activity(&self) -> bool {
        match self {
            Self::ToolCall { id, raw_input, .. } | Self::ToolCallUpdate { id, raw_input, .. } => {
                crate::response::is_hosted_search_input(id, raw_input.as_ref())
            }
            _ => false,
        }
    }
}

/// Apply one normalized plan update/removal to the latest-plan projection.
/// Updates replace the complete current plan. Removals affect only the plan
/// whose identifier matches the durable removal event.
pub fn reduce_agent_plan_event(
    current: &mut Option<AgentStructuredPlan>,
    event: &AgentRunEvent,
) -> bool {
    match event {
        AgentRunEvent::Plan { plan } | AgentRunEvent::NativePlanCaptured { plan, .. } => {
            *current = Some(plan.clone());
            true
        }
        AgentRunEvent::PlanRemoved { plan_id }
            if current.as_ref().and_then(|plan| plan.plan_id.as_deref())
                == Some(plan_id.as_str()) =>
        {
            *current = None;
            true
        }
        AgentRunEvent::PlanRemoved { .. } => false,
        _ => false,
    }
}

/// Terminal result of one worker. Full reports live in the worker log; root
/// `ReportsReady` owns the bounded model-visible copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunOutcome {
    pub descriptor: AgentRunDescriptor,
    pub status: AgentRunStatus,
    pub report: String,
    pub plan: Option<AgentStructuredPlan>,
    pub confirmation: Option<Box<ConfirmedWorkerPlan>>,
    pub partial: bool,
    pub failure: Option<String>,
    pub usage: Option<AgentUsage>,
    pub acp_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_decisions: Vec<AgentUserDecisionBatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_ids: Vec<AgentUserDecisionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_decisions: Vec<AgentUnavailableDecision>,
}

pub const WORKER_ABANDONMENT_REASON: &str =
    "Permanently abandoned by the user; plan and captured answers excluded from synthesis.";

impl AgentRunOutcome {
    /// Historical evidence stays in the journal, never in the frozen disposition.
    pub fn sanitize_abandonment(&mut self) {
        self.status = AgentRunStatus::Abandoned;
        self.report.clear();
        self.plan = None;
        self.confirmation = None;
        self.partial = false;
        self.failure = Some(WORKER_ABANDONMENT_REASON.into());
        self.user_decisions.clear();
        self.decision_ids.clear();
        self.unavailable_decisions.clear();
    }

    pub fn is_sanitized_abandonment(&self) -> bool {
        self.status == AgentRunStatus::Abandoned
            && !self.partial
            && self.failure.as_deref() == Some(WORKER_ABANDONMENT_REASON)
            && self.report.is_empty()
            && self.plan.is_none()
            && self.confirmation.is_none()
            && self.user_decisions.is_empty()
            && self.decision_ids.is_empty()
            && self.unavailable_decisions.is_empty()
    }

    pub fn has_plan_proof(&self) -> bool {
        self.plan
            .as_ref()
            .is_some_and(AgentStructuredPlan::has_markdown_proof)
    }

    pub fn has_usable_evidence(&self) -> bool {
        !self.report.trim().is_empty()
            || self.plan.is_some()
            || !self.decision_ids.is_empty()
            || !self.user_decisions.is_empty()
            || !self.unavailable_decisions.is_empty()
    }

    pub fn summary(&self) -> AgentRunSummary {
        let mut decision_ids = self.decision_ids.clone();
        merge_decision_ids_from_batches(&mut decision_ids, &self.user_decisions);
        AgentRunSummary {
            descriptor: self.descriptor.clone(),
            status: self.status,
            partial: self.partial,
            failure: self.failure.clone(),
            has_report: self.has_usable_evidence(),
            has_plan_proof: self.has_plan_proof(),
            confirmation: self.confirmation.clone(),
            decision_ids,
            unavailable_decisions: self.unavailable_decisions.clone(),
        }
    }
}

/// Compact worker terminal metadata retained in the root transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunSummary {
    pub descriptor: AgentRunDescriptor,
    pub status: AgentRunStatus,
    pub partial: bool,
    pub failure: Option<String>,
    pub has_report: bool,
    pub has_plan_proof: bool,
    pub confirmation: Option<Box<ConfirmedWorkerPlan>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_ids: Vec<AgentUserDecisionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_decisions: Vec<AgentUnavailableDecision>,
}

impl AgentRunSummary {
    pub fn is_sanitized_abandonment(&self) -> bool {
        self.status == AgentRunStatus::Abandoned
            && !self.partial
            && self.failure.as_deref() == Some(WORKER_ABANDONMENT_REASON)
            && !self.has_report
            && !self.has_plan_proof
            && self.confirmation.is_none()
            && self.decision_ids.is_empty()
            && self.unavailable_decisions.is_empty()
    }
}
