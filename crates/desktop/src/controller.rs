//! In-process session ownership and explicit ordered replacement/shutdown.
mod event_pump;
mod runtime_lifecycle;
use crate::{
    engine::{EngineRuntime, LOG_FILE, LogGuard},
    session::{DraftContent, InputRoute, RecalledOrigin, WorkspaceModel},
};
use gpui_kit::{Context, Subscription, Task};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use zevria_content::PromptImage;
use zevria_foundation::{CommandJobId, SideChatId, SubtaskId};
use zevria_host::{
    Config, RunningSession, SessionStart,
    sessions::{self, SessionSummary},
};
use zevria_presentation::{
    Reduction,
    blocks::PresentationBlockId,
    draft::DraftSlot,
    edit::{EditState, RecallEdit},
};
use zevria_session_api::{
    ControlCommand, ModeSelectionResult, SessionCommand, SessionUpdate, SideChatCommand,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Starting,
    Live,
    Stopping,
    Failed(String),
}
pub struct SessionRequest {
    pub workspace: PathBuf,
    pub start: SessionStart,
}
#[derive(Debug)]
pub struct SavedComposerDraft {
    draft: DraftContent,
    orchestrate: bool,
}

struct PendingSideChatDraft {
    request_id: String,
    generation: u64,
}

pub struct SessionController {
    pub phase: Phase,
    pub config: Option<Arc<Config>>,
    pub config_error: Option<String>,
    pub workspace: Option<PathBuf>,
    pub model: WorkspaceModel,
    pub sessions: Vec<SessionSummary>,
    pub draft: String,
    pub(crate) main_draft_focus_generation: u64,
    pub draft_images: Vec<PromptImage>,
    pub edit: EditState<SavedComposerDraft>,
    recalled_origin: Option<RecalledOrigin>,
    pub recall_generation: u64,
    pub(crate) rejected_edit_generation: u64,
    pub orchestrate: bool,
    pub cancelling: bool,
    pub show_sessions: bool,
    pub resume_reveal_generation: u64,
    /// Native capturing dialogs (for example replacement confirmation).
    capturing_dialog: bool,
    session_generation: u64,
    draft_generation: u64,
    pending_side_chat_draft: Option<PendingSideChatDraft>,
    pub draft_slot: DraftSlot<DraftContent>,
    running: Option<RunningSession>,
    command: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    pending: Option<SessionRequest>,
    terminal_error: Option<String>,
    quit_requested: bool,
    pump: Option<Task<()>>,
    transition: Option<Task<()>>,
    tick: Option<Task<()>>,
    catalog: Option<Task<()>>,
    _quit: Subscription,
}
impl SessionController {
    pub fn new(
        config: Result<Config, String>,
        workspace: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (config, config_error) = match config {
            Ok(config) => (Some(Arc::new(config)), None),
            Err(error) => (None, Some(error)),
        };
        let quit = cx.on_app_quit(|this, cx| {
            this.pump.take();
            this.tick.take();
            this.command.take();
            let job = this
                .running
                .take()
                .map(|running| cx.global::<EngineRuntime>().spawn(running.shutdown()));
            async move {
                if let Some(job) = job {
                    let _ = job.await;
                }
            }
        });
        Self {
            phase: Phase::Idle,
            config,
            config_error,
            workspace,
            model: WorkspaceModel::default(),
            sessions: Vec::new(),
            draft: String::new(),
            main_draft_focus_generation: 0,
            draft_images: Vec::new(),
            edit: EditState::default(),
            recalled_origin: None,
            recall_generation: 0,
            rejected_edit_generation: 0,
            orchestrate: false,
            cancelling: false,
            show_sessions: false,
            resume_reveal_generation: 0,
            capturing_dialog: false,
            session_generation: 0,
            draft_generation: 0,
            pending_side_chat_draft: None,
            draft_slot: Default::default(),
            running: None,
            command: None,
            pending: None,
            terminal_error: None,
            quit_requested: false,
            pump: None,
            transition: None,
            tick: None,
            catalog: None,
            _quit: quit,
        }
    }
    pub(crate) fn capturing_dialog(&self) -> bool {
        self.capturing_dialog
    }

    pub(crate) fn set_capturing_dialog(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.capturing_dialog != value {
            self.capturing_dialog = value;
            cx.notify();
        }
    }

    /// An actual editor change, including undo to identical text, retires the
    /// old submitted draft's replacement token. Programmatic sync emits no edit.
    pub fn edit_draft(&mut self, text: String, cx: &mut Context<Self>) {
        self.edit_composer_draft(text, cx);
        cx.notify();
    }

