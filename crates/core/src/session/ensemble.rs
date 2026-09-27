//! Ensemble orchestration.

use super::*;

/// Recover a root synthesis that durably reached its final no-tool response
/// before an older Zevria process could append the ensemble terminal marker.
/// Plan completion additionally requires either the accepted candidate or an
/// already-materialized Ready artifact, so an invalid no-submit response is
/// never mistaken for success.
pub(super) fn invalid_plan_outcomes(outcomes: &[AgentRunOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|outcome| {
            if outcome.is_sanitized_abandonment() {
                return false;
            }
            outcome.status != AgentRunStatus::Completed
                || outcome.partial
                || !outcome.confirmation.as_ref().is_some_and(|confirmed| {
                    confirmed.validate(&outcome.descriptor.id)
                        && outcome.plan.as_ref() == Some(&confirmed.snapshot.plan)
                })
        })
        .map(|outcome| {
            let mut detail = format!(
                "{}: status={}, partial={}, has_plan_proof={}",
                outcome.descriptor.label,
                outcome.status,
                outcome.partial,
                outcome.has_plan_proof()
            );
            if let Some(failure) = &outcome.failure {
                detail.push_str(&format!(", failure={failure}"));
            }
            detail
        })
        .collect()
}

pub(super) fn invalid_plan_summaries(summaries: &[AgentRunSummary]) -> Vec<String> {
    summaries
        .iter()
        .filter(|summary| {
            if summary.is_sanitized_abandonment() {
                return false;
            }
            summary.status != AgentRunStatus::Completed
                || summary.partial
                || !summary
                    .confirmation
                    .as_ref()
                    .is_some_and(|confirmed| confirmed.validate(&summary.descriptor.id))
        })
        .map(|summary| {
            let mut detail = format!(
                "{}: status={}, partial={}, has_plan_proof={}",
                summary.descriptor.label, summary.status, summary.partial, summary.has_plan_proof
            );
            if let Some(failure) = &summary.failure {
                detail.push_str(&format!(", failure={failure}"));
            }
            detail
        })
        .collect()
}

pub(super) fn recovered_ensemble_synthesis(
    items: &[TranscriptItem],
    run_id: &EnsembleRunId,
    workflow: EnsembleWorkflow,
) -> Result<Option<RecoveredEnsembleSynthesis>, zevria_workflow::ReportReconciliationError> {
    let gate = if workflow == EnsembleWorkflow::Plan {
        plan_submission_gate_after_reports(items, run_id)?
    } else {
        None
    };
    let Some(reports_index) = items.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                run_id: candidate,
                ..
            }) if candidate == run_id
        )
    }) else {
        return Ok(None);
    };
    let tail = &items[reports_index.saturating_add(1)..];
    let display_attempt_id = tail
        .iter()
        .rev()
        .find(|item| {
            item.message().is_some_and(|message| {
                matches!(message, Message::Assistant { .. })
                    && assistant_tool_calls(message).is_empty()
            })
        })
        .and_then(TranscriptItem::display_attempt_id)
        .map(str::to_owned);
    if workflow == EnsembleWorkflow::Plan
        && let Some(artifact) = tail.iter().rev().find_map(|item| match item {
            TranscriptItem::Plan(PlanRecord::Published { artifact, .. }) => Some(artifact.clone()),
            _ => None,
        })
    {
        return Ok(Some(RecoveredEnsembleSynthesis {
            candidate: None,
            ready: Some(artifact),
            display_attempt_id,
        }));
    }
    if tail
        .iter()
        .rev()
        .find_map(|item| {
            let message = item.message()?;
            matches!(message, Message::Assistant { .. })
                .then(|| {
                    assistant_tool_calls(message)
                        .is_empty()
                        .then(|| message.clone())
                })
                .flatten()
        })
        .is_none()
    {
        return Ok(None);
    }
    let ready = tail.iter().rev().find_map(|item| match item {
        TranscriptItem::Plan(PlanRecord::Published { artifact, .. }) => Some(artifact.clone()),
        _ => None,
    });
    let candidate = gate.and_then(|gate| gate.candidate);
    if workflow == EnsembleWorkflow::Plan && ready.is_none() && candidate.is_none() {
        return Ok(None);
    }
    Ok(Some(RecoveredEnsembleSynthesis {
        candidate,
        ready,
        display_attempt_id,
    }))
}

