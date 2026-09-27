//! ACP subprocess supervision for independent ensemble workers.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt::Display,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, LineDirection,
    is_incoming_transport_closed,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, CancelNotification, ClientCapabilities, ClientSessionCapabilities,
            ConfigOptionUpdate, ContentBlock, ContentChunk, CreateElicitationRequest,
            CreateElicitationResponse, CurrentModeUpdate, ElicitationAcceptAction,
            ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
            ElicitationFormCapabilities, ElicitationMode, ElicitationPropertySchema,
            ElicitationScope, Error as AcpError, ErrorCode, Implementation, InitializeRequest,
            LoadSessionRequest, Meta, MultiSelectItems, NewSessionRequest, PermissionOption,
            PermissionOptionKind, Plan, PlanCapabilities, PlanUpdateContent, PromptRequest,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigKind,
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionsCapabilities,
            SessionConfigSelectOptions, SessionId, SessionModeState, SessionNotification,
            SessionUpdate as AcpSessionUpdate, SetSessionConfigOptionRequest,
            SetSessionModeRequest, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
            ToolKind,
        },
    },
};
use anyhow::Context as _;
use futures_util::{StreamExt as _, future::BoxFuture, stream};
use tokio::sync::{mpsc, oneshot, watch};
use zevria_foundation::QuestionAnswerValue;
use zevria_foundation::QuestionOption;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequest;
use zevria_foundation::QuestionRequestId;
use zevria_foundation::QuestionResponse;
use zevria_session_api::EnsembleLaunchFuture;
use zevria_session_api::EnsembleLaunchRequest;
use zevria_session_api::EnsembleLauncher;
use zevria_session_api::QuestionRequester;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionEventSender;
use zevria_session_api::TurnContext;
use zevria_transcript::AGENT_RUN_TRANSCRIPT_VERSION;
use zevria_transcript::AgentRunProjection;
use zevria_transcript::AgentRunTranscriptHeader;
use zevria_transcript::AgentRunTranscriptRecord;
use zevria_transcript::AgentRunTranscriptWriter;
use zevria_transcript::agent_run_path;
use zevria_transcript::load_agent_run_projection;
use zevria_workflow::AgentElicitationOutcome;
use zevria_workflow::AgentPlanEntry;
use zevria_workflow::AgentProtocolDirection;
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentRunEvent;
use zevria_workflow::AgentRunId;
use zevria_workflow::AgentRunLocation;
use zevria_workflow::AgentRunOutcome;
use zevria_workflow::AgentRunRepair;
use zevria_workflow::AgentRunStatus;
use zevria_workflow::AgentStructuredPlan;
use zevria_workflow::AgentUnavailableDecision;
use zevria_workflow::AgentUsage;
use zevria_workflow::AgentUserDecisionAnswer;
use zevria_workflow::AgentUserDecisionBatch;
use zevria_workflow::AgentUserDecisionId;
use zevria_workflow::AgentUserDecisionValue;
use zevria_workflow::CLAUDE_PLAN_HANDOFF_PLAN_ID;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::MAX_NORMALIZED_USER_DECISION_BYTES;
use zevria_workflow::reduce_agent_plan_event;
use zevria_workflow::validate_worker_synthesis_payload;

#[path = "ensemble_interactive.rs"]
mod interactive;
use interactive::{InteractiveConnection, optional_timeout};
use zevria_session_api::worker::*;
use zevria_workflow::ensemble_review::*;

use crate::config::{
    EnsembleAgentConfig, EnsembleConfig, PlanHandoffTransport, ReviewSystemPromptTransport,
};

const REVIEW_WORKER_INSTRUCTION: &str = "Act as an independent review worker. Perform a source-read-only review of the requested changes and return actionable findings directly, prioritized by severity with precise locations. Inspect relevant repository evidence independently. Protect non-scratch files: leave the original workspace and other non-scratch state unchanged. Task-relevant external reads, downloads, and private OS-temp scratch-contained execution are allowed only if your own active policy permits them; this envelope grants no additional capabilities or ACP mutation permissions. Your own policies and sandboxes remain authoritative. Scratch findings are not completed implementation, publication, confirmation, or approval.";
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const RUN_LOG_QUEUE_CAPACITY: usize = 1_024;
const RUN_LOG_BATCH_BYTES: usize = 64 * 1_024;
const RUN_LOG_BATCH_WINDOW: Duration = Duration::from_millis(25);
const DEBUG_LOG_QUEUE_CAPACITY: usize = 64;
const RAW_DIAGNOSTIC_MAX_LINE_BYTES: usize = 256 * 1_024;
const RAW_DIAGNOSTIC_MAX_RUN_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS: usize = 1;
const LIVE_PROCESS_CONTINUATION_PROMPT: &str = "continue";
const MAX_LIVE_TRANSIENT_CONTINUATIONS: usize = 1;
const TRANSIENT_CONTINUATION_BACKOFF: Duration = Duration::from_secs(2);
const ACP_PROCESS_EXIT_DATA_PREFIX: &str = "Process exited with ";

#[cfg(windows)]
fn native_acp_helper() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot resolve the native ACP job helper")?;
    // A Rust libtest executable does not dispatch Zevria's private helper flag.
    // Workspace integration tests build the real binary alongside debug/deps.
    #[cfg(test)]
    let executable = std::env::var_os("ZEVRIA_TEST_ACP_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            executable
                .parent()
                .expect("test executable parent")
                .parent()
                .expect("Cargo target profile directory")
                .join("zevria.exe")
        });
    anyhow::ensure!(
        executable.is_file(),
        "native ACP job helper is missing at {}; on Windows build Zevria before package-local ensemble tests (cargo build --locked -p zevria), or set ZEVRIA_TEST_ACP_HELPER",
        executable.display()
    );
    Ok(executable)
}

#[derive(Clone)]
pub struct EnsembleSupervisor {
    config: EnsembleConfig,
    #[cfg(windows)]
    acp_helper: PathBuf,
    workspace: PathBuf,
    agent_runs_root: PathBuf,
    questions: QuestionRequester,
    question_gate: Arc<tokio::sync::Mutex<()>>,
}

