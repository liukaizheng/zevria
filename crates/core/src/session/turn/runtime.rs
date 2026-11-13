//! One FIFO dispatcher and one supervisor for turns and model maintenance.

use crate::session::SessionEngine;
use crate::session::turn::execution::{Rejection, TurnWork};
use futures_util::FutureExt as _;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zevria_foundation::policy::TurnId;
use zevria_session_api::command::{ControlCommand, ManagementCommand, SessionCommand, TurnCommand};
use zevria_session_api::event::{ModeSelectionResult, SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::question::QuestionResponder;
use zevria_session_api::turn::TurnContext;
use zevria_session_api::worker::WorkerControlRouter;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::ensemble::latest_ensemble_recovery;
use zevria_workflow::ensemble_review::WorkerControlResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunControl {
    Continue,
    Shutdown,
}

enum ActiveWork {
    Turn(TurnContext),
    Maintenance {
        request_id: String,
        cancellation: CancellationToken,
    },
}

struct ActiveWorkControl {
    work: ActiveWork,
    question_responder: Option<QuestionResponder>,
    worker_controls: WorkerControlRouter,
    command_jobs: Option<zevria_session_api::CommandJobs>,
    skill_queries: Arc<Mutex<zevria_instructions::skill::SkillContext>>,
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Explicit composition-root grant for frontend ordering barriers, never
    /// installed on children or workers. Notifications are not persisted.
    pub fn with_idle_notifications(mut self) -> Self {
        self.capabilities.idle_notifications = true;
        self
    }

    /// Restored work and queued turns run FIFO. Shutdown cancels and drains the
    /// active work, discards the queue, and repairs persistence only on normal exit.
    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<SessionCommand>,
        events: SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        let mut pending = self.restored_work();
        let mut completions = self
            .capabilities
            .command_jobs
            .as_ref()
            .filter(|jobs| !jobs.is_closed())
            .map(|jobs| jobs.completions_changed());
        if pending.is_empty() && commands.is_empty() {
            self.refresh_continuation_offer(&events).await;
        }
        let mut prewarmed = false;
        loop {
            let control = if let Some(work) = pending.pop_front() {
                prewarmed = false;
                self.dispatch_work(work, &mut commands, &mut pending, &events)
                    .await?
            } else {
                let role = self.policies.policy(self.selected_mode()).model_role;
                tokio::select! {
                    biased;
                    command = commands.recv() => {
                        let Some(command) = command else { break };
                        // The idle future is dropped before command dispatch.
                        // Completing the command begins a fresh idle period.
                        prewarmed = false;
                        self.dispatch_idle(command, &mut commands, &mut pending, &events).await?
                    }
                    changed = async {
                        match &mut completions {
                            Some(receiver) => receiver.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if changed.is_err() || self.capabilities.command_jobs.as_ref().is_some_and(|jobs| jobs.is_closed()) { completions = None; }
                        self.refresh_continuation_offer(&events).await;
                        RunControl::Continue
                    }
                    () = self.provider.prewarm(role), if !prewarmed => {
                        prewarmed = true;
                        RunControl::Continue
                    }
                }
            };
            if control == RunControl::Shutdown {
                break;
            }
            if pending.is_empty() && commands.is_empty() {
                self.refresh_continuation_offer(&events).await;
            }
        }
        self.repair_transcript_on_exit();
        Ok(())
    }

    fn restored_work(&self) -> VecDeque<TurnWork> {
        let mut pending = VecDeque::new();
        if !self.conversation.is_read_only()
            && let Some(recovery) =
                latest_ensemble_recovery(self.conversation.items().iter().filter_map(|item| {
                    match item {
                        TranscriptItem::Ensemble(record) => Some(record),
                        _ => None,
                    }
                }))
        {
            pending.push_back(TurnWork::ResumeEnsemble(recovery));
        }
        pending
    }

    fn repair_transcript_on_exit(&mut self) {
        if !self.conversation.is_read_only()
            && let Err(error) = self.conversation.ensure_durable()
        {
            tracing::error!("failed to repair the transcript during shutdown: {error:#}");
        }
    }

    async fn dispatch_idle(
        &mut self,
        command: SessionCommand,
        commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
        pending: &mut VecDeque<TurnWork>,
        events: &SessionEventSender,
    ) -> Result<RunControl, SessionReplayError> {
        self.ensure_replay_valid()?;
        let (action, reply) = self.route_idle_command(command, false);
        if let Some(reply) = reply {
            let _ = events.send(reply).await;
        }
        let command = match action {
            CommandAction::Continue => return Ok(RunControl::Continue),
            CommandAction::Shutdown => return Ok(RunControl::Shutdown),
            CommandAction::Dispatch(command) => command,
            CommandAction::Queue(_) => unreachable!("idle work is dispatched, not queued"),
        };
        match command {
            SessionCommand::Turn(command) => {
                self.dispatch_work(TurnWork::Command(command), commands, pending, events)
                    .await
            }
            SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) => {
                self.manage_mode(request_id, mode, events).await?;
                if pending.is_empty() && commands.is_empty() {
                    self.refresh_continuation_offer(events).await;
                }
                Ok(RunControl::Continue)
            }
            SessionCommand::Manage(ManagementCommand::Skills {
                request_id,
                request,
            }) => {
                self.manage_skills(request_id, request, events).await?;
                Ok(RunControl::Continue)
            }
            SessionCommand::Manage(ManagementCommand::Models {
                request_id,
                request,
            }) => {
                let cancellation = CancellationToken::new();
                let control = ActiveWorkControl {
                    work: ActiveWork::Maintenance {
                        request_id: request_id.clone(),
                        cancellation: cancellation.clone(),
                    },
                    question_responder: self.capabilities.question_responder.clone(),
                    worker_controls: self.capabilities.worker_controls.clone(),
                    command_jobs: self.capabilities.command_jobs.clone(),
                    skill_queries: Arc::new(Mutex::new(self.management_skill_context()?)),
                };
                let active = self
                    .manage_models(request_id, request, events, &cancellation)
                    .boxed();
                Self::supervise(active, &control, commands, pending, events).await
            }
            SessionCommand::Control(_) => unreachable!("control plane consumed the command"),
        }
    }

    async fn dispatch_work(
        &mut self,
        work: TurnWork,
        commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
        pending: &mut VecDeque<TurnWork>,
        events: &SessionEventSender,
    ) -> Result<RunControl, SessionReplayError> {
        let turn = self.next_turn(work.mode());
        if matches!(
            &work,
            TurnWork::Command(TurnCommand::ContinueAfterJobs { .. })
        ) && (!pending.is_empty() || !commands.is_empty())
        {
            self.capabilities
                .job_follow
                .raise_floor(TurnId::new(turn.id.get().saturating_add(1)));
            self.finish_rejected(
                &turn,
                events,
                Rejection::Rejected(
                    "newer queued work takes precedence over background jobs".into(),
                ),
            )
            .await?;
            self.refresh_continuation_offer(events).await;
            if self.capabilities.idle_notifications && pending.is_empty() {
                let _ = events.send(SessionEvent::EngineIdle).await;
            }
            return Ok(RunControl::Continue);
        }
        let control = ActiveWorkControl {
            work: ActiveWork::Turn(turn.clone()),
            question_responder: self.capabilities.question_responder.clone(),
            worker_controls: self.capabilities.worker_controls.clone(),
            command_jobs: self.capabilities.command_jobs.clone(),
            skill_queries: self.enter_turn()?,
        };
        // Preserve the Send-only provider contract by erasing the nested future.
        let active = self.execute_turn(work, events, &turn).boxed();
        let outcome = Self::supervise(active, &control, commands, pending, events).await;
        self.exit_turn();
        if outcome == Ok(RunControl::Continue) && pending.is_empty() {
            if commands.is_empty() {
                self.refresh_continuation_offer(events).await;
            }
            if self.capabilities.idle_notifications {
                let _ = events.send(SessionEvent::EngineIdle).await;
            }
        }
        if outcome.is_err() {
            events.stream_cleared(turn.id);
        }
        outcome
    }

    async fn supervise(
        mut active: futures_util::future::BoxFuture<'_, Result<(), SessionReplayError>>,
        control: &ActiveWorkControl,
        commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
        pending: &mut VecDeque<TurnWork>,
        events: &SessionEventSender,
    ) -> Result<RunControl, SessionReplayError> {
        let outcome = loop {
            tokio::select! {
                biased;
                incoming = commands.recv() => {
                    // Apply control effects before polling work that may be
                    // immediately ready; only reply publication may suspend.
                    let (action, reply) = control.handle_command::<P>(incoming, pending);
                    let replying = async {
                        if let Some(reply) = reply {
                            let _ = events.send(reply).await;
                        }
                        action
                    };
                    tokio::pin!(replying);
                    let action = tokio::select! {
                        biased;
                        // Poll work even when commands and their replies are
                        // always ready, so command traffic cannot starve it.
                        result = &mut active => {
                            // Fatal replay bypasses control backpressure. A clean
                            // completion still owes the correlated control reply.
                            break match result {
                                Err(error) => Err(error),
                                Ok(()) => Ok(replying.await),
                            };
                        },
                        action = &mut replying => action,
                    };
                    if action == RunControl::Shutdown {
                        break active.await.map(|()| RunControl::Shutdown);
                    }
                },
                result = &mut active => break result.map(|()| RunControl::Continue),
            }
        };
        if outcome.is_err() || outcome == Ok(RunControl::Shutdown) {
            pending.clear();
        }
        outcome
    }
}

