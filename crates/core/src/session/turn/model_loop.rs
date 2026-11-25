//! Model loop orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::capacity::CapacityCompactionState;
use crate::session::kernel::records::recoverable;
use crate::session::kernel::timing::LifecycleTiming;
use crate::session::turn::execution::{AcceptedKind, AcceptedTurn, Failure, TurnCompletion};
use crate::session::turn::tools::{
    BatchPlan, ToolExecutionScope, assistant_tool_calls, execute_planned_tool_calls,
};
use crate::session::workflow::plan::{PlanSubmissionGate, ensemble_plan_terminal_error};
use rig_core::message::{Message, ToolCall};
use std::collections::HashSet;
use tokio::sync::mpsc;
use zevria_foundation::policy::{SessionMode, TurnPolicy};
use zevria_foundation::tool_result::ToolResultDetail;
use zevria_model::compaction::CompactionTrigger;
use zevria_model::request::ModelRequest;
use zevria_model::telemetry::TokenUsage;
use zevria_session_api::event::{ProgressReporter, SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_session_api::{PlanEvent, TurnEvent};
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::ensemble::{EnsembleRecord, EnsembleWorkflow};
use zevria_workflow::plan::{PlanArtifact, PlanPublicationProvenance, PlanRecord, derived_title};

#[derive(Debug)]
pub(in crate::session) struct ModelTurnOutput {
    pub(in crate::session) message: Option<Message>,
    pub(in crate::session) attempt_id: Option<String>,
    pub(in crate::session) submission_gate: PlanSubmissionGate,
    pub(in crate::session) final_item: Option<TranscriptItem>,
    pub(in crate::session) usage: Option<TokenUsage>,
}

/// Shared durable completion tail; engine publications do not impersonate a
/// recovered provider response or pass through the synthesis submission gate.
pub(in crate::session) struct TurnFinalization {
    pub(in crate::session) publication: Option<(PlanRecord, PlanArtifact)>,
    pub(in crate::session) retained: Vec<TranscriptItem>,
    pub(in crate::session) usage: Option<TokenUsage>,
    pub(in crate::session) terminal: SessionEvent,
    pub(in crate::session) defer_publication: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum FinalResponsePersistence {
    Immediate,
    Deferred,
    Recovered,
}

struct DispatchedModel {
    completion_result: Option<anyhow::Result<zevria_model::ModelResponse>>,
    checkpointed: HashSet<String>,
    checkpoint_failure: Option<String>,
}

struct ReadyResponse {
    usage: Option<TokenUsage>,
    attempt_id: Option<String>,
    completed: TranscriptItem,
    attempts: Vec<zevria_content::ResponseAttempt>,
    persistence_failure: Option<anyhow::Error>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseDisposition {
    Intermediate,
    Final,
    DeferredFinal,
}

struct CommittedResponse {
    message: Option<Message>,
    attempt_id: Option<String>,
    final_item: Option<TranscriptItem>,
    usage: Option<TokenUsage>,
}

struct ToolBatchRound<'a> {
    mode: SessionMode,
    policy: &'a TurnPolicy,
    progress: &'a ProgressReporter,
    turn: &'a TurnContext,
    submission_gate: &'a mut PlanSubmissionGate,
    gate: &'a mut crate::session::turn::orchestration::OrchestrationGate,
}

/// Legacy/gateway output can contain hosted search evidence. Its reported
/// total may count transient retrieval input, not the next request's history.
pub(super) fn has_hosted_search(item: &TranscriptItem) -> bool {
    item.provider_replay().is_some_and(|replay| {
        replay
            .items
            .iter()
            .any(|item| item["type"].as_str() == Some("web_search_call"))
    })
}

impl<P: ModelProvider> SessionEngine<P> {
    async fn publish_attempts(
        &mut self,
        progress: &ProgressReporter,
        attempts: Vec<zevria_content::ResponseAttempt>,
        persistence_failure: Option<&anyhow::Error>,
    ) -> Result<(), SessionReplayError> {
        if let Some(error) = persistence_failure {
            self.persistence_failed(error, progress.events()).await?;
        }
        for attempt in attempts
            .into_iter()
            .filter(zevria_content::ResponseAttempt::has_display)
        {
            let _ = progress
                .events()
                .send(SessionEvent::Turn(TurnEvent::ResponseAttemptUpdated {
                    turn_id: progress.turn().id,
                    attempt: Box::new(attempt),
                }))
                .await;
        }
        Ok(())
    }

    async fn prepare_dispatch(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        state: &mut DispatchState,
    ) -> Result<(), Failure> {
        let _timing = LifecycleTiming::new("prepare_dispatch", self.conversation.items().len());
        loop {
            if progress.turn().is_cancelled() {
                return Err(anyhow::anyhow!("turn cancelled").into());
            }
            self.reconcile_before_dispatch(policy)?;
            let snapshot = self
                .assess_current_request(policy, progress, true, state.force_exact_recheck)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "cannot assess the prepared {} request: {error:#}",
                        policy.model_role.name()
                    )
                })?;
            state.force_exact_recheck = false;
            let compaction_available = self.conversation.has_compaction_prompt();
            let over_limit = snapshot
                .projected_input_tokens
                .is_some_and(|tokens| tokens > snapshot.input_token_limit);
            let automatic_due = self.context.is_armed(policy.model_role)
                && (snapshot.known_input_tokens >= snapshot.automatic_trigger || over_limit);
            if !state.compaction_attempted && compaction_available && automatic_due {
                let trigger = if state.dispatched_once {
                    CompactionTrigger::AutomaticMidTurn
                } else {
                    CompactionTrigger::AutomaticPreTurn
                };
                self.compact_context(
                    trigger,
                    policy,
                    progress.events(),
                    progress.turn(),
                )
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "automatic compaction failed before the prepared {} dispatch: {error:#}",
                        policy.model_role.name()
                    )
                })?;
                state.compaction_attempted = true;
                state.force_exact_recheck = true;
                continue;
            }
            if over_limit {
                let compaction_state = if state.compaction_attempted {
                    CapacityCompactionState::Completed
                } else if compaction_available {
                    CapacityCompactionState::NotAttempted
                } else {
                    CapacityCompactionState::Unavailable
                };
                return Err(anyhow::anyhow!(Self::dispatch_capacity_error(
                    &snapshot,
                    compaction_state,
                ))
                .into());
            }
            return Ok(());
        }
    }

    async fn dispatch_model(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        call: usize,
        timings: &mut crate::session::kernel::timing::RoundTimings,
    ) -> Result<DispatchedModel, Failure> {
        let prepared = self.request_plan(policy);
        // The provider borrows items, while attempt checkpoints use only
        // the disjoint writer. Install their display-only records afterward.
        let (input, input_identity, mut checkpoint_writer) =
            self.conversation.borrow_for_dispatch();
        if input.is_empty() {
            return Err(anyhow::anyhow!("cannot run a model turn without model input").into());
        }
        let request = ModelRequest {
            input_identity,
            tools: &prepared.tools,
            forbid_tool_calls: false,
            instructions: &prepared.instructions,
            input,
            model_role: policy.model_role,
            allowed_tool_names: policy.allowed_tool_names.as_deref(),
        };
        let (checkpoints, mut checkpoint_rx) =
            mpsc::unbounded_channel::<zevria_session_api::event::AttemptCheckpoint>();
        let request_progress = progress
            .clone()
            .with_model_call(call)
            .with_checkpoints(checkpoints);
        let mut checkpointed = std::collections::HashSet::<String>::new();
        let mut checkpoint_failure: Option<String> = None;
        let completion_result = {
            let _ = progress
                .events()
                .send(SessionEvent::Turn(TurnEvent::ModelCallStarted {
                    turn_id: progress.turn().id,
                    call,
                }))
                .await;
            timings.advance(crate::session::kernel::timing::RoundPhase::Model);
            let completion = self.provider.complete(request, request_progress);
            tokio::pin!(completion);
            loop {
                tokio::select! {
                    biased;
                    () = progress.turn().cancellation().cancelled() => break None,
                    Some(checkpoint) = checkpoint_rx.recv() => {
                        let attempt = checkpoint.attempt;
                        let result = if !checkpointed.insert(attempt.id.clone()) {
                            checkpoint_failure.clone().map_or(Ok(()), |error| Err(anyhow::anyhow!(error)))
                        } else {
                            zevria_transcript::IncompleteResponse::from_attempt(&attempt).and_then(|record| {
                                match record {
                                    Some(record) => checkpoint_writer.append(record),
                                    None => Ok(()),
                                }
                            })
                        };
                        let result = result.map_err(|error| format!("attempt checkpoint persistence failed: {error:#}"));
                        if let Err(error) = &result { checkpoint_failure = Some(error.clone()); }
                        // No frontend await, collector lock, or model-input mutation.
                        let _ = checkpoint.ack.send(result);
                    }
                    response = &mut completion => break Some(response),
                }
            }
        };
        timings.advance(crate::session::kernel::timing::RoundPhase::CommitResponse);
        let checkpoint_records = checkpoint_writer.into_records();
        self.conversation
            .install_attempt_checkpoints(checkpoint_records);
        Ok(DispatchedModel {
            completion_result,
            checkpointed,
            checkpoint_failure,
        })
    }

    async fn settle_attempts(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        dispatched: DispatchedModel,
    ) -> Result<ReadyResponse, Failure> {
        let DispatchedModel {
            completion_result,
            checkpointed,
            checkpoint_failure,
        } = dispatched;
        // No provider future or request borrow survives this point. Cancellation
        // also wins if it arrived just as the provider returned a completed response.
        let completion_result = if progress.turn().is_cancelled() {
            None
        } else {
            completion_result
        };
        let completion_result = completion_result.map(|response| {
            response.map(|mut response| {
                let usage = response.usage;
                let attempt_id = response.attempt_id.take();
                (
                    usage,
                    attempt_id,
                    TranscriptItem::from(response.into_record()),
                )
            })
        });
        let committed = completion_result
            .as_ref()
            .and_then(|response| response.as_ref().ok());
        let committed_id = committed.and_then(|(_, id, _)| id.as_deref());
        let committed_item = committed.map(|(_, _, item)| item);
        let mut attempts = progress.drain_attempts();
        let observed_execution = !checkpointed.is_empty()
            || attempts
                .iter()
                .any(|attempt| attempt.has_display() || attempt.response_id.is_some());
        let mut retained = Vec::new();
        for attempt in &mut attempts {
            if Some(attempt.id.as_str()) == committed_id {
                let (presentation, hosted) = committed_item
                    .and_then(zevria_transcript::committed_display)
                    .unwrap_or_default();
                attempt.complete_with(presentation, hosted);
            } else {
                attempt.close_uncommitted(if completion_result.is_none() {
                    zevria_content::ResponseAttemptOutcome::Interrupted
                } else {
                    zevria_content::ResponseAttemptOutcome::Failed
                });
                if !checkpointed.contains(&attempt.id)
                    && let Some(record) =
                        zevria_transcript::IncompleteResponse::from_attempt(attempt)?
                {
                    retained.push(TranscriptItem::IncompleteResponse(Box::new(record)));
                }
            }
        }
        if !observed_execution
            && completion_result.as_ref().is_some_and(|result| {
                result
                    .as_ref()
                    .is_err_and(zevria_model::ModelInputTooLarge::safe_to_replay)
            })
        {
            // Even a rejection with no generated tokens needs durable attempt
            // evidence before recovery. Its diagnostic is model-inert on replay.
            let attempt = attempts.first().cloned().unwrap_or_else(|| {
                zevria_content::ResponseAttempt::new(
                    self.context_policy(policy.model_role).profile.clone(),
                )
            });
            let record = zevria_transcript::IncompleteResponse::token_window_rejection(&attempt)?;
            let display = record.to_attempt();
            if let Some(attempt) = attempts.iter_mut().find(|attempt| attempt.id == display.id) {
                *attempt = display;
            } else {
                attempts.push(display);
            }
            retained.push(TranscriptItem::IncompleteResponse(Box::new(record)));
        }
        let recovery_failure = recoverable(self.record_completed(retained))?;
        let persistence_failure = checkpoint_failure
            .map(anyhow::Error::msg)
            .or(recovery_failure);
        let Some(response) = completion_result else {
            self.publish_attempts(progress, attempts, persistence_failure.as_ref())
                .await?;
            self.provider.cancel();
            return Err(Failure::Cancelled);
        };
        let (usage, attempt_id, completed) = match response {
            Ok(response) => response,
            Err(error) => {
                // Adapter proof is necessary, but observed output/hosted activity
                // from any transmission also vetoes automatic replay.
                let error =
                    if observed_execution
                        && zevria_model::ModelInputTooLarge::safe_to_replay(&error)
                    {
                        zevria_model::ModelInputTooLarge::new(error.context(
                            "observed upstream output/attempt evidence makes replay unsafe",
                        ))
                        .into()
                    } else {
                        error
                    };
                self.publish_attempts(progress, attempts, persistence_failure.as_ref())
                    .await?;
                return Err(error.into());
            }
        };
        Ok(ReadyResponse {
            usage,
            attempt_id,
            completed,
            attempts,
            persistence_failure,
        })
    }

    /// The single response acceptance/publication sequence. Deferred workflow
    /// finals keep their existing atomic commit and terminal-publication point.
    async fn commit_response(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        response: ReadyResponse,
        assistant: Message,
        disposition: ResponseDisposition,
    ) -> Result<CommittedResponse, Failure> {
        let ReadyResponse {
            usage,
            attempt_id,
            completed,
            attempts,
            persistence_failure,
        } = response;
        let hosted_search = usage.is_some() && has_hosted_search(&completed);
        let (final_item, persistence) = if disposition == ResponseDisposition::DeferredFinal {
            (Some(completed), Ok(()))
        } else {
            (None, self.record_completed(completed).map(|_| ()))
        };
        // A slow lifecycle receiver cannot prevent the response write above.
        self.publish_attempts(progress, attempts, persistence_failure.as_ref())
            .await?;
        persistence?;
        if disposition != ResponseDisposition::DeferredFinal
            && let Some(usage) = usage
        {
            if !hosted_search {
                self.report_provider_usage(policy, usage.total_tokens)?;
            }
            progress.usage_updated(usage).await;
            self.publish_usage_projection(policy, progress).await?;
        }
        if disposition == ResponseDisposition::Intermediate {
            let _ = progress
                .events()
                .send(SessionEvent::Turn(TurnEvent::Intermediate {
                    turn_id: progress.turn().id,
                    message: assistant,
                    attempt_id,
                }))
                .await;
            Ok(CommittedResponse {
                message: None,
                attempt_id: None,
                final_item: None,
                usage: None,
            })
        } else {
            Ok(CommittedResponse {
                message: Some(assistant),
                attempt_id,
                final_item,
                usage: (disposition == ResponseDisposition::DeferredFinal)
                    .then_some(usage)
                    .flatten(),
            })
        }
    }

    async fn run_tool_batch(
        &mut self,
        calls: &[ToolCall],
        round: ToolBatchRound<'_>,
        timings: &mut crate::session::kernel::timing::RoundTimings,
    ) -> Result<bool, Failure> {
        let ToolBatchRound {
            mode,
            policy,
            progress,
            turn,
            submission_gate,
            gate,
        } = round;
        // Every call in the batch — subtask runs included — finishes
        // inside `execute_tool_calls`, so one continuation prompt always
        // carries the complete, correlated result set. Skill execution is
        // seeded from transcript pins; accepted applications are embedded in
        // their correlated results only after shared engine admission.
        timings.advance(crate::session::kernel::timing::RoundPhase::Tools);
        let plan = BatchPlan::new(calls, submission_gate);
        let active = if plan.calls.iter().any(|call| call.tool.is_skill()) {
            self.active_skills()?.clone()
        } else {
            Default::default()
        };
        let scope = ToolExecutionScope::new(self, mode, policy, turn, submission_gate)
            .with_events(progress.events());
        let mut batch = execute_planned_tool_calls(&scope, &plan, active, submission_gate).await;
        timings.advance(crate::session::kernel::timing::RoundPhase::CommitResults);
        let previous_plan = self.plan_state()?.clone();
        if let Some(first) = batch.drafts.first() {
            let path = self.plan_projection_path(
                first.target.id,
                derived_title(&first.markdown).expect("validated draft"),
            );
            for (metadata, call) in batch.metadata.iter_mut().zip(&plan.calls) {
                if call.tool.authors_draft()
                    && let Some(ToolResultDetail::PlanDraft(detail)) = &mut metadata.detail
                {
                    detail.projection = path.clone();
                }
            }
        }
        let mut records = vec![TranscriptItem::ToolResults {
            message: batch.message.clone(),
            metadata: batch.metadata.clone(),
            skill_applications: batch.skill_applications.clone(),
        }];
        records.extend(
            batch
                .drafts
                .iter()
                .cloned()
                .map(|draft| TranscriptItem::Plan(Box::new(PlanRecord::DraftSaved { draft }))),
        );
        if !batch.skill_applications.is_empty() {
            let staged = self.instruction_replay()?.clone().apply_suffix(&records)?;
            records.extend(self.instruction_updates_from_state(
                staged.directives(),
                policy,
                staged.skills(),
            )?);
        }
        self.record_completed(records)?;
        gate.observe(&plan.calls, &batch);
        let force_exact_recheck = !batch.skill_applications.is_empty();
        submission_gate.apply_batch(&batch);
        let _ = progress
            .events()
            .send(SessionEvent::Turn(TurnEvent::ToolResults {
                turn_id: progress.turn().id,
                message: batch.message,
                metadata: batch.metadata,
            }))
            .await;
        self.emit_plan_state_if_changed(previous_plan, progress.events())
            .await?;
        if let Some(draft) = batch.drafts.last() {
            self.project_plan_draft(draft, progress.events()).await;
        }
        if progress.turn().is_cancelled() {
            return Err(anyhow::anyhow!("turn cancelled").into());
        }
        Ok(force_exact_recheck)
    }

    pub(in crate::session) async fn run_model_tool_loop(
        &mut self,
        accepted: &AcceptedTurn,
        policy: &TurnPolicy,
        progress: ProgressReporter,
        mut submission_gate: PlanSubmissionGate,
        final_persistence: FinalResponsePersistence,
    ) -> Result<ModelTurnOutput, Failure> {
        let mut state = DispatchState {
            compaction_attempted: accepted.compaction_attempted_for_first_dispatch,
            ..DispatchState::default()
        };
        let mut gate = crate::session::turn::orchestration::OrchestrationGate::new(accepted);
        // Never inherit authorization from the caller's nominal mode/context.
        let effective_turn = progress.turn().clone().with_build_subtasks(
            policy.orchestration
                && accepted.request.as_ref().is_some_and(|request| {
                    request.behavior == zevria_foundation::RequestBehavior::Orchestrate
                }),
        );
        let mut call = 0;
        loop {
            let mut timings = crate::session::kernel::timing::RoundTimings::new(
                progress.turn().id,
                call + 1,
                self.conversation.items().len(),
            );
            if !matches!(
                accepted.kind,
                AcceptedKind::Ensemble { .. } | AcceptedKind::ManualCompaction { .. }
            ) {
                self.deliver_job_notices(policy, &submission_gate, &progress)
                    .await?;
            }
            let response = loop {
                self.prepare_dispatch(policy, &progress, &mut state).await?;
                call += 1;
                let dispatched = self
                    .dispatch_model(policy, &progress, call, &mut timings)
                    .await?;
                // Always settle/durably retain attempt evidence before considering
                // a retry. No failed response ever reaches executable tool calls.
                match self.settle_attempts(policy, &progress, dispatched).await {
                    Ok(response) => break response,
                    Err(Failure::Failed {
                        error,
                        retained,
                        usage,
                    }) if zevria_model::ModelInputTooLarge::safe_to_replay(&error)
                        && retained.is_empty()
                        && usage.is_none()
                        && !state.compaction_attempted
                        && self.context.is_armed(policy.model_role)
                        && self.conversation.has_compaction_prompt() =>
                    {
                        if progress.turn().is_cancelled() {
                            return Err(Failure::Cancelled);
                        }
                        self.ensure_replay_valid()?;
                        // Persistence failure/cancellation outranks extra upstream work.
                        self.gate_persistence(progress.events()).await.map_err(
                            |error| match error {
                                crate::session::turn::execution::Rejection::Cancelled => {
                                    Failure::Cancelled
                                }
                                crate::session::turn::execution::Rejection::Fatal(error) => {
                                    Failure::Fatal(error)
                                }
                                crate::session::turn::execution::Rejection::Rejected(error) => {
                                    anyhow::anyhow!(error).into()
                                }
                            },
                        )?;
                        state.compaction_attempted = true;
                        tracing::warn!(
                            role = policy.model_role.name(),
                            normal_completion_invocations = call,
                            "confirmed pre-execution token-window rejection; spending remaining compaction/retry budget"
                        );
                        self.compact_context(
                            if state.dispatched_once {
                                CompactionTrigger::AutomaticMidTurn
                            } else {
                                CompactionTrigger::AutomaticPreTurn
                            },
                            policy,
                            progress.events(),
                            progress.turn(),
                        )
                        .await?;
                        // Normal checkpoint installation resets continuation and measurements.
                        state.force_exact_recheck = true;
                        if progress.turn().is_cancelled() {
                            return Err(Failure::Cancelled);
                        }
                    }
                    Err(Failure::Failed {
                        error,
                        retained,
                        usage,
                    }) if error.is::<zevria_model::ModelInputTooLarge>() => {
                        let diagnostic = if !zevria_model::ModelInputTooLarge::safe_to_replay(
                            &error,
                        ) {
                            "token-window failure was not confirmed pre-execution (or followed an ambiguous transport attempt); refusing automatic replay of possible upstream work"
                        } else if state.compaction_attempted {
                            "token-window rejection: the pending model round has already spent its one compaction/retry budget; select a larger compatible model or reduce input"
                        } else {
                            "token-window rejection: no armed, replay-safe compaction source is available; select a larger compatible model or start a fresh session"
                        };
                        return Err(Failure::Failed {
                            error: error.context(diagnostic),
                            retained,
                            usage,
                        });
                    }
                    Err(error) => return Err(error),
                }
            };
            // Replay-backed responses expose their one authoritative cached message.
            let assistant = response
                .completed
                .message()
                .expect("a completed model response must contain a message")
                .clone();
            let calls = assistant_tool_calls(&assistant);
            timings.tool_calls(calls.len());
            let premature = calls.is_empty() && !gate.satisfied();
            let disposition = if premature || !calls.is_empty() {
                ResponseDisposition::Intermediate
            } else {
                match final_persistence {
                    FinalResponsePersistence::Immediate => ResponseDisposition::Final,
                    FinalResponsePersistence::Deferred => ResponseDisposition::DeferredFinal,
                    FinalResponsePersistence::Recovered => {
                        unreachable!("recovery does not call the provider")
                    }
                }
            };
            let committed = self
                .commit_response(policy, &progress, response, assistant, disposition)
                .await?;
            if premature {
                if progress.turn().is_cancelled() {
                    return Err(Failure::Cancelled);
                }
                let Some(correction) = gate.correction() else {
                    return Err(anyhow::anyhow!("concurrent delegation was not fulfilled: orchestration requires at least two distinct accepted children in one launch_subtasks batch; the single corrective directive has already been issued").into());
                };
                self.commit(correction)?;
                state.dispatched_once = true;
                state.compaction_attempted = false;
                state.force_exact_recheck = true;
                continue;
            }
            if calls.is_empty() {
                return Ok(ModelTurnOutput {
                    message: committed.message,
                    attempt_id: committed.attempt_id,
                    submission_gate,
                    final_item: committed.final_item,
                    usage: committed.usage,
                });
            }
            state.force_exact_recheck |= self
                .run_tool_batch(
                    &calls,
                    ToolBatchRound {
                        mode: accepted.mode(),
                        policy,
                        progress: &progress,
                        turn: &effective_turn,
                        submission_gate: &mut submission_gate,
                        gate: &mut gate,
                    },
                    &mut timings,
                )
                .await?;
            state.dispatched_once = true;
            state.compaction_attempted = false;
        }
    }
}

