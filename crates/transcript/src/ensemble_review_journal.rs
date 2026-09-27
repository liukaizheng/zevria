//! Worker-journal review projection and root/worker crash-boundary reconciliation.
//! Root controls are authority; a worker journal can prove delivery/settlement,
//! never manufacture acceptance or confirmation.
use std::collections::BTreeMap;

use crate::{
    AgentRunEvent, AgentRunTranscriptHeader, AgentRunTranscriptRecord, WorkerInput,
    WorkerReviewEvent, WorkerReviewState,
};

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerReviewJournal {
    pub state: WorkerReviewState,
    pub inputs: BTreeMap<u64, WorkerInput>,
    /// Only worker-origin execution evidence, in durable observation order.
    pub events: Vec<WorkerReviewEvent>,
    pub abandonment: Option<crate::WorkerControlId>,
    run_id: crate::EnsembleRunId,
    publishing: bool,
    replay: bool,
    collect_events: bool,
}
impl WorkerReviewJournal {
    pub fn new(header: &AgentRunTranscriptHeader) -> Self {
        Self {
            state: WorkerReviewState::new(header.descriptor.clone()),
            inputs: BTreeMap::new(),
            events: Vec::new(),
            abandonment: None,
            run_id: header.ensemble_run_id.clone(),
            publishing: false,
            replay: false,
            collect_events: true,
        }
    }