pub(in crate::session) fn command_control_reply(
    jobs: Option<&zevria_session_api::CommandJobs>,
    control: zevria_session_api::CommandControl,
) -> SessionEvent {
    let request_id = control.request_id.clone();
    let result = jobs.map_or_else(
        || zevria_session_api::CommandControlResult::Rejected {
            message: "command jobs are not enabled for this session".into(),
        },
        |jobs| jobs.handle_control(control),
    );
    SessionEvent::CommandControlResult { request_id, result }
}

impl ActiveWorkControl {
    fn cancel(&self, requested: Option<TurnId>) {
        match &self.work {
            ActiveWork::Turn(turn) if requested.is_none_or(|id| id == turn.id) => {
                turn.cancellation().cancel()
            }
            ActiveWork::Maintenance { cancellation, .. } if requested.is_none() => {
                cancellation.cancel()
            }
            _ => {}
        }
    }

    /// Apply control effects before publication or the next active-work poll.
    fn handle_command<P: ModelProvider>(
        &self,
        incoming: Option<SessionCommand>,
        pending: &mut VecDeque<TurnWork>,
    ) -> (RunControl, Option<SessionEvent>) {
        let (action, reply) = CommandRouter {
            active: Some(self),
            question_responder: self.question_responder.as_ref(),
            worker_controls: &self.worker_controls,
            command_jobs: self.command_jobs.as_ref(),
            allow_worker_routes: true,
        }
        .route::<P>(incoming);
        let control = match action {
            CommandAction::Queue(command) => {
                pending.push_back(TurnWork::Command(command));
                RunControl::Continue
            }
            CommandAction::Continue => RunControl::Continue,
            CommandAction::Shutdown => RunControl::Shutdown,
            CommandAction::Dispatch(_) => {
                unreachable!("active work never dispatches management directly")
            }
        };
        (control, reply)
    }
}

