//! In-process session ownership and explicit ordered replacement/shutdown.
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
struct SubmittedDraft {
    text: String,
    generation: u64,
    accepted: bool,
}
pub struct SessionController {
    pub phase: Phase,
    pub config: Option<Arc<Config>>,
    pub config_error: Option<String>,
    pub workspace: Option<PathBuf>,
    pub model: WorkspaceModel,
    pub sessions: Vec<SessionSummary>,
    pub draft: String,
    pub recovery: Vec<String>,
    pub orchestrate: bool,
    pub cancelling: bool,
    pub show_sessions: bool,
    draft_generation: u64,
    submitted: Option<SubmittedDraft>,
    running: Option<RunningSession>,
    command: Option<tokio::sync::mpsc::UnboundedSender<SessionCommand>>,
    pending: Option<SessionRequest>,
    terminal_error: Option<String>,
    quit_requested: bool,
    pump: Option<Task<()>>,
    exits: Option<Task<()>>,
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
            this.exits.take();
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
            recovery: Vec::new(),
            orchestrate: false,
            cancelling: false,
            show_sessions: false,
            draft_generation: 0,
            submitted: None,
            running: None,
            command: None,
            pending: None,
            terminal_error: None,
            quit_requested: false,
            pump: None,
            exits: None,
            transition: None,
            tick: None,
            catalog: None,
            _quit: quit,
        }
    }
    pub fn start_initial(&mut self, continue_session: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        if self.config.is_none() {
            return;
        }
        self.phase = Phase::Starting;
        cx.notify();
        let config = self.config.as_ref().unwrap().clone();
        let job = cx.global::<EngineRuntime>().spawn(async move {
            let (start, notice) = if continue_session {
                match sessions::latest(&workspace)? {
                    Some(path) => (SessionStart::Resume(path), None),
                    None => (
                        SessionStart::New {
                            inherited_models: None,
                        },
                        Some("No previous session was found; started a new session.".to_string()),
                    ),
                }
            } else {
                (
                    SessionStart::New {
                        inherited_models: None,
                    },
                    None,
                )
            };
            Self::open(config, workspace, start, notice).await
        });
        self.await_start(job, cx);
    }
    async fn open(
        config: Arc<Config>,
        workspace: PathBuf,
        start: SessionStart,
        notice: Option<String>,
    ) -> anyhow::Result<(RunningSession, WorkspaceModel)> {
        let mut running = zevria_host::start_session(&config, &workspace, start).await?;
        let restored = running.take_restored();
        // Projection and restoration I/O never run on the GPUI main thread.
        let model = tokio::task::spawn_blocking(move || WorkspaceModel::restore(restored)).await;
        match model {
            Ok(Ok(mut model)) => {
                if let Some(notice) = notice {
                    model.root.conversation.push_error(notice);
                }
                Ok((running, model))
            }
            outcome => {
                let _ = running.shutdown().await;
                match outcome {
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                    _ => unreachable!(),
                }
            }
        }
    }
    fn await_start(
        &mut self,
        job: tokio::task::JoinHandle<anyhow::Result<(RunningSession, WorkspaceModel)>>,
        cx: &mut Context<Self>,
    ) {
        self.transition = Some(cx.spawn(async move |owner, cx| {
            let result = job.await;
            // If the window/controller disappears, still shut a completed engine
            // down instead of detaching it with live supervisor ownership.
            let mut ready = Some(result);
            let installed = owner.update(cx, |this, cx| {
                match ready.take().unwrap() {
                    Ok(Ok((running, model))) => {
                        this.running = Some(running);
                        this.model = model;
                        if this.quit_requested
                            || this.pending.is_some()
                            || this.phase == Phase::Stopping
                        {
                            this.stop(cx);
                        } else {
                            this.phase = Phase::Live;
                            this.attach(cx);
                            this.refresh_sessions(cx);
                        }
                    }
                    Ok(Err(error)) => this.start_failed(format!("{error:#}"), cx),
                    Err(error) => {
                        this.start_failed(format!("Session startup task failed: {error}"), cx)
                    }
                }
                cx.notify();
            });
            if installed.is_err()
                && let Some(Ok(Ok((running, _)))) = ready.take()
            {
                let engine = cx.update(|cx| cx.global::<EngineRuntime>().clone());
                engine.spawn(running.shutdown());
            }
        }));
    }
    fn start_failed(&mut self, error: String, cx: &mut Context<Self>) {
        if self.quit_requested {
            self.finish_quit(cx);
        } else if let Some(next) = self.pending.take() {
            self.launch(next, cx);
        } else {
            self.phase = Phase::Failed(error);
        }
    }
    fn launch(&mut self, request: SessionRequest, cx: &mut Context<Self>) {
        let Some(config) = self.config.clone() else {
            return;
        };
        self.workspace = Some(request.workspace.clone());
        self.catalog = None;
        self.sessions.clear();
        self.show_sessions = false;
        self.model = WorkspaceModel::default();
        self.recovery.clear();
        self.terminal_error = None;
        self.phase = Phase::Starting;
        self.draft.clear();
        self.draft_generation = 0;
        self.submitted = None;
        self.cancelling = false;
        self.orchestrate = false;
        let job = cx.global::<EngineRuntime>().spawn(Self::open(
            config,
            request.workspace,
            request.start,
            None,
        ));
        self.await_start(job, cx);
        cx.notify();
    }
    pub fn replace(&mut self, request: SessionRequest, cx: &mut Context<Self>) {
        if self.quit_requested {
            return;
        }
        match self.phase {
            Phase::Starting => {
                self.pending = Some(request);
                self.phase = Phase::Stopping;
            }
            Phase::Stopping => self.pending = Some(request),
            _ if self.running.is_some() => {
                self.pending = Some(request);
                self.stop(cx);
            }
            _ => self.launch(request, cx),
        }
        cx.notify();
    }
    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.clone() {
            let inherited_models = if self.model.session_id.is_empty() {
                None
            } else {
                match self.model.root.session_models() {
                    Ok(models) => Some(models),
                    Err(error) => {
                        self.model
                            .root
                            .conversation
                            .push_error(format!("Cannot replace session: {error:#}"));
                        cx.notify();
                        return;
                    }
                }
            };
            self.replace(
                SessionRequest {
                    workspace,
                    start: SessionStart::New { inherited_models },
                },
                cx,
            );
        }
    }
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.quit_requested = true;
        self.pending = None;
        if self.phase == Phase::Starting {
            self.phase = Phase::Stopping;
        } else if self.phase != Phase::Stopping {
            self.stop(cx);
        }
        cx.notify();
    }
    fn stop(&mut self, cx: &mut Context<Self>) {
        // Drop the sole receiver first, unblocking bounded lifecycle sends.
        self.pump.take();
        self.exits.take();
        self.tick.take();
        self.command.take();
        let Some(running) = self.running.take() else {
            if self.quit_requested {
                self.finish_quit(cx);
            } else if let Some(error) = self.terminal_error.take() {
                self.phase = Phase::Failed(error);
            }
            return;
        };
        self.phase = Phase::Stopping;
        let job = cx.global::<EngineRuntime>().spawn(running.shutdown());
        self.transition = Some(cx.spawn(async move |owner, cx| {
            let result = job.await;
            let _ = owner.update(cx, |this, cx| {
                if let Err(error) = &result {
                    tracing::error!("shutdown task: {error}");
                }
                if let Ok(Err(error)) = &result {
                    tracing::error!("session shutdown: {error:#}");
                }
                let shutdown_error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("Session shutdown failed: {error:#}")),
                    Err(error) => Some(format!("Shutdown task failed: {error}")),
                };
                if this.quit_requested {
                    this.finish_quit(cx);
                } else if let Some(error) = shutdown_error {
                    this.pending = None;
                    this.phase = Phase::Failed(error);
                } else if let Some(next) = this.pending.take() {
                    this.launch(next, cx);
                } else if let Some(error) = this.terminal_error.take() {
                    this.phase = Phase::Failed(error);
                } else {
                    this.phase = Phase::Idle;
                }
                cx.notify();
            });
        }));
    }
    fn finish_quit(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<LogGuard>() {
            cx.global_mut::<LogGuard>().0.take();
        }
        cx.quit();
    }
    fn attach(&mut self, cx: &mut Context<Self>) {
        let running = self.running.as_mut().expect("live running session");
        self.command = Some(running.command_sender());
        let mut receiver = running.take_event_receiver().expect("sole receiver");
        let mut exits = running.take_exit_receiver().expect("sole exit receiver");
        self.pump = Some(cx.spawn(async move |owner, cx| {
            while let Some(first) = receiver.recv().await {
                let mut batch = Vec::with_capacity(64);
                batch.push(first);
                for _ in 1..64 {
                    match receiver.try_recv() {
                        Ok(update) => batch.push(update),
                        Err(_) => break,
                    }
                }
                let keep_going = owner.update(cx, |this, cx| {
                    let mut fresh = None;
                    for update in batch {
                        if let Some(handoff) = this.apply(update, cx) {
                            fresh = Some(handoff);
                            break;
                        }
                    }
                    this.ensure_tick(cx);
                    cx.notify();
                    if let Some(handoff) = fresh {
                        let inherited_models = match this.model.root.session_models() {
                            Ok(models) => Some(models),
                            Err(error) => {
                                this.model.root.conversation.push_error(format!(
                                    "Cannot create fresh Plan session: {error:#}"
                                ));
                                cx.notify();
                                return true;
                            }
                        };
                        let workspace = this.workspace.clone().expect("live workspace");
                        let weak = cx.weak_entity();
                        cx.defer(move |cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.replace(
                                    SessionRequest {
                                        workspace,
                                        start: SessionStart::FromPlan {
                                            handoff,
                                            inherited_models,
                                        },
                                    },
                                    cx,
                                )
                            });
                        });
                        false
                    } else {
                        true
                    }
                });
                if !matches!(keep_going, Ok(true)) {
                    break;
                }
                // Yield even when a producer continuously fills lifecycle traffic.
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
        }));
        self.exits = Some(cx.spawn(async move |owner, cx| {
            let exit = exits.recv().await;
            let _ = owner.update(cx, |this, cx| {
                let detail = exit.map_or_else(
                    || "Session runtime disconnected".into(),
                    |exit| {
                        format!(
                            "{} stopped{}",
                            exit.component,
                            exit.error.map_or_else(String::new, |e| format!(": {e}"))
                        )
                    },
                );
                let reduction = this.model.disconnected();
                this.apply_reduction(reduction);
                this.terminal_error = Some(detail);
                this.phase = Phase::Stopping;
                let weak = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.stop(cx);
                        cx.notify();
                    });
                });
                cx.notify();
            });
        }));
    }
    fn apply(
        &mut self,
        update: SessionUpdate,
        _cx: &mut Context<Self>,
    ) -> Option<zevria_workflow::PlanHandoff> {
        self.model.observe_clock(Instant::now());
        let reduction = self.model.apply(update);
        if let Some(root) = reduction.root {
            self.apply_reduction(root);
        }
        if !self.model.root.is_busy() {
            self.cancelling = false;
            self.tick.take();
            if let Some(submitted) = self.submitted.take()
                && self.draft_generation == submitted.generation
                && self.draft == submitted.text
            {
                self.draft.clear();
                self.draft_generation += 1;
            }
        }
        reduction.fresh
    }
    fn apply_reduction(&mut self, reduction: Reduction) {
        if reduction.accepted == Some(zevria_presentation::session::OperationKind::Submit) {
            self.orchestrate = false;
        }
        if reduction.accepted.is_some()
            && let Some(submitted) = &mut self.submitted
        {
            if self.draft_generation == submitted.generation && self.draft == submitted.text {
                self.draft.clear();
                self.draft_generation += 1;
                // The empty replacement is a separate editor identity. User
                // editing and undoing cannot revive this restoration token.
                submitted.generation = self.draft_generation;
            }
            submitted.accepted = true;
        }
        if let Some((_, settlement)) = reduction.settled {
            self.cancelling = false;
            if let Some(submitted) = self.submitted.take()
                && settlement.restores_draft()
            {
                let unchanged = self.draft_generation == submitted.generation;
                if unchanged
                    && (if submitted.accepted {
                        self.draft.is_empty()
                    } else {
                        self.draft == submitted.text
                    })
                {
                    if self.draft != submitted.text {
                        self.draft = submitted.text;
                        self.draft_generation += 1;
                    }
                } else {
                    self.recovery.push(submitted.text);
                }
            }
        }
        if let Some(mode) = reduction.mode_selection {
            if matches!(
                mode.result,
                ModeSelectionResult::Accepted {
                    mode: zevria_foundation::SessionMode::Plan,
                    ..
                }
            ) {
                self.orchestrate = false;
            }
            if let Some(submitted) = self.submitted.take() {
                let unchanged =
                    self.draft_generation == submitted.generation && self.draft == submitted.text;
                match mode.result {
                    ModeSelectionResult::Accepted { .. } if unchanged => {
                        self.draft.clear();
                        self.draft_generation += 1;
                    }
                    ModeSelectionResult::Rejected { .. } if !unchanged => {
                        self.recovery.push(submitted.text)
                    }
                    _ => {}
                }
            }
        }
    }
    /// An actual editor change, including undo to identical text, retires the
    /// old submitted draft's replacement token. Programmatic sync emits no edit.
    pub fn edit_draft(&mut self, text: String, cx: &mut Context<Self>) {
        self.draft = text;
        self.draft_generation += 1;
        cx.notify();
    }
    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if !self.model.root.is_busy() || self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |owner, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = owner.update(cx, |this, cx| {
                    this.model.observe_clock(Instant::now());
                    cx.notify();
                    if this.model.root.is_busy() {
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
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Live
            || self.model.selected_child.is_some()
            || self.model.question.is_some()
        {
            return;
        }
        match self.model.route_input(&self.draft, self.orchestrate) {
            InputRoute::Command(command) => {
                self.submitted = Some(SubmittedDraft {
                    text: self.draft.clone(),
                    generation: self.draft_generation,
                    accepted: false,
                });
                self.send(*command, cx);
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
        if self.phase != Phase::Live || self.cancelling {
            return;
        }
        if let Some(command) = self.model.root.cancel() {
            self.cancelling = true;
            self.send(SessionCommand::Control(command), cx);
        }
    }
    pub fn select_mode(&mut self, mode: zevria_foundation::SessionMode, cx: &mut Context<Self>) {
        if self.phase != Phase::Live {
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
        if self.phase != Phase::Live {
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
        let Some(form) = self.model.question.as_mut() else {
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
                        .push_error(format!("Session list: {error:#}")),
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
                        crate::ui::theme::apply(tokens, cx);
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
    #[cfg(test)]
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
    #[cfg(test)]
    pub fn fixture_event(
        &mut self,
        event: zevria_session_api::SessionEvent,
        cx: &mut Context<Self>,
    ) {
        self.apply(SessionUpdate::Lifecycle(event), cx);
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