impl EnsembleSupervisor {
    pub fn new(
        config: EnsembleConfig,
        workspace: &Path,
        agent_runs_root: PathBuf,
        questions: QuestionRequester,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        #[cfg(windows)]
        let workspace = zevria_foundation::windows_io::checked_directory_path(workspace)
            .context("unsupported protected Windows ensemble workspace")?;
        #[cfg(not(windows))]
        let workspace = std::fs::canonicalize(workspace).with_context(|| {
            format!(
                "failed to resolve the absolute ensemble workspace at {}",
                workspace.display()
            )
        })?;
        ensure_agent_run_ignore_guard(&workspace)?;
        Ok(Self {
            config,
            #[cfg(windows)]
            acp_helper: native_acp_helper()?,
            workspace,
            agent_runs_root,
            questions,
            question_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    fn selected_agents(&self, workflow: EnsembleWorkflow) -> &[String] {
        match workflow {
            EnsembleWorkflow::Plan => &self.config.plan_agents,
            EnsembleWorkflow::Review => &self.config.review_agents,
        }
    }

    async fn launch_all(
        &self,
        request: EnsembleLaunchRequest,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<Vec<AgentRunOutcome>> {
        if request.start.workflow == EnsembleWorkflow::Plan {
            anyhow::bail!(
                "Plan workers require the interactive review execution handle; prompt completion is not user confirmation"
            );
        }
        preflight_existing_logs(&self.agent_runs_root, &request.start)?;
        let max_concurrent = self.config.max_concurrent_agents;
        let mut outcomes = stream::iter(request.start.agents.clone().into_iter().enumerate())
            .map(|(index, descriptor)| {
                let supervisor = self.clone();
                let start = request.start.clone();
                let events = events.clone();
                let turn = turn.clone();
                async move {
                    let outcome = supervisor
                        .run_worker(start, descriptor, request.resume, events, turn)
                        .await;
                    (index, outcome)
                }
            })
            .buffer_unordered(max_concurrent)
            .collect::<Vec<_>>()
            .await;
        outcomes.sort_by_key(|(index, _)| *index);
        Ok(outcomes.into_iter().map(|(_, outcome)| outcome).collect())
    }

    async fn run_worker(
        &self,
        start: zevria_workflow::EnsembleStart,
        descriptor: AgentRunDescriptor,
        resume: bool,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> AgentRunOutcome {
        let path = agent_run_path(&self.agent_runs_root, &start.run_id, &descriptor.id);
        let (log, previous) = match RunLog::open(
            path.clone(),
            &start,
            &descriptor,
            resume,
            events,
            turn.clone(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return failed_outcome(descriptor, AgentRunStatus::Interrupted, error.to_string());
            }
        };
        let mut recovery_repair = None;
        let mut durable_recovery_session_id = previous.acp_session_id.clone();
        if resume && let Some(outcome) = previous.recoverable_outcome() {
            if durable_recovery_session_id.is_none() {
                durable_recovery_session_id.clone_from(&outcome.acp_session_id);
            }
            // A root crash can happen after every worker durably finished but
            // before the root ReportsReady was committed.
            // Review remains evidence-tolerant. Plan completion is terminal
            // only when its durable final Markdown proof is present.
            if start.workflow == EnsembleWorkflow::Review || outcome.has_plan_proof() {
                return outcome;
            }
            if let Some(repair) = &previous.repair {
                return self
                    .finish_worker(
                        &log,
                        start.workflow,
                        descriptor,
                        AgentRunStatus::Failed,
                        true,
                        Some(format!(
                            "Plan worker recovery found no final Markdown proof after its one semantic repair was already consumed ({repair:?})"
                        )),
                    )
                    .await;
            }
            if durable_recovery_session_id.is_none() {
                return self
                    .finish_worker(
                        &log,
                        start.workflow,
                        descriptor,
                        AgentRunStatus::Failed,
                        true,
                        Some(
                            "Plan worker recovery found no final Markdown proof and no ACP session ID is available for the one repair attempt"
                                .to_string(),
                        ),
                    )
                    .await;
            }
            recovery_repair = Some(AgentRunRepair::MissingPlanProof);
        }
        let Some(agent_config) = self.config.agents.get(&descriptor.agent).cloned() else {
            let agent = descriptor.agent.clone();
            return self
                .finish_worker(
                    &log,
                    start.workflow,
                    descriptor,
                    AgentRunStatus::Failed,
                    true,
                    Some(format!(
                        "configured agent {:?} is no longer available for recovery",
                        agent
                    )),
                )
                .await;
        };
        let initial_status = if resume {
            AgentRunStatus::Resuming
        } else {
            AgentRunStatus::Starting
        };
        if let Err(error) = log
            .emit(AgentRunEvent::Status {
                status: initial_status,
                detail: None,
            })
            .await
        {
            return failed_outcome(descriptor, AgentRunStatus::Failed, error.to_string());
        }
        if turn.is_cancelled() {
            return self
                .finish_worker(
                    &log,
                    start.workflow,
                    descriptor,
                    AgentRunStatus::Cancelled,
                    true,
                    Some("ensemble turn cancelled before the worker started".to_string()),
                )
                .await;
        }

        // Root cancellation propagates into every child token, while a mode
        // or permission violation cancels only this worker.
        let cancellation = turn.cancellation().child_token();
        let plan_handoff = if start.workflow == EnsembleWorkflow::Plan
            && agent_config.plan_handoff_transport
                == Some(PlanHandoffTransport::ClaudeCodeExitPlanMode)
        {
            match ClaudePlanHandoff::new(&agent_config, &self.workspace) {
                Ok(handoff) => Some(handoff),
                Err(error) => {
                    return self
                        .finish_worker(
                            &log,
                            start.workflow,
                            descriptor,
                            AgentRunStatus::Failed,
                            true,
                            Some(format!(
                                "cannot enforce configured Claude plan handoff: {error:#}"
                            )),
                        )
                        .await;
                }
            }
        } else {
            None
        };
        let base_prompt = worker_prompt(start.workflow, &start.prompt, plan_handoff.is_some());
        let diagnostics = RawDiagnosticSink::spawn(&log);
        let mut attempt_mode = if resume {
            WorkerAttemptMode::DurableRecovery {
                session_id: durable_recovery_session_id,
                repair: recovery_repair,
            }
        } else {
            WorkerAttemptMode::Fresh
        };
        let mut recovery_attempts = 0usize;
        let mut recovery_context = None;
        // Live logical-run state, deliberately outside the adapter-attempt loop.
        let transient_recovery = Arc::new(Mutex::new(TransientRecoveryState::default()));
        let final_attempt = loop {
            let attempt = self
                .run_worker_attempt(WorkerAttemptContext {
                    agent_config: &agent_config,
                    descriptor: &descriptor,
                    workflow: start.workflow,
                    base_prompt: &base_prompt,
                    mode: attempt_mode,
                    log: &log,
                    cancellation: &cancellation,
                    plan_handoff: plan_handoff.as_ref(),
                    transient_recovery: &transient_recovery,
                    turn: &turn,
                    diagnostics: diagnostics.sender(),
                    interactive: None,
                })
                .await;
            diagnostics.barrier().await;
            if let Err(error) = log.barrier().await {
                log.set_failure(error.to_string());
            }
            let recovery = if recovery_attempts < MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS
                && log.failure().is_none()
                && !turn.is_cancelled()
                && !cancellation.is_cancelled()
            {
                attempt.recoverable_process_error().and_then(|error| {
                    attempt
                        .durable_session_id
                        .clone()
                        .map(|session_id| (session_id, concise_process_failure(error)))
                })
            } else {
                None
            };
            let Some((session_id, process_failure)) = recovery else {
                break attempt;
            };

            let attempt_number = recovery_attempts + 1;
            if let Err(error) = log
                .emit(AgentRunEvent::Status {
                    status: AgentRunStatus::Resuming,
                    detail: Some(format!(
                        "worker process terminated unexpectedly ({process_failure}); relaunching the ACP adapter (automatic recovery attempt {attempt_number} of {MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS})"
                    )),
                })
                .await
            {
                log.set_failure(error.to_string());
                break attempt;
            }
            recovery_context = Some(format!(
                "automatic process recovery attempt {attempt_number} of {MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS} followed an unexpected process termination: {process_failure}"
            ));
            recovery_attempts = attempt_number;
            attempt_mode = WorkerAttemptMode::LiveProcessRecovery { session_id };
        };
        diagnostics.finish().await;

        let WorkerAttemptResult {
            connection_result,
            startup_timed_out,
            cancelled_during_startup,
            semantic_end,
            violation,
            ..
        } = final_attempt;
        let log_failure = log.failure();
        // Unfinished artifacts are semantic validation, not a policy violation.
        // They must never hide an ACP error or change cancellation/timeout status.
        let unresolved = plan_handoff
            .as_ref()
            .and_then(ClaudePlanHandoff::unresolved_violation);
        let semantic_end_observed = semantic_end.is_some();
        let connection_result = semantic_end.map(Ok).or(connection_result);
        let (status, partial, failure) = if let Some(error) = log_failure {
            (AgentRunStatus::Failed, true, Some(error))
        } else if turn.is_cancelled() {
            (
                AgentRunStatus::Cancelled,
                true,
                Some("ensemble turn cancelled by the user".to_string()),
            )
        } else if let Some(error) = violation {
            (AgentRunStatus::Failed, true, Some(error))
        } else if startup_timed_out {
            (
                AgentRunStatus::TimedOut,
                true,
                Some(format!(
                    "worker startup timed out after {} seconds",
                    self.config.review_startup_timeout_seconds
                )),
            )
        } else if !semantic_end_observed
            && (cancelled_during_startup || cancellation.is_cancelled())
        {
            (
                AgentRunStatus::Cancelled,
                true,
                Some(
                    if connection_result.is_none() {
                        "ensemble turn cancelled during worker startup"
                    } else {
                        "worker was cancelled locally"
                    }
                    .to_string(),
                ),
            )
        } else {
            match connection_result.expect("classified absent connection result") {
                Ok(WorkerEnd::NativeHandoffFailed(error)) => (AgentRunStatus::Failed, true, Some(error)),
                Ok(WorkerEnd::NativeHandoffCompleted) if log.has_plan_proof() => {
                    (AgentRunStatus::Completed, false, None)
                }
                Ok(WorkerEnd::NativeHandoffCompleted) => (
                    AgentRunStatus::Failed,
                    true,
                    Some(
                        "validated native handoff completed without a persisted nonempty Markdown plan"
                            .to_string(),
                    ),
                ),
                Ok(WorkerEnd::PromptResponse { .. }) if unresolved.is_some() => {
                    (AgentRunStatus::Failed, true, unresolved.clone())
                }
                Ok(WorkerEnd::PromptResponse { stop_reason })
                    if stop_reason == "end_turn"
                        && (start.workflow == EnsembleWorkflow::Review
                            || log.has_plan_proof()) =>
                {
                    (AgentRunStatus::Completed, false, None)
                }
                Ok(WorkerEnd::PromptResponse { stop_reason })
                    if stop_reason == "end_turn" =>
                {
                    (
                        AgentRunStatus::Failed,
                        true,
                        Some(format!(
                            "Plan worker ended with stop reason {stop_reason:?} without a persisted nonempty inline Markdown plan after the one semantic repair opportunity"
                        )),
                    )
                }
                Ok(WorkerEnd::PromptResponse { stop_reason }) => (
                    AgentRunStatus::Failed,
                    true,
                    Some(if stop_reason == "cancelled" {
                        format!(
                            "ACP prompt ended with stop reason {stop_reason:?}; the cancellation was not a root cancellation and did not qualify for another semantic repair"
                        )
                    } else {
                        format!(
                            "ACP prompt ended with stop reason {stop_reason:?} after the one semantic repair opportunity"
                        )
                    }),
                ),
                Ok(WorkerEnd::TimedOut) => (
                    AgentRunStatus::TimedOut,
                    true,
                    Some(format!(
                        "worker turn timed out after {} seconds",
                        self.config.review_turn_timeout_seconds
                    )),
                ),
                Ok(WorkerEnd::LocalCancelled) => (
                    AgentRunStatus::Cancelled,
                    true,
                    Some("ensemble turn cancelled by the user".to_string()),
                ),
                Ok(WorkerEnd::Interrupted(error)) if log.repair().is_some() => {
                    (AgentRunStatus::Failed, true, Some(error))
                }
                Ok(WorkerEnd::Interrupted(error)) => {
                    (AgentRunStatus::Interrupted, true, Some(error))
                }
                Err(error) => (
                    AgentRunStatus::Failed,
                    true,
                    Some(error_with_login_hint(&error, &agent_config.login_hint)),
                ),
            }
        };
        let failure_context = [
            unresolved,
            transient_recovery
                .lock()
                .expect("transient recovery lock poisoned")
                .failure_context(&agent_config.login_hint),
            recovery_context,
            plan_handoff
                .as_ref()
                .and_then(ClaudePlanHandoff::abandoned_preparations),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        self.finish_worker_with_context(
            &log,
            start.workflow,
            descriptor,
            status,
            partial,
            failure,
            &failure_context,
        )
        .await
    }

    async fn run_worker_attempt(&self, context: WorkerAttemptContext<'_>) -> WorkerAttemptResult {
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
        let violation = Arc::new(Mutex::new(None::<String>));
        let session_gate = Arc::new(Mutex::new(SessionBootstrapGate::default()));
        let configuration_expectation =
            Arc::new(Mutex::new(None::<SessionConfigurationExpectation>));
        let elicitation_ready = Arc::new(AtomicBool::new(false));
        let elicitation_lifetime = tokio_util::sync::CancellationToken::new();
        let permission_attempt =
            plan_handoff.map(|handoff| handoff.start_attempt(elicitation_lifetime.clone()));
        let permission_attempt_id = permission_attempt.as_ref().map(|attempt| attempt.id);
        let active_question = Arc::new(Mutex::new(None::<QuestionRequestId>));
        let elicitation_cleanup = WorkerElicitationCleanup {
            lifetime: elicitation_lifetime.clone(),
            active_question: active_question.clone(),
            questions: self.questions.clone(),
        };
        let progress = Arc::new(WorkerAttemptProgress::default());
        let interactions = Arc::new(AtomicU64::new(0));
        let permission_interactions = interactions.clone();
        let elicitation_interactions = interactions.clone();

        #[cfg(not(windows))]
        let process_config = AcpAgentConfig::new(&agent_config.command)
            .args(agent_config.args.clone())
            .envs(agent_config.env.clone());
        // The pinned SDK owns only its direct Windows child. Killing this helper
        // closes its non-inherited Job handle and kills the complete agent tree.
        #[cfg(windows)]
        let process_config = AcpAgentConfig::new(&self.acp_helper)
            .args(
                std::iter::once("--__acp-job".to_string())
                    .chain(std::iter::once(agent_config.command.clone()))
                    .chain(agent_config.args.clone()),
            )
            .envs(agent_config.env.clone());
        let process = AcpAgent::new(process_config)
            .with_debug(move |line, direction| diagnostics.record(line, direction));

        let notification_log = log.clone();
        let notification_cancel = cancellation.clone();
        let notification_violation = violation.clone();
        let notification_session = session_gate.clone();
        let notification_configuration = configuration_expectation.clone();
        let notification_handoff = plan_handoff.cloned();
        let permission_log = log.clone();
        let permission_cancel = cancellation.clone();
        let permission_violation = violation.clone();
        let permission_session = session_gate.clone();
        let permission_handoff = plan_handoff.cloned();
        let permission_policy = constraint_policy;
        let permission_lifetime = elicitation_lifetime.clone();
        let elicitation_questions = self.questions.clone();
        let elicitation_gate = self.question_gate.clone();
        let elicitation_log = log.clone();
        let elicitation_cancel = cancellation.clone();
        let elicitation_violation = violation.clone();
        let elicitation_session = session_gate.clone();
        let elicitation_readiness = elicitation_ready.clone();
        let elicitation_worker_lifetime = elicitation_lifetime.clone();
        let elicitation_active_question = active_question.clone();
        let elicitation_label = descriptor.label.clone();
        let elicitation_turn = turn.clone();
        let permission_interactive = interactive.clone();
        let permission_grace = Duration::from_secs(self.config.cancel_grace_seconds);
        let elicitation_interactive = interactive.clone();
        let builder = Client
            .builder()
            .name(format!("zevria-{}", descriptor.agent))
            .on_receive_notification(
                async move |notification: SessionNotification, connection| {
                    handle_notification(
                        notification,
                        &connection,
                        NotificationContext {
                            log: &notification_log,
                            cancellation: &notification_cancel,
                            violation: &notification_violation,
                            session_gate: &notification_session,
                            configuration_expectation: &notification_configuration,
                            plan_handoff: notification_handoff.as_ref(),
                        },
                    )
                    .await
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, connection| {
                    let interaction =
                        interactive::PendingInteraction::new(permission_interactions.clone());
                    let peer_cancellation = responder.cancellation();
                    let (cancellation, lifetime) = permission_interactive.as_ref().map_or_else(
                        || (permission_cancel.clone(), permission_lifetime.clone()),
                        |interactive| {
                            interactive.interaction_tokens(&permission_cancel, &permission_lifetime)
                        },
                    );
                    handle_permission_request(
                        request,
                        responder,
                        &connection,
                        PermissionContext {
                            log: &permission_log,
                            cancellation: &cancellation,
                            violation: &permission_violation,
                            constraint_policy: permission_policy,
                            session_gate: &permission_session,
                            plan_handoff: permission_handoff.as_ref(),
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
                        elicitation_interactive.as_ref().map_or_else(
                            || {
                                (
                                    elicitation_cancel.clone(),
                                    elicitation_worker_lifetime.clone(),
                                )
                            },
                            |interactive| {
                                interactive.interaction_tokens(
                                    &elicitation_cancel,
                                    &elicitation_worker_lifetime,
                                )
                            },
                        );
                    let context = ElicitationHandlerContext {
                        questions: elicitation_questions.clone(),
                        gate: elicitation_gate.clone(),
                        log: elicitation_log.clone(),
                        cancellation,
                        worker_lifetime,
                        violation: elicitation_violation.clone(),
                        session_gate: elicitation_session.clone(),
                        session_ready: elicitation_readiness.load(AtomicOrdering::Acquire),
                        active_question: elicitation_active_question.clone(),
                        source_label: elicitation_label.clone(),
                        turn: elicitation_turn.clone(),
                    };
                    let interaction =
                        interactive::PendingInteraction::new(elicitation_interactions.clone());
                    connection.spawn(async move {
                        let _interaction = interaction;
                        handle_elicitation_request(request, responder, context).await
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            );

        let startup_timeout = (workflow == EnsembleWorkflow::Review)
            .then(|| Duration::from_secs(self.config.review_startup_timeout_seconds));
        let turn_timeout = (workflow == EnsembleWorkflow::Review)
            .then(|| Duration::from_secs(self.config.review_turn_timeout_seconds));
        let cancel_grace = Duration::from_secs(self.config.cancel_grace_seconds);
        let workspace = self.workspace.clone();
        let safe_mode = descriptor.safe_mode.clone();
        let session_meta =
            worker_session_meta(agent_config.review_system_prompt_transport, workflow);
        let workflow_config_options = agent_config.config_options_for(workflow).clone();
        let recovery_session_id = mode.recovery_session_id();
        let recovering = mode.is_recovery();
        let consumed_repair = log.repair();
        let attempt_prompt =
            worker_attempt_prompt(&mode, workflow, base_prompt, consumed_repair.as_ref());
        let connection_log = log.clone();
        let connection_cancel = cancellation.clone();
        let connection_session = session_gate.clone();
        let connection_configuration = configuration_expectation.clone();
        let connection_handoff = plan_handoff.cloned();
        let connection_elicitation_ready = elicitation_ready.clone();
        let connection_elicitation_lifetime = elicitation_lifetime.clone();
        let connection_progress = progress.clone();
        let connection_transient_recovery = transient_recovery.clone();
        let connection_violation = violation.clone();
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
                    &connection_log,
                    &connection_session,
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
                    connection_log
                        .emit(AgentRunEvent::SessionAllocated {
                            session_id: session_id.to_string(),
                        })
                        .await
                        .map_err(acp_error)?;
                    *connection_progress
                        .durable_session_id
                        .lock()
                        .expect("worker progress poisoned") = Some(session_id.to_string());
                }
                bind_session_gate(&connection_session, &session_id).map_err(acp_error)?;
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
            set_mutex(
                &connection_configuration,
                Some(SessionConfigurationExpectation {
                    safe_mode: safe_mode_expectation,
                    workflow,
                    workflow_options: workflow_config_options,
                }),
            );
            let durable_session_id = setup.session_id.to_string();
            connection_log
                .emit(AgentRunEvent::SessionEstablished {
                    session_id: durable_session_id.clone(),
                    capabilities: capabilities_json,
                    safe_mode: safe_mode.clone(),
                    recovered: setup.recovered,
                })
                .await
                .map_err(acp_error)?;
            *connection_progress
                .durable_session_id
                .lock()
                .expect("worker attempt progress lock poisoned") = Some(durable_session_id);
            if let Some(interactive) = interactive {
                connection_elicitation_ready.store(true, AtomicOrdering::Release);
                let _ = started_tx.send(());
                return interactive
                    .run(
                        &connection,
                        setup.session_id,
                        &connection_log,
                        &connection_cancel,
                        &connection_elicitation_lifetime,
                        connection_handoff.as_ref(),
                        cancel_grace,
                        &interactions,
                        &connection_progress,
                        &connection_violation,
                        capabilities.prompt_capabilities.image,
                    )
                    .await;
            }
            connection_log
                .emit(AgentRunEvent::Status {
                    status: AgentRunStatus::Running,
                    detail: None,
                })
                .await
                .map_err(acp_error)?;
            connection_log
                .emit(AgentRunEvent::Prompt {
                    text: attempt_prompt.text.display_projection(),
                    continuation: attempt_prompt.continuation,
                    repair: attempt_prompt.repair.clone(),
                })
                .await
                .map_err(acp_error)?;
            connection_elicitation_ready.store(true, AtomicOrdering::Release);
            let _ = started_tx.send(());
            let result = run_prompt_sequence(
                &connection,
                setup.session_id,
                attempt_prompt.text,
                workflow,
                &connection_log,
                PromptRunContext {
                    cancellation: &connection_cancel,
                    elicitation_lifetime: &connection_elicitation_lifetime,
                    plan_handoff: connection_handoff.as_ref(),
                    prompt_dispatched: &connection_progress.prompt_dispatched,
                    semantic_end: &connection_progress.semantic_end,
                    transient_recovery: &connection_transient_recovery,
                    turn_timeout,
                    cancel_grace,
                    keep_alive: false,
                },
            )
            .await;
            if let Ok(end) = &result
                && end.is_semantic_prompt_end()
            {
                *connection_progress
                    .semantic_end
                    .lock()
                    .expect("worker attempt progress lock poisoned") = Some(end.clone());
            }
            connection_elicitation_lifetime.cancel();
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
            violation: take_mutex(&violation),
        }
    }

    async fn finish_worker(
        &self,
        log: &RunLog,
        workflow: EnsembleWorkflow,
        descriptor: AgentRunDescriptor,
        status: AgentRunStatus,
        partial: bool,
        failure: Option<String>,
    ) -> AgentRunOutcome {
        self.finish_worker_with_context(log, workflow, descriptor, status, partial, failure, &[])
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_worker_with_context(
        &self,
        log: &RunLog,
        workflow: EnsembleWorkflow,
        descriptor: AgentRunDescriptor,
        mut status: AgentRunStatus,
        mut partial: bool,
        mut failure: Option<String>,
        failure_context: &[String],
    ) -> AgentRunOutcome {
        if let Err(error) = log.barrier().await {
            status = AgentRunStatus::Failed;
            partial = true;
            merge_failure(
                &mut failure,
                format!("failed to flush the durable worker report: {error:#}"),
            );
        }
        let path = log.path().to_path_buf();
        let projection =
            match tokio::task::spawn_blocking(move || load_agent_run_projection(&path)).await {
                Ok(Ok(projection)) => projection,
                Ok(Err(error)) => {
                    status = AgentRunStatus::Failed;
                    partial = true;
                    merge_failure(
                        &mut failure,
                        format!("failed to project the durable worker report: {error:#}"),
                    );
                    AgentRunProjection::default()
                }
                Err(error) => {
                    status = AgentRunStatus::Failed;
                    partial = true;
                    merge_failure(
                        &mut failure,
                        format!("durable worker projection task failed: {error}"),
                    );
                    AgentRunProjection::default()
                }
            };
        if status == AgentRunStatus::Completed && partial {
            status = AgentRunStatus::Failed;
            merge_failure(
                &mut failure,
                "internal worker classification attempted a partial Completed outcome".to_string(),
            );
        }
        let mut outcome = AgentRunOutcome {
            descriptor,
            status,
            report: projection.report,
            plan: projection.plan,
            confirmation: None,
            partial,
            failure,
            usage: projection.usage,
            acp_session_id: projection.acp_session_id,
            user_decisions: projection.user_decisions,
            decision_ids: projection.decision_ids,
            unavailable_decisions: projection.unavailable_decisions,
        };
        if outcome.status == AgentRunStatus::Completed
            && workflow == EnsembleWorkflow::Plan
            && let Err(error) = validate_worker_synthesis_payload(
                workflow,
                &outcome,
                self.config.max_synthesis_bytes_per_agent,
            )
        {
            outcome.status = AgentRunStatus::Failed;
            outcome.partial = true;
            merge_failure(
                &mut outcome.failure,
                format!("Plan worker final proof cannot enter synthesis: {error:#}"),
            );
        }
        if outcome.status != AgentRunStatus::Completed {
            for context in failure_context {
                merge_failure(&mut outcome.failure, context.clone());
            }
        }
        if let Err(error) = log.append_outcome(outcome.clone()).await {
            outcome.status = AgentRunStatus::Failed;
            outcome.partial = true;
            merge_failure(
                &mut outcome.failure,
                format!("failed to persist the worker outcome: {error:#}"),
            );
            for context in failure_context {
                merge_failure(&mut outcome.failure, context.clone());
            }
            return outcome;
        }
        let detail = outcome.failure.clone();
        if let Err(error) = log
            .emit(AgentRunEvent::Status {
                status: outcome.status,
                detail,
            })
            .await
        {
            tracing::warn!(target: "zevria::ensemble", %error, "terminal worker outcome is durable but its status diagnostic could not be appended");
            return outcome;
        }
        if let Some(error) = outcome.failure.clone()
            && let Err(persistence_error) = log.emit(AgentRunEvent::Failure { error }).await
        {
            tracing::warn!(target: "zevria::ensemble", %persistence_error, "terminal worker outcome is durable but its failure diagnostic could not be appended");
        }
        outcome
    }
}

fn ensure_agent_run_ignore_guard(workspace: &Path) -> anyhow::Result<()> {
    let directory =
        zevria_foundation::runtime_paths::workspace_state_root(workspace).join("agent-runs");
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create the agent-run transcript directory at {}",
            directory.display()
        )
    })?;
    let path = directory.join(".gitignore");
    let mut file = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(anyhow::Error::from(error).context(format!(
                "failed to create the agent-run ignore guard at {}",
                path.display()
            )));
        }
    };
    file.write_all(b"*\n")
        .and_then(|()| file.sync_all())
        .with_context(|| {
            format!(
                "failed to persist the agent-run ignore guard at {}",
                path.display()
            )
        })
}

impl EnsembleLauncher for EnsembleSupervisor {
    fn workers(&self, workflow: EnsembleWorkflow) -> anyhow::Result<Vec<AgentRunDescriptor>> {
        self.selected_agents(workflow)
            .iter()
            .map(|name| {
                let config = self.config.agents.get(name).with_context(|| {
                    format!("ensemble workflow references unknown agent {name:?}")
                })?;
                let safe_mode = config.mode_for(workflow).with_context(|| {
                    format!("ensemble agent {name:?} has no configured mode for {workflow}")
                })?;
                Ok(AgentRunDescriptor {
                    id: AgentRunId::new(),
                    agent: name.clone(),
                    label: config.label.clone(),
                    safe_mode: safe_mode.to_string(),
                })
            })
            .collect()
    }

    fn max_synthesis_bytes_per_agent(&self) -> usize {
        self.config.max_synthesis_bytes_per_agent
    }

    fn recover_review(
        &self,
        start: &zevria_workflow::EnsembleStart,
        history: &[(AgentRunId, WorkerReviewEvent)],
    ) -> anyhow::Result<Vec<WorkerActorUpdate>> {
        preflight_existing_logs(&self.agent_runs_root, start)?;
        let mut recovered = Vec::new();
        for descriptor in &start.agents {
            let path = agent_run_path(&self.agent_runs_root, &start.run_id, &descriptor.id);
            if !path.try_exists()? {
                continue;
            }
            let projection = load_agent_run_projection(&path)?;
            let root = history
                .iter()
                .filter(|(id, _)| id == &descriptor.id)
                .map(|(_, event)| event.clone())
                .collect::<Vec<_>>();
            if root
                .iter()
                .any(|event| matches!(event, WorkerReviewEvent::Abandoned { .. }))
            {
                if let Some(journal) = &projection.review {
                    journal.reconcile(&root).map_err(anyhow::Error::msg)?;
                }
                continue;
            }
            anyhow::ensure!(
                !projection
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.status == AgentRunStatus::Completed),
                "worker completion exists without an authoritative root all-worker seal"
            );
            if let Some(journal) = projection.review {
                for event in journal.reconcile(&root).map_err(anyhow::Error::msg)? {
                    recovered.push(WorkerActorUpdate {
                        worker_id: descriptor.id.clone(),
                        event,
                    });
                }
            }
        }
        Ok(recovered)
    }

    fn start_review(
        &self,
        request: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        self.start_interactive_review(request, states, events, turn)
    }

    fn finalize_review<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        outcomes: Vec<AgentRunOutcome>,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async move {
            preflight_existing_logs(&self.agent_runs_root, &request.start)?;
            for outcome in &outcomes {
                if outcome.status == AgentRunStatus::Abandoned {
                    anyhow::ensure!(
                        outcome.is_sanitized_abandonment(),
                        "invalid abandoned frozen outcome"
                    );
                    continue;
                }
                let path = agent_run_path(
                    &self.agent_runs_root,
                    &request.start.run_id,
                    &outcome.descriptor.id,
                );
                let (log, previous) = RunLog::open(
                    path,
                    &request.start,
                    &outcome.descriptor,
                    true,
                    events.clone(),
                    turn.clone(),
                )
                .await?;
                if previous.outcome.as_ref() != Some(outcome) {
                    let confirmed = outcome
                        .confirmation
                        .as_ref()
                        .context("sealed Plan outcome lacks a receipt")?;
                    log.emit(AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::Confirmed {
                            receipt: confirmed.receipt.clone(),
                        }),
                    })
                    .await?;
                    if let Some(receipt) = &confirmed.baseline {
                        log.emit(AgentRunEvent::Review {
                            event: Box::new(WorkerReviewEvent::BaselineMarked {
                                receipt: receipt.clone(),
                            }),
                        })
                        .await?;
                    }
                    log.emit(AgentRunEvent::Review {
                        event: Box::new(WorkerReviewEvent::Sealed),
                    })
                    .await?;
                    log.append_outcome(outcome.clone()).await?;
                }
            }
            Ok(outcomes)
        })
    }

    fn launch<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async move { self.launch_all(request, events, turn).await })
    }
}