/// Scheduling remains with the caller; the control plane owns routing effects.
pub(in crate::session) enum CommandAction {
    Dispatch(SessionCommand),
    Queue(TurnCommand),
    Continue,
    Shutdown,
}

struct CommandRouter<'a> {
    active: Option<&'a ActiveWorkControl>,
    question_responder: Option<&'a QuestionResponder>,
    worker_controls: &'a WorkerControlRouter,
    command_jobs: Option<&'a zevria_session_api::CommandJobs>,
    // The direct test API historically permits explicitly registered routes;
    // production idle dispatch rejects them. Keep that distinction explicit.
    allow_worker_routes: bool,
}
impl CommandRouter<'_> {
    fn route<P: ModelProvider>(
        &self,
        incoming: Option<SessionCommand>,
    ) -> (CommandAction, Option<SessionEvent>) {
        if self.active.is_none()
            && matches!(
                &incoming,
                Some(SessionCommand::Turn(_) | SessionCommand::Manage(_))
            )
        {
            return (
                CommandAction::Dispatch(incoming.expect("dispatch command")),
                None,
            );
        }
        let mut reply = None;
        match incoming {
            Some(SessionCommand::Turn(command)) => return (CommandAction::Queue(command), None),
            Some(SessionCommand::Control(ControlCommand::Command(control))) => {
                reply = Some(command_control_reply(self.command_jobs, control));
            }
            Some(SessionCommand::Control(ControlCommand::Worker(control))) => {
                let result = if self.allow_worker_routes {
                    self.worker_controls.route(control)
                } else {
                    Err(WorkerControlResult::rejected(
                        control,
                        "no active interactive worker review",
                    ))
                };
                if let Err(result) = result {
                    reply = Some(SessionEvent::WorkerControlResult { result });
                }
            }
            Some(SessionCommand::Control(ControlCommand::CancelTurn { turn_id })) => {
                if let Some(active) = self.active {
                    active.cancel(turn_id);
                }
            }
            Some(SessionCommand::Control(ControlCommand::AnswerQuestion {
                request_id,
                response,
            })) => {
                if let Some(responder) = self.question_responder {
                    responder.respond(&request_id, response);
                }
            }
            Some(SessionCommand::Manage(ManagementCommand::SetMode { request_id, .. })) => {
                reply = Some(SessionEvent::ModeResult {
                    request_id,
                    result: ModeSelectionResult::rejected(
                        "busy",
                        "mode changes require an idle session; request was not queued",
                    ),
                });
            }
            Some(SessionCommand::Manage(ManagementCommand::Skills {
                request_id,
                request,
            })) => {
                let result = if request.is_mutation() {
                    zevria_instructions::skill::SkillManagementResult::error(
                        "busy",
                        "skill mutations require an idle session; request was not queued",
                    )
                } else {
                    let projection = self
                        .active
                        .expect("active skill query")
                        .skill_queries
                        .lock()
                        .expect("skill query projection poisoned");
                    SessionEngine::<P>::query_skills(&projection, &request)
                };
                reply = Some(SessionEvent::SkillsResult { request_id, result });
            }
            Some(SessionCommand::Manage(ManagementCommand::Models {
                request_id,
                request,
            })) => {
                if let Some(ActiveWork::Maintenance {
                    request_id: active_id,
                    cancellation,
                }) = self.active.map(|active| &active.work)
                    && active_id == &request_id
                    && matches!(
                        request,
                        zevria_model::models::ModelManagementRequest::Cancel
                    )
                {
                    cancellation.cancel();
                } else {
                    reply = Some(SessionEvent::ModelsResult {
                        request_id,
                        result: zevria_model::models::ModelManagementResult::rejected(
                            "busy",
                            "model changes require an idle root session; request was not queued",
                        ),
                    });
                }
            }
            Some(SessionCommand::Control(ControlCommand::Shutdown)) | None => {
                if let Some(active) = self.active {
                    active.cancel(None);
                }
                return (CommandAction::Shutdown, None);
            }
        }
        (CommandAction::Continue, reply)
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn route_idle_command(
        &self,
        command: SessionCommand,
        direct: bool,
    ) -> (CommandAction, Option<SessionEvent>) {
        CommandRouter {
            active: None,
            question_responder: self.capabilities.question_responder.as_ref(),
            worker_controls: &self.capabilities.worker_controls,
            command_jobs: self.capabilities.command_jobs.as_ref(),
            allow_worker_routes: direct,
        }
        .route::<P>(Some(command))
    }
}
