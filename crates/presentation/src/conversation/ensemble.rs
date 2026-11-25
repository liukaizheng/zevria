//! Root worker display projection and review reconciliation.

use super::{
    AgentRunId, AgentRunStatus, ConversationState, DisplayTurn, EnsembleHistory, EnsembleRunId,
    EnsembleStart, EnsembleWorkflow, HistoryEntry, NativeHeader,
};

impl ConversationState {
    pub fn ensemble_turn(&self, run_id: &EnsembleRunId) -> Option<DisplayTurn> {
        let HistoryEntry::Ensemble(entry) = self.entry(*self.ensemble_indices.get(run_id)?)? else {
            return None;
        };
        entry.header.map(NativeHeader::turn)
    }

    pub fn add_ensemble(
        &mut self,
        start: EnsembleStart,
        status: AgentRunStatus,
        header: Option<NativeHeader>,
    ) {
        self.push_entry(HistoryEntry::Ensemble(EnsembleHistory::new(
            start, status, header,
        )));
    }

    pub fn has_ensemble(&self, run_id: &EnsembleRunId) -> bool {
        self.ensemble_indices.contains_key(run_id)
    }

    fn ensemble_mut(&mut self, run_id: &EnsembleRunId) -> Option<(usize, &mut EnsembleHistory)> {
        let index = *self.ensemble_indices.get(run_id)?;
        let HistoryEntry::Ensemble(ensemble) = &mut self.history[index] else {
            return None;
        };
        Some((index, ensemble))
    }

    pub fn ensemble_baseline(&self, run_id: &EnsembleRunId) -> Option<&AgentRunId> {
        let HistoryEntry::Ensemble(entry) = self.entry(*self.ensemble_indices.get(run_id)?)? else {
            return None;
        };
        entry.baseline.as_ref()
    }

    pub fn update_ensemble_baseline(
        &mut self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
        selected: bool,
    ) {
        if let Some((index, ensemble)) = self.ensemble_mut(run_id) {
            if ensemble.workers.iter().any(|worker| {
                worker.review_authoritative
                    && (&worker.descriptor.id == worker_id
                        || ensemble.baseline.as_ref() == Some(&worker.descriptor.id))
            }) {
                return;
            }
            let previous = ensemble.baseline.clone();
            if selected
                && ensemble.workflow == EnsembleWorkflow::Plan
                && ensemble.workers.iter().any(|worker| {
                    &worker.descriptor.id == worker_id && worker.status != AgentRunStatus::Abandoned
                })
            {
                ensemble.baseline = Some(worker_id.clone());
            } else if ensemble.baseline.as_ref() == Some(worker_id) {
                ensemble.baseline = None;
            }
            if previous != ensemble.baseline {
                self.touch_entry(index);
            }
        }
    }

    pub fn update_ensemble_worker(
        &mut self,
        run_id: &EnsembleRunId,
        agent_run_id: &AgentRunId,
        status: AgentRunStatus,
        failure: Option<String>,
    ) {
        if let Some((index, ensemble)) = self.ensemble_mut(run_id) {
            let mut changed = ensemble.update_status(agent_run_id, status, failure);
            if status == AgentRunStatus::Abandoned
                && ensemble.baseline.as_ref() == Some(agent_run_id)
            {
                ensemble.baseline = None;
                changed = true;
            }
            if changed {
                self.touch_entry(index);
            }
        }
    }

    pub fn project_worker_completion(
        &mut self,
        run_id: &EnsembleRunId,
        completion: crate::projection::WorkerCompletion,
    ) {
        let crate::projection::WorkerCompletion {
            worker_id,
            mut status,
            partial,
            has_plan_proof,
            baseline,
            mut failure,
        } = completion;
        self.update_ensemble_baseline(run_id, &worker_id, baseline);
        let Some((index, ensemble)) = self.ensemble_mut(run_id) else {
            return;
        };
        if status == AgentRunStatus::Completed
            && (partial || (ensemble.workflow == EnsembleWorkflow::Plan && !has_plan_proof))
        {
            status = AgentRunStatus::Failed;
            failure.get_or_insert_with(|| {
                if partial {
                    "worker reported an invalid partial Completed outcome".to_string()
                } else {
                    "Plan worker reported Completed without final Markdown proof".to_string()
                }
            });
        }
        if ensemble.update_status(&worker_id, status, failure) {
            self.touch_entry(index);
        }
    }

    pub fn project_worker_review(
        &mut self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
        state: &zevria_workflow::WorkerReviewState,
        mode: crate::projection::ProjectionMode,
    ) {
        if &state.descriptor.id != worker_id {
            return;
        }
        let Some((index, ensemble)) = self.ensemble_mut(run_id) else {
            return;
        };
        if ensemble.workflow != EnsembleWorkflow::Plan {
            return;
        }
        let Some(worker) = ensemble
            .workers
            .iter_mut()
            .find(|worker| &worker.descriptor.id == worker_id)
        else {
            return;
        };
        if worker.descriptor != state.descriptor {
            return;
        }
        let confirmed = state.confirmed_plan();
        if state.proposals.confirmation.is_some()
            && confirmed
                .as_ref()
                .is_none_or(|plan| &plan.receipt.target.run_id != run_id)
        {
            return;
        }
        let previous_status = worker.status;
        let mut changed = !worker.review_authoritative || worker.failure != state.diagnostic();
        let previous_baseline = ensemble.baseline.clone();
        worker.review_authoritative = true;
        worker.status = state.status();
        if mode == crate::projection::ProjectionMode::Restored
            && matches!(
                worker.status,
                AgentRunStatus::Running
                    | AgentRunStatus::Queued
                    | AgentRunStatus::Starting
                    | AgentRunStatus::Resuming
            )
        {
            worker.status = AgentRunStatus::Interrupted;
        }
        changed |= previous_status != worker.status;
        worker.failure.clone_from(&state.diagnostic());
        if confirmed.is_some_and(|plan| plan.baseline.is_some()) && !state.is_abandoned() {
            ensemble.baseline = Some(worker_id.clone());
        } else if ensemble.baseline.as_ref() == Some(worker_id) {
            ensemble.baseline = None;
        }
        if changed || previous_baseline != ensemble.baseline {
            self.touch_entry(index);
        }
    }

    pub fn reviewed_worker_status(
        &self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
    ) -> Option<AgentRunStatus> {
        let HistoryEntry::Ensemble(ensemble) = self.entry(*self.ensemble_indices.get(run_id)?)?
        else {
            return None;
        };
        ensemble
            .workers
            .iter()
            .find(|worker| &worker.descriptor.id == worker_id && worker.review_authoritative)
            .map(|worker| worker.status)
    }

    pub fn update_unfinished_ensemble_workers(
        &mut self,
        run_id: &EnsembleRunId,
        status: AgentRunStatus,
    ) {
        let Some((index, ensemble)) = self.ensemble_mut(run_id) else {
            return;
        };
        let mut changed = false;
        for worker in &mut ensemble.workers {
            if !worker.review_authoritative
                && !worker.status.is_terminal()
                && worker.status != status
            {
                worker.status = status;
                changed = true;
            }
        }
        if changed {
            self.touch_entry(index);
        }
    }
}