#[derive(Clone)]
struct RunLog {
    path: PathBuf,
    writer: mpsc::Sender<RunLogWriteCommand>,
    failure: Arc<Mutex<Option<String>>>,
    evidence: Arc<Mutex<WorkerEvidenceState>>,
    review_publication: Arc<Mutex<Option<interactive::ReviewPublication>>>,
    event_order: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone)]
struct PersistedPermissionDecision {
    tool_kind: Option<String>,
    decision: String,
    option_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct WorkerEvidenceState {
    plan: Option<AgentStructuredPlan>,
    repair: Option<AgentRunRepair>,
    last_permission: Option<PersistedPermissionDecision>,
}

impl WorkerEvidenceState {
    fn from_projection(projection: &AgentRunProjection) -> Self {
        Self {
            plan: projection.plan.clone(),
            repair: projection.repair.clone(),
            last_permission: None,
        }
    }

    fn apply(&mut self, event: &AgentRunEvent) {
        reduce_agent_plan_event(&mut self.plan, event);
        match event {
            AgentRunEvent::Prompt {
                continuation: false,
                repair,
                ..
            } => {
                self.plan = None;
                self.repair.clone_from(repair);
                self.last_permission = None;
            }
            AgentRunEvent::Prompt {
                continuation: true,
                repair,
                ..
            } => {
                if repair.is_some() {
                    self.repair.clone_from(repair);
                }
                self.last_permission = None;
            }
            AgentRunEvent::Permission {
                tool_kind,
                decision,
                option_id,
            } => {
                self.last_permission = Some(PersistedPermissionDecision {
                    tool_kind: tool_kind.clone(),
                    decision: decision.clone(),
                    option_id: option_id.clone(),
                });
            }
            // A rejected tool can immediately report its terminal update before
            // the prompt returns `cancelled`, so ToolCallUpdate deliberately
            // preserves causality. Later semantic output, a new action, or a
            // session/replay boundary makes the rejection too stale to infer
            // why a subsequent cancellation occurred.
            AgentRunEvent::SessionEstablished { .. }
            | AgentRunEvent::UserMessage { .. }
            | AgentRunEvent::AgentMessage { .. }
            | AgentRunEvent::Thought { .. }
            | AgentRunEvent::ToolCall { .. }
            | AgentRunEvent::Plan { .. }
            | AgentRunEvent::NativePlanCaptured { .. }
            | AgentRunEvent::PlanRemoved { .. }
            | AgentRunEvent::Elicitation { .. }
            | AgentRunEvent::ReplayBoundary
            | AgentRunEvent::Unsupported { .. } => {
                self.last_permission = None;
            }
            _ => {}
        }
    }

