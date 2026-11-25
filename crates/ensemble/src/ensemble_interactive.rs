//! Long-lived ACP Plan workers. The root owns acceptance and confirmation;
//! actors own IO, same-session continuation and prompt-scoped cancellation.
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zevria_workflow::{WorkerEventObserver, WorkerObservation};

use super::EnsembleSupervisor;
use super::attempt::{
    InteractionCounter, WorkerAttemptContext, WorkerAttemptMode, WorkerAttemptProgress, WorkerEnd,
};
use super::diagnostics::RawDiagnosticSink;
use super::failure::{acp_error, concise_process_failure, error_with_login_hint};
use super::handoff::ClaudePlanHandoff;
use super::history::preflight_existing_logs;
use super::logging::RunLog;
use super::prompt::{worker_attempt_prompt, worker_prompt};
use super::recovery::{PromptRunContext, TransientRecoveryState, run_prompt_sequence};
use super::run::MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS;
use crate::config::{EnsembleAgentConfig, PlanHandoffTransport};
use agent_client_protocol::schema::v1::{Error as AcpError, SessionId};
use agent_client_protocol::{Agent, ConnectionTo};
use anyhow::Context as _;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use zevria_session_api::worker::{EnsembleReviewExecution, WorkerActorCommand, WorkerActorUpdate};
use zevria_session_api::{EnsembleLaunchRequest, SessionEventSender, TurnContext};
use zevria_transcript::agent_run_path;
use zevria_workflow::ensemble_review::{
    WORKER_CONTROL_CAPACITY, WorkerInput, WorkerPromptKind, WorkerReviewEvent, WorkerReviewState,
};
use zevria_workflow::{
    AgentRunDescriptor, AgentRunEvent, AgentRunId, AgentRunStatus, EnsembleWorkflow,
};

