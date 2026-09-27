//! Capacity orchestration.

use super::*;

pub(super) enum CapacityCompactionState {
    Completed,
    PreparedForEdit,
    NotAttempted,
    Unavailable,
}

pub(super) async fn count_input_tokens_for_turn<P: ModelProvider>(
    provider: &mut P,
    state: &mut TurnInputCountState,
    request: ModelRequest<'_>,
    turn: &TurnContext,
) -> anyhow::Result<InputTokenCount> {
    if *state == (TurnInputCountState::Failed { turn_id: turn.id }) {
        return Ok(InputTokenCount::Unsupported);
    }
    // A new measurement supersedes any incompatible prepared request/outcome.
    *state = TurnInputCountState::Empty;
    if turn.is_cancelled() {
        anyhow::bail!("turn cancelled");
    }

    let count = provider.count_input_tokens(request);
    tokio::pin!(count);
    let result = tokio::select! {
        biased;
        () = turn.cancellation().cancelled() => return Err(anyhow::anyhow!("turn cancelled")),
        result = &mut count => result,
    };
    if result.is_err() {
        *state = TurnInputCountState::Failed { turn_id: turn.id };
    }
    result
}

/// Fingerprint the already-rendered stable prefix and filtered tools. Admission
/// shares these bytes with overhead estimation and the counted request.
pub(super) fn prepared_request_shape(
    policy: &TurnPolicy,
    context: &ModelContextPolicy,
    instructions: &str,
    tools: &[u8],
) -> RequestShapeFingerprint {
    let mut hash = Sha256::new();
    hash.update(request_shape_fingerprint(policy).0);
    // Match InstructionSet::identity's lowercase hex without allocating a
    // temporary String for every digest byte.
    let mut identity = [0_u8; 64];
    for (index, byte) in Sha256::digest(instructions.as_bytes()).iter().enumerate() {
        identity[2 * index] = b"0123456789abcdef"[usize::from(byte >> 4)];
        identity[2 * index + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
    }
    hash.update(identity);
    hash.update(serde_json::to_vec(context).expect("profile serializes"));
    hash.update(tools);
    RequestShapeFingerprint(hash.finalize().into())
}

pub(super) fn prepared_fixed_tokens(instructions: &str, tools: &[u8]) -> u64 {
    zevria_model::compaction::approximate_tokens(instructions).saturating_add(
        zevria_model::compaction::approximate_tokens_from_bytes(tools.len()),
    )
}

pub(super) fn prepared_request_identity(
    shape: RequestShapeFingerprint,
    instructions: &str,
    input: &[ModelRequestItem<'_>],
) -> anyhow::Result<RequestShapeFingerprint> {
    let mut hash = Sha256::new();
    hash.update(shape.0);
    hash.update(instructions.as_bytes());
    // Same canonical array encoding as snapshot_model_input, without holding
    // an owned copy of the complete history just to compute an identity.
    hash.update(b"[");
    for (index, item) in input.iter().enumerate() {
        if index != 0 {
            hash.update(b",");
        }
        hash.update(serde_json::to_vec(&item.to_owned_item()?)?);
    }
    hash.update(b"]");
    Ok(RequestShapeFingerprint(hash.finalize().into()))
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) fn context_policy(&self, role: ModelRole) -> &ModelContextPolicy {
        self.compaction.for_role(role)
    }

    pub(super) fn request_shape(
        &self,
        policy: &TurnPolicy,
    ) -> Result<RequestShapeFingerprint, SessionReplayError> {
        self.ensure_replay_valid()?;
        Ok(prepared_request_shape(
            policy,
            self.context_policy(policy.model_role),
            &self.rendered_instructions(policy),
            &self.filtered_tool_bytes(policy),
        ))
    }

    pub(super) fn logical_request_identity(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<RequestShapeFingerprint> {
        self.ensure_replay_valid()?;
        let instructions = self.rendered_instructions(policy);
        let shape = prepared_request_shape(
            policy,
            self.context_policy(policy.model_role),
            &instructions,
            &self.filtered_tool_bytes(policy),
        );
        prepared_request_identity(shape, &instructions, &self.conversation.model_input())
    }

    pub(super) fn filtered_tool_bytes(&self, policy: &TurnPolicy) -> Vec<u8> {
        let tools = self
            .tools
            .static_tool_defs()
            .into_iter()
            .filter(|tool| policy.allows_tool(&tool.name))
            .collect::<Vec<_>>();
        serde_json::to_vec(&tools).expect("tools serialize")
    }

    pub(super) fn fixed_input_tokens(&self, policy: &TurnPolicy) -> u64 {
        prepared_fixed_tokens(
            &self.rendered_instructions(policy),
            &self.filtered_tool_bytes(policy),
        )
    }

    pub(super) fn compactable_context_tokens(
        &self,
        policy: &TurnPolicy,
    ) -> Result<u64, SessionReplayError> {
        let profile = &self.context_policy(policy.model_role).profile;
        let shape = self.request_shape(policy)?;
        Ok(self
            .context
            .usage
            .get(profile)
            .and_then(|tracker| tracker.compactable_tokens_for(shape))
            .unwrap_or_else(|| {
                estimate_model_input_for_profile(self.conversation.model_input(), profile)
                    .unwrap_or_else(|_| estimate_model_input(self.conversation.model_input()))
                    .conservative_tokens
            }))
    }

    pub(super) fn projected_context_tokens(
        &self,
        policy: &TurnPolicy,
    ) -> Result<u64, SessionReplayError> {
        Ok(self
            .compactable_context_tokens(policy)?
            .saturating_add(self.fixed_input_tokens(policy)))
    }

    pub(super) fn report_provider_usage(
        &mut self,
        policy: &TurnPolicy,
        total_tokens: u64,
    ) -> Result<(), SessionReplayError> {
        let profile = self.context_policy(policy.model_role).profile.clone();
        let shape = self.request_shape(policy)?;
        let fixed_overhead = self.fixed_input_tokens(policy);
        self.context
            .usage
            .entry(profile)
            .or_default()
            .provider_reported(total_tokens, fixed_overhead, shape);
        Ok(())
    }

    pub(super) fn reestimate_context_usage(&mut self) {
        let mut usage = HashMap::new();
        for role in ModelRole::ALL {
            let profile = self.context_policy(role).profile.clone();
            if usage.contains_key(&profile) {
                continue;
            }
            let estimate =
                estimate_model_input_for_profile(self.conversation.model_input(), &profile)
                    .unwrap_or_else(|_| estimate_model_input(self.conversation.model_input()));
            usage.insert(
                profile,
                ContextUsageTracker {
                    appended_estimate: estimate,
                    ..ContextUsageTracker::default()
                },
            );
        }
        self.context.usage = usage;
    }

    pub(super) fn append_context_estimates(&mut self, items: &[TranscriptItem]) {
        if items
            .iter()
            .any(|item| matches!(item, TranscriptItem::Directive(_)))
        {
            self.context.last_snapshots.clear();
            self.invalidate_model_preview();
        }
        if items.iter().any(|item| item.model_request_item().is_some()) {
            self.context.invalidate_prepared_count();
        }
        let profiles = self.context.usage.keys().cloned().collect::<Vec<_>>();
        for profile in profiles {
            let estimate = items
                .iter()
                .filter_map(TranscriptItem::model_request_item)
                .map(|item| {
                    estimate_model_request_item_for_profile(item, &profile)
                        .unwrap_or_else(|_| estimate_model_request_item(item))
                })
                .fold(
                    ContextTokenEstimate::default(),
                    ContextTokenEstimate::saturating_add,
                );
            self.context
                .usage
                .entry(profile)
                .or_default()
                .appended_tokens(estimate);
        }
    }

    pub(super) fn local_context_candidates(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<(u64, u64, Option<u64>)> {
        let context = self.context_policy(policy.model_role);
        // Preview the next policy's body reconciliation without committing it
        // or calling a model. Its instruction set is counted as fixed overhead.
        let updates = self.reconcile_instruction_state(
            self.directive_state()?,
            policy,
            self.active_skills()?,
        )?;
        let mut input = self.conversation.model_input();
        input.extend(updates.iter().map(ModelRequestItem::DeveloperInstruction));
        let update_tokens = estimate_model_input(
            updates
                .iter()
                .map(ModelRequestItem::DeveloperInstruction)
                .collect(),
        )
        .conservative_tokens;
        let estimate = match self.provider.preflight_input(&context.profile, &input)? {
            zevria_model::models::ReplayPreflight::Compatible(estimate) => estimate,
            zevria_model::models::ReplayPreflight::ConversionRequired { sources } => anyhow::bail!(
                "destination {} cannot read opaque history from {}; use /model to select its source or confirm conversion, or start a fresh session",
                context.profile,
                sources
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let shape = self.request_shape(policy)?;
        let instructions = self.fixed_input_tokens(policy);
        let payload = estimate.payload_tokens.saturating_add(instructions);
        let conservative = estimate.conservative_tokens.saturating_add(instructions);
        let usage = self
            .context
            .usage
            .get(&context.profile)
            .and_then(|tracker| tracker.projected_tokens_for(shape, instructions))
            .map(|tokens| tokens.saturating_add(update_tokens));
        Ok((payload, conservative, usage))
    }

    pub(super) async fn assess_current_request(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        allow_exact_count: bool,
        force_exact_count: bool,
    ) -> anyhow::Result<ContextTokenSnapshot> {
        let context = self.context_policy(policy.model_role).clone();
        let (payload, conservative, usage) = self.local_context_candidates(policy)?;
        let identity = self.logical_request_identity(policy)?;
        let pending_exact =
            self.context
                .input_count
                .take_prepared(progress.turn().id, policy.model_role, identity);
        let trigger = self.compaction.trigger_tokens(policy.model_role);
        let input_limit = context.input_token_limit;
        let assessment = CapacityAssessment::from_candidates(
            CapacityCandidates {
                payload,
                conservative,
                usage,
                exact: pending_exact,
            },
            trigger,
            input_limit,
        );
        let (mut projected_input_tokens, mut source) = (assessment.tokens, assessment.source);
        let has_images = self
            .conversation
            .model_input()
            .into_iter()
            .filter_map(ModelRequestItem::message_ref)
            .any(zevria_content::prompt::message_has_images);
        if allow_exact_count
            && (has_images || force_exact_count || assessment.needs_exact())
            && pending_exact.is_none()
        {
            let input = self.conversation.model_input();
            let instructions = self.rendered_instructions(policy);
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
                progress.turn(),
            )
            .await
            {
                Ok(InputTokenCount::Exact(tokens)) => {
                    projected_input_tokens = tokens;
                    source = ContextTokenSource::Exact;
                }
                Ok(InputTokenCount::Unsupported) => {}
                Err(error) if progress.turn().is_cancelled() => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        profile = %context.profile,
                        role = policy.model_role.name(),
                        error = %error,
                        "exact input-token counting failed; using the local fallback"
                    );
                }
            }
        }

        let snapshot = ContextTokenSnapshot {
            profile: context.profile,
            model_role: policy.model_role,
            projected_input_tokens,
            source,
            automatic_trigger: trigger,
            input_token_limit: input_limit,
            context_window_tokens: context.context_window_tokens,
        };
        let changed = self.context.last_snapshots.get(&policy.model_role) != Some(&snapshot);
        if changed {
            self.context
                .last_snapshots
                .insert(policy.model_role, snapshot.clone());
            let _ = progress
                .events()
                .send(SessionEvent::ContextUsageUpdated {
                    turn_id: progress.turn().id,
                    snapshot: snapshot.clone(),
                })
                .await;
        }
        Ok(snapshot)
    }

    pub(super) fn dispatch_capacity_error(
        snapshot: &ContextTokenSnapshot,
        compaction: CapacityCompactionState,
    ) -> String {
        let measurement = if snapshot.source.is_estimated() {
            format!(
                "is estimated at {} input tokens ({})",
                snapshot.projected_input_tokens,
                snapshot.source.label()
            )
        } else {
            format!("requires {} input tokens", snapshot.projected_input_tokens)
        };
        let compaction = match compaction {
            CapacityCompactionState::Completed => {
                "automatic compaction completed, but could not shrink the rebuilt request enough"
            }
            CapacityCompactionState::PreparedForEdit => {
                "automatic compaction was prepared for the edited prefix, but its checkpoint was not installed because the rebuilt request still did not fit"
            }
            CapacityCompactionState::NotAttempted => {
                "automatic compaction was not attempted for this already-compacted request"
            }
            CapacityCompactionState::Unavailable => {
                "automatic compaction was unavailable because the request history had no compaction prompt"
            }
        };
        format!(
            "the prepared {} request {measurement}, exceeding the {}-token input limit for profile {} (physical context window: {} tokens); {compaction}",
            snapshot.model_role.name(),
            snapshot.input_token_limit,
            snapshot.profile,
            snapshot.context_window_tokens,
        )
    }
}

/// Bands are inclusive at the trigger and at the input limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapacityBand {
    Fits,
    AtTrigger,
    OverLimit,
}
impl CapacityBand {
    fn for_tokens(tokens: u64, trigger: u64, limit: u64) -> Self {
        if tokens > limit {
            Self::OverLimit
        } else if tokens >= trigger {
            Self::AtTrigger
        } else {
            Self::Fits
        }
    }
}
pub(super) struct CapacityCandidates {
    pub(super) payload: u64,
    pub(super) conservative: u64,
    pub(super) usage: Option<u64>,
    pub(super) exact: Option<u64>,
}
#[derive(Debug, Clone, Copy)]
pub(super) struct CapacityAssessment {
    pub(super) tokens: u64,
    pub(super) source: ContextTokenSource,
    pub(super) band: CapacityBand,
    pub(super) measurements_disagree: bool,
}
impl CapacityAssessment {
    pub(super) fn from_candidates(c: CapacityCandidates, trigger: u64, limit: u64) -> Self {
        let (tokens, source) = if let Some(tokens) = c.exact {
            (tokens, ContextTokenSource::Exact)
        } else if let Some(tokens) = c.usage {
            (tokens, ContextTokenSource::UsagePlusDelta)
        } else {
            (c.conservative, ContextTokenSource::ConservativeEstimate)
        };
        let payload_band = CapacityBand::for_tokens(c.payload, trigger, limit);
        let measurements_disagree = [Some(c.conservative), c.usage]
            .into_iter()
            .flatten()
            .any(|tokens| CapacityBand::for_tokens(tokens, trigger, limit) != payload_band);
        Self {
            tokens,
            source,
            band: CapacityBand::for_tokens(tokens, trigger, limit),
            measurements_disagree,
        }
    }
    pub(super) fn needs_exact(&self) -> bool {
        self.source != ContextTokenSource::Exact
            && (self.band != CapacityBand::Fits || self.measurements_disagree)
    }
}
