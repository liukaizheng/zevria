//! ACP wire projection and event fences.

use super::{
    AcpError, Arc, ContextTokenSnapshot, ElicitationKey, EnsembleEvent, HashMap, JobEvent,
    KnownTools, LiveSession, ManagementEvent, PlanEvent, PlanWorkflowState, PromptPhase,
    QuestionEvent, ResponseUsageSnapshot, SessionEvent, SessionMode, SessionUpdate, StreamSegments,
    SubtaskEvent, ToolCallOutcome, TurnEvent, TurnId, complete_tool_results, diagnostic_update,
    invalid_params, project_tool_calls, retry_diagnostic, usage_update,
};

#[derive(Default)]
pub(super) struct ProjectionState {
    pub(super) streams: HashMap<TurnId, StreamSegments>,
    pub(super) network_scope: Option<(TurnId, usize, usize)>,
    pub(super) network_status: Option<zevria_session_api::event::NetworkStatus>,
    pub(super) known_tools: KnownTools,
    pub(super) hosted_search: crate::project::HostedSearchProjection,
    pub(super) subtasks: crate::project::SubtaskProjection,
    pub(super) response_usage: Option<ResponseUsageSnapshot>,
    pub(super) context_usage: Option<ContextTokenSnapshot>,
}

impl LiveSession {
    pub(super) fn handle_update(self: &Arc<Self>, update: SessionUpdate) -> Result<(), AcpError> {
        match update {
            SessionUpdate::Streams(batch) => {
                if let Some(root) = batch.root {
                    self.handle_assistant_snapshot(root)?;
                }
            }
            SessionUpdate::Lifecycle(event) => self.handle_event(event)?,
        }
        Ok(())
    }

    pub(super) fn handle_assistant_snapshot(
        &self,
        stream: zevria_session_api::SessionStreamState,
    ) -> Result<(), AcpError> {
        let turn_id = stream.turn_id;
        let updates = {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.turn_is_terminal(turn_id) {
                return Ok(());
            }
            if stream.message.is_none() && stream.attempt.is_none() {
                state
                    .projection
                    .streams
                    .get_mut(&turn_id)
                    .map_or_else(Vec::new, StreamSegments::reset)
            } else {
                let mut updates = stream.attempt.as_ref().map_or_else(Vec::new, |attempt| {
                    state.projection.hosted_search.update(attempt)
                });
                updates.extend(
                    state
                        .projection
                        .streams
                        .entry(turn_id)
                        .or_default()
                        .display_stream_at(
                            turn_id,
                            stream.message.as_deref(),
                            stream.attempt,
                            tokio::time::Instant::now(),
                        ),
                );
                updates
            }
        };
        self.send_updates(updates)
    }

