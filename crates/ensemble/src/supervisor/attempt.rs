//! One adapter attempt and its callback-scoped state.

use super::bootstrap::{
    SafeModeBootstrap, SessionBootstrapGate, SessionConfigurationExpectation, SessionSetup,
    bind_session_gate, client_capabilities, new_session_request,
};
use super::diagnostics::RawDiagnosticSender;
use super::elicitation_request::{
    ElicitationHandlerContext, WorkerElicitationCleanup, handle_elicitation_request,
};
use super::failure::{acp_error, is_unexpected_process_termination};
use super::handoff::ClaudePlanHandoff;
use super::interactive::{InteractiveAttempt, optional_timeout};
use super::logging::RunLog;
use super::normalization::handle_notification;
use super::permission::{PermissionContext, WorkflowConstraintPolicy, handle_permission_request};
use super::prompt::{worker_attempt_prompt, worker_session_meta};
use super::recovery::{
    PromptRunContext, TransientRecoveryState, apply_workflow_config_options, enforce_safe_mode,
    recover_session, run_prompt_sequence,
};
use super::{EnsembleSupervisor, interactive};
use crate::config::EnsembleAgentConfig;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, Error as AcpError, Implementation, InitializeRequest,
    RequestPermissionRequest, SessionNotification,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client};
use futures_util::future::BoxFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use zevria_foundation::QuestionRequestId;
use zevria_session_api::TurnContext;
use zevria_workflow::{
    AgentRunDescriptor, AgentRunEvent, AgentRunRepair, AgentRunStatus, EnsembleWorkflow,
};

#[derive(Debug, Clone)]
pub(super) enum WorkerEnd {
    NativeHandoffCompleted,
    NativeHandoffFailed(String),
    PromptResponse { stop_reason: &'static str },
    TimedOut,
    LocalCancelled,
    Interrupted(String),
}

impl WorkerEnd {
    pub(super) fn is_semantic_prompt_end(&self) -> bool {
        matches!(
            self,
            Self::NativeHandoffCompleted
                | Self::NativeHandoffFailed(_)
                | Self::PromptResponse { .. }
                | Self::TimedOut
                | Self::LocalCancelled
        )
    }
}

#[derive(Debug, Clone)]
pub(super) enum WorkerAttemptMode {
    Fresh,
    DurableRecovery {
        session_id: Option<String>,
        repair: Option<AgentRunRepair>,
    },
    LiveProcessRecovery {
        session_id: String,
    },
}

impl WorkerAttemptMode {
    fn recovery_session_id(&self) -> Option<String> {
        match self {
            Self::Fresh => None,
            Self::DurableRecovery { session_id, .. } => session_id.clone(),
            Self::LiveProcessRecovery { session_id } => Some(session_id.clone()),
        }
    }

    fn is_recovery(&self) -> bool {
        !matches!(self, Self::Fresh)
    }
}

pub(super) struct WorkerAttemptContext<'a> {
    pub(super) agent_config: &'a EnsembleAgentConfig,
    pub(super) descriptor: &'a AgentRunDescriptor,
    pub(super) workflow: EnsembleWorkflow,
    pub(super) base_prompt: &'a zevria_content::UserPrompt,
    pub(super) mode: WorkerAttemptMode,
    pub(super) log: &'a RunLog,
    pub(super) cancellation: &'a tokio_util::sync::CancellationToken,
    pub(super) plan_handoff: Option<&'a ClaudePlanHandoff>,
    pub(super) transient_recovery: &'a Mutex<TransientRecoveryState>,
    pub(super) turn: &'a TurnContext,
    pub(super) diagnostics: RawDiagnosticSender,
    pub(super) interactive: Option<InteractiveAttempt<'a>>,
}

