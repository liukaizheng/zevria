//! Serialized input, mode and management admission.

use super::{
    AcpError, AcpSessionUpdate, Arc, CancellationToken, CurrentModeUpdate, HashMap, LiveSession,
    ModeSelectionResult, Ordering, PlanDecision, PlanWorkflowState, SessionCommand, SessionMode,
    StopReason, TurnId, invalid_params, mode_id, oneshot,
};

pub(super) struct AdmissionState {
    pub(super) mode: SessionMode,
    pub(super) pending: Option<PendingPrompt>,
    pub(super) preparing: Option<(u64, CancellationToken)>,
    pub(super) pending_mode: Option<PendingMode>,
    pub(super) skill_requests: HashMap<String, PendingSkillRequest>,
    pub(super) last_terminal_turn: Option<TurnId>,
    pub(super) unavailable: Option<String>,
}

pub(super) struct PendingMode {
    pub(super) request_id: String,
    pub(super) mode: SessionMode,
    // The waiter can leave before the engine acknowledges its authoritative selection.
    // Correlation and the admission lock outlive that local RPC request.
    pub(super) outcome: Option<oneshot::Sender<Result<(), AcpError>>>,
}

impl PendingMode {
    pub(super) fn resolve_waiter(&mut self, result: Result<(), AcpError>) {
        if let Some(outcome) = self.outcome.take() {
            let _ = outcome.send(result);
        }
    }
}

pub(super) struct PendingSkillRequest {
    pub(super) mutation: bool,
    pub(super) outcome: oneshot::Sender<zevria_instructions::skill::SkillManagementResult>,
}

pub(super) struct PendingPrompt {
    pub(super) outcome: oneshot::Sender<Result<StopReason, AcpError>>,
    pub(super) turn_id: Option<TurnId>,
    pub(super) mode: SessionMode,
    pub(super) phase: PromptPhase,
    pub(super) successful_plan_submit: bool,
    pub(super) completed_plan_turn: bool,
    pub(super) cancel_requested: bool,
}

#[derive(Debug)]
pub(super) enum PromptPhase {
    Normal,
    WaitingForRevision {
        command: Box<zevria_session_api::TurnCommand>,
    },
    AwaitingReady,
    AwaitingDecision,
    DecisionRevise,
    AwaitingImplementation,
}

pub(super) enum SessionInput {
    Prompt {
        text: zevria_content::UserPrompt,
        behavior: zevria_foundation::RequestBehavior,
    },
    Skill {
        name: zevria_instructions::SkillName,
        args: zevria_content::UserPrompt,
    },
}

impl SessionInput {
    fn content(&self) -> &zevria_content::UserPrompt {
        match self {
            Self::Prompt { text, .. } => text,
            Self::Skill { args, .. } => args,
        }
    }

    fn behavior(&self) -> zevria_foundation::RequestBehavior {
        match self {
            Self::Prompt { behavior, .. } => *behavior,
            Self::Skill { .. } => zevria_foundation::RequestBehavior::Standard,
        }
    }

    fn into_command(self, mode: SessionMode) -> zevria_session_api::TurnCommand {
        match self {
            Self::Prompt { text, behavior } => zevria_session_api::TurnCommand::Submit {
                text,
                mode,
                behavior,
            },
            Self::Skill { name, args } => {
                zevria_session_api::TurnCommand::InvokeSkill { name, args, mode }
            }
        }
    }
}

pub(crate) struct PromptWait {
    pub response: oneshot::Receiver<Result<StopReason, AcpError>>,
}

/// Reserves request order and the idle/management lock without beginning an
/// engine turn. Dropping a failed/cancelled preparation releases the reservation.
pub(crate) struct InputPreparation {
    live: Arc<LiveSession>,
    id: u64,
    cancellation: CancellationToken,
}
impl InputPreparation {
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
    pub(crate) fn interrupted_error(&self) -> AcpError {
        let state = self.live.state.lock().expect("ACP session state poisoned");
        state
            .admission
            .unavailable
            .as_ref()
            .map_or_else(AcpError::request_cancelled, |error| {
                AcpError::internal_error().data(error.clone())
            })
    }
    pub(crate) fn begin_prompt(
        self,
        prompt: zevria_content::UserPrompt,
        behavior: zevria_foundation::RequestBehavior,
    ) -> Result<PromptWait, AcpError> {
        self.live.begin_input(
            SessionInput::Prompt {
                text: prompt,
                behavior,
            },
            Some(self.id),
        )
    }
    pub(crate) fn begin_skill(
        self,
        name: zevria_instructions::skill::SkillName,
        args: zevria_content::UserPrompt,
    ) -> Result<PromptWait, AcpError> {
        self.live.check_skill_input(&args)?;
        self.live
            .begin_input(SessionInput::Skill { name, args }, Some(self.id))
    }
}
impl Drop for InputPreparation {
    fn drop(&mut self) {
        let mut state = self.live.state.lock().expect("ACP session state poisoned");
        if state
            .admission
            .preparing
            .as_ref()
            .is_some_and(|(id, _)| *id == self.id)
        {
            state.admission.preparing = None;
        }
    }
}

