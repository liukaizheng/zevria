//! Staged prompt admission: preflight, persistence, preparation, measurement,
//! optional checkpoint/rebuild, authoritative commit, then acceptance events.

use super::*;
use std::borrow::Cow;
use zevria_content::UserPrompt;
use zevria_transcript::InstructionReplayState;

#[cfg(test)]
thread_local! {
    pub(super) static PROMPT_PLAN_PREFIX_REPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Admission-local only: never carried across counting, compaction, or commit.
/// Appends borrow the coherent committed state; edits own one prefix reduction.
pub(super) struct PromptAnchorState<'a> {
    pub(super) instructions: Cow<'a, InstructionReplayState>,
    plan: Cow<'a, PlanWorkflowState>,
    replaced_invocation: Option<&'a SkillInvocation>,
}

/// Checked before persistence repair. In particular, rejected orchestration
/// cannot repair or otherwise mutate the transcript as a side effect.
pub(super) struct PromptPreflight {
    anchor: TurnAnchor,
    mode: SessionMode,
    policy: TurnPolicy,
    input: PromptTurnInput,
}

/// Immutable, admission-local material shared by preparation and every distinct
/// measurement. No replay borrow or history payload survives preparation.
pub(super) struct PromptContext {
    mode: SessionMode,
    pub(super) policy: TurnPolicy,
    instructions: super::directives::InstructionPreparation,
    fixed_tokens: u64,
    limits: ModelContextPolicy,
    shape: RequestShapeFingerprint,
    request_boundaries: bool,
}

pub(super) enum PromptTurnInput {
    Message {
        text: UserPrompt,
        behavior: zevria_foundation::RequestBehavior,
    },
    Skill {
        name: SkillName,
        args: UserPrompt,
    },
    RevisionSkill {
        expected: PlanVersion,
        name: SkillName,
        args: UserPrompt,
    },
}

#[derive(Clone, Copy)]
pub(super) enum PromptTurnKind {
    Message,
    Skill,
}

/// The final ordered prompt suffix, prepared exactly once. Checkpoints are
/// separate because append compaction is durable even if this prompt is rejected.
pub(super) struct PreparedPrompt {
    pub(super) anchor: TurnAnchor,
    pub(super) records: Vec<TranscriptItem>,
    pub(super) display: Message,
    pub(super) kind: PromptTurnKind,
    pub(super) request: Option<zevria_foundation::RequestMetadata>,
    pub(super) plan_transition: Option<PlanRecord>,
}

impl PreparedPrompt {
    fn project<'a>(
        &'a self,
        committed: &'a [TranscriptItem],
        checkpoint: Option<&'a CompactionCheckpoint>,
    ) -> Vec<ModelRequestItem<'a>> {
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.projections += 1);
        let retained = match self.anchor {
            TurnAnchor::Append => committed,
            TurnAnchor::ReplaceFrom(index) => &committed[..index],
        };
        if let Some(checkpoint) = checkpoint {
            let mut input = zevria_transcript::transcript::model_input_with_checkpoint(
                retained.iter(),
                checkpoint,
            );
            input.extend(
                self.records
                    .iter()
                    .filter_map(TranscriptItem::model_request_item),
            );
            input
        } else {
            zevria_transcript::transcript::model_input_from_records(
                retained.iter().chain(self.records.iter()),
            )
        }
    }
}

pub(super) struct PromptMeasurement {
    assessment: CapacityAssessment,
    exact: Option<u64>,
    identity: Option<RequestShapeFingerprint>,
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Resolve a typed edit target without mutating transcript state.
    pub(super) fn resolve_edit_target(
        &self,
        target: &TranscriptEditTarget,
    ) -> Result<usize, String> {
        match target {
            TranscriptEditTarget::PromptOrdinal(prompt_ordinal) => self
                .conversation
                .prompt_position(*prompt_ordinal)
                .ok_or_else(|| {
                    format!("the edited prompt #{prompt_ordinal} is not in the session history")
                }),
            TranscriptEditTarget::EnsembleRun(run_id) => self
                .conversation
                .ensemble_start_position(run_id)
                .ok_or_else(|| format!("ensemble run {run_id} is not in the session history")),
        }
    }

