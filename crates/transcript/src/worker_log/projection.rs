//! Durable worker evidence reduction and ACP replay boundaries.

use std::sync::Arc;
use zevria_workflow::{
    AgentElicitationOutcome, AgentRunEvent, AgentRunOutcome, AgentRunRepair, AgentRunStatus,
    AgentStructuredPlan, AgentUnavailableDecision, AgentUsage, AgentUserDecisionBatch,
    AgentUserDecisionId, EnsembleWorkflow, merge_decision_evidence, merge_unavailable_decisions,
    reduce_agent_plan_event,
};

use super::{AgentRunTranscriptHeader, AgentRunTranscriptRecord};

/// Report/session projection reduced solely from a durable worker log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRunProjection {
    pub header: Option<AgentRunTranscriptHeader>,
    pub report: String,
    pub plan: Option<AgentStructuredPlan>,
    pub native_capture: Option<zevria_workflow::NativePlanCapture>,
    pub usage: Option<AgentUsage>,
    pub acp_session_id: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub outcome: Option<AgentRunOutcome>,
    pub user_decisions: Vec<AgentUserDecisionBatch>,
    pub decision_ids: Vec<AgentUserDecisionId>,
    pub unavailable_decisions: Vec<AgentUnavailableDecision>,
    pub repair: Option<AgentRunRepair>,
    pub review: Option<Arc<crate::WorkerReviewJournal>>,
    /// Non-fallible in-memory projections retain errors; file readers reject them.
    pub review_error: Option<String>,
    last_message_id: Option<String>,
    report_fenced: bool,
    outcome_after_latest_prompt: bool,
    replay: Option<ReplayProjection>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ReplayProjection {
    report: String,
    plan: Option<AgentStructuredPlan>,
    native_capture: Option<zevria_workflow::NativePlanCapture>,
    usage: Option<AgentUsage>,
    last_message_id: Option<String>,
    report_fenced: bool,
}

impl AgentRunProjection {
    pub fn from_records(records: &[AgentRunTranscriptRecord]) -> Self {
        let mut projection = Self::default();
        for record in records {
            projection.apply(record);
        }
        projection
    }

    /// Return only an authoritative durable outcome for the current prompt.
    pub fn recoverable_outcome(&self) -> Option<AgentRunOutcome> {
        if self.outcome_after_latest_prompt
            && let Some(outcome) = &self.outcome
            && outcome.status.is_terminal()
        {
            let mut outcome = outcome.clone();
            if outcome.status == AgentRunStatus::Abandoned {
                return Some(outcome);
            }
            merge_decision_evidence(
                &mut outcome.user_decisions,
                &mut outcome.decision_ids,
                &mut outcome.unavailable_decisions,
                &self.user_decisions,
                &self.decision_ids,
                &self.unavailable_decisions,
            );
            return Some(outcome);
        }
        None
    }

    pub fn apply(&mut self, record: &AgentRunTranscriptRecord) {
        if let AgentRunTranscriptRecord::Header { header } = record {
            self.review = (header.workflow == EnsembleWorkflow::Plan)
                .then(|| Arc::new(crate::WorkerReviewJournal::new(header)));
        }
        if crate::WorkerReviewJournal::observes(record)
            && let Some(review) = &mut self.review
            && let Err(error) = Arc::make_mut(review).apply(record)
        {
            self.review_error = Some(error);
        }
        self.apply_display(record);
    }

