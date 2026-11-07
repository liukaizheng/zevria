//! Public TUI façade and cross-domain input/event coordination.

mod conversation;
mod draft_submission;
mod edit;
mod fold;
#[cfg(test)]
mod image_tests;
mod interaction;
mod keys;
mod mode;
pub(crate) mod overlays;
mod pane;
mod queue;
pub(crate) use queue::QueueView;
use queue::{FollowUpQueue, MAX_QUEUED_FOLLOW_UPS, QueuePause, QueuedFollowUp, QueuedWork};
mod render_state;
mod session;
mod submission;
pub(crate) use render_state::{
    ComposerChrome, ConversationTail, HeaderTimings, PlanDialogView, RenderParts,
};
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod undo_tests;
mod view;
mod worker_review;
#[cfg(test)]
mod worker_review_tests;
mod workflow;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::Event;
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
#[cfg(test)]
use rig_core::message::Message;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::ModelRole;
use zevria_foundation::QuestionRequestId;
use zevria_foundation::QuestionResponse;
use zevria_foundation::SessionMode;
use zevria_foundation::TurnId;
use zevria_foundation::subtask::SubtaskId;
use zevria_instructions::SkillMeta;
use zevria_session_api::SessionEvent;
use zevria_session_api::TranscriptEdit;
use zevria_session_api::TranscriptEditReplacement;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::AgentRunId;
use zevria_workflow::AgentRunStatus;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanVersion;

use crate::command::{ClassifiedInput, SlashCommand};
use crate::completion::{
    CompletionAcceptance, CompletionKind, CompletionMenu, CompletionView, FileCompletionRequest,
    FileSearchStatus,
};
use crate::composer::ComposerState;
use crate::input::{InputEvent, Surface, SurfaceKind, UserInput};
use zevria_workflow::ensemble_review::*;

pub(crate) use conversation::{
    ConversationChange, ConversationState, EnsembleHistory, HistoryEntry, Selection, ToolCallState,
    ToolCallStatus, TurnStartTarget,
};
use edit::{EditState, RecallEdit};
pub(crate) use fold::{EntryFolds, FoldKey, FoldState, SpanRole, TurnFold};
#[cfg(test)]
pub(crate) use interaction::FocusState;
pub(crate) use interaction::{ActiveSelection, SelectionScope};
use interaction::{InteractionState, SelectionEntry};
#[cfg(test)]
use ratatui::crossterm::event::KeyEventKind;

use pane::PaneState;
pub(crate) use session::{
    ActivePhase, ConfiguredModelProfiles, OperationKind, RetryCountdown, SessionActivity, TurnTail,
    role_for_mode,
};
use view::ViewState;
pub(crate) use workflow::{PlanChoice, PlanDialogState};
use workflow::{PlanIntent, WorkflowKeys};

use crate::status::{StatusAccent, StatusBarView, StatusTone};

#[cfg(test)]
pub(crate) use session::RetryNotice;

const SCROLL_STEP: usize = 1;

/// Side effect requested by the pure [`App`] input reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    WorkerControl(WorkerControl),
    SetMode {
        request_id: String,
        mode: SessionMode,
    },
    ReadClipboard {
        generation: u64,
        cursor: usize,
    },
    Submit {
        text: zevria_content::UserPrompt,
        mode: SessionMode,
        behavior: zevria_foundation::RequestBehavior,
    },
    EditTranscript(TranscriptEdit),
    InvokeSkill {
        name: zevria_instructions::SkillName,
        args: zevria_content::UserPrompt,
        mode: SessionMode,
    },
    Compact {
        mode: SessionMode,
    },
    CancelTurn {
        turn_id: Option<TurnId>,
    },
    AnswerQuestion {
        request_id: QuestionRequestId,
        response: QuestionResponse,
    },
    Copy {
        text: String,
    },
    OpenSubtask {
        id: SubtaskId,
    },
    OpenAgentRun {
        id: AgentRunId,
    },
    RunEnsemble {
        workflow: EnsembleWorkflow,
        prompt: zevria_content::UserPrompt,
    },
    RunCommand(SlashCommand),
    ResumeSession {
        path: PathBuf,
    },
    ResolvePlan {
        expected: PlanVersion,
        decision: PlanDecision,
    },
    Quit,
}

/// Runtime-only consequences of reducing an engine event.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AppEffect {
    PruneEnsembleRuns(Vec<EnsembleRunId>),
}

