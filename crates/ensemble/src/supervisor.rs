//! ACP subprocess supervision for independent ensemble workers.

use crate::config::EnsembleConfig;
use anyhow::Context as _;
use history::load_existing_worker;
use logging::RunLog;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zevria_session_api::worker::{EnsembleReviewExecution, WorkerActorUpdate};
use zevria_session_api::{
    EnsembleLaunchFuture, EnsembleLaunchRequest, EnsembleLauncher, QuestionRequester,
    SessionEventSender, TurnContext,
};
use zevria_transcript::agent_run_path;
use zevria_workflow::ensemble_review::{WorkerReviewEvent, WorkerReviewState};
use zevria_workflow::{
    AgentRunDescriptor, AgentRunEvent, AgentRunId, AgentRunOutcome, AgentRunStatus,
    EnsembleWorkflow,
};

#[cfg(windows)]
fn native_acp_helper() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot resolve the native ACP job helper")?;
    // A Rust libtest executable does not dispatch Zevria's private helper flag.
    // Cargo can place it under deps or a deeper per-package artifact directory.
    #[cfg(test)]
    let executable = std::env::var_os("ZEVRIA_TEST_ACP_HELPER")
        .map(PathBuf::from)
        .or_else(|| test_zevria_executable(&executable))
        .with_context(|| {
            format!(
                "cannot find the native ACP job helper near {}; build Zevria with the same Cargo profile before package-local ensemble tests, or set ZEVRIA_TEST_ACP_HELPER",
                executable.display()
            )
        })?;
    anyhow::ensure!(
        executable.is_file(),
        "native ACP job helper is missing at {}; build Zevria with the same Cargo profile before package-local ensemble tests, or set ZEVRIA_TEST_ACP_HELPER",
        executable.display()
    );
    Ok(executable)
}

#[cfg(test)]
fn test_zevria_executable(test_executable: &Path) -> Option<PathBuf> {
    test_executable
        .ancestors()
        .skip(1)
        .take(8)
        .map(|directory| directory.join(format!("zevria{}", std::env::consts::EXE_SUFFIX)))
        .find(|path| path.is_file() && path != test_executable)
}