    fn has_plan_proof(&self) -> bool {
        self.plan
            .as_ref()
            .is_some_and(AgentStructuredPlan::has_markdown_proof)
    }

    fn denied_permission_repair(&self) -> Option<AgentRunRepair> {
        let permission = self.last_permission.as_ref()?;
        matches!(
            permission.decision.as_str(),
            "reject_once" | "reject_always"
        )
        .then(|| AgentRunRepair::HostDeniedPermission {
            tool_kind: permission.tool_kind.clone(),
            option_id: permission.option_id.clone(),
        })
    }
}

enum RunLogWriteCommand {
    Record(Box<RunLogRecordCommand>),
    Barrier {
        acknowledgement: oneshot::Sender<Result<(), String>>,
    },
}

struct RunLogRecordCommand {
    record: AgentRunTranscriptRecord,
    publication: Option<AgentRunEvent>,
    force_sync: bool,
    acknowledgement: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Clone)]
struct RunLogPublication {
    events: SessionEventSender,
    turn_id: zevria_foundation::TurnId,
    ensemble_run_id: EnsembleRunId,
    agent_run_id: AgentRunId,
}

/// Batch preflight runs before concurrent workers or frontend restoration.
/// A missing log is a valid interruption before that worker was started.
pub fn preflight_existing_logs(
    root: &Path,
    start: &zevria_workflow::EnsembleStart,
) -> anyhow::Result<()> {
    for descriptor in &start.agents {
        let path = agent_run_path(root, &start.run_id, &descriptor.id);
        match std::fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let projection = load_agent_run_projection(&path)?;
        let expected = AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            descriptor: descriptor.clone(),
            prompt: start.prompt.clone(),
        };
        if projection.header.as_ref() != Some(&expected) {
            return Err(zevria_transcript::transcript::UnsupportedHistory::new(
                &path,
                Some(1),
                "worker identity matching the root ensemble start",
            )
            .into());
        }
    }
    Ok(())
}

#[derive(Clone)]
struct RawDiagnosticSender {
    sender: mpsc::Sender<RawDiagnosticMessage>,
    dropped_lines: Arc<AtomicU64>,
}

enum RawDiagnosticMessage {
    Line(String, LineDirection),
    Barrier(oneshot::Sender<()>),
}

struct RawDiagnosticSink {
    sender: RawDiagnosticSender,
    task: tokio::task::JoinHandle<()>,
    log: RunLog,
}

trait RunLogWriter: Send + 'static {
    fn append_buffered(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize>;
    fn sync(&mut self) -> anyhow::Result<()>;
}

impl RunLogWriter for AgentRunTranscriptWriter {
    fn append_buffered(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        AgentRunTranscriptWriter::append_buffered(self, record)
    }

    fn sync(&mut self) -> anyhow::Result<()> {
        AgentRunTranscriptWriter::sync(self)
    }
}

#[derive(Debug, Clone)]
enum WorkerEnd {
    NativeHandoffCompleted,
    NativeHandoffFailed(String),
    PromptResponse { stop_reason: String },
    TimedOut,
    LocalCancelled,
    Interrupted(String),
}