#[derive(Default)]
pub(in crate::session) struct DispatchState {
    compaction_attempted: bool,
    dispatched_once: bool,
    force_exact_recheck: bool,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) async fn run_turn(
        &mut self,
        accepted: &AcceptedTurn,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let policy = self.policies.policy(accepted.mode()).clone();
        let progress = ProgressReporter::for_turn(
            events.clone(),
            turn.clone(),
            policy.model_role,
            self.context_policy(policy.model_role),
        );
        let output = self
            .run_model_tool_loop(
                accepted,
                &policy,
                progress,
                PlanSubmissionGate::inert(None),
                FinalResponsePersistence::Immediate,
            )
            .await
            .map_err(|failure| failure.during_work(turn))?;
        self.finalize_turn(
            accepted,
            output,
            FinalResponsePersistence::Immediate,
            events,
            turn,
        )
        .await
    }

    /// Shared persistence and event tail for prompt, handoff, and live/recovered
    /// ensemble synthesis. Deferred provider work is retained even on validation failure.
    pub(in crate::session) async fn finalize_turn(
        &mut self,
        accepted: &AcceptedTurn,
        mut output: ModelTurnOutput,
        persistence: FinalResponsePersistence,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let retained: Vec<_> = output.final_item.take().into_iter().collect();
        let ensemble = match &accepted.kind {
            AcceptedKind::Ensemble {
                run_id, workflow, ..
            } => Some((run_id, *workflow)),
            _ => None,
        };
        let validation = if persistence != FinalResponsePersistence::Recovered
            && ensemble.is_some_and(|(_, workflow)| workflow == EnsembleWorkflow::Plan)
        {
            ensemble_plan_terminal_error(&output.submission_gate)
        } else {
            None
        };
        let validation = validation.or_else(|| {
            (output.submission_gate.candidate.is_some() && accepted.mode() != SessionMode::Plan)
                .then(|| {
                    if ensemble.is_some() {
                        "submit_plan is valid only during an Ensemble Plan synthesis"
                    } else {
                        "submit_plan is valid only during a Plan turn"
                    }
                    .to_string()
                })
        });
        if let Some(error) = validation {
            return Err(Failure::Failed {
                error: anyhow::anyhow!(error),
                retained,
                usage: output.usage,
            });
        }
        let ready = match output.submission_gate.candidate.take() {
            Some(candidate) => match self.plan_ready_record(candidate, turn.id) {
                Ok((record, artifact)) => Some((
                    if ensemble.is_some_and(|(_, workflow)| workflow == EnsembleWorkflow::Plan) {
                        PlanRecord::Published {
                            artifact: artifact.clone(),
                            provenance: PlanPublicationProvenance::Synthesized,
                        }
                    } else {
                        record
                    },
                    artifact,
                )),
                Err(error) => {
                    return Err(match Failure::from(error) {
                        Failure::Failed { error, .. } => Failure::Failed {
                            error,
                            retained,
                            usage: output.usage,
                        },
                        fatal => fatal,
                    });
                }
            },
            None => None,
        };
        let terminal = if persistence == FinalResponsePersistence::Recovered {
            SessionEvent::Turn(TurnEvent::Recovered { turn_id: turn.id })
        } else {
            SessionEvent::Turn(TurnEvent::Completed {
                turn_id: turn.id,
                attempt_id: output.attempt_id,
                message: output
                    .message
                    .expect("live completion has a final response"),
            })
        };
        self.commit_turn_completion(
            accepted,
            TurnFinalization {
                publication: ready,
                retained,
                usage: output.usage,
                terminal,
                defer_publication: persistence != FinalResponsePersistence::Immediate,
            },
            events,
            turn,
        )
        .await
    }

    pub(in crate::session) async fn commit_turn_completion(
        &mut self,
        accepted: &AcceptedTurn,
        completion: TurnFinalization,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let previous = self.plan_state()?.clone();
        let TurnFinalization {
            publication: ready,
            mut retained,
            usage,
            terminal,
            defer_publication,
        } = completion;
        if let Some((record, _)) = &ready {
            if !defer_publication {
                if let Err(error) = self.commit(TranscriptItem::Plan(Box::new(record.clone()))) {
                    self.persistence_failed(&error, events).await?;
                    return Err(error
                        .context("failed to persist the completed Plan artifact")
                        .into());
                }
            } else {
                retained.push(TranscriptItem::Plan(Box::new(record.clone())));
            }
        }
        if let AcceptedKind::Ensemble {
            run_id, workflow, ..
        } = &accepted.kind
        {
            retained.push(TranscriptItem::Ensemble(Box::new(
                EnsembleRecord::Completed {
                    run_id: run_id.clone(),
                },
            )));
            let hosted_search = usage.is_some() && retained.iter().any(has_hosted_search);
            let result = recoverable(self.record_completed(retained))?;
            if let Some(usage) = usage {
                let policy = self.ensemble_policy(*workflow);
                if !hosted_search {
                    self.report_provider_usage(&policy, usage.total_tokens)?;
                }
                let progress = ProgressReporter::for_turn(
                    events.clone(),
                    turn.clone(),
                    policy.model_role,
                    self.context_policy(policy.model_role),
                );
                progress.usage_updated(usage).await;
                self.publish_usage_projection(&policy, &progress).await?;
            }
            if let Some(error) = result {
                self.persistence_failed(&error, events).await?;
            }
        }
        let warning = ready.and_then(|(_, artifact)| match self.project_plan(&artifact) {
            Ok(()) => {
                self.plan_projection_warnings.remove(&artifact.version.id);
                None
            }
            Err((path, error)) => Some(SessionEvent::Plan(PlanEvent::ProjectionWarning {
                version: artifact.version,
                draft: None,
                path,
                error,
            })),
        });
        let _ = events.send(terminal).await;
        self.emit_plan_state_if_changed(previous, events).await?;
        if let Some(warning) = warning {
            let _ = events.send(warning).await;
        }
        Ok(TurnCompletion)
    }
}
