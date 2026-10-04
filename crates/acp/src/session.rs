#[cfg(test)]
#[path = "mode_tests.rs"]
mod mode_tests;
#[cfg(test)]
#[path = "retry_tests.rs"]
mod retry_tests;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use agent_client_protocol::schema::v1::{
    CreateElicitationResponse, CurrentModeUpdate, Error as AcpError, SessionId,
    SessionUpdate as AcpSessionUpdate, StopReason,
};
use agent_client_protocol::{ConnectionTo, SentRequest};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
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
    KnownTools, ResponseUsageSnapshot, diagnostic_update, plan_artifact_update,
    plan_ready_instructions, project_tool_calls, project_tool_results, usage_update,
    worker_plan_update,
};
use crate::stream::StreamSegments;
use crate::{ClientState, ExecutionProfile, RuntimeExit, SessionRuntimeLifecycle};

pub(crate) struct LiveSession {
    id: SessionId,
    workspace: PathBuf,
    connection: ConnectionTo<agent_client_protocol::Client>,
    client: ClientState,
    profile: ExecutionProfile,
    commands: Mutex<Option<mpsc::UnboundedSender<SessionCommand>>>,
    lifecycle: tokio::sync::Mutex<Option<Box<dyn SessionRuntimeLifecycle>>>,
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    state: Mutex<State>,
    elicitations: Mutex<HashMap<String, CancellationToken>>,
    closed: AtomicBool,
    next_management_request: std::sync::atomic::AtomicU64,
}

struct State {
    mode: SessionMode,
    plan: PlanWorkflowState,
    pending: Option<PendingPrompt>,
    pending_mode: Option<PendingMode>,
    skill_requests: HashMap<String, PendingSkillRequest>,
    last_terminal_turn: Option<TurnId>,
    streams: HashMap<TurnId, StreamSegments>,
    network_scope: Option<(TurnId, usize, usize)>,
    network_status: Option<zevria_session_api::event::NetworkStatus>,
    known_tools: KnownTools,
    hosted_search: crate::project::HostedSearchProjection,
    subtasks: crate::project::SubtaskProjection,
    emitted_plans: HashSet<PlanVersion>,
    unavailable: Option<String>,
    response_usage: Option<ResponseUsageSnapshot>,
    context_usage: Option<ContextTokenSnapshot>,
}

impl State {
    fn accept_network(&mut self, turn_id: TurnId, call: usize, attempt: usize) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.turn_id == Some(turn_id))
            || self.last_terminal_turn.is_some_and(|last| turn_id <= last)
            || !self
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
        self.network_scope = Some((turn_id, call, attempt));
        true
    }
}

struct PendingMode {
    request_id: String,
    mode: SessionMode,
    // The waiter can leave before the engine acknowledges its durable commit.
    // Correlation and the admission lock outlive that local RPC request.
    outcome: Option<oneshot::Sender<Result<(), AcpError>>>,
}

impl PendingMode {
    fn resolve_waiter(&mut self, result: Result<(), AcpError>) {
        if let Some(outcome) = self.outcome.take() {
            let _ = outcome.send(result);
        }
    }
}

struct PendingSkillRequest {
    mutation: bool,
    outcome: oneshot::Sender<zevria_instructions::skill::SkillManagementResult>,
}

struct PendingPrompt {
    outcome: oneshot::Sender<Result<StopReason, AcpError>>,
    turn_id: Option<TurnId>,
    mode: SessionMode,
    phase: PromptPhase,
    successful_plan_submit: bool,
    completed_plan_turn: bool,
    cancel_requested: bool,
}

#[derive(Debug)]
enum PromptPhase {
    Normal,
    WaitingForRevision {
        command: Box<zevria_session_api::TurnCommand>,
    },
    AwaitingReady,
    AwaitingDecision,
    DecisionRevise,
    AwaitingImplementation,
}

pub(crate) struct PromptWait {
    pub response: oneshot::Receiver<Result<StopReason, AcpError>>,
}