    // The native editor already owns its redraw. Only an admitted continuation
    // goes through `send`, which notifies session/transcript observers.
    pub fn edit_composer_draft(&mut self, text: String, cx: &mut Context<Self>) {
        if self.edit.is_awaiting_acceptance() {
            return;
        }
        self.draft = text;
        self.draft_generation += 1;
        self.dispatch_job_continuation(cx);
    }

    pub fn can_begin_transcript_edit(&self) -> bool {
        self.phase == Phase::Live
            && self.pending.is_none()
            && !self.quit_requested
            && !self.capturing_dialog
            && !self.cancelling
            && !self.draft_slot.has_pending()
            && !self.edit.is_awaiting_acceptance()
            && self.model.can_edit_transcript()
    }

    pub fn is_prompt_editable(&self, (index, block): (usize, PresentationBlockId)) -> bool {
        self.can_begin_transcript_edit() && self.model.recallable_prompt(index, block).is_some()
    }

    pub fn active_edit_history_index(&self) -> Option<usize> {
        self.model
            .controls
            .selected_child
            .is_none()
            .then(|| self.edit.active_target_index())
            .flatten()
    }

    pub fn recall_prompt(
        &mut self,
        (index, block): (usize, PresentationBlockId),
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.can_begin_transcript_edit() {
            return false;
        }
        let Some((prompt, target, target_index)) = self.model.recallable_prompt(index, block)
        else {
            return false;
        };
        if self.edit.active_target_index() == Some(target_index) {
            return false;
        }
        if self.edit.is_recalling() {
            self.edit.switch_target(target, target_index);
        } else {
            let saved_input = SavedComposerDraft {
                draft: self.draft_content(),
                orchestrate: self.orchestrate,
            };
            self.edit.recall(RecallEdit {
                saved_input,
                target,
                target_index,
            });
        }
        let (draft, origin) = DraftContent::from_user_prompt(&prompt);
        self.replace_draft(draft);
        self.recalled_origin = Some(origin);
        self.orchestrate = false;
        self.recall_generation += 1;
        cx.notify();
        true
    }

    pub fn cancel_recall(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(recall) = self.edit.cancel_recall() else {
            return false;
        };
        self.replace_draft(recall.saved_input.draft);
        self.orchestrate = recall.saved_input.orchestrate;
        self.recalled_origin = None;
        self.recall_generation += 1;
        self.dispatch_job_continuation(cx);
        cx.notify();
        true
    }

    pub(crate) fn image_attachment_generation(&self, side: Option<SideChatId>) -> Option<u64> {
        (self.phase == Phase::Live
            && !self.capturing_dialog
            && self.model.controls.selected_child.is_none()
            && (side.is_some() || !self.edit.is_awaiting_acceptance())
            && !self.model.controls.models.is_open()
            && self.model.controls.question.is_none()
            && self.model.root.workflow.dialog().is_none()
            && side.is_none_or(|id| {
                self.model
                    .side_chats
                    .iter()
                    .any(|pane| pane.descriptor.id == id && !pane.is_running())
            }))
        .then_some(self.session_generation)
    }

    pub(crate) fn begin_image_dialog(
        &mut self,
        side: Option<SideChatId>,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let generation = self.image_attachment_generation(side)?;
        self.set_capturing_dialog(true, cx);
        Some(generation)
    }

    pub(crate) fn finish_image_dialog(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if self.phase != Phase::Live || self.session_generation != generation {
            return false;
        }
        self.set_capturing_dialog(false, cx);
        true
    }

    pub(crate) fn finish_image_attachment(
        &mut self,
        side: Option<SideChatId>,
        generation: u64,
        result: Result<Vec<PromptImage>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.image_attachment_generation(side) != Some(generation) {
            return;
        }
        match result {
            Ok(images) => self.attach_images(side, images, cx),
            Err(error) => self.image_attachment_error(side, error, cx),
        }
    }

    fn attach_images(
        &mut self,
        side: Option<SideChatId>,
        images: Vec<PromptImage>,
        cx: &mut Context<Self>,
    ) {
        use zevria_content::prompt::{MAX_PROMPT_IMAGE_BYTES, MAX_PROMPT_IMAGES};
        let (target, side_error) = if let Some(id) = side {
            let pane = self
                .model
                .side_chats
                .iter_mut()
                .find(|pane| pane.descriptor.id == id)
                .expect("attachment target was checked");
            (&mut pane.draft_images, Some(&mut pane.error))
        } else {
            (&mut self.draft_images, None)
        };
        if target.len() + images.len() > MAX_PROMPT_IMAGES {
            self.image_attachment_error(
                side,
                format!("Attach at most {MAX_PROMPT_IMAGES} images per message."),
                cx,
            );
            return;
        }
        if target
            .iter()
            .chain(&images)
            .map(PromptImage::encoded_len)
            .sum::<usize>()
            > MAX_PROMPT_IMAGE_BYTES
        {
            self.image_attachment_error(
                side,
                "Images exceed the 20 MiB per-message limit.".into(),
                cx,
            );
            return;
        }
        target.extend(images);
        if let Some(error) = side_error {
            *error = None;
        } else {
            self.draft_generation += 1;
        }
        cx.notify();
    }

