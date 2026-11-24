//! Worker-journal review projection and root/worker crash-boundary reconciliation.
//! Root controls are authority; a worker journal can prove delivery/settlement,
//! never manufacture acceptance or confirmation.
use std::{borrow::Cow, collections::BTreeMap};

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
pub(crate) enum JournalChange<'a> {
    None,
    StoreInput(&'a WorkerReviewEvent, &'a WorkerInput),
    Transition {
        event: Cow<'a, WorkerReviewEvent>,
        stop_publishing: bool,
        log: bool,
    },
    BeginReplay,
    EndReplay,
    BeginPrompt,
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
    /// Continue from a validated reduction without retaining its audit event log.
    /// The projection can outlive the writer without forcing a COW clone on append.
    pub(crate) fn resume_validator(&self) -> Self {
        Self {
            state: self.state.clone(),
            inputs: self.inputs.clone(),
            events: Vec::new(),
            abandonment: self.abandonment.clone(),
            run_id: self.run_id.clone(),
            publishing: self.publishing,
            replay: self.replay,
            collect_events: false,
        }
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
    pub fn apply(&mut self, record: &AgentRunTranscriptRecord) -> Result<(), String> {
        let change = self.prepare(record)?;
        self.commit(change);
        Ok(())
    }

    pub(crate) fn prepare<'a>(
        &self,
        record: &'a AgentRunTranscriptRecord,
    ) -> Result<JournalChange<'a>, String> {
        use JournalChange::*;
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
        let transition = |event: Cow<'a, WorkerReviewEvent>, stop_publishing, log| {
            self.state.validate_transition(&event)?;
            Ok(Transition {
                event,
                stop_publishing,
                log,
            })
        };
        match record {
            AgentRunTranscriptRecord::Event { event } => match event {
                AgentRunEvent::Review { event } => match event.as_ref() {
                    WorkerReviewEvent::InputAccepted { input } => {
                        if let Some(previous) = self.inputs.get(&input.generation) {
                            if previous != input { return Err("conflicting worker input mirror".into()); }
                            return Ok(None);
                        }
                        self.state.validate_transition(event)?;
                        Ok(StoreInput(event, input))
                    }
                    WorkerReviewEvent::Dispatched { .. } | WorkerReviewEvent::Settled { .. } | WorkerReviewEvent::Recovering { .. } => {
                        transition(Cow::Borrowed(event), true, true)
                    }
                    WorkerReviewEvent::Interrupted { .. } | WorkerReviewEvent::Abandoned { .. } => {
                        transition(Cow::Borrowed(event), true, false)
                    }
                    WorkerReviewEvent::Connection { .. } | WorkerReviewEvent::ImageCapability { .. } => transition(Cow::Borrowed(event), false, false),
                    WorkerReviewEvent::Confirmed { receipt } | WorkerReviewEvent::BaselineMarked { receipt } => {
                        if receipt.target.run_id != self.run_id || !self.state.retained.as_ref().is_some_and(|snapshot| receipt.validates_snapshot(snapshot, &self.state.descriptor.id)) {
                            return Err("worker confirmation mirror has no exact eligible snapshot".into());
                        }
                        Ok(None)
                    }
                    WorkerReviewEvent::CancelRequested { generation } => {
                        if *generation == 0 || *generation > self.state.accepted_generation { return Err("worker cancellation mirror has no accepted input".into()); }
                        Ok(None)
                    }
                    WorkerReviewEvent::Withdrawn { .. } | WorkerReviewEvent::BaselineCleared { .. } | WorkerReviewEvent::Sealed | WorkerReviewEvent::PayloadChecked { .. } => Ok(None),
                    WorkerReviewEvent::Fatal { .. } | WorkerReviewEvent::Published { .. } | WorkerReviewEvent::Removed { .. } | WorkerReviewEvent::Decisions { .. } => {
                        Err("worker journal uses raw durable Plan evidence, not synthetic publication mirrors".into())
                    }
                },
                AgentRunEvent::ReplayBoundary => Ok(BeginReplay),
                AgentRunEvent::SessionEstablished { .. } => Ok(EndReplay),
                AgentRunEvent::Prompt { .. } => Ok(BeginPrompt),
                AgentRunEvent::NativePlanCaptured { capture, .. } if self.publishing && self.state.active.as_ref().is_none_or(|active| active.generation != capture.generation) => {
                    Err("native capture generation does not match active review input".into())
                }
                AgentRunEvent::Plan { plan } | AgentRunEvent::NativePlanCaptured { plan, .. } if self.publishing => {
                    let generation = self.state.active.as_ref().ok_or("publication without active worker input")?.generation;
                    transition(Cow::Owned(WorkerReviewEvent::Published { generation, plan: plan.clone(), replay: false }), false, true)
                }
                AgentRunEvent::Elicitation { outcome: crate::AgentElicitationOutcome::Accepted, decision, decision_unavailable, .. } if !self.inputs.is_empty() => {
                    transition(Cow::Owned(WorkerReviewEvent::Decisions { decision: decision.clone(), unavailable: decision_unavailable.clone() }), false, true)
                }
                AgentRunEvent::PlanRemoved { plan_id } if !self.replay && !self.inputs.is_empty() => {
                    transition(Cow::Owned(WorkerReviewEvent::Removed { plan_id: plan_id.clone() }), false, true)
                }
                _ => Ok(None),
            },
            AgentRunTranscriptRecord::Outcome { outcome } if outcome.status == crate::AgentRunStatus::Abandoned => {
                if self.abandonment.is_none() || !outcome.is_sanitized_abandonment() {
                    return Err("abandoned outcome requires an abandonment audit mirror and sanitized evidence".into());
                }
                Ok(None)
            }
            AgentRunTranscriptRecord::Outcome { outcome } if self.state.abandoned => Err(format!("non-abandoned outcome after abandonment: {}", outcome.status)),
            AgentRunTranscriptRecord::Outcome { outcome } if !self.inputs.is_empty() && outcome.confirmation.is_some() => {
                let confirmed = outcome.confirmation.as_ref().expect("guarded confirmation");
                if self.state.retained.as_ref() != Some(&confirmed.snapshot) {
                    return Err("terminal worker snapshot differs from its durable settled review evidence".into());
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn commit(&mut self, change: JournalChange<'_>) {
        match change {
            JournalChange::None => {}
            JournalChange::StoreInput(event, input) => {
                self.state.apply_validated_transition(event);
                self.inputs.insert(input.generation, input.clone());
            }
            JournalChange::Transition {
                event,
                stop_publishing,
                log,
            } => {
                self.state.apply_validated_transition(&event);
                if let WorkerReviewEvent::Abandoned { request_id } = event.as_ref() {
                    self.abandonment = Some(request_id.clone());
                }
                if stop_publishing {
                    self.publishing = false;
                }
                if log && self.collect_events {
                    self.events.push(event.into_owned());
                }
            }
            JournalChange::BeginReplay => {
                self.replay = true;
                self.publishing = false;
            }
            JournalChange::EndReplay => self.replay = false,
            JournalChange::BeginPrompt => {
                self.publishing = self.state.active.is_some() && !self.replay
            }
        }
    }

    /// Reconcile before any writable restoration or provider startup. Worker
    /// execution updates are a prefix in root history (controls may interleave).
    /// Missing suffix evidence is committed by the root before interrupted work
    /// is settled, so proven dispatch cannot be mistaken for unsent feedback.
    pub fn reconcile(&self, root: &[WorkerReviewEvent]) -> Result<Vec<WorkerReviewEvent>, String> {
        self.reconcile_events(root.iter())
    }

    pub fn reconcile_events<'a>(
        &self,
        root: impl Iterator<Item = &'a WorkerReviewEvent> + Clone,
    ) -> Result<Vec<WorkerReviewEvent>, String> {
        let root_abandonment = root.clone().find_map(|event| match event {
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
            .clone()
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