struct ReviewDriver {
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl ReviewDriver {
    async fn stop(&mut self, grace: Duration) {
        self.cancellation.cancel();
        if tokio::time::timeout(grace, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}
impl Drop for ReviewDriver {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

#[derive(Clone)]
pub(super) struct ReviewPublication {
    worker_id: AgentRunId,
    updates: mpsc::Sender<WorkerActorUpdate>,
    observer: Arc<Mutex<WorkerEventObserver>>,
}
impl ReviewPublication {
    pub(super) async fn observe(&self, event: &AgentRunEvent) -> anyhow::Result<()> {
        let transition = {
            let mut observer = self.observer.lock().expect("worker observation poisoned");
            if let AgentRunEvent::Review { event } = event {
                observer.observe_review(event);
                matches!(
                    event.as_ref(),
                    WorkerReviewEvent::Dispatched { .. }
                        | WorkerReviewEvent::ImageCapability { .. }
                        | WorkerReviewEvent::Recovering { .. }
                        | WorkerReviewEvent::Settled { .. }
                )
                .then(|| event.as_ref().clone())
            } else {
                observer
                    .observe(event, WorkerObservation::Live)
                    .map_err(anyhow::Error::msg)?
            }
        };
        if let Some(event) = transition {
            self.send(event).await?;
        }
        Ok(())
    }
    async fn send(&self, event: WorkerReviewEvent) -> anyhow::Result<()> {
        self.updates
            .send(WorkerActorUpdate {
                worker_id: self.worker_id.clone(),
                event,
            })
            .await
            .map_err(|_| anyhow::anyhow!("root review coordinator stopped"))
    }
}

#[derive(Clone)]
struct PromptScope {
    generation: u64,
    cancellation: tokio_util::sync::CancellationToken,
    lifetime: tokio_util::sync::CancellationToken,
    accepting: Arc<AtomicBool>,
}

/// Shared command-side state. Only the IO driver can drain accepted input.
pub(super) struct InteractiveController {
    input: Mutex<InteractiveInputState>,
    publication: ReviewPublication,
    descriptor: AgentRunDescriptor,
    permits: Arc<Semaphore>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    attempt: AtomicU64,
    native: bool,
    continuing: AtomicBool,
    relaunches: AtomicU64,
    transient: Mutex<TransientRecoveryState>,
}

struct InteractiveInputState {
    current: Option<WorkerInput>,
    scope: Option<PromptScope>,
    cancelled: HashSet<u64>,
    settled_generation: u64,
}

pub(super) struct InteractiveAttempt<'a> {
    pub(super) controller: Arc<InteractiveController>,
    pub(super) queue: &'a mut mpsc::UnboundedReceiver<WorkerInput>,
}

struct InteractiveCleanup(Arc<InteractiveController>);
impl Drop for InteractiveCleanup {
    fn drop(&mut self) {
        self.0.close_prompt();
        self.0.release();
    }
}

async fn next_input(
    queue: &mut mpsc::UnboundedReceiver<WorkerInput>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<WorkerInput> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        input = queue.recv() => input,
    }
}

impl InteractiveController {
    fn close_prompt(&self) {
        if let Some(scope) = self
            .input
            .lock()
            .expect("worker input poisoned")
            .scope
            .take()
        {
            scope.accepting.store(false, AtomicOrdering::Release);
            scope.cancellation.cancel();
            scope.lifetime.cancel();
        }
    }

    pub(super) fn interaction_tokens(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
        lifetime: &tokio_util::sync::CancellationToken,
    ) -> (
        tokio_util::sync::CancellationToken,
        tokio_util::sync::CancellationToken,
    ) {
        self.input
            .lock()
            .expect("worker input poisoned")
            .scope
            .as_ref()
            .filter(|scope| scope.accepting.load(AtomicOrdering::Acquire))
            .map_or_else(
                || {
                    let cancellation = cancellation.child_token();
                    cancellation.cancel();
                    let lifetime = lifetime.child_token();
                    lifetime.cancel();
                    (cancellation, lifetime)
                },
                |scope| (scope.cancellation.clone(), scope.lifetime.clone()),
            )
    }

    fn current_input(&self) -> Option<WorkerInput> {
        self.input
            .lock()
            .expect("worker input poisoned")
            .current
            .clone()
    }

    fn input_cancelled(&self) -> bool {
        let state = self.input.lock().expect("worker input poisoned");
        state
            .current
            .as_ref()
            .is_some_and(|input| state.cancelled.contains(&input.generation))
    }

    fn cancel_input(&self, generation: u64, attempt: &tokio_util::sync::CancellationToken) {
        let mut state = self.input.lock().expect("worker input poisoned");
        if generation <= state.settled_generation {
            return;
        }
        state.cancelled.insert(generation);
        if let Some(scope) = state
            .scope
            .as_ref()
            .filter(|scope| scope.generation == generation)
        {
            scope.cancellation.cancel();
            scope.lifetime.cancel();
        } else if state
            .current
            .as_ref()
            .is_some_and(|input| input.generation == generation)
        {
            attempt.cancel();
        }
    }

    fn begin_prompt(&self, scope: PromptScope) {
        let mut state = self.input.lock().expect("worker input poisoned");
        if state.cancelled.contains(&scope.generation) {
            scope.cancellation.cancel();
        }
        state.scope = Some(scope);
    }

    async fn dispatch(&self, input: WorkerInput, log: &RunLog) -> Result<(), AcpError> {
        let event = WorkerReviewEvent::Dispatched {
            generation: input.generation,
            attempt: self.attempt.load(AtomicOrdering::Acquire),
        };
        self.input.lock().expect("worker input poisoned").current = Some(input);
        self.continuing.store(false, AtomicOrdering::Release);
        self.relaunches.store(0, AtomicOrdering::Release);
        *self.transient.lock().expect("transient recovery poisoned") =
            TransientRecoveryState::default();
        log.emit(AgentRunEvent::Review {
            event: Box::new(event),
        })
        .await
        .map_err(acp_error)
    }
    async fn acquire(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), AcpError> {
        if self
            .permit
            .lock()
            .expect("worker permit poisoned")
            .is_some()
        {
            return Ok(());
        }
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(acp_error("worker attempt cancelled while waiting for active-work capacity")),
            permit = self.permits.clone().acquire_owned() => permit.map_err(acp_error)?,
        };
        *self.permit.lock().expect("worker permit poisoned") = Some(permit);
        Ok(())
    }
    fn release(&self) {
        self.permit.lock().expect("worker permit poisoned").take();
    }
    async fn settle(
        &self,
        log: &RunLog,
        failure: Option<String>,
        connected: bool,
    ) -> Result<(), AcpError> {
        log.barrier().await.map_err(acp_error)?;
        let input = {
            let mut state = self.input.lock().expect("worker input poisoned");
            let input = state.current.take();
            if let Some(input) = &input {
                state.settled_generation = input.generation;
                state.cancelled.remove(&input.generation);
                if let Some(scope) = state.scope.take() {
                    scope.lifetime.cancel();
                }
            }
            input
        };
        self.release();
        if let Some(input) = input {
            if let Some(error) = &failure {
                log.emit(AgentRunEvent::Failure {
                    error: error.clone(),
                })
                .await
                .map_err(acp_error)?;
            }
            let evidence = log.outcome_evidence(
                self.descriptor.clone(),
                AgentRunStatus::AwaitingFeedback,
                false,
                failure.clone(),
            );
            let event = WorkerReviewEvent::Settled {
                generation: input.generation,
                failure,
                connected,
                evidence: Box::new(evidence),
            };
            log.emit(AgentRunEvent::Review {
                event: Box::new(event),
            })
            .await
            .map_err(acp_error)?;
        } else {
            self.publication
                .send(WorkerReviewEvent::Connection {
                    connected,
                    diagnostic: failure,
                })
                .await
                .map_err(acp_error)?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run(
        &self,
        queue: &mut mpsc::UnboundedReceiver<WorkerInput>,
        connection: &ConnectionTo<Agent>,
        session_id: SessionId,
        log: &RunLog,
        cancellation: &tokio_util::sync::CancellationToken,
        lifetime: &tokio_util::sync::CancellationToken,
        handoff: Option<&ClaudePlanHandoff>,
        cancel_grace: Duration,
        interactions: &InteractionCounter,
        progress: &WorkerAttemptProgress,
        violation: &Mutex<Option<String>>,
        images_supported: bool,
    ) -> Result<WorkerEnd, AcpError> {
        log.emit(AgentRunEvent::Review {
            event: Box::new(WorkerReviewEvent::ImageCapability {
                supported: images_supported,
            }),
        })
        .await
        .map_err(acp_error)?;
        self.publication
            .send(WorkerReviewEvent::Connection {
                connected: true,
                diagnostic: None,
            })
            .await
            .map_err(acp_error)?;
        loop {
            let input = if let Some(input) = self.current_input() {
                input
            } else {
                self.release();
                let Some(input) = next_input(queue, cancellation).await else {
                    return Ok(WorkerEnd::LocalCancelled);
                };
                self.dispatch(input.clone(), log).await?;
                input
            };
            if input.text.has_images() && !images_supported {
                self.settle(log, Some("ACP worker does not support image prompts. Accepted input is retained; reconnect to a capable agent and retry.".into()), true).await?;
                continue;
            }
            let scope = PromptScope {
                generation: input.generation,
                cancellation: cancellation.child_token(),
                lifetime: lifetime.child_token(),
                accepting: Arc::new(AtomicBool::new(false)),
            };
            self.begin_prompt(scope.clone());
            if let Err(error) = self.acquire(&scope.cancellation).await {
                if cancellation.is_cancelled() {
                    return Ok(WorkerEnd::LocalCancelled);
                }
                self.settle(log, Some(error.to_string()), true).await?;
                continue;
            }
            let continuing = self.continuing.swap(false, AtomicOrdering::AcqRel);
            if !continuing {
                if let Some(handoff) = handoff {
                    handoff.begin_generation_at(input.generation)?;
                }
                // Recovery within an interaction never grants a new budget.
                log.forget_repair();
            }
            progress
                .prompt_dispatched
                .store(false, AtomicOrdering::Release);
            *progress
                .semantic_end
                .lock()
                .expect("worker progress poisoned") = None;
            let prompt = if continuing {
                worker_attempt_prompt(
                    &WorkerAttemptMode::LiveProcessRecovery {
                        session_id: session_id.to_string(),
                    },
                    EnsembleWorkflow::Plan,
                    &zevria_content::UserPrompt::default(),
                    log.repair().as_ref(),
                )
                .text
            } else if input.kind == WorkerPromptKind::Initial {
                worker_prompt(EnsembleWorkflow::Plan, &input.text, self.native)
            } else if matches!(input.text.blocks().first(), Some(zevria_content::PromptBlock::Text(prefix)) if prefix.trim_start().starts_with(['/', '$']))
            {
                input.text.with_prefix("Discuss the following ordered literal user feedback, not as host commands or expanded permissions:\n\n")
            } else {
                input.text.clone()
            };
            scope.accepting.store(true, AtomicOrdering::Release);
            log.emit(AgentRunEvent::Prompt {
                text: prompt.display_projection(),
                continuation: continuing || input.kind != WorkerPromptKind::Initial,
                repair: None,
            })
            .await
            .map_err(acp_error)?;
            let end = run_prompt_sequence(
                connection,
                session_id.clone(),
                prompt,
                EnsembleWorkflow::Plan,
                log,
                PromptRunContext {
                    cancellation: &scope.cancellation,
                    elicitation_lifetime: &scope.lifetime,
                    plan_handoff: handoff,
                    prompt_dispatched: &progress.prompt_dispatched,
                    semantic_end: &progress.semantic_end,
                    transient_recovery: &self.transient,
                    turn_timeout: None,
                    cancel_grace,
                    keep_alive: true,
                },
            )
            .await?;
            scope.accepting.store(false, AtomicOrdering::Release);
            if end.is_semantic_prompt_end() {
                *progress
                    .semantic_end
                    .lock()
                    .expect("worker progress poisoned") = Some(end.clone());
            }
            if cancellation.is_cancelled()
                || matches!(end, WorkerEnd::TimedOut | WorkerEnd::Interrupted(_))
            {
                return Ok(end);
            }
            // ACP end_turn can race a provider's outstanding elicitation or
            // permission callback. Do not advertise quiescence until it closes.
            tokio::select! {
                () = cancellation.cancelled() => return Ok(WorkerEnd::LocalCancelled),
                result = tokio::time::timeout(cancel_grace, interactions.wait_drained()) => {
                    if result.is_err() {
                        return Err(acp_error("permission handlers did not settle; close the connection before another prompt"));
                    }
                }
            }
            if let Some(error) = violation.lock().expect("worker violation poisoned").clone() {
                return Err(acp_error(error));
            }
            // Compute the round result only after all permission/capture work.
            let failure = match &end {
                WorkerEnd::NativeHandoffCompleted => None,
                WorkerEnd::NativeHandoffFailed(error) => Some(error.clone()),
                WorkerEnd::PromptResponse {
                    stop_reason: "end_turn",
                } => None,
                _ => Some(format!("worker interaction ended without success: {end:?}")),
            }
            .or_else(|| handoff.and_then(ClaudePlanHandoff::unresolved_violation));
            let failure = if scope.cancellation.is_cancelled() {
                Some("worker-local cancellation; feedback was not incorporated".into())
            } else {
                failure
            };
            let close_native = scope.cancellation.is_cancelled()
                && handoff.is_some_and(|handoff| handoff.unresolved_violation().is_some());
            self.settle(log, failure, !close_native).await?;
            if close_native {
                return Ok(WorkerEnd::LocalCancelled);
            }
        }
    }
}

impl EnsembleSupervisor {
    pub(super) fn start_interactive_review(
        &self,
        request: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        events: SessionEventSender,
        turn: TurnContext,
        preflight: bool,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        anyhow::ensure!(
            request.start.workflow == EnsembleWorkflow::Plan,
            "interactive worker review is exclusive to Ensemble Plan"
        );
        anyhow::ensure!(
            states.len() == request.start.agents.len()
                && states
                    .iter()
                    .zip(&request.start.agents)
                    .all(|(state, descriptor)| &state.descriptor == descriptor),
            "review state must match every selected worker in descriptor order"
        );
        if preflight {
            preflight_existing_logs(&self.inner.agent_runs_root, &request.start)?;
        }
        let cancellation = turn.cancellation().child_token();
        let permits = Arc::new(Semaphore::new(self.inner.config.max_concurrent_agents));
        let (updates, receiver) = mpsc::channel(WORKER_CONTROL_CAPACITY);
        let mut commands = HashMap::new();
        for state in states {
            if state.is_abandoned() {
                continue;
            }
            let (tx, rx) = mpsc::unbounded_channel();
            commands.insert(state.descriptor.id.clone(), tx);
            let supervisor = self.clone();
            let start = request.start.clone();
            let events = events.clone();
            let turn = turn.clone();
            let cancellation = cancellation.child_token();
            let permits = permits.clone();
            let updates = updates.clone();
            tokio::spawn(async move {
                let worker_id = state.descriptor.id.clone();
                if let Err(error) = supervisor
                    .interactive_actor(
                        start,
                        request.resume,
                        state,
                        rx,
                        updates.clone(),
                        permits,
                        events,
                        turn,
                        cancellation,
                    )
                    .await
                {
                    tracing::error!(target: "zevria::ensemble::interactive", %error, "interactive Plan worker stopped; root must not synthesize");
                    let _ = updates
                        .send(WorkerActorUpdate {
                            worker_id,
                            event: WorkerReviewEvent::Fatal {
                                error: error.to_string(),
                            },
                        })
                        .await;
                }
            });
        }
        Ok(EnsembleReviewExecution {
            commands,
            updates: receiver,
            cancellation,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn interactive_actor(
        &self,
        start: zevria_workflow::EnsembleStart,
        resume: bool,
        state: WorkerReviewState,
        mut commands: mpsc::UnboundedReceiver<WorkerActorCommand>,
        updates: mpsc::Sender<WorkerActorUpdate>,
        permits: Arc<Semaphore>,
        events: SessionEventSender,
        turn: TurnContext,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let descriptor = state.descriptor.clone();
        let (log, previous) = RunLog::open(
            agent_run_path(&self.inner.agent_runs_root, &start.run_id, &descriptor.id),
            &start,
            &descriptor,
            resume,
            events,
            turn.clone(),
        )
        .await?;
        let (queue_tx, queue_rx) = mpsc::unbounded_channel();
        if let Some(input) = previous
            .review
            .as_ref()
            .and_then(|journal| journal.state.inputs.active.as_ref())
            && state.inputs.active.is_none()
            && state.inputs.settled_generation >= input.generation
        {
            log.emit(AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::Interrupted {
                    generation: input.generation,
                }),
            })
            .await?;
        }
        for input in &state.inputs.pending {
            // Root acceptance is authoritative, even if its old worker mirror
            // was interrupted. Review records carry stable request identities.
            log.emit(AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::InputAccepted {
                    input: input.clone(),
                }),
            })
            .await?;
            queue_tx.send(input.clone())?;
        }
        let publication = ReviewPublication {
            worker_id: descriptor.id.clone(),
            updates,
            observer: Arc::new(Mutex::new(WorkerEventObserver::new(resume))),
        };
        *log.review_publication
            .lock()
            .expect("review publication poisoned") = Some(publication.clone());
        let agent = self
            .inner
            .config
            .agents
            .get(&descriptor.agent)
            .context("configured worker is unavailable")?;
        let native =
            agent.plan_handoff_transport == Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
        let interactive = Arc::new(InteractiveController {
            input: Mutex::new(InteractiveInputState {
                current: None,
                scope: None,
                cancelled: state.inputs.cancel_requested.into_iter().collect(),
                settled_generation: state.inputs.settled_generation,
            }),
            publication,
            descriptor: descriptor.clone(),
            permits,
            permit: Mutex::new(None),
            attempt: AtomicU64::new(state.connection.attempt),
            native,
            continuing: AtomicBool::new(false),
            relaunches: AtomicU64::new(0),
            transient: Mutex::new(TransientRecoveryState::default()),
        });
        let attempt_cancel = Arc::new(Mutex::new(cancellation.child_token()));
        let native_recovery_safe = !resume
            || !native
            || previous.acp_session_id.is_none()
            || (state.quiescent()
                && state.proposals.retained.as_ref().is_some_and(|snapshot| {
                    snapshot.revision.generation == state.inputs.settled_generation
                })
                && state.diagnostic().is_none());
        // IO must keep polling while the command pump awaits a durable mirror.
        // Co-polling both in one select branch can deadlock on event_order when
        // a prompt publication holds it across a journal acknowledgement.
        let supervisor = self.clone();
        let driving = interactive.clone();
        let driving_log = log.clone();
        let driving_agent = agent.clone();
        let driving_turn = turn.clone();
        let driving_cancel = cancellation.clone();
        let driving_slot = attempt_cancel.clone();
        let mut driver = ReviewDriver {
            cancellation: cancellation.clone(),
            task: tokio::spawn(async move {
                // The nested driver future (and ACP process) is dropped before
                // this guard releases capacity on hard abort or early error.
                let _cleanup = InteractiveCleanup(driving.clone());
                supervisor
                    .drive_interactive(
                        &driving,
                        queue_rx,
                        &driving_log,
                        &driving_agent,
                        &driving_turn,
                        &driving_cancel,
                        driving_slot,
                        previous.acp_session_id,
                        native_recovery_safe,
                    )
                    .await
            }),
        };
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    interactive.close_prompt();
                    driver.stop(Duration::from_secs(self.inner.config.cancel_grace_seconds)).await;
                    interactive.release();
                    if turn.is_cancelled() {
                        let context = interactive.transient.lock().expect("transient recovery poisoned").failure_context(&agent.login_hint).into_iter().collect::<Vec<_>>();
                        self.finish_worker_with_context(&log, descriptor.clone(), AgentRunStatus::Cancelled, true,
                            Some("ensemble turn cancelled by the user".into()), &context).await;
                    }
                    return Ok(());
                }
                command = commands.recv() => match command {
                    Some(WorkerActorCommand::Prompt(input)) => {
                        log.emit(AgentRunEvent::Review { event: Box::new(WorkerReviewEvent::InputAccepted { input: input.clone() }) }).await?;
                        queue_tx.send(input)?;
                    }
                    Some(WorkerActorCommand::CancelPrompt { generation }) => { interactive.cancel_input(generation, &attempt_cancel.lock().expect("worker cancellation poisoned")); }
                    Some(WorkerActorCommand::Mirror(event)) => { log.emit(AgentRunEvent::Review { event: Box::new(event) }).await?; }
                    Some(WorkerActorCommand::Abandon { outcome, request_id }) => {
                        commands.close();
                        drop(queue_tx);
                        interactive.close_prompt();
                        let grace = Duration::from_secs(self.inner.config.cancel_grace_seconds);
                        driver.stop(grace).await;
                        interactive.release();
                        // Never mirror terminal abandonment ahead of concurrent
                        // settlement. Exclusion survives any sidecar failure.
                        let persistence = async {
                            anyhow::ensure!(outcome.is_sanitized_abandonment(), "invalid abandoned actor disposition");
                            log.emit(AgentRunEvent::Review { event: Box::new(WorkerReviewEvent::Abandoned { request_id }) }).await?;
                            log.append_outcome(*outcome).await
                        };
                        match tokio::time::timeout(grace, persistence).await {
                            Ok(Ok(())) => {}
                            result => tracing::warn!(target: "zevria::ensemble::interactive", worker = %descriptor.id, ?result, "abandonment mirror incomplete; root disposition remains authoritative"),
                        }
                        return Ok(());
                    }
                    Some(WorkerActorCommand::Finish { outcome, acknowledgement }) => {
                        let result = async {
                            let confirmed = outcome.confirmation.as_ref().context("sealed Plan outcome lacks a receipt")?;
                            log.emit(AgentRunEvent::Review { event: Box::new(WorkerReviewEvent::Confirmed { receipt: confirmed.receipt.clone() }) }).await?;
                            if let Some(receipt) = &confirmed.baseline {
                                log.emit(AgentRunEvent::Review { event: Box::new(WorkerReviewEvent::BaselineMarked { receipt: receipt.clone() }) }).await?;
                            }
                            log.emit(AgentRunEvent::Review { event: Box::new(WorkerReviewEvent::Sealed) }).await?;
                            log.append_outcome(*outcome).await
                        }.await.map_err(|error: anyhow::Error| error.to_string());
                        driver.stop(Duration::from_secs(self.inner.config.cancel_grace_seconds)).await;
                        let _ = acknowledgement.send(result);
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                result = &mut driver.task => return result.context("worker IO task stopped unexpectedly")?,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_interactive(
        &self,
        interactive: &Arc<InteractiveController>,
        mut queue: mpsc::UnboundedReceiver<WorkerInput>,
        log: &RunLog,
        agent: &EnsembleAgentConfig,
        turn: &TurnContext,
        root: &tokio_util::sync::CancellationToken,
        cancel_slot: Arc<Mutex<tokio_util::sync::CancellationToken>>,
        mut session_id: Option<String>,
        native_recovery_safe: bool,
    ) -> anyhow::Result<()> {
        if !native_recovery_safe {
            let diagnostic = "Claude artifact ownership is ambiguous after interrupted review. No provider was relaunched. Confirm the eligible retained proposal, or cancel this ensemble; automatic artifact-scheduler reconstruction is unavailable.".to_string();
            interactive
                .publication
                .send(WorkerReviewEvent::Connection {
                    connected: false,
                    diagnostic: Some(diagnostic.clone()),
                })
                .await?;
            while let Some(input) = next_input(&mut queue, root).await {
                interactive
                    .dispatch(input, log)
                    .await
                    .map_err(anyhow::Error::msg)?;
                interactive
                    .settle(log, Some(diagnostic.clone()), false)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            return Ok(());
        }
        let handoff = if interactive.native {
            Some(ClaudePlanHandoff::new(agent, &self.inner.workspace)?)
        } else {
            None
        };
        let mut first = true;
        let diagnostics = RawDiagnosticSink::spawn(log);
        let result = async {
            let mut recovery_context = None;
            loop {
                let cancellation = root.child_token();
                *cancel_slot.lock().expect("worker cancellation poisoned") = cancellation.clone();
                let attempt = interactive.attempt.fetch_add(1, AtomicOrdering::AcqRel) + 1;
                let continuing = interactive.current_input();
                if let Some(input) = continuing {
                    let event = WorkerReviewEvent::Recovering {
                        generation: input.generation,
                        attempt,
                    };
                    log.emit(AgentRunEvent::Review {
                        event: Box::new(event),
                    })
                    .await?;
                } else {
                    recovery_context = None;
                    let queued = if first {
                        queue.try_recv().ok()
                    } else {
                        None
                    };
                    if let Some(input) = queued {
                        interactive
                            .dispatch(input, log)
                            .await
                            .map_err(anyhow::Error::msg)?;
                    } else if !first || session_id.is_none() {
                        let Some(input) = next_input(&mut queue, root).await else {
                            return Ok(());
                        };
                        interactive
                            .dispatch(input, log)
                            .await
                            .map_err(anyhow::Error::msg)?;
                    }
                }
                first = false;
                if interactive.input_cancelled() {
                    interactive
                        .settle(
                            log,
                            Some("worker input cancelled before startup or dispatch".into()),
                            false,
                        )
                        .await
                        .map_err(anyhow::Error::msg)?;
                    continue;
                }
                if let Err(error) = interactive.acquire(&cancellation).await {
                    interactive
                        .settle(log, Some(error.to_string()), false)
                        .await
                        .map_err(anyhow::Error::msg)?;
                    continue;
                }
                let result = self
                    .run_worker_attempt(WorkerAttemptContext {
                        agent_config: agent,
                        descriptor: &interactive.descriptor,
                        workflow: EnsembleWorkflow::Plan,
                        base_prompt: &zevria_content::UserPrompt::default(),
                        mode: session_id
                            .clone()
                            .map_or(WorkerAttemptMode::Fresh, |session_id| {
                                WorkerAttemptMode::DurableRecovery {
                                    session_id: Some(session_id),
                                    repair: None,
                                }
                            }),
                        log,
                        cancellation: &cancellation,
                        plan_handoff: handoff.as_ref(),
                        transient_recovery: &interactive.transient,
                        turn,
                        diagnostics: diagnostics.sender(),
                        interactive: Some(InteractiveAttempt {
                            controller: interactive.clone(),
                            queue: &mut queue,
                        }),
                    })
                    .await;
                interactive.release();
                diagnostics.barrier().await;
                log.barrier().await?;
                if let Some(id) = &result.durable_session_id {
                    session_id = Some(id.clone());
                }
                if root.is_cancelled() {
                    return Ok(());
                }
                if !cancellation.is_cancelled()
                    && !interactive.input_cancelled()
                    && interactive.input.lock().expect("worker input poisoned").current.is_some()
                    && interactive.relaunches.load(AtomicOrdering::Acquire)
                        < MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS as u64
                    && let Some(error) = result.recoverable_process_error()
                {
                    let number = interactive.relaunches.fetch_add(1, AtomicOrdering::AcqRel) + 1;
                    recovery_context = Some(format!(
                        "automatic process recovery attempt {number} of {MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS} followed an unexpected process termination: {}",
                        concise_process_failure(error)
                    ));
                    log.emit(AgentRunEvent::Status {
                        status: AgentRunStatus::Resuming,
                        detail: recovery_context.clone(),
                    })
                    .await?;
                    interactive.continuing.store(true, AtomicOrdering::Release);
                    continue;
                }
                let mut failure = result
                    .violation
                    .unwrap_or_else(|| match result.connection_result {
                        Some(Err(error)) => error_with_login_hint(&error, &agent.login_hint),
                        Some(Ok(end)) => format!("{end:?}"),
                        None => "worker attempt cancelled".into(),
                    });
                for context in [
                    handoff
                        .as_ref()
                        .and_then(ClaudePlanHandoff::unresolved_violation),
                    interactive
                        .transient
                        .lock()
                        .expect("transient recovery poisoned")
                        .failure_context(&agent.login_hint),
                    recovery_context.take(),
                    handoff
                        .as_ref()
                        .and_then(ClaudePlanHandoff::abandoned_preparations),
                ]
                .into_iter()
                .flatten()
                {
                    if !failure.contains(&context) {
                        failure.push_str("; ");
                        failure.push_str(&context);
                    }
                }
                interactive
                    .settle(log, Some(failure), false)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
        }
        .await;
        diagnostics.finish().await;
        result
    }
}

pub(super) async fn optional_timeout<F: std::future::Future>(
    timeout: Option<Duration>,
    future: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future).await,
        None => Ok(future.await),
    }
}