#[derive(Clone)]
pub struct EnsembleSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
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
            inner: Arc::new(SupervisorInner {
                config,
                #[cfg(windows)]
                acp_helper: native_acp_helper()?,
                workspace,
                agent_runs_root,
                questions,
                question_gate: Arc::new(tokio::sync::Mutex::new(())),
            }),
        })
    }

    fn selected_agents(&self, workflow: EnsembleWorkflow) -> &[String] {
        match workflow {
            EnsembleWorkflow::Plan => &self.inner.config.plan_agents,
            EnsembleWorkflow::Review => &self.inner.config.review_agents,
        }
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
                let config = self.inner.config.agents.get(name).with_context(|| {
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
        self.inner.config.max_synthesis_bytes_per_agent
    }

    fn recover_review(
        &self,
        start: &zevria_workflow::EnsembleStart,
        history: &[(AgentRunId, WorkerReviewEvent)],
    ) -> anyhow::Result<Vec<WorkerActorUpdate>> {
        let mut by_worker: HashMap<_, Vec<_>> = HashMap::new();
        for (id, event) in history {
            by_worker.entry(id).or_default().push(event);
        }
        let mut recovered = Vec::new();
        let mut conflict = None;
        for descriptor in &start.agents {
            let Some(projection) =
                load_existing_worker(&self.inner.agent_runs_root, start, descriptor)?
            else {
                continue;
            };
            // A later invalid header/log still wins over an earlier journal
            // conflict, as it did with the separate preflight pass.
            if conflict.is_some() {
                continue;
            }
            let root = by_worker.get(&descriptor.id).map_or(&[][..], Vec::as_slice);
            let abandoned = root
                .iter()
                .any(|event| matches!(event, WorkerReviewEvent::Abandoned { .. }));
            if !abandoned
                && projection
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.status == AgentRunStatus::Completed)
            {
                conflict = Some(
                    "worker completion exists without an authoritative root all-worker seal"
                        .to_string(),
                );
                continue;
            }
            if let Some(journal) = projection.review {
                match journal.reconcile_events(root.iter().copied()) {
                    Ok(events) => {
                        recovered.extend(events.into_iter().map(|event| WorkerActorUpdate {
                            worker_id: descriptor.id.clone(),
                            event,
                        }))
                    }
                    Err(error) => conflict = Some(error),
                }
            }
        }
        if let Some(error) = conflict {
            anyhow::bail!(error);
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
        self.start_interactive_review(request, states, events, turn, true)
    }

    fn start_recovered_review(
        &self,
        request: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        anyhow::ensure!(
            request.resume,
            "recovered review must resume its validated run"
        );
        self.start_interactive_review(request, states, events, turn, false)
    }

    fn finalize_review<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        outcomes: Vec<AgentRunOutcome>,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        Box::pin(async move {
            preflight_existing_logs(&self.inner.agent_runs_root, &request.start)?;
            for outcome in &outcomes {
                if outcome.status == AgentRunStatus::Abandoned {
                    anyhow::ensure!(
                        outcome.is_sanitized_abandonment(),
                        "invalid abandoned frozen outcome"
                    );
                    continue;
                }
                let path = agent_run_path(
                    &self.inner.agent_runs_root,
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

#[cfg(test)]
#[path = "native_acp_helper_tests.rs"]
mod native_acp_helper_tests;

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

#[path = "diagnostics.rs"]
mod diagnostics;

#[path = "handoff.rs"]
mod handoff;

#[path = "elicitation.rs"]
mod elicitation;

#[path = "recovery.rs"]
mod recovery;

#[path = "normalization.rs"]
mod normalization;

mod attempt;
mod bootstrap;
mod elicitation_request;
mod failure;
mod history;
#[path = "ensemble_interactive.rs"]
mod interactive;
mod permission;
mod prompt;
mod run;
mod wire;

pub use history::preflight_existing_logs;

#[cfg(test)]
use crate::config::{PlanHandoffTransport, ReviewSystemPromptTransport};
#[cfg(test)]
use agent_client_protocol::LineDirection;
#[cfg(test)]
use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, ContentBlock, CreateElicitationRequest, ElicitationContentValue,
    Error as AcpError, PermissionOptionKind, RequestPermissionRequest, RequestPermissionResponse,
    SessionId, SessionNotification, SessionUpdate as AcpSessionUpdate, ToolKind,
};
#[cfg(test)]
use attempt::{WorkerAttemptMode, WorkerAttemptResult, WorkerEnd};
#[cfg(test)]
use bootstrap::{
    SafeModeExpectation, SessionBootstrapGate, SessionConfigurationExpectation,
    client_capabilities, load_session_request, new_session_request, resume_session_request,
};
#[cfg(test)]
use diagnostics::{
    DEBUG_LOG_QUEUE_CAPACITY, RAW_DIAGNOSTIC_MAX_LINE_BYTES, RAW_DIAGNOSTIC_MAX_RUN_BYTES,
    RawDiagnosticSink,
};
#[cfg(test)]
use elicitation::ElicitationFieldKind;
#[cfg(test)]
use elicitation::{
    ConvertedElicitation, ElicitationConversionError, convert_elicitation, elicitation_field_count,
};
#[cfg(test)]
use elicitation_request::{CapturedDecision, bounded_captured_decision};
#[cfg(test)]
use failure::{
    acp_error, error_with_login_hint, is_unexpected_process_termination,
    transient_prompt_error_kind,
};
#[cfg(test)]
use handoff::CLAUDE_CONFIG_DIR_ENV;
#[cfg(test)]
use handoff::{
    ClaudeHandoffPermission, ClaudePlanAdmission, ClaudePlanCandidate, ClaudePlanHandoff,
    ClaudePlanHandoffPhase, NativeCaptureGuard,
};
#[cfg(test)]
use logging::{
    RunLogPublication, RunLogRecordCommand, RunLogWriteCommand, RunLogWriter,
    run_log_event_requires_sync, spawn_run_log_writer,
};
#[cfg(test)]
use normalization::{normalize_notification, normalize_update, session_configuration_violation};
#[cfg(test)]
use permission::{
    PermissionResponder, WorkflowConstraintPolicy, artifact_mutation_permission_option,
    deliver_artifact_permission, keep_planning_permission_option, select_permission_option,
};
#[cfg(test)]
use prompt::REVIEW_WORKER_INSTRUCTION;
#[cfg(test)]
use prompt::{
    WorkerPrompt, continuation_prompt, semantic_repair_prompt, worker_attempt_prompt,
    worker_prompt, worker_session_meta,
};
#[cfg(test)]
use recovery::{TransientRecoveryState, log_transient_continuation};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use tokio::sync::oneshot;
#[cfg(test)]
use wire::permission_name;
#[cfg(test)]
use zevria_foundation::{
    QuestionAnswerValue, QuestionPrompt, QuestionPromptKind, QuestionRequest, QuestionRequestId,
    QuestionResponse,
};
#[cfg(test)]
use zevria_session_api::SessionEvent;
#[cfg(test)]
use zevria_session_api::worker::WorkerActorCommand;
#[cfg(test)]
use zevria_transcript::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunProjection, AgentRunTranscriptHeader,
    AgentRunTranscriptRecord, AgentRunTranscriptWriter, load_agent_run_projection,
};
#[cfg(test)]
use zevria_workflow::ensemble_review::{
    ENSEMBLE_REVIEW_VERSION, WORKER_CONTROL_CAPACITY, WorkerConfirmationReceipt, WorkerControl,
    WorkerControlAction, WorkerControlId, WorkerControlResult, WorkerControlTarget, WorkerInput,
    WorkerPromptKind,
};
#[cfg(test)]
use zevria_workflow::{
    AgentElicitationOutcome, AgentRunRepair, AgentStructuredPlan, AgentUserDecisionAnswer,
    AgentUserDecisionBatch, AgentUserDecisionId, AgentUserDecisionValue,
    CLAUDE_PLAN_HANDOFF_PLAN_ID, EnsembleRunId, MAX_NORMALIZED_USER_DECISION_BYTES,
};
