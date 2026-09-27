//! Serialized root-owned worker controls and crash-atomic confirmation barrier.
use super::*;

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) async fn review_plan_workers(
        &mut self,
        launcher: Arc<dyn EnsembleLauncher>,
        start: &EnsembleStart,
        resume: bool,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<Vec<AgentRunOutcome>, Failure> {
        let mut states = start
            .agents
            .iter()
            .cloned()
            .map(WorkerReviewState::new)
            .collect::<Vec<_>>();
        let mut results: HashMap<WorkerControlId, WorkerControlResult> = HashMap::new();
        let mut frozen = None;
        let mut versioned = false;
        let mut history = Vec::new();
        for item in self.conversation.items() {
            let TranscriptItem::Ensemble(record) = item else {
                continue;
            };
            if record.run_id() != &start.run_id {
                continue;
            }
            match record {
                EnsembleRecord::ReviewStarted { version, .. } => {
                    if *version != ENSEMBLE_REVIEW_VERSION {
                        return Err(anyhow::anyhow!(
                            "incompatible ensemble review format {version}"
                        )
                        .into());
                    }
                    versioned = true;
                }
                EnsembleRecord::WorkerReview {
                    worker_id,
                    event,
                    result,
                    ..
                } => {
                    apply_worker_review_event(&mut states, worker_id, event)
                        .map_err(anyhow::Error::msg)?;
                    history.push((worker_id.clone(), *event.clone()));
                    if let Some(result) = result {
                        results.insert(result.control.request_id.clone(), result.clone());
                    }
                }
                EnsembleRecord::ControlResult { result, .. } => {
                    if result.accepted
                        && matches!(result.control.action, WorkerControlAction::CancelPrompt)
                    {
                        let state = states
                            .iter_mut()
                            .find(|state| state.descriptor.id == result.control.target.worker_id)
                            .ok_or_else(|| anyhow::anyhow!("foreign cancellation target"))?;
                        let generation = state
                            .cancellable_generation()
                            .ok_or_else(|| anyhow::anyhow!("cancellation has no active input"))?;
                        state
                            .apply(&WorkerReviewEvent::CancelRequested { generation })
                            .map_err(anyhow::Error::msg)?;
                    }
                    results.insert(result.control.request_id.clone(), result.clone());
                }
                EnsembleRecord::WorkersConfirmed {
                    outcomes,
                    final_confirmation,
                    ..
                } => {
                    let control = &final_confirmation.control;
                    let event = control.sealing_event().map_err(anyhow::Error::msg)?;
                    apply_worker_review_event(&mut states, &control.target.worker_id, &event)
                        .map_err(anyhow::Error::msg)?;
                    frozen = Some(outcomes.clone());
                }
                _ => {}
            }
        }
        if !versioned {
            return Err(anyhow::anyhow!("incompatible legacy Ensemble Plan history: explicit worker review records are required; proof is not confirmation").into());
        }
        if let Some(outcomes) = frozen {
            validate_frozen_workers(start, &outcomes).map_err(anyhow::Error::msg)?;
            for (state, outcome) in states.iter_mut().zip(&outcomes) {
                if &state.outcome() != outcome {
                    return Err(anyhow::anyhow!("frozen outcome differs from root review").into());
                }
                if !state.abandoned {
                    state
                        .apply(&WorkerReviewEvent::Sealed)
                        .map_err(anyhow::Error::msg)?;
                }
                publish_worker_review(events, turn, start, state).await;
            }
            return launcher
                .finalize_review(
                    EnsembleLaunchRequest {
                        start: start.clone(),
                        resume: true,
                    },
                    outcomes,
                    events.clone(),
                    turn.clone(),
                )
                .await
                .map_err(|error| review_launch_error(error).into());
        }
        if resume {
            for update in launcher
                .recover_review(start, &history)
                .map_err(|error| review_preflight_blocked(&error))?
            {
                let state = states
                    .iter_mut()
                    .find(|state| state.descriptor.id == update.worker_id)
                    .ok_or_else(|| anyhow::anyhow!("foreign worker recovery evidence"))?;
                if !state.abandoned {
                    self.commit_worker_transition(start, state, update.event, None)?;
                }
            }
        }
        if states.iter().all(|state| state.abandoned) {
            for state in &states {
                publish_worker_review(events, turn, start, state).await;
            }
            return Err(Failure::Cancelled);
        }
        // Interrupted prompts may already have reached the provider. Do not
        // resend their text. Preserve the fallback and require explicit retry.
        for state in &mut states {
            if state.abandoned {
                continue;
            }
            if resume && state.image_capability.is_some() {
                self.commit_worker_transition(
                    start,
                    state,
                    WorkerReviewEvent::Connection {
                        connected: false,
                        diagnostic: None,
                    },
                    None,
                )?;
            }
            if let Some(input) = state.active.clone() {
                let event = WorkerReviewEvent::Interrupted {
                    generation: input.generation,
                };
                self.commit_worker_transition(start, state, event, None)?;
            }
            if state.accepted_generation == 0 {
                self.commit_worker_transition(
                    start,
                    state,
                    WorkerReviewEvent::InputAccepted {
                        input: WorkerInput {
                            generation: 1,
                            request_id: WorkerControlId::new(),
                            kind: WorkerPromptKind::Initial,
                            text: start.prompt.clone(),
                        },
                    },
                    None,
                )?;
            }
        }
        let mut execution = launcher
            .start_review(
                EnsembleLaunchRequest {
                    start: start.clone(),
                    resume,
                },
                states.clone(),
                events.clone(),
                turn.clone(),
            )
            .map_err(review_launch_error)?;
        let (registration, mut controls) = self
            .capabilities
            .worker_controls
            .register(start.run_id.clone(), turn.id);
        let mut last_control = None;
        let review_result: Result<Vec<AgentRunOutcome>, Failure> = async {
        // Once controls can be routed, every failure must pass through the
        // acknowledgement drain, including initial payload persistence.
        for state in &mut states {
            self.check_worker_payload(
                start,
                state,
                launcher.max_synthesis_bytes_per_agent(),
                turn,
            )?;
            publish_worker_review(events, turn, start, state).await;
        }
        let outcomes = loop {
            tokio::select! {
                biased;
                () = turn.cancellation().cancelled() => return Err(Failure::Cancelled),
                incoming = controls.recv() => {
                    let Some(control) = incoming else { return Err(anyhow::anyhow!("worker control router closed").into()) };
                    last_control = Some(control.clone());
                    let reject = |detail| WorkerControlResult::rejected(control.clone(), detail);
                    if control.request_id.0.trim().is_empty() || control.request_id.0.len() > 128 {
                        let _ = events.send(SessionEvent::WorkerControlResult { result: reject("invalid control request identity") }).await;
                        continue;
                    }
                    if let Some(prior) = results.get(&control.request_id) {
                        let result = if prior.control == control { prior.clone() } else { reject("request ID was already used for a different action") };
                        let _ = events.send(SessionEvent::WorkerControlResult { result }).await;
                        continue;
                    }
                    let Some(index) = states.iter().position(|state| state.descriptor.id == control.target.worker_id) else {
                        let _ = events.send(SessionEvent::WorkerControlResult { result: reject("foreign worker target") }).await;
                        continue;
                    };
                    if control.target.run_id != start.run_id || control.target.turn_id != turn.id {
                        let _ = events.send(SessionEvent::WorkerControlResult { result: reject("stale worker turn or run") }).await;
                        continue;
                    }
                    let state = &states[index];
                    if state.abandoned {
                        let _ = events.send(SessionEvent::WorkerControlResult { result: reject("worker is permanently abandoned for this run") }).await;
                        continue;
                    }
                    let mut actor_command = None;
                    let transition: Result<Option<WorkerReviewEvent>, String> = match &control.action {
                        WorkerControlAction::SendFeedback { text } => {
                            if let Err(error) = text.validate() { Err(error.to_string()) }
                            else if text.has_images() && state.image_capability != Some(true) { Err("worker image capability is unsupported or not yet negotiated; draft was not accepted".into()) }
                            else if text.is_blank() || text.text_len() > MAX_WORKER_FEEDBACK_BYTES { Err("feedback requires text or an image, with at most 64 KiB of text".into()) }
                            else if let Err(error) = zevria_workflow::ensemble::validate_synthesis_image_budget(&start.prompt, &states, Some(text)) { Err(format!("synthesis image evidence budget: {error}")) }
                            else if state.pending.len() >= WORKER_CONTROL_CAPACITY { Err("worker feedback queue is full; draft was not accepted".into()) }
                            else {
                                let input = WorkerInput { generation: state.accepted_generation + 1, request_id: control.request_id.clone(), kind: WorkerPromptKind::UserFeedback, text: text.clone() };
                                actor_command = Some(WorkerActorCommand::Prompt(input.clone()));
                                Ok(Some(WorkerReviewEvent::InputAccepted { input }))
                            }
                        }
                        WorkerControlAction::Confirm { expected_revision } | WorkerControlAction::Baseline { expected_revision } => {
                            if let Some(error) = &state.synthesis_error { Err(error.clone()) }
                            else if state.eligible_snapshot().map(|plan| &plan.revision) != Some(expected_revision) { Err("displayed revision is stale or not confirmable; wait for queued work and publish a fresh plan".into()) }
                            else if let Err(error) = zevria_workflow::ensemble::validate_synthesis_image_budget(&start.prompt, &states, None) { Err(format!("synthesis image evidence budget: {error}")) }
                            else if let Err(error) = zevria_workflow::validate_worker_synthesis_payload(EnsembleWorkflow::Plan, &state.outcome(), launcher.max_synthesis_bytes_per_agent()) { Err(error.to_string()) }
                            else {
                                let receipt = WorkerConfirmationReceipt { request_id: control.request_id.clone(), target: control.target.clone(), revision: expected_revision.clone() };
                                Ok(Some(if matches!(control.action, WorkerControlAction::Baseline { .. }) {
                                    WorkerReviewEvent::BaselineMarked { receipt }
                                } else { WorkerReviewEvent::Confirmed { receipt } }))
                            }
                        }
                        WorkerControlAction::Unconfirm { expected_revision } => {
                            if state.confirmation.as_ref().map(|receipt| &receipt.revision) != Some(expected_revision) { Err("worker is not confirmed at the displayed revision".into()) }
                            else { Ok(Some(WorkerReviewEvent::Withdrawn { request_id: control.request_id.clone() })) }
                        }
                        WorkerControlAction::Unbaseline { expected_revision } => {
                            if state.confirmed_plan().as_ref().map(|plan| &plan.snapshot.revision) != Some(expected_revision) { Err("worker is not confirmed at the displayed revision".into()) }
                            else { Ok(Some(WorkerReviewEvent::BaselineCleared { request_id: control.request_id.clone() })) }
                        }
                        WorkerControlAction::Abandon => Ok(Some(WorkerReviewEvent::Abandoned { request_id: control.request_id.clone() })),
                        WorkerControlAction::CancelPrompt => {
                            if let Some(generation) = state.cancellable_generation() {
                                actor_command = Some(WorkerActorCommand::CancelPrompt { generation });
                                Ok(Some(WorkerReviewEvent::CancelRequested { generation }))
                            } else { Err("worker is idle; no prompt was cancelled".into()) }
                        }
                        WorkerControlAction::Retry => {
                            if !state.quiescent() { Err("worker still has active or queued input".into()) }
                            else if state.failed_image_input.is_some() && state.image_capability != Some(true) { Err("the failed image input is retained; reconnect to an image-capable worker before retrying".into()) }
                            else {
                                let input = WorkerInput { generation: state.accepted_generation + 1, request_id: control.request_id.clone(), kind: WorkerPromptKind::RecoveryContinuation,
                                    text: if let Some(failed) = &state.failed_image_input { failed.text.clone() } else if state.evidence.acp_session_id.is_none() { start.prompt.clone() } else { "Continue the interrupted planning interaction in this session. Publish the complete Markdown proposal when ready.".into() } };
                                actor_command = Some(WorkerActorCommand::Prompt(input.clone()));
                                Ok(Some(WorkerReviewEvent::InputAccepted { input }))
                            }
                        }
                    };
                    let transition = match transition {
                        Ok(transition) => transition,
                        Err(detail) => { let _ = events.send(SessionEvent::WorkerControlResult { result: reject(&detail) }).await; continue; }
                    };
                    let mut proposed = states.clone();
                    let affected = if let Some(event) = &transition {
                        match apply_worker_review_event(&mut proposed, &control.target.worker_id, event) {
                            Ok(affected) => affected,
                            Err(error) => {
                                let _ = events.send(SessionEvent::WorkerControlResult { result: reject(&error) }).await;
                                continue;
                            }
                        }
                    } else { vec![control.target.worker_id.clone()] };
                    if matches!(control.action, WorkerControlAction::Confirm { .. } | WorkerControlAction::Baseline { .. })
                        && let Err(error) = zevria_workflow::validate_worker_synthesis_payload(EnsembleWorkflow::Plan, &proposed[index].outcome(), launcher.max_synthesis_bytes_per_agent()) {
                        let _ = events.send(SessionEvent::WorkerControlResult { result: reject(&error.to_string()) }).await;
                        continue;
                    }
                    let participates = |state: &WorkerReviewState| !state.abandoned;
                    let seals = proposed.iter().any(participates)
                        && proposed.iter().all(|state| state.abandoned || state.confirmed_plan().is_some());
                    if seals || matches!(control.action, WorkerControlAction::SendFeedback { .. } | WorkerControlAction::Confirm { .. } | WorkerControlAction::Baseline { .. } | WorkerControlAction::Retry) {
                        let preflight = if seals {
                            let outcomes = proposed.iter().map(WorkerReviewState::outcome).collect::<Vec<_>>();
                            match zevria_workflow::ensemble::build_synthesis_prompt_with_feedback(start.workflow, &start.prompt, &outcomes, &proposed, launcher.max_synthesis_bytes_per_agent()) {
                                Ok(message) if start.publishes_confirmed_worker_plan() || zevria_content::prompt::message_has_images(&message) => self.preflight_synthesis_message(start, &message, &outcomes, turn).await,
                                Ok(_) => Ok(()),
                                Err(error) => Err(error),
                            }
                        } else {
                            self.preflight_potential_synthesis(start, &proposed, launcher.max_synthesis_bytes_per_agent(), turn).await
                        };
                        if let Err(error) = preflight {
                            let _ = events.send(SessionEvent::WorkerControlResult { result: reject(&format!("synthesis evidence preflight: {error}")) }).await;
                            continue;
                        }
                    }
                    let mut result = WorkerControlResult { control: control.clone(), accepted: true, detail: match &control.action {
                        WorkerControlAction::SendFeedback { .. } => "Feedback durably accepted and queued on this worker session.",
                        WorkerControlAction::Confirm { .. } => "Exact proposal revision confirmed; reopenable until all participating workers confirm.",
                        WorkerControlAction::Abandon => if states.iter().enumerate().all(|(other, state)| other == index || state.abandoned) {
                            "Last worker abandoned; ensemble cancelled. No synthesis or Plan publication."
                        } else {
                            zevria_workflow::ensemble::WORKER_ABANDONMENT_REASON
                        },
                        WorkerControlAction::Unconfirm { .. } => "Confirmation withdrawn; any baseline mark cleared.",
                        WorkerControlAction::Baseline { .. } => "Exact proposal revision confirmed and selected as the synthesis baseline; previous worker confirmations retained.",
                        WorkerControlAction::Unbaseline { .. } => "Baseline mark removed; confirmation retained.",
                        WorkerControlAction::Retry => "Retry accepted.",
                        WorkerControlAction::CancelPrompt => "Worker-local cancellation durably requested for this input generation.",
                    }.into() };
                    if matches!(control.action, WorkerControlAction::Baseline { .. })
                        && let Some(previous) = states.iter().find(|state| state.baseline.is_some()) {
                        result.detail.push_str(&format!(" Replaced baseline {} ({}); its confirmation was retained.", previous.descriptor.label, previous.descriptor.id));
                    }
                    if seals {
                        let outcomes = proposed.iter().map(WorkerReviewState::outcome).collect::<Vec<_>>();
                        validate_frozen_workers(start, &outcomes).map_err(anyhow::Error::msg)?;
                        for outcome in outcomes.iter().filter(|outcome| outcome.status != AgentRunStatus::Abandoned) { zevria_workflow::validate_worker_synthesis_payload(EnsembleWorkflow::Plan, outcome, launcher.max_synthesis_bytes_per_agent())?; }
                        // The final explicit receipt and the complete ordered frozen
                        // set have ONE durability boundary, never sampled booleans.
                        self.record_required(TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed { run_id: start.run_id.clone(), final_confirmation: result.clone(), outcomes: outcomes.clone() }))?;
                        states = proposed;
                        for worker_id in affected.iter().filter(|id| *id != &control.target.worker_id) {
                            if let Some(actor) = execution.commands.get(worker_id) {
                                let _ = actor.send(WorkerActorCommand::Mirror(WorkerReviewEvent::BaselineCleared { request_id: control.request_id.clone() }));
                            }
                        }
                        if matches!(control.action, WorkerControlAction::Abandon) {
                            abandon_actor(&execution, &states[index], &control.request_id);
                        }
                        results.insert(control.request_id.clone(), result.clone());
                        let _ = events.send(SessionEvent::WorkerControlResult { result }).await;
                        for state in &mut states {
                            if !state.abandoned {
                                state.apply(&WorkerReviewEvent::Sealed).map_err(anyhow::Error::msg)?;
                            }
                            publish_worker_review(events, turn, start, state).await;
                        }
                        break outcomes;
                    }
                    if let Some(event) = transition {
                        self.record_required(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                            run_id: start.run_id.clone(), worker_id: control.target.worker_id.clone(),
                            event: Box::new(event.clone()), result: Some(result.clone()),
                        }))?;
                        states = proposed;
                        for worker_id in &affected {
                            let mirror = if worker_id == &control.target.worker_id { event.clone() }
                                else { WorkerReviewEvent::BaselineCleared { request_id: control.request_id.clone() } };
                            if let Some(actor) = execution.commands.get(worker_id)
                                && !states.iter().any(|state| &state.descriptor.id == worker_id && state.abandoned) {
                                let _ = actor.send(WorkerActorCommand::Mirror(mirror));
                            }
                        }
                    } else {
                        self.record_required(TranscriptItem::Ensemble(EnsembleRecord::ControlResult { run_id: start.run_id.clone(), result: result.clone() }))?;
                    }
                    results.insert(control.request_id.clone(), result.clone());
                    let _ = events.send(SessionEvent::WorkerControlResult { result }).await;
                    for worker_id in &affected {
                        let state = states.iter().find(|state| &state.descriptor.id == worker_id).expect("affected worker");
                        publish_worker_review(events, turn, start, state).await;
                    }
                    if states[index].abandoned {
                        abandon_actor(&execution, &states[index], &control.request_id);
                        if states.iter().all(|state| state.abandoned) {
                            return Err(Failure::Cancelled);
                        }
                    }
                    if let Some(command) = actor_command {
                        execution.commands.get(&control.target.worker_id).ok_or_else(|| review_interrupted("worker actor missing after durable acceptance"))?
                            .send(command).map_err(|_| review_interrupted("worker actor stopped after durable control acceptance"))?;
                    }
                }
                update = execution.updates.recv() => {
                    let update = update.ok_or_else(|| review_interrupted("worker update channel closed before confirmation"))?;
                    let state = states.iter_mut().find(|state| state.descriptor.id == update.worker_id).ok_or_else(|| anyhow::anyhow!("foreign worker update"))?;
                    if state.abandoned || matches!(update.event, WorkerReviewEvent::Abandoned { .. } | WorkerReviewEvent::Confirmed { .. } | WorkerReviewEvent::BaselineMarked { .. } | WorkerReviewEvent::BaselineCleared { .. } | WorkerReviewEvent::Withdrawn { .. } | WorkerReviewEvent::Sealed) {
                        tracing::debug!(worker = %update.worker_id, "ignored excluded-worker update or worker-origin abandonment mirror");
                        continue;
                    }
                    if let WorkerReviewEvent::Fatal { error } = &update.event { return Err(review_interrupted(error).into()); }
                    self.commit_worker_transition(start, state, update.event, None)?;
                    self.check_worker_payload(start, state, launcher.max_synthesis_bytes_per_agent(), turn)?;
                    publish_worker_review(events, turn, start, state).await;
                }
            }
        };
        Ok(outcomes)
        }.await;
        drop(registration);
        // Reject already-routed controls on every exit, not just a seal.
        // A failure after durable acceptance preserves its accepted result.
        if review_result.is_err()
            && let Some(control) = last_control
        {
            let result = results
                .get(&control.request_id)
                .filter(|prior| prior.control == control)
                .cloned()
                .unwrap_or_else(|| {
                    WorkerControlResult::rejected(
                        control,
                        "worker review stopped before durable acceptance; draft retained",
                    )
                });
            let _ = events
                .send(SessionEvent::WorkerControlResult { result })
                .await;
        }
        // Reject already-routed controls ordered after the seal; never silently
        // lose a correlated result or retarget it to the next ensemble.
        controls.close();
        while let Some(control) = controls.recv().await {
            let result = results
                .get(&control.request_id)
                .filter(|prior| prior.control == control)
                .cloned()
                .unwrap_or_else(|| {
                    WorkerControlResult::rejected(
                        control,
                        if review_result.is_ok() {
                            "all-worker review is sealed"
                        } else {
                            "worker review stopped; draft was not accepted"
                        },
                    )
                });
            let _ = events
                .send(SessionEvent::WorkerControlResult { result })
                .await;
        }
        let outcomes = review_result?;
        let mut acknowledgements = Vec::new();
        for outcome in outcomes
            .iter()
            .filter(|outcome| outcome.status != AgentRunStatus::Abandoned)
        {
            let (tx, rx) = tokio::sync::oneshot::channel();
            execution
                .commands
                .get(&outcome.descriptor.id)
                .ok_or_else(|| review_interrupted("worker actor missing at durable seal"))?
                .send(WorkerActorCommand::Finish {
                    outcome: Box::new(outcome.clone()),
                    acknowledgement: tx,
                })
                .map_err(|_| {
                    review_interrupted("worker stopped before terminal mirror of the durable seal")
                })?;
            acknowledgements.push(rx);
        }
        let mut updates_open = true;
        for mut rx in acknowledgements {
            loop {
                tokio::select! {
                    biased;
                    () = turn.cancellation().cancelled() => return Err(Failure::Cancelled),
                    result = &mut rx => {
                        result.map_err(|_| review_interrupted("worker terminal persistence was interrupted"))?.map_err(|error| review_interrupted(&error))?;
                        break;
                    }
                    update = execution.updates.recv(), if updates_open => {
                        // The frozen root set is already authoritative. Drain
                        // late telemetry so bounded publication cannot prevent
                        // a worker from flushing its terminal mirror.
                        updates_open = update.is_some();
                    }
                }
            }
        }
        Ok(outcomes)
    }

    fn check_worker_payload(
        &mut self,
        start: &EnsembleStart,
        state: &mut WorkerReviewState,
        limit: usize,
        turn: &TurnContext,
    ) -> Result<(), Failure> {
        if state.abandoned {
            return Ok(());
        }
        let mut checked = state.clone();
        checked.synthesis_error = None;
        let error = if let Some(snapshot) = checked.eligible_snapshot() {
            let direct_error = if start.publishes_confirmed_worker_plan() {
                DirectWorkerPlan::validate(snapshot.plan.markdown.clone().unwrap_or_default())
                    .err()
                    .map(|error| error.to_string())
            } else {
                None
            };
            let marking = WorkerConfirmationReceipt {
                // Reserve every accepted request-ID length, not just UUIDs.
                request_id: WorkerControlId("x".repeat(128)),
                target: WorkerControlTarget {
                    turn_id: turn.id,
                    run_id: start.run_id.clone(),
                    worker_id: checked.descriptor.id.clone(),
                },
                revision: snapshot.revision.clone(),
            };
            checked.confirmation.get_or_insert_with(|| marking.clone());
            checked.baseline = Some(marking);
            direct_error.or_else(|| {
                zevria_workflow::validate_worker_synthesis_payload(
                    EnsembleWorkflow::Plan,
                    &checked.outcome(),
                    limit,
                )
                .err()
                .map(|error| error.to_string())
            })
        } else {
            None
        };
        if state.synthesis_error != error {
            self.commit_worker_transition(
                start,
                state,
                WorkerReviewEvent::PayloadChecked { error },
                None,
            )?;
        }
        Ok(())
    }

    fn commit_worker_transition(
        &mut self,
        start: &EnsembleStart,
        state: &mut WorkerReviewState,
        event: WorkerReviewEvent,
        result: Option<WorkerControlResult>,
    ) -> Result<(), Failure> {
        let mut next = state.clone();
        next.apply(&event).map_err(anyhow::Error::msg)?;
        self.record_required(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: state.descriptor.id.clone(),
            event: Box::new(event),
            result,
        }))?;
        *state = next;
        Ok(())
    }
}
fn abandon_actor(
    execution: &EnsembleReviewExecution,
    state: &WorkerReviewState,
    request_id: &WorkerControlId,
) {
    let delivered = execution
        .commands
        .get(&state.descriptor.id)
        .is_some_and(|actor| {
            actor
                .send(WorkerActorCommand::Abandon {
                    outcome: Box::new(state.outcome()),
                    request_id: request_id.clone(),
                })
                .is_ok()
        });
    if !delivered {
        tracing::debug!(worker = %state.descriptor.id, "abandoned worker actor already unavailable; durable exclusion stands");
    }
}