pub use zevria_presentation::RestorationInput;

use zevria_presentation::{PaneScope, Reduction, SessionModel};

/// Public façade over cohesive, independently valid state machines.
#[derive(Default)]
pub struct App {
    model: SessionModel,
    folds: FoldState,
    composer: ComposerState,
    interaction: InteractionState,
    edit: EditState,
    pane: PaneState,
    worker: worker_review::WorkerReviewUiState,
    drafts: draft_submission::DraftSubmissions,
    queue: FollowUpQueue,
    mode_command_draft: Option<(crate::composer::ComposerDraft, u64)>,
    pane_id: crate::input::PaneId,
    view: ViewState,
    help: Option<crate::hints::HelpOverlay>,
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn subtask_inspect(title: impl Into<String>) -> Self {
        Self {
            pane: PaneState::subtask_inspect(title, ModelRole::Explore),
            model: {
                let mut model = SessionModel::default();
                model.set_scope(PaneScope::Inspect {
                    role: Some(ModelRole::Explore),
                });
                model
            },
            ..Self::default()
        }
    }

    pub fn set_inspect_model_role(&mut self, role: ModelRole) {
        if self.pane.set_inspect_model_role(role) {
            self.model
                .set_scope(PaneScope::Inspect { role: Some(role) });
        }
    }

    pub(crate) fn acp_inspect(title: impl Into<String>) -> Self {
        Self {
            pane: PaneState::acp_inspect(title),
            model: {
                let mut model = SessionModel::default();
                model.set_scope(PaneScope::Inspect { role: None });
                model
            },
            ..Self::default()
        }
    }

    pub(crate) fn bind_worker_review(
        &mut self,
        target: WorkerControlTarget,
        state: Box<WorkerReviewState>,
    ) {
        self.view.invalidate_rendered_geometry();
        if self.worker.bind(target, state, &mut self.composer) {
            self.view.retire_turn_alignment();
            self.pane.freeze_worker();
        } else {
            self.pane.enable_worker();
        }
        self.composer.worker_commands();
        self.leave_insert_if_composer_locked();
    }

    pub(crate) fn freeze_worker(&mut self) {
        self.view.retire_turn_alignment();
        self.view.invalidate_rendered_geometry();
        self.composer.cancel_paste();
        self.pane.freeze_worker();
        self.worker.retire();
        self.leave_insert_if_composer_locked();
    }

    pub(crate) fn worker_control_result(&mut self, result: &WorkerControlResult) {
        if let Some(error) = self
            .worker
            .settle(result, &mut self.composer, &mut self.drafts)
        {
            self.view.invalidate_rendered_geometry();
            if let Some(error) = error {
                self.push_error(error);
            }
        }
    }

    pub(crate) fn worker_action(&mut self, action: WorkerControlAction) -> Option<UiAction> {
        if !self.pane.is_worker() {
            return None;
        }
        self.worker
            .admit(action, &self.composer)
            .map(UiAction::WorkerControl)
    }

    fn confirm_worker(&mut self, command: bool) -> Option<UiAction> {
        if !command && !self.composer.is_empty() {
            self.push_error("Unsent worker draft: send or clear it before confirming.".into());
            return None;
        }
        let revision = self
            .worker
            .bound
            .as_ref()?
            .1
            .eligible_snapshot()
            .map(|snapshot| snapshot.revision.clone());
        let Some(expected_revision) = revision else {
            self.push_error("This worker has no quiescent, eligible proposal. Finish queued feedback and request a fresh complete Markdown publication.".into());
            return None;
        };
        self.worker_action(WorkerControlAction::Confirm { expected_revision })
    }