impl WorkerEnd {
    fn is_semantic_prompt_end(&self) -> bool {
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
enum WorkerAttemptMode {
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

struct WorkerAttemptContext<'a> {
    agent_config: &'a EnsembleAgentConfig,
    descriptor: &'a AgentRunDescriptor,
    workflow: EnsembleWorkflow,
    base_prompt: &'a zevria_content::UserPrompt,
    mode: WorkerAttemptMode,
    log: &'a RunLog,
    cancellation: &'a tokio_util::sync::CancellationToken,
    plan_handoff: Option<&'a ClaudePlanHandoff>,
    transient_recovery: &'a Arc<Mutex<TransientRecoveryState>>,
    turn: &'a TurnContext,
    diagnostics: RawDiagnosticSender,
    interactive: Option<InteractiveConnection>,
}

#[derive(Default)]
struct TransientRecoveryState {
    continuations: usize,
    original: Option<TransientPromptFailure>,
}

struct TransientPromptFailure {
    kind: &'static str,
    error: AcpError,
    dispatched: bool,
}

impl TransientRecoveryState {
    fn observe(&mut self, kind: &'static str, error: &AcpError) {
        self.original.get_or_insert_with(|| TransientPromptFailure {
            kind,
            error: error.clone(),
            dispatched: false,
        });
    }

    fn reserve(&mut self, kind: &'static str, error: &AcpError) -> bool {
        if self.continuations >= MAX_LIVE_TRANSIENT_CONTINUATIONS {
            return false;
        }
        self.observe(kind, error);
        self.continuations += 1;
        true
    }

    fn failure_context(&self, login_hint: &str) -> Option<String> {
        let original = self.original.as_ref()?;
        if self.continuations == 0 {
            return Some(format!(
                "transient prompt failure ({}); continuation not scheduled: {}",
                original.kind,
                error_with_login_hint(&original.error, login_hint)
            ));
        }
        let action = if original.dispatched {
            "dispatched"
        } else {
            "scheduled but not dispatched"
        };
        Some(format!(
            "automatic same-session transient continuation {} of {MAX_LIVE_TRANSIENT_CONTINUATIONS} {action} after {}: {}",
            self.continuations,
            original.kind,
            error_with_login_hint(&original.error, login_hint)
        ))
    }
}

#[derive(Default)]
struct WorkerAttemptProgress {
    durable_session_id: Mutex<Option<String>>,
    prompt_dispatched: AtomicBool,
    semantic_end: Mutex<Option<WorkerEnd>>,
}

struct WorkerAttemptResult {
    connection_result: Option<Result<WorkerEnd, AcpError>>,
    startup_timed_out: bool,
    cancelled_during_startup: bool,
    durable_session_id: Option<String>,
    prompt_dispatched: bool,
    semantic_end: Option<WorkerEnd>,
    violation: Option<String>,
}

impl WorkerAttemptResult {
    fn recoverable_process_error(&self) -> Option<&AcpError> {
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

struct SessionSetup {
    session_id: SessionId,
    modes: Option<SessionModeState>,
    config_options: Option<Vec<SessionConfigOption>>,
    recovered: bool,
    interrupted: Option<String>,
}

#[derive(Clone)]
struct SessionConfigurationExpectation {
    safe_mode: SafeModeExpectation,
    workflow: EnsembleWorkflow,
    workflow_options: BTreeMap<String, String>,
}

#[derive(Clone)]
struct SafeModeExpectation {
    desired: String,
    config_id: Option<String>,
}

struct SafeModeBootstrap {
    expectation: SafeModeExpectation,
    config_options: Option<Vec<SessionConfigOption>>,
}

#[derive(Debug, Default)]
struct SessionBootstrapGate {
    state: SessionBootstrapState,
}

#[derive(Debug, Default)]
enum SessionBootstrapState {
    #[default]
    Bootstrapping,
    Provisional(String),
    Bound(String),
}

impl SessionBootstrapGate {
    fn observe_notification(&mut self, session_id: &str) -> Result<(), String> {
        match &self.state {
            SessionBootstrapState::Bootstrapping => {
                self.state = SessionBootstrapState::Provisional(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Provisional(provisional) if provisional == session_id => Ok(()),
            SessionBootstrapState::Bound(expected) if expected == session_id => Ok(()),
            SessionBootstrapState::Provisional(provisional) => Err(format!(
                "agent sent an update for conflicting provisional ACP session {session_id:?}; first provisional session was {provisional:?}"
            )),
            SessionBootstrapState::Bound(expected) => Err(format!(
                "agent sent an update for mismatched ACP session {session_id:?}; expected {expected:?}"
            )),
        }
    }

    fn bind(&mut self, session_id: &str) -> Result<(), String> {
        match &self.state {
            SessionBootstrapState::Bootstrapping => {
                self.state = SessionBootstrapState::Bound(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Provisional(provisional) if provisional == session_id => {
                self.state = SessionBootstrapState::Bound(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Bound(expected) if expected == session_id => Ok(()),
            SessionBootstrapState::Provisional(provisional) => Err(format!(
                "session/new returned ACP session {session_id:?}, which conflicts with provisional session {provisional:?}"
            )),
            SessionBootstrapState::Bound(expected) => Err(format!(
                "cannot rebind ACP session from {expected:?} to {session_id:?}"
            )),
        }
    }

    fn bound_id(&self) -> Option<&str> {
        match &self.state {
            SessionBootstrapState::Bound(session_id) => Some(session_id),
            SessionBootstrapState::Bootstrapping | SessionBootstrapState::Provisional(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowConstraintPolicy {
    EnforcedReadOnly,
    PromptOnlyReadOnly,
}

impl WorkflowConstraintPolicy {
    const fn for_workflow(workflow: EnsembleWorkflow) -> Self {
        match workflow {
            EnsembleWorkflow::Plan => Self::EnforcedReadOnly,
            EnsembleWorkflow::Review => Self::PromptOnlyReadOnly,
        }
    }
}

#[derive(Debug)]
enum ElicitationConversionError {
    Decline { field_count: usize, reason: String },
    ProtocolViolation { field_count: usize, reason: String },
}

impl ElicitationConversionError {
    fn decline(field_count: usize, reason: impl Into<String>) -> Self {
        Self::Decline {
            field_count,
            reason: reason.into(),
        }
    }

    fn field_count(&self) -> usize {
        match self {
            Self::Decline { field_count, .. } | Self::ProtocolViolation { field_count, .. } => {
                *field_count
            }
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::Decline { reason, .. } | Self::ProtocolViolation { reason, .. } => reason,
        }
    }
}

#[derive(Debug)]
struct ConvertedElicitation {
    request: QuestionRequest,
    fields: Vec<ElicitationFieldMapping>,
}

#[derive(Debug)]
struct ElicitationFieldMapping {
    property: String,
    required: bool,
    kind: ElicitationFieldKind,
}

/// Native and Codex user-note companions return a primary Other token plus text.
/// Legacy Codex/Claude companions return only text (and normal multi tokens).
#[derive(Debug, Clone)]
struct CustomAnswerCompanion {
    property: String,
    other_value: Option<String>,
    default: Option<String>,
}

struct CustomCompanionTarget {
    question_id: String,
    other_value: Option<String>,
    codex_user_note: bool,
}

#[derive(Debug)]
enum ElicitationFieldKind {
    Text,
    Single {
        values: HashMap<String, String>,
        companion: Option<CustomAnswerCompanion>,
    },
    Multi {
        values: HashMap<String, String>,
        companion: Option<CustomAnswerCompanion>,
    },
    Boolean {
        values: HashMap<String, bool>,
    },
}

fn client_capabilities(workflow: EnsembleWorkflow) -> ClientCapabilities {
    let capabilities = ClientCapabilities::new()
        .session(
            ClientSessionCapabilities::new()
                .config_options(SessionConfigOptionsCapabilities::new()),
        )
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    match workflow {
        EnsembleWorkflow::Plan => capabilities.plan(PlanCapabilities::new()),
        EnsembleWorkflow::Review => capabilities,
    }
}

struct PromptRunContext<'a> {
    cancellation: &'a tokio_util::sync::CancellationToken,
    elicitation_lifetime: &'a tokio_util::sync::CancellationToken,
    plan_handoff: Option<&'a ClaudePlanHandoff>,
    prompt_dispatched: &'a AtomicBool,
    semantic_end: &'a Mutex<Option<WorkerEnd>>,
    transient_recovery: &'a Mutex<TransientRecoveryState>,
    turn_timeout: Option<Duration>,
    cancel_grace: Duration,
    keep_alive: bool,
}

struct NotificationContext<'a> {
    log: &'a RunLog,
    cancellation: &'a tokio_util::sync::CancellationToken,
    violation: &'a Arc<Mutex<Option<String>>>,
    session_gate: &'a Arc<Mutex<SessionBootstrapGate>>,
    configuration_expectation: &'a Arc<Mutex<Option<SessionConfigurationExpectation>>>,
    plan_handoff: Option<&'a ClaudePlanHandoff>,
}

async fn handle_notification(
    notification: SessionNotification,
    connection: &ConnectionTo<Agent>,
    context: NotificationContext<'_>,
) -> Result<(), AcpError> {
    let NotificationContext {
        log,
        cancellation,
        violation,
        session_gate,
        configuration_expectation,
        plan_handoff,
    } = context;
    let session_id = notification.session_id.to_string();
    let session_result = session_gate
        .lock()
        .expect("ACP session gate lock poisoned")
        .observe_notification(&session_id);
    if let Err(error) = session_result {
        set_violation(violation, error.clone());
        cancellation.cancel();
        log.emit(AgentRunEvent::Failure {
            error: error.clone(),
        })
        .await
        .map_err(acp_error)?;
        connection.send_notification(CancelNotification::new(notification.session_id))?;
        return Ok(());
    }

    if let Some(error) = session_configuration_violation(
        &notification.update,
        &clone_mutex(configuration_expectation),
    ) {
        set_violation(violation, error.clone());
        cancellation.cancel();
        log.emit(AgentRunEvent::Failure { error })
            .await
            .map_err(acp_error)?;
        connection.send_notification(CancelNotification::new(notification.session_id.clone()))?;
    }
    if let Some(plan_handoff) = plan_handoff
        && let Err(error) = plan_handoff.inspect_update(&notification.update)
    {
        set_violation(violation, error.clone());
        cancellation.cancel();
        log.emit(AgentRunEvent::Failure { error })
            .await
            .map_err(acp_error)?;
        connection.send_notification(CancelNotification::new(notification.session_id.clone()))?;
    }
    for event in normalize_notification(notification) {
        log.emit(event).await.map_err(acp_error)?;
    }
    Ok(())
}

struct PermissionContext<'a> {
    log: &'a RunLog,
    cancellation: &'a tokio_util::sync::CancellationToken,
    violation: &'a Arc<Mutex<Option<String>>>,
    constraint_policy: WorkflowConstraintPolicy,
    session_gate: &'a Arc<Mutex<SessionBootstrapGate>>,
    plan_handoff: Option<&'a ClaudePlanHandoff>,
    attempt_id: Option<u64>,
    worker_lifetime: &'a tokio_util::sync::CancellationToken,
    peer_cancellation: agent_client_protocol::RequestCancellation,
    cancel_grace: Duration,
    interaction: interactive::PendingInteraction,
}

async fn handle_permission_request<R>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    context: PermissionContext<'_>,
) -> Result<(), AcpError>
where
    R: PermissionResponder + Send + 'static,
{
    let PermissionContext {
        log,
        cancellation,
        violation,
        constraint_policy,
        session_gate,
        plan_handoff,
        attempt_id,
        worker_lifetime,
        peer_cancellation,
        cancel_grace,
        interaction,
    } = context;
    if cancellation.is_cancelled()
        || worker_lifetime.is_cancelled()
        || peer_cancellation.is_cancelled()
    {
        return respond_cancelled_permission(request, responder, log).await;
    }

    let request_session_id = request.session_id.to_string();
    let expected_session_id = session_gate
        .lock()
        .expect("ACP session gate lock poisoned")
        .bound_id()
        .map(str::to_string);
    if expected_session_id.as_deref() != Some(request_session_id.as_str()) {
        return reject_invalid_permission(
            request,
            responder,
            connection,
            log,
            cancellation,
            violation,
            format!(
                "agent requested permission for mismatched ACP session {request_session_id:?}; expected {expected_session_id:?}"
            ),
        )
        .await;
    }

    if let Some(plan_handoff) = plan_handoff {
        match plan_handoff.permission(&request) {
            ClaudeHandoffPermission::Ordinary => {}
            ClaudeHandoffPermission::ArtifactMutation => {
                let Some(option) = artifact_mutation_permission_option(&request.options) else {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        "Claude plan artifact mutation offered no allow_once permission option"
                            .to_string(),
                    )
                    .await;
                };
                let option_id = option.option_id.clone();
                let ticket = match plan_handoff.register_permission(
                    &request,
                    attempt_id.expect("native handoff has a connection attempt"),
                ) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        return reject_invalid_permission(
                            request,
                            responder,
                            connection,
                            log,
                            cancellation,
                            violation,
                            error,
                        )
                        .await;
                    }
                };
                let context = ArtifactPermissionContext {
                    log: log.clone(),
                    cancellation: cancellation.clone(),
                    worker_lifetime: worker_lifetime.clone(),
                    peer_cancellation,
                    violation: violation.clone(),
                    session_gate: session_gate.clone(),
                };
                let task_connection = connection.clone();
                // The ticket is already registered in receive order. Its Drop
                // guard removes it even if spawn fails or the task is abandoned.
                connection.spawn(async move {
                    let _interaction = interaction;
                    handle_artifact_permission(
                        request,
                        responder,
                        &task_connection,
                        context,
                        ticket,
                        option_id,
                    )
                    .await
                })?;
                return Ok(());
            }
            ClaudeHandoffPermission::ExitPlanMode { source } => {
                let Some(option) = keep_planning_permission_option(&request.options) else {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        "Claude ExitPlanMode offered no one-shot rejection option".to_string(),
                    )
                    .await;
                };
                let option_id = option.option_id.clone();
                let durable_option_id = Some(option_id.to_string());
                if cancellation.is_cancelled() {
                    return respond_cancelled_permission(request, responder, log).await;
                }
                let tool_call_id = request.tool_call.tool_call_id.to_string();
                if let Err(error) = plan_handoff.begin_settlement(&tool_call_id, source) {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        error,
                    )
                    .await;
                }
                let guard = NativeCaptureGuard(plan_handoff.clone());
                responder.respond_permission(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))?;
                let permission = AgentRunEvent::Permission {
                    tool_kind: request.tool_call.fields.kind.as_ref().map(wire_name),
                    decision: "reject_once".to_string(),
                    option_id: durable_option_id,
                };
                let log = log.clone();
                let cancellation = cancellation.clone();
                let lifetime = worker_lifetime.clone();
                // Answer first: some adapters cannot deliver the Write terminal
                // until ExitPlanMode's rejection has been received.
                connection.spawn(async move {
                    let _interaction = interaction;
                    guard
                        .settle(
                            log,
                            permission,
                            cancellation,
                            lifetime,
                            peer_cancellation,
                            cancel_grace,
                        )
                        .await
                })?;
                return Ok(());
            }
            ClaudeHandoffPermission::Invalid(error) => {
                return reject_invalid_permission(
                    request,
                    responder,
                    connection,
                    log,
                    cancellation,
                    violation,
                    error,
                )
                .await;
            }
        }
    }

    let tool_kind = request.tool_call.fields.kind;
    let choice = select_permission_option(
        constraint_policy,
        &request.options,
        tool_kind,
        cancellation.is_cancelled(),
    );
    let kind_name = tool_kind.map(|kind| wire_name(&kind));
    let (outcome, decision, option_id) = match choice {
        Some(option) => (
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            wire_name(&option.kind),
            Some(option.option_id.to_string()),
        ),
        None => {
            let decision = if cancellation.is_cancelled() {
                "cancelled".to_string()
            } else {
                let error = match constraint_policy {
                    WorkflowConstraintPolicy::EnforcedReadOnly => {
                        "agent offered no safe permission response; cancelling worker"
                    }
                    WorkflowConstraintPolicy::PromptOnlyReadOnly => {
                        "review agent offered no allow_once permission response; cancelling worker"
                    }
                }
                .to_string();
                set_violation(violation, error);
                cancellation.cancel();
                connection
                    .send_notification(CancelNotification::new(request.session_id.clone()))?;
                "cancelled_no_safe_option".to_string()
            };
            (RequestPermissionOutcome::Cancelled, decision, None)
        }
    };
    // Respond synchronously before awaiting transcript I/O. This keeps a
    // Ctrl-C observed by the handler from turning into a late allow_once.
    responder.respond_permission(RequestPermissionResponse::new(outcome))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: kind_name,
        decision,
        option_id,
    })
    .await
    .map_err(acp_error)
}

struct ArtifactPermissionContext {
    log: RunLog,
    cancellation: tokio_util::sync::CancellationToken,
    worker_lifetime: tokio_util::sync::CancellationToken,
    peer_cancellation: agent_client_protocol::RequestCancellation,
    violation: Arc<Mutex<Option<String>>>,
    session_gate: Arc<Mutex<SessionBootstrapGate>>,
}

impl ArtifactPermissionContext {
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
            || self.worker_lifetime.is_cancelled()
            || self.peer_cancellation.is_cancelled()
    }
}

async fn handle_artifact_permission<R: PermissionResponder>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    context: ArtifactPermissionContext,
    ticket: ClaudePlanPermissionTicket,
    option_id: agent_client_protocol::schema::v1::PermissionOptionId,
) -> Result<(), AcpError> {
    // Subscribe BEFORE checking state. watch retains a change between that
    // check and changed().await, including a terminal sent immediately after
    // the first permission response. No mutex guard crosses an await.
    let mut changed = ticket.handoff.changed.subscribe();
    let result = async {
        loop {
            if context.is_cancelled() {
                return respond_cancelled_permission(request, responder, &context.log).await;
            }
            let session_matches = context
                .session_gate
                .lock()
                .expect("ACP session gate lock poisoned")
                .bound_id()
                == Some(ticket.key.session_id.as_str());
            let admission = if session_matches {
                ticket.try_admit(&request, || context.is_cancelled())
            } else {
                Err("queued Claude plan permission no longer matches the ACP session".to_string())
            };
            match admission {
                Ok(ClaudePlanAdmission::Waiting) => {}
                Ok(ClaudePlanAdmission::Cancelled) => {
                    return respond_cancelled_permission(request, responder, &context.log).await;
                }
                Ok(ClaudePlanAdmission::Granted) => {
                    // Ownership is logical state, not a task-scoped lock. There
                    // is no await between the final cancellation check/reserve
                    // and queuing the exact offered one-shot response.
                    return deliver_artifact_permission(
                        &request,
                        responder,
                        &context.log,
                        &ticket,
                        option_id,
                    )?
                    .await;
                }
                Err(error) => {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        &context.log,
                        &context.cancellation,
                        &context.violation,
                        error,
                    )
                    .await;
                }
            }
            tokio::select! {
                biased;
                () = context.cancellation.cancelled() => {},
                () = context.worker_lifetime.cancelled() => {},
                () = context.peer_cancellation.cancelled() => {},
                result = changed.changed() => { result.map_err(acp_error)?; },
            }
        }
    }
    .await;
    if let Err(error) = &result {
        // In particular, ambiguous response delivery or failed Permission sync
        // must fail closed before another waiter can be admitted.
        set_violation(
            &context.violation,
            format!("Claude plan permission task failed: {error}"),
        );
        context.cancellation.cancel();
    }
    result
}

