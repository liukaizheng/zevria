//! Restore frontend worker controls from root-authoritative records only.

use std::collections::HashMap;
use zevria_workflow::{
    EnsembleRecord, EnsembleRunId, EnsembleWorkflow, WorkerReviewEvent, WorkerReviewState,
    apply_worker_review_event,
};

use crate::transcript::TranscriptItem;

/// Root-authoritative display restoration. Sidecar audit messages never select
/// a baseline. Callers validate the transcript before exposing live controls.
pub fn project_worker_reviews(
    items: &[TranscriptItem],
) -> Result<HashMap<EnsembleRunId, Vec<WorkerReviewState>>, String> {
    let mut runs: HashMap<EnsembleRunId, Vec<WorkerReviewState>> = HashMap::new();
    for item in items {
        let TranscriptItem::Ensemble(record) = item else {
            continue;
        };
        match record.as_ref() {
            EnsembleRecord::Started { start } if start.workflow == EnsembleWorkflow::Plan => {
                runs.insert(
                    start.run_id.clone(),
                    start
                        .agents
                        .iter()
                        .cloned()
                        .map(WorkerReviewState::new)
                        .collect(),
                );
            }
            EnsembleRecord::WorkerReview {
                run_id,
                worker_id,
                event,
                ..
            } => {
                let states = runs.get_mut(run_id).ok_or("review without Plan start")?;
                apply_worker_review_event(states, worker_id, event)?;
            }
            EnsembleRecord::WorkersConfirmed {
                run_id,
                final_confirmation,
                outcomes,
            } => {
                let states = runs.get_mut(run_id).ok_or("seal without Plan start")?;
                let control = &final_confirmation.control;
                apply_worker_review_event(
                    states,
                    &control.target.worker_id,
                    &control.sealing_event()?,
                )?;
                if states.len() != outcomes.len() {
                    return Err("incomplete frozen set".into());
                }
                for (state, outcome) in states.iter_mut().zip(outcomes) {
                    if &state.outcome() != outcome {
                        return Err("frozen outcome differs from root review".into());
                    }
                    if !state.is_abandoned() {
                        state.apply(&WorkerReviewEvent::Sealed)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(runs)
}
