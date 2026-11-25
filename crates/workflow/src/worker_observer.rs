//! Shared interpretation of worker evidence for live review and journal replay.

use crate::{AgentElicitationOutcome, AgentRunEvent, WorkerReviewEvent};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkerEventObserver {
    generation: Option<u64>,
    replay: bool,
    publishing: bool,
}

#[derive(Clone, Copy)]
pub enum WorkerObservation {
    Live,
    Journal { has_inputs: bool },
}

impl WorkerEventObserver {
    pub fn new(replay: bool) -> Self {
        Self {
            replay,
            ..Self::default()
        }
    }

    pub fn observe_review(&mut self, event: &WorkerReviewEvent) {
        match event {
            WorkerReviewEvent::Dispatched { generation, .. } => {
                self.generation = Some(*generation);
                self.publishing = false;
            }
            WorkerReviewEvent::Settled { .. }
            | WorkerReviewEvent::Interrupted { .. }
            | WorkerReviewEvent::Abandoned { .. } => {
                self.generation = None;
                self.publishing = false;
            }
            WorkerReviewEvent::Recovering { .. } => self.publishing = false,
            _ => {}
        }
    }

    pub fn observe(
        &mut self,
        event: &AgentRunEvent,
        source: WorkerObservation,
    ) -> Result<Option<WorkerReviewEvent>, &'static str> {
        let has_inputs = !matches!(source, WorkerObservation::Journal { has_inputs: false });
        let transition = match event {
            AgentRunEvent::ReplayBoundary => {
                self.replay = true;
                self.publishing = false;
                None
            }
            AgentRunEvent::SessionEstablished { .. } => {
                self.replay = false;
                None
            }
            AgentRunEvent::Prompt { .. } => {
                self.publishing = self.generation.is_some() && !self.replay;
                None
            }
            AgentRunEvent::NativePlanCaptured { capture, .. }
                if (self.publishing || matches!(source, WorkerObservation::Live))
                    && Some(capture.generation) != self.generation =>
            {
                return Err(match source {
                    WorkerObservation::Live => {
                        "stale native capture does not belong to the active review generation"
                    }
                    WorkerObservation::Journal { .. } => {
                        "native capture generation does not match active review input"
                    }
                });
            }
            AgentRunEvent::Plan { plan } | AgentRunEvent::NativePlanCaptured { plan, .. }
                if self.publishing =>
            {
                let generation = self
                    .generation
                    .ok_or("publication without active worker input")?;
                Some(WorkerReviewEvent::Published {
                    generation,
                    plan: plan.clone(),
                    replay: self.replay,
                })
            }
            AgentRunEvent::Elicitation {
                outcome: AgentElicitationOutcome::Accepted,
                decision,
                decision_unavailable,
                ..
            } if has_inputs => Some(WorkerReviewEvent::Decisions {
                decision: decision.clone(),
                unavailable: decision_unavailable.clone(),
            }),
            AgentRunEvent::PlanRemoved { plan_id } if has_inputs && !self.replay => {
                Some(WorkerReviewEvent::Removed {
                    plan_id: plan_id.clone(),
                })
            }
            _ => None,
        };
        Ok(transition)
    }
}
