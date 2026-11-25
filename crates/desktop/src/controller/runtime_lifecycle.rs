//! Native session startup, replacement, and shutdown.

use super::{
    Arc, Config, Context, EngineRuntime, LogGuard, PathBuf, Phase, RunningSession,
    SessionController, SessionRequest, SessionStart, WorkspaceModel, sessions,
};

impl SessionController {
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
        let zevria_host::StartedSession { running, restored } =
            zevria_host::start_session(&config, &workspace, start).await?;
        // Projection and restoration I/O never run on the GPUI main thread.
        let model = tokio::task::spawn_blocking(move || crate::session::restore(restored)).await;
        match model {
            Ok(mut model) => {
                if let Some(notice) = notice {
                    model.root.conversation.push_error(notice);
                }
                Ok((running, model))
            }
            Err(error) => {
                let _ = running.shutdown().await;
                Err(error.into())
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
        self.terminal_error = None;
        self.phase = Phase::Starting;
        self.draft.clear();
        self.draft_generation = 0;
        self.draft_slot = Default::default();
        self.cancelling = false;
        self.capturing_dialog = false;
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
        if self.quit_requested || self.model.controls.models.is_open() {
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
    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        // Drop the sole receiver first, unblocking bounded lifecycle sends.
        self.pump.take();
        self.tick.take();
        self.command.take();
        if self.model.root.pending_prompt().is_some() {
            let reduction = self.model.disconnected();
            self.apply_reduction(reduction);
        }
        self.model.controls.models.reset();
        self.model.root.session.finish_model_management();
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
}
