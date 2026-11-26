//! In-process session ownership and explicit ordered replacement/shutdown.
mod event_pump;
mod runtime_lifecycle;
use crate::{
    engine::{EngineRuntime, LOG_FILE, LogGuard},
    session::{InputRoute, WorkspaceModel},
};
use gpui_kit::{Context, Subscription, Task};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use zevria_foundation::{CommandJobId, SubtaskId};
use zevria_host::{
    Config, RunningSession, SessionStart,
    sessions::{self, SessionSummary},
};
use zevria_presentation::Reduction;
use zevria_session_api::{ControlCommand, ModeSelectionResult, SessionCommand, SessionUpdate};

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
pub struct SessionController {
    pub phase: Phase,
    pub config: Option<Arc<Config>>,
    pub config_error: Option<String>,
    pub workspace: Option<PathBuf>,
    pub model: WorkspaceModel,
    pub sessions: Vec<SessionSummary>,
    pub draft: String,
    pub orchestrate: bool,
    pub cancelling: bool,
    pub show_sessions: bool,
    /// Native capturing dialogs (for example replacement confirmation).
    pub capturing_dialog: bool,
    draft_generation: u64,
    pub draft_slot: zevria_presentation::draft::DraftSlot<String>,
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
            orchestrate: false,
            cancelling: false,
            show_sessions: false,
            capturing_dialog: false,
            draft_generation: 0,
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
    /// An actual editor change, including undo to identical text, retires the
    /// old submitted draft's replacement token. Programmatic sync emits no edit.
    pub fn edit_draft(&mut self, text: String, cx: &mut Context<Self>) {
        self.edit_composer_draft(text, cx);
        cx.notify();
    }

    // The native editor already owns its redraw. Only an admitted continuation
    // goes through `send`, which notifies session/transcript observers.
    pub fn edit_composer_draft(&mut self, text: String, cx: &mut Context<Self>) {
        self.draft = text;
        self.draft_generation += 1;
        self.dispatch_job_continuation(cx);
    }
    pub(crate) fn clock_required(&self) -> bool {
        self.model.root.is_busy()
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
        if self
            .command
            .as_ref()
            .is_none_or(|sender| sender.send(command).is_err())
        {
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
        self.model.controls.mcp.show();
        self.flush_mcp_commands(cx);
        true
    }

    pub fn run_mcp_prompt(
        &mut self,
        arguments: std::collections::BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
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
        if !self.can_open_session_panel() {
            return false;
        }
        self.model.root.jobs_inspector_mut().close();
        self.model.controls.mcp.close();
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
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Live
            || self.model.controls.selected_child.is_some()
            || self.model.controls.question.is_some()
            || self.capturing_dialog
            || self.model.controls.models.is_open()
        {
            return;
        }
        match self.model.route_input(&self.draft, self.orchestrate) {
            InputRoute::Command(command) | InputRoute::McpPrompt(command) => {
                self.draft_slot
                    .stage(self.draft.clone(), self.draft_generation);
                self.send(*command, cx);
            }
            InputRoute::Models(scope) => {
                // Management has no TurnStarted acknowledgement and must never
                // acquire submitted-turn bookkeeping. Only an accepted slash
                // invocation consumes its own command text.
                if self.open_model_picker(scope, cx) {
                    self.draft.clear();
                    self.draft_generation += 1;
                }
            }
            InputRoute::Mcp => {
                if self.open_mcp(cx) {
                    self.draft.clear();
                    self.draft_generation += 1;
                }
            }
            InputRoute::Jobs => {
                if self.open_jobs(cx) {
                    self.draft.clear();
                    self.draft_generation += 1;
                }
            }
            InputRoute::Subtasks => {
                if self.open_subtasks(cx) {
                    self.draft.clear();
                    self.draft_generation += 1;
                }
            }
            InputRoute::New => self.new_session(cx),
            InputRoute::Resume => {
                self.show_sessions = true;
                self.refresh_sessions(cx);
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
        if self.phase != Phase::Live || self.model.controls.models.is_open() {
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
mod tests {
    use super::*;
    use gpui_kit::{AppContext as _, TestAppContext};
    fn config(workspace: &std::path::Path, address: &str) -> Config {
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
    fn until(cx: &mut TestAppContext, mut ready: impl FnMut(&mut TestAppContext) -> bool) {
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