    pub(crate) fn validator(header: &AgentRunTranscriptHeader) -> Self {
        let mut journal = Self::new(header);
        journal.collect_events = false;
        journal
    }
    pub(crate) fn observes(record: &AgentRunTranscriptRecord) -> bool {
        matches!(
            record,
            AgentRunTranscriptRecord::Outcome { .. }
                | AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Review { .. }
                        | AgentRunEvent::ReplayBoundary
                        | AgentRunEvent::SessionEstablished { .. }
                        | AgentRunEvent::Prompt { .. }
                        | AgentRunEvent::Plan { .. }
                        | AgentRunEvent::NativePlanCaptured { .. }
                        | AgentRunEvent::PlanRemoved { .. }
                        | AgentRunEvent::Elicitation { .. }
                }
        )
    }
    fn record_event(&mut self, event: WorkerReviewEvent) {
        if self.collect_events {
            self.events.push(event);
        }
    }

    pub fn apply(&mut self, record: &AgentRunTranscriptRecord) -> Result<(), String> {
        if self.state.abandoned
            && matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Review { .. } | AgentRunEvent::Prompt { .. }
                }
            )
        {
            return Err("worker review transition follows terminal abandonment".into());
        }
        match record {
            AgentRunTranscriptRecord::Event { event } => match event {
                AgentRunEvent::Review { event } => match event.as_ref() {
                    WorkerReviewEvent::InputAccepted { input } => {
                        if let Some(previous) = self.inputs.get(&input.generation) {
                            if previous != input {
                                return Err("conflicting worker input mirror".into());
                            }
                            return Ok(());
                        }
                        self.state.apply(event)?;
                        self.inputs.insert(input.generation, input.clone());
                    }
                    WorkerReviewEvent::Dispatched { .. }
                    | WorkerReviewEvent::Settled { .. }
                    | WorkerReviewEvent::Recovering { .. } => {
                        self.state.apply(event)?;
                        self.publishing = false;
                        self.record_event(*event.clone());
                    }
                    WorkerReviewEvent::Interrupted { .. } => {
                        self.state.apply(event)?;
                        self.publishing = false;
                    }
                    WorkerReviewEvent::Connection { .. }
                    | WorkerReviewEvent::ImageCapability { .. } => self.state.apply(event)?,
                    WorkerReviewEvent::Abandoned { request_id } => {
                        self.state.apply(event)?;
                        self.abandonment = Some(request_id.clone());
                        self.publishing = false;
                    }
                    // Host receipt mirrors are audit evidence. Root replay is
                    // the only authority for control acceptance and sealing.
                    WorkerReviewEvent::Confirmed { receipt }
                    | WorkerReviewEvent::BaselineMarked { receipt } => {
                        if receipt.target.run_id != self.run_id
                            || !self.state.retained.as_ref().is_some_and(|snapshot| {
                                receipt.validates_snapshot(snapshot, &self.state.descriptor.id)
                            })
                        {
                            return Err(
                                "worker confirmation mirror has no exact eligible snapshot".into(),
                            );
                        }
                    }
                    WorkerReviewEvent::CancelRequested { generation } => {
                        if *generation == 0 || *generation > self.state.accepted_generation {
                            return Err("worker cancellation mirror has no accepted input".into());
                        }
                        // This host mirror may follow actual settlement. Only
                        // root ordering determines pending cancellation intent.
                    }
                    WorkerReviewEvent::Withdrawn { .. }
                    | WorkerReviewEvent::BaselineCleared { .. }
                    | WorkerReviewEvent::Sealed
                    | WorkerReviewEvent::PayloadChecked { .. } => {}
                    WorkerReviewEvent::Fatal { .. }
                    | WorkerReviewEvent::Published { .. }
                    | WorkerReviewEvent::Removed { .. }
                    | WorkerReviewEvent::Decisions { .. } => {
                        return Err("worker journal uses raw durable Plan evidence, not synthetic publication mirrors".into());
                    }
                },
                AgentRunEvent::ReplayBoundary => {
                    self.replay = true;
                    self.publishing = false;
                }
                AgentRunEvent::SessionEstablished { .. } => {
                    self.replay = false;
                }
                AgentRunEvent::Prompt { .. } => {
                    self.publishing = self.state.active.is_some() && !self.replay;
                }
                AgentRunEvent::NativePlanCaptured { capture, .. }
                    if self.publishing
                        && self
                            .state
                            .active
                            .as_ref()
                            .is_none_or(|active| active.generation != capture.generation) =>
                {
                    return Err(
                        "native capture generation does not match active review input".into(),
                    );
                }
                AgentRunEvent::Plan { plan } | AgentRunEvent::NativePlanCaptured { plan, .. }
                    if self.publishing =>
                {
                    let generation = self
                        .state
                        .active
                        .as_ref()
                        .ok_or("publication without active worker input")?
                        .generation;
                    let event = WorkerReviewEvent::Published {
                        generation,
                        plan: plan.clone(),
                        replay: false,
                    };
                    self.state.apply(&event)?;
                    self.record_event(event);
                }
                AgentRunEvent::Elicitation {
                    outcome: crate::AgentElicitationOutcome::Accepted,
                    decision,
                    decision_unavailable,
                    ..
                } if !self.inputs.is_empty() => {
                    let event = WorkerReviewEvent::Decisions {
                        decision: decision.clone(),
                        unavailable: decision_unavailable.clone(),
                    };
                    self.state.apply(&event)?;
                    self.record_event(event);
                }
                AgentRunEvent::PlanRemoved { plan_id }
                    if !self.replay && !self.inputs.is_empty() =>
                {
                    let event = WorkerReviewEvent::Removed {
                        plan_id: plan_id.clone(),
                    };
                    self.state.apply(&event)?;
                    self.record_event(event);
                }
                _ => {}
            },
            AgentRunTranscriptRecord::Outcome { outcome }
                if outcome.status == crate::AgentRunStatus::Abandoned =>
            {
                if self.abandonment.is_none() || !outcome.is_sanitized_abandonment() {
                    return Err("abandoned outcome requires an abandonment audit mirror and sanitized evidence".into());
                }
            }
            AgentRunTranscriptRecord::Outcome { outcome } if self.state.abandoned => {
                return Err(format!(
                    "non-abandoned outcome after abandonment: {}",
                    outcome.status
                ));
            }
            AgentRunTranscriptRecord::Outcome { outcome }
                if !self.inputs.is_empty() && outcome.confirmation.is_some() =>
            {
                let confirmed = outcome.confirmation.as_ref().expect("guarded confirmation");
                // Root sealing orders controls and late provider updates.
                // A later idle removal cannot revoke an already sealed receipt.
                // The snapshot must still be the exact successful publication,
                // while eligibility at the seal is checked in root replay.
                if self.state.retained.as_ref() != Some(&confirmed.snapshot) {
                    return Err(
                        "terminal worker snapshot differs from its durable settled review evidence"
                            .into(),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Reconcile before any writable restoration or provider startup. Worker
    /// execution updates are a prefix in root history (controls may interleave).
    /// Missing suffix evidence is committed by the root before interrupted work
    /// is settled, so proven dispatch cannot be mistaken for unsent feedback.
    pub fn reconcile(&self, root: &[WorkerReviewEvent]) -> Result<Vec<WorkerReviewEvent>, String> {
        let root_abandonment = root.iter().find_map(|event| match event {
            WorkerReviewEvent::Abandoned { request_id } => Some(request_id),
            _ => None,
        });
        if self
            .abandonment
            .as_ref()
            .is_some_and(|request| Some(request) != root_abandonment)
        {
            return Err(
                "worker-only or mismatched abandonment has no authoritative root control".into(),
            );
        }
        if root_abandonment.is_some() {
            // Valid late execution evidence remains archived, never promoted.
            return Ok(Vec::new());
        }
        let accepted = root
            .iter()
            .filter_map(|event| match event {
                WorkerReviewEvent::InputAccepted { input } => Some((input.generation, input)),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        for (generation, input) in &self.inputs {
            if accepted.get(generation).copied() != Some(input) {
                return Err(
                    "worker journal contains input not exactly accepted by the root".into(),
                );
            }
        }
        let observed = root
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    WorkerReviewEvent::Dispatched { .. }
                        | WorkerReviewEvent::Recovering { .. }
                        | WorkerReviewEvent::Published { replay: false, .. }
                        | WorkerReviewEvent::Removed { .. }
                        | WorkerReviewEvent::Settled { .. }
                        | WorkerReviewEvent::Decisions { .. }
                )
            })
            .collect::<Vec<_>>();
        if observed.len() > self.events.len()
            || observed
                .iter()
                .zip(&self.events)
                .any(|(root, worker)| *root != worker)
        {
            return Err("root review evidence disagrees with the durable worker journal".into());
        }
        Ok(self.events[observed.len()..].to_vec())
    }
}

#[cfg(test)]
#[path = "ensemble_review_journal_tests.rs"]
mod tests;