    pub(super) fn handle_event(self: &Arc<Self>, event: SessionEvent) -> Result<(), AcpError> {
        // Question closure and detached child completion are independently
        // correlated, even after the turn that opened them has ended.
        let turn = match &event {
            SessionEvent::Turn(TurnEvent::ResponseAttemptUpdated { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::ModelCallStarted { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Started { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Intermediate { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::ToolResults { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::UsageUpdated { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::ContextUsageUpdated { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Completed { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Recovered { turn_id })
            | SessionEvent::Turn(TurnEvent::Cancelled { turn_id })
            | SessionEvent::Turn(TurnEvent::Failed { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Rejected { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::NetworkStatus { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::Retrying { turn_id, .. })
            | SessionEvent::Job(JobEvent::ContinuationStarted { turn_id, .. })
            | SessionEvent::Job(JobEvent::Reported { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::CompactionStarted { turn_id, .. })
            | SessionEvent::Turn(TurnEvent::CompactionCompleted { turn_id, .. })
            | SessionEvent::Plan(PlanEvent::HandoffStarted { turn_id, .. })
            | SessionEvent::Question(QuestionEvent::Asked { turn_id, .. })
            | SessionEvent::Subtask(SubtaskEvent::Launched { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::Started { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::ReportsReady { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::AgentRunUpdated { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::AgentRunFinished { turn_id, .. }) => {
                Some(*turn_id)
            }
            _ => None,
        };
        if turn.is_some_and(|id| {
            self.state
                .lock()
                .expect("ACP session state poisoned")
                .turn_is_terminal(id)
        }) {
            return Ok(());
        }
        match event {
            SessionEvent::EngineIdle | SessionEvent::Job(JobEvent::ContinuationChanged { .. }) => {}
            SessionEvent::Job(JobEvent::Reported { turn_id, notice })
            | SessionEvent::Job(JobEvent::ContinuationStarted {
                turn_id, notice, ..
            }) => {
                let category = format!("job-notice-{}", notice.jobs()[0].id);
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    &category,
                    notice.summary_lines().join("\n"),
                ))?;
            }
            SessionEvent::Turn(TurnEvent::ResponseAttemptUpdated { turn_id, attempt }) => {
                self.handle_assistant_snapshot(zevria_session_api::SessionStreamState {
                    revision: 0,
                    turn_id,
                    message: None,
                    attempt: Some(Arc::from(attempt)),
                })?;
            }
            // Model management is local TUI traffic, never assistant/tool content.
            SessionEvent::Manage(ManagementEvent::ModelsResult { .. })
            | SessionEvent::Manage(ManagementEvent::McpResult { .. })
            | SessionEvent::Manage(ManagementEvent::McpChanged { .. })
            | SessionEvent::Ensemble(EnsembleEvent::WorkerReviewUpdated { .. })
            | SessionEvent::Ensemble(EnsembleEvent::WorkerControlResult { .. }) => {}
            SessionEvent::Manage(ManagementEvent::ModeResult { request_id, result }) => {
                self.handle_mode_result(request_id, result)?;
            }
            SessionEvent::Turn(TurnEvent::ModeChanged { mode }) => {
                let mode = if matches!(self.plan_state(), PlanWorkflowState::Ready { .. }) {
                    SessionMode::Plan
                } else {
                    mode
                };
                self.select_workflow_mode(mode)?;
            }
            SessionEvent::Manage(ManagementEvent::SkillsResult { request_id, result }) => {
                if let Some(pending) = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .admission
                    .skill_requests
                    .remove(&request_id)
                {
                    let _ = pending.outcome.send(*result);
                }
            }
            SessionEvent::Manage(ManagementEvent::SkillsChanged { revision, counts }) => {
                self.connection
                    .send_notification(crate::skills::SkillsChangedNotification {
                        version: crate::skills::SKILLS_EXTENSION_VERSION,
                        session_id: self.id.clone(),
                        revision,
                        counts,
                    })?;
            }
            SessionEvent::Turn(TurnEvent::ModelCallStarted { turn_id, call }) => {
                let mut state = self.state.lock().expect("ACP session state poisoned");
                if call > 0
                    && state
                        .admission
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == Some(turn_id))
                    && state
                        .projection
                        .network_scope
                        .is_none_or(|(id, previous, _)| id == turn_id && call > previous)
                {
                    state.projection.network_scope = Some((turn_id, call, 0));
                    state.projection.network_status = None;
                }
            }
            SessionEvent::Turn(TurnEvent::Started { turn_id, mode, .. }) => {
                let mut state = self.state.lock().expect("ACP session state poisoned");
                // ModeChanged owns explicit workflow selection. A turn's
                // execution mode is not a replacement for that selection.
                let updates = state
                    .projection
                    .streams
                    .get_mut(&turn_id)
                    .map_or_else(Vec::new, StreamSegments::reset);
                state.projection.network_scope = Some((turn_id, 1, 0));
                state.projection.network_status = None;
                if let Some(pending) = &mut state.admission.pending {
                    pending.turn_id = Some(turn_id);
                    pending.mode = mode;
                }
                drop(state);
                self.send_updates(updates)?;
            }
            SessionEvent::Turn(TurnEvent::Intermediate {
                turn_id,
                message,
                attempt_id,
            }) => {
                let (stream_updates, tool_updates) = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    let streams = state.projection.streams.entry(turn_id).or_default();
                    let mut stream_updates =
                        streams.terminal_bound(turn_id, &message, attempt_id.as_deref());
                    stream_updates.extend(streams.reset());
                    let tool_updates = project_tool_calls(
                        &message,
                        &self.workspace,
                        &mut state.projection.known_tools,
                    );
                    (stream_updates, tool_updates)
                };
                self.send_updates(stream_updates)?;
                self.send_updates(tool_updates)?;
            }
            SessionEvent::Turn(TurnEvent::ToolResults {
                turn_id,
                message,
                metadata,
            }) => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if metadata.iter().any(|metadata| {
                        metadata.tool_name == zevria_foundation::SUBMIT_PLAN_TOOL_NAME
                            && metadata.outcome == ToolCallOutcome::Success
                    }) && let Some(pending) = &mut state.admission.pending
                        && pending.turn_id == Some(turn_id)
                    {
                        pending.successful_plan_submit = true;
                    }
                    let mut updates = state.projection.subtasks.results(&metadata);
                    updates.extend(complete_tool_results(
                        &message,
                        &metadata,
                        &self.workspace,
                        &mut state.projection.known_tools,
                    ));
                    updates
                };
                self.send_updates(updates)?;
            }
            SessionEvent::Turn(TurnEvent::UsageUpdated {
                turn_id: _,
                usage,
                profile,
                model_role,
                input_token_limit,
                context_window_tokens,
            }) => {
                let update = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    state.projection.response_usage = Some(ResponseUsageSnapshot {
                        usage,
                        profile,
                        model_role,
                        input_token_limit,
                        context_window_tokens,
                    });
                    state.projection.context_usage.as_ref().map(|context| {
                        usage_update(state.projection.response_usage.as_ref(), context)
                    })
                };
                if let Some(update) = update {
                    self.send_update(update)?;
                }
            }
            SessionEvent::Turn(TurnEvent::ContextUsageUpdated {
                turn_id: _,
                snapshot,
            }) => {
                let update = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    state.projection.context_usage = Some(*snapshot);
                    usage_update(
                        state.projection.response_usage.as_ref(),
                        state
                            .projection
                            .context_usage
                            .as_ref()
                            .expect("context usage was just installed"),
                    )
                };
                self.send_update(update)?;
            }
            SessionEvent::Turn(TurnEvent::Completed {
                turn_id,
                message,
                attempt_id,
            }) => {
                let updates = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    state
                        .projection
                        .streams
                        .entry(turn_id)
                        .or_default()
                        .terminal_bound(turn_id, &message, attempt_id.as_deref())
                };
                self.send_updates(updates)?;
                self.handle_turn_terminal(turn_id, false, None)?;
            }
            SessionEvent::Turn(TurnEvent::Recovered { turn_id, .. }) => {
                self.handle_turn_terminal(turn_id, false, None)?;
            }
            SessionEvent::Turn(TurnEvent::Cancelled { turn_id }) => {
                self.handle_turn_terminal(turn_id, true, None)?;
            }
            SessionEvent::Turn(TurnEvent::Rejected { turn_id, error }) => {
                self.handle_turn_terminal(turn_id, false, Some(("rejection", error)))?;
            }
            SessionEvent::Turn(TurnEvent::Failed { turn_id, error }) => {
                self.handle_turn_terminal(turn_id, false, Some(("failure", error)))?;
            }
            SessionEvent::Turn(TurnEvent::NetworkStatus {
                turn_id,
                call,
                attempt,
                transport,
                status,
            }) => {
                use zevria_session_api::event::NetworkStatus;
                let report = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if !state.accept_network(turn_id, call, attempt)
                        || state.projection.network_status.as_ref() == Some(&status)
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
                                        state.projection.network_status,
                                        Some(NetworkStatus::Quiet { .. })
                                    )
                            }
                            NetworkStatus::AttemptStarted
                            | NetworkStatus::Connecting
                            | NetworkStatus::AwaitingResponse => attempt > 1,
                        };
                        state.projection.network_status = Some(status.clone());
                        report
                    }
                };
                if report {
                    let message = match status {
                        NetworkStatus::AttemptStarted => {
                            format!("Starting {transport:?} attempt {attempt}")
                        }
                        NetworkStatus::Connecting => {
                            format!("Connecting via {transport:?} · attempt {attempt}")
                        }
                        NetworkStatus::AwaitingResponse => {
                            format!("Awaiting response · attempt {attempt}")
                        }
                        NetworkStatus::Quiet {
                            idle_for,
                            retry_in,
                            will_retry,
                        } => format!(
                            "No response progress for {}s · {} in {}m {}s · attempt {attempt}",
                            idle_for.as_secs(),
                            if will_retry {
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
            SessionEvent::Turn(TurnEvent::Retrying {
                turn_id,
                call,
                attempt,
                retry_after,
                retrying_for,
                error,
            }) => {
                {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if !state.accept_network(turn_id, call, attempt) {
                        return Ok(());
                    }
                    state.projection.network_status = None;
                }
                self.handle_assistant_snapshot(zevria_session_api::SessionStreamState::clear(
                    turn_id,
                ))?;
                self.send_update(retry_diagnostic(
                    turn_id,
                    attempt,
                    retry_after,
                    retrying_for,
                    &error,
                ))?;
            }
            SessionEvent::Plan(PlanEvent::StateChanged { state }) => {
                self.handle_plan_state(*state)?;
            }
            SessionEvent::Plan(PlanEvent::HandoffStarted { turn_id, handoff }) => {
                if self.profile.is_worker() {
                    return Err(invalid_params(
                        "ensemble workers cannot start implementation",
                    ));
                }
                {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if let Some(pending) = &mut state.admission.pending {
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
            SessionEvent::Plan(PlanEvent::FreshHandoffRequested { handoff }) => {
                let error = format!(
                    "Plan {} requested Implement Fresh, which ACP v1 does not support; the durable Ready artifact was left unchanged.",
                    handoff.artifact.version
                );
                self.send_update(diagnostic_update(None, "fresh-plan-unsupported", &error))?;
                self.finish_prompt(Err(invalid_params(error)));
            }
            SessionEvent::Plan(PlanEvent::ProjectionWarning {
                version,
                draft,
                path,
                error,
            }) => {
                self.send_update(diagnostic_update(
                    None,
                    "plan-projection",
                    format!(
                        "Plan {version}{} remains durable, but {} could not be projected: {error}",
                        draft.map_or_else(String::new, |draft| format!(" draft {draft}")),
                        path.display()
                    ),
                ))?;
            }
            SessionEvent::Turn(TurnEvent::CompactionStarted { turn_id, trigger }) => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "compaction-start",
                    format!("Creating a context checkpoint ({trigger:?})."),
                ))?;
            }
            SessionEvent::Turn(TurnEvent::CompactionCompleted {
                turn_id,
                trigger,
                backend,
            }) => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "compaction-complete",
                    format!("Context checkpoint completed ({trigger:?}, {backend:?})."),
                ))?;
            }
            SessionEvent::Question(QuestionEvent::Asked { request, .. }) => {
                self.start_question(request)?;
            }
            SessionEvent::Question(QuestionEvent::Closed { request_id, .. }) => {
                self.elicitations
                    .cancel(&ElicitationKey::Question(request_id));
            }
            SessionEvent::Subtask(SubtaskEvent::Launched {
                call_id,
                entry_index,
                descriptor,
                ..
            }) => {
                let update = self
                    .state
                    .lock()
                    .expect("ACP session state poisoned")
                    .projection
                    .subtasks
                    .launch(&call_id, entry_index, *descriptor);
                self.send_update(update)?;
            }
            SessionEvent::Subtask(SubtaskEvent::Status {
                turn_id,
                id,
                status,
            }) => {
                let update = {
                    let mut state = self.state.lock().expect("ACP session state poisoned");
                    if state.turn_is_terminal(turn_id) {
                        state.projection.subtasks.finish_detached(&id, status)
                    } else {
                        state.projection.subtasks.status(&id, status)
                    }
                };
                if let Some(update) = update {
                    self.send_update(update)?;
                }
            }
            SessionEvent::Subtask(SubtaskEvent::Session { .. })
            | SessionEvent::Job(JobEvent::Updated { .. })
            | SessionEvent::Job(JobEvent::ControlResult { .. }) => {}
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
            SessionEvent::Ensemble(EnsembleEvent::Started { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::ReportsReady { turn_id, .. }) => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "ensemble-recovery",
                    "Recovered durable ensemble workflow state from this transcript.",
                ))?;
            }
            SessionEvent::Ensemble(EnsembleEvent::AgentRunUpdated { turn_id, .. })
            | SessionEvent::Ensemble(EnsembleEvent::AgentRunFinished { turn_id, .. }) => {
                self.send_update(diagnostic_update(
                    Some(turn_id),
                    "ensemble-worker",
                    "Recovered an ensemble worker lifecycle update.",
                ))?;
            }
        }
        Ok(())
    }
}
