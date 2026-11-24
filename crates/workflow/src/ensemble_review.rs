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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReviewState {
    pub descriptor: AgentRunDescriptor,
    pub image_capability: Option<bool>,
    /// Exact successfully incorporated image feedback, in generation order.
    pub incorporated_images: Vec<WorkerInput>,
    /// Most recent failed image input. Explicit retry must retain these exact
    /// bytes even when a session ID was allocated before image dispatch failed.
    pub failed_image_input: Option<WorkerInput>,
    pub accepted_generation: u64,
    pub settled_generation: u64,
    pub pending: VecDeque<WorkerInput>,
    pub active: Option<WorkerInput>,
    pub attempt: u64,
    pub next_revision: u64,
    pub candidate: Option<WorkerPlanSnapshot>,
    pub retained: Option<WorkerPlanSnapshot>,
    /// Last successful interaction that requires republication. Failed rounds
    /// never move this backwards, even if a later failed round published a draft.
    pub publication_floor: u64,
    pub retained_removed: bool,
    pub connected: bool,
    pub confirmation: Option<WorkerConfirmationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<WorkerConfirmationReceipt>,
    pub sealed: bool,
    #[serde(default)]
    pub abandoned: bool,
    pub diagnostic: Option<String>,
    pub cancel_requested: Option<u64>,
    pub synthesis_error: Option<String>,
    pub evidence: AgentRunOutcome,
}
impl WorkerReviewState {
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
            image_capability: None,
            incorporated_images: Vec::new(),
            failed_image_input: None,
            accepted_generation: 0,
            settled_generation: 0,
            pending: VecDeque::new(),
            active: None,
            attempt: 0,
            next_revision: 1,
            candidate: None,
            retained: None,
            publication_floor: 0,
            retained_removed: false,
            connected: false,
            confirmation: None,
            baseline: None,
            sealed: false,
            abandoned: false,
            diagnostic: None,
            cancel_requested: None,
            synthesis_error: None,
        }
    }
    pub fn cancellable_generation(&self) -> Option<u64> {
        if self.abandoned || self.sealed {
            return None;
        }
        self.active
            .as_ref()
            .or(self.pending.front())
            .map(|input| input.generation)
    }
    pub fn quiescent(&self) -> bool {
        self.active.is_none()
            && self.pending.is_empty()
            && self.accepted_generation == self.settled_generation
    }
    pub fn eligible_snapshot(&self) -> Option<&WorkerPlanSnapshot> {
        self.retained.as_ref().filter(|snapshot| {
            !self.abandoned
                && !self.sealed
                && self.quiescent()
                && !self.retained_removed
                && self.synthesis_error.is_none()
                && snapshot.revision.generation >= self.publication_floor
        })
    }
    pub fn status(&self) -> AgentRunStatus {
        if self.abandoned {
            AgentRunStatus::Abandoned
        } else if self.sealed {
            AgentRunStatus::Completed
        } else if self.active.is_some() {
            AgentRunStatus::Running
        } else if !self.pending.is_empty() {
            AgentRunStatus::Queued
        } else if self.confirmation.is_some() {
            AgentRunStatus::Confirmed
        } else if self.eligible_snapshot().is_some() {
            AgentRunStatus::AwaitingConfirmation
        } else if self.evidence.failure.is_some() || self.synthesis_error.is_some() {
            AgentRunStatus::Blocked
        } else if self.connected {
            AgentRunStatus::AwaitingFeedback
        } else {
            AgentRunStatus::Blocked
        }
    }
    pub fn confirmed_plan(&self) -> Option<ConfirmedWorkerPlan> {
        if self.abandoned {
            return None;
        }
        let snapshot = if self.sealed {
            self.retained.as_ref()?
        } else {
            self.eligible_snapshot()?
        };
        let confirmed = ConfirmedWorkerPlan {
            snapshot: snapshot.clone(),
            receipt: self.confirmation.clone()?,
            baseline: self.baseline.clone(),
        };
        confirmed.validate(&self.descriptor.id).then_some(confirmed)
    }
    pub fn outcome(&self) -> AgentRunOutcome {
        let mut outcome = self.evidence.clone();
        if self.abandoned {
            outcome.sanitize_abandonment();
            return outcome;
        }
        outcome.report.clear();
        outcome.plan = self.retained.as_ref().map(|snapshot| snapshot.plan.clone());
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
        self.confirmation = None;
        self.baseline = None;
    }

    fn validate_receipt(&self, receipt: &WorkerConfirmationReceipt) -> Result<(), String> {
        let snapshot = self
            .eligible_snapshot()
            .ok_or("worker has no quiescent, eligible plan revision")?;
        if !receipt.validates_snapshot(snapshot, &self.descriptor.id)
            || self.confirmation.as_ref().is_some_and(|original| {
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
        if self.abandoned {
            return Err("worker is permanently abandoned for this run".into());
        }
        if self.sealed {
            return Err("worker review is sealed".into());
        }
        match event {
            WorkerReviewEvent::Fatal { error } => return Err(error.clone()),
            WorkerReviewEvent::Interrupted { generation } => {
                if self.active.as_ref().map(|input| input.generation) != Some(*generation) {
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
                if input.generation != self.accepted_generation.saturating_add(1)
                    || input.text.is_blank()
                    || input.text.validate().is_err()
                    || input.request_id.0.trim().is_empty()
                    || input.request_id.0.len() > 128
                    || (input.kind == WorkerPromptKind::UserFeedback
                        && input.text.text_len() > MAX_WORKER_FEEDBACK_BYTES)
                    || (input.kind == WorkerPromptKind::RecoveryContinuation
                        && self
                            .failed_image_input
                            .as_ref()
                            .is_some_and(|failed| failed.text != input.text))
                    || ((input.kind == WorkerPromptKind::Initial) != (input.generation == 1))
                {
                    return Err("invalid worker input generation or prompt".into());
                }
            }
            WorkerReviewEvent::Dispatched { generation, .. } => {
                if self.active.is_some()
                    || self.pending.front().map(|input| input.generation) != Some(*generation)
                {
                    return Err("worker prompt is overlapping or out of order".into());
                }
            }
            WorkerReviewEvent::Recovering {
                generation,
                attempt,
            } => {
                if self.active.as_ref().map(|input| input.generation) != Some(*generation)
                    || *attempt <= self.attempt
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
                if self.active.as_ref().map(|input| input.generation) != Some(*generation) {
                    return Err("publication belongs to an inactive input generation".into());
                }
            }
            WorkerReviewEvent::Settled {
                generation,
                evidence,
                ..
            } => {
                if self.active.as_ref().map(|input| input.generation) != Some(*generation) {
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
                if self.active.is_none() || decision.is_some() == unavailable.is_some() {
                    return Err("accepted worker decision requires an active interaction and exactly one representation".into());
                }
            }
            WorkerReviewEvent::Confirmed { receipt } => {
                self.validate_receipt(receipt)?;
                if self.confirmation.is_some() {
                    return Err("worker is already confirmed".into());
                }
            }
            WorkerReviewEvent::BaselineMarked { receipt } => {
                self.validate_receipt(receipt)?;
                if self.baseline.is_some() {
                    return Err("worker is already the baseline".into());
                }
            }
            WorkerReviewEvent::BaselineCleared { request_id } => {
                if !request_id.validate() || self.baseline.is_none() {
                    return Err("worker is not the baseline or request identity is invalid".into());
                }
            }
            WorkerReviewEvent::Withdrawn { .. } => {
                if self.confirmation.is_none() {
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
                self.image_capability = Some(*supported)
            }
            WorkerReviewEvent::Fatal { .. } => {
                unreachable!("fatal transitions cannot be validated")
            }
            WorkerReviewEvent::CancelRequested { generation } => {
                self.cancel_requested = Some(*generation);
            }
            WorkerReviewEvent::InputAccepted { input } => {
                self.accepted_generation = input.generation;
                self.pending.push_back(input.clone());
                self.invalidate_confirmation();
            }
            WorkerReviewEvent::Dispatched { attempt, .. } => {
                self.active = self.pending.pop_front();
                self.attempt = *attempt;
                self.candidate = None;
                self.diagnostic = None;
            }
            WorkerReviewEvent::Recovering { attempt, .. } => {
                self.attempt = *attempt;
                self.connected = false;
                self.image_capability = None;
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
                    self.candidate =
                        Some(WorkerPlanSnapshot(std::sync::Arc::new(WorkerPlanData {
                            revision: WorkerPlanRevision {
                                worker_id: self.descriptor.id.clone(),
                                generation: *generation,
                                revision: self.next_revision,
                                digest: crate::ensemble::sha256_hex(markdown.as_bytes()),
                            },
                            plan: plan.clone(),
                        })));
                    self.next_revision += 1;
                }
            }
            WorkerReviewEvent::Removed { plan_id } => {
                if self
                    .candidate
                    .as_ref()
                    .and_then(|snapshot| snapshot.plan.plan_id.as_ref())
                    == Some(plan_id)
                {
                    self.candidate = None;
                }
                if self
                    .retained
                    .as_ref()
                    .and_then(|snapshot| snapshot.plan.plan_id.as_ref())
                    == Some(plan_id)
                {
                    self.retained_removed = true;
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
                self.connected = *connected;
                if !connected {
                    self.image_capability = None;
                }
                self.settled_generation = *generation;
                let input = self
                    .active
                    .as_ref()
                    .expect("settlement has an active generation");
                let retry = input.kind == WorkerPromptKind::RecoveryContinuation
                    && self
                        .failed_image_input
                        .as_ref()
                        .is_some_and(|failed| failed.text == input.text);
                if failure.is_none() {
                    if input.text.has_images()
                        && (input.kind == WorkerPromptKind::UserFeedback
                            || (retry
                                && self.failed_image_input.as_ref().is_some_and(|failed| {
                                    failed.kind == WorkerPromptKind::UserFeedback
                                })))
                    {
                        self.incorporated_images.push(input.clone());
                    }
                    self.failed_image_input = None;
                } else if !retry {
                    self.failed_image_input = input.text.has_images().then(|| input.clone());
                }
                self.active = None;
                self.invalidate_confirmation();
                self.cancel_requested = None;
                if let Some(error) = failure {
                    self.diagnostic = Some(format!(
                        "Feedback was not incorporated: {error}. The preceding successful plan is retained; confirm it explicitly or retry."
                    ));
                    self.candidate = None;
                } else {
                    self.publication_floor = *generation;
                    if let Some(snapshot) = self.candidate.take() {
                        self.retained = Some(snapshot);
                        self.retained_removed = false;
                    }
                    self.diagnostic = if self
                        .retained
                        .as_ref()
                        .is_none_or(|snapshot| snapshot.revision.generation < *generation)
                    {
                        Some("Discussion completed. Publish a fresh, complete Markdown plan before confirmation.".into())
                    } else {
                        None
                    };
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
                self.synthesis_error.clone_from(error);
                if error.is_some() {
                    self.invalidate_confirmation();
                }
            }
            WorkerReviewEvent::Connection {
                connected,
                diagnostic,
            } => {
                self.connected = *connected;
                if !connected {
                    self.image_capability = None;
                }
                if diagnostic.is_some() {
                    self.diagnostic.clone_from(diagnostic);
                }
            }
            WorkerReviewEvent::Confirmed { receipt } => {
                self.confirmation = Some(receipt.clone());
            }
            WorkerReviewEvent::BaselineMarked { receipt } => {
                if self.confirmation.is_none() {
                    self.confirmation = Some(receipt.clone());
                }
                self.baseline = Some(receipt.clone());
            }
            WorkerReviewEvent::BaselineCleared { .. } => {
                self.baseline = None;
            }
            WorkerReviewEvent::Withdrawn { .. } => {
                self.invalidate_confirmation();
            }
            WorkerReviewEvent::Abandoned { .. } => {
                self.abandoned = true;
                self.invalidate_confirmation();
                self.active = None;
                self.pending.clear();
                self.failed_image_input = None;
                self.image_capability = None;
                // Queue disposal, not successful incorporation of feedback.
                self.settled_generation = self.accepted_generation;
                self.connected = false;
                self.cancel_requested = None;
                self.synthesis_error = None;
                self.diagnostic = Some(crate::ensemble::WORKER_ABANDONMENT_REASON.into());
            }
            WorkerReviewEvent::Sealed => {
                self.sealed = true;
                self.connected = false;
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
            .filter(|(index, state)| *index != target && state.baseline.is_some())
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