    fn submit_worker(&mut self) -> Option<UiAction> {
        if !self.pane.is_worker() || self.composer.is_paste_pending() || self.composer.is_blank() {
            return None;
        }
        let classified = match self.composer.classify() {
            Ok(classified) => classified,
            Err(error) => {
                self.push_error(error.to_string());
                return None;
            }
        };
        match classified {
            ClassifiedInput::Builtin(SlashCommand::Confirm) => self.confirm_worker(true),
            ClassifiedInput::Builtin(SlashCommand::Baseline) => {
                let revision = self
                    .worker
                    .bound
                    .as_ref()?
                    .1
                    .eligible_snapshot()
                    .map(|snapshot| snapshot.revision.clone());
                let Some(expected_revision) = revision else {
                    self.push_error(
                        "This worker has no quiescent, eligible proposal to mark as baseline."
                            .into(),
                    );
                    return None;
                };
                self.worker_action(WorkerControlAction::Baseline { expected_revision })
            }
            ClassifiedInput::Builtin(SlashCommand::Unbaseline) => {
                let revision = self
                    .worker
                    .bound
                    .as_ref()?
                    .1
                    .confirmed_plan()
                    .map(|plan| plan.snapshot.revision);
                let Some(expected_revision) = revision else {
                    self.push_error("This worker has no confirmed baseline proposal.".into());
                    return None;
                };
                self.worker_action(WorkerControlAction::Unbaseline { expected_revision })
            }
            ClassifiedInput::Builtin(SlashCommand::Unconfirm) => {
                let expected_revision = self
                    .worker
                    .bound
                    .as_ref()?
                    .1
                    .confirmation
                    .as_ref()?
                    .revision
                    .clone();
                self.worker_action(WorkerControlAction::Unconfirm { expected_revision })
            }
            ClassifiedInput::Builtin(SlashCommand::Retry) => {
                self.worker_action(WorkerControlAction::Retry)
            }
            ClassifiedInput::Builtin(SlashCommand::CancelPrompt) => {
                self.worker_action(WorkerControlAction::CancelPrompt)
            }
            ClassifiedInput::Builtin(SlashCommand::Abandon) => {
                self.worker_action(WorkerControlAction::Abandon)
            }
            ClassifiedInput::Message(text) => {
                let action = self.worker_action(WorkerControlAction::SendFeedback { text })?;
                self.close_composer_edit_group();
                self.interaction.enter_normal();
                Some(action)
            }
            _ => None,
        }
    }

    pub(crate) fn ensemble_baseline(
        &self,
        run_id: &zevria_workflow::EnsembleRunId,
    ) -> Option<&zevria_workflow::AgentRunId> {
        self.model.conversation.ensemble_baseline(run_id)
    }

    pub(crate) fn reviewed_worker_status(
        &self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
    ) -> Option<AgentRunStatus> {
        self.model
            .conversation
            .reviewed_worker_status(run_id, worker_id)
    }

    pub(crate) fn update_pane_metadata(
        &mut self,
        title: impl Into<String>,
        external_context: Option<crate::status::ExternalContextUsage>,
    ) {
        let mut title = title.into();
        if let Some((_, state)) = &self.worker.bound {
            if let Some(snapshot) = &state.retained {
                title.push_str(&format!(" · proposal r{}", snapshot.revision.revision));
            }
            if !state.pending.is_empty() {
                title.push_str(&format!(" · {} queued", state.pending.len()));
            }
            if !state.connected && !state.sealed && !state.abandoned {
                title.push_str(" · disconnected");
            }
        }
        let _ = self.pane.update_metadata(title, external_context);
    }

    /// Seed configured provider/model identities for status display before the
    /// first authoritative usage event arrives.
    pub fn with_model_profiles(
        mut self,
        profiles: impl IntoIterator<Item = (ModelRole, ModelProfileRef)>,
    ) -> Self {
        self.model.model_profiles = Arc::new(ConfiguredModelProfiles::from_iter(profiles));
        self
    }

    pub fn with_reasoning_levels(
        mut self,
        levels: [zevria_foundation::ReasoningLevel; ModelRole::COUNT],
    ) -> Self {
        for role in ModelRole::ALL {
            self.model
                .session
                .install_reasoning(role, levels[role.index()]);
        }
        self
    }

    pub(crate) fn with_shared_model_profiles(
        mut self,
        profiles: Arc<ConfiguredModelProfiles>,
    ) -> Self {
        self.model.model_profiles = profiles;
        self
    }

    pub(crate) fn with_inspect_reasoning_from(mut self, root: &App) -> Self {
        // Children retain their configured defaults, never the mutable root
        // Build/Plan choices. External agents without a local role omit them.
        for role in [ModelRole::Review, ModelRole::Explore, ModelRole::Builder] {
            if let Some(level) = root.model.session.reasoning(role) {
                self.model.session.install_reasoning(role, level);
            }
        }
        self
    }

    pub(crate) fn shared_model_profiles(&self) -> Arc<ConfiguredModelProfiles> {
        Arc::clone(&self.model.model_profiles)
    }