#[derive(Debug)]
pub(super) struct RecoveredEnsembleSynthesis {
    pub(super) display_attempt_id: Option<String>,
    pub(super) candidate: Option<PlanCandidate>,
    pub(super) ready: Option<PlanArtifact>,
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Check the future image-bearing root request without committing a seal or
    /// consuming worker confirmation. Incomplete workers contribute only known
    /// proof; confirmation repeats this check with the complete evidence set.
    pub(super) async fn preflight_potential_synthesis(
        &mut self,
        start: &EnsembleStart,
        states: &[WorkerReviewState],
        max_bytes_per_agent: usize,
        turn: &TurnContext,
    ) -> anyhow::Result<()> {
        let images =
            zevria_workflow::ensemble::potential_synthesis_images(&start.prompt, states, None)?;
        if !images.has_images() {
            return Ok(());
        }
        let outcomes = states
            .iter()
            .map(WorkerReviewState::outcome)
            .collect::<Vec<_>>();
        let known = outcomes
            .iter()
            .filter(|outcome| {
                outcome.has_plan_proof() && outcome.status != AgentRunStatus::Abandoned
            })
            .cloned()
            .collect::<Vec<_>>();
        let text = if known.is_empty() {
            format!(
                "{}\n\n{}",
                zevria_workflow::ensemble::UNTRUSTED_EVIDENCE_PREAMBLE,
                start.prompt.display_projection()
            )
        } else {
            zevria_workflow::ensemble::build_synthesis_input(
                start.workflow,
                &start.prompt.display_projection(),
                &known,
                max_bytes_per_agent,
            )?
        };
        // Reserve bounded source labels even for queued generations whose final
        // proof is not yet known. Never turn this estimate into durable evidence.
        let labels = "\nPotential worker feedback image source, generation and block reference.\n"
            .repeat(images.images().count());
        let message = images.with_prefix(format!("{text}{labels}")).to_message();
        self.preflight_synthesis_message(start, &message, &outcomes, turn)
            .await
    }

