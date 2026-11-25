//! Plan publication and correlated decision transitions.

use super::{
    AcpError, Arc, HashSet, LiveSession, PlanArtifact, PlanChoice, PlanDecision, PlanVersion,
    PlanWorkflowState, PromptPhase, SessionCommand, SessionMode, StopReason, TurnId,
    diagnostic_update, invalid_params, plan_artifact_update, plan_ready_instructions,
    worker_plan_update,
};

pub(super) struct PlanDecisionCoordinator {
    pub(super) state: PlanWorkflowState,
    pub(super) emitted_plans: HashSet<PlanVersion>,
}

impl LiveSession {
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
                .plan
                .emitted_plans
                .insert(artifact.version);
        }
        self.publish_plan(&artifact)
    }

    pub(super) fn publish_plan(&self, artifact: &PlanArtifact) -> Result<(), AcpError> {
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

    pub(super) fn require_plan_capability(&self) -> Result<(), AcpError> {
        if self.profile.is_worker() && !self.profile.plan_operations {
            return Err(invalid_params(
                "ensemble Plan workers require the ACP client Plan-operation capability",
            ));
        }
        Ok(())
    }

    pub(super) fn handle_turn_terminal(
        self: &Arc<Self>,
        turn_id: TurnId,
        cancelled: bool,
        failure: Option<(&str, String)>,
    ) -> Result<(), AcpError> {
        let mut start_decision = None;
        let mut updates = Vec::new();
        let outcome = 'terminal: {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            if state.turn_is_terminal(turn_id) {
                return Ok(());
            }
            state.admission.last_terminal_turn = Some(turn_id);
            if let Some(streams) = state.projection.streams.get_mut(&turn_id) {
                streams.flush_preview(tokio::time::Instant::now(), &mut updates);
            }
            state.projection.streams.retain(|&id, _| id > turn_id);
            if let Some((category, error)) = &failure {
                updates.push(diagnostic_update(Some(turn_id), category, error));
            }
            state.projection.known_tools.clear();
            state.projection.hosted_search.clear();
            state.projection.subtasks.prune_completed();
            if state
                .projection
                .network_scope
                .is_some_and(|(id, _, _)| id == turn_id)
            {
                state.projection.network_scope = None;
                state.projection.network_status = None;
            }
            let ready_artifact = match &state.plan.state {
                PlanWorkflowState::Ready { artifact } => Some(artifact.clone()),
                _ => None,
            };
            let Some(pending) = &mut state.admission.pending else {
                break 'terminal None;
            };
            if pending.turn_id.is_some_and(|id| id != turn_id)
                || (pending.turn_id.is_none() && failure.is_none() && !cancelled)
            {
                break 'terminal None;
            }
            if cancelled || pending.cancel_requested {
                Some(Ok(StopReason::Cancelled))
            } else if let Some((_, error)) = failure {
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
        self.send_updates(updates)?;
        if let Some(outcome) = outcome {
            self.finish_prompt(outcome);
        }
        if let Some(artifact) = start_decision {
            self.start_plan_decision(artifact)?;
        }
        Ok(())
    }

    pub(super) fn handle_plan_state(
        self: &Arc<Self>,
        plan: PlanWorkflowState,
    ) -> Result<(), AcpError> {
        let mut command = None;
        let mut complete = None;
        let mut ready = None;
        let mut mode_changed = false;
        {
            let mut state = self.state.lock().expect("ACP session state poisoned");
            let entering_planning = !matches!(state.plan.state, PlanWorkflowState::Planning { .. });
            state.plan.state = plan.clone();
            match &plan {
                PlanWorkflowState::Planning { .. } if entering_planning => {
                    if let Some(pending) = &mut state.admission.pending {
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
                    mode_changed = state.admission.mode != SessionMode::Plan;
                    state.admission.mode = SessionMode::Plan;
                    if state.plan.emitted_plans.insert(artifact.version) {
                        ready = Some(artifact.clone());
                    }
                    if let Some(pending) = &mut state.admission.pending
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
                    if state.plan.emitted_plans.insert(artifact.version) {
                        ready = Some(artifact.clone());
                    }
                    complete = Some(Ok(StopReason::EndTurn));
                }
                PlanWorkflowState::Idle
                | PlanWorkflowState::Resolved { .. }
                | PlanWorkflowState::Planning { .. } => {}
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
                .admission
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

    pub(super) fn apply_plan_choice(
        &self,
        version: PlanVersion,
        choice: PlanChoice,
    ) -> Result<(), AcpError> {
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
                    .admission
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
                    .admission
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
}