    /// Snapshot both committed root selections for a session replacement.
    /// Restoration and correlated model-management acknowledgements own this
    /// state; usage telemetry, picker drafts and the active mode do not.
    pub fn session_models(&self) -> anyhow::Result<zevria_model::models::SessionModels> {
        self.model.session_models()
    }

    pub(crate) fn contains_presented_error(&self, error: &str) -> bool {
        self.model.conversation.contains_presented_error(error)
    }

    /// Add the session's skills to the composer command registry.
    pub fn with_skills(mut self, skills: Vec<SkillMeta>) -> Self {
        self.composer.with_skills(skills);
        self
    }

    /// Replace authoritative, name-unique explicit-invocation completions.
    pub(crate) fn replace_skill_entries(&mut self, entries: Vec<SkillMeta>) {
        self.composer.with_skills(entries);
    }

    pub fn with_skill_context(
        mut self,
        context: &zevria_instructions::skill::SkillContext,
    ) -> Self {
        self.replace_skill_entries(context.completions());
        self
    }

    /// Rebuild committed projection from an unchanged transcript format.
    pub fn restore(&mut self, items: Vec<TranscriptItem>) {
        self.model.restore_transcript(items);
        self.reset_frontend();
    }

    fn reset_frontend(&mut self) {
        self.drafts = Default::default();
        self.queue = Default::default();
        self.mode_command_draft = None;
        self.worker = Default::default();
        self.help = None;
        self.composer.clear();
        self.interaction.reset();
        self.edit.reset();
        self.folds.clear();
        self.view.reset_conversation();
    }

    /// Install one complete authoritative restore input, without replaying live work.
    pub fn restore_session(&mut self, input: RestorationInput) {
        self.reset_frontend();
        self.model.restore(input);
        self.leave_insert_if_composer_locked();
    }

    /// Seed Plan presentation after transcript restoration and before live events.
    /// This applies the engine-backed snapshot without commands, effects, or a turn.
    pub fn restore_plan_state(&mut self, state: zevria_workflow::PlanWorkflowState) {
        self.view.invalidate_rendered_geometry();
        self.apply_plan_snapshot(state);
        self.leave_insert_if_composer_locked();
    }

    /// Append a visible local notice that does not enter model history.
    pub fn push_error(&mut self, error: String) {
        self.view.invalidate_rendered_geometry();
        self.model.conversation.push_error(error);
    }

    /// Only the resolved composer's cancellation path may clear its draft.
    pub(crate) fn clear_input_if_nonempty(&mut self) -> bool {
        let cleared = self.composer.clear_if_nonempty();
        if cleared {
            self.view.invalidate_rendered_geometry();
            self.view.set_composer_scroll(0);
            self.interaction.clear_chords();
        }
        cleared
    }

    pub(crate) fn install_reasoning(
        &mut self,
        role: ModelRole,
        level: zevria_foundation::ReasoningLevel,
    ) {
        self.model.session.install_reasoning(role, level);
    }

    pub(crate) fn begin_model_management(&mut self) -> Option<SessionMode> {
        if self.capabilities().manage_session.is_err() || self.model.workflow.dialog().is_some() {
            return None;
        }
        let mode = self.model.session.next_mode();
        self.model
            .session
            .begin_operation(OperationKind::ModelManagement, mode, role_for_mode(mode))
            .then_some(mode)
    }

    /// Seed from the actual restored runtime, not startup assignment/usage snapshots.
    pub fn restore_model_contexts(
        &mut self,
        snapshots: impl IntoIterator<Item = zevria_model::ContextTokenSnapshot>,
    ) {
        for snapshot in snapshots {
            self.model.session.install_model(
                snapshot.model_role,
                snapshot.profile.clone(),
                Some(snapshot),
            );
        }
    }

    pub(crate) fn finish_model_management(&mut self) {
        self.model.session.finish_model_management();
    }

    pub(crate) fn install_model(
        &mut self,
        role: ModelRole,
        context: &zevria_foundation::ModelContextPolicy,
        snapshot: Option<zevria_model::ContextTokenSnapshot>,
    ) {
        self.model.install_model(role, context, snapshot);
    }

    pub(crate) const fn is_busy(&self) -> bool {
        self.model.session.is_busy()
    }

    pub(crate) const fn active_turn_id(&self) -> Option<TurnId> {
        self.model.session.active_id()
    }

