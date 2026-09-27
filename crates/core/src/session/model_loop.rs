//! Model loop orchestration.

use super::*;

#[derive(Debug)]
pub(super) struct ModelTurnOutput {
    pub(super) message: Option<Message>,
    pub(super) display_attempt_id: Option<String>,
    pub(super) submission_gate: PlanSubmissionGate,
    pub(super) final_item: Option<TranscriptItem>,
    pub(super) usage: Option<TokenUsage>,
}

/// Shared durable completion tail; engine publications do not impersonate a
/// recovered provider response or pass through the synthesis submission gate.
pub(super) struct TurnFinalization {
    pub(super) publication: Option<(PlanRecord, PlanArtifact)>,
    pub(super) retained: Vec<TranscriptItem>,
    pub(super) usage: Option<TokenUsage>,
    pub(super) terminal: SessionEvent,
    pub(super) defer_publication: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinalResponsePersistence {
    Immediate,
    Deferred,
    Recovered,
}

impl<P: ModelProvider> SessionEngine<P> {
    async fn prepare_dispatch(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        state: &mut DispatchState,
    ) -> Result<(), Failure> {
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
            let over_limit = snapshot.projected_input_tokens > snapshot.input_token_limit;
            let automatic_due = self.context.is_armed(policy.model_role)
                && (snapshot.projected_input_tokens >= snapshot.automatic_trigger || over_limit);
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

    pub(super) async fn run_model_tool_loop(
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
        let mut gate = super::orchestration::OrchestrationGate::new(accepted);
        // Never inherit authorization from the caller's nominal mode/context.
        let effective_turn = progress.turn().clone().with_build_subtasks(
            policy.orchestration
                && accepted.request.as_ref().is_some_and(|request| {
                    request.behavior == zevria_foundation::RequestBehavior::Orchestrate
                }),
        );
        let mut call = 0;
        loop {
            self.prepare_dispatch(policy, &progress, &mut state).await?;
            // Immutable owned input releases the conversation borrow while the
            // provider runs. Only display checkpoints may be appended below;
            // they cannot change this request, capacity or workflow state.
            let owned_input = self
                .conversation
                .model_input()
                .into_iter()
                .map(ModelRequestItem::to_owned_item)
                .collect::<anyhow::Result<Vec<_>>>()?;
            let input = owned_input
                .iter()
                .map(OwnedModelRequestItem::as_borrowed)
                .collect::<Vec<_>>();
            if input.is_empty() {
                return Err(anyhow::anyhow!("cannot run a model turn without model input").into());
            }
            let instructions = self.rendered_instructions(policy);
            let request = ModelRequest {
                instructions: &instructions,
                input,
                model_role: policy.model_role,
                allowed_tool_names: policy.allowed_tool_names.as_deref(),
            };
            let (checkpoints, mut checkpoint_rx) =
                mpsc::unbounded_channel::<zevria_session_api::event::AttemptCheckpoint>();
            let request_progress = progress.clone().with_checkpoints(checkpoints);
            let mut checkpointed = HashMap::<String, u64>::new();
            let mut checkpoint_failure = None;
            let completion_result = {
                call += 1;
                let _ = progress
                    .events()
                    .send(SessionEvent::ModelCallStarted {
                        turn_id: progress.turn().id,
                        call,
                    })
                    .await;
                let completion = self.provider.complete(request, request_progress);
                tokio::pin!(completion);
                loop {
                    tokio::select! {
                        biased;
                        () = progress.turn().cancellation().cancelled() => break None,
                        Some(checkpoint) = checkpoint_rx.recv() => {
                            let attempt = checkpoint.attempt;
                            let result = if checkpointed.get(&attempt.id).is_some_and(|revision| *revision >= attempt.revision) {
                                Ok(())
                            } else {
                                attempt.validate().and_then(|()| {
                                    let id = attempt.id.clone();
                                    let revision = attempt.revision;
                                    let result = self.conversation.push_completed_batch(vec![TranscriptItem::WebSearchAttempt(attempt)]);
                                    // Completed writes retain memory even on failure. Never
                                    // append that same revision again during the final drain.
                                    checkpointed.insert(id, revision);
                                    result
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
            if let Some(error) = checkpoint_failure {
                self.persistence_failed(&anyhow::anyhow!(error), progress.events())
                    .await?;
            }
            // The scope above has dropped both the future and borrowed request.
            // Retain all observed retries before errors, cancellation, local
            // dispatch, or publication of the turn's terminal outcome.
            let outcome = match &completion_result {
                None => zevria_content::WebSearchAttemptOutcome::Interrupted,
                Some(Ok(_)) => zevria_content::WebSearchAttemptOutcome::Completed,
                Some(Err(_)) => zevria_content::WebSearchAttemptOutcome::Failed,
            };
            let attempts = progress
                .drain_web_search(outcome)
                .into_iter()
                .filter(zevria_content::WebSearchAttemptRecord::has_display)
                .collect::<Vec<_>>();
            let retained = attempts
                .iter()
                .filter(|attempt| {
                    !checkpointed
                        .get(&attempt.id)
                        .is_some_and(|revision| *revision >= attempt.revision)
                })
                .cloned()
                .map(TranscriptItem::WebSearchAttempt)
                .collect();
            if !attempts.is_empty() {
                self.record_recoverable_items(retained, progress.events())
                    .await?;
                for attempt in attempts {
                    let _ = progress
                        .events()
                        .send(SessionEvent::WebSearchUpdated {
                            turn_id: progress.turn().id,
                            attempt,
                        })
                        .await;
                }
            }
            let Some(response) = completion_result else {
                self.provider.cancel();
                return Err(anyhow::anyhow!("turn cancelled").into());
            };
            let response = response?;
            let usage = response.usage;
            let display_attempt_id = response.record().display_attempt_id().map(str::to_owned);
            let completed = TranscriptItem::from(response.into_record());
            // For replay-backed providers this clone comes from the replay's
            // cached canonical message, never an independently supplied model
            // response. It is therefore authoritative for display and tool
            // dispatch as well as persistence and future requests.
            let assistant = completed
                .message()
                .expect("a completed model response must contain a message")
                .clone();

            let calls = assistant_tool_calls(&assistant);
            if calls.is_empty() && !gate.satisfied() {
                // A premature final is still completed provider work. Preserve
                // its native replay, usage and display identity before correction.
                self.record_completed(completed)?;
                self.ensure_replay_valid()?;
                if let Some(usage) = usage {
                    self.report_provider_usage(policy, usage.total_tokens)?;
                }
                let _ = progress
                    .events()
                    .send(SessionEvent::Intermediate {
                        turn_id: progress.turn().id,
                        message: assistant,
                        display_attempt_id,
                    })
                    .await;
                if progress.turn().is_cancelled() {
                    return Err(Failure::Cancelled);
                }
                let Some(correction) = gate.correction() else {
                    return Err(anyhow::anyhow!("concurrent delegation was not fulfilled: orchestration requires at least two distinct accepted children in one launch_subtasks batch; the single corrective directive has already been issued").into());
                };
                self.record_required(correction)?;
                self.ensure_replay_valid()?;
                state.dispatched_once = true;
                state.compaction_attempted = false;
                state.force_exact_recheck = true;
                continue;
            }
            if calls.is_empty() {
                let (final_item, deferred_usage) = match final_persistence {
                    FinalResponsePersistence::Immediate => {
                        self.record_completed(completed)?;
                        if let Some(usage) = usage {
                            self.report_provider_usage(policy, usage.total_tokens)?;
                        }
                        (None, None)
                    }
                    FinalResponsePersistence::Deferred => (Some(completed), usage),
                    FinalResponsePersistence::Recovered => {
                        unreachable!("recovery does not call the provider")
                    }
                };
                return Ok(ModelTurnOutput {
                    message: Some(assistant),
                    display_attempt_id,
                    submission_gate,
                    final_item,
                    usage: deferred_usage,
                });
            }
            // Intermediate provider work must be durable before executing its
            // tools. Only the final no-tool response may be deferred so its
            // workflow terminal records can share one atomic boundary.
            self.record_completed(completed)?;
            if let Some(usage) = usage {
                self.report_provider_usage(policy, usage.total_tokens)?;
            }
            let _ = progress
                .events()
                .send(SessionEvent::Intermediate {
                    turn_id: progress.turn().id,
                    message: assistant,
                    display_attempt_id,
                })
                .await;

            // Every call in the batch — subtask runs included — finishes
            // inside `execute_tool_calls`, so one continuation prompt always
            // carries the complete, correlated result set. Skill execution is
            // seeded from transcript pins; accepted applications are embedded in
            // their correlated results only after shared engine admission.
            let scope = ToolExecutionScope::new(
                self,
                accepted.mode(),
                policy,
                &effective_turn,
                &submission_gate,
            );
            let batch = execute_tool_calls(
                &scope,
                &calls,
                self.active_skills()?.clone(),
                &submission_gate,
            )
            .await;
            let mut records = vec![TranscriptItem::ToolResults {
                message: batch.message.clone(),
                metadata: batch.metadata.clone(),
                skill_applications: batch.skill_applications.clone(),
            }];
            let mut source = self.conversation.items().to_vec();
            source.extend(records.iter().cloned());
            let active = replay_active_skills(&source)?;
            records.extend(self.instruction_updates(&source, policy, &active)?);
            let persistence = self.record_completed_items(records);
            self.ensure_replay_valid()?;
            persistence?;
            gate.observe(&calls, &batch);
            state.force_exact_recheck |= !batch.skill_applications.is_empty();
            submission_gate.apply_batch(&batch);
            let _ = progress
                .events()
                .send(SessionEvent::ToolResults {
                    turn_id: progress.turn().id,
                    message: batch.message,
                    metadata: batch.metadata,
                })
                .await;
            if progress.turn().is_cancelled() {
                return Err(anyhow::anyhow!("turn cancelled").into());
            }
            state.dispatched_once = true;
            state.compaction_attempted = false;
        }
    }
}

#[derive(Default)]
pub(super) struct DispatchState {
    compaction_attempted: bool,
    dispatched_once: bool,
    force_exact_recheck: bool,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) async fn run_turn(
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
    pub(super) async fn finalize_turn(
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
            SessionEvent::TurnRecovered {
                turn_id: turn.id,
                display_attempt_id: output.display_attempt_id,
            }
        } else {
            SessionEvent::TurnCompleted {
                turn_id: turn.id,
                display_attempt_id: output.display_attempt_id,
                message: output
                    .message
                    .expect("live completion has a final response"),
            }
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
        )
        .await
    }

    pub(super) async fn commit_turn_completion(
        &mut self,
        accepted: &AcceptedTurn,
        completion: TurnFinalization,
        events: &SessionEventSender,
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
                if let Err(error) = self.record_required(TranscriptItem::Plan(record.clone())) {
                    self.persistence_failed(&error, events).await?;
                    return Err(error
                        .context("failed to persist the completed Plan artifact")
                        .into());
                }
            } else {
                retained.push(TranscriptItem::Plan(record.clone()));
            }
        }
        if let AcceptedKind::Ensemble {
            run_id, workflow, ..
        } = &accepted.kind
        {
            retained.push(TranscriptItem::Ensemble(EnsembleRecord::Completed {
                run_id: run_id.clone(),
            }));
            let result = self.record_completed_items(retained);
            if let Some(usage) = usage {
                let policy = self.ensemble_policy(*workflow);
                self.report_provider_usage(&policy, usage.total_tokens)?;
            }
            if let Err(error) = result {
                self.persistence_failed(&error, events).await?;
            }
        }
        let warning = ready.and_then(|(_, artifact)| {
            self.project_plan(&artifact).err().map(|(path, error)| {
                SessionEvent::PlanProjectionWarning {
                    version: artifact.version,
                    path,
                    error,
                }
            })
        });
        let _ = events.send(terminal).await;
        self.emit_plan_state_if_changed(previous, events).await?;
        if let Some(warning) = warning {
            let _ = events.send(warning).await;
        }
        Ok(TurnCompletion)
    }
}
