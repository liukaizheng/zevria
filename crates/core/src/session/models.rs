//! Idle-only model management. Checkpoint-first persistence is deliberate:
//! failure saving defaults may leave a useful portable checkpoint, never a
//! new route with history it cannot read.
use super::*;
use anyhow::Context as _;
use zevria_foundation::ReasoningLevel;
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
use zevria_model::models::ModelSelectionPreview;
use zevria_model::models::ModelSettingsService;
use zevria_model::models::ReplayPreflight;
use zevria_model::models::mode_role;
use zevria_model::models::{ModelSelection, ModelSelectionScope};

impl<P: ModelProvider> SessionEngine<P> {
    /// Local restoration telemetry for root modes, never a startup model call.
    pub fn restored_model_contexts(
        &self,
    ) -> std::result::Result<Vec<ContextTokenSnapshot>, SessionReplayError> {
        self.ensure_replay_valid()?;
        Ok([SessionMode::Build, SessionMode::Plan]
            .into_iter()
            .filter_map(|mode| {
                let policy = self.policies.policy(mode);
                let role = policy.model_role;
                let context = self.context_policy(role);
                let (_, projected_input_tokens, _) = self.local_context_candidates(policy).ok()?;
                Some(ContextTokenSnapshot {
                    profile: context.profile.clone(),
                    model_role: role,
                    projected_input_tokens,
                    source: ContextTokenSource::ConservativeEstimate,
                    automatic_trigger: self.compaction.trigger_tokens(role),
                    input_token_limit: context.input_token_limit,
                    context_window_tokens: context.context_window_tokens,
                })
            })
            .collect())
    }

    pub fn with_model_management(
        mut self,
        service: Arc<dyn ModelSettingsService>,
        revision: String,
    ) -> Self {
        self.clear_model_preview();
        let context = ModelManagementContext {
            service,
            revision,
            generation: 0,
            session_generation: zevria_transcript::transcript::pick_session_id(),
        };
        self.capabilities.models = Some(ModelManagement {
            context,
            preview: None,
        });
        self
    }

    pub(super) fn model_management(&self) -> Option<&ModelManagementContext> {
        self.capabilities
            .models
            .as_ref()
            .map(|models| &models.context)
    }
    pub(super) fn model_management_mut(&mut self) -> Option<&mut ModelManagementContext> {
        self.capabilities
            .models
            .as_mut()
            .map(|models| &mut models.context)
    }
    pub(super) fn model_preview(&self) -> Option<&ModelSelectionPreview> {
        self.capabilities
            .models
            .as_ref()
            .and_then(|models| models.preview.as_ref())
    }
    fn clear_model_preview(&mut self) {
        if let Some(models) = &mut self.capabilities.models {
            models.preview = None;
        }
    }

    fn cancel_model_preview(&mut self, request_id: &str) {
        if self
            .model_preview()
            .is_some_and(|preview| preview.request_id == request_id)
        {
            self.clear_model_preview();
        }
    }