fn deliver_artifact_permission<'a, R: PermissionResponder>(
    request: &'a RequestPermissionRequest,
    responder: R,
    log: &'a RunLog,
    ticket: &'a ClaudePlanPermissionTicket,
    option_id: agent_client_protocol::schema::v1::PermissionOptionId,
) -> Result<impl Future<Output = Result<(), AcpError>> + Send + 'a, AcpError> {
    // Deliberately not an async fn: queue the response synchronously, before
    // returning the transcript future to the caller.
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.clone())),
    ))?;
    Ok(async move {
        log.emit(AgentRunEvent::Permission {
            tool_kind: request.tool_call.fields.kind.as_ref().map(wire_name),
            decision: "allow_once".to_string(),
            option_id: Some(option_id.to_string()),
        })
        .await
        .map_err(acp_error)?;
        ticket.delivered();
        Ok(())
    })
}

async fn respond_cancelled_permission<R>(
    request: RequestPermissionRequest,
    responder: R,
    log: &RunLog,
) -> Result<(), AcpError>
where
    R: PermissionResponder,
{
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Cancelled,
    ))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: request.tool_call.fields.kind.as_ref().map(wire_name),
        decision: "cancelled".to_string(),
        option_id: None,
    })
    .await
    .map_err(acp_error)
}

async fn reject_invalid_permission<R>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    log: &RunLog,
    cancellation: &tokio_util::sync::CancellationToken,
    violation: &Arc<Mutex<Option<String>>>,
    error: String,
) -> Result<(), AcpError>
where
    R: PermissionResponder,
{
    set_violation(violation, error);
    cancellation.cancel();
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Cancelled,
    ))?;
    connection.send_notification(CancelNotification::new(request.session_id))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: request.tool_call.fields.kind.as_ref().map(wire_name),
        decision: "cancelled_invalid_handoff".to_string(),
        option_id: None,
    })
    .await
    .map_err(acp_error)
}

#[derive(Clone)]
/// Also runs when the ACP future is dropped or its driver is hard-aborted.
struct WorkerElicitationCleanup {
    lifetime: tokio_util::sync::CancellationToken,
    active_question: Arc<Mutex<Option<QuestionRequestId>>>,
    questions: QuestionRequester,
}
impl Drop for WorkerElicitationCleanup {
    fn drop(&mut self) {
        self.lifetime.cancel();
        if let Some(request_id) = take_mutex(&self.active_question) {
            self.questions.cancel_request(&request_id);
        }
    }
}

