use crate::*;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
/// Nonblocking, run-scoped routing. A dropped registration never targets a new
/// run and cannot leave a stale sender installed during session replacement.
type WorkerRoute = (EnsembleRunId, TurnId, mpsc::Sender<WorkerControl>);
#[derive(Clone, Default)]
pub struct WorkerControlRouter(Arc<Mutex<Option<WorkerRoute>>>);
pub struct WorkerControlRegistration {
    router: WorkerControlRouter,
    run_id: EnsembleRunId,
}
impl WorkerControlRouter {
    pub fn register(
        &self,
        run_id: EnsembleRunId,
        turn_id: TurnId,
    ) -> (WorkerControlRegistration, mpsc::Receiver<WorkerControl>) {
        let (tx, rx) = mpsc::channel(WORKER_CONTROL_CAPACITY);
        *self.0.lock().expect("worker router poisoned") = Some((run_id.clone(), turn_id, tx));
        (
            WorkerControlRegistration {
                router: self.clone(),
                run_id,
            },
            rx,
        )
    }
    // The cold rejection path deliberately returns the exact recoverable draft.
    #[allow(clippy::result_large_err)]
    pub fn route(&self, control: WorkerControl) -> Result<(), WorkerControlResult> {
        let guard = self.0.lock().expect("worker router poisoned");
        let Some((run_id, turn_id, sender)) = guard.as_ref() else {
            return Err(WorkerControlResult::rejected(
                control,
                "no active interactive worker review",
            ));
        };
        if run_id != &control.target.run_id || turn_id != &control.target.turn_id {
            return Err(WorkerControlResult::rejected(
                control,
                "stale or historical worker target",
            ));
        }
        sender.try_send(control).map_err(|error| {
            WorkerControlResult::rejected(
                error.into_inner(),
                "worker routing capacity is full or review is sealed; draft was not accepted",
            )
        })
    }
}
impl Drop for WorkerControlRegistration {
    fn drop(&mut self) {
        let mut guard = self.router.0.lock().expect("worker router poisoned");
        if guard.as_ref().is_some_and(|(id, _, _)| id == &self.run_id) {
            *guard = None;
        }
    }
}

/// Actor commands are distinct from user controls; only the coordinator can
/// dispatch accepted input or freeze an outcome. All IO runs outside its reducer.
pub enum WorkerActorCommand {
    Prompt(WorkerInput),
    Retry,
    CancelPrompt {
        generation: u64,
    },
    Mirror(WorkerReviewEvent),
    /// Root acceptance is sufficient; excluded workers never block on an ack.
    Abandon {
        outcome: Box<AgentRunOutcome>,
        request_id: WorkerControlId,
    },
    Finish {
        outcome: Box<AgentRunOutcome>,
        acknowledgement: oneshot::Sender<Result<(), String>>,
    },
}
pub struct WorkerActorUpdate {
    pub worker_id: AgentRunId,
    pub event: WorkerReviewEvent,
}
pub struct EnsembleReviewExecution {
    pub commands: std::collections::HashMap<AgentRunId, mpsc::UnboundedSender<WorkerActorCommand>>,
    pub updates: mpsc::Receiver<WorkerActorUpdate>,
    pub cancellation: tokio_util::sync::CancellationToken,
}
impl Drop for EnsembleReviewExecution {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