    pub(crate) fn accepts_progress(&self, turn_id: TurnId) -> bool {
        self.model.session.accepts_progress(turn_id)
    }

    pub(crate) fn accepts_terminal(&self, turn_id: TurnId) -> bool {
        self.model.session.accepts_terminal(turn_id)
    }

    pub(crate) const fn cancel_target(&self) -> Option<Option<TurnId>> {
        self.model.session.cancel_target()
    }

    fn worker_busy(&self) -> bool {
        self.pane.is_worker() && self.worker.busy()
    }

    fn work_pending(&self) -> bool {
        self.model.session.is_busy() || self.worker_busy()
    }

    pub(crate) fn capabilities(&self) -> crate::input::Capabilities {
        use crate::input::DisabledReason::*;
        let editable = if !self.pane.can_compose() {
            Err(ReadOnly)
        } else if self.edit.is_awaiting_acceptance() {
            Err(TranscriptEditPending)
        } else if self.composer.is_paste_pending() {
            Err(ClipboardPending)
        } else {
            Ok(())
        };
        let work = editable.and_then(|()| {
            if self.work_pending() {
                Err(WorkPending)
            } else if matches!(
                self.model.workflow.snapshot(),
                zevria_workflow::PlanWorkflowState::Ready { .. }
            ) {
                Err(PlanDecisionRequired)
            } else {
                Ok(())
            }
        });
        let management = work.and_then(|()| {
            if !self.pane.is_root() {
                Err(ReadOnly)
            } else if !self.edit.is_none() {
                Err(RecallActive)
            } else {
                Ok(())
            }
        });
        let queue_work = editable.and_then(|()| {
            if !self.pane.is_root() {
                Err(ReadOnly)
            } else if !self.edit.is_none() {
                Err(RecallActive)
            } else if matches!(
                self.model.workflow.snapshot(),
                zevria_workflow::PlanWorkflowState::Ready { .. }
            ) {
                Err(PlanDecisionRequired)
            } else if !self.model.session.has_turn_work() && !self.queue.is_running_nonempty() {
                Err(NothingPending)
            } else if self.queue.len() == MAX_QUEUED_FOLLOW_UPS {
                Err(QueueFull)
            } else {
                Ok(())
            }
        });
        crate::input::Capabilities {
            edit_draft: editable,
            submit_work: work,
            queue_work,
            edit_transcript: management,
            manage_session: management,
        }
    }

    pub(crate) fn command_menu_active(&self) -> bool {
        self.composer_editable() && self.composer.completion_filter_active()
    }

    pub(crate) fn reconcile_file_completion(
        &mut self,
        owns_input: bool,
    ) -> Option<FileCompletionRequest> {
        self.composer
            .reconcile_file_completion(owns_input && self.composer_editable())
    }

    pub(crate) fn suspend_file_completion(&mut self) {
        self.composer.suspend_file_completion();
    }

    pub(crate) fn install_file_results(
        &mut self,
        request: &FileCompletionRequest,
        paths: Vec<String>,
        status: FileSearchStatus,
    ) -> bool {
        if !self.composer_editable() || !self.composer.install_file_results(request, paths, status)
        {
            return false;
        }
        self.view.invalidate_rendered_geometry();
        true
    }

    fn composer_editable(&self) -> bool {
        self.interaction.is_insert()
            && self.can_edit_draft()
            && !self.interaction.is_selecting()
            && self.model.workflow.dialog().is_none()
    }

    fn can_edit_draft(&self) -> bool {
        self.capabilities().edit_draft.is_ok()
    }

    pub(crate) fn can_submit_work(&self) -> bool {
        self.capabilities().submit_work.is_ok()
    }