pub(crate) struct ModeWait {
    pub request_id: String,
    pub response: oneshot::Receiver<Result<(), AcpError>>,
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
        Arc::new(Self {
            id: SessionId::new(id),
            workspace,
            connection,
            client,
            profile,
            commands: Mutex::new(Some(commands)),
            lifecycle: tokio::sync::Mutex::new(Some(lifecycle)),
            event_task: Mutex::new(None),
            permit: Mutex::new(Some(permit)),
            state: Mutex::new(State {
                mode,
                plan: initial_plan,
                pending: None,
                pending_mode: None,
                skill_requests: HashMap::new(),
                last_terminal_turn: None,
                streams: HashMap::new(),
                network_scope: None,
                network_status: None,
                known_tools: KnownTools::new(),
                hosted_search: crate::project::HostedSearchProjection::default(),
                subtasks: crate::project::SubtaskProjection::default(),
                emitted_plans: HashSet::new(),
                unavailable: None,
                response_usage: None,
                context_usage: None,
            }),
            elicitations: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
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
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn mode(&self) -> SessionMode {
        self.state.lock().expect("ACP session state poisoned").mode
    }

    pub(crate) fn plan_state(&self) -> PlanWorkflowState {
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .plan
            .clone()
    }

    pub(crate) fn start_event_loop(
        self: &Arc<Self>,
        events: SessionEventReceiver,
        background_exit: std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeExit> + Send>>,
    ) {
        let session = Arc::clone(self);
        let task = tokio::spawn(async move {
            session.run_event_loop(events, background_exit).await;
        });
        *self
            .event_task
            .lock()
            .expect("ACP event task lock poisoned") = Some(task);
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

    pub(crate) fn emit_initial_plan(&self) -> Result<(), AcpError> {
        let artifact = match self.plan_state() {
            PlanWorkflowState::Ready { artifact } | PlanWorkflowState::Published { artifact } => {
                artifact
            }
            PlanWorkflowState::Planning {
                previous: Some(artifact),
                ..
            } if self.profile.is_worker() => artifact,
            _ => return Ok(()),
        };
        {
            self.state
                .lock()
                .expect("ACP session state poisoned")
                .emitted_plans
                .insert(artifact.version);
        }
        self.publish_plan(&artifact)
    }

    fn publish_plan(&self, artifact: &PlanArtifact) -> Result<(), AcpError> {
        if self.profile.is_worker() {
            self.require_plan_capability()?;
            self.send_update(worker_plan_update(artifact))
        } else if matches!(self.plan_state(), PlanWorkflowState::Published { .. }) {
            self.send_update(plan_artifact_update(artifact))
        } else {
            self.send_updates([
                plan_artifact_update(artifact),
                plan_ready_instructions(artifact),
            ])
        }
    }

    fn require_plan_capability(&self) -> Result<(), AcpError> {
        if self.profile.is_worker() && !self.client.plan_operations {
            return Err(invalid_params(
                "ensemble Plan workers require the ACP client Plan-operation capability",
            ));
        }
        Ok(())
    }

    pub(crate) fn set_mode(&self, mode: SessionMode) -> Result<ModeWait, AcpError> {
        if mode == SessionMode::Plan {
            self.require_plan_capability()?;
        }
        let request_id = format!(
            "acp-mode-{}",
            self.next_management_request.fetch_add(1, Ordering::Relaxed)
        );
        let (outcome, response) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if let Some(error) = &state.unavailable {
                return Err(AcpError::internal_error().data(error.clone()));
            }
            if self.is_closed() {
                return Err(invalid_params("session is closed"));
            }
            if state.pending.is_some() {
                return Err(invalid_params("cannot change mode during an active prompt"));
            }
            if state.pending_mode.is_some() || !state.skill_requests.is_empty() {
                return Err(invalid_params(
                    "cannot change mode while session management is pending",
                ));
            }
            if matches!(state.plan, PlanWorkflowState::Ready { .. })
                && !(self.profile.is_worker() && mode == SessionMode::Plan)
            {
                return Err(invalid_params(if self.profile.is_worker() {
                    "completed worker Plan reports retain their workflow; start a fresh worker session"
                } else {
                    "cannot change mode while a Plan artifact is awaiting a decision"
                }));
            }
            if self.profile.is_worker() {
                // Worker runtimes reject root management. Their Plan/Review
                // selection remains frontend-local and is carried by the turn.
                state.mode = mode;
                self.publish_mode(mode)?;
                let _ = outcome.send(Ok(()));
                return Ok(ModeWait {
                    request_id,
                    response,
                });
            }
            state.pending_mode = Some(PendingMode {
                request_id: request_id.clone(),
                mode,
                outcome: Some(outcome),
            });
        }
        if let Err(error) = self.send_command(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::SetMode {
                request_id: request_id.clone(),
                mode,
            },
        )) {
            // A failed enqueue definitely did not commit. Unlike abandoning
            // an enqueued request, it is safe to release the admission lock.
            self.discard_mode(Some(&request_id), error.clone());
            return Err(error);
        }
        Ok(ModeWait {
            request_id,
            response,
        })
    }

    pub(crate) fn interrupt_mode(&self, request_id: Option<&str>, error: AcpError) {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if let Some(pending) = state
            .pending_mode
            .as_mut()
            .filter(|pending| request_id.is_none_or(|request_id| pending.request_id == request_id))
        {
            // SetMode has no cancellation operation once enqueued. Only stop
            // waiting locally: the correlated acknowledgment must still select
            // the committed mode before prompts or more management can enter.
            // This also keeps timeout and waiter-spawn failures fail-closed.
            pending.resolve_waiter(Err(error));
        }
    }

    // Only use when the command was not enqueued, or admission is permanently
    // closed by shutdown. Other interruptions must preserve pending correlation.
    fn discard_mode(&self, request_id: Option<&str>, error: AcpError) {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if state.pending_mode.as_ref().is_some_and(|pending| {
            request_id.is_none_or(|request_id| pending.request_id == request_id)
        }) && let Some(mut pending) = state.pending_mode.take()
        {
            pending.resolve_waiter(Err(error));
        }
    }

    fn publish_mode(&self, mode: SessionMode) -> Result<(), AcpError> {
        self.send_update(AcpSessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            mode_id(mode, self.profile),
        )))
    }

    fn select_workflow_mode(&self, mode: SessionMode) -> Result<(), AcpError> {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if state.mode != mode {
            state.mode = mode;
            self.publish_mode(mode)?;
        }
        Ok(())
    }

    fn handle_mode_result(
        &self,
        request_id: String,
        result: ModeSelectionResult,
    ) -> Result<(), AcpError> {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if !state
            .pending_mode
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            // Unmatched or already-resolved replies cannot overwrite a newer
            // selection. An abandoned waiter still has a correlated pending
            // entry and must reconcile the engine's durable result below.
            return Ok(());
        }
        let pending = state.pending_mode.as_mut().expect("matched pending mode");
        if let ModeSelectionResult::Accepted { mode, .. } = &result
            && *mode != pending.mode
        {
            let error =
                AcpError::internal_error().data("engine acknowledged a different session mode");
            pending.resolve_waiter(Err(error.clone()));
            // This is a protocol failure, not a rejection of the commit. Keep
            // admission locked until the event loop shuts down the runtime.
            return Err(error);
        }
        let mut pending = state.pending_mode.take().expect("matched pending mode");
        match result {
            ModeSelectionResult::Accepted { mode, .. } => {
                state.mode = mode;
                let published = self.publish_mode(mode);
                pending.resolve_waiter(published.clone());
                published
            }
            ModeSelectionResult::Rejected { code, message } => {
                pending.resolve_waiter(Err(AcpError::invalid_params()
                    .data(serde_json::json!({ "code": code, "message": message }))));
                Ok(())
            }
        }
    }

    pub(crate) async fn manage_skills(
        &self,
        request: zevria_instructions::skill::SkillManagementRequest,
    ) -> Result<crate::skills::SkillsResponse, AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params(
                "skill management is disabled for ensemble workers",
            ));
        }
        if self.is_closed() {
            return Err(invalid_params("session is closed"));
        }
        let id = format!(
            "acp-skills-{}",
            self.next_management_request.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.pending_mode.is_some() {
                return Err(invalid_params(
                    "mode selection is pending; skill request was not queued",
                ));
            }
            if request.is_mutation() && state.pending.is_some() {
                return Err(invalid_params(
                    "skill mutations require an idle session; request was not queued",
                ));
            }
            if state.skill_requests.len() >= 32 {
                return Err(invalid_params("too many pending skill requests"));
            }
            state.skill_requests.insert(
                id.clone(),
                PendingSkillRequest {
                    mutation: request.is_mutation(),
                    outcome: sender,
                },
            );
        }
        if let Err(error) = self.send_command(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Skills {
                request_id: id.clone(),
                request,
            },
        )) {
            self.state
                .lock()
                .expect("ACP session state poisoned")
                .skill_requests
                .remove(&id);
            return Err(error);
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await;
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .skill_requests
            .remove(&id);
        match outcome {
            Ok(Ok(result)) => Ok(crate::skills::SkillsResponse { version: crate::skills::SKILLS_EXTENSION_VERSION, result }),
            _ => Err(AcpError::internal_error().data("skill request interrupted or timed out; query the installed revision before retrying a mutation")),
        }
    }

    pub(crate) fn begin_skill(
        &self,
        name: zevria_instructions::skill::SkillName,
        args: zevria_content::UserPrompt,
    ) -> Result<PromptWait, AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params("skills are disabled for ensemble workers"));
        }
        if args.text_len() > 64 * 1024 {
            return Err(invalid_params("skill arguments exceed 64 KiB"));
        }
        self.begin_input(
            None,
            Some((name, args)),
            zevria_foundation::RequestBehavior::Standard,
        )
    }

    #[cfg(test)]
    pub(crate) fn begin_prompt(
        &self,
        text: zevria_content::UserPrompt,
    ) -> Result<PromptWait, AcpError> {
        self.begin_prompt_with_behavior(text, zevria_foundation::RequestBehavior::Standard)
    }

    pub(crate) fn begin_prompt_with_behavior(
        &self,
        text: zevria_content::UserPrompt,
        behavior: zevria_foundation::RequestBehavior,
    ) -> Result<PromptWait, AcpError> {
        self.begin_input(Some(text), None, behavior)
    }

    fn begin_input(
        &self,
        text: Option<zevria_content::UserPrompt>,
        skill: Option<(
            zevria_instructions::skill::SkillName,
            zevria_content::UserPrompt,
        )>,
        behavior: zevria_foundation::RequestBehavior,
    ) -> Result<PromptWait, AcpError> {
        let prompt = text
            .as_ref()
            .or_else(|| skill.as_ref().map(|(_, args)| args))
            .expect("input");
        prompt
            .validate()
            .map_err(|error| invalid_params(error.to_string()))?;
        let implement = behavior == zevria_foundation::RequestBehavior::Standard
            && text.as_ref().is_some_and(|prompt| {
                !prompt.has_images() && prompt.text_projection().trim() == "/implement"
            });
        if prompt.has_images()
            && matches!(prompt.blocks().first(), Some(zevria_content::PromptBlock::Text(prefix)) if prefix.starts_with("/implement") && prefix.split_whitespace().next().is_some_and(|command| matches!(command, "/implement" | "/implement-fresh")))
        {
            return Err(invalid_params(
                "implementation controls do not accept images",
            ));
        }
        let (outcome, response) = oneshot::channel();
        let command = {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if let Some(error) = &state.unavailable {
                return Err(AcpError::internal_error().data(error.clone()));
            }
            if state.pending.is_some() {
                return Err(invalid_params(
                    "this session already has an in-flight prompt",
                ));
            }
            if self.is_closed() {
                return Err(invalid_params("session is closed"));
            }
            if state.pending_mode.is_some()
                || state
                    .skill_requests
                    .values()
                    .any(|request| request.mutation)
            {
                return Err(invalid_params(
                    "session management is pending; prompt was not queued",
                ));
            }
            if behavior == zevria_foundation::RequestBehavior::Orchestrate
                && (self.profile.is_worker()
                    || state.mode != SessionMode::Build
                    || matches!(
                        state.plan,
                        PlanWorkflowState::Ready { .. } | PlanWorkflowState::Planning { .. }
                    ))
            {
                return Err(invalid_params(
                    "zevria.orchestration requires root Build without a pending Plan workflow; it is not Plan approval",
                ));
            }
            let blank_text = text
                .as_ref()
                .is_some_and(zevria_content::UserPrompt::is_blank);
            if self.profile.is_worker() {
                if matches!(
                    prompt.text_projection().split_whitespace().next(),
                    Some("/build" | "/plan")
                ) {
                    return Err(invalid_params(
                        "root mode commands are unavailable to ensemble workers; use the structured Plan/Review mode API",
                    ));
                }
                if skill.is_some() || implement {
                    return Err(invalid_params(
                        "ensemble workers are report-only; skills and implementation are disabled",
                    ));
                }
                if state.mode == SessionMode::Plan {
                    self.require_plan_capability()?;
                }
                if blank_text {
                    return Err(invalid_params("worker feedback must not be empty"));
                }
            }
            let input_mode = if matches!(state.plan, PlanWorkflowState::Ready { .. }) {
                SessionMode::Plan
            } else {
                state.mode
            };
            let input = match skill {
                Some((name, args)) => zevria_session_api::TurnCommand::InvokeSkill {
                    name,
                    args,
                    mode: input_mode,
                },
                None => zevria_session_api::TurnCommand::Submit {
                    text: text.clone().expect("text input"),
                    mode: input_mode,
                    behavior,
                },
            };
            let (phase, mode, command) = match &state.plan {
                _ if blank_text => (PromptPhase::Normal, input_mode, input),
                PlanWorkflowState::Ready { artifact }
                | PlanWorkflowState::Published { artifact }
                    if implement =>
                {
                    (
                        PromptPhase::AwaitingImplementation,
                        SessionMode::Build,
                        zevria_session_api::TurnCommand::ResolvePlan {
                            expected: artifact.version,
                            decision: PlanDecision::ImplementCurrent,
                        },
                    )
                }
                PlanWorkflowState::Ready { artifact }
                    if matches!(input, zevria_session_api::TurnCommand::InvokeSkill { .. }) =>
                {
                    let zevria_session_api::TurnCommand::InvokeSkill { name, args, .. } = input
                    else {
                        unreachable!()
                    };
                    (
                        PromptPhase::Normal,
                        SessionMode::Plan,
                        zevria_session_api::TurnCommand::RevisePlanWithSkill {
                            expected: artifact.version,
                            name,
                            args,
                        },
                    )
                }
                PlanWorkflowState::Ready { artifact } => (
                    PromptPhase::WaitingForRevision {
                        command: Box::new(input.clone()),
                    },
                    SessionMode::Plan,
                    zevria_session_api::TurnCommand::ResolvePlan {
                        expected: artifact.version,
                        decision: PlanDecision::Revise,
                    },
                ),
                _ => (PromptPhase::Normal, state.mode, input),
            };
            state.pending = Some(PendingPrompt {
                outcome,
                turn_id: None,
                mode,
                phase,
                successful_plan_submit: false,
                completed_plan_turn: false,
                cancel_requested: false,
            });
            command
        };

        if let Err(error) = self.send_command(SessionCommand::Turn(command)) {
            self.finish_prompt(Err(error.clone()));
            return Err(error);
        }
        Ok(PromptWait { response })
    }

    pub(crate) fn cancel(&self) {
        self.interrupt_mode(None, AcpError::request_cancelled());
        self.cancel_elicitations();
        let (turn_id, complete_now) = {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            let Some(pending) = &mut state.pending else {
                return;
            };
            pending.cancel_requested = true;
            // An unaccepted normal prompt still owns queued engine work. Wait
            // for its terminal event rather than attach a late TurnStarted to
            // a newer prompt. Deferred Plan input has not been sent yet.
            let complete_now = matches!(
                pending.phase,
                PromptPhase::WaitingForRevision { .. }
                    | PromptPhase::AwaitingReady
                    | PromptPhase::AwaitingDecision
                    | PromptPhase::DecisionRevise
            );
            (pending.turn_id, complete_now)
        };
        let _ = self.send_command(SessionCommand::Control(
            zevria_session_api::ControlCommand::CancelTurn { turn_id },
        ));
        if complete_now {
            self.finish_prompt(Ok(StopReason::Cancelled));
        }
    }

    pub(crate) async fn shutdown(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.cancel();
        self.finish_prompt(Ok(StopReason::Cancelled));
        let event_task = {
            self.event_task
                .lock()
                .expect("ACP event task lock poisoned")
                .take()
        };
        if let Some(task) = event_task {
            task.abort();
            let _ = task.await;
        }
        self.shutdown_runtime().await
    }

    async fn shutdown_runtime(&self) -> anyhow::Result<()> {
        self.discard_mode(
            None,
            AcpError::request_cancelled().data("session runtime is closing"),
        );
        self.cancel_elicitations();
        self.state
            .lock()
            .expect("ACP session state poisoned")
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
        result
    }

    fn send_command(&self, command: SessionCommand) -> Result<(), AcpError> {
        // Shutdown remains available after the runtime is marked unavailable.
        if !matches!(
            command,
            SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown)
        ) {
            let state = self.state.lock().expect("ACP session state poisoned");
            if let Some(error) = &state.unavailable {
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
        let cleanup = Arc::clone(&self);
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            if let Err(error) = cleanup.shutdown().await {
                tracing::warn!(%error, "failed to clean up an unavailable ACP session runtime");
            }
        });
    }

    fn handle_update(self: &Arc<Self>, update: SessionUpdate) -> Result<(), AcpError> {
        match update {
            SessionUpdate::Streams(batch) => {
                if let Some(root) = batch.root {
                    let updates = {
                        let mut state = self.state.lock().expect("ACP session state poisoned");
                        let mut updates = root
                            .attempt
                            .as_ref()
                            .map_or_else(Vec::new, |attempt| state.hosted_search.update(attempt));
                        let streams = state.streams.entry(root.turn_id).or_default();
                        if root.message.is_some() || root.attempt.is_some() {
                            updates.extend(streams.display_snapshot(
                                root.turn_id,
                                zevria_content::AssistantStreamSnapshot {
                                    message: root.message,
                                    attempt: root.attempt,
                                },
                            ));
                        } else {
                            streams.reset();
                        }
                        updates
                    };
                    self.send_updates(updates)?;
                }
            }
            SessionUpdate::Lifecycle(event) => self.handle_event(event)?,
        }
        Ok(())
    }

    fn handle_event(self: &Arc<Self>, event: SessionEvent) -> Result<(), AcpError> {
        match event {
            SessionEvent::WebSearchUpdated { turn_id, attempt } => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    let mut updates = state.hosted_search.update(&attempt);
                    updates.extend(state.streams.entry(turn_id).or_default().display_snapshot(
                        turn_id,
                        zevria_content::AssistantStreamSnapshot {
                            message: None,
                            attempt: Some(attempt),
                        },
                    ));
                    updates
                };
                self.send_updates(updates)?;
            }
            SessionEvent::AssistantStreamUpdated { turn_id, snapshot } => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    let mut updates = snapshot
                        .attempt
                        .as_ref()
                        .map_or_else(Vec::new, |attempt| state.hosted_search.update(attempt));
                    updates.extend(
                        state
                            .streams
                            .entry(turn_id)
                            .or_default()
                            .display_snapshot(turn_id, snapshot),
                    );
                    updates
                };
                self.send_updates(updates)?;
            }
            // Model management is local TUI traffic, never assistant/tool content.
            SessionEvent::ModelsResult { .. }
            | SessionEvent::WorkerReviewUpdated { .. }
            | SessionEvent::WorkerControlResult { .. } => {}
            SessionEvent::ModeResult { request_id, result } => {
                self.handle_mode_result(request_id, result)?;
            }
            SessionEvent::ModeChanged { mode } => {
                let mode = if matches!(self.plan_state(), PlanWorkflowState::Ready { .. }) {
                    SessionMode::Plan
                } else {
                    mode
                };
                self.select_workflow_mode(mode)?;
            }
            SessionEvent::SkillsResult { request_id, result } => {
                if let Some(pending) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .skill_requests
                    .remove(&request_id)
                {
                    let _ = pending.outcome.send(result);
                }
            }
            SessionEvent::SkillsChanged { revision, counts } => {
                self.connection
                    .send_notification(crate::skills::SkillsChangedNotification {
                        version: crate::skills::SKILLS_EXTENSION_VERSION,
                        session_id: self.id.clone(),
                        revision,
                        counts,
                    })?;
            }
            SessionEvent::ModelCallStarted { turn_id, call } => {
                let mut state = self.state.lock().expect("ACP session state poisoned");
                if call > 0
                    && state
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == Some(turn_id))
                    && state
                        .network_scope
                        .is_none_or(|(id, previous, _)| id == turn_id && call > previous)
                {
                    state.network_scope = Some((turn_id, call, 0));
                    state.network_status = None;
                }
            }
            SessionEvent::TurnStarted { turn_id, mode, .. } => {
                let mut state = self.state.lock().expect("ACP session state poisoned");
                // ModeChanged owns explicit workflow selection. A turn's
                // execution mode is not a replacement for that selection.
                state.streams.entry(turn_id).or_default().reset();
                state.network_scope = Some((turn_id, 1, 0));
                state.network_status = None;
                if let Some(pending) = &mut state.pending {
                    pending.turn_id = Some(turn_id);
                    pending.mode = mode;
                }
            }
            SessionEvent::StreamCleared { turn_id } => {
                if let Some(streams) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .streams
                    .get_mut(&turn_id)
                {
                    streams.reset();
                }
            }
            SessionEvent::Intermediate {
                turn_id,
                message,
                display_attempt_id,
            } => {
                let (stream_updates, tool_updates) = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    let stream_updates = state.streams.entry(turn_id).or_default().terminal_bound(
                        turn_id,
                        &message,
                        display_attempt_id.as_deref(),
                    );
                    state.streams.entry(turn_id).or_default().reset();
                    let tool_updates =
                        project_tool_calls(&message, &self.workspace, &mut state.known_tools);
                    (stream_updates, tool_updates)
                };
                self.send_updates(stream_updates)?;
                self.send_updates(tool_updates)?;
            }
            SessionEvent::ToolResults {
                turn_id,
                message,
                metadata,
            } => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if metadata.iter().any(|metadata| {
                        metadata.tool_name == zevria_foundation::SUBMIT_PLAN_TOOL_NAME
                            && metadata.outcome == ToolCallOutcome::Success
                    }) && let Some(pending) = &mut state.pending
                        && pending.turn_id == Some(turn_id)
                    {
                        pending.successful_plan_submit = true;
                    }
                    let mut updates = state.subtasks.results(&metadata);
                    updates.extend(project_tool_results(
                        &message,
                        &metadata,
                        &self.workspace,
                        &state.known_tools,
                    ));
                    updates
                };
                self.send_updates(updates)?;
            }
            SessionEvent::UsageUpdated {
                turn_id: _,
                usage,
                profile,
                model_role,
                input_token_limit,
                context_window_tokens,
            } => {
                let update = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    state.response_usage = Some(ResponseUsageSnapshot {
                        usage,
                        profile,
                        model_role,
                        input_token_limit,
                        context_window_tokens,
                    });
                    state
                        .context_usage
                        .as_ref()
                        .map(|context| usage_update(state.response_usage.as_ref(), context))
                };
                if let Some(update) = update {
                    self.send_update(update)?;
                }
            }
            SessionEvent::ContextUsageUpdated {
                turn_id: _,
                snapshot,
            } => {
                let update = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    state.context_usage = Some(snapshot);
                    usage_update(
                        state.response_usage.as_ref(),
                        state
                            .context_usage
                            .as_ref()
                            .expect("context usage was just installed"),
                    )
                };
                self.send_update(update)?;
            }
            SessionEvent::TurnCompleted {
                turn_id,
                message,
                display_attempt_id,
            } => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    let updates = state.streams.entry(turn_id).or_default().terminal_bound(
                        turn_id,
                        &message,
                        display_attempt_id.as_deref(),
                    );
                    state.streams.entry(turn_id).or_default().reset();
                    updates
                };
                self.send_updates(updates)?;
                self.handle_turn_terminal(turn_id, false, None)?;
            }
            SessionEvent::TurnRecovered { turn_id, .. } => {
                self.handle_turn_terminal(turn_id, false, None)?;
            }
            SessionEvent::TurnCancelled { turn_id } => {
                self.handle_turn_terminal(turn_id, true, None)?;
            }
            SessionEvent::TurnRejected { turn_id, error } => {
                self.send_update(diagnostic_update(Some(turn_id), "rejection", &error))?;
                self.handle_turn_terminal(turn_id, false, Some(error))?;
            }
            SessionEvent::TurnFailed { turn_id, error } => {
                self.send_update(diagnostic_update(Some(turn_id), "failure", &error))?;
                self.handle_turn_terminal(turn_id, false, Some(error))?;
            }
            SessionEvent::NetworkStatus {
                turn_id,
                call,
                attempt,
                max_attempts,
                transport,
                status,
            } => {
                use zevria_session_api::event::NetworkStatus;
                let report = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if !state.accept_network(turn_id, call, attempt)
                        || state.network_status.as_ref() == Some(&status)
                    {
                        false
                    } else {
                        // Routine first-attempt phases are internal metadata,
                        // not diagnostics. Still acknowledge recovery from a
                        // displayed quiet warning, even on the first attempt.
                        let report = match &status {
                            NetworkStatus::Quiet { .. } => true,
                            NetworkStatus::ProgressResumed => {
                                attempt > 1
                                    || matches!(
                                        state.network_status,
                                        Some(NetworkStatus::Quiet { .. })
                                    )
                            }
                            NetworkStatus::AttemptStarted
                            | NetworkStatus::Connecting
                            | NetworkStatus::AwaitingResponse => attempt > 1,
                        };
                        state.network_status = Some(status.clone());
                        report
                    }
                };
                if report {
                    let message = match status {
                        NetworkStatus::AttemptStarted => {
                            format!("Starting {transport:?} attempt {attempt}/{max_attempts}")
                        }
                        NetworkStatus::Connecting => format!(
                            "Connecting via {transport:?} · attempt {attempt}/{max_attempts}"
                        ),
                        NetworkStatus::AwaitingResponse => {
                            format!("Awaiting response · attempt {attempt}/{max_attempts}")
                        }
                        NetworkStatus::Quiet { idle_for, retry_in } => format!(
                            "No response progress for {}s · {} in {}m {}s · attempt {attempt}/{max_attempts}",
                            idle_for.as_secs(),
                            if attempt < max_attempts {
                                "automatic retry"
                            } else {
                                "request will stop"
                            },
                            retry_in.as_secs() / 60,
                            retry_in.as_secs() % 60
                        ),
                        NetworkStatus::ProgressResumed => "Response progress resumed".to_string(),
                    };
                    self.send_update(diagnostic_update(
                        Some(turn_id),
                        &format!("network_{call}_{attempt}"),
                        &message,
                    ))?;
                }
            }
            SessionEvent::TurnRetrying {
                turn_id,
                call,
                attempt,
                max_attempts,
                retry_after,
                error,
            } => {
                {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if !state.accept_network(turn_id, call, attempt) {
                        return Ok(());
                    }
                    state.network_status = None;
                }
                self.send_update(retry_diagnostic(
                    turn_id,
                    attempt,
                    max_attempts,
                    retry_after,
                    &error,
                ))?;
                if let Some(streams) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .streams
                    .get_mut(&turn_id)
                {
                    streams.reset();
                }
            }
            SessionEvent::PlanStateChanged { state } => {
                self.handle_plan_state(state)?;
            }
            SessionEvent::PlanHandoffStarted { turn_id, handoff } => {
                if self.profile.is_worker() {
                    return Err(invalid_params(
                        "ensemble workers cannot start implementation",
                    ));
                }
                {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if let Some(pending) = &mut state.pending {
                        pending.turn_id = Some(turn_id);
                        pending.mode = SessionMode::Build;
                        pending.phase = PromptPhase::AwaitingImplementation;
                    }
                }
                self.select_workflow_mode(SessionMode::Build)?;
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "plan-handoff",
                    format!(
                        "Implementing approved Plan {} in this session.",
                        handoff.artifact.version
                    ),
                ))?;
            }
            SessionEvent::FreshPlanHandoffRequested { handoff } => {
                let error = format!(
                    "Plan {} requested Implement Fresh, which ACP v1 does not support; the durable Ready artifact was left unchanged.",
                    handoff.artifact.version
                );
                self.send_update(diagnostic_update(None, "fresh-plan-unsupported", &error))?;
                self.finish_prompt(Err(invalid_params(error)));
            }
            SessionEvent::PlanProjectionWarning {
                version,
                path,
                error,
            } => {
                self.send_update(diagnostic_update(
                    None,
                    "plan-projection",
                    format!(
                        "Plan {version} remains durable, but {} could not be projected: {error}",
                        path.display()
                    ),
                ))?;
            }
            SessionEvent::CompactionStarted { turn_id, trigger } => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "compaction-start",
                    format!("Creating a context checkpoint ({trigger:?})."),
                ))?;
            }
            SessionEvent::CompactionCompleted {
                turn_id,
                trigger,
                backend,
            } => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "compaction-complete",
                    format!("Context checkpoint completed ({trigger:?}, {backend:?})."),
                ))?;
            }
            SessionEvent::QuestionAsked { request, .. } => {
                self.start_question(request)?;
            }
            SessionEvent::QuestionClosed { request_id, .. } => {
                self.cancel_elicitation(request_id.as_str());
            }
            SessionEvent::SubtaskLaunched {
                call_id,
                entry_index,
                descriptor,
                ..
            } => {
                let update = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .subtasks
                    .launch(&call_id, entry_index, descriptor);
                self.send_update(update)?;
            }
            SessionEvent::SubtaskStatus { id, status, .. } => {
                let update = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .subtasks
                    .status(&id, status);
                if let Some(update) = update {
                    self.send_update(update)?;
                }
            }
            SessionEvent::SubtaskSession { .. } => {}
            SessionEvent::PersistenceChanged { path, error } => {
                let detail = error.map_or_else(
                    || format!("Transcript persistence recovered at {}.", path.display()),
                    |error| {
                        format!(
                            "Transcript persistence degraded at {}: {error}",
                            path.display()
                        )
                    },
                );
                self.send_update(diagnostic_update(None, "persistence", detail))?;
            }
            SessionEvent::EnsembleStarted { turn_id, .. }
            | SessionEvent::EnsembleReportsReady { turn_id, .. } => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "ensemble-recovery",
                    "Recovered durable ensemble workflow state from this transcript.",
                ))?;
            }
            SessionEvent::AgentRunUpdated { turn_id, .. }
            | SessionEvent::AgentRunFinished { turn_id, .. } => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "ensemble-worker",
                    "Recovered an ensemble worker lifecycle update.",
                ))?;
            }
        }
        Ok(())
    }

    fn handle_turn_terminal(
        self: &Arc<Self>,
        turn_id: TurnId,
        cancelled: bool,
        failure: Option<String>,
    ) -> Result<(), AcpError> {
        let mut start_decision = None;
        let outcome = {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.last_terminal_turn.is_some_and(|id| turn_id <= id) {
                return Ok(());
            }
            state.last_terminal_turn = Some(turn_id);
            if state.network_scope.is_some_and(|(id, _, _)| id == turn_id) {
                state.network_scope = None;
                state.network_status = None;
            }
            let ready_artifact = match &state.plan {
                PlanWorkflowState::Ready { artifact } => Some(artifact.clone()),
                _ => None,
            };
            let Some(pending) = &mut state.pending else {
                return Ok(());
            };
            if pending.turn_id.is_some_and(|id| id != turn_id)
                || (pending.turn_id.is_none() && failure.is_none() && !cancelled)
            {
                return Ok(());
            }
            if cancelled || pending.cancel_requested {
                Some(Ok(StopReason::Cancelled))
            } else if let Some(error) = failure {
                Some(Err(AcpError::internal_error().data(error)))
            } else if pending.mode == SessionMode::Plan
                && (pending.successful_plan_submit
                    || (self.profile.is_worker() && ready_artifact.is_some()))
            {
                pending.completed_plan_turn = true;
                if let Some(artifact) = ready_artifact {
                    if self.profile.is_worker() {
                        // Ready publication happened before installing the
                        // terminal result. A tool result alone is not proof.
                        Some(Ok(StopReason::EndTurn))
                    } else {
                        pending.phase = PromptPhase::AwaitingDecision;
                        start_decision = Some(artifact);
                        None
                    }
                } else {
                    pending.phase = PromptPhase::AwaitingReady;
                    None
                }
            } else {
                Some(Ok(StopReason::EndTurn))
            }
        };
        if let Some(outcome) = outcome {
            self.finish_prompt(outcome);
        }
        if let Some(artifact) = start_decision {
            self.start_plan_decision(artifact)?;
        }
        Ok(())
    }

    fn handle_plan_state(self: &Arc<Self>, plan: PlanWorkflowState) -> Result<(), AcpError> {
        let mut command = None;
        let mut complete = None;
        let mut ready = None;
        let mut mode_changed = false;
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            state.plan = plan.clone();
            match &plan {
                PlanWorkflowState::Planning { .. } => {
                    if let Some(pending) = &mut state.pending {
                        match std::mem::replace(&mut pending.phase, PromptPhase::Normal) {
                            PromptPhase::WaitingForRevision { command: input } => {
                                pending.mode = SessionMode::Plan;
                                command = Some(*input);
                            }
                            PromptPhase::DecisionRevise => {
                                complete = Some(Ok(StopReason::EndTurn));
                            }
                            phase => pending.phase = phase,
                        }
                    }
                }
                PlanWorkflowState::Ready { artifact } => {
                    mode_changed = state.mode != SessionMode::Plan;
                    state.mode = SessionMode::Plan;
                    if state.emitted_plans.insert(artifact.version) {
                        ready = Some(artifact.clone());
                    }
                    if let Some(pending) = &mut state.pending
                        && pending.successful_plan_submit
                        && pending.completed_plan_turn
                    {
                        if self.profile.is_worker() {
                            complete = Some(Ok(StopReason::EndTurn));
                        } else {
                            pending.phase = PromptPhase::AwaitingDecision;
                        }
                    }
                }
                PlanWorkflowState::Published { artifact } => {
                    if state.emitted_plans.insert(artifact.version) {
                        ready = Some(artifact.clone());
                    }
                    complete = Some(Ok(StopReason::EndTurn));
                }
                PlanWorkflowState::Idle | PlanWorkflowState::Resolved { .. } => {}
            }
        }
        if mode_changed {
            self.publish_mode(SessionMode::Plan)?;
        }
        if let Some(artifact) = &ready {
            self.publish_plan(artifact)?;
        }
        if let Some(command) = command {
            self.send_command(SessionCommand::Turn(command))?;
        }
        if let Some(outcome) = complete {
            self.finish_prompt(outcome);
        }
        if let PlanWorkflowState::Ready { artifact } = plan
            && !self.profile.is_worker()
        {
            let should_decide = self
                .state
                .lock()
                .expect("ACP session state poisoned")
                .pending
                .as_ref()
                .is_some_and(|pending| {
                    pending.successful_plan_submit
                        && pending.completed_plan_turn
                        && matches!(pending.phase, PromptPhase::AwaitingDecision)
                });
            if should_decide {
                self.start_plan_decision(artifact)?;
            }
        }
        Ok(())
    }

    fn start_question(
        self: &Arc<Self>,
        request: zevria_foundation::QuestionRequest,
    ) -> Result<(), AcpError> {
        if !self.client.form_elicitation {
            self.send_command(SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id: request.id.clone(),
                    response: QuestionResponse::Dismissed,
                },
            ))?;
            self.send_update(diagnostic_update(
                None,
                "question-unavailable",
                "The ACP client does not advertise form elicitation; the question was dismissed so the model can continue.",
            ))?;
            return Ok(());
        }
        let form = match question_form(&self.id, &request) {
            Ok(form) => form,
            Err(error) => {
                self.send_command(SessionCommand::Control(
                    zevria_session_api::ControlCommand::AnswerQuestion {
                        request_id: request.id.clone(),
                        response: QuestionResponse::Dismissed,
                    },
                ))?;
                self.send_update(diagnostic_update(None, "question-invalid", error))?;
                return Ok(());
            }
        };
        let token = CancellationToken::new();
        self.elicitations
            .lock()
            .expect("ACP elicitation lock poisoned")
            .insert(request.id.to_string(), token.clone());
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let sent = session.connection.send_request(form.request.clone());
            let response = await_elicitation(sent, token.clone()).await;
            if token.is_cancelled() {
                return;
            }
            let answer = match response {
                Ok(response) => match form.response(response) {
                    Ok(answer) => answer,
                    Err(error) => {
                        let _ = session.send_update(diagnostic_update(
                            None,
                            "question-response-invalid",
                            format!(
                                "The ACP client returned an invalid question response: {error}"
                            ),
                        ));
                        QuestionResponse::Dismissed
                    }
                },
                Err(error) => {
                    let _ = session.send_update(diagnostic_update(
                        None,
                        "question-response-unavailable",
                        format!("The ACP question could not be completed: {error}"),
                    ));
                    QuestionResponse::Dismissed
                }
            };
            session.cancel_elicitation(request.id.as_str());
            let _ = session.send_command(SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id: request.id,
                    response: answer,
                },
            ));
        });
        Ok(())
    }

    fn start_plan_decision(self: &Arc<Self>, artifact: PlanArtifact) -> Result<(), AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params(
                "ensemble worker Plans are reports, not approval requests",
            ));
        }
        const KEY: &str = "__zevria_plan_decision";
        if !self.client.form_elicitation {
            self.finish_prompt(Ok(StopReason::EndTurn));
            return Ok(());
        }
        if self
            .elicitations
            .lock()
            .expect("ACP elicitation lock poisoned")
            .contains_key(KEY)
        {
            return Ok(());
        }
        let token = CancellationToken::new();
        self.elicitations
            .lock()
            .expect("ACP elicitation lock poisoned")
            .insert(KEY.to_string(), token.clone());
        let request = plan_decision_request(&self.id, &artifact.title);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let sent = session.connection.send_request(request);
            let response = await_elicitation(sent, token.clone()).await;
            if token.is_cancelled() {
                return;
            }
            session.cancel_elicitation(KEY);
            let choice = match response {
                Ok(response) => match plan_choice(response) {
                    Ok(choice) => choice,
                    Err(error) => {
                        let _ = session.send_update(diagnostic_update(
                            None,
                            "plan-decision-invalid",
                            format!("The ACP client returned an invalid Plan decision: {error}"),
                        ));
                        PlanChoice::Declined
                    }
                },
                Err(error) => {
                    let _ = session.send_update(diagnostic_update(
                        None,
                        "plan-decision-unavailable",
                        format!("The ACP Plan decision could not be completed: {error}"),
                    ));
                    PlanChoice::Declined
                }
            };
            let _ = session.apply_plan_choice(artifact.version, choice);
        });
        Ok(())
    }

    fn apply_plan_choice(&self, version: PlanVersion, choice: PlanChoice) -> Result<(), AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params(
                "only the parent ensemble can approve implementation",
            ));
        }
        match choice {
            PlanChoice::Declined => {
                self.finish_prompt(Ok(StopReason::EndTurn));
                Ok(())
            }
            PlanChoice::Revise => {
                if let Some(pending) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .pending
                    .as_mut()
                {
                    pending.phase = PromptPhase::DecisionRevise;
                }
                self.send_command(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::ResolvePlan {
                        expected: version,
                        decision: PlanDecision::Revise,
                    },
                ))
            }
            PlanChoice::ImplementCurrent => {
                if let Some(pending) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .pending
                    .as_mut()
                {
                    pending.phase = PromptPhase::AwaitingImplementation;
                    pending.turn_id = None;
                    pending.mode = SessionMode::Build;
                }
                self.send_command(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::ResolvePlan {
                        expected: version,
                        decision: PlanDecision::ImplementCurrent,
                    },
                ))
            }
        }
    }

    fn finish_prompt(&self, outcome: Result<StopReason, AcpError>) {
        let pending = self
            .state
            .lock()
            .expect("ACP session state poisoned")
            .pending
            .take();
        if let Some(pending) = pending {
            let _ = pending.outcome.send(outcome);
        }
    }

    fn runtime_unavailable(&self, error: String) {
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.unavailable.is_some() {
                return;
            }
            state.unavailable = Some(error.clone());
        }
        self.finish_prompt(Err(AcpError::internal_error().data(error.clone())));
        self.interrupt_mode(None, AcpError::internal_error().data(error.clone()));
        let requests = std::mem::take(
            &mut self
                .state
                .lock()
                .expect("ACP session state poisoned")
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

    fn cancel_elicitation(&self, key: &str) {
        if let Some(token) = self
            .elicitations
            .lock()
            .expect("ACP elicitation lock poisoned")
            .remove(key)
        {
            token.cancel();
        }
    }

    fn cancel_elicitations(&self) {
        let tokens = self
            .elicitations
            .lock()
            .expect("ACP elicitation lock poisoned")
            .drain()
            .map(|(_, token)| token)
            .collect::<Vec<_>>();
        for token in tokens {
            token.cancel();
        }
    }
}

async fn await_elicitation(
    request: SentRequest<CreateElicitationResponse>,
    cancellation: CancellationToken,
) -> Result<CreateElicitationResponse, AcpError> {
    let mut response = Box::pin(request.block_task());
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(AcpError::request_cancelled()),
        response = &mut response => response,
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
    max_attempts: usize,
    retry_after: std::time::Duration,
    error: &str,
) -> AcpSessionUpdate {
    let delay = if retry_after.is_zero() {
        String::new()
    } else {
        let seconds = retry_after
            .as_secs()
            .saturating_add(u64::from(retry_after.subsec_nanos() > 0));
        format!(" (next attempt in {seconds}s)")
    };
    diagnostic_update(
        Some(turn_id),
        "retry",
        format!("Provider retry {attempt}/{max_attempts}{delay}: {error}"),
    )
}

fn invalid_params(message: impl Into<String>) -> AcpError {
    AcpError::invalid_params().data(message.into())
}
