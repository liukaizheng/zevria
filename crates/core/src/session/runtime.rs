//! One FIFO dispatcher and one supervisor for turns and model maintenance.

use super::*;

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
    skill_queries: Arc<Mutex<zevria_instructions::skill::SkillContext>>,
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Restored work and queued turns run FIFO. Shutdown cancels and drains the
    /// active work, discards the queue, and repairs persistence only on normal exit.
    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<SessionCommand>,
        events: SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        let mut pending = self.restored_work();
        loop {
            let control = if let Some(work) = pending.pop_front() {
                self.dispatch_work(work, &mut commands, &mut pending, &events)
                    .await?
            } else if let Some(command) = commands.recv().await {
                self.dispatch_idle(command, &mut commands, &mut pending, &events)
                    .await?
            } else {
                break;
            };
            if control == RunControl::Shutdown {
                break;
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
        match command {
            SessionCommand::Turn(command) => {
                self.dispatch_work(TurnWork::Command(command), commands, pending, events)
                    .await
            }
            SessionCommand::Control(ControlCommand::Worker(control)) => {
                let result =
                    WorkerControlResult::rejected(control, "no active interactive worker review");
                let _ = events
                    .send(SessionEvent::WorkerControlResult { result })
                    .await;
                Ok(RunControl::Continue)
            }
            SessionCommand::Control(ControlCommand::Shutdown) => Ok(RunControl::Shutdown),
            SessionCommand::Control(ControlCommand::CancelTurn { .. }) => Ok(RunControl::Continue),
            SessionCommand::Control(ControlCommand::AnswerQuestion {
                request_id,
                response,
            }) => {
                if let Some(responder) = &self.capabilities.question_responder {
                    responder.respond(&request_id, response);
                }
                Ok(RunControl::Continue)
            }
            SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) => {
                self.manage_mode(request_id, mode, events).await?;
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
                    skill_queries: Arc::new(Mutex::new(self.management_skill_context()?)),
                };
                let active = self
                    .manage_models(request_id, request, events, &cancellation)
                    .boxed();
                Self::supervise(active, &control, commands, pending, events).await
            }
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
        let control = ActiveWorkControl {
            work: ActiveWork::Turn(turn.clone()),
            question_responder: self.capabilities.question_responder.clone(),
            worker_controls: self.capabilities.worker_controls.clone(),
            skill_queries: self.enter_turn()?,
        };
        // Preserve the Send-only provider contract by erasing the nested future.
        let active = self.execute_turn(work, events, &turn).boxed();
        let outcome = Self::supervise(active, &control, commands, pending, events).await;
        self.exit_turn();
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

    /// Apply a received command immediately and return any owed lifecycle reply.
    fn handle_command<P: ModelProvider>(
        &self,
        incoming: Option<SessionCommand>,
        pending: &mut VecDeque<TurnWork>,
    ) -> (RunControl, Option<SessionEvent>) {
        let mut reply = None;
        match incoming {
            Some(SessionCommand::Turn(command)) => pending.push_back(TurnWork::Command(command)),
            Some(SessionCommand::Control(ControlCommand::Worker(control))) => {
                if let Err(result) = self.worker_controls.route(control) {
                    reply = Some(SessionEvent::WorkerControlResult { result });
                }
            }
            Some(SessionCommand::Control(ControlCommand::CancelTurn { turn_id })) => {
                self.cancel(turn_id)
            }
            Some(SessionCommand::Control(ControlCommand::AnswerQuestion {
                request_id,
                response,
            })) => {
                if let Some(responder) = &self.question_responder {
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
                if let ActiveWork::Maintenance {
                    request_id: active_id,
                    cancellation,
                } = &self.work
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
                self.cancel(None);
                return (RunControl::Shutdown, None);
            }
        }
        (RunControl::Continue, reply)
    }
}