    pub(crate) fn hint_eligibility(&self) -> crate::hints::Eligibility {
        use crate::input::Action;
        let mut hints = crate::hints::Eligibility::default();
        for (available, action) in [
            (
                self.can_submit_work()
                    || self.capabilities().queue_work.is_ok()
                    || (self.composer.is_blank() && self.can_send_queued()),
                Action::Submit,
            ),
            (self.can_edit_queued(), Action::EditQueued),
            (
                self.model.workflow.dialog().is_some()
                    || self.pane.is_worker()
                    || (matches!(
                        self.surface().id.kind,
                        SurfaceKind::Composer | SurfaceKind::Completion
                    ) && !self.composer.is_empty())
                    || (self.pane.is_root()
                        && (!self.model.session.is_busy() || self.cancel_target().is_some())),
                Action::Cancel,
            ),
            (self.can_edit_draft(), Action::Insert),
            (self.can_recall_selected(), Action::Edit),
            (
                self.drafts.has_recovery() && self.can_edit_draft() && self.composer.is_empty(),
                Action::RecoverDraft,
            ),
            (
                self.model.workflow.submitted_artifact().is_some() && self.pane.is_root(),
                Action::PlanReview,
            ),
            (
                self.pane.is_worker()
                    && self
                        .worker
                        .bound
                        .as_ref()
                        .is_some_and(|(_, state)| state.eligible_snapshot().is_some())
                    && self.composer.is_empty(),
                Action::ConfirmWorker,
            ),
            (self.pane.diagnostics_supported(), Action::Diagnostics),
            (!self.pane.is_root(), Action::ReturnRoot),
            (
                self.capabilities().manage_session.is_ok(),
                Action::ToggleMode,
            ),
        ] {
            if !available {
                hints.disabled.push(action);
            }
        }
        if self.surface().id.kind == SurfaceKind::Transcript
            && self.model.workflow.fresh_retry_version().is_none()
            && !(self.composer.is_blank() && self.can_send_queued())
        {
            hints.disabled.push(Action::Confirm);
        }
        if self.model.workflow.dialog().is_some() && self.model.session.is_busy() {
            hints.disabled.push(Action::Confirm);
        }
        if self.composer.is_blank() && self.can_send_queued() {
            hints.labels.push((Action::Submit, "send queued"));
            if self.surface().id.kind == SurfaceKind::Transcript
                && self.model.workflow.fresh_retry_version().is_none()
            {
                hints.labels.push((Action::Confirm, "send queued"));
            }
        } else if self.capabilities().queue_work.is_ok() {
            hints.labels.push((Action::Submit, "queue"));
        }
        hints.labels.push((
            Action::Diagnostics,
            if self.pane.diagnostics_visible() {
                "hide diagnostics"
            } else {
                "diagnostics"
            },
        ));
        hints.labels.push((
            Action::Cancel,
            if self.pane.is_worker() {
                "clear/cancel worker"
            } else if self.work_pending() {
                "cancel turn"
            } else if self.can_clear_queue() {
                "clear queue"
            } else {
                "quit"
            },
        ));
        hints.labels.push((
            Action::Close,
            match self.surface().id.kind {
                SurfaceKind::Completion => "cancel",
                SurfaceKind::Composer if self.edit.is_recalling() => "cancel",
                SurfaceKind::Selection
                    if self.interaction.selection_scope() == Some(SelectionScope::Block) =>
                {
                    "back"
                }
                SurfaceKind::Selection => "exit",
                SurfaceKind::PlanReview => "close",
                _ => "normal",
            },
        ));
        hints.labels.push((
            Action::ToggleMode,
            match self.model.session.next_mode() {
                SessionMode::Build => "Plan",
                SessionMode::Plan => "Build",
            },
        ));
        if self.interaction.selection_scope() == Some(SelectionScope::Message) {
            hints.labels.extend([
                (Action::Copy, "copy message"),
                (Action::Confirm, "blocks"),
                (Action::Down, "next message"),
                (Action::Up, "previous message"),
            ]);
            hints
                .disabled
                .extend([Action::CopyOutput, Action::CopyList]);
        } else if self.interaction.is_selecting() {
            hints.labels.extend([
                (Action::Confirm, "inspect child"),
                (Action::Down, "next block"),
                (Action::Up, "previous block"),
            ]);
        }
        if matches!(
            self.composer.completion_kind(),
            Some(CompletionKind::Skill | CompletionKind::File)
        ) {
            hints.labels.push((Action::AcceptCompletion, "complete"));
        }
        hints
    }

    pub(crate) fn render_help(
        &mut self,
        frame: &mut ratatui::Frame,
        bounds: ratatui::layout::Rect,
    ) {
        if let Some(help) = &mut self.help {
            help.render(frame, bounds);
        }
    }

    pub(crate) fn surface(&self) -> Surface {
        let kind = if self.help.is_some() {
            SurfaceKind::Help
        } else if self.model.workflow.dialog().is_some() {
            SurfaceKind::PlanReview
        } else if self.command_menu_active() {
            SurfaceKind::Completion
        } else if self.interaction.is_insert() && self.pane.can_compose() {
            SurfaceKind::Composer
        } else if self.interaction.is_selecting() {
            SurfaceKind::Selection
        } else {
            SurfaceKind::Transcript
        };
        Surface::new(self.pane_id, kind)
    }