    fn image_attachment_error(
        &mut self,
        side: Option<SideChatId>,
        error: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = side {
            if let Some(pane) = self
                .model
                .side_chats
                .iter_mut()
                .find(|pane| pane.descriptor.id == id)
            {
                pane.error = Some(error);
            }
        } else {
            self.model.root.conversation.push_error(error);
        }
        cx.notify();
    }

    pub fn remove_side_image(
        &mut self,
        id: zevria_foundation::SideChatId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Live || self.capturing_dialog {
            return;
        }
        if let Some(pane) = self
            .model
            .side_chats
            .iter_mut()
            .find(|pane| pane.descriptor.id == id && !pane.is_running())
            && index < pane.draft_images.len()
        {
            pane.draft_images.remove(index);
            cx.notify();
        }
    }

    pub fn remove_draft_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.edit.is_awaiting_acceptance() && index < self.draft_images.len() {
            self.draft_images.remove(index);
            self.draft_generation += 1;
            self.dispatch_job_continuation(cx);
            cx.notify();
        }
    }

    pub fn recover_draft(&mut self, cx: &mut Context<Self>) {
        if self.edit.is_none()
            && self.draft_is_empty()
            && let Some(draft) = self.draft_slot.recover()
        {
            self.replace_draft(draft);
            cx.notify();
        }
    }

    fn draft_content(&self) -> DraftContent {
        DraftContent {
            text: self.draft.clone(),
            images: self.draft_images.clone(),
        }
    }

    fn draft_is_empty(&self) -> bool {
        self.draft.is_empty() && self.draft_images.is_empty()
    }

    fn replace_draft(&mut self, draft: DraftContent) {
        self.draft = draft.text;
        self.draft_images = draft.images;
        self.draft_generation += 1;
    }

    fn clear_draft(&mut self) {
        self.replace_draft(DraftContent::default());
    }

    pub(crate) fn clock_required(&self) -> bool {
        self.model.root.is_busy()
            || self.side_chat_running()
            || self.model.root.jobs_inspector().is_open()
            || self
                .model
                .root
                .jobs_inspector()
                .jobs()
                .iter()
                .any(zevria_presentation::jobs::JobsInspector::can_terminate)
    }
    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if !self.clock_required() || self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |owner, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = owner.update(cx, |this, cx| {
                    let now = Instant::now();
                    this.model.observe_clock(now);
                    this.model.root.jobs_inspector_mut().tick(now);
                    this.flush_job_commands(cx);
                    cx.notify();
                    if this.clock_required() {
                        true
                    } else {
                        this.tick = None;
                        false
                    }
                });
                if !matches!(busy, Ok(true)) {
                    break;
                }
            }
        }));
    }
    pub fn send(&mut self, command: SessionCommand, cx: &mut Context<Self>) {
        let disconnected = self
            .command
            .as_ref()
            .is_none_or(|sender| sender.send(command).is_err());
        // An attached pump observes runtime exit after consuming ready events.
        // Don't settle ahead of a buffered Started for an already committed prompt.
        if disconnected && self.pump.is_none() {
            let reduction = self.model.disconnected();
            self.apply_reduction(reduction);
            self.terminal_error = Some("The session engine is no longer available.".into());
            self.stop(cx);
        }
        self.ensure_tick(cx);
        cx.notify();
    }
    /// One admission check for status, keyboard, menu and slash entry.
    pub fn model_selection_available(&self) -> Result<(), &'static str> {
        if self.phase != Phase::Live || self.pending.is_some() || self.quit_requested {
            return Err("Model selection requires a live interactive session.");
        }
        if self.capturing_dialog {
            return Err("Close the current dialog before changing models.");
        }
        if self.edit.is_active() {
            return Err("Cancel or finish editing the message before changing models.");
        }
        if self.draft_slot.has_pending() || self.cancelling {
            return Err("Wait for the pending operation before changing models.");
        }
        self.model.model_selection_available()
    }
    fn flush_model_commands(&mut self, cx: &mut Context<Self>) {
        while let Some(command) = self.model.controls.models.commands.pop_front() {
            self.send(command, cx);
            if self.phase != Phase::Live {
                break;
            }
        }
    }
    pub fn open_model_picker(
        &mut self,
        scope: zevria_model::models::ModelSelectionScope,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.model_selection_available().is_err() || !self.model.open_model_picker(scope) {
            return false;
        }
        self.flush_model_commands(cx);
        self.phase == Phase::Live && self.model.controls.models.is_open()
    }
    pub fn select_model(&mut self, profile: usize, cx: &mut Context<Self>) {
        self.model.controls.models.choose_model(profile);
        cx.notify();
    }
    pub fn select_model_effort(&mut self, effort: usize, cx: &mut Context<Self>) {
        self.model.controls.models.choose_effort(effort);
        cx.notify();
    }
    pub fn apply_model_selection(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Live && self.model.controls.models.apply() {
            self.flush_model_commands(cx);
        }
    }
    pub fn confirm_model_conversion(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Live && self.model.controls.models.confirm_conversion() {
            self.flush_model_commands(cx);
        }
    }
    pub fn cancel_model_selection(&mut self, cx: &mut Context<Self>) {
        self.model.controls.models.cancel();
        self.flush_model_commands(cx);
    }
    pub fn flush_job_commands(&mut self, cx: &mut Context<Self>) {
        while self.phase == Phase::Live {
            let Some(command) = self.model.root.jobs_inspector_mut().commands.pop_front() else {
                break;
            };
            self.send(command, cx);
        }
    }
    fn can_open_session_panel(&self) -> bool {
        self.phase == Phase::Live
            && self.model.root.workflow.dialog().is_none()
            && !self.capturing_dialog
            && !self.model.controls.models.is_open()
    }
    pub fn flush_mcp_commands(&mut self, cx: &mut Context<Self>) {
        while self.phase == Phase::Live {
            let Some(command) = self.model.controls.mcp.commands.pop_front() else {
                break;
            };
            self.send(command, cx);
        }
        cx.notify();
    }

    fn open_mcp(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_open_session_panel() || self.model.controls.selected_child.is_some() {
            return false;
        }
        self.model.root.jobs_inspector_mut().close();
        self.model.controls.subtasks_open = false;
        self.model.controls.side_chat_open = false;
        self.model.controls.mcp.show();
        self.flush_mcp_commands(cx);
        true
    }

    pub fn run_mcp_prompt(
        &mut self,
        arguments: std::collections::BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.edit.is_active() {
            return;
        }
        if let Some(draft) = &mut self.model.controls.mcp.draft {
            draft.values = arguments;
        }
        if self.model.root.ready_revision_target().is_some() {
            self.model.controls.mcp.notice =
                Some("Finish reviewing the current Plan first.".into());
            cx.notify();
            return;
        }
        let mode = self.model.root.next_mode();
        if let Some(command) = self
            .model
            .controls
            .mcp
            .run_prompt(mode, self.model.root.is_busy())
            && self.model.root.begin_mcp_prompt(mode)
        {
            self.send(SessionCommand::Turn(command), cx);
        }
        cx.notify();
    }

    pub(crate) fn can_inspect_jobs(&self) -> bool {
        self.can_open_session_panel() && self.model.controls.selected_child.is_none()
    }

    fn open_jobs(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_inspect_jobs() {
            return false;
        }
        self.model.controls.subtasks_open = false;
        self.model.controls.mcp.close();
        self.model.controls.side_chat_open = false;
        self.model.root.jobs_inspector_mut().show();
        self.model.select_default_job();
        self.flush_job_commands(cx);
        self.ensure_tick(cx);
        cx.notify();
        self.model.root.jobs_inspector().is_open()
    }
    pub fn toggle_jobs(&mut self, cx: &mut Context<Self>) {
        if self.model.jobs_panel_visible() {
            self.model.root.jobs_inspector_mut().close();
            cx.notify();
        } else {
            self.open_jobs(cx);
        }
    }
    fn open_subtasks(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_open_session_panel() || self.model.children.is_empty() {
            return false;
        }
        self.model.root.jobs_inspector_mut().close();
        self.model.controls.mcp.close();
        self.model.controls.side_chat_open = false;
        self.model.controls.subtasks_open = true;
        cx.notify();
        true
    }
    pub fn toggle_subtasks(&mut self, cx: &mut Context<Self>) {
        if self.model.controls.subtasks_open {
            self.model.controls.subtasks_open = false;
            cx.notify();
        } else {
            self.open_subtasks(cx);
        }
    }
    pub fn select_pane(&mut self, child: Option<SubtaskId>, cx: &mut Context<Self>) {
        self.model.controls.selected_child = child;
        cx.notify();
    }
    pub fn inspect_job(&mut self, id: CommandJobId, cx: &mut Context<Self>) {
        if self.open_jobs(cx) {
            self.model.root.jobs_inspector_mut().inspect(id);
            self.flush_job_commands(cx);
            cx.notify();
        }
    }
    pub fn terminate_job(&mut self, id: CommandJobId, cx: &mut Context<Self>) {
        self.control_job(id, true, cx);
    }
    pub fn background_job(&mut self, id: CommandJobId, cx: &mut Context<Self>) {
        self.control_job(id, false, cx);
    }
    fn control_job(&mut self, id: CommandJobId, terminate: bool, cx: &mut Context<Self>) {
        if self.phase != Phase::Live {
            return;
        }
        let notice = if terminate {
            self.model.root.jobs_inspector_mut().terminate(id)
        } else {
            self.model.root.jobs_inspector_mut().move_to_background(id)
        };
        if let Some(notice) = notice {
            self.model.root.conversation.push_error(notice);
        }
        self.flush_job_commands(cx);
        cx.notify();
    }
    pub fn quote_side_answer(
        &mut self,
        id: zevria_foundation::SideChatId,
        answer: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self
            .model
            .side_chats
            .iter()
            .find(|pane| pane.descriptor.id == id)
        else {
            return;
        };
        let quote =
            zevria_presentation::side_chats::format_quote_for_main(&pane.descriptor.title, answer);
        if self.edit.is_awaiting_acceptance() {
            return;
        }
        self.model.controls.selected_child = None;
        let mut draft = self.draft.clone();
        if !draft.is_empty() {
            draft.push_str("\n\n");
        }
        draft.push_str(&quote);
        self.main_draft_focus_generation = self.main_draft_focus_generation.wrapping_add(1);
        self.edit_draft(draft, cx);
    }

    pub fn insert_side_reference(
        &mut self,
        id: zevria_foundation::SideChatId,
        turn: usize,
        cx: &mut Context<Self>,
    ) {
        if self.edit.is_awaiting_acceptance() {
            return;
        }
        let Some(pane) = self
            .model
            .side_chats
            .iter()
            .find(|pane| pane.descriptor.id == id)
        else {
            return;
        };
        let mut draft = self.draft.clone();
        if draft
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
        {
            draft.push(' ');
        }
        draft.push_str(&format!("@side:{}#{turn} ", pane.descriptor.short_id));
        self.model.controls.selected_child = None;
        self.main_draft_focus_generation = self.main_draft_focus_generation.wrapping_add(1);
        self.edit_draft(draft, cx);
    }

    pub fn side_chat_running(&self) -> bool {
        self.model.side_chats.iter().any(|pane| pane.is_running())
    }

    pub fn toggle_side_chat(&mut self, cx: &mut Context<Self>) {
        if self.model.controls.side_chat_open {
            self.close_side_chat_panel(cx);
        } else if self.model.side_chats.is_empty() {
            self.create_side_chat(None, None, cx);
        } else {
            self.open_side_chat_panel(cx);
        }
    }

    pub fn close_side_chat_panel(&mut self, cx: &mut Context<Self>) {
        self.model.controls.side_chat_open = false;
        cx.notify();
    }

    fn open_side_chat_panel(&mut self, cx: &mut Context<Self>) -> bool {
        if self.phase != Phase::Live || self.capturing_dialog {
            return false;
        }
        let selected = self
            .model
            .controls
            .selected_side_chat
            .or_else(|| self.model.side_chats.last().map(|pane| pane.descriptor.id));
        let opened = self.model.open_side_chat(selected);
        if opened {
            cx.notify();
        }
        opened
    }

    pub fn select_side_chat(&mut self, id: zevria_foundation::SideChatId, cx: &mut Context<Self>) {
        if self.phase == Phase::Live
            && !self.capturing_dialog
            && self
                .model
                .side_chats
                .iter()
                .any(|pane| pane.descriptor.id == id)
            && self.model.open_side_chat(Some(id))
        {
            cx.notify();
        }
    }

    pub fn create_side_chat(
        &mut self,
        prompt: Option<zevria_content::UserPrompt>,
        excerpt: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.request_side_chat(prompt, excerpt, false, cx);
    }

    fn request_side_chat(
        &mut self,
        prompt: Option<zevria_content::UserPrompt>,
        excerpt: Option<String>,
        from_draft: bool,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Live
            || self.capturing_dialog
            || (from_draft && self.pending_side_chat_draft.is_some())
            || !self.model.open_side_chat(None)
        {
            return;
        }
        let command = zevria_presentation::side_chats::new_side_chat(prompt, excerpt);
        if from_draft {
            let SideChatCommand::Create { request_id, .. } = &command else {
                unreachable!("new Side Chat command");
            };
            // Leave input in the editor until this specific request succeeds.
            self.pending_side_chat_draft = Some(PendingSideChatDraft {
                request_id: request_id.clone(),
                generation: self.draft_generation,
            });
        }
        self.send(
            SessionCommand::Control(ControlCommand::SideChat(command)),
            cx,
        );
    }

    pub fn update_side_draft(
        &mut self,
        id: zevria_foundation::SideChatId,
        draft: String,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Live || self.capturing_dialog {
            return;
        }
        if let Some(pane) = self
            .model
            .side_chats
            .iter_mut()
            .find(|pane| pane.descriptor.id == id)
        {
            pane.draft = draft;
            cx.notify();
        }
    }

    pub fn submit_side_chat(&mut self, id: zevria_foundation::SideChatId, cx: &mut Context<Self>) {
        if self.phase != Phase::Live || self.capturing_dialog {
            return;
        }
        let Some(pane) = self
            .model
            .side_chats
            .iter_mut()
            .find(|pane| pane.descriptor.id == id)
        else {
            return;
        };
        if pane.is_running() {
            return;
        }
        let Ok(text) = zevria_content::UserPrompt::new(
            std::iter::once(zevria_content::PromptBlock::Text(pane.draft.clone()))
                .filter(|_| !pane.draft.trim().is_empty())
                .chain(
                    pane.draft_images
                        .iter()
                        .cloned()
                        .map(zevria_content::PromptBlock::Image),
                )
                .collect(),
        ) else {
            return;
        };
        if pane
            .model
            .submit_in_mode(
                text.clone(),
                zevria_foundation::RequestBehavior::SideChat,
                pane.descriptor.forked_mode,
            )
            .is_none()
        {
            return;
        }
        self.send(
            SessionCommand::Control(zevria_session_api::ControlCommand::SideChat(
                zevria_session_api::SideChatCommand::Submit { id, text },
            )),
            cx,
        );
        cx.notify();
    }

    pub fn cancel_side_chat(&mut self, id: zevria_foundation::SideChatId, cx: &mut Context<Self>) {
        self.send(
            SessionCommand::Control(zevria_session_api::ControlCommand::SideChat(
                zevria_session_api::SideChatCommand::Cancel { id },
            )),
            cx,
        );
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Live
            || self.model.controls.selected_child.is_some()
            || self.model.controls.question.is_some()
            || self.capturing_dialog
            || self.model.controls.models.is_open()
            || self.edit.is_awaiting_acceptance()
        {
            return;
        }
        let draft = self.draft_content();
        let prompt = draft.to_user_prompt(self.recalled_origin.as_ref());
        let route = if let Some((target, target_index)) = self.edit.recalled_target() {
            self.model.route_edit_input(&prompt, target, target_index)
        } else {
            self.model.route_prompt(&prompt, self.orchestrate)
        };
        match route {
            InputRoute::Prompt(command) => {
                self.clear_draft();
                self.draft_slot.stage(draft, self.draft_generation);
                self.send(*command, cx);
            }
            InputRoute::Control(command) => {
                self.clear_draft();
                self.send(*command, cx);
            }
            InputRoute::Edit(command) => {
                // Keep the edit and caret locked in place until acceptance.
                self.edit.stage_submission();
                self.send(*command, cx);
            }
            InputRoute::Models(scope) => match self.model_selection_available() {
                Ok(()) if self.open_model_picker(scope, cx) => self.clear_draft(),
                Ok(()) => {}
                Err(reason) => self.model.root.conversation.push_error(reason.into()),
            },
            InputRoute::Mcp => {
                if self.open_mcp(cx) {
                    self.clear_draft();
                }
            }
            InputRoute::Jobs => {
                if self.open_jobs(cx) {
                    self.clear_draft();
                }
            }
            InputRoute::Subtasks => {
                if self.can_open_session_panel() && self.model.children.is_empty() {
                    self.model
                        .root
                        .conversation
                        .push_error("No subtasks in this session.".into());
                    self.clear_draft();
                } else if self.open_subtasks(cx) {
                    self.clear_draft();
                }
            }
            InputRoute::SideChat(prompt) => {
                if prompt.is_some() || self.model.side_chats.is_empty() {
                    self.request_side_chat(prompt, None, true, cx);
                } else if self.open_side_chat_panel(cx) {
                    self.clear_draft();
                }
            }
            InputRoute::New => {
                if self.new_session(cx) {
                    self.clear_draft();
                }
            }
            InputRoute::Resume => {
                self.show_sessions = true;
                self.resume_reveal_generation += 1;
                self.refresh_sessions(cx);
                self.clear_draft();
            }
            InputRoute::Notice(notice) => self.model.root.conversation.push_error(notice),
            InputRoute::Retained => {}
        }
        cx.notify();
    }
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.model.controls.models.is_open() {
            self.cancel_model_selection(cx);
            return;
        }
        if self.phase != Phase::Live || self.cancelling {
            return;
        }
        if let Some(command) = self.model.root.cancel() {
            self.cancelling = true;
            self.send(SessionCommand::Control(command), cx);
        }
    }
    pub fn select_mode(&mut self, mode: zevria_foundation::SessionMode, cx: &mut Context<Self>) {
        if self.phase != Phase::Live
            || self.model.controls.models.is_open()
            || self.capturing_dialog
            || self.edit.is_active()
        {
            return;
        }
        if let Some(command) = self.model.root.select_mode(mode) {
            self.send(SessionCommand::Manage(command), cx);
        }
    }
    pub fn plan_decision(
        &mut self,
        decision: zevria_workflow::PlanDecision,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Live
            || self.edit.is_active()
            || self.model.controls.models.is_open()
        {
            return;
        }
        if let Some(artifact) = self.model.root.workflow.dialog_artifact() {
            let version = artifact.version;
            if let Some(command) = self.model.root.resolve_plan(version, decision) {
                self.send(SessionCommand::Turn(command), cx);
            }
        }
    }
    pub fn answer_question(&mut self, dismiss: bool, cx: &mut Context<Self>) {
        if self.phase != Phase::Live {
            return;
        }
        let Some(form) = self.model.controls.question.as_mut() else {
            return;
        };
        if form.pending {
            return;
        }
        match if dismiss {
            form.dismiss()
        } else {
            form.response()
        } {
            Ok(response) => {
                form.pending = true;
                let request_id = form.request.id.clone();
                self.send(
                    SessionCommand::Control(ControlCommand::AnswerQuestion {
                        request_id,
                        response,
                    }),
                    cx,
                );
            }
            Err(error) => form.error = Some(error.to_string()),
        }
        cx.notify();
    }
    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let exclude = self.model.session_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { sessions::resumable(&workspace, Some(&exclude)) });
        self.catalog = Some(cx.spawn(async move |owner, cx| {
            let result = task.await;
            let _ = owner.update(cx, |this, cx| {
                match result {
                    Ok(sessions) => this.sessions = sessions,
                    Err(error) => this
                        .model
                        .root
                        .conversation
                        .push_error(format!("Couldn't load earlier sessions: {error:#}")),
                }
                cx.notify();
            });
        }));
    }
    pub fn retry_config(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async {
            let config = Config::load().map_err(|e| format!("{e:#}"))?;
            let tokens = crate::ui::theme::prepare(&config).map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>((config, tokens))
        });
        self.transition = Some(cx.spawn(async move |owner, cx| {
            let config = task.await;
            let _ = owner.update(cx, |this, cx| {
                match config {
                    Ok((config, tokens)) => {
                        if !cx.has_global::<LogGuard>() {
                            match zevria_foundation::logging::init_named(config.log(), LOG_FILE) {
                                Ok(guard) => cx.set_global(LogGuard(Some(guard))),
                                Err(error) => {
                                    this.config_error = Some(error.to_string());
                                    cx.notify();
                                    return;
                                }
                            }
                        }
                        crate::ui::theme::select(tokens, cx);
                        this.config = Some(Arc::new(config));
                        this.config_error = None;
                        this.start_initial(false, cx);
                    }
                    Err(error) => this.config_error = Some(error),
                }
                cx.notify();
            });
        }));
    }
    #[cfg(any(test, feature = "fixtures"))]
    pub fn fixture(
        sender: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(Err("fixture".into()), Some(PathBuf::from("/tmp")), cx);
        this.config_error = None;
        this.phase = Phase::Live;
        this.command = Some(sender);
        this
    }
    #[cfg(any(test, feature = "fixtures"))]
    pub fn fixture_event(&mut self, event: impl Into<SessionUpdate>, cx: &mut Context<Self>) {
        self.apply(event.into(), cx);
        cx.notify();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui_kit::{AppContext as _, TestAppContext};
    pub(crate) fn config(workspace: &std::path::Path, address: &str) -> Config {
        let source = format!(
            r#"
[providers.test]
base_url = "http://{address}/v1/responses"
api_key = "fixture"
supports_websockets = false
[providers.test.input_token_count]
enabled = false
[providers.test.models.fixture]
context_window_tokens = 272000
retained_user_tokens = 20000
reasoning_levels = ["medium"]
reasoning_summary_level = "detailed"
[modes]
build = {{ provider = "test", model = "fixture", reasoning_level = "medium" }}
plan = {{ provider = "test", model = "fixture", reasoning_level = "medium" }}
review = {{ provider = "test", model = "fixture", reasoning_level = "medium" }}
explore = {{ provider = "test", model = "fixture", reasoning_level = "medium" }}
builder = {{ provider = "test", model = "fixture", reasoning_level = "medium" }}
"#
        );
        zevria_host::test_support::write_fixture(&workspace.join("config.toml"), &source).unwrap()
    }
    pub(crate) fn until(
        cx: &mut TestAppContext,
        mut ready: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            // The pump may yield after startup metadata before conversational
            // work arrives. Real Tokio time does not advance GPUI's test timers.
            cx.executor().advance_clock(Duration::from_millis(2));
            cx.run_until_parked();
            if ready(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "controller transition timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[gpui_kit::test]
    fn replacing_an_active_session_releases_its_lease_before_restarting(cx: &mut TestAppContext) {
        cx.executor().allow_parking(); // Intentional real Tokio I/O, not a deterministic synthetic test.
        let directory = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let runtime = EngineRuntime::new().unwrap();
        let listener = runtime
            .0
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let config = config(&workspace, &listener.local_addr().unwrap().to_string());
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let server = runtime.spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let (mut stream, _) = listener.accept().await.unwrap();
            accepted_tx.send(()).unwrap();
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes).await;
        });
        let controller = cx.update(|cx| {
            cx.set_global(runtime.clone());
            cx.new(|cx| SessionController::new(Ok(config), Some(workspace.clone()), cx))
        });
        controller.update(cx, |this, cx| this.start_initial(false, cx));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| this.phase == Phase::Live)
        });
        let path = controller.read_with(cx, |this, _| this.model.transcript_path.clone());
        controller.update(cx, |this, cx| {
            this.draft = "start and cancel by replacement".into();
            this.submit(cx);
        });
        until(cx, |cx| {
            controller.read_with(cx, |this, _| this.model.root.active_turn_id().is_some())
        });
        until(cx, |_| accepted_rx.try_recv().is_ok());
        assert!(path.with_extension("jsonl.lock").exists());
        controller.update(cx, |this, cx| this.new_session(cx));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| {
                this.phase == Phase::Live && this.model.transcript_path != path
            })
        });
        assert!(!path.with_extension("jsonl.lock").exists());
        assert!(path.is_file());
        let replacement_path =
            controller.read_with(cx, |this, _| this.model.transcript_path.clone());
        assert!(
            !replacement_path.exists(),
            "unused desktop replacement stays pending"
        );
        controller.update(cx, |this, cx| {
            this.replace(
                SessionRequest {
                    workspace: workspace.clone(),
                    start: SessionStart::Resume(path.clone()),
                },
                cx,
            )
        });
        until(cx, |cx| {
            controller.read_with(cx, |this, _| {
                this.phase == Phase::Live && this.model.transcript_path == path
            })
        });
        assert!(controller.read_with(cx, |this, _| {
            !this.model.root.conversation.history().is_empty()
        }));
        controller.update(cx, |this, cx| this.stop(cx));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| this.phase == Phase::Idle)
        });
        assert!(!path.with_extension("jsonl.lock").exists());
        assert!(!replacement_path.exists());
        server.abort();
    }
    #[gpui_kit::test]
    fn replacement_requested_during_startup_is_serialized(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first = std::fs::canonicalize(first.path()).unwrap();
        let second = std::fs::canonicalize(second.path()).unwrap();
        let config = config(&first, "127.0.0.1:9");
        let runtime = EngineRuntime::new().unwrap();
        let controller = cx.update(|cx| {
            cx.set_global(runtime);
            cx.new(|cx| SessionController::new(Ok(config), Some(first.clone()), cx))
        });
        controller.update(cx, |this, cx| {
            this.start_initial(false, cx);
            this.replace(
                SessionRequest {
                    workspace: second.clone(),
                    start: SessionStart::New {
                        inherited_models: None,
                    },
                },
                cx,
            );
        });
        until(cx, |cx| {
            controller.read_with(cx, |this, _| {
                this.phase == Phase::Live && this.workspace.as_ref() == Some(&second)
            })
        });
        let pending_path = controller.read_with(cx, |this, _| this.model.transcript_path.clone());
        assert!(!pending_path.exists());
        for entry in std::fs::read_dir(zevria_transcript::transcript::sessions_dir(&first)).unwrap()
        {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.ends_with(".jsonl") && !name.ends_with(".jsonl.lock"),
                "outgoing unused session leaked {name}"
            );
        }
        controller.update(cx, |this, cx| this.stop(cx));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| this.phase == Phase::Idle)
        });
    }
}