pub(crate) struct ModeWait {
    pub request_id: String,
    pub response: oneshot::Receiver<Result<(), AcpError>>,
}

impl LiveSession {
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
            if let Some(error) = &state.admission.unavailable {
                return Err(AcpError::internal_error().data(error.clone()));
            }
            if self.is_closed() {
                return Err(invalid_params("session is closed"));
            }
            if state.admission.pending.is_some() || state.admission.preparing.is_some() {
                return Err(invalid_params("cannot change mode during an active prompt"));
            }
            if state.admission.pending_mode.is_some() || !state.admission.skill_requests.is_empty()
            {
                return Err(invalid_params(
                    "cannot change mode while session management is pending",
                ));
            }
            if matches!(state.plan.state, PlanWorkflowState::Ready { .. })
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
                state.admission.mode = mode;
                drop(state);
                self.publish_mode(mode)?;
                let _ = outcome.send(Ok(()));
                return Ok(ModeWait {
                    request_id,
                    response,
                });
            }
            state.admission.pending_mode = Some(PendingMode {
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
        if let Some(pending) =
            state.admission.pending_mode.as_mut().filter(|pending| {
                request_id.is_none_or(|request_id| pending.request_id == request_id)
            })
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
    pub(super) fn discard_mode(&self, request_id: Option<&str>, error: AcpError) {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if state
            .admission
            .pending_mode
            .as_ref()
            .is_some_and(|pending| {
                request_id.is_none_or(|request_id| pending.request_id == request_id)
            })
            && let Some(mut pending) = state.admission.pending_mode.take()
        {
            pending.resolve_waiter(Err(error));
        }
    }

    pub(super) fn publish_mode(&self, mode: SessionMode) -> Result<(), AcpError> {
        self.send_update(AcpSessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            mode_id(mode, self.profile.execution),
        )))
    }

    pub(super) fn select_workflow_mode(&self, mode: SessionMode) -> Result<(), AcpError> {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if state.admission.mode != mode {
            state.admission.mode = mode;
            drop(state);
            self.publish_mode(mode)?;
        }
        Ok(())
    }

    pub(super) fn handle_mode_result(
        &self,
        request_id: String,
        result: ModeSelectionResult,
    ) -> Result<(), AcpError> {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if !state
            .admission
            .pending_mode
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            // Unmatched or already-resolved replies cannot overwrite a newer
            // selection. An abandoned waiter still has a correlated pending
            // entry and must reconcile the engine's authoritative result below.
            return Ok(());
        }
        let pending = state
            .admission
            .pending_mode
            .as_mut()
            .expect("matched pending mode");
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
        let mut pending = state
            .admission
            .pending_mode
            .take()
            .expect("matched pending mode");
        match result {
            ModeSelectionResult::Accepted { mode, .. } => {
                state.admission.mode = mode;
                drop(state);
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
            if state.admission.pending_mode.is_some() {
                return Err(invalid_params(
                    "mode selection is pending; skill request was not queued",
                ));
            }
            if request.is_mutation()
                && (state.admission.pending.is_some() || state.admission.preparing.is_some())
            {
                return Err(invalid_params(
                    "skill mutations require an idle session; request was not queued",
                ));
            }
            if state.admission.skill_requests.len() >= 32 {
                return Err(invalid_params("too many pending skill requests"));
            }
            state.admission.skill_requests.insert(
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
                .admission
                .skill_requests
                .remove(&id);
            return Err(error);
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await;
        self.state
            .lock()
            .expect("ACP session state poisoned")
            .admission
            .skill_requests
            .remove(&id);
        match outcome {
            Ok(Ok(result)) => Ok(crate::skills::SkillsResponse { version: crate::skills::SKILLS_EXTENSION_VERSION, result }),
            _ => Err(AcpError::internal_error().data("skill request interrupted or timed out; query the installed revision before retrying a mutation")),
        }
    }

    pub(crate) fn reserve_input_preparation(
        self: &Arc<Self>,
    ) -> Result<InputPreparation, AcpError> {
        let mut state = self.state.lock().expect("ACP session state poisoned");
        if let Some(error) = &state.admission.unavailable {
            return Err(AcpError::internal_error().data(error.clone()));
        }
        if self.is_closed() {
            return Err(invalid_params("session is closed"));
        }
        if state.admission.pending.is_some() || state.admission.preparing.is_some() {
            return Err(invalid_params(
                "this session already has an in-flight prompt",
            ));
        }
        if state.admission.pending_mode.is_some()
            || state
                .admission
                .skill_requests
                .values()
                .any(|request| request.mutation)
        {
            return Err(invalid_params(
                "session management is pending; prompt was not queued",
            ));
        }
        let id = self.next_management_request.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        state.admission.preparing = Some((id, cancellation.clone()));
        Ok(InputPreparation {
            live: Arc::clone(self),
            id,
            cancellation,
        })
    }

    pub(super) fn check_skill_input(
        &self,
        args: &zevria_content::UserPrompt,
    ) -> Result<(), AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params("skills are disabled for ensemble workers"));
        }
        if args.text_len() > 64 * 1024 {
            return Err(invalid_params("skill arguments exceed 64 KiB"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn begin_prompt(
        &self,
        text: zevria_content::UserPrompt,
    ) -> Result<PromptWait, AcpError> {
        self.begin_prompt_with_behavior(text, zevria_foundation::RequestBehavior::Standard)
    }

    #[cfg(test)]
    pub(crate) fn begin_prompt_with_behavior(
        &self,
        text: zevria_content::UserPrompt,
        behavior: zevria_foundation::RequestBehavior,
    ) -> Result<PromptWait, AcpError> {
        self.begin_input(SessionInput::Prompt { text, behavior }, None)
    }

    pub(super) fn begin_input(
        &self,
        input: SessionInput,
        reservation: Option<u64>,
    ) -> Result<PromptWait, AcpError> {
        let prompt = input.content();
        let behavior = input.behavior();
        let skill = matches!(&input, SessionInput::Skill { .. });
        prompt
            .validate()
            .map_err(|error| invalid_params(error.to_string()))?;
        let implement = behavior == zevria_foundation::RequestBehavior::Standard
            && !skill
            && !prompt.has_images()
            && prompt.text_projection().trim() == "/implement";
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
            if let Some(error) = &state.admission.unavailable {
                return Err(AcpError::internal_error().data(error.clone()));
            }
            if let Some(id) = reservation {
                match &state.admission.preparing {
                    Some((current, token)) if *current == id && !token.is_cancelled() => {}
                    _ => return Err(AcpError::request_cancelled()),
                }
            } else if state.admission.preparing.is_some() {
                return Err(invalid_params(
                    "this session already has an in-flight prompt",
                ));
            }
            if state.admission.pending.is_some() {
                return Err(invalid_params(
                    "this session already has an in-flight prompt",
                ));
            }
            if self.is_closed() {
                return Err(invalid_params("session is closed"));
            }
            if state.admission.pending_mode.is_some()
                || state
                    .admission
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
                    || state.admission.mode != SessionMode::Build
                    || matches!(
                        state.plan.state,
                        PlanWorkflowState::Ready { .. } | PlanWorkflowState::Planning { .. }
                    ))
            {
                return Err(invalid_params(
                    "zevria.orchestration requires root Build without a pending Plan workflow; it is not Plan approval",
                ));
            }
            let blank_text = !skill && prompt.is_blank();
            if self.profile.is_worker() {
                if matches!(
                    prompt.text_projection().split_whitespace().next(),
                    Some("/build" | "/plan")
                ) {
                    return Err(invalid_params(
                        "root mode commands are unavailable to ensemble workers; use the structured Plan/Review mode API",
                    ));
                }
                if skill || implement {
                    return Err(invalid_params(
                        "ensemble workers are report-only; skills and implementation are disabled",
                    ));
                }
                if state.admission.mode == SessionMode::Plan {
                    self.require_plan_capability()?;
                }
                if blank_text {
                    return Err(invalid_params("worker feedback must not be empty"));
                }
            }
            let input_mode = if matches!(state.plan.state, PlanWorkflowState::Ready { .. }) {
                SessionMode::Plan
            } else {
                state.admission.mode
            };
            let input = input.into_command(input_mode);
            let (phase, mode, command) = match &state.plan.state {
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
                _ => (PromptPhase::Normal, state.admission.mode, input),
            };
            state.admission.preparing = None;
            state.admission.pending = Some(PendingPrompt {
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
            if let Some((_, cancellation)) = &state.admission.preparing {
                cancellation.cancel();
            }
            let Some(pending) = &mut state.admission.pending else {
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

    pub(super) fn finish_prompt(&self, outcome: Result<StopReason, AcpError>) {
        let pending = self
            .state
            .lock()
            .expect("ACP session state poisoned")
            .admission
            .pending
            .take();
        if let Some(pending) = pending {
            let _ = pending.outcome.send(outcome);
        }
    }
}
