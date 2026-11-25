//! One-shot Review worker scheduling, recovery, and terminal outcomes.

use super::EnsembleSupervisor;
use super::attempt::{WorkerAttemptContext, WorkerAttemptMode, WorkerAttemptResult, WorkerEnd};
use super::diagnostics::RawDiagnosticSink;
use super::failure::{concise_process_failure, error_with_login_hint};
use super::history::preflight_existing_logs;
use super::logging::RunLog;
use super::prompt::worker_prompt;
use super::recovery::TransientRecoveryState;
use futures_util::{StreamExt as _, stream};
use std::sync::Mutex;
use zevria_session_api::{EnsembleLaunchRequest, SessionEventSender, TurnContext};
use zevria_transcript::agent_run_path;
use zevria_workflow::{
    AgentRunDescriptor, AgentRunEvent, AgentRunOutcome, AgentRunStatus, EnsembleWorkflow,
};

pub(super) const MAX_LIVE_PROCESS_RECOVERY_ATTEMPTS: usize = 1;

impl EnsembleSupervisor {
    pub(super) async fn launch_all(
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
        preflight_existing_logs(&self.inner.agent_runs_root, &request.start)?;
        let max_concurrent = self.inner.config.max_concurrent_agents;
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
        let path = agent_run_path(&self.inner.agent_runs_root, &start.run_id, &descriptor.id);
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
        debug_assert_eq!(start.workflow, EnsembleWorkflow::Review);
        if resume && let Some(outcome) = previous.recoverable_outcome() {
            return outcome;
        }
        let durable_recovery_session_id = previous.acp_session_id;
        let Some(agent_config) = self.inner.config.agents.get(&descriptor.agent).cloned() else {
            let agent = descriptor.agent.clone();
            return self
                .finish_worker(
                    &log,
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
        let base_prompt = worker_prompt(EnsembleWorkflow::Review, &start.prompt, false);
        let diagnostics = RawDiagnosticSink::spawn(&log);
        let mut attempt_mode = if resume {
            WorkerAttemptMode::DurableRecovery {
                session_id: durable_recovery_session_id,
                repair: None,
            }
        } else {
            WorkerAttemptMode::Fresh
        };
        let mut recovery_attempts = 0usize;
        let mut recovery_context = None;
        // Live logical-run state, deliberately outside the adapter-attempt loop.
        let transient_recovery = Mutex::new(TransientRecoveryState::default());
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
                    plan_handoff: None,
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
                    self.inner.config.review_startup_timeout_seconds
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
                Ok(WorkerEnd::NativeHandoffCompleted | WorkerEnd::NativeHandoffFailed(_)) => {
                    unreachable!("Review has no native handoff")
                }
                Ok(WorkerEnd::PromptResponse { stop_reason }) if stop_reason == "end_turn" => {
                    (AgentRunStatus::Completed, false, None)
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
                        self.inner.config.review_turn_timeout_seconds
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
            transient_recovery
                .lock()
                .expect("transient recovery lock poisoned")
                .failure_context(&agent_config.login_hint),
            recovery_context,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        self.finish_worker_with_context(
            &log,
            descriptor,
            status,
            partial,
            failure,
            &failure_context,
        )
        .await
    }

    async fn finish_worker(
        &self,
        log: &RunLog,
        descriptor: AgentRunDescriptor,
        status: AgentRunStatus,
        partial: bool,
        failure: Option<String>,
    ) -> AgentRunOutcome {
        self.finish_worker_with_context(log, descriptor, status, partial, failure, &[])
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_worker_with_context(
        &self,
        log: &RunLog,
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
        if status == AgentRunStatus::Completed && partial {
            status = AgentRunStatus::Failed;
            merge_failure(
                &mut failure,
                "internal worker classification attempted a partial Completed outcome".to_string(),
            );
        }
        let mut outcome = log.outcome_evidence(descriptor, status, partial, failure);
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