    /// Reduce report and outcome evidence without collecting a second review
    /// journal. Use only for records that have already been validated.
    pub fn apply_display(&mut self, record: &AgentRunTranscriptRecord) {
        match record {
            AgentRunTranscriptRecord::Header { header } => self.header = Some(header.clone()),
            AgentRunTranscriptRecord::Outcome { outcome } => {
                merge_decision_evidence(
                    &mut self.user_decisions,
                    &mut self.decision_ids,
                    &mut self.unavailable_decisions,
                    &outcome.user_decisions,
                    &outcome.decision_ids,
                    &outcome.unavailable_decisions,
                );
                let outcome = outcome.clone();
                if self.acp_session_id.is_none() {
                    self.acp_session_id.clone_from(&outcome.acp_session_id);
                }
                self.outcome = Some(outcome);
                self.outcome_after_latest_prompt = true;
            }
            AgentRunTranscriptRecord::Event { event } => match event {
                AgentRunEvent::SessionAllocated { session_id } => {
                    self.acp_session_id = Some(session_id.clone());
                }
                AgentRunEvent::Prompt {
                    continuation: false,
                    repair,
                    ..
                } => {
                    self.report.clear();
                    self.plan = None;
                    self.native_capture = None;
                    self.repair.clone_from(repair);
                    self.outcome_after_latest_prompt = false;
                    self.last_message_id = None;
                    self.report_fenced = false;
                    self.replay = None;
                    self.user_decisions.clear();
                    self.decision_ids.clear();
                    self.unavailable_decisions.clear();
                    if let Some(outcome) = &mut self.outcome {
                        outcome.user_decisions.clear();
                        outcome.decision_ids.clear();
                        outcome.unavailable_decisions.clear();
                    }
                }
                AgentRunEvent::Prompt {
                    continuation: true,
                    repair,
                    ..
                } => {
                    if repair.is_some() {
                        self.repair.clone_from(repair);
                    }
                    self.outcome_after_latest_prompt = false;
                }
                AgentRunEvent::ReplayBoundary => {
                    self.replay = Some(ReplayProjection::default());
                }
                AgentRunEvent::AgentMessage { text, message_id } => {
                    if let Some(replay) = &mut self.replay {
                        append_report_delta(
                            &mut replay.report,
                            &mut replay.last_message_id,
                            &mut replay.report_fenced,
                            text,
                            message_id,
                        );
                    } else {
                        append_report_delta(
                            &mut self.report,
                            &mut self.last_message_id,
                            &mut self.report_fenced,
                            text,
                            message_id,
                        );
                    }
                }
                event @ (AgentRunEvent::Plan { .. }
                | AgentRunEvent::NativePlanCaptured { .. }
                | AgentRunEvent::PlanRemoved { .. }) => match &mut self.replay {
                    Some(replay) => {
                        if reduce_agent_plan_event(&mut replay.plan, event) {
                            replay.native_capture = match event {
                                AgentRunEvent::NativePlanCaptured { capture, .. } => {
                                    Some(capture.clone())
                                }
                                _ => None,
                            };
                        }
                    }
                    None => {
                        if reduce_agent_plan_event(&mut self.plan, event) {
                            self.native_capture = match event {
                                AgentRunEvent::NativePlanCaptured { capture, .. } => {
                                    Some(capture.clone())
                                }
                                _ => None,
                            };
                        }
                    }
                },
                AgentRunEvent::Usage { usage } => match &mut self.replay {
                    Some(replay) => replay.usage = Some(usage.clone()),
                    None => self.usage = Some(usage.clone()),
                },
                AgentRunEvent::SessionEstablished {
                    session_id,
                    capabilities,
                    recovered,
                    ..
                } => {
                    if *recovered && let Some(replay) = self.replay.take() {
                        self.report = replay.report;
                        if self.native_capture.is_none() || replay.native_capture.is_some() {
                            self.plan = replay.plan;
                            self.native_capture = replay.native_capture;
                        }
                        self.last_message_id = replay.last_message_id;
                        self.report_fenced = replay.report_fenced;
                        if replay.usage.is_some() {
                            self.usage = replay.usage;
                        }
                    }
                    self.acp_session_id = Some(session_id.clone());
                    self.capabilities = Some(capabilities.clone());
                }
                AgentRunEvent::Elicitation {
                    field_count: _,
                    outcome: AgentElicitationOutcome::Accepted,
                    decision,
                    decision_unavailable,
                } => {
                    if let Some(decision) = decision {
                        merge_decision_evidence(
                            &mut self.user_decisions,
                            &mut self.decision_ids,
                            &mut self.unavailable_decisions,
                            std::slice::from_ref(decision),
                            &[],
                            &[],
                        );
                    } else if let Some(unavailable) = decision_unavailable {
                        merge_unavailable_decisions(
                            &mut self.unavailable_decisions,
                            std::slice::from_ref(unavailable),
                        );
                    }
                }
                AgentRunEvent::ResponseDisplay { .. }
                | AgentRunEvent::Review { .. }
                | AgentRunEvent::Status { .. }
                | AgentRunEvent::Failure { .. }
                | AgentRunEvent::UserMessage { .. }
                | AgentRunEvent::UserImage { .. }
                | AgentRunEvent::Thought { .. }
                | AgentRunEvent::ToolCall { .. }
                | AgentRunEvent::ToolCallUpdate { .. }
                | AgentRunEvent::ToolResultMetadata { .. }
                | AgentRunEvent::ModeChanged { .. }
                | AgentRunEvent::ConfigOptionsChanged { .. }
                | AgentRunEvent::SessionInfo { .. }
                | AgentRunEvent::Permission { .. }
                | AgentRunEvent::Elicitation { .. }
                | AgentRunEvent::Stderr { .. }
                | AgentRunEvent::Protocol { .. }
                | AgentRunEvent::Unsupported { .. } => {}
            },
        }
        if let AgentRunTranscriptRecord::Event { event } = record
            && report_segment_boundary(event)
        {
            if let Some(replay) = &mut self.replay {
                replay.report_fenced = true;
            } else {
                self.report_fenced = true;
            }
        }
    }
}

fn append_report_delta(
    report: &mut String,
    last_message_id: &mut Option<String>,
    report_fenced: &mut bool,
    text: &str,
    message_id: &Option<String>,
) {
    if !report.is_empty() && (*report_fenced || last_message_id.as_ref() != message_id.as_ref()) {
        report.push('\n');
    }
    report.push_str(text);
    last_message_id.clone_from(message_id);
    *report_fenced = false;
}

fn report_segment_boundary(event: &AgentRunEvent) -> bool {
    matches!(
        event,
        AgentRunEvent::Prompt {
            continuation: true,
            ..
        } | AgentRunEvent::UserMessage { .. }
            | AgentRunEvent::Thought { .. }
            | AgentRunEvent::ToolCall { .. }
            | AgentRunEvent::ToolCallUpdate { .. }
            | AgentRunEvent::Plan { .. }
            | AgentRunEvent::NativePlanCaptured { .. }
            | AgentRunEvent::PlanRemoved { .. }
            | AgentRunEvent::Elicitation { .. }
            | AgentRunEvent::Unsupported { .. }
            | AgentRunEvent::Failure { .. }
    )
}