/// Callback and connection state for one adapter attempt. A failed attempt
/// cannot keep a later session's gate, question, or interaction count alive.
pub(super) struct AttemptShared {
    pub(super) log: RunLog,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) lifetime: tokio_util::sync::CancellationToken,
    pub(super) plan_handoff: Option<ClaudePlanHandoff>,
    pub(super) source_label: String,
    pub(super) turn: TurnContext,
    pub(super) violation: Mutex<Option<String>>,
    pub(super) session_gate: Mutex<SessionBootstrapGate>,
    pub(super) configuration_expectation: Mutex<Option<SessionConfigurationExpectation>>,
    elicitation_ready: AtomicBool,
    pub(super) active_question: Mutex<Option<QuestionRequestId>>,
    progress: WorkerAttemptProgress,
    interactions: InteractionCounter,
}

impl AttemptShared {
    fn interaction_tokens(
        &self,
        interactive: Option<&interactive::InteractiveController>,
    ) -> (
        tokio_util::sync::CancellationToken,
        tokio_util::sync::CancellationToken,
    ) {
        interactive.map_or_else(
            || (self.cancellation.clone(), self.lifetime.clone()),
            |controller| controller.interaction_tokens(&self.cancellation, &self.lifetime),
        )
    }
}

#[derive(Default)]
pub(super) struct InteractionCounter {
    active: AtomicU64,
    drained: tokio::sync::Notify,
}

impl InteractionCounter {
    pub(super) async fn wait_drained(&self) {
        loop {
            // `notify_waiters` covers futures created before the notification,
            // even if they have not been polled yet.
            let drained = self.drained.notified();
            if self.active.load(AtomicOrdering::Acquire) == 0 {
                return;
            }
            drained.await;
        }
    }
}

pub(super) struct PendingInteraction(Arc<AttemptShared>);
impl PendingInteraction {
    fn new(shared: Arc<AttemptShared>) -> Self {
        shared
            .interactions
            .active
            .fetch_add(1, AtomicOrdering::AcqRel);
        Self(shared)
    }
}
impl Drop for PendingInteraction {
    fn drop(&mut self) {
        if self
            .0
            .interactions
            .active
            .fetch_sub(1, AtomicOrdering::AcqRel)
            == 1
        {
            self.0.interactions.drained.notify_waiters();
        }
    }
}

#[derive(Default)]
pub(super) struct WorkerAttemptProgress {
    durable_session_id: Mutex<Option<String>>,
    pub(super) prompt_dispatched: AtomicBool,
    pub(super) semantic_end: Mutex<Option<WorkerEnd>>,
}

pub(super) struct WorkerAttemptResult {
    pub(super) connection_result: Option<Result<WorkerEnd, AcpError>>,
    pub(super) startup_timed_out: bool,
    pub(super) cancelled_during_startup: bool,
    pub(super) durable_session_id: Option<String>,
    pub(super) prompt_dispatched: bool,
    pub(super) semantic_end: Option<WorkerEnd>,
    pub(super) violation: Option<String>,
}

impl WorkerAttemptResult {
    pub(super) fn recoverable_process_error(&self) -> Option<&AcpError> {
        if self.startup_timed_out
            || self.cancelled_during_startup
            || self.durable_session_id.is_none()
            || !self.prompt_dispatched
            || self.semantic_end.is_some()
            || self.violation.is_some()
        {
            return None;
        }
        match self.connection_result.as_ref()? {
            Err(error) if is_unexpected_process_termination(error) => Some(error),
            Ok(_) | Err(_) => None,
        }
    }
}