    pub(super) fn prompt_anchor_state(
        &self,
        anchor: TurnAnchor,
    ) -> Result<PromptAnchorState<'_>, Rejection> {
        let (instructions, plan) = match &self.replay {
            SessionReplayState::Valid { instructions, plan } => (instructions, plan),
            SessionReplayState::Failed(error) => return Err(error.clone().into()),
        };
        match anchor {
            TurnAnchor::Append => Ok(PromptAnchorState {
                instructions: Cow::Borrowed(instructions),
                plan: Cow::Borrowed(plan),
                replaced_invocation: None,
            }),
            TurnAnchor::ReplaceFrom(index) => {
                let source = self.conversation.items().get(..index).ok_or_else(|| {
                    Rejection::Rejected("replacement index is outside the transcript".into())
                })?;
                let instructions = InstructionReplayState::replay(source).map_err(|error| {
                    format!(
                        "cannot edit a transcript with an invalid instruction lifecycle: {error:#}"
                    )
                })?;
                #[cfg(test)]
                PROMPT_PLAN_PREFIX_REPLAYS.set(PROMPT_PLAN_PREFIX_REPLAYS.get() + 1);
                #[cfg(feature = "test-support")]
                super::pipeline_probe::record(|counts| counts.plan_prefix_replays += 1);
                let plan = replay_plan_state(source.iter().filter_map(|item| match item {
                    TranscriptItem::Plan(record) => Some(record),
                    _ => None,
                }))
                .map_err(|error| {
                    format!("cannot edit a transcript with an invalid Plan workflow: {error:#}")
                })?;
                Ok(PromptAnchorState {
                    instructions: Cow::Owned(instructions),
                    plan: Cow::Owned(plan),
                    replaced_invocation: self.conversation.items().get(index).and_then(|item| {
                        match item {
                            TranscriptItem::SkillInvocation(invocation) => Some(invocation),
                            _ => None,
                        }
                    }),
                })
            }
        }
    }

    pub(super) fn preflight_prompt(
        &self,
        anchor: TurnAnchor,
        input: PromptTurnInput,
        mode: SessionMode,
    ) -> Result<PromptPreflight, Rejection> {
        self.ensure_replay_valid()?;
        let policy = self.policies.policy(mode).clone();
        if matches!(
            &input,
            PromptTurnInput::Message {
                behavior: zevria_foundation::RequestBehavior::Orchestrate,
                ..
            }
        ) {
            self.admit_orchestration(mode, &policy)?;
        }
        Ok(PromptPreflight {
            anchor,
            input,
            mode,
            policy,
        })
    }

    pub(super) fn prepare_prompt(
        &self,
        checked: PromptPreflight,
    ) -> Result<(PromptContext, PreparedPrompt), Rejection> {
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.prompt_preparations += 1);
        let PromptPreflight {
            anchor,
            mode,
            policy,
            input,
        } = checked;
        let instructions = self.instruction_preparation(&policy)?;
        let tools = self.filtered_tool_bytes(&policy);
        let limits = self.context_policy(policy.model_role).clone();
        let context = PromptContext {
            mode,
            fixed_tokens: prepared_fixed_tokens(&instructions.rendered, &tools),
            shape: prepared_request_shape(&policy, &limits, &instructions.rendered, &tools),
            policy,
            instructions,
            limits,
            // Use the committed session's capability/history, even if an edit
            // will discard the last historical typed boundary.
            request_boundaries: self.request_boundaries_enabled(),
        };
        let captured = self.prompt_anchor_state(anchor)?;
        let plan_transition = if let PromptTurnInput::RevisionSkill { expected, .. } = &input {
            if anchor != TurnAnchor::Append || mode != SessionMode::Plan {
                return Err("Ready Plan skill revisions must append in Plan mode"
                    .to_string()
                    .into());
            }
            let artifact = match captured.plan.as_ref() {
                PlanWorkflowState::Ready { artifact } if &artifact.version == expected => {
                    artifact.clone()
                }
                _ => {
                    return Err(Rejection::Rejected(
                        "the Ready Plan revision changed; refresh before invoking the skill".into(),
                    ));
                }
            };
            Some(PlanRecord::RevisionRequested { artifact })
        } else {
            self.prepare_prompt_plan_workflow(&captured.plan, mode)?
        };
        let mut before = context.instructions.prepare_updates(
            captured.instructions.directives(),
            captured.instructions.skills(),
        )?;
        let mut records = std::mem::take(&mut before.records);
        let (display, kind, request) = match input {
            PromptTurnInput::Message { text, behavior } => {
                text.validate().map_err(|error| error.to_string())?;
                if text.is_blank() {
                    return Err("a message turn requires text or an image"
                        .to_string()
                        .into());
                }
                let prompt = text.trimmed();
                let message = prompt.to_message();
                let request = context
                    .request_boundaries
                    .then(|| zevria_foundation::RequestMetadata::new(behavior));
                let display = if behavior == zevria_foundation::RequestBehavior::Orchestrate {
                    prompt.with_prefix("/orchestrate ").to_message()
                } else if request.is_some()
                    && matches!(prompt.blocks().first(), Some(zevria_content::PromptBlock::Text(text)) if text.starts_with('/') || text.starts_with('$'))
                {
                    prompt.with_prefix(" ").to_message()
                } else {
                    message.clone()
                };
                if let Some(request) = &request {
                    records.push(TranscriptItem::RequestPrompt {
                        message,
                        request: request.clone(),
                    });
                    records.push(TranscriptItem::RequestDirective(
                        zevria_instructions::RequestDirective::boundary(request.clone()),
                    ));
                } else {
                    records.push(TranscriptItem::Message(message));
                }
                (display, PromptTurnKind::Message, request)
            }
            PromptTurnInput::Skill { name, args }
            | PromptTurnInput::RevisionSkill { name, args, .. } => {
                args.validate().map_err(|error| error.to_string())?;
                let available = context.instructions.skill_activation_available();
                if !available {
                    return Err(
                        format!("the skill capability is unavailable in {mode} mode").into(),
                    );
                }
                // Only the replaced same-name first use may supply a discarded
                // snapshot. Every other pin comes from the retained prefix.
                let historical = captured
                    .replaced_invocation
                    .filter(|invocation| invocation.name() == &name)
                    .and_then(|invocation| match invocation.application() {
                        SkillApplication::Activate(snapshot) => Some(snapshot.clone()),
                        SkillApplication::Reapply(_) => None,
                    });
                let skills = self.captured_skill_context(captured.instructions.skills(), available);
                let application = super::skills::SkillApplicationPreparation {
                    context: &skills,
                    instructions: &context.instructions,
                    state: &before.state,
                    fixed_tokens: context.fixed_tokens,
                    input_token_limit: context.limits.input_token_limit,
                }
                .prepare(
                    &name,
                    zevria_instructions::skill::SkillInvocationOrigin::Explicit,
                    historical,
                )
                .map_err(|error| error.to_string())?;
                let invocation = SkillInvocation::new(name, args, application.application);
                let display = invocation.display_message();
                let request = context.request_boundaries.then(|| {
                    zevria_foundation::RequestMetadata::new(
                        zevria_foundation::RequestBehavior::Standard,
                    )
                });
                records.push(TranscriptItem::SkillInvocation(invocation));
                records.extend(
                    request
                        .clone()
                        .map(zevria_instructions::RequestDirective::boundary)
                        .map(TranscriptItem::RequestDirective),
                );
                // Reuse the post-invocation update that passed the skill capacity
                // check. Its body follows its owner, never pre-prompt reconciliation.
                records.extend(application.updates.records);
                (display, PromptTurnKind::Skill, request)
            }
        };
        before.ensure_capacity(&context.policy, &context.limits, context.fixed_tokens)?;
        // Validate only the prospective instruction tail. Final cross-domain
        // validation belongs to commit, after asynchronous counting/compaction.
        captured
            .instructions
            .into_owned()
            .apply_suffix(&records)
            .map_err(|error| error.to_string())?;
        tracing::debug!(transcript_index = anchor.replacement_index(), %mode, skill = matches!(kind, PromptTurnKind::Skill), "anchored prompt prepared");
        Ok((
            context,
            PreparedPrompt {
                anchor,
                records,
                display,
                kind,
                request,
                plan_transition,
            },
        ))
    }

    pub(super) fn skill_state_for_anchor(
        &self,
        anchor: TurnAnchor,
        invalid_prefix_context: &str,
    ) -> Result<ActiveSkills, String> {
        match anchor {
            TurnAnchor::Append => self
                .active_skills()
                .cloned()
                .map_err(|error| error.to_string()),
            TurnAnchor::ReplaceFrom(index) => {
                replay_active_skills(&self.conversation.items()[..index])
                    .map_err(|error| format!("{invalid_prefix_context}: {error:#}"))
            }
        }
    }

    pub(super) fn plan_state_for_anchor(
        &self,
        anchor: TurnAnchor,
        invalid_prefix_context: &str,
    ) -> Result<PlanWorkflowState, String> {
        match anchor {
            TurnAnchor::Append => self
                .plan_state()
                .cloned()
                .map_err(|error| error.to_string()),
            TurnAnchor::ReplaceFrom(index) => replay_plan_state(
                self.conversation.items()[..index]
                    .iter()
                    .filter_map(|item| match item {
                        TranscriptItem::Plan(record) => Some(record),
                        _ => None,
                    }),
            )
            .map_err(|error| format!("{invalid_prefix_context}: {error:#}")),
        }
    }

    pub(super) fn prepare_prompt_plan_workflow(
        &self,
        plan: &PlanWorkflowState,
        mode: SessionMode,
    ) -> Result<Option<PlanRecord>, String> {
        let transition = workflow_transition_for_mode(plan, mode)?;
        if let Some(record) = &transition {
            apply_plan_record(plan.clone(), record)
                .map_err(|error| format!("cannot construct the edited Plan workflow: {error:#}"))?;
        }
        Ok(transition)
    }

    pub(super) async fn edit_transcript(
        &mut self,
        edit: TranscriptEdit,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        let index = match self.resolve_edit_target(&edit.target) {
            Ok(index) => index,
            Err(error) => return self.finish_rejected(turn, events, error.into()).await,
        };
        let anchor = TurnAnchor::ReplaceFrom(index);
        let (input, mode) = match edit.replacement {
            TranscriptEditReplacement::Message {
                text,
                mode,
                behavior,
            } => (PromptTurnInput::Message { text, behavior }, mode),
            TranscriptEditReplacement::Skill { name, args, mode } => {
                (PromptTurnInput::Skill { name, args }, mode)
            }
            TranscriptEditReplacement::Ensemble { workflow, prompt } => {
                return self
                    .run_ensemble(anchor, workflow, prompt, events, turn)
                    .await;
            }
        };
        self.run_prompt_turn(anchor, input, mode, events, turn)
            .await
    }

    pub(super) async fn measure_prompt(
        &mut self,
        context: &PromptContext,
        prepared: &PreparedPrompt,
        checkpoint: Option<&CheckpointPlacement>,
        force_exact: bool,
        turn: &TurnContext,
    ) -> Result<PromptMeasurement, Rejection> {
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.measurements += 1);
        let policy = &context.policy;
        let limits = &context.limits;
        let folded = checkpoint.and_then(CheckpointPlacement::folded);
        // The only projection for this measurement version. These are borrowed
        // model items, shared by estimation, active-image detection, and counting.
        let input = prepared.project(self.conversation.items(), folded);
        let explicit = estimate_model_input_for_profile(input.clone(), &limits.profile)
            .map_err(|error| {
                format!(
                    "cannot project the prepared request for profile {}: {error:#}",
                    limits.profile
                )
            })?
            .saturating_add(ContextTokenEstimate::new(
                context.fixed_tokens,
                context.fixed_tokens,
            ));
        // Only a compatible append baseline merits estimating the addition.
        // Never carry old provider usage through a checkpoint rebuild.
        let baseline = if prepared.anchor == TurnAnchor::Append && checkpoint.is_none() {
            self.context.usage.get(&limits.profile).and_then(|tracker| {
                tracker.projected_tokens_for(context.shape, context.fixed_tokens)
            })
        } else {
            None
        };
        let usage = baseline.map(|existing| {
            let addition = prepared
                .records
                .iter()
                .filter_map(TranscriptItem::model_request_item)
                .map(|item| {
                    estimate_model_request_item_for_profile(item, &limits.profile)
                        .unwrap_or_else(|_| estimate_model_request_item(item))
                        .payload_tokens
                })
                .fold(0_u64, u64::saturating_add);
            existing.saturating_add(addition)
        });
        let candidates = |exact| CapacityCandidates {
            payload: explicit.payload_tokens,
            conservative: explicit.conservative_tokens,
            usage,
            exact,
        };
        let trigger = self.compaction.trigger_tokens(policy.model_role);
        let mut assessment = CapacityAssessment::from_candidates(
            candidates(None),
            trigger,
            limits.input_token_limit,
        );
        let has_images = input
            .iter()
            .copied()
            .filter_map(ModelRequestItem::message_ref)
            .any(zevria_content::prompt::message_has_images);
        let mut exact = None;
        if has_images || assessment.needs_exact() || force_exact {
            let request = ModelRequest {
                instructions: &context.instructions.rendered,
                input: input.clone(),
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
                Ok(InputTokenCount::Exact(tokens)) => {
                    exact = Some(tokens);
                    assessment = CapacityAssessment::from_candidates(
                        candidates(exact),
                        trigger,
                        limits.input_token_limit,
                    );
                }
                Ok(InputTokenCount::Unsupported) => {}
                Err(_) if turn.is_cancelled() => return Err(Rejection::Cancelled),
                Err(error) => {
                    tracing::warn!(profile = %limits.profile, role = policy.model_role.name(), %error, "exact prepared-prompt counting failed; using the local fallback")
                }
            }
        }
        let identity = exact.and_then(|_| {
            prepared_request_identity(context.shape, &context.instructions.rendered, &input).ok()
        });
        Ok(PromptMeasurement {
            assessment,
            exact,
            identity,
        })
        // The projection is dropped here, before any compaction can start.
    }

    fn ensure_prompt_fits(
        &self,
        context: &PromptContext,
        prepared: &PreparedPrompt,
        checkpoint: Option<&CheckpointPlacement>,
        measurement: &PromptMeasurement,
    ) -> Result<(), Rejection> {
        if measurement.assessment.band != CapacityBand::OverLimit {
            return Ok(());
        }
        let role = context.policy.model_role;
        let limits = &context.limits;
        let snapshot = ContextTokenSnapshot {
            profile: limits.profile.clone(),
            model_role: role,
            projected_input_tokens: measurement.assessment.tokens,
            source: measurement.assessment.source,
            automatic_trigger: self.compaction.trigger_tokens(role),
            input_token_limit: limits.input_token_limit,
            context_window_tokens: limits.context_window_tokens,
        };
        let available = match prepared.anchor {
            TurnAnchor::Append => self.conversation.has_compaction_prompt(),
            TurnAnchor::ReplaceFrom(index) => self.conversation.items()[..index]
                .iter()
                .any(zevria_transcript::transcript::is_compaction_prompt_item),
        };
        let state = match checkpoint {
            Some(CheckpointPlacement::FoldedIntoCommit(_)) => {
                CapacityCompactionState::PreparedForEdit
            }
            Some(CheckpointPlacement::InstalledBeforeCommit) => CapacityCompactionState::Completed,
            None if available => CapacityCompactionState::NotAttempted,
            None => CapacityCompactionState::Unavailable,
        };
        Err(Rejection::Rejected(format!(
            "{}; the prompt was not committed",
            Self::dispatch_capacity_error(&snapshot, state)
        )))
    }

    pub(super) async fn prepare_and_accept_prompt(
        &mut self,
        anchor: TurnAnchor,
        input: PromptTurnInput,
        mode: SessionMode,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<AcceptedTurn, Rejection> {
        let checked = self.preflight_prompt(anchor, input, mode)?;
        let role = checked.policy.model_role;
        let progress = ProgressReporter::for_turn(
            events.clone(),
            turn.clone(),
            role,
            self.context_policy(role),
        );
        self.gate_persistence(progress.events()).await?;
        let (context, prepared) = self.prepare_prompt(checked)?;
        let initial = self
            .measure_prompt(&context, &prepared, None, false, progress.turn())
            .await?;
        let due = self.context.is_armed(role) && initial.assessment.band != CapacityBand::Fits;
        // At most one checkpoint attempt. An unavailable checkpoint does not
        // change the request and must not repeat an Unsupported exact count.
        let checkpoint = self
            .automatic_checkpoint_for_prompt(
                prepared.anchor,
                &context.policy,
                due,
                progress.events(),
                progress.turn(),
            )
            .await?;
        let measurement = if checkpoint.is_some() {
            self.measure_prompt(
                &context,
                &prepared,
                checkpoint.as_ref(),
                initial.exact.is_some(),
                progress.turn(),
            )
            .await?
        } else {
            initial
        };
        self.ensure_prompt_fits(&context, &prepared, checkpoint.as_ref(), &measurement)?;

        // Commit one authoritative proposal against the post-compaction branch.
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        let attempted = checkpoint.is_some();
        let mut records = Vec::new();
        let metadata = if let Some(CheckpointPlacement::FoldedIntoCommit(checkpoint)) = checkpoint {
            let metadata = (checkpoint.trigger, checkpoint.backend);
            records.push(TranscriptItem::Compaction(checkpoint));
            Some(metadata)
        } else {
            None
        };
        records.extend(prepared.records);
        if let Some(record) = prepared.plan_transition {
            records.push(TranscriptItem::Plan(record));
        }
        let previous = match self.commit_anchored_records(prepared.anchor, records, context.mode) {
            Ok(previous) => previous,
            Err(error) => {
                let operation = match (prepared.anchor, prepared.kind) {
                    (TurnAnchor::Append, PromptTurnKind::Message) => "submitted prompt",
                    (TurnAnchor::Append, PromptTurnKind::Skill) => "skill invocation",
                    (TurnAnchor::ReplaceFrom(_), PromptTurnKind::Message) => "edited prompt",
                    (TurnAnchor::ReplaceFrom(_), PromptTurnKind::Skill) => {
                        "edited skill invocation"
                    }
                };
                return Err(self.acceptance_failed(operation, error, events).await);
            }
        };
        if let (Some(tokens), Some(identity)) = (measurement.exact, measurement.identity) {
            self.context.input_count = TurnInputCountState::Prepared {
                turn_id: turn.id,
                role: context.policy.model_role,
                identity,
                tokens,
            };
        }
        // Acceptance effects follow durability, in their established order.
        if let Some((trigger, backend)) = metadata {
            self.announce_compaction_completed(trigger, backend, events, turn)
                .await?;
        }
        self.emit_selected_mode(events).await;
        let _ = events
            .send(SessionEvent::TurnStarted {
                turn_id: turn.id,
                message: prepared.display,
                mode: context.mode,
            })
            .await;
        self.emit_plan_state_if_changed(previous, events).await?;
        Ok(AcceptedTurn {
            kind: AcceptedKind::Prompt { mode: context.mode },
            request: prepared.request,
            compaction_attempted_for_first_dispatch: attempted,
        })
    }

    pub(super) async fn run_prompt_turn(
        &mut self,
        anchor: TurnAnchor,
        input: PromptTurnInput,
        mode: SessionMode,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        match self
            .prepare_and_accept_prompt(anchor, input, mode, events, turn)
            .await
        {
            Err(rejection) => self.finish_rejected(turn, events, rejection).await,
            Ok(accepted) => {
                let outcome = self.run_turn(&accepted, events, turn).await;
                self.finish_accepted(&accepted, turn, events, outcome).await
            }
        }
    }
}
