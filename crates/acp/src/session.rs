mod admission;
mod elicitation_driver;
mod plan;
mod profile_policy;
mod projection;

use admission::{AdmissionState, PromptPhase};
pub(crate) use admission::{InputPreparation, PromptWait};
use elicitation_driver::{ElicitationDriver, ElicitationKey};
use plan::PlanDecisionCoordinator;
use profile_policy::ProfilePolicy;
use projection::ProjectionState;

#[cfg(test)]
#[path = "mode_tests.rs"]
mod mode_tests;
#[cfg(test)]
#[path = "retry_tests.rs"]
mod retry_tests;

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
};
use zevria_session_api::{
    EnsembleEvent, JobEvent, ManagementEvent, PlanEvent, QuestionEvent, SubtaskEvent, TurnEvent,
};

use agent_client_protocol::schema::v1::{
    CreateElicitationResponse, CurrentModeUpdate, Error as AcpError, SessionId,
    SessionUpdate as AcpSessionUpdate, StopReason,
};
use agent_client_protocol::{ConnectionTo, SentRequest};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use zevria_foundation::QuestionResponse;
use zevria_foundation::SessionMode;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::TurnId;
use zevria_model::ContextTokenSnapshot;
use zevria_session_api::ModeSelectionResult;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionEventReceiver;
use zevria_session_api::SessionUpdate;
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;

use crate::elicitation::{PlanChoice, plan_choice, plan_decision_request, question_form};
use crate::project::{
    KnownTools, ResponseUsageSnapshot, complete_tool_results, diagnostic_update,
    plan_artifact_update, plan_ready_instructions, project_tool_calls, usage_update,
    worker_plan_update,
};
use crate::stream::StreamSegments;
use crate::{ClientState, ExecutionProfile, RuntimeExit, SessionRuntimeLifecycle};

type ShutdownOutcome = Option<Result<(), String>>;

pub(crate) struct LiveSession {
    id: SessionId,
    workspace: PathBuf,
    connection: ConnectionTo<agent_client_protocol::Client>,
    profile: ProfilePolicy,
    commands: Mutex<Option<mpsc::UnboundedSender<SessionCommand>>>,
    lifecycle: tokio::sync::Mutex<Option<Box<dyn SessionRuntimeLifecycle>>>,
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    state: Mutex<State>,
    elicitations: ElicitationDriver,
    shutdown_owner: Mutex<Option<watch::Sender<ShutdownOutcome>>>,
    shutdown_outcome: watch::Receiver<ShutdownOutcome>,
    next_management_request: std::sync::atomic::AtomicU64,
}

// Admission, Plan decisions, and projection share one commit boundary, not
// one set of state variables. No async work holds the session lock.
struct State {
    admission: AdmissionState,
    plan: PlanDecisionCoordinator,
    projection: ProjectionState,
}

impl State {
    fn turn_is_terminal(&self, turn_id: TurnId) -> bool {
        self.admission
            .last_terminal_turn
            .is_some_and(|last| turn_id <= last)
    }

    fn accept_network(&mut self, turn_id: TurnId, call: usize, attempt: usize) -> bool {
        if !self
            .admission
            .pending
            .as_ref()
            .is_some_and(|pending| pending.turn_id == Some(turn_id))
            || self.turn_is_terminal(turn_id)
            || !self
                .projection
                .network_scope
                .is_some_and(|(id, current_call, current_attempt)| {
                    id == turn_id
                        && call == current_call
                        && attempt > 0
                        && attempt >= current_attempt
                })
        {
            return false;
        }
        self.projection.network_scope = Some((turn_id, call, attempt));
        true
    }
}