impl EnsembleSupervisor {
    pub(super) async fn run_worker_attempt(
        &self,
        context: WorkerAttemptContext<'_>,
    ) -> WorkerAttemptResult {
        let WorkerAttemptContext {
            agent_config,
            descriptor,
            workflow,
            base_prompt,
            mode,
            log,
            cancellation,
            plan_handoff,
            transient_recovery,
            turn,
            diagnostics,
            interactive,
        } = context;
        let constraint_policy = WorkflowConstraintPolicy::for_workflow(workflow);
        let shared = Arc::new(AttemptShared {
            log: log.clone(),
            cancellation: cancellation.clone(),
            lifetime: tokio_util::sync::CancellationToken::new(),
            plan_handoff: plan_handoff.cloned(),
            source_label: descriptor.label.clone(),
            turn: turn.clone(),
            violation: Mutex::new(None),
            session_gate: Mutex::new(SessionBootstrapGate::default()),
            configuration_expectation: Mutex::new(None),
            elicitation_ready: AtomicBool::new(false),
            active_question: Mutex::new(None),
            progress: WorkerAttemptProgress::default(),
            interactions: InteractionCounter::default(),
        });
        let permission_attempt =
            plan_handoff.map(|handoff| handoff.start_attempt(shared.lifetime.clone()));
        let permission_attempt_id = permission_attempt.as_ref().map(|attempt| attempt.id);
        let elicitation_cleanup = WorkerElicitationCleanup {
            shared: shared.clone(),
            supervisor: self.inner.clone(),
        };

        #[cfg(not(windows))]
        let process_config = AcpAgentConfig::new(&agent_config.command)
            .args(agent_config.args.clone())
            .envs(agent_config.env.clone());
        // The pinned SDK owns only its direct Windows child. Killing this helper
        // closes its non-inherited Job handle and kills the complete agent tree.
        #[cfg(windows)]
        let process_config = AcpAgentConfig::new(&self.inner.acp_helper)
            .args(
                std::iter::once("--__acp-job".to_string())
                    .chain(std::iter::once(agent_config.command.clone()))
                    .chain(agent_config.args.clone()),
            )
            .envs(agent_config.env.clone());
        let process = AcpAgent::new(process_config)
            .with_debug(move |line, direction| diagnostics.record(line, direction));

        let notification_shared = shared.clone();
        let permission_shared = shared.clone();
        let elicitation_shared = shared.clone();
        let permission_interactive = interactive.as_ref().map(|input| input.controller.clone());
        let elicitation_interactive = permission_interactive.clone();
        let permission_grace = Duration::from_secs(self.inner.config.cancel_grace_seconds);
        let supervisor = self.inner.clone();
        let builder = Client
            .builder()
            .name(format!("zevria-{}", descriptor.agent))
            .on_receive_notification(
                async move |notification: SessionNotification, connection| {
                    handle_notification(notification, &connection, &notification_shared).await
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, connection| {
                    let interaction = PendingInteraction::new(permission_shared.clone());
                    let peer_cancellation = responder.cancellation();
                    let (cancellation, lifetime) =
                        permission_shared.interaction_tokens(permission_interactive.as_deref());
                    handle_permission_request(
                        request,
                        responder,
                        &connection,
                        PermissionContext {
                            shared: &permission_shared,
                            cancellation: &cancellation,
                            constraint_policy,
                            attempt_id: permission_attempt_id,
                            worker_lifetime: &lifetime,
                            peer_cancellation,
                            cancel_grace: permission_grace,
                            interaction,
                        },
                    )
                    .await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CreateElicitationRequest, responder, connection| {
                    let (cancellation, worker_lifetime) =
                        elicitation_shared.interaction_tokens(elicitation_interactive.as_deref());
                    let context = ElicitationHandlerContext {
                        supervisor: supervisor.clone(),
                        shared: elicitation_shared.clone(),
                        cancellation,
                        worker_lifetime,
                        session_ready: elicitation_shared
                            .elicitation_ready
                            .load(AtomicOrdering::Acquire),
                    };
                    let interaction = PendingInteraction::new(elicitation_shared.clone());
                    connection.spawn(async move {
                        let _interaction = interaction;
                        handle_elicitation_request(request, responder, context).await
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

        let startup_timeout = (workflow == EnsembleWorkflow::Review)
            .then(|| Duration::from_secs(self.inner.config.review_startup_timeout_seconds));
        let turn_timeout = (workflow == EnsembleWorkflow::Review)
            .then(|| Duration::from_secs(self.inner.config.review_turn_timeout_seconds));
        let cancel_grace = Duration::from_secs(self.inner.config.cancel_grace_seconds);
        let workspace = self.inner.workspace.clone();
        let safe_mode = descriptor.safe_mode.clone();
        let session_meta =
            worker_session_meta(agent_config.review_system_prompt_transport, workflow);
        let workflow_config_options = agent_config.config_options_for(workflow).clone();
        let recovery_session_id = mode.recovery_session_id();
        let recovering = mode.is_recovery();
        let consumed_repair = log.repair();
        let attempt_prompt =
            worker_attempt_prompt(&mode, workflow, base_prompt, consumed_repair.as_ref());
        let connected = shared.clone();
        let (started_tx, started_rx) = oneshot::channel();
        let connection = Box::pin(builder.connect_with(process, async move |connection| {
            let initialize = InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(client_capabilities(workflow))
                .client_info(Implementation::new("zevria", env!("CARGO_PKG_VERSION")));
            let initialized = connection.send_request(initialize).block_task().await?;
            if initialized.protocol_version != ProtocolVersion::V1 {
                return Err(acp_error(format!(
                    "agent negotiated unsupported ACP protocol {}",
                    initialized.protocol_version
                )));
            }
            let capabilities = initialized.agent_capabilities.clone();
            if interactive.is_none() && attempt_prompt.text.has_images() && !capabilities.prompt_capabilities.image {
                return Err(acp_error("ACP worker does not advertise image prompt support; the image request was not downgraded"));
            }
            let capabilities_json =
                serde_json::to_value(&capabilities).map_err(AcpError::into_internal_error)?;

            let setup = if recovering {
                recover_session(
                    &connection,
                    recovery_session_id,
                    &workspace,
                    &capabilities,
                    &connected.log,
                    &connected.session_gate,
                    &session_meta,
                )
                .await?
            } else {
                let response = connection
                    .send_request(new_session_request(&workspace, &session_meta))
                    .block_task()
                    .await?;
                let session_id = response.session_id;
                if interactive.is_some() {
                    connected.log
                        .emit(AgentRunEvent::SessionAllocated {
                            session_id: session_id.to_string(),
                        })
                        .await
                        .map_err(acp_error)?;
                    *connected.progress
                        .durable_session_id
                        .lock()
                        .expect("worker progress poisoned") = Some(session_id.to_string());
                }
                bind_session_gate(&connected.session_gate, &session_id).map_err(acp_error)?;
                SessionSetup {
                    session_id,
                    modes: response.modes,
                    config_options: response.config_options,
                    recovered: false,
                    interrupted: None,
                }
            };
            if let Some(error) = setup.interrupted {
                let _ = started_tx.send(());
                return Ok(WorkerEnd::Interrupted(error));
            }
            let SafeModeBootstrap {
                expectation: safe_mode_expectation,
                config_options,
            } = enforce_safe_mode(
                &connection,
                &setup.session_id,
                &safe_mode,
                setup.config_options,
                setup.modes,
            )
            .await?;
            apply_workflow_config_options(
                &connection,
                &setup.session_id,
                workflow,
                &workflow_config_options,
                &safe_mode_expectation,
                config_options,
            )
            .await?;
            *connected.configuration_expectation.lock().expect("ACP configuration poisoned") =
                Some(SessionConfigurationExpectation {
                    safe_mode: safe_mode_expectation,
                    workflow,
                    workflow_options: workflow_config_options,
                });
            let durable_session_id = setup.session_id.to_string();
            connected.log
                .emit(AgentRunEvent::SessionEstablished {
                    session_id: durable_session_id.clone(),
                    capabilities: capabilities_json,
                    safe_mode: safe_mode.clone(),
                    recovered: setup.recovered,
                })
                .await
                .map_err(acp_error)?;
            *connected.progress
                .durable_session_id
                .lock()
                .expect("worker attempt progress lock poisoned") = Some(durable_session_id);
            if let Some(InteractiveAttempt { controller, queue }) = interactive {
                connected.elicitation_ready.store(true, AtomicOrdering::Release);
                let _ = started_tx.send(());
                return controller
                    .run(
                        queue,
                        &connection,
                        setup.session_id,
                        &connected.log,
                        &connected.cancellation,
                        &connected.lifetime,
                        connected.plan_handoff.as_ref(),
                        cancel_grace,
                        &connected.interactions,
                        &connected.progress,
                        &connected.violation,
                        capabilities.prompt_capabilities.image,
                    )
                    .await;
            }
            connected.log
                .emit(AgentRunEvent::Status {
                    status: AgentRunStatus::Running,
                    detail: None,
                })
                .await
                .map_err(acp_error)?;
            connected.log
                .emit(AgentRunEvent::Prompt {
                    text: attempt_prompt.text.display_projection(),
                    continuation: attempt_prompt.continuation,
                    repair: attempt_prompt.repair.clone(),
                })
                .await
                .map_err(acp_error)?;
            connected.elicitation_ready.store(true, AtomicOrdering::Release);
            let _ = started_tx.send(());
            let result = run_prompt_sequence(
                &connection,
                setup.session_id,
                attempt_prompt.text,
                workflow,
                &connected.log,
                PromptRunContext {
                    cancellation: &connected.cancellation,
                    elicitation_lifetime: &connected.lifetime,
                    plan_handoff: connected.plan_handoff.as_ref(),
                    prompt_dispatched: &connected.progress.prompt_dispatched,
                    semantic_end: &connected.progress.semantic_end,
                    transient_recovery,
                    turn_timeout,
                    cancel_grace,
                    keep_alive: false,
                },
            )
            .await;
            if let Ok(end) = &result
                && end.is_semantic_prompt_end()
            {
                *connected.progress
                    .semantic_end
                    .lock()
                    .expect("worker attempt progress lock poisoned") = Some(end.clone());
            }
            connected.lifetime.cancel();
            result
        }));
        // Erase the concrete ACP connection future before it enters `select!` so its
        // callback types do not overflow rustc's auto-trait recursion while proving `Send`.
        let mut connection: BoxFuture<'_, Result<WorkerEnd, AcpError>> = connection;
        let connection_result = tokio::select! {
            biased;
            result = connection.as_mut() => Some(result),
            () = cancellation.cancelled() => None,
            result = optional_timeout(startup_timeout, started_rx) => {
                match result {
                    Ok(Ok(())) => Some(connection.as_mut().await),
                    Ok(Err(_)) => Some(connection.as_mut().await),
                    Err(_) => None,
                }
            }
        };
        let startup_timed_out = connection_result.is_none() && !cancellation.is_cancelled();
        let cancelled_during_startup = connection_result.is_none() && cancellation.is_cancelled();
        drop(elicitation_cleanup);
        drop(permission_attempt);
        drop(connection);

        let progress = &shared.progress;
        let mut semantic_end = progress
            .semantic_end
            .lock()
            .expect("worker attempt progress lock poisoned")
            .clone();
        if semantic_end.is_none() && plan_handoff.is_some_and(ClaudePlanHandoff::is_completed) {
            semantic_end = Some(WorkerEnd::NativeHandoffCompleted);
        }
        WorkerAttemptResult {
            connection_result,
            startup_timed_out,
            cancelled_during_startup,
            durable_session_id: progress
                .durable_session_id
                .lock()
                .expect("worker attempt progress lock poisoned")
                .clone(),
            prompt_dispatched: progress.prompt_dispatched.load(AtomicOrdering::Acquire),
            semantic_end,
            violation: shared
                .violation
                .lock()
                .expect("ACP violation lock poisoned")
                .take(),
        }
    }
}