struct ElicitationHandlerContext {
    questions: QuestionRequester,
    gate: Arc<tokio::sync::Mutex<()>>,
    log: RunLog,
    cancellation: tokio_util::sync::CancellationToken,
    worker_lifetime: tokio_util::sync::CancellationToken,
    violation: Arc<Mutex<Option<String>>>,
    session_gate: Arc<Mutex<SessionBootstrapGate>>,
    session_ready: bool,
    active_question: Arc<Mutex<Option<QuestionRequestId>>>,
    source_label: String,
    turn: TurnContext,
}

async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    responder: agent_client_protocol::Responder<CreateElicitationResponse>,
    context: ElicitationHandlerContext,
) -> Result<(), AcpError> {
    let peer_cancellation = responder.cancellation();
    if peer_cancellation.is_cancelled() {
        context
            .log
            .emit(AgentRunEvent::Elicitation {
                field_count: elicitation_field_count(&request),
                outcome: AgentElicitationOutcome::RequestCancelled,
                decision: None,
                decision_unavailable: None,
            })
            .await
            .map_err(acp_error)?;
        return responder.respond_with_error(AcpError::request_cancelled());
    }
    if !context.session_ready {
        context
            .log
            .emit(AgentRunEvent::Elicitation {
                field_count: elicitation_field_count(&request),
                outcome: AgentElicitationOutcome::Unsupported,
                decision: None,
                decision_unavailable: None,
            })
            .await
            .map_err(acp_error)?;
        tracing::debug!(target: "zevria::ensemble", "declined ACP elicitation received before the prompt turn started");
        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Decline));
    }
    let expected_session = context
        .session_gate
        .lock()
        .expect("ACP session gate lock poisoned")
        .bound_id()
        .map(str::to_string);

    let request_id = QuestionRequestId::generate();
    let converted = match convert_elicitation(
        &request,
        expected_session.as_deref(),
        &context.source_label,
        request_id.clone(),
    ) {
        Ok(converted) => converted,
        Err(error @ ElicitationConversionError::Decline { .. }) => {
            context
                .log
                .emit(AgentRunEvent::Elicitation {
                    field_count: error.field_count(),
                    outcome: AgentElicitationOutcome::Unsupported,
                    decision: None,
                    decision_unavailable: None,
                })
                .await
                .map_err(acp_error)?;
            tracing::debug!(target: "zevria::ensemble", reason = %error.reason(), "declined unsupported ACP elicitation");
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Decline));
        }
        Err(error @ ElicitationConversionError::ProtocolViolation { .. }) => {
            let violation = error.reason().to_string();
            set_violation(&context.violation, violation.clone());
            context.cancellation.cancel();
            context
                .log
                .emit(AgentRunEvent::Elicitation {
                    field_count: error.field_count(),
                    outcome: AgentElicitationOutcome::ProtocolViolation,
                    decision: None,
                    decision_unavailable: None,
                })
                .await
                .map_err(acp_error)?;
            tracing::debug!(target: "zevria::ensemble", reason = %violation, "cancelled invalid ACP elicitation");
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    };
    let field_count = converted.fields.len();

    let gate = tokio::select! {
        biased;
        () = peer_cancellation.cancelled() => {
            context.log.emit(AgentRunEvent::Elicitation {
                field_count,
                outcome: AgentElicitationOutcome::RequestCancelled,
                decision: None,
                decision_unavailable: None,
            }).await.map_err(acp_error)?;
            return responder.respond_with_error(AcpError::request_cancelled());
        }
        () = context.cancellation.cancelled() => {
            context.log.emit(AgentRunEvent::Elicitation {
                field_count,
                outcome: AgentElicitationOutcome::Cancelled,
                decision: None,
                decision_unavailable: None,
            }).await.map_err(acp_error)?;
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
        () = context.worker_lifetime.cancelled() => {
            context.log.emit(AgentRunEvent::Elicitation {
                field_count,
                outcome: AgentElicitationOutcome::Cancelled,
                decision: None,
                decision_unavailable: None,
            }).await.map_err(acp_error)?;
            return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
        gate = context.gate.lock() => gate,
    };
    if peer_cancellation.is_cancelled() {
        drop(gate);
        context
            .log
            .emit(AgentRunEvent::Elicitation {
                field_count,
                outcome: AgentElicitationOutcome::RequestCancelled,
                decision: None,
                decision_unavailable: None,
            })
            .await
            .map_err(acp_error)?;
        return responder.respond_with_error(AcpError::request_cancelled());
    }
    if context.cancellation.is_cancelled() || context.worker_lifetime.is_cancelled() {
        drop(gate);
        context
            .log
            .emit(AgentRunEvent::Elicitation {
                field_count,
                outcome: AgentElicitationOutcome::Cancelled,
                decision: None,
                decision_unavailable: None,
            })
            .await
            .map_err(acp_error)?;
        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
    }

    set_mutex(&context.active_question, Some(request_id.clone()));
    let question_cancellation = tokio_util::sync::CancellationToken::new();
    let question_turn = TurnContext::new(
        context.turn.id,
        context.turn.mode,
        question_cancellation.clone(),
    )
    .with_build_subtasks(context.turn.build_subtasks);
    let mut question = Box::pin(
        context
            .questions
            .ask_request(converted.request.clone(), question_turn),
    );
    let resolution = tokio::select! {
        biased;
        () = peer_cancellation.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::RequestCancelled
        }
        () = context.cancellation.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::Cancelled
        }
        () = context.worker_lifetime.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::Cancelled
        }
        response = question.as_mut() => match response {
            Ok(QuestionResponse::Dismissed) => ElicitationResolution::Declined,
            Ok(response @ QuestionResponse::Answered { .. }) => {
                let decision = converted.normalized_decision(&response);
                match (decision, converted.accepted_content(response)) {
                    (Ok(decision), Ok(content)) => ElicitationResolution::Accepted {
                        content,
                        decision: bounded_captured_decision(decision, field_count),
                    },
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::debug!(target: "zevria::ensemble", reason = %error, "cancelled invalid frontend elicitation response");
                        ElicitationResolution::Unavailable
                    }
                }
            }
            Err(error) => {
                tracing::debug!(target: "zevria::ensemble", reason = %error, "ACP elicitation question broker became unavailable");
                ElicitationResolution::Unavailable
            }
        },
    };
    clear_active_question(&context.active_question, &request_id);

    let (outcome, response, decision, decision_unavailable) = match resolution {
        ElicitationResolution::Accepted { content, decision } => {
            let (decision, decision_unavailable) = match decision {
                CapturedDecision::Available(decision) => (Some(decision), None),
                CapturedDecision::Unavailable(unavailable) => (None, Some(unavailable)),
            };
            (
                AgentElicitationOutcome::Accepted,
                Ok(CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(content),
                ))),
                decision,
                decision_unavailable,
            )
        }
        ElicitationResolution::Declined => (
            AgentElicitationOutcome::Declined,
            Ok(CreateElicitationResponse::new(ElicitationAction::Decline)),
            None,
            None,
        ),
        ElicitationResolution::Cancelled => (
            AgentElicitationOutcome::Cancelled,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
            None,
            None,
        ),
        ElicitationResolution::RequestCancelled => (
            AgentElicitationOutcome::RequestCancelled,
            Err(AcpError::request_cancelled()),
            None,
            None,
        ),
        ElicitationResolution::Unavailable => (
            AgentElicitationOutcome::Unavailable,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
            None,
            None,
        ),
    };
    context
        .log
        .emit(AgentRunEvent::Elicitation {
            field_count,
            outcome,
            decision,
            decision_unavailable,
        })
        .await
        .map_err(acp_error)?;
    // Keep the fair global gate until the terminal ACP response has entered
    // the connection's outgoing queue.
    let result = responder.respond_with_result(response);
    drop(gate);
    result
}

enum ElicitationResolution {
    Accepted {
        content: BTreeMap<String, ElicitationContentValue>,
        decision: CapturedDecision,
    },
    Declined,
    Cancelled,
    RequestCancelled,
    Unavailable,
}

enum CapturedDecision {
    Available(AgentUserDecisionBatch),
    Unavailable(AgentUnavailableDecision),
}

fn bounded_captured_decision(
    decision: AgentUserDecisionBatch,
    field_count: usize,
) -> CapturedDecision {
    let serialized_len = serde_json::to_vec(&decision)
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX);
    if serialized_len <= MAX_NORMALIZED_USER_DECISION_BYTES {
        CapturedDecision::Available(decision)
    } else {
        CapturedDecision::Unavailable(AgentUnavailableDecision::normalized_payload_too_large(
            decision.request_id,
            field_count,
        ))
    }
}

fn clear_active_question(
    active: &Arc<Mutex<Option<QuestionRequestId>>>,
    request_id: &QuestionRequestId,
) {
    let mut active = active.lock().expect("ACP question state lock poisoned");
    if active.as_ref() == Some(request_id) {
        *active = None;
    }
}

fn select_permission_option(
    constraint_policy: WorkflowConstraintPolicy,
    options: &[PermissionOption],
    tool_kind: Option<ToolKind>,
    cancelled: bool,
) -> Option<&PermissionOption> {
    if cancelled {
        return None;
    }
    if constraint_policy == WorkflowConstraintPolicy::PromptOnlyReadOnly {
        return options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::AllowOnce);
    }
    let safe_kind = matches!(
        tool_kind,
        Some(ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Execute)
    );
    if safe_kind
        && let Some(option) = options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::AllowOnce)
    {
        return Some(option);
    }
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectAlways)
        })
}

fn artifact_mutation_permission_option(options: &[PermissionOption]) -> Option<&PermissionOption> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
}

fn keep_planning_permission_option(options: &[PermissionOption]) -> Option<&PermissionOption> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
}

/// Small adapter around the SDK responder so the permission policy remains
/// directly unit-testable without naming its internal generic type.
trait PermissionResponder {
    fn respond_permission(self, response: RequestPermissionResponse) -> Result<(), AcpError>;
}

impl PermissionResponder for agent_client_protocol::Responder<RequestPermissionResponse> {
    fn respond_permission(self, response: RequestPermissionResponse) -> Result<(), AcpError> {
        self.respond(response)
    }
}

fn wire_name(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn error_with_login_hint(error: &AcpError, login_hint: &str) -> String {
    // Render data once in a stable form. ACP Display may pretty-print JSON or
    // quote/escape string data, which otherwise duplicates the same diagnostic.
    let mut message_only = error.clone();
    message_only.data = None;
    let mut rendered = message_only.to_string();
    // Native runtimes may carry their entire diagnostic in ACP error data.
    if let Some(data) = &error.data {
        let detail = data
            .as_str()
            .map_or_else(|| data.to_string(), str::to_string);
        if !rendered.contains(&detail) {
            rendered.push_str(": ");
            rendered.push_str(&detail);
        }
    }
    let lower = rendered.to_ascii_lowercase();
    if error.code == ErrorCode::AuthRequired
        || ["auth", "login", "unauthorized", "401"]
            .iter()
            .any(|needle| lower.contains(needle))
    {
        format!("{rendered}. {login_hint}")
    } else {
        rendered
    }
}

fn is_unexpected_process_termination(error: &AcpError) -> bool {
    if is_incoming_transport_closed(error) {
        return true;
    }
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        if value
            .as_str()
            .is_some_and(|data| data.starts_with(ACP_PROCESS_EXIT_DATA_PREFIX))
        {
            return true;
        }
        let Some(inner) = spawned_task_error_data(value) else {
            return false;
        };
        if is_incoming_transport_closed(&error.clone().data(inner.clone())) {
            return true;
        }
        data = Some(inner);
    }
    false
}