fn review_preflight_blocked(error: &anyhow::Error) -> SessionReplayError {
    // recover_review is read-only preflight/reconciliation. Failure here must
    // not start actors, commit recovery transitions, or imply restart repairs
    // an unreadable journal. Preserve typed history context through wrappers.
    let detail = if let Some(history) =
        error.downcast_ref::<zevria_transcript::transcript::UnsupportedHistory>()
    {
        history.to_string()
    } else {
        error.to_string()
    };
    SessionReplayError::Ensemble(format!(
        "{detail}; worker journal preflight is blocked. Preserve the existing root and worker logs and resolve the reported journal decoding/validation or access issue before resuming this root session. Do not resend dispatched feedback."
    ))
}

fn review_launch_error(error: anyhow::Error) -> SessionReplayError {
    if error
        .downcast_ref::<zevria_transcript::transcript::UnsupportedHistory>()
        .is_some()
    {
        review_preflight_blocked(&error)
    } else {
        review_interrupted(&error.to_string())
    }
}

fn review_interrupted(detail: &str) -> SessionReplayError {
    // Do not append Ensemble::Failed: accepted input and a committed seal must
    // remain recoverable when IO/actor delivery, rather than the user, stops us.
    SessionReplayError::Ensemble(format!(
        "{detail}; review is durably interrupted. Restart/resume this root session to recover without resending dispatched feedback."
    ))
}