    pub(super) async fn preflight_synthesis_message(
        &mut self,
        start: &EnsembleStart,
        message: &Message,
        outcomes: &[AgentRunOutcome],
        turn: &TurnContext,
    ) -> anyhow::Result<()> {
        let record = TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: message.clone(),
            agents: outcomes.iter().map(AgentRunOutcome::summary).collect(),
        });
        anyhow::ensure!(
            serde_json::to_vec(&record)?.len()
                < zevria_transcript::transcript::MAX_ROOT_RECORD_BYTES,
            "synthesis record exceeds the 64 MiB record limit; input was not accepted"
        );
        // Reports and images remain bounded durable evidence, but a direct
        // publication never needs provider-specific root admission or counting.
        if start.publishes_confirmed_worker_plan() {
            return Ok(());
        }
        let policy = self.ensemble_policy(start.workflow);
        let context = self.context_policy(policy.model_role).clone();
        // Match actual synthesis dispatch: the reports enter history first,
        // then pending body revocations follow.
        let updates = self.reconcile_instruction_state(
            self.directive_state()?,
            &policy,
            self.active_skills()?,
        )?;
        let mut input = self.conversation.model_input();
        input.push(ModelRequestItem::Message(message));
        let boundary = self.standard_request_boundary();
        input.extend(
            boundary
                .as_ref()
                .and_then(TranscriptItem::model_request_item),
        );
        input.extend(updates.iter().map(ModelRequestItem::DeveloperInstruction));
        let estimate = estimate_model_input_for_profile(input.clone(), &context.profile)?;
        let mut tokens = estimate
            .conservative_tokens
            .saturating_add(self.fixed_input_tokens(&policy));
        let mut measurement = "estimated";
        let instructions = self.rendered_instructions(&policy);
        let request = ModelRequest {
            instructions: &instructions,
            input,
            model_role: policy.model_role,
            allowed_tool_names: policy.allowed_tool_names.as_deref(),
        };
        match count_input_tokens_for_turn(
            &mut self.provider,
            &mut self.context.input_count,
            request,
            turn,
        )
        .await
        {
            Ok(InputTokenCount::Exact(count)) => {
                tokens = count;
                measurement = "exact";
            }
            Ok(InputTokenCount::Unsupported) => {}
            Err(error) if turn.is_cancelled() => return Err(error),
            Err(error) => {
                tracing::warn!(%error, "synthesis exact input count failed; using approximate image accounting")
            }
        }
        anyhow::ensure!(
            tokens <= context.input_token_limit,
            "potential root synthesis requires {tokens} input tokens ({measurement}), exceeding the {} input-token limit; reduce evidence or compact root history before confirming; input was not accepted",
            context.input_token_limit
        );
        Ok(())
    }

    pub(super) fn prepare_ensemble_plan_workflow(
        &self,
        anchor: TurnAnchor,
        workflow: EnsembleWorkflow,
    ) -> Result<Option<PlanRecord>, String> {
        let prefix_state = self.plan_state_for_anchor(
            anchor,
            "cannot edit an ensemble in a transcript prefix with an invalid Plan workflow",
        )?;
        let transition = match workflow {
            EnsembleWorkflow::Plan => {
                workflow_transition_for_mode(&prefix_state, SessionMode::Plan)?
            }
            EnsembleWorkflow::Review => {
                if let PlanWorkflowState::Ready { artifact } = &prefix_state {
                    return Err(format!(
                        "Plan {} is awaiting a decision; revise or approve it before running a review",
                        artifact.version
                    ));
                }
                None
            }
        };
        if let Some(record) = &transition {
            apply_plan_record(prefix_state, record).map_err(|error| {
                format!("cannot construct the replacement Plan workflow: {error:#}")
            })?;
        }
        Ok(transition)
    }
    pub(super) fn ensemble_policy(&self, workflow: EnsembleWorkflow) -> TurnPolicy {
        match workflow {
            EnsembleWorkflow::Plan => TurnPolicy::new(
                ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS,
                Some(vec![
                    zevria_foundation::WEB_SEARCH_TOOL_NAME.to_string(),
                    "command".to_string(),
                    RECONCILE_REPORTS_TOOL_NAME.to_string(),
                    QUESTION_TOOL_NAME.to_string(),
                    SUBMIT_PLAN_TOOL_NAME.to_string(),
                ]),
                ModelRole::Plan,
                false,
            )
            .with_scope("synthesis:plan")
            .with_contract(WorkspaceContract::SourceReadOnlyScratch),
            EnsembleWorkflow::Review => TurnPolicy::new(
                ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS,
                Some(vec![
                    "command".to_string(),
                    zevria_foundation::WEB_SEARCH_TOOL_NAME.to_string(),
                ]),
                ModelRole::Review,
                false,
            )
            .with_scope("synthesis:review")
            .with_contract(WorkspaceContract::SourceReadOnlyScratch),
        }
    }
}
impl<P: ModelProvider> SessionEngine<P> {
    async fn accept_ensemble(
        &mut self,
        anchor: TurnAnchor,
        workflow: EnsembleWorkflow,
        prompt: zevria_content::UserPrompt,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(AcceptedTurn, EnsembleStart), Rejection> {
        prompt.validate().map_err(|error| error.to_string())?;
        if prompt.is_blank() {
            return Err(Rejection::Rejected(format!(
                "{} requires a non-empty prompt",
                workflow.slash_command()
            )));
        }
        self.gate_persistence(events).await?;
        self.skill_state_for_anchor(
            anchor,
            "cannot edit an ensemble in a transcript prefix with an invalid skill lifecycle",
        )?;
        let plan = self.prepare_ensemble_plan_workflow(anchor, workflow)?;
        let launcher = self
            .capabilities
            .ensemble_launcher
            .as_ref()
            .ok_or_else(|| {
                Rejection::Rejected("ensemble workflows are not configured for this session".into())
            })?;
        let agents = launcher.workers(workflow)?;
        if agents.is_empty() {
            return Err(Rejection::Rejected(format!(
                "{workflow} has no configured workers"
            )));
        }
        let start = EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow,
            prompt,
            agents,
        };
        let mut records = vec![TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        })];
        if workflow == EnsembleWorkflow::Plan {
            records.push(TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }));
        }
        if let Some(record) = plan {
            records.push(TranscriptItem::Plan(record));
        }
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        let previous = match self.commit_anchored_records(
            anchor,
            records,
            match workflow {
                EnsembleWorkflow::Plan => SessionMode::Plan,
                EnsembleWorkflow::Review => SessionMode::Build,
            },
        ) {
            Ok(previous) => previous,
            Err(error) => {
                return Err(self
                    .acceptance_failed("ensemble start", error, events)
                    .await);
            }
        };
        self.emit_selected_mode(events).await;
        let accepted = AcceptedTurn::new(AcceptedKind::Ensemble {
            run_id: start.run_id.clone(),
            workflow,
            resumed: false,
        });
        let _ = events
            .send(SessionEvent::EnsembleStarted {
                turn_id: turn.id,
                start: start.clone(),
                resumed: false,
            })
            .await;
        self.emit_plan_state_if_changed(previous, events).await?;
        Ok((accepted, start))
    }

    pub(super) async fn run_ensemble(
        &mut self,
        anchor: TurnAnchor,
        workflow: EnsembleWorkflow,
        prompt: zevria_content::UserPrompt,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        match self
            .accept_ensemble(anchor, workflow, prompt, events, turn)
            .await
        {
            Err(rejection) => self.finish_rejected(turn, events, rejection).await,
            Ok((accepted, start)) => {
                let outcome = self.launch_ensemble(&accepted, start, events, turn).await;
                self.finish_accepted(&accepted, turn, events, outcome).await
            }
        }
    }

    pub(super) async fn resume_ensemble(
        &mut self,
        recovery: EnsembleRecovery,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        let accepted = AcceptedTurn::new(AcceptedKind::Ensemble {
            run_id: recovery.start.run_id.clone(),
            workflow: recovery.start.workflow,
            resumed: true,
        });
        let _ = events
            .send(SessionEvent::EnsembleStarted {
                turn_id: turn.id,
                start: recovery.start.clone(),
                resumed: true,
            })
            .await;
        let outcome = self
            .resume_accepted_ensemble(&accepted, recovery, events, turn)
            .await;
        self.finish_accepted(&accepted, turn, events, outcome).await
    }

    async fn resume_accepted_ensemble(
        &mut self,
        accepted: &AcceptedTurn,
        recovery: EnsembleRecovery,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        if turn.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if let Some((_message, agents)) = recovery.reports_ready {
            if recovery.start.workflow == EnsembleWorkflow::Plan {
                let invalid = invalid_plan_summaries(&agents);
                if !invalid.is_empty() {
                    return Err(anyhow::anyhow!("cannot recover legacy Ensemble Plan ReportsReady without valid final-plan proof metadata for every participating worker: {}", invalid.join("; ")).into());
                }
            }
            let _ = events
                .send(SessionEvent::EnsembleReportsReady {
                    turn_id: turn.id,
                    run_id: recovery.start.run_id.clone(),
                    agents,
                })
                .await;
            if recovery.start.publishes_confirmed_worker_plan() {
                return self
                    .publish_confirmed_worker_plan(accepted, &recovery.start, events, turn)
                    .await;
            }
            if let Some(completed) = recovered_ensemble_synthesis(
                self.conversation.items(),
                &recovery.start.run_id,
                recovery.start.workflow,
            )
            .map_err(anyhow::Error::from)?
            {
                return self
                    .finish_recovered_ensemble_synthesis(accepted, completed, events, turn)
                    .await;
            }
            self.run_ensemble_synthesis(accepted, events, turn).await
        } else {
            self.launch_ensemble(accepted, recovery.start, events, turn)
                .await
        }
    }

    async fn launch_ensemble(
        &mut self,
        accepted: &AcceptedTurn,
        start: EnsembleStart,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let launcher = self.capabilities.ensemble_launcher.clone().ok_or_else(|| {
            anyhow::anyhow!("ensemble workflows are not configured for this session")
        })?;
        let resume = matches!(accepted.kind, AcceptedKind::Ensemble { resumed: true, .. });
        let outcomes = if start.workflow == EnsembleWorkflow::Plan {
            self.review_plan_workers(launcher.clone(), &start, resume, events, turn)
                .await?
        } else {
            launcher
                .launch(
                    EnsembleLaunchRequest {
                        start: start.clone(),
                        resume,
                    },
                    events.clone(),
                    turn.clone(),
                )
                .await
                .map_err(|error| Failure::from(error).during_work(turn))?
        };
        for outcome in &outcomes {
            let _ = events
                .send(SessionEvent::AgentRunFinished {
                    turn_id: turn.id,
                    ensemble_run_id: start.run_id.clone(),
                    outcome: outcome.clone(),
                })
                .await;
        }
        if turn.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if start.workflow == EnsembleWorkflow::Plan {
            let invalid = invalid_plan_outcomes(&outcomes);
            if !invalid.is_empty() {
                return Err(anyhow::anyhow!("Ensemble Plan requires every participating worker to complete with durable final Markdown proof: {}", invalid.join("; ")).into());
            }
        } else if !outcomes.iter().any(AgentRunOutcome::has_usable_evidence) {
            let failures = outcomes
                .iter()
                .map(|outcome| {
                    format!(
                        "{}: {}",
                        outcome.descriptor.label,
                        outcome.failure.as_deref().unwrap_or("no usable report")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow::anyhow!(
                "all ensemble workers failed or returned no usable evidence: {failures}"
            )
            .into());
        }
        let reviews = zevria_transcript::project_worker_reviews(self.conversation.items())
            .map_err(anyhow::Error::msg)?;
        let synthesis = zevria_workflow::ensemble::build_synthesis_prompt_with_feedback(
            start.workflow,
            &start.prompt,
            &outcomes,
            reviews.get(&start.run_id).map_or(&[], Vec::as_slice),
            launcher.max_synthesis_bytes_per_agent(),
        )?;
        if start.publishes_confirmed_worker_plan()
            || zevria_content::prompt::message_has_images(&synthesis)
        {
            self.preflight_synthesis_message(&start, &synthesis, &outcomes, turn)
                .await?;
        }
        let summaries = outcomes
            .iter()
            .map(AgentRunOutcome::summary)
            .collect::<Vec<_>>();
        let mut records = vec![TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: synthesis,
            agents: summaries.clone(),
        })];
        records.extend(self.standard_request_boundary());
        if let Err(error) = self.record_required_items(records) {
            self.persistence_failed(&error, events).await?;
            return Err(error.context("failed to persist ensemble reports").into());
        }
        let _ = events
            .send(SessionEvent::EnsembleReportsReady {
                turn_id: turn.id,
                run_id: start.run_id.clone(),
                agents: summaries,
            })
            .await;
        if start.publishes_confirmed_worker_plan() {
            return self
                .publish_confirmed_worker_plan(accepted, &start, events, turn)
                .await;
        }
        self.run_ensemble_synthesis(accepted, events, turn).await
    }

    async fn publish_confirmed_worker_plan(
        &mut self,
        accepted: &AcceptedTurn,
        start: &EnsembleStart,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        if turn.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        self.ensure_replay_valid()?;
        if !start.publishes_confirmed_worker_plan() {
            return Err(
                anyhow::anyhow!("direct publication requires a single-worker Plan run").into(),
            );
        }
        let published = self.conversation.items().iter().any(|item| {
            matches!(item,
                TranscriptItem::Plan(PlanRecord::Published {
                    provenance: PlanPublicationProvenance::ConfirmedWorker { run_id, .. }, ..
                }) if run_id == &start.run_id
            )
        });
        let completion = if published {
            TurnFinalization {
                publication: None,
                retained: Vec::new(),
                usage: None,
                terminal: SessionEvent::TurnRecovered {
                    turn_id: turn.id,
                    display_attempt_id: None,
                },
                defer_publication: true,
            }
        } else {
            // The root seal is the only content authority. Never read a mutable
            // worker artifact, report prose, or a sidecar during publication.
            let outcomes = self
                .conversation
                .items()
                .iter()
                .find_map(|item| match item {
                    TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
                        run_id,
                        outcomes,
                        ..
                    }) if run_id == &start.run_id => Some(outcomes),
                    _ => None,
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("direct publication requires a durable worker seal")
                })?;
            worker_review::validate_frozen_workers(start, outcomes).map_err(anyhow::Error::msg)?;
            let outcome = &outcomes[0];
            let confirmation = outcome
                .confirmation
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("direct worker is not confirmed"))?;
            let markdown = confirmation
                .snapshot
                .plan
                .markdown
                .clone()
                .ok_or_else(|| anyhow::anyhow!("confirmed worker has no Markdown proof"))?;
            let artifact = DirectWorkerPlan::validate(markdown)
                .map_err(anyhow::Error::from)?
                .into_artifact(self.next_plan_version()?, turn.id);
            let record = PlanRecord::Published {
                artifact: artifact.clone(),
                provenance: PlanPublicationProvenance::ConfirmedWorker {
                    run_id: start.run_id.clone(),
                    worker_id: outcome.descriptor.id.clone(),
                    revision: confirmation.snapshot.revision.clone(),
                },
            };
            let message = Message::assistant(
                "Published the explicitly confirmed worker plan. Implementation still requires an explicit implementation command.",
            );
            TurnFinalization {
                publication: Some((record, artifact)),
                retained: vec![TranscriptItem::Message(message.clone())],
                usage: None,
                terminal: SessionEvent::TurnCompleted {
                    turn_id: turn.id,
                    display_attempt_id: None,
                    message,
                },
                defer_publication: true,
            }
        };
        self.commit_turn_completion(accepted, completion, events)
            .await
    }

    async fn run_ensemble_synthesis(
        &mut self,
        accepted: &AcceptedTurn,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let AcceptedKind::Ensemble {
            workflow, run_id, ..
        } = &accepted.kind
        else {
            unreachable!("ensemble synthesis acceptance")
        };
        let policy = self.ensemble_policy(*workflow);
        let progress = ProgressReporter::for_turn(
            events.clone(),
            turn.clone(),
            policy.model_role,
            self.context_policy(policy.model_role),
        );
        let gate = if *workflow == EnsembleWorkflow::Plan {
            plan_submission_gate_after_reports(self.conversation.items(), run_id)
                .map_err(anyhow::Error::from)?
                .unwrap_or_else(|| {
                    PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default())
                })
        } else {
            PlanSubmissionGate::inert(None)
        };
        let output = self
            .run_model_tool_loop(
                accepted,
                &policy,
                progress,
                gate,
                FinalResponsePersistence::Deferred,
            )
            .await
            .map_err(|failure| failure.during_work(turn))?;
        self.finalize_turn(
            accepted,
            output,
            FinalResponsePersistence::Deferred,
            events,
            turn,
        )
        .await
    }

    async fn finish_recovered_ensemble_synthesis(
        &mut self,
        accepted: &AcceptedTurn,
        completed: RecoveredEnsembleSynthesis,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<TurnCompletion, Failure> {
        let candidate = if completed.ready.is_some() {
            None
        } else {
            completed.candidate
        };
        let output = ModelTurnOutput {
            display_attempt_id: completed.display_attempt_id,
            message: None,
            submission_gate: PlanSubmissionGate::inert(candidate),
            final_item: None,
            usage: None,
        };
        self.finalize_turn(
            accepted,
            output,
            FinalResponsePersistence::Recovered,
            events,
            turn,
        )
        .await
    }
}
