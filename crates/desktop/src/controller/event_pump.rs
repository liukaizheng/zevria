//! GPUI event pump and projection updates.

use super::{
    Context, Duration, Instant, ModeSelectionResult, Phase, Reduction, SessionCommand,
    SessionController, SessionRequest, SessionStart, SessionUpdate,
};

impl SessionController {
    pub(super) fn attach(&mut self, cx: &mut Context<Self>) {
        let running = self.running.as_mut().expect("live running session");
        self.command = Some(running.command_sender());
        let mut receiver = running.take_event_receiver().expect("sole receiver");
        let mut exits = running.take_exit_receiver().expect("sole exit receiver");
        self.pump = Some(cx.spawn(async move |owner, cx| {
            let mut ready = None;
            loop {
                // Consume any ready acceptance before settling a disconnected runtime.
                let next = if let Some(update) = ready.take() {
                    Ok(update)
                } else {
                    tokio::select! {
                        biased;
                        update = receiver.recv() => update.ok_or_else(|| exits.try_recv().ok()),
                        exit = exits.recv() => Err(exit),
                    }
                };
                let first = match next {
                    Ok(update) => update,
                    Err(exit) => {
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
                        let _ = owner.update(cx, |this, cx| this.runtime_disconnected(detail, cx));
                        break;
                    }
                };
                let mut batch = Vec::with_capacity(64);
                batch.push(first);
                for _ in 1..64 {
                    match receiver.try_recv() {
                        Ok(update) => batch.push(update),
                        Err(_) => break,
                    }
                }
                let saturated = batch.len() == 64;
                let keep_going = owner.update(cx, |this, cx| {
                    let mut fresh = None;
                    for update in batch {
                        if let Some(handoff) = this.apply(update, cx) {
                            fresh = Some(handoff);
                            break;
                        }
                    }
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
                if saturated {
                    // Pace sustained lifecycle floods.
                    cx.background_executor()
                        .timer(Duration::from_millis(1))
                        .await;
                } else if let Ok(update) = receiver.try_recv() {
                    // A watch stream can be continuously ready with only one
                    // update per batch. Let GPUI paint and handle input before
                    // synchronizing another frame, without delaying with a timer.
                    ready = Some(update);
                    tokio::task::yield_now().await;
                }
            }
        }));
    }
    fn runtime_disconnected(&mut self, detail: String, cx: &mut Context<Self>) {
        let reduction = self.model.disconnected();
        self.apply_reduction(reduction);
        self.terminal_error = Some(detail);
        self.phase = Phase::Stopping;
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |this, cx| {
                this.stop(cx);
                cx.notify();
            });
        });
        cx.notify();
    }
    pub(super) fn apply(
        &mut self,
        update: SessionUpdate,
        cx: &mut Context<Self>,
    ) -> Option<zevria_workflow::PlanHandoff> {
        self.model.observe_clock(Instant::now());
        let reduction = self.model.apply(update);
        self.flush_job_commands(cx);
        self.flush_mcp_commands(cx);
        while self.phase == Phase::Live {
            let Some(command) = self.model.controls.skills.commands.pop_front() else {
                break;
            };
            self.send(command, cx);
        }
        self.ensure_tick(cx);
        if let Some(root) = reduction.root {
            self.apply_reduction(root);
        }
        if !self.model.root.is_busy() {
            self.cancelling = false;
            if let Some(submitted) = self.draft_slot.take()
                && self.draft_generation == submitted.generation
                && self.draft == submitted.draft
            {
                self.draft.clear();
                self.draft_generation += 1;
            }
        }
        if reduction.fresh.is_none() {
            self.dispatch_job_continuation(cx);
        }
        reduction.fresh
    }
    pub(super) fn dispatch_job_continuation(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Live
            && self.draft.is_empty()
            && !self.draft_slot.has_pending()
            && self.model.controls.question.is_none()
            && let Some(command) = self.model.root.continue_after_jobs()
        {
            self.send(SessionCommand::Turn(command), cx);
        }
    }
    pub(super) fn apply_reduction(&mut self, reduction: Reduction) {
        if reduction.accepted == Some(zevria_presentation::session::OperationKind::Submit) {
            self.orchestrate = false;
        }
        if reduction.accepted.is_some_and(|kind| {
            !matches!(
                kind,
                zevria_presentation::session::OperationKind::JobContinuation
                    | zevria_presentation::session::OperationKind::AuthoritativeJobContinuation
            )
        }) && let Some(submitted) = self.draft_slot.pending_mut()
        {
            if self.draft_generation == submitted.generation && self.draft == submitted.draft {
                self.draft.clear();
                self.draft_generation += 1;
                // The empty replacement is a separate editor identity. User
                // editing and undoing cannot revive this restoration token.
                submitted.generation = self.draft_generation;
            }
            submitted.accepted = true;
        }
        if let Some((kind, settlement)) = reduction.settled
            && !matches!(
                kind,
                zevria_presentation::session::OperationKind::JobContinuation
                    | zevria_presentation::session::OperationKind::AuthoritativeJobContinuation
            )
        {
            self.cancelling = false;
            if let Some(submitted) = self.draft_slot.take()
                && settlement.restores_draft()
            {
                let unchanged = self.draft_generation == submitted.generation;
                if unchanged
                    && (if submitted.accepted {
                        self.draft.is_empty()
                    } else {
                        self.draft == submitted.draft
                    })
                {
                    if self.draft != submitted.draft {
                        self.draft = submitted.draft;
                        self.draft_generation += 1;
                    }
                } else {
                    self.draft_slot.retain(submitted.draft);
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
            if let Some(submitted) = self.draft_slot.take() {
                let unchanged =
                    self.draft_generation == submitted.generation && self.draft == submitted.draft;
                match mode.result {
                    ModeSelectionResult::Accepted { .. } if unchanged => {
                        self.draft.clear();
                        self.draft_generation += 1;
                    }
                    ModeSelectionResult::Rejected { .. } if !unchanged => {
                        self.draft_slot.retain(submitted.draft)
                    }
                    _ => {}
                }
            }
        }
    }
}
