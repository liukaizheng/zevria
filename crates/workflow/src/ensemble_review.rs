//! Host-owned interactive Plan review. This reducer is shared by live review,
//! root replay and worker-pane snapshots. Transport completion is never consent.
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{
    AgentRunDescriptor, AgentRunId, AgentRunOutcome, AgentRunStatus, AgentStructuredPlan,
    EnsembleRunId, TurnId,
};

pub const ENSEMBLE_REVIEW_VERSION: u32 = 1;
pub const MAX_WORKER_FEEDBACK_BYTES: usize = 64 * 1024;
pub const WORKER_CONTROL_CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerControlId(pub String);
impl WorkerControlId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}
impl WorkerControlId {
    pub fn validate(&self) -> bool {
        !self.0.trim().is_empty() && self.0.len() <= 128
    }
}
impl Default for WorkerControlId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControlTarget {
    pub turn_id: TurnId,
    pub run_id: EnsembleRunId,
    pub worker_id: AgentRunId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlanRevision {
    pub worker_id: AgentRunId,
    pub generation: u64,
    pub revision: u64,
    pub digest: String,
}

/// An immutable, verified proposal. Cloning a receipt or view does not copy
/// the Markdown, and eligibility checks never hash it again.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "WorkerPlanData")]
pub struct WorkerPlanSnapshot(std::sync::Arc<WorkerPlanData>);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlanData {
    pub revision: WorkerPlanRevision,
    pub plan: AgentStructuredPlan,
}

impl std::ops::Deref for WorkerPlanSnapshot {
    type Target = WorkerPlanData;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for WorkerPlanSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}

impl TryFrom<WorkerPlanData> for WorkerPlanSnapshot {
    type Error = &'static str;
    fn try_from(data: WorkerPlanData) -> Result<Self, Self::Error> {
        if data.revision.generation == 0
            || data.revision.revision == 0
            || !data.plan.markdown.as_deref().is_some_and(|markdown| {
                !markdown.trim().is_empty()
                    && data.revision.digest == crate::ensemble::sha256_hex(markdown.as_bytes())
            })
        {
            return Err("invalid worker plan snapshot identity or Markdown digest");
        }
        Ok(Self(std::sync::Arc::new(data)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfirmationReceipt {
    pub request_id: WorkerControlId,
    pub target: WorkerControlTarget,
    pub revision: WorkerPlanRevision,
}
impl WorkerConfirmationReceipt {
    pub fn validates_snapshot(&self, snapshot: &WorkerPlanSnapshot, worker: &AgentRunId) -> bool {
        self.request_id.validate()
            && !self.target.run_id.as_str().trim().is_empty()
            && &snapshot.revision.worker_id == worker
            && &self.target.worker_id == worker
            && self.revision == snapshot.revision
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedWorkerPlan {
    pub snapshot: WorkerPlanSnapshot,
    pub receipt: WorkerConfirmationReceipt,
    /// Exact marking control, independent of the original confirmation/turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<WorkerConfirmationReceipt>,
}
impl ConfirmedWorkerPlan {
    pub fn validate(&self, worker: &AgentRunId) -> bool {
        self.receipt.validates_snapshot(&self.snapshot, worker)
            && self.baseline.as_ref().is_none_or(|receipt| {
                receipt.validates_snapshot(&self.snapshot, worker)
                    && receipt.target.run_id == self.receipt.target.run_id
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerPromptKind {
    Initial,
    UserFeedback,
    RecoveryContinuation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerInput {
    pub generation: u64,
    pub request_id: WorkerControlId,
    pub kind: WorkerPromptKind,
    /// Exact user input, not the provider instruction envelope.
    pub text: crate::UserPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerControlAction {
    SendFeedback {
        text: crate::UserPrompt,
    },
    Confirm {
        expected_revision: WorkerPlanRevision,
    },
    Unconfirm {
        expected_revision: WorkerPlanRevision,
    },
    Baseline {
        expected_revision: WorkerPlanRevision,
    },
    Unbaseline {
        expected_revision: WorkerPlanRevision,
    },
    Retry,
    CancelPrompt,
    Abandon,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControl {
    pub request_id: WorkerControlId,
    pub target: WorkerControlTarget,
    pub action: WorkerControlAction,
}
impl WorkerControl {
    /// Only these controls can complete the participating confirmation barrier.
    pub fn sealing_event(&self) -> Result<WorkerReviewEvent, String> {
        match &self.action {
            WorkerControlAction::Confirm { expected_revision }
            | WorkerControlAction::Baseline { expected_revision } => {
                let receipt = WorkerConfirmationReceipt {
                    request_id: self.request_id.clone(),
                    target: self.target.clone(),
                    revision: expected_revision.clone(),
                };
                Ok(
                    if matches!(self.action, WorkerControlAction::Baseline { .. }) {
                        WorkerReviewEvent::BaselineMarked { receipt }
                    } else {
                        WorkerReviewEvent::Confirmed { receipt }
                    },
                )
            }
            WorkerControlAction::Abandon => Ok(WorkerReviewEvent::Abandoned {
                request_id: self.request_id.clone(),
            }),
            _ => Err("invalid final review control".into()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControlResult {
    pub control: WorkerControl,
    pub accepted: bool,
    pub detail: String,
}
impl WorkerControlResult {
    pub fn rejected(control: WorkerControl, detail: impl Into<String>) -> Self {
        Self {
            control,
            accepted: false,
            detail: detail.into(),
        }
    }
}

/// Root records persist these transitions before publishing their projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transition", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerReviewEvent {
    ImageCapability {
        supported: bool,
    },
    /// IO failure prevents trustworthy review; never persisted as eligibility.
    Fatal {
        error: String,
    },
    InputAccepted {
        input: WorkerInput,
    },
    Dispatched {
        generation: u64,
        attempt: u64,
    },
    /// Host recovery boundary: dispatched work cannot be blindly redelivered.
    Interrupted {
        generation: u64,
    },
    Recovering {
        generation: u64,
        attempt: u64,
    },
    Published {
        generation: u64,
        plan: AgentStructuredPlan,
        replay: bool,
    },
    Removed {
        plan_id: String,
    },
    Settled {
        generation: u64,
        failure: Option<String>,
        connected: bool,
        evidence: Box<AgentRunOutcome>,
    },
    Connection {
        connected: bool,
        diagnostic: Option<String>,
    },
    PayloadChecked {
        error: Option<String>,
    },
    CancelRequested {
        generation: u64,
    },
    Decisions {
        decision: Option<crate::AgentUserDecisionBatch>,
        unavailable: Option<crate::AgentUnavailableDecision>,
    },
    Confirmed {
        receipt: WorkerConfirmationReceipt,
    },
    BaselineMarked {
        receipt: WorkerConfirmationReceipt,
    },
    BaselineCleared {
        request_id: WorkerControlId,
    },
    Withdrawn {
        request_id: WorkerControlId,
    },
    Abandoned {
        request_id: WorkerControlId,
    },
    Sealed,
}

impl_wire_types! {
    WorkerReviewEvent, KNOWN_WIRE_TRANSITIONS;
    Self::ImageCapability { .. } => "image_capability",
    Self::Fatal { .. } => "fatal",
    Self::InputAccepted { .. } => "input_accepted",
    Self::Dispatched { .. } => "dispatched",
    Self::Interrupted { .. } => "interrupted",
    Self::Recovering { .. } => "recovering",
    Self::Published { .. } => "published",
    Self::Removed { .. } => "removed",
    Self::Settled { .. } => "settled",
    Self::Connection { .. } => "connection",
    Self::PayloadChecked { .. } => "payload_checked",
    Self::CancelRequested { .. } => "cancel_requested",
    Self::Decisions { .. } => "decisions",
    Self::Confirmed { .. } => "confirmed",
    Self::BaselineMarked { .. } => "baseline_marked",
    Self::BaselineCleared { .. } => "baseline_cleared",
    Self::Withdrawn { .. } => "withdrawn",
    Self::Abandoned { .. } => "abandoned",
    Self::Sealed => "sealed",
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReviewLifecycle {
    Active,
    Sealed,
    Abandoned,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputQueue {
    pub accepted_generation: u64,
    pub settled_generation: u64,
    pub pending: VecDeque<WorkerInput>,
    pub active: Option<WorkerInput>,
    pub cancel_requested: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalState {
    pub next_revision: u64,
    pub candidate: Option<WorkerPlanSnapshot>,
    pub retained: Option<WorkerPlanSnapshot>,
    pub publication_floor: u64,
    pub retained_removed: bool,
    pub confirmation: Option<WorkerConfirmationReceipt>,
    pub baseline: Option<WorkerConfirmationReceipt>,
    pub synthesis_error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionState {
    pub connected: bool,
    pub attempt: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageEvidence {
    pub image_capability: Option<bool>,
    pub incorporated_images: Vec<WorkerInput>,
    pub failed_image_input: Option<WorkerInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReviewState {
    pub descriptor: AgentRunDescriptor,
    pub lifecycle: WorkerReviewLifecycle,
    pub inputs: InputQueue,
    pub proposals: ProposalState,
    pub connection: ConnectionState,
    pub images: ImageEvidence,
    /// The outcome of the last settled generation, independent of transport diagnostics.
    pub settlement_failure: Option<String>,
    pub connection_diagnostic: Option<String>,
    pub evidence: AgentRunOutcome,
}
impl WorkerReviewState {
    pub const fn is_abandoned(&self) -> bool {
        matches!(self.lifecycle, WorkerReviewLifecycle::Abandoned)
    }

    pub const fn is_sealed(&self) -> bool {
        matches!(self.lifecycle, WorkerReviewLifecycle::Sealed)
    }

    pub fn diagnostic(&self) -> Option<String> {
        if self.is_abandoned() {
            return Some(crate::ensemble::WORKER_ABANDONMENT_REASON.into());
        }
        if let Some(message) = &self.connection_diagnostic {
            return Some(message.clone());
        }
        if self.inputs.active.is_some() || self.inputs.settled_generation == 0 {
            return None;
        }
        if let Some(error) = &self.settlement_failure {
            return Some(format!(
                "Feedback was not incorporated: {error}. The preceding successful plan is retained; confirm it explicitly or retry."
            ));
        }
        self.proposals
            .retained
            .as_ref()
            .is_none_or(|snapshot| snapshot.revision.generation < self.inputs.settled_generation)
            .then(|| {
                "Discussion completed. Publish a fresh, complete Markdown plan before confirmation."
                    .into()
            })
    }

    pub fn new(descriptor: AgentRunDescriptor) -> Self {
        Self {
            evidence: AgentRunOutcome {
                descriptor: descriptor.clone(),
                status: AgentRunStatus::Queued,
                report: String::new(),
                plan: None,
                confirmation: None,
                partial: false,
                failure: None,
                usage: None,
                acp_session_id: None,
                user_decisions: Vec::new(),
                decision_ids: Vec::new(),
                unavailable_decisions: Vec::new(),
            },
            descriptor,
            lifecycle: WorkerReviewLifecycle::Active,
            inputs: InputQueue::default(),
            proposals: ProposalState {
                next_revision: 1,
                ..ProposalState::default()
            },
            connection: ConnectionState::default(),
            images: ImageEvidence::default(),
            settlement_failure: None,
            connection_diagnostic: None,
        }
    }
    pub fn cancellable_generation(&self) -> Option<u64> {
        if self.is_abandoned() || self.is_sealed() {
            return None;
        }
        self.inputs
            .active
            .as_ref()
            .or(self.inputs.pending.front())
            .map(|input| input.generation)
    }
    pub fn quiescent(&self) -> bool {
        self.inputs.active.is_none()
            && self.inputs.pending.is_empty()
            && self.inputs.accepted_generation == self.inputs.settled_generation
    }
    pub fn eligible_snapshot(&self) -> Option<&WorkerPlanSnapshot> {
        self.proposals.retained.as_ref().filter(|snapshot| {
            !self.is_abandoned()
                && !self.is_sealed()
                && self.quiescent()
                && !self.proposals.retained_removed
                && self.proposals.synthesis_error.is_none()
                && snapshot.revision.generation >= self.proposals.publication_floor
        })
    }
    pub fn status(&self) -> AgentRunStatus {
        if self.is_abandoned() {
            AgentRunStatus::Abandoned
        } else if self.is_sealed() {
            AgentRunStatus::Completed
        } else if self.inputs.active.is_some() {
            AgentRunStatus::Running
        } else if !self.inputs.pending.is_empty() {
            AgentRunStatus::Queued
        } else if self.proposals.confirmation.is_some() {
            AgentRunStatus::Confirmed
        } else if self.eligible_snapshot().is_some() {
            AgentRunStatus::AwaitingConfirmation
        } else if self.evidence.failure.is_some() || self.proposals.synthesis_error.is_some() {
            AgentRunStatus::Blocked
        } else if self.connection.connected {
            AgentRunStatus::AwaitingFeedback
        } else {
            AgentRunStatus::Blocked
        }
    }
    pub fn confirmed_plan(&self) -> Option<ConfirmedWorkerPlan> {
        if self.is_abandoned() {
            return None;
        }
        let snapshot = if self.is_sealed() {
            self.proposals.retained.as_ref()?
        } else {
            self.eligible_snapshot()?
        };
        let confirmed = ConfirmedWorkerPlan {
            snapshot: snapshot.clone(),
            receipt: self.proposals.confirmation.clone()?,
            baseline: self.proposals.baseline.clone(),
        };
        confirmed.validate(&self.descriptor.id).then_some(confirmed)
    }
    pub fn outcome(&self) -> AgentRunOutcome {
        let mut outcome = self.evidence.clone();
        if self.is_abandoned() {
            outcome.sanitize_abandonment();
            return outcome;
        }
        outcome.report.clear();
        outcome.plan = self
            .proposals
            .retained
            .as_ref()
            .map(|snapshot| snapshot.plan.clone());
        outcome.confirmation = self.confirmed_plan().map(Box::new);
        outcome.status = AgentRunStatus::Completed;
        outcome.partial = false;
        outcome.failure = None;
        outcome
    }
    pub fn apply(&mut self, event: &WorkerReviewEvent) -> Result<(), String> {
        self.validate_transition(event)?;
        self.apply_validated_transition(event);
        Ok(())
    }

    fn invalidate_confirmation(&mut self) {
        self.proposals.confirmation = None;
        self.proposals.baseline = None;
    }

    fn validate_receipt(&self, receipt: &WorkerConfirmationReceipt) -> Result<(), String> {
        let snapshot = self
            .eligible_snapshot()
            .ok_or("worker has no quiescent, eligible plan revision")?;
        if !receipt.validates_snapshot(snapshot, &self.descriptor.id)
            || self
                .proposals
                .confirmation
                .as_ref()
                .is_some_and(|original| {
                    !original.validates_snapshot(snapshot, &self.descriptor.id)
                        || original.target.run_id != receipt.target.run_id
                })
        {
            return Err("stale or foreign plan revision".into());
        }
        Ok(())
    }

    /// Validate a transition against the current state without copying it.
    pub fn validate_transition(&self, event: &WorkerReviewEvent) -> Result<(), String> {
        if self.is_abandoned() {
            return Err("worker is permanently abandoned for this run".into());
        }
        if self.is_sealed() {
            return Err("worker review is sealed".into());
        }
        match event {
            WorkerReviewEvent::Fatal { error } => return Err(error.clone()),
            WorkerReviewEvent::Interrupted { generation } => {
                if self.inputs.active.as_ref().map(|input| input.generation) != Some(*generation) {
                    return Err("settlement belongs to an inactive input generation".into());
                }
                if self.evidence.descriptor != self.descriptor {
                    return Err("foreign worker evidence".into());
                }
            }
            WorkerReviewEvent::CancelRequested { generation } => {
                if self.cancellable_generation() != Some(*generation) {
                    return Err("cancellation belongs to an inactive input generation".into());
                }
            }
            WorkerReviewEvent::InputAccepted { input } => {
                if input.generation != self.inputs.accepted_generation.saturating_add(1)
                    || input.text.is_blank()
                    || input.text.validate().is_err()
                    || input.request_id.0.trim().is_empty()
                    || input.request_id.0.len() > 128
                    || (input.kind == WorkerPromptKind::UserFeedback
                        && input.text.text_len() > MAX_WORKER_FEEDBACK_BYTES)
                    || (input.kind == WorkerPromptKind::RecoveryContinuation
                        && self
                            .images
                            .failed_image_input
                            .as_ref()
                            .is_some_and(|failed| failed.text != input.text))
                    || ((input.kind == WorkerPromptKind::Initial) != (input.generation == 1))
                {
                    return Err("invalid worker input generation or prompt".into());
                }
            }
            WorkerReviewEvent::Dispatched { generation, .. } => {
                if self.inputs.active.is_some()
                    || self.inputs.pending.front().map(|input| input.generation)
                        != Some(*generation)
                {
                    return Err("worker prompt is overlapping or out of order".into());
                }
            }
            WorkerReviewEvent::Recovering {
                generation,
                attempt,
            } => {
                if self.inputs.active.as_ref().map(|input| input.generation) != Some(*generation)
                    || *attempt <= self.connection.attempt
                {
                    return Err(
                        "recovery belongs to an inactive generation or stale attempt".into(),
                    );
                }
            }
            WorkerReviewEvent::Published {
                generation,
                replay: false,
                ..
            } => {
                if self.inputs.active.as_ref().map(|input| input.generation) != Some(*generation) {
                    return Err("publication belongs to an inactive input generation".into());
                }
            }
            WorkerReviewEvent::Settled {
                generation,
                evidence,
                ..
            } => {
                if self.inputs.active.as_ref().map(|input| input.generation) != Some(*generation) {
                    return Err("settlement belongs to an inactive input generation".into());
                }
                if evidence.descriptor != self.descriptor {
                    return Err("foreign worker evidence".into());
                }
            }
            WorkerReviewEvent::Decisions {
                decision,
                unavailable,
            } => {
                if self.inputs.active.is_none() || decision.is_some() == unavailable.is_some() {
                    return Err("accepted worker decision requires an active interaction and exactly one representation".into());
                }
            }
            WorkerReviewEvent::Confirmed { receipt } => {
                self.validate_receipt(receipt)?;
                if self.proposals.confirmation.is_some() {
                    return Err("worker is already confirmed".into());
                }
            }
            WorkerReviewEvent::BaselineMarked { receipt } => {
                self.validate_receipt(receipt)?;
                if self.proposals.baseline.is_some() {
                    return Err("worker is already the baseline".into());
                }
            }
            WorkerReviewEvent::BaselineCleared { request_id } => {
                if !request_id.validate() || self.proposals.baseline.is_none() {
                    return Err("worker is not the baseline or request identity is invalid".into());
                }
            }
            WorkerReviewEvent::Withdrawn { .. } => {
                if self.proposals.confirmation.is_none() {
                    return Err("worker is not confirmed".into());
                }
            }
            WorkerReviewEvent::Abandoned { request_id } => {
                if request_id.0.trim().is_empty() || request_id.0.len() > 128 {
                    return Err("invalid abandonment request identity".into());
                }
            }
            WorkerReviewEvent::Sealed => {
                if self.confirmed_plan().is_none() {
                    return Err("cannot seal an unconfirmed worker".into());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Commit a transition validated against this unchanged state.
    #[doc(hidden)]
    pub fn apply_validated_transition(&mut self, event: &WorkerReviewEvent) {
        match event {
            WorkerReviewEvent::ImageCapability { supported } => {
                self.images.image_capability = Some(*supported)
            }
            WorkerReviewEvent::Fatal { .. } => {
                unreachable!("fatal transitions cannot be validated")
            }
            WorkerReviewEvent::CancelRequested { generation } => {
                self.inputs.cancel_requested = Some(*generation);
            }
            WorkerReviewEvent::InputAccepted { input } => {
                self.inputs.accepted_generation = input.generation;
                self.inputs.pending.push_back(input.clone());
                self.invalidate_confirmation();
            }
            WorkerReviewEvent::Dispatched { attempt, .. } => {
                self.inputs.active = self.inputs.pending.pop_front();
                self.connection.attempt = *attempt;
                self.proposals.candidate = None;
                self.connection_diagnostic = None;
            }
            WorkerReviewEvent::Recovering { attempt, .. } => {
                self.connection.attempt = *attempt;
                self.connection.connected = false;
                self.images.image_capability = None;
            }
            WorkerReviewEvent::Interrupted { generation } => {
                self.apply_validated_transition(&WorkerReviewEvent::Settled {
                    generation: *generation, connected: false, evidence: Box::new(self.evidence.clone()),
                    failure: Some("process stopped during this interaction; delivery is ambiguous, so its text was not resent".into()),
                });
            }
            WorkerReviewEvent::Published {
                generation,
                plan,
                replay,
            } => {
                if *replay {
                    return;
                }
                if let Some(markdown) = plan
                    .markdown
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                {
                    self.proposals.candidate =
                        Some(WorkerPlanSnapshot(std::sync::Arc::new(WorkerPlanData {
                            revision: WorkerPlanRevision {
                                worker_id: self.descriptor.id.clone(),
                                generation: *generation,
                                revision: self.proposals.next_revision,
                                digest: crate::ensemble::sha256_hex(markdown.as_bytes()),
                            },
                            plan: plan.clone(),
                        })));
                    self.proposals.next_revision += 1;
                }
            }
            WorkerReviewEvent::Removed { plan_id } => {
                if self
                    .proposals
                    .candidate
                    .as_ref()
                    .and_then(|snapshot| snapshot.plan.plan_id.as_ref())
                    == Some(plan_id)
                {
                    self.proposals.candidate = None;
                }
                if self
                    .proposals
                    .retained
                    .as_ref()
                    .and_then(|snapshot| snapshot.plan.plan_id.as_ref())
                    == Some(plan_id)
                {
                    self.proposals.retained_removed = true;
                    self.invalidate_confirmation();
                }
            }
            WorkerReviewEvent::Settled {
                generation,
                failure,
                connected,
                evidence,
            } => {
                let mut user_decisions = std::mem::take(&mut self.evidence.user_decisions);
                let mut decision_ids = std::mem::take(&mut self.evidence.decision_ids);
                let mut unavailable_decisions =
                    std::mem::take(&mut self.evidence.unavailable_decisions);
                crate::ensemble::merge_decision_evidence(
                    &mut user_decisions,
                    &mut decision_ids,
                    &mut unavailable_decisions,
                    &evidence.user_decisions,
                    &evidence.decision_ids,
                    &evidence.unavailable_decisions,
                );
                self.evidence = AgentRunOutcome {
                    descriptor: evidence.descriptor.clone(),
                    status: evidence.status,
                    report: evidence.report.clone(),
                    plan: evidence.plan.clone(),
                    confirmation: None,
                    partial: evidence.partial,
                    failure: evidence.failure.clone(),
                    usage: evidence.usage.clone(),
                    acp_session_id: evidence.acp_session_id.clone(),
                    user_decisions,
                    decision_ids,
                    unavailable_decisions,
                };
                self.connection.connected = *connected;
                if !connected {
                    self.images.image_capability = None;
                }
                self.inputs.settled_generation = *generation;
                let input = self
                    .inputs
                    .active
                    .as_ref()
                    .expect("settlement has an active generation");
                let retry = input.kind == WorkerPromptKind::RecoveryContinuation
                    && self
                        .images
                        .failed_image_input
                        .as_ref()
                        .is_some_and(|failed| failed.text == input.text);
                if failure.is_none() {
                    if input.text.has_images()
                        && (input.kind == WorkerPromptKind::UserFeedback
                            || (retry
                                && self
                                    .images
                                    .failed_image_input
                                    .as_ref()
                                    .is_some_and(|failed| {
                                        failed.kind == WorkerPromptKind::UserFeedback
                                    })))
                    {
                        self.images.incorporated_images.push(input.clone());
                    }
                    self.images.failed_image_input = None;
                } else if !retry {
                    self.images.failed_image_input = input.text.has_images().then(|| input.clone());
                }
                self.inputs.active = None;
                self.invalidate_confirmation();
                self.inputs.cancel_requested = None;
                self.settlement_failure.clone_from(failure);
                self.connection_diagnostic = None;
                if failure.is_some() {
                    self.proposals.candidate = None;
                } else {
                    self.proposals.publication_floor = *generation;
                    if let Some(snapshot) = self.proposals.candidate.take() {
                        self.proposals.retained = Some(snapshot);
                        self.proposals.retained_removed = false;
                    }
                }
            }
            WorkerReviewEvent::Decisions {
                decision,
                unavailable,
            } => {
                crate::ensemble::merge_decision_evidence(
                    &mut self.evidence.user_decisions,
                    &mut self.evidence.decision_ids,
                    &mut self.evidence.unavailable_decisions,
                    decision.as_slice(),
                    &[],
                    unavailable.as_slice(),
                );
            }
            WorkerReviewEvent::PayloadChecked { error } => {
                self.proposals.synthesis_error.clone_from(error);
                if error.is_some() {
                    self.invalidate_confirmation();
                }
            }
            WorkerReviewEvent::Connection {
                connected,
                diagnostic,
            } => {
                self.connection.connected = *connected;
                if !connected {
                    self.images.image_capability = None;
                }
                if diagnostic.is_some() {
                    self.connection_diagnostic.clone_from(diagnostic);
                }
            }
            WorkerReviewEvent::Confirmed { receipt } => {
                self.proposals.confirmation = Some(receipt.clone());
            }
            WorkerReviewEvent::BaselineMarked { receipt } => {
                if self.proposals.confirmation.is_none() {
                    self.proposals.confirmation = Some(receipt.clone());
                }
                self.proposals.baseline = Some(receipt.clone());
            }
            WorkerReviewEvent::BaselineCleared { .. } => {
                self.proposals.baseline = None;
            }
            WorkerReviewEvent::Withdrawn { .. } => {
                self.invalidate_confirmation();
            }
            WorkerReviewEvent::Abandoned { .. } => {
                self.lifecycle = WorkerReviewLifecycle::Abandoned;
                self.invalidate_confirmation();
                self.inputs.active = None;
                self.inputs.pending.clear();
                self.images.failed_image_input = None;
                self.images.image_capability = None;
                // Queue disposal, not successful incorporation of feedback.
                self.inputs.settled_generation = self.inputs.accepted_generation;
                self.connection.connected = false;
                self.inputs.cancel_requested = None;
                self.proposals.synthesis_error = None;
                self.connection_diagnostic = None;
                self.settlement_failure = None;
            }
            WorkerReviewEvent::Sealed => {
                self.lifecycle = WorkerReviewLifecycle::Sealed;
                self.connection.connected = false;
            }
        }
    }
}

/// One atomic run transition. A mark displaces other marks as a deterministic
/// consequence, never as a second independently authoritative root record.
/// All consumers (live control, replay, validation and display) use this reducer.
pub fn apply_worker_review_event(
    states: &mut [WorkerReviewState],
    worker_id: &AgentRunId,
    event: &WorkerReviewEvent,
) -> Result<Vec<AgentRunId>, String> {
    let changes = validate_worker_review_event(states, worker_id, event)?;
    let mut affected = Vec::with_capacity(changes.displaced.len() + 1);
    affected.push(worker_id.clone());
    states[changes.target].apply_validated_transition(event);
    if let Some(clear) = changes.clear {
        for index in changes.displaced {
            let state = &mut states[index];
            state.apply_validated_transition(&clear);
            affected.push(state.descriptor.id.clone());
        }
    }
    Ok(affected)
}

pub struct WorkerReviewChanges {
    pub target: usize,
    pub displaced: Vec<usize>,
    pub clear: Option<WorkerReviewEvent>,
}

pub fn validate_worker_review_event(
    states: &[WorkerReviewState],
    worker_id: &AgentRunId,
    event: &WorkerReviewEvent,
) -> Result<WorkerReviewChanges, String> {
    let target = states
        .iter()
        .position(|state| &state.descriptor.id == worker_id)
        .ok_or("foreign worker review transition")?;
    states[target].validate_transition(event)?;
    let mut displaced = Vec::new();
    let clear = if let WorkerReviewEvent::BaselineMarked { receipt } = event {
        let clear = WorkerReviewEvent::BaselineCleared {
            request_id: receipt.request_id.clone(),
        };
        for (index, state) in states
            .iter()
            .enumerate()
            .filter(|(index, state)| *index != target && state.proposals.baseline.is_some())
        {
            state.validate_transition(&clear)?;
            displaced.push(index);
        }
        Some(clear)
    } else {
        None
    };
    Ok(WorkerReviewChanges {
        target,
        displaced,
        clear,
    })
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn snapshots_verify_exact_bytes_at_the_wire_boundary() {
        let markdown = "# Exact plan\n";
        let mut wire = serde_json::json!({
            "revision": {
                "worker_id": "worker", "generation": 1, "revision": 1,
                "digest": crate::ensemble::sha256_hex(markdown.as_bytes())
            },
            "plan": { "markdown": markdown, "entries": [] }
        });
        let snapshot: WorkerPlanSnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(snapshot).unwrap(), wire);
        wire["plan"]["markdown"] = "# Exact plan".into();
        assert!(serde_json::from_value::<WorkerPlanSnapshot>(wire).is_err());
    }
}