#[test]
fn blocked_history_classification_survives_error_context_without_runtime_restart_advice() {
    let history = zevria_transcript::transcript::UnsupportedHistory::new(
        std::path::Path::new("worker.jsonl"),
        Some(642),
        "a valid current record",
    );
    let error = anyhow::Error::new(history).context("supervisor launch wrapper");
    let diagnostic = review_launch_error(error).to_string();
    assert!(diagnostic.contains("worker.jsonl:642"));
    assert!(diagnostic.contains("preflight is blocked"));
    assert!(!diagnostic.contains("Restart/resume"));
}

async fn publish_worker_review(
    events: &SessionEventSender,
    turn: &TurnContext,
    start: &EnsembleStart,
    state: &WorkerReviewState,
) {
    let _ = events
        .send(SessionEvent::WorkerReviewUpdated {
            target: WorkerControlTarget {
                turn_id: turn.id,
                run_id: start.run_id.clone(),
                worker_id: state.descriptor.id.clone(),
            },
            state: Box::new(state.clone()),
        })
        .await;
}

pub(crate) fn validate_frozen_workers(
    start: &EnsembleStart,
    outcomes: &[AgentRunOutcome],
) -> Result<(), String> {
    if start.agents.len() != outcomes.len() || outcomes.is_empty() {
        return Err("sealed worker set is incomplete".into());
    }
    if outcomes
        .iter()
        .all(|outcome| outcome.status == AgentRunStatus::Abandoned)
    {
        return Err("cannot seal an all-abandoned worker set".into());
    }
    if outcomes
        .iter()
        .filter(|outcome| {
            outcome
                .confirmation
                .as_ref()
                .is_some_and(|plan| plan.baseline.is_some())
        })
        .count()
        > 1
    {
        return Err("sealed worker set has multiple baselines".into());
    }
    let mut seen = std::collections::HashSet::new();
    for (descriptor, outcome) in start.agents.iter().zip(outcomes) {
        if descriptor != &outcome.descriptor || !seen.insert(&descriptor.id) {
            return Err("sealed worker set is partial, unordered, duplicate or foreign".into());
        }
        if outcome.is_sanitized_abandonment() {
            continue;
        }
        if outcome.partial || outcome.status != AgentRunStatus::Completed {
            return Err("sealed worker has an invalid terminal disposition".into());
        }
        if !outcome.confirmation.as_ref().is_some_and(|confirmed| {
            confirmed.validate(&descriptor.id)
                && confirmed.receipt.target.run_id == start.run_id
                && outcome.plan.as_ref() == Some(&confirmed.snapshot.plan)
        }) {
            return Err("Plan outcome lacks an exact explicit worker confirmation".into());
        }
    }
    Ok(())
}