fn transient_prompt_error_kind(error: &AcpError) -> Option<&'static str> {
    if is_unexpected_process_termination(error) {
        return None;
    }
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        match value.get("errorKind").and_then(serde_json::Value::as_str) {
            Some("server_error") => return Some("server_error"),
            Some("overloaded") => return Some("overloaded"),
            Some("rate_limit") => return Some("rate_limit"),
            _ => data = spawned_task_error_data(value),
        }
    }
    None
}

fn spawned_task_error_data(value: &serde_json::Value) -> Option<&serde_json::Value> {
    let object = value.as_object()?;
    (object.len() == 2 && object.get("spawned_at")?.is_string())
        .then(|| object.get("data"))
        .flatten()
}

fn process_exit_data(error: &AcpError) -> Option<&str> {
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        if let Some(data) = value.as_str()
            && data.starts_with(ACP_PROCESS_EXIT_DATA_PREFIX)
        {
            return Some(data);
        }
        data = spawned_task_error_data(value);
    }
    None
}

fn concise_process_failure(error: &AcpError) -> String {
    const MAX_CHARS: usize = 512;

    let detail = process_exit_data(error).unwrap_or(&error.message);
    let compact = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        format!("{}…", compact.chars().take(MAX_CHARS).collect::<String>())
    }
}

fn failed_outcome(
    descriptor: AgentRunDescriptor,
    status: AgentRunStatus,
    error: String,
) -> AgentRunOutcome {
    AgentRunOutcome {
        descriptor,
        status,
        report: String::new(),
        plan: None,
        confirmation: None,
        partial: false,
        failure: Some(error),
        usage: None,
        acp_session_id: None,
        user_decisions: Vec::new(),
        decision_ids: Vec::new(),
        unavailable_decisions: Vec::new(),
    }
}

fn merge_failure(target: &mut Option<String>, error: String) {
    match target {
        Some(existing) => {
            if !existing.contains(&error) {
                existing.push_str("; ");
                existing.push_str(&error);
            }
        }
        None => *target = Some(error),
    }
}

fn acp_error(error: impl Display) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}

fn set_violation(target: &Arc<Mutex<Option<String>>>, error: String) {
    let mut target = target.lock().expect("ACP violation lock poisoned");
    target.get_or_insert(error);
}

fn set_mutex<T>(target: &Arc<Mutex<T>>, value: T) {
    *target.lock().expect("ACP state lock poisoned") = value;
}

fn bind_session_gate(
    gate: &Arc<Mutex<SessionBootstrapGate>>,
    session_id: &SessionId,
) -> Result<(), String> {
    gate.lock()
        .expect("ACP session gate lock poisoned")
        .bind(&session_id.to_string())
}

fn worker_session_meta(
    transport: Option<ReviewSystemPromptTransport>,
    workflow: EnsembleWorkflow,
) -> Option<Meta> {
    if workflow != EnsembleWorkflow::Review
        || transport != Some(ReviewSystemPromptTransport::ClaudeCodeAppend)
    {
        return None;
    }
    Some(Meta::from_iter([(
        "systemPrompt".to_string(),
        serde_json::json!({ "append": REVIEW_WORKER_INSTRUCTION }),
    )]))
}

fn new_session_request(workspace: &Path, meta: &Option<Meta>) -> NewSessionRequest {
    NewSessionRequest::new(workspace.to_path_buf()).meta(meta.clone())
}

fn resume_session_request(
    session_id: SessionId,
    workspace: &Path,
    meta: &Option<Meta>,
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, workspace.to_path_buf()).meta(meta.clone())
}

fn load_session_request(
    session_id: SessionId,
    workspace: &Path,
    meta: &Option<Meta>,
) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, workspace.to_path_buf()).meta(meta.clone())
}

fn worker_prompt(
    workflow: EnsembleWorkflow,
    request: &zevria_content::UserPrompt,
    native_plan_handoff: bool,
) -> zevria_content::UserPrompt {
    let instructions = match workflow {
        EnsembleWorkflow::Plan => {
            let shared = "Act as an independent planning worker. Your plans are proposals for per-worker review, never user confirmation or implementation permission. Follow-ups continue this same conversation. Ordinary prose discussion is valid; after successful feedback, explicitly republish the complete nonempty Markdown plan before the user can confirm it. Intentional identical republication is valid. Finishing a proposal ends only this prompt round; later user feedback permits more source-read-only investigation and questions. Inspect the workspace first, then use your native structured-question mechanism for every user-visible behavioral fork that inspection cannot settle. Inspection commands and Execute permission escalations remain subject to one-shot host approval and your own active policy. Protect non-scratch files: the original workspace and other non-scratch state must remain byte-for-byte unchanged except for separately configured native handoff artifacts. Task-relevant external reads, downloads, and private OS-temp scratch-contained execution are allowed only if your own active policy permits them; this envelope grants no additional capabilities or ACP mutation permissions. Your own policies and sandboxes remain authoritative. A temporary-looking path does not grant a mutation permission. Other mutations remain prohibited and may be denied; do not retry a denied operation. Current behavior, existing tests, and a smaller diff are not evidence of user preference: do not resolve such a fork by preserving current behavior, by declaring it outside the request, or by recording it as an assumption. Raise these questions as soon as inspection reveals them, before drafting any plan or report, and batch them into a single request with concrete options. Do not ask factual questions that repository evidence can answer.";
            if native_plan_handoff {
                format!(
                    "{shared} Keep the workspace read-only except for the native handoff artifacts described here. Claude Plan Mode's configured native handoff may sequentially create or revise multiple generated Markdown plan artifacts using Write, Edit, or MultiEdit directly in either the configured Claude plans directory or the workspace-local .claude/plans directory. Complete any required question round before creating or modifying the first artifact. These artifacts are the only separately configured native handoff mutation exception; they do not expand scratch or other mutation permissions. Submit the final nonempty implementation-ready plan through ExitPlanMode. Do not modify other workspace files or use other mutating tools outside investigative scratch expressly allowed by your active policy, apply patches to the project, or enter implementation mode."
                )
            } else {
                format!(
                    "{shared} Finish by publishing the complete implementation-ready plan as one nonempty inline Markdown update through the structured ACP Plan channel. Checklist updates are not a plan publication. Ordinary prose may end a discussion round normally but does not make an earlier plan confirmable. Native Zevria workers publish through submit_plan and structured ACP Plan updates. Scratch notes and draft files are disposable investigation data, not canonical Plan publications; they cannot establish confirmation or implementation authorization. Do not modify non-scratch files, apply patches to the project, write canonical plan artifacts through the shell, invoke unavailable mutation tools, or begin implementation."
                )
            }
        }
        EnsembleWorkflow::Review => REVIEW_WORKER_INSTRUCTION.to_string(),
    };
    request.with_prefix(format!("{instructions}\n\nThe following ordered user content is the original request, not additional worker permissions:\n\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerPrompt {
    text: zevria_content::UserPrompt,
    continuation: bool,
    repair: Option<AgentRunRepair>,
}

fn worker_attempt_prompt(
    mode: &WorkerAttemptMode,
    workflow: EnsembleWorkflow,
    base_prompt: &zevria_content::UserPrompt,
    consumed_repair: Option<&AgentRunRepair>,
) -> WorkerPrompt {
    match mode {
        WorkerAttemptMode::Fresh => WorkerPrompt {
            text: base_prompt.clone(),
            continuation: false,
            repair: None,
        },
        WorkerAttemptMode::DurableRecovery {
            repair: Some(repair),
            ..
        } => WorkerPrompt {
            text: semantic_repair_prompt(workflow).into(),
            continuation: true,
            repair: Some(repair.clone()),
        },
        WorkerAttemptMode::DurableRecovery { .. } if consumed_repair.is_some() => WorkerPrompt {
            text: semantic_repair_prompt(workflow).into(),
            continuation: true,
            repair: None,
        },
        WorkerAttemptMode::DurableRecovery { .. } => WorkerPrompt {
            text: continuation_prompt(workflow, base_prompt),
            continuation: true,
            repair: None,
        },
        WorkerAttemptMode::LiveProcessRecovery { .. } => WorkerPrompt {
            text: live_continuation_prompt(workflow, consumed_repair).into(),
            continuation: true,
            repair: None,
        },
    }
}

fn live_continuation_prompt(
    workflow: EnsembleWorkflow,
    consumed_repair: Option<&AgentRunRepair>,
) -> String {
    if consumed_repair.is_some() {
        semantic_repair_prompt(workflow)
    } else {
        LIVE_PROCESS_CONTINUATION_PROMPT.to_string()
    }
}

fn semantic_repair_prompt(workflow: EnsembleWorkflow) -> String {
    match workflow {
        EnsembleWorkflow::Plan => "Return the complete implementation-ready plan now through the structured Plan channel as one nonempty inline Markdown payload. Do not repeat inspection, retry any denied operation, implement anything, or mutate files. The workspace must remain unchanged; finish the plan and end the turn.".to_string(),
        EnsembleWorkflow::Review => "Return the complete findings-first review now. Do not repeat inspection or mutate files; provide the final actionable review and end the turn.".to_string(),
    }
}

fn continuation_prompt(
    workflow: EnsembleWorkflow,
    _prompt: &zevria_content::UserPrompt,
) -> zevria_content::UserPrompt {
    // The loaded session already owns the original input. Do not duplicate its images.
    live_continuation_prompt(workflow, None).into()
}

fn clone_mutex<T: Clone>(target: &Arc<Mutex<T>>) -> T {
    target.lock().expect("ACP state lock poisoned").clone()
}

fn take_mutex<T>(target: &Arc<Mutex<Option<T>>>) -> Option<T> {
    target.lock().expect("ACP state lock poisoned").take()
}

#[cfg(test)]
#[path = "ensemble_worker_tests.rs"]
mod ensemble_worker_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "response_display_tests.rs"]
mod response_display_tests;

#[path = "logging.rs"]
mod logging;
#[cfg(test)]
use logging::*;

#[path = "diagnostics.rs"]
mod diagnostics;

#[path = "handoff.rs"]
mod handoff;
use handoff::*;

#[path = "elicitation.rs"]
mod elicitation;
use elicitation::*;

#[path = "recovery.rs"]
mod recovery;
use recovery::*;

#[path = "normalization.rs"]
mod normalization;
use normalization::*;
