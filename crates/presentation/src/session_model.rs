//! One authoritative frontend-neutral reducer for native session panes.
use crate::{
    conversation::{self, ConversationChange, ConversationState, ToolCallStatus},
    session::*,
    workflow::WorkflowState,
};
use rig_core::message::Message;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use zevria_content::UserPrompt;
use zevria_foundation::{
    ModelProfileRef, ModelRole, ReasoningLevel, RequestBehavior, SessionMode, TurnId,
};
use zevria_model::CompactionTrigger;
use zevria_session_api::{
    ControlCommand, ManagementCommand, ModeSelectionResult, SessionEvent, TranscriptEdit,
    TranscriptEditReplacement, TurnCommand,
};
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::{
    AgentRunEvent, AgentRunStatus, EnsembleWorkflow, PlanDecision, PlanVersion, PlanWorkflowState,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PaneScope {
    #[default]
    Root,
    Inspect {
        role: Option<ModelRole>,
    },
}
impl PaneScope {
    pub const fn is_root(self) -> bool {
        matches!(self, Self::Root)
    }
    pub const fn model_role_override(self) -> Option<ModelRole> {
        match self {
            Self::Root => None,
            Self::Inspect { role } => role,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Settlement {
    Completed,
    Recovered,
    Rejected(String),
    Failed(String),
    Cancelled,
}
impl Settlement {
    pub fn restores_draft(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::Failed(_) | Self::Cancelled)
    }
}
#[derive(Debug)]
pub struct ModeSelectionSettled {
    pub request_id: String,
    pub result: ModeSelectionResult,
}
#[derive(Debug, Default)]
pub struct Reduction {
    pub accepted: Option<OperationKind>,
    pub settled: Option<(OperationKind, Settlement)>,
    pub change: ConversationChange,
    pub plan_ready: bool,
    pub mode_selection: Option<ModeSelectionSettled>,
}
#[derive(Debug)]
pub struct PendingEdit {
    pub target_index: usize,
    pub compacted: bool,
}
#[derive(Debug, Default)]
pub struct SessionModel {
    pub session: SessionState,
    pub workflow: WorkflowState,
    pub conversation: ConversationState,
    pub model_profiles: Arc<ConfiguredModelProfiles>,
    scope: PaneScope,
    jobs: crate::jobs::CommandJobsState,
    pending_edit: Option<PendingEdit>,
}
pub struct RestorationInput {
    pub items: Vec<TranscriptItem>,
    pub workflow: zevria_workflow::PlanWorkflowState,
    pub selected_mode: SessionMode,
    pub model_profiles: Vec<(ModelRole, ModelProfileRef)>,
    pub contexts: Vec<zevria_model::ContextTokenSnapshot>,
    pub reasoning: [zevria_foundation::ReasoningLevel; ModelRole::COUNT],
    pub persistence_error: Option<String>,
}

impl SessionModel {
    pub fn jobs(&self) -> &crate::jobs::CommandJobsState {
        &self.jobs
    }
    pub fn running_job_count(&self) -> usize {
        self.jobs.running_count()
    }
    pub fn job_for_call(&self, call: &str) -> Option<&zevria_foundation::CommandJobSnapshot> {
        self.jobs.job_for_call(call)
    }
    pub fn reduce(&mut self, event: SessionEvent) -> Reduction {
        let mut effects = Reduction::default();
        match event {
            SessionEvent::CommandJobUpdated { job } => {
                self.conversation.update_command_job(&job);
                self.jobs.update(job);
            }
            SessionEvent::CommandControlResult { .. } => {}
            SessionEvent::EngineIdle => self.session.observe_engine_idle(),
            SessionEvent::ResponseAttemptUpdated { turn_id, attempt } => {
                if !self.session.accepts_active(turn_id) { return effects; }
                self.conversation.update_native_attempt(attempt, self.session.call_header());
            }
            SessionEvent::ModeResult { request_id, result } => {
                effects.mode_selection = self.accept_mode_selection(&request_id, result);
            }
            SessionEvent::ModeChanged { mode } => self.apply_selected_mode(mode),
            // Management transport belongs to SessionViews. Inspect panes never
            // install root completion metadata or accept management actions.
            SessionEvent::ModelsResult { .. } | SessionEvent::SkillsResult { .. } | SessionEvent::SkillsChanged { .. } | SessionEvent::WorkerControlResult { .. } => {}
            SessionEvent::WorkerReviewUpdated { target, state } => {
                self.conversation.project_worker_review(&target.run_id, &target.worker_id, &state, crate::projection::ProjectionMode::Live);
            }
            SessionEvent::CompactionStarted { turn_id, trigger } => {
                let idle_role = self
                    .scope
                    .model_role_override()
                    .unwrap_or_else(|| role_for_mode(self.session.next_mode()));
                let _ = self
                    .session
                    .compaction_started(turn_id, trigger, idle_role);
            }
            SessionEvent::CompactionCompleted {
                turn_id, trigger, ..
            } => {
                let Some(transition) = self.session.compaction_completed(turn_id, trigger) else {
                    return effects;
                };
                self.conversation.push_compaction_divider();
                if transition.kind.is_transcript_edit()
                    && trigger == CompactionTrigger::AutomaticPreTurn
                    && let Some(edit) = &mut self.pending_edit
                { edit.compacted = true; }
            }
            SessionEvent::TurnStarted {
                turn_id,
                message,
                mode,
            } => {
                let role = self
                    .scope
                    .model_role_override()
                    .unwrap_or_else(|| role_for_mode(mode));
                let Some(start) = self.session.start_turn(turn_id, mode, role) else {
                    return effects;
                };
                effects.accepted = Some(self.session.operation_kind().expect("accepted operation"));
                if start.operation().is_transcript_edit() && let Some(edit) = self.pending_edit.take() {
                    effects.change.merge(self.conversation.commit_edit(edit.target_index, edit.compacted));
                }
                let turn = self.conversation.allocate_turn();
                self.session.bind_display_turn(turn);
                self.conversation.push_user_turn(message, turn);
            }
            SessionEvent::ModelCallStarted { turn_id, call } => {
                let previous = self.session.call_header();
                if self.session.model_call_started(turn_id, call) {
                        let now = self.session.observed_at();
                    self.conversation.finish_header(previous, now);
                    if let Some(turn) = self.session.display_turn() {
                        let header = self.conversation.allocate_call(turn);
                        self.conversation.start_header(header, now);
                        self.session.bind_call_header(header);
                    }
                }
            }
            SessionEvent::EnsembleStarted {
                turn_id,
                start,
                resumed,
            } => {
                let mode = ensemble_mode(start.workflow);
                let role = ensemble_role(start.workflow);
                let Some(transition) = self.session.start_ensemble(turn_id, mode, role) else {
                    return effects;
                };
                effects.accepted = Some(self.session.operation_kind().expect("accepted operation"));
                if transition.operation().is_transcript_edit() && let Some(edit) = self.pending_edit.take() {
                    effects.change.merge(self.conversation.commit_edit(edit.target_index, edit.compacted));
                }
                let turn = self.conversation.ensemble_turn(&start.run_id)
                    .unwrap_or_else(|| self.conversation.allocate_turn());
                self.session.bind_display_turn(turn);
                if !self.conversation.has_ensemble(&start.run_id) {
                    self.conversation.add_ensemble(
                        start,
                        if resumed {
                            AgentRunStatus::Resuming
                        } else {
                            AgentRunStatus::Queued
                        },
                        Some(crate::presentation::NativeHeader::Prompt(turn)),
                    );
                } else if resumed {
                    self.conversation.update_unfinished_ensemble_workers(
                        &start.run_id,
                        AgentRunStatus::Resuming,
                    );
                }
            }
            SessionEvent::AgentRunUpdated {
                ensemble_run_id,
                agent_run_id,
                event,
                ..
            } => {
                // Stable run/worker identity permits late child evidence without
                // changing a newer root stream or retry tail.
                if let AgentRunEvent::Status { status, detail } = event {
                    self.conversation.update_ensemble_worker(
                        &ensemble_run_id,
                        &agent_run_id,
                        status,
                        detail.filter(|_| status.is_terminal()),
                    );
                }
            }
            SessionEvent::AgentRunFinished { ensemble_run_id, outcome, .. } => {
                self.conversation.project_worker_completion(&ensemble_run_id, crate::projection::WorkerCompletion::outcome(&ensemble_run_id, outcome));
            }
            SessionEvent::EnsembleReportsReady { run_id, agents, .. } => {
                for summary in agents {
                    self.conversation.project_worker_completion(&run_id, crate::projection::WorkerCompletion::summary(&run_id, summary));
                }
            }
            SessionEvent::PlanHandoffStarted { turn_id, handoff } => {
                if self.session.start_plan_handoff(turn_id).is_none() {
                    return effects;
                }
                let turn = self.conversation.allocate_turn();
                self.session.bind_display_turn(turn);
                self.conversation.push_plan_handoff(handoff, turn);
                self.session.set_next_mode(SessionMode::Build);
            }
            SessionEvent::PlanStateChanged { state } => effects.plan_ready = self.apply_plan_snapshot(state),
            SessionEvent::PlanProjectionWarning {
                version,
                draft,
                path,
                error,
            } => self.conversation.push_error(format!(
                "Plan {version}{} remains available, but its projection at {} could not be written: {error}",
                draft.map_or_else(String::new, |draft| format!(" draft {draft}")), path.display()
            )),
            SessionEvent::AssistantStreamUpdated { turn_id, snapshot } => {
                if !self.session.preview_progress(turn_id) { return effects; }
                if let Some(attempt) = snapshot.attempt {
                    self.conversation.update_native_attempt(attempt, self.session.call_header());
                } else if let Some(message) = snapshot.message.and_then(conversation::without_tool_calls) {
                    self.session.stream(turn_id, message);
                }
            }
            SessionEvent::StreamCleared { turn_id } => {
                let _ = self.session.clear_stream(turn_id);
            }
            SessionEvent::Intermediate { turn_id, message, attempt_id } => {
                if !self.session.progress(turn_id) {
                    return effects;
                }
                let now = self.session.observed_at();
                let header = self.session.call_header();
                let _ = self.session.clear_stream(turn_id);
                for finished in self.conversation.finish_tool_results(&message, &[]) {
                    self.conversation.finish_header(Some(finished), now);
                }
                self.conversation.commit_response(message, attempt_id.as_deref(), ToolCallStatus::Executing, header);
                // Keep the message's clock live while its commands/tools execute.
                if !header.is_some_and(|header| self.conversation.header_has_executing_tool_calls(header)) {
                    self.conversation.finish_header(header, now);
                }
            }
            SessionEvent::ToolResults {
                turn_id,
                message,
                metadata,
            } => {
                if !self.session.progress(turn_id) {
                    return effects;
                }
                for header in self.conversation.finish_tool_results(&message, &metadata) {
                    self.conversation.finish_header(Some(header), self.session.observed_at());
                }
                for job in self.jobs.iter() { self.conversation.update_command_job(job); }
            }
            SessionEvent::SubtaskLaunched {
                call_id,
                entry_index,
                descriptor,
                ..
            } => {
                self.conversation
                    .attach_subtask_launch(&call_id, entry_index, descriptor);
            }
            SessionEvent::SubtaskStatus { id, status, .. } => {
                self.conversation.update_subtask_status(&id, status);
            }
            SessionEvent::PersistenceChanged { path, error } => {
                let warning = error.map(|error| {
                    format!(
                        "Transcript persistence degraded at {}: {error}",
                        path.display()
                    )
                });
                self.session.set_persistence_error(warning.clone());
                if let Some(warning) = warning {
                    self.conversation.push_error(warning);
                }
            }
            SessionEvent::UsageUpdated {
                turn_id,
                usage,
                profile,
                model_role,
                input_token_limit,
                context_window_tokens,
            } => {
                self.session.update_usage(
                    turn_id,
                    usage,
                    profile,
                    model_role,
                    input_token_limit,
                    context_window_tokens,
                );
            }
            SessionEvent::ContextUsageUpdated { turn_id, snapshot } => {
                self.session.update_context_usage(turn_id, snapshot);
            }
            SessionEvent::FreshPlanHandoffRequested { handoff } => {
                self.session
                    .settle_fresh_handoff_request(handoff.artifact.version);
            }
            SessionEvent::SubtaskSession { .. }
            | SessionEvent::QuestionAsked { .. }
            | SessionEvent::QuestionClosed { .. } => {}
            SessionEvent::TurnCompleted { turn_id, message, attempt_id } => {
                let Some((_kind, header)) = self.finish_turn(turn_id) else {
                    return effects;
                };
                self.pending_edit = None;
                effects.settled = Some((_kind, Settlement::Completed));
                self.conversation.interrupt_executing_tool_calls();
                self.conversation
                    .commit_response(message, attempt_id.as_deref(), ToolCallStatus::Finished, header);
            }
            SessionEvent::TurnRecovered { turn_id, .. } => {
                let Some((_kind, _)) = self.finish_turn(turn_id) else {
                    return effects;
                };
                self.pending_edit = None;
                effects.settled = Some((_kind, Settlement::Recovered));
                self.conversation.interrupt_executing_tool_calls();
            }
            SessionEvent::TurnRejected { turn_id, error } => {
                let Some((kind, _)) = self.finish_turn(turn_id) else {
                    return effects;
                };
                self.pending_edit = None;
                effects.settled = Some((kind, Settlement::Rejected(error.clone())));
                if kind.is_plan_decision() {
                    self.workflow.restore_dialog_after_failure();
                }
                // The old transcript tail is authoritative; no accepted tool
                // execution belongs to this rejected operation.
                self.conversation.push_error(error);
            }
            SessionEvent::TurnFailed { turn_id, error } => {
                let Some((kind, _)) = self.finish_turn(turn_id) else {
                    return effects;
                };
                self.pending_edit = None;
                effects.settled = Some((kind, Settlement::Failed(error.clone())));
                if kind.is_plan_decision() {
                    self.workflow.restore_dialog_after_failure();
                }
                self.conversation.interrupt_executing_tool_calls();
                self.conversation.push_error(error);
            }
            SessionEvent::TurnCancelled { turn_id } => {
                let Some((kind, _)) = self.finish_turn(turn_id) else {
                    return effects;
                };
                self.pending_edit = None;
                effects.settled = Some((kind, Settlement::Cancelled));
                if kind.is_plan_decision() {
                    self.workflow.restore_dialog_after_failure();
                }
                self.conversation.interrupt_executing_tool_calls();
                self.conversation
                    .push_error("Turn cancelled".to_string());
            }
            SessionEvent::NetworkStatus { turn_id, call, attempt, transport, status } => {
                self.session.network_status(turn_id, call, attempt, transport, status);
            }
            SessionEvent::TurnRetrying {
                turn_id,
                call,
                attempt,
                retry_after,
                retrying_for,
                error,
            } => {
                if self.session.accepts_network(turn_id, call, attempt) {
                    self.session.retry(turn_id, attempt, retry_after, retrying_for, error);
                }
            }
        }
        effects
    }

    /// Capture the header before accepted terminal handling clears the active call.
    /// Already settled messages keep their frozen duration; finishing is idempotent.
    fn finish_turn(
        &mut self,
        turn_id: TurnId,
    ) -> Option<(OperationKind, Option<crate::presentation::NativeHeader>)> {
        let header = self.session.call_header();
        let kind = self.session.finish(turn_id)?;
        self.conversation
            .finish_header(header, self.session.observed_at());
        Some((kind, header))
    }

    pub fn apply_plan_snapshot(&mut self, state: zevria_workflow::PlanWorkflowState) -> bool {
        if let zevria_workflow::PlanWorkflowState::Ready { artifact }
        | zevria_workflow::PlanWorkflowState::Published { artifact } = &state
        {
            self.conversation.push_plan_artifact(artifact.clone());
        }
        let ready = matches!(state, zevria_workflow::PlanWorkflowState::Ready { .. });
        let draft_only = self.workflow.is_draft_update(&state);
        let changed = self.workflow.apply_snapshot(state);
        if !draft_only {
            self.session.settle_plan_state(self.workflow.snapshot());
        }
        if ready && changed {
            self.session.set_next_mode(SessionMode::Plan);
        }
        ready && changed
    }
}
pub fn ensemble_mode(workflow: EnsembleWorkflow) -> SessionMode {
    match workflow {
        EnsembleWorkflow::Plan => SessionMode::Plan,
        EnsembleWorkflow::Review => SessionMode::Build,
    }
}

pub const fn ensemble_role(workflow: EnsembleWorkflow) -> ModelRole {
    match workflow {
        EnsembleWorkflow::Plan => ModelRole::Plan,
        EnsembleWorkflow::Review => ModelRole::Review,
    }
}

pub const fn replacement_role(replacement: &TranscriptEditReplacement) -> ModelRole {
    match replacement {
        TranscriptEditReplacement::Message { mode, .. }
        | TranscriptEditReplacement::Skill { mode, .. } => role_for_mode(*mode),
        TranscriptEditReplacement::Ensemble { workflow, .. } => ensemble_role(*workflow),
    }
}

/// Renderers derive their own decoration from this shared live-tail projection.
pub enum ConversationTail<'a> {
    None,
    Compacting {
        elapsed: Duration,
    },
    Waiting {
        elapsed: Duration,
    },
    Streaming {
        message: &'a Message,
        elapsed: Duration,
    },
    Retrying {
        notice: &'a RetryNotice,
        countdown: RetryCountdown,
        retrying_for: Duration,
        elapsed: Duration,
    },
}
impl SessionModel {
    pub fn scope(&self) -> PaneScope {
        self.scope
    }
    pub fn set_scope(&mut self, scope: PaneScope) {
        self.scope = scope;
        if let Some(role) = scope.model_role_override() {
            self.session.reconcile_inspect_role(role);
        }
    }
    pub fn observe_clock(&mut self, now: Instant) {
        self.session.observe_clock(now);
    }
    pub fn apply_stream(
        &mut self,
        stream: &zevria_session_api::event::SessionStreamState,
    ) -> Reduction {
        self.reduce(if stream.message.is_some() || stream.attempt.is_some() {
            SessionEvent::AssistantStreamUpdated {
                turn_id: stream.turn_id,
                snapshot: zevria_content::AssistantStreamSnapshot {
                    message: stream.message.clone(),
                    attempt: stream.attempt.clone(),
                },
            }
        } else {
            SessionEvent::StreamCleared {
                turn_id: stream.turn_id,
            }
        })
    }
    pub fn restore_transcript(&mut self, items: Vec<TranscriptItem>) {
        self.session.reset_for_restore();
        self.workflow.reset();
        self.pending_edit = None;
        self.conversation.restore(items);
        for job in self.jobs.iter() {
            self.conversation.update_command_job(job);
        }
    }
    pub fn restore(&mut self, input: RestorationInput) {
        self.restore_transcript(input.items);
        self.model_profiles = Arc::new(ConfiguredModelProfiles::from_iter(input.model_profiles));
        for role in ModelRole::ALL {
            self.session
                .install_reasoning(role, input.reasoning[role.index()]);
        }
        for snapshot in input.contexts {
            self.session.install_model(
                snapshot.model_role,
                snapshot.profile.clone(),
                Some(snapshot),
            );
        }
        self.apply_plan_snapshot(input.workflow);
        self.apply_selected_mode(input.selected_mode);
        self.session.set_persistence_error(input.persistence_error);
    }
    pub fn apply_selected_mode(&mut self, mode: SessionMode) {
        if self.scope.is_root() {
            self.session.set_next_mode(
                if matches!(self.workflow.snapshot(), PlanWorkflowState::Ready { .. }) {
                    SessionMode::Plan
                } else {
                    mode
                },
            );
        }
    }
    pub fn begin_operation(
        &mut self,
        kind: OperationKind,
        mode: SessionMode,
        role: ModelRole,
    ) -> bool {
        self.scope.is_root() && self.session.begin_operation(kind, mode, role)
    }
    fn admits_work(&self) -> bool {
        self.scope.is_root()
            && !self.is_busy()
            && !matches!(self.workflow.snapshot(), PlanWorkflowState::Ready { .. })
    }
    pub fn submit(&mut self, text: UserPrompt, behavior: RequestBehavior) -> Option<TurnCommand> {
        self.submit_in_mode(text, behavior, self.next_mode())
    }
    pub fn submit_in_mode(
        &mut self,
        text: UserPrompt,
        behavior: RequestBehavior,
        mode: SessionMode,
    ) -> Option<TurnCommand> {
        if !self.admits_work() || text.is_blank() {
            return None;
        }
        self.begin_operation(OperationKind::Submit, mode, role_for_mode(mode))
            .then_some(TurnCommand::Submit {
                text,
                mode,
                behavior,
            })
    }
    pub fn invoke_skill(
        &mut self,
        name: zevria_instructions::SkillName,
        args: UserPrompt,
    ) -> Option<TurnCommand> {
        self.invoke_skill_in_mode(name, args, self.next_mode())
    }
    pub fn invoke_skill_in_mode(
        &mut self,
        name: zevria_instructions::SkillName,
        args: UserPrompt,
        mode: SessionMode,
    ) -> Option<TurnCommand> {
        if !self.admits_work() {
            return None;
        }
        self.begin_operation(OperationKind::Skill, mode, role_for_mode(mode))
            .then_some(TurnCommand::InvokeSkill { name, args, mode })
    }
    pub fn compact(&mut self) -> Option<TurnCommand> {
        if !self.admits_work() {
            return None;
        }
        let mode = self.next_mode();
        self.begin_operation(OperationKind::ManualCompaction, mode, role_for_mode(mode))
            .then_some(TurnCommand::Compact { mode })
    }
    pub fn run_ensemble(
        &mut self,
        workflow: EnsembleWorkflow,
        prompt: UserPrompt,
    ) -> Option<TurnCommand> {
        if !self.admits_work() || prompt.is_blank() {
            return None;
        }
        self.begin_operation(
            OperationKind::Ensemble,
            ensemble_mode(workflow),
            ensemble_role(workflow),
        )
        .then_some(TurnCommand::RunEnsemble { workflow, prompt })
    }
    pub fn resolve_plan(
        &mut self,
        expected: PlanVersion,
        decision: PlanDecision,
    ) -> Option<TurnCommand> {
        let role = if decision == PlanDecision::Revise {
            ModelRole::Plan
        } else {
            ModelRole::Build
        };
        self.begin_operation(
            OperationKind::plan_decision(expected, decision),
            SessionMode::Build,
            role,
        )
        .then_some(TurnCommand::ResolvePlan { expected, decision })
    }
    pub fn stage_edit(&mut self, target_index: usize) {
        self.pending_edit = Some(PendingEdit {
            target_index,
            compacted: false,
        });
    }
    pub fn edit_transcript(
        &mut self,
        edit: TranscriptEdit,
        target_index: usize,
    ) -> Option<TurnCommand> {
        if !self.admits_work()
            || !self.begin_operation(
                OperationKind::TranscriptEdit,
                edit.replacement.mode(),
                replacement_role(&edit.replacement),
            )
        {
            return None;
        }
        self.stage_edit(target_index);
        Some(TurnCommand::EditTranscript(edit))
    }
    pub fn select_mode(&mut self, mode: SessionMode) -> Option<ManagementCommand> {
        if !self.admits_work() || self.workflow.dialog().is_some() {
            return None;
        }
        let current = self.next_mode();
        if !self.begin_operation(
            OperationKind::ModeManagement,
            current,
            role_for_mode(current),
        ) {
            return None;
        }
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let request_id = format!(
            "mode-{}",
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.session.pending_mode_selection = Some(PendingModeSelection {
            request_id: request_id.clone(),
            mode,
        });
        Some(ManagementCommand::SetMode { request_id, mode })
    }
    fn accept_mode_selection(
        &mut self,
        request_id: &str,
        result: ModeSelectionResult,
    ) -> Option<ModeSelectionSettled> {
        let pending = self.session.pending_mode_selection.as_ref()?;
        if !self.scope.is_root()
            || pending.request_id != request_id
            || matches!(&result, ModeSelectionResult::Accepted { mode, .. } if *mode != pending.mode)
        {
            return None;
        }
        self.session.pending_mode_selection = None;
        self.session.finish_mode_management();
        match &result {
            ModeSelectionResult::Accepted { mode, .. } => self.apply_selected_mode(*mode),
            ModeSelectionResult::Rejected { code, message } => self
                .conversation
                .push_error(format!("Mode selection rejected ({code}): {message}")),
        }
        Some(ModeSelectionSettled {
            request_id: request_id.to_string(),
            result,
        })
    }
    pub fn disconnected(&mut self) -> Reduction {
        if let Some(pending) = &self.session.pending_mode_selection {
            let id = pending.request_id.clone();
            return Reduction {
                mode_selection: self.accept_mode_selection(
                    &id,
                    ModeSelectionResult::Rejected {
                        code: "engine_stopped".into(),
                        message: "The session engine stopped before acknowledging mode selection."
                            .into(),
                    },
                ),
                ..Default::default()
            };
        }
        // Finish even an unpromoted pending turn; management has no cancel target.
        if self.session.cancel_target().is_some() {
            return self.reduce(SessionEvent::TurnFailed {
                turn_id: self.active_turn_id().unwrap_or(TurnId::new(0)),
                error: "The session engine stopped.".into(),
            });
        }
        self.session.finish_model_management();
        Reduction::default()
    }
    pub fn cancel(&self) -> Option<ControlCommand> {
        if !self.scope.is_root() {
            return None;
        }
        self.session
            .cancel_target()
            .map(|turn_id| ControlCommand::CancelTurn { turn_id })
    }
    pub fn is_busy(&self) -> bool {
        self.session.is_busy()
    }
    pub fn next_mode(&self) -> SessionMode {
        self.session.next_mode()
    }
    pub fn display_role(&self) -> ModelRole {
        self.session.display_role(self.scope.model_role_override())
    }
    pub fn active_turn_id(&self) -> Option<TurnId> {
        self.session.active_id()
    }
    pub fn accepts_progress(&self, id: TurnId) -> bool {
        self.session.accepts_progress(id)
    }
    pub fn accepts_terminal(&self, id: TurnId) -> bool {
        self.session.accepts_terminal(id)
    }
    pub fn header_timings(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (crate::blocks::NativeHeader, Duration)> + '_ {
        self.conversation.header_timings(now)
    }
    pub fn install_model(
        &mut self,
        role: ModelRole,
        context: &zevria_foundation::ModelContextPolicy,
        snapshot: Option<zevria_model::ContextTokenSnapshot>,
    ) {
        Arc::make_mut(&mut self.model_profiles).set(role, context.profile.clone());
        self.session
            .install_model(role, context.profile.clone(), snapshot);
    }
    pub fn install_reasoning(&mut self, role: ModelRole, level: ReasoningLevel) {
        self.session.install_reasoning(role, level);
    }
    pub fn session_models(&self) -> anyhow::Result<zevria_model::models::SessionModels> {
        use zevria_model::models::{ModelSelection, SessionModels, mode_role};
        anyhow::ensure!(
            self.scope.is_root(),
            "only the root owns session model selections"
        );
        let selection = |mode| -> anyhow::Result<ModelSelection> {
            let role = mode_role(mode);
            let profile = self.model_profiles.get(role).ok_or_else(|| {
                anyhow::anyhow!("cannot replace session: missing committed {mode} model profile")
            })?;
            let reasoning = self.session.reasoning(role).ok_or_else(|| {
                anyhow::anyhow!("cannot replace session: missing committed {mode} reasoning level")
            })?;
            Ok(ModelSelection::new(profile.clone(), reasoning))
        };
        SessionModels::new(
            selection(SessionMode::Build)?,
            selection(SessionMode::Plan)?,
        )
    }
    pub fn tail(&self, now: Instant) -> ConversationTail<'_> {
        ConversationTail::observe(&self.session, now)
    }
}
impl<'a> ConversationTail<'a> {
    pub fn observe(session: &'a SessionState, now: Instant) -> Self {
        let elapsed = session.elapsed().unwrap_or_default();
        let pending_manual = session.operation_kind() == Some(OperationKind::ManualCompaction)
            && matches!(session.activity(), SessionActivity::Pending(_));
        if session.is_compacting() || pending_manual {
            return ConversationTail::Compacting { elapsed };
        }
        match session.activity() {
            SessionActivity::Idle => ConversationTail::None,
            SessionActivity::Pending(pending)
                if pending.kind.is_plan_decision()
                    || pending.kind == OperationKind::ModeManagement =>
            {
                ConversationTail::None
            }
            SessionActivity::Pending(_) => ConversationTail::Waiting { elapsed },
            SessionActivity::Active(active) => match &active.phase {
                ActivePhase::Compacting { .. } => ConversationTail::Compacting { elapsed },
                ActivePhase::AwaitingStart | ActivePhase::Running(TurnTail::Waiting) => {
                    ConversationTail::Waiting { elapsed }
                }
                ActivePhase::Running(TurnTail::Streaming(message)) => {
                    ConversationTail::Streaming { message, elapsed }
                }
                ActivePhase::Running(TurnTail::Retrying(notice)) => ConversationTail::Retrying {
                    notice,
                    countdown: notice.countdown(now),
                    retrying_for: notice.recovery_remaining(now),
                    elapsed,
                },
            },
        }
    }
}