    fn cancel_surface(&mut self, owner: SurfaceKind) -> Option<UiAction> {
        if matches!(owner, SurfaceKind::Composer | SurfaceKind::Completion)
            && self.clear_input_if_nonempty()
        {
            return None;
        }
        if owner == SurfaceKind::PlanReview {
            self.model.workflow.hide();
            self.interaction.enter_normal();
            return None;
        }
        if self.pane.is_worker() {
            return self.worker_action(WorkerControlAction::CancelPrompt);
        }
        if !self.pane.is_root() {
            return None;
        }
        match self.cancel_target() {
            Some(turn_id) => {
                self.queue.pause(QueuePause::Cancelled);
                self.view.invalidate_rendered_geometry();
                Some(UiAction::CancelTurn { turn_id })
            }
            None if self.can_clear_queue() => {
                let count = self.queue.clear();
                self.push_error(format!("Cleared {count} queued follow-ups."));
                None
            }
            None if !self.model.session.is_busy() => Some(UiAction::Quit),
            None => None,
        }
    }

    pub(crate) fn observe_clock(&mut self, now: Instant) {
        self.model.session.observe_clock(now);
    }

    /// Temporary focus owners and pane switches end runs without losing history.
    pub(crate) fn close_composer_edit_group(&mut self) {
        self.composer.suspend_file_completion();
        self.composer.close_edit_group();
        self.interaction.clear_chords();
    }

    /// Workspace allocation or pane activation changed before the next draw.
    pub(crate) fn invalidate_rendered_geometry(&mut self) {
        self.view.invalidate_rendered_geometry();
        if let Some(help) = &mut self.help {
            help.invalidate_geometry();
        }
    }

    /// Reduce one crossterm event using the current monotonic clock.
    pub fn handle_event(&mut self, event: Event) -> Option<UiAction> {
        self.handle_event_at(event, Instant::now())
    }

    /// Deterministic input reducer used by tests for double-Esc and `yy`.
    pub(crate) fn handle_event_at(&mut self, event: Event, now: Instant) -> Option<UiAction> {
        match crate::input::normalize(event) {
            InputEvent::User(input) => self.handle_input_on(self.surface(), input, now),
            InputEvent::Resize => {
                self.invalidate_rendered_geometry();
                None
            }
            InputEvent::Focus { .. } => {
                self.composer.close_edit_group();
                None
            }
            InputEvent::Ignored => None,
        }
    }

    pub(crate) fn handle_input_on(
        &mut self,
        owner: Surface,
        input: UserInput,
        now: Instant,
    ) -> Option<UiAction> {
        self.observe_clock(now);
        let composer_generation = self.composer.generation();
        let plan_dialog_open = self.model.workflow.dialog().is_some();
        debug_assert_eq!(owner.id.pane, self.pane_id);
        let action = self.dispatch_input_at(owner, input, now);
        self.reconcile_file_completion(true);
        if !self.composer_editable() {
            self.composer.close_edit_group();
        }
        if self.composer.generation() != composer_generation
            || self.model.workflow.dialog().is_some() != plan_dialog_open
        {
            self.view.invalidate_rendered_geometry();
        }
        action
    }

    /// Apply one provider-neutral event and return runtime effects explicitly.
    pub(crate) fn reduce(&mut self, event: SessionEvent) -> Vec<AppEffect> {
        self.reduce_at(event, Instant::now())
    }

    pub(crate) fn presentation_selection_anchor(
        &self,
    ) -> Option<crate::presentation::PresentationAnchor> {
        self.interaction.selection().and_then(|selection| {
            self.model
                .conversation
                .selection_identity(selection)
                .map(|block| crate::presentation::PresentationAnchor {
                    epoch: self.model.conversation.epoch(),
                    history: selection.history_index,
                    block,
                })
        })
    }

    pub(crate) fn restore_presentation_selection(
        &mut self,
        anchor: Option<crate::presentation::PresentationAnchor>,
    ) {
        if self.interaction.selection().is_some()
            && let Some(anchor) = anchor
            && anchor.epoch == self.model.conversation.epoch()
            && let Some(selection) = self
                .model
                .conversation
                .selection_for_identity(anchor.history, anchor.block)
        {
            self.interaction.set_selection(selection);
        }
    }