impl LiveSession {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: String,
        workspace: PathBuf,
        commands: mpsc::UnboundedSender<SessionCommand>,
        lifecycle: Box<dyn SessionRuntimeLifecycle>,
        permit: OwnedSemaphorePermit,
        selected_mode: SessionMode,
        initial_plan: PlanWorkflowState,
        connection: ConnectionTo<agent_client_protocol::Client>,
        client: ClientState,
        profile: ExecutionProfile,
    ) -> Arc<Self> {
        // Ready is an unresolved approval lock. Other Plan snapshots are
        // retained artifacts, not evidence of the user's current selection.
        let mode = if matches!(initial_plan, PlanWorkflowState::Ready { .. }) {
            SessionMode::Plan
        } else {
            selected_mode
        };
        let (shutdown_owner, shutdown_outcome) = watch::channel(None);
        Arc::new(Self {
            id: SessionId::new(id),
            workspace,
            connection,
            profile: ProfilePolicy::new(profile, &client),
            commands: Mutex::new(Some(commands)),
            lifecycle: tokio::sync::Mutex::new(Some(lifecycle)),
            event_task: Mutex::new(None),
            permit: Mutex::new(Some(permit)),
            state: Mutex::new(State {
                admission: AdmissionState {
                    mode,
                    pending: None,
                    preparing: None,
                    pending_mode: None,
                    skill_requests: HashMap::new(),
                    last_terminal_turn: None,
                    unavailable: None,
                },
                plan: PlanDecisionCoordinator {
                    state: initial_plan,
                    emitted_plans: HashSet::new(),
                },
                projection: ProjectionState::default(),
            }),
            elicitations: ElicitationDriver::default(),
            shutdown_owner: Mutex::new(Some(shutdown_owner)),
            shutdown_outcome,
            next_management_request: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub(crate) fn id(&self) -> &SessionId {
        &self.id
    }

    pub(crate) fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shutdown_owner
            .lock()
            .expect("ACP shutdown lock poisoned")
            .is_none()
    }

    pub(crate) fn mode(&self) -> SessionMode {
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .admission
            .mode
    }

    pub(crate) fn plan_state(&self) -> PlanWorkflowState {
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .plan
            .state
            .clone()
    }

    pub(crate) fn start_event_loop(
        self: &Arc<Self>,
        events: SessionEventReceiver,
        background_exit: std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeExit> + Send>>,
    ) {
        let mut event_task = self
            .event_task
            .lock()
            .expect("ACP event task lock poisoned");
        if !self.is_closed() {
            let session = Arc::clone(self);
            *event_task = Some(tokio::spawn(async move {
                session.run_event_loop(events, background_exit).await;
            }));
        }
    }

    pub(crate) fn send_update(&self, update: AcpSessionUpdate) -> Result<(), AcpError> {
        self.connection
            .send_notification(crate::stream::session_notification(self.id.clone(), update))
    }

    pub(crate) fn send_updates(
        &self,
        updates: impl IntoIterator<Item = AcpSessionUpdate>,
    ) -> Result<(), AcpError> {
        for update in updates {
            self.send_update(update)?;
        }
        Ok(())
    }

    fn take_shutdown_owner(&self) -> Option<watch::Sender<ShutdownOutcome>> {
        self.shutdown_owner
            .lock()
            .expect("ACP shutdown lock poisoned")
            .take()
    }

    pub(crate) async fn shutdown(self: &Arc<Self>) -> anyhow::Result<()> {
        if let Some(owner) = self.take_shutdown_owner() {
            // Teardown belongs to the session, not to the request waiting for
            // it. Dropping that request must not interrupt cleanup for others.
            let session = Arc::clone(self);
            tokio::spawn(async move {
                session.cancel();
                session.finish_prompt(Ok(StopReason::Cancelled));
                let event_task = session
                    .event_task
                    .lock()
                    .expect("ACP event task lock poisoned")
                    .take();
                if let Some(task) = event_task {
                    task.abort();
                    let _ = task.await;
                }
                let _ = session.shutdown_runtime(owner).await;
            });
        }
        let mut completion = self.shutdown_outcome.clone();
        let outcome = completion.wait_for(Option::is_some).await.map_err(|_| {
            anyhow::anyhow!("ACP session teardown ended without reporting completion")
        })?;
        outcome
            .as_ref()
            .expect("waited for a shutdown outcome")
            .clone()
            .map_err(anyhow::Error::msg)
    }

    async fn shutdown_runtime(&self, owner: watch::Sender<ShutdownOutcome>) -> anyhow::Result<()> {
        self.discard_mode(
            None,
            AcpError::request_cancelled().data("session runtime is closing"),
        );
        self.cancel_elicitations();
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .admission
            .skill_requests
            .clear();
        let commands = self
            .commands
            .lock()
            .expect("ACP command sender lock poisoned")
            .take();
        if let Some(commands) = commands {
            let _ = commands.send(SessionCommand::Control(
                zevria_session_api::ControlCommand::Shutdown,
            ));
            drop(commands);
        }
        let lifecycle = self.lifecycle.lock().await.take();
        let result = if let Some(lifecycle) = lifecycle {
            lifecycle.shutdown().await
        } else {
            Ok(())
        };
        self.permit
            .lock()
            .expect("ACP session permit lock poisoned")
            .take();
        owner.send_replace(Some(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        ));
        result
    }

    fn send_command(&self, command: SessionCommand) -> Result<(), AcpError> {
        // Shutdown remains available after the runtime is marked unavailable.
        if !matches!(
            command,
            SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown)
        ) {
            let state = self.state.lock().expect("ACP session state poisoned");
            if let Some(error) = &state.admission.unavailable {
                return Err(AcpError::internal_error().data(error.clone()));
            }
        }
        let commands = self
            .commands
            .lock()
            .expect("ACP command sender lock poisoned")
            .as_ref()
            .cloned()
            .ok_or_else(|| AcpError::internal_error().data("session runtime is closed"))?;
        commands
            .send(command)
            .map_err(|_| AcpError::internal_error().data("session engine is unavailable"))
    }

    async fn run_event_loop(
        self: Arc<Self>,
        mut events: SessionEventReceiver,
        mut background_exit: std::pin::Pin<
            Box<dyn std::future::Future<Output = RuntimeExit> + Send>,
        >,
    ) {
        loop {
            let preview_deadline = self
                .state
                .lock()
                .expect("ACP session state poisoned")
                .projection
                .streams
                .values()
                .filter_map(StreamSegments::preview_deadline)
                .min();
            tokio::select! {
                biased;
                exit = &mut background_exit => {
                    let detail = exit.error.map_or_else(
                        || format!("{} stopped unexpectedly", exit.component),
                        |error| format!("{} failed: {error}", exit.component),
                    );
                    let _ = self.send_update(diagnostic_update(None, "runtime-exit", &detail));
                    self.runtime_unavailable(detail);
                    break;
                }
                _ = async { tokio::time::sleep_until(preview_deadline.expect("pending preview")).await }, if preview_deadline.is_some() => {
                    let updates = {
                        let now = tokio::time::Instant::now();
                        let mut state = self.state.lock().expect("ACP session state poisoned");
                        let mut updates = Vec::new();
                        for streams in state.projection.streams.values_mut() {
                            if streams.preview_deadline().is_some_and(|deadline| now >= deadline) {
                                streams.flush_preview(now, &mut updates);
                            }
                        }
                        updates
                    };
                    if let Err(error) = self.send_updates(updates) {
                        self.runtime_unavailable(format!("failed to publish ACP preview: {error}"));
                        break;
                    }
                }
                update = events.recv() => {
                    let Some(update) = update else {
                        self.runtime_unavailable("session event channel closed unexpectedly".to_string());
                        break;
                    };
                    if let Err(error) = self.handle_update(update) {
                        self.runtime_unavailable(format!("failed to publish ACP session update: {error}"));
                        break;
                    }
                }
            }
        }
        // Close the receive side before awaiting teardown, so an exiting
        // engine cannot be blocked trying to publish into a full queue.
        drop(events);
        drop(background_exit);
        // External shutdown may already own cleanup. Otherwise do it here
        // without aborting or joining this event task from within itself.
        if let Some(owner) = self.take_shutdown_owner()
            && let Err(error) = self.shutdown_runtime(owner).await
        {
            tracing::warn!(%error, "failed to clean up an unavailable ACP session runtime");
        }
    }

    fn runtime_unavailable(&self, error: String) {
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.admission.unavailable.is_some() {
                return;
            }
            state.admission.unavailable = Some(error.clone());
            if let Some((_, cancellation)) = &state.admission.preparing {
                cancellation.cancel();
            }
        }
        self.finish_prompt(Err(AcpError::internal_error().data(error.clone())));
        self.interrupt_mode(None, AcpError::internal_error().data(error.clone()));
        let requests = std::mem::take(
            &mut self
                .state
                .lock()
                .expect("ACP session state poisoned")
                .admission
                .skill_requests,
        );
        for (_, pending) in requests {
            let _ = pending
                .outcome
                .send(zevria_instructions::skill::SkillManagementResult::error(
                    "unavailable",
                    error.clone(),
                ));
        }
        self.cancel_elicitations();
        let _ = self.send_command(SessionCommand::Control(
            zevria_session_api::ControlCommand::Shutdown,
        ));
    }

    fn cancel_elicitations(&self) {
        self.elicitations.cancel_all();
    }
}

pub(crate) fn mode_id(mode: SessionMode, profile: ExecutionProfile) -> &'static str {
    match mode {
        SessionMode::Build if profile.is_worker() => "review",
        mode => mode.name(),
    }
}

pub(crate) fn retry_diagnostic(
    turn_id: TurnId,
    attempt: usize,
    retry_after: std::time::Duration,
    retrying_for: std::time::Duration,
    error: &str,
) -> AcpSessionUpdate {
    let seconds = retry_after
        .as_secs()
        .saturating_add(u64::from(retry_after.subsec_nanos() > 0));
    let delay = format!(
        " (next attempt in {seconds}s; retrying up to {}m {}s)",
        retrying_for.as_secs() / 60,
        retrying_for.as_secs() % 60
    );
    diagnostic_update(
        Some(turn_id),
        "retry",
        format!("Provider retry {attempt}{delay}: {error}"),
    )
}

fn invalid_params(message: impl Into<String>) -> AcpError {
    AcpError::invalid_params().data(message.into())
}