    pub(super) fn set_model_preview(
        &mut self,
        preview: ModelSelectionPreview,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.phase, EnginePhase::Idle) && self.capabilities.models.is_some(),
            "model confirmation requires idle management"
        );
        self.capabilities
            .models
            .as_mut()
            .expect("management checked")
            .preview = Some(preview);
        Ok(())
    }

    pub(super) fn invalidate_model_preview(&mut self) {
        self.clear_model_preview();
        if let Some(models) = self.model_management_mut() {
            models.generation = models.generation.saturating_add(1);
        }
        self.context.count_failures.clear();
    }

    pub(super) fn model_management_allowed(&self) -> anyhow::Result<()> {
        self.ensure_replay_valid()?;
        anyhow::ensure!(
            !self.conversation.is_read_only(),
            "model changes are unavailable for read-only history"
        );
        anyhow::ensure!(
            !matches!(self.plan_state()?, PlanWorkflowState::Ready { .. }),
            "resolve the pending Plan approval before changing models"
        );
        anyhow::ensure!(
            self.conversation.persistence_error().is_none(),
            "repair transcript persistence before changing models"
        );
        anyhow::ensure!(
            self.model_management().is_some(),
            "model management is unavailable in this session"
        );
        Ok(())
    }

    pub(super) async fn manage_models(
        &mut self,
        request_id: String,
        request: Request,
        events: &SessionEventSender,
        cancellation: &CancellationToken,
    ) -> std::result::Result<(), SessionReplayError> {
        self.refresh_replay()?;
        let result = self
            .prepare_model_selection(&request_id, request, events, cancellation)
            .await;
        self.ensure_replay_valid()?;
        let result = result.unwrap_or_else(|error| {
            if cancellation.is_cancelled() {
                Result::Cancelled
            } else {
                Result::rejected("selection_failed", format!("{error:#}"))
            }
        });
        if matches!(
            result,
            Result::Catalog { .. } | Result::ConfirmationRequired(_)
        ) {
            // Publication may wait on lifecycle backpressure after preparation
            // finished. A consumed cancellation must still yield a terminal
            // reply, not a catalog/preview that strands the cancelling picker.
            // Lifecycle send reserves capacity before sequencing, so dropping
            // this nonterminal send cannot publish a partial/duplicate event.
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    self.cancel_model_preview(&request_id);
                    let _ = events.send(SessionEvent::ModelsResult {
                        request_id: request_id.clone(), result: Result::Cancelled,
                    }).await;
                }
                _ = events.send(SessionEvent::ModelsResult { request_id: request_id.clone(), result }) => {}
            }
        } else {
            // Never hide an already committed success or partial-save failure
            // behind cancellation, even if its acknowledgement was delayed.
            let _ = events
                .send(SessionEvent::ModelsResult { request_id, result })
                .await;
        }
        Ok(())
    }

    pub(super) async fn selection_fits(
        &mut self,
        context: &ModelContextPolicy,
        reasoning_level: ReasoningLevel,
        input: &[OwnedModelRequestItem],
        policy: &TurnPolicy,
        live_state: bool,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let borrowed = input
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        self.selection_fits_view(
            context,
            reasoning_level,
            &borrowed,
            policy,
            live_state,
            cancellation,
        )
        .await
    }

    pub(super) async fn selection_fits_view(
        &mut self,
        context: &ModelContextPolicy,
        reasoning_level: ReasoningLevel,
        input: &[ModelRequestItem<'_>],
        policy: &TurnPolicy,
        live_state: bool,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let mut projected = input.to_vec();
        let mut state = zevria_instructions::DirectiveState::default();
        for item in input {
            if let ModelRequestItem::DeveloperInstruction(directive) = item {
                state.apply(directive)?;
            }
        }
        let mut updates = Vec::new();
        if live_state {
            if state.snapshot().directives.is_empty() {
                updates = self.directive_state()?.snapshot().directives;
                for directive in &updates {
                    state.apply(directive)?;
                }
            }
            updates.extend(self.reconcile_instruction_state(
                &state,
                policy,
                self.active_skills()?,
            )?);
            projected.extend(updates.iter().map(ModelRequestItem::DeveloperInstruction));
        }
        let ReplayPreflight::Compatible(estimate) = self
            .provider
            .preflight_input(&context.profile, &projected)?
        else {
            anyhow::bail!(
                "profile {} cannot read this opaque checkpoint; select its exact source or start a fresh session",
                context.profile
            );
        };
        let instructions = if live_state {
            self.rendered_instructions(policy)
        } else {
            self.prepare_maintenance_guidance().render()
        };
        let overhead = if live_state {
            self.fixed_input_tokens(policy)
        } else {
            zevria_model::compaction::approximate_tokens(&instructions)
        };
        let estimate = estimate.conservative_tokens.saturating_add(overhead);
        if estimate < context.trigger_tokens(self.compaction.auto_trigger_percent()) {
            return Ok(true);
        }
        if self.context.count_failures.contains(&context.profile) {
            return Ok(estimate <= context.input_token_limit);
        }
        let request = ModelRequest {
            instructions: &instructions,
            input: projected,
            model_role: policy.model_role,
            allowed_tool_names: policy.allowed_tool_names.as_deref(),
        };
        let selection = ModelSelection::new(context.profile.clone(), reasoning_level);
        let count = {
            let count = self.provider.count_profile(&selection, request);
            tokio::pin!(count);
            tokio::select! { biased; () = cancellation.cancelled() => anyhow::bail!("model selection cancelled"), result = &mut count => result }
        };
        match count {
            Ok(InputTokenCount::Exact(tokens)) => Ok(tokens <= context.input_token_limit),
            Ok(InputTokenCount::Unsupported) => Ok(estimate <= context.input_token_limit),
            Err(error) => {
                self.context.count_failures.insert(context.profile.clone());
                tracing::warn!(%error, "model selection exact count failed; using conservative estimate");
                Ok(estimate <= context.input_token_limit)
            }
        }
    }

    async fn prepare_model_selection(
        &mut self,
        request_id: &str,
        request: Request,
        events: &SessionEventSender,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<Result> {
        if cancellation.is_cancelled() || matches!(request, Request::Cancel) {
            self.cancel_model_preview(request_id);
            return Ok(Result::Cancelled);
        }
        self.model_management_allowed()?;
        let models = self.model_management().expect("checked service").clone();
        let service = models.service.clone();
        service.validate(&models.revision)?;
        if let Request::List { mode, scope } = request {
            anyhow::ensure!(
                self.model_preview().is_none(),
                "a model conversion is awaiting confirmation; cancel it first"
            );
            return Ok(Result::Catalog {
                mode,
                scope,
                current: self
                    .conversation
                    .session_models()
                    .context("session model metadata is missing")?
                    .for_mode(mode)
                    .clone(),
                profiles: self.provider.model_catalog(),
                revision: models.revision.clone(),
            });
        }
        let (mode, scope, target, confirmed) = match request {
            Request::Select {
                mode,
                scope,
                target,
                revision,
            } => {
                anyhow::ensure!(
                    self.model_preview().is_none(),
                    "another selection awaits confirmation"
                );
                anyhow::ensure!(
                    revision == models.revision,
                    "model settings changed; reopen {} before selecting",
                    scope.command()
                );
                self.context.count_failures.clear();
                (mode, scope, target, None)
            }
            Request::Confirm { preview } => {
                anyhow::ensure!(
                    self.model_preview() == Some(&preview)
                        && preview.request_id == request_id
                        && preview.generation == models.generation
                        && preview.session_generation == models.session_generation
                        && preview.revision == models.revision,
                    "stale model confirmation; reopen {}",
                    self.model_preview()
                        .map_or(preview.scope, |stored| stored.scope)
                        .command()
                );
                self.clear_model_preview();
                (
                    preview.mode,
                    preview.scope,
                    preview.target.clone(),
                    Some(preview),
                )
            }
            Request::List { .. } | Request::Cancel => unreachable!(),
        };
        let role = mode_role(mode);
        let selections = self.conversation.session_models()
            .context("session model metadata is missing; explicitly recover this root before changing models")?
            .with_selection(mode, target.clone())?;
        let context = self.provider.prepare_model_update(role, &target)?;
        let prepared_compaction = self.compaction.with_role(role, context.clone())?;
        let current = self
            .conversation
            .session_models()
            .expect("checked metadata")
            .for_mode(mode)
            .clone();
        let same_selection = current == target;
        let same_profile = current.profile == target.profile;
        let policy = self.policies.policy(mode).clone();
        let history = snapshot_model_input(self.conversation.model_input())?;
        let borrowed = history
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        let preflight = self.provider.preflight_input(&target.profile, &borrowed)?;
        let conversion = match preflight {
            ReplayPreflight::ConversionRequired { sources } => {
                anyhow::ensure!(
                    sources.len() == 1,
                    "multiple opaque sources cannot be safely summarized together; select a compatible source or start a fresh session"
                );
                Some((
                    sources[0].clone(),
                    "The effective checkpoint contains source-model-only opaque context."
                        .to_string(),
                ))
            }
            ReplayPreflight::Compatible(_) => {
                if self
                    .selection_fits(
                        &context,
                        target.reasoning_level,
                        &history,
                        &policy,
                        true,
                        cancellation,
                    )
                    .await?
                {
                    None
                } else {
                    Some((
                        current.profile.clone(),
                        "The effective history exceeds the destination input limit.".to_string(),
                    ))
                }
            }
        };
        let checkpoint = if let Some((source, reason)) = conversion {
            let source_candidate = self.provider.model_catalog().into_iter().find(|entry| entry.context.profile == source)
                .ok_or_else(|| anyhow::anyhow!("source profile {source} is unavailable; restore its configured identity, stay with its source, or start a fresh session"))?;
            anyhow::ensure!(
                source_candidate
                    .reasoning_levels
                    .contains(&current.reasoning_level),
                "source profile {source} does not support the captured reasoning level {}; select that source with a supported level first, then retry conversion",
                current.reasoning_level
            );
            let source = ModelSelection::new(source, current.reasoning_level);
            let source_context = source_candidate.context;
            if confirmed.is_none() {
                let preview = ModelSelectionPreview {
                    request_id: request_id.into(),
                    generation: models.generation,
                    session_generation: models.session_generation.clone(),
                    mode,
                    scope,
                    target,
                    source,
                    revision: models.revision.clone(),
                    reason: format!(
                        "{reason} Conversion makes a model call, costs tokens, and can lose summary detail. It changes shared root context for both Build and Plan; {}",
                        match scope {
                            ModelSelectionScope::SessionOnly =>
                                "only this mode's session selection changes, saved for resume; config unchanged.",
                            ModelSelectionScope::SessionAndDefault =>
                                "only this mode's session selection and global default change.",
                        }
                    ),
                };
                self.set_model_preview(preview.clone())?;
                return Ok(Result::ConfirmationRequired(preview));
            }
            anyhow::ensure!(
                confirmed
                    .as_ref()
                    .is_some_and(|preview| preview.source == source),
                "conversion source changed; request a new preview"
            );
            service.validate(&models.revision)?;
            Some(
                self.convert_model_history(
                    (&source_context, &source),
                    (&context, &target),
                    &history,
                    &policy,
                    events,
                    cancellation,
                )
                .await?,
            )
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Ok(Result::Cancelled);
        }
        // No lock crosses counting or inference. Session-only commits cannot
        // rely on the config writer for this final stale-catalog check.
        if checkpoint.is_some() || scope == ModelSelectionScope::SessionOnly {
            service.validate(&models.revision)?;
        }
        if scope == ModelSelectionScope::SessionOnly && same_selection && checkpoint.is_none() {
            return Ok(Result::Changed {
                role,
                scope,
                context,
                reasoning_level: target.reasoning_level,
                snapshot: None,
                revision: models.revision.clone(),
                unchanged: true,
            });
        }
        // Synchronous short local commit: cancellation cannot interrupt the
        // checkpoint/default/header/route sequence after persistence begins.
        let checkpoint_installed = checkpoint.is_some();
        if let Some(checkpoint) = checkpoint {
            self.record_required(TranscriptItem::Compaction(checkpoint))?;
            self.reset_after_history_replacement()?;
        }
        let revision = match scope {
            // Never enter a configuration-writing transaction for this scope.
            ModelSelectionScope::SessionOnly => models.revision.clone(),
            ModelSelectionScope::SessionAndDefault => {
                let revision = match service.save(&models.revision, role, &target) {
                    Ok(revision) => revision,
                    Err(error) => {
                        return Ok(Result::Rejected {
                            code: "save_failed".into(),
                            message: if checkpoint_installed {
                                format!(
                                    "Portable checkpoint saved, but the active model and global default are unchanged: {error:#}. Retry /model to reuse the checkpoint without another summary call."
                                )
                            } else {
                                format!("Model and default unchanged: {error:#}")
                            },
                            checkpoint_installed,
                            current_revision: None,
                        });
                    }
                };
                // These are two durable files, not one transaction. Retain the
                // committed global revision even if the session header fails.
                self.model_management_mut()
                    .expect("management retained")
                    .revision = revision.clone();
                revision
            }
        };
        if let Err(error) = self.conversation.replace_session_models(selections) {
            return Ok(Result::Rejected {
                code: "session_save_failed".into(),
                message: match scope {
                    ModelSelectionScope::SessionOnly => format!(
                        "{}This session's active and durable model selections are unchanged; config was not modified: {error:#}. Retry /model-session after fixing transcript persistence.{}",
                        if checkpoint_installed {
                            "Portable checkpoint saved. "
                        } else {
                            ""
                        },
                        if checkpoint_installed {
                            " Retry reuses the checkpoint without another summary call."
                        } else {
                            ""
                        },
                    ),
                    ModelSelectionScope::SessionAndDefault => format!(
                        "{}The global default was saved, but this session's active and durable model selections are unchanged: {error:#}. Retry /model after fixing transcript persistence.",
                        if checkpoint_installed {
                            "Portable checkpoint saved. "
                        } else {
                            ""
                        },
                    ),
                },
                checkpoint_installed,
                current_revision: (scope == ModelSelectionScope::SessionAndDefault)
                    .then_some(revision),
            });
        }
        if same_profile && !checkpoint_installed {
            if !same_selection {
                self.provider.install_model_update(role, &target);
            }
            self.invalidate_model_preview();
            return Ok(Result::Changed {
                role,
                scope,
                context,
                reasoning_level: target.reasoning_level,
                snapshot: None,
                revision,
                unchanged: same_selection,
            });
        }
        if !same_selection {
            self.provider.install_model_update(role, &target);
        }
        self.compaction = prepared_compaction;
        self.context.count_failures.clear();
        self.invalidate_model_preview();
        // Same-profile reasoning changes must not reset sockets, cache state or
        // the cacheable input/instruction prefix. Request properties own continuation.
        if !same_profile || checkpoint_installed {
            self.provider.reset();
        }
        self.context.reset_for_model_update(role);
        self.reestimate_context_usage();
        let snapshot = ContextTokenSnapshot {
            profile: context.profile.clone(),
            model_role: role,
            projected_input_tokens: self.local_context_candidates(&policy).map_or_else(
                |_| self.projected_context_tokens(&policy),
                |(_, conservative, _)| Ok(conservative),
            )?,
            source: ContextTokenSource::ConservativeEstimate,
            automatic_trigger: self.compaction.trigger_tokens(role),
            input_token_limit: context.input_token_limit,
            context_window_tokens: context.context_window_tokens,
        };
        Ok(Result::Changed {
            role,
            scope,
            context,
            reasoning_level: target.reasoning_level,
            snapshot: Some(snapshot),
            revision,
            unchanged: false,
        })
    }

    async fn convert_model_history(
        &mut self,
        source: (&ModelContextPolicy, &ModelSelection),
        destination: (&ModelContextPolicy, &ModelSelection),
        history: &[OwnedModelRequestItem],
        policy: &TurnPolicy,
        events: &SessionEventSender,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<CompactionCheckpoint> {
        let (source, source_selection) = source;
        let (destination, destination_selection) = destination;
        let instructions = self.prepare_maintenance_guidance().render();
        let mut input = history
            .iter()
            .filter(|item| {
                !matches!(
                    item,
                    OwnedModelRequestItem::DeveloperInstruction(_)
                        | OwnedModelRequestItem::RequestInstruction(_)
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        input.push(OwnedModelRequestItem::message(Message::user(
            self.compaction.summary_prompt().to_string(),
        )));
        let summary_policy = TurnPolicy::new(
            "Summarize without tools",
            Some(Vec::new()),
            policy.model_role,
            false,
        );
        anyhow::ensure!(
            self.selection_fits(
                source,
                source_selection.reasoning_level,
                &input,
                &summary_policy,
                false,
                cancellation
            )
            .await?,
            "source profile {} cannot fit its effective input plus summary request; stay with its source model or start a fresh session",
            source.profile
        );
        let turn = TurnContext::new(
            TurnId::new(0),
            if policy.model_role == ModelRole::Plan {
                SessionMode::Plan
            } else {
                SessionMode::Build
            },
            cancellation.clone(),
        );
        let progress =
            ProgressReporter::silent_for_turn(events.clone(), turn, policy.model_role, source);
        let request = ModelRequest {
            instructions: &instructions,
            input: input
                .iter()
                .map(OwnedModelRequestItem::as_borrowed)
                .collect(),
            model_role: policy.model_role,
            allowed_tool_names: Some(&[]),
        };
        zevria_model::maintenance::validate_maintenance_input(&request.input)?;
        self.provider.reset();
        let result = {
            let completion = self
                .provider
                .complete_profile(source_selection, request, progress);
            tokio::pin!(completion);
            tokio::select! { biased; () = cancellation.cancelled() => None, result = &mut completion => Some(result) }
        };
        if result.is_none() {
            self.provider.cancel();
        }
        // Synthetic source traffic can never own a subsequent continuation.
        self.provider.reset();
        let response = result.ok_or_else(|| anyhow::anyhow!("model conversion cancelled"))??;
        anyhow::ensure!(
            assistant_tool_calls(response.message()).is_empty(),
            "source model returned tool calls instead of a summary; selection unchanged"
        );
        let summary = assistant_plain_text(response.message());
        anyhow::ensure!(
            !summary.trim().is_empty(),
            "source model returned an empty summary; selection unchanged"
        );
        let retained = select_recent_user_messages(
            &self.conversation.retained_user_candidates(),
            destination.retained_user_tokens,
        );
        let mut replacement = local_replacement_history(&retained, &summary);
        append_task_snapshot_context(
            &mut replacement,
            latest_successful_task_snapshot(self.conversation.items()).as_ref(),
        );
        anyhow::ensure!(
            self.selection_fits(
                destination,
                destination_selection.reasoning_level,
                &replacement,
                policy,
                true,
                cancellation
            )
            .await?,
            "portable summary still exceeds destination input limit; checkpoint and selection unchanged"
        );
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            replacement,
            retained,
        )
    }
}