    /// Deterministic lifecycle reduction with an externally observed clock.
    pub(crate) fn reduce_at(&mut self, event: SessionEvent, now: Instant) -> Vec<AppEffect> {
        self.view.capture_conversation_anchor();
        self.view.invalidate_rendered_geometry();
        self.observe_clock(now);
        let anchor = self.presentation_selection_anchor();
        let effects = self.reduce_event(event);
        self.restore_presentation_selection(anchor);
        self.view
            .reconcile_anchor_identity(&self.model.conversation);
        self.folds.reconcile(&self.model.conversation);
        self.leave_insert_if_composer_locked();
        effects
    }

    fn reduce_event(&mut self, event: SessionEvent) -> Vec<AppEffect> {
        let reduction = self.model.reduce(event);
        self.apply_reduction(reduction)
    }

    fn apply_reduction(&mut self, reduction: Reduction) -> Vec<AppEffect> {
        if let Some(kind) = reduction.accepted {
            self.drafts.accept();
            if kind.is_transcript_edit() {
                self.edit.accept();
            }
        }
        if let Some((_, settlement)) = reduction.settled {
            match &settlement {
                zevria_presentation::Settlement::Failed(_) => self.queue.pause(QueuePause::Failed),
                zevria_presentation::Settlement::Rejected(_) => {
                    self.queue.pause(QueuePause::Rejected)
                }
                _ => {}
            }
            self.view.invalidate_rendered_geometry();
            self.edit.reject();
            if settlement.restores_draft() {
                self.restore_rejected_draft();
            }
        }
        if reduction.plan_ready {
            self.queue.pause(QueuePause::PlanDecision);
            self.view.invalidate_rendered_geometry();
            self.composer.close_edit_group();
            self.interaction.clear_selection();
            self.interaction.clear_chords();
        }
        if let Some(settled) = reduction.mode_selection {
            match settled.result {
                zevria_session_api::ModeSelectionResult::Accepted { .. } => {
                    self.mode_command_draft = None;
                    self.view.invalidate_rendered_geometry();
                    self.composer.close_edit_group();
                }
                zevria_session_api::ModeSelectionResult::Rejected { .. } => {
                    if let Some((draft, generation)) = self.mode_command_draft.take() {
                        self.drafts
                            .restore_or_retain(&mut self.composer, draft, generation);
                    }
                }
            }
        }
        if reduction.change == ConversationChange::default() {
            Vec::new()
        } else {
            self.apply_conversation_change(reduction.change)
        }
    }

    pub(crate) fn apply_conversation_change(
        &mut self,
        mut change: ConversationChange,
    ) -> Vec<AppEffect> {
        self.view.capture_conversation_anchor();
        self.view
            .reconcile_anchor_identity(&self.model.conversation);
        self.folds.reconcile(&self.model.conversation);
        self.view.invalidate_rendered_geometry();
        if let Some(index) = change.invalidate_from() {
            self.view.invalidate_from(index);
        }
        if change.clears_selection() {
            self.interaction.clear_selection();
        } else if let Some(active) = self.interaction.active_selection() {
            match self.model.conversation.reconcile_selection(
                active.selection,
                active.scope,
                self.pane.diagnostics_visible(),
            ) {
                Some(selection) => {
                    self.interaction.set_selection(selection);
                }
                None => self.interaction.clear_selection(),
            }
        }
        if change.resets_viewport() {
            self.folds.clear();
            self.view.reset_conversation();
        }
        let removed = change.take_removed_ensemble_runs();
        if removed.is_empty() {
            Vec::new()
        } else {
            vec![AppEffect::PruneEnsembleRuns(removed)]
        }
    }

    /// ACP projection access is scoped to the dedicated reducer. Every
    /// replacement it performs must be followed by `apply_conversation_change`.
    pub(crate) fn conversation_projection_mut(&mut self) -> &mut ConversationState {
        self.view.invalidate_rendered_geometry();
        &mut self.model.conversation
    }
}

// Private-state characterization tests retain their original access paths.
#[cfg(test)]
impl std::ops::Deref for App {
    type Target = SessionModel;
    fn deref(&self) -> &SessionModel {
        &self.model
    }
}
#[cfg(test)]
impl std::ops::DerefMut for App {
    fn deref_mut(&mut self) -> &mut SessionModel {
        &mut self.model
    }
}
