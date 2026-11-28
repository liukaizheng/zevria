//! Capacity orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::accounting::CapacityCandidates;
use crate::session::kernel::request_plan::RequestPlan;
use crate::session::kernel::state::{RequestShapeFingerprint, request_shape_fingerprint};
use sha2::{Digest as _, Sha256};
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::policy::{ModelRole, TurnPolicy};
use zevria_instructions::DirectiveContent;
use zevria_model::ContextTokenEstimate;
use zevria_model::estimates::estimate_model_request_item;
use zevria_model::request::{InputTokenCount, ModelInputIdentity, ModelRequest, ModelRequestItem};
use zevria_model::telemetry::{ContextTokenSnapshot, ContextTokenSource};
use zevria_session_api::TurnEvent;
use zevria_session_api::event::{ProgressReporter, SessionEvent};
use zevria_session_api::provider::ModelProvider;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;

type LocalContextCandidates = (ContextTokenEstimate, Option<ContextTokenEstimate>);

fn fixed_input_tokens_for(plan: &RequestPlan) -> u64 {
    plan.fixed_tokens.saturating_add(
        plan.tool_update
            .as_ref()
            .and_then(TranscriptItem::model_request_item)
            .map(estimate_model_request_item)
            .map_or(0, |estimate| estimate.estimated_tokens),
    )
}

pub(in crate::session) enum CapacityCompactionState {
    Completed,
    PreparedForEdit,
    NotAttempted,
    Unavailable,
    DisabledForSideChat,
}

/// Fingerprint the already-rendered stable prefix and filtered tools. Admission
/// shares these bytes with overhead estimation and the counted request.
pub(in crate::session) fn prepared_request_shape(
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
    hash.update(zevria_foundation::image_accounting::IMAGE_ACCOUNTING_VERSION.to_be_bytes());
    hash.update(tools);
    RequestShapeFingerprint(hash.finalize().into())
}

pub(in crate::session) fn prepared_fixed_tokens(instructions: &str, tools: &[u8]) -> u64 {
    zevria_model::compaction::approximate_tokens(instructions).saturating_add(
        zevria_model::compaction::approximate_tokens_from_bytes(tools.len()),
    )
}

pub(in crate::session) fn prepared_request_identity(
    shape: RequestShapeFingerprint,
    instructions: &str,
    input: &[ModelRequestItem<'_>],
) -> anyhow::Result<RequestShapeFingerprint> {
    let identity = ModelInputIdentity::from_items(input)?;
    Ok(prepared_identity_from_digest(
        shape,
        instructions,
        identity.digest(),
    ))
}

fn prepared_identity_from_digest(
    shape: RequestShapeFingerprint,
    instructions: &str,
    digest: [u8; 32],
) -> RequestShapeFingerprint {
    let mut hash = Sha256::new();
    hash.update(shape.0);
    hash.update(instructions.as_bytes());
    hash.update(digest);
    RequestShapeFingerprint(hash.finalize().into())
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn context_policy(&self, role: ModelRole) -> &ModelContextPolicy {
        self.compaction.for_role(role)
    }

    pub(in crate::session) fn request_shape(
        &self,
        policy: &TurnPolicy,
    ) -> Result<RequestShapeFingerprint, SessionReplayError> {
        self.ensure_replay_valid()?;
        Ok(self.request_plan(policy).shape)
    }

    pub(in crate::session) fn logical_request_identity(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<RequestShapeFingerprint> {
        self.ensure_replay_valid()?;
        let prepared = self.request_plan(policy);
        let instructions = &prepared.instructions;
        let shape = prepared.shape;
        match self.conversation.input_identity() {
            Some(identity) => Ok(prepared_identity_from_digest(
                shape,
                instructions,
                identity.digest(),
            )),
            None => {
                prepared_request_identity(shape, instructions, &self.conversation.model_input())
            }
        }
    }

    /// Recorded additions are input; only a not-yet-recorded addition is fixed
    /// overhead. An Initial is already represented by request_tools.
    pub(in crate::session) fn fixed_input_tokens(&self, policy: &TurnPolicy) -> u64 {
        fixed_input_tokens_for(&self.request_plan(policy))
    }

    /// A checkpoint can discard the old declaration window. Only a freshly
    /// rendered desired set is irreducible; current/prospective declarations
    /// still count in full during ordinary request measurement.
    #[cfg(test)]
    pub(in crate::session) fn irreducible_input_tokens(
        &self,
        instructions: &str,
        policy: &TurnPolicy,
    ) -> u64 {
        zevria_model::compaction::approximate_tokens(instructions)
            .saturating_add(self.desired_tool_declarations(policy).approximate_tokens())
    }

    #[cfg(test)]
    pub(in crate::session) fn compactable_context_tokens(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<u64> {
        let context = self.context_policy(policy.model_role);
        let shape = self.request_shape(policy)?;
        let estimate = match self
            .context
            .usage
            .get(&context.profile)
            .and_then(|tracker| tracker.compactable_estimate_for(shape))
        {
            Some(estimate) => estimate,
            None => zevria_model::estimates::estimate_model_input_with_policy(
                &self.conversation.model_input(),
                &context.profile,
                &context.image_accounting,
            )?,
        };
        estimate
            .complete_tokens()
            .ok_or_else(|| anyhow::anyhow!("incomplete accounting"))
    }

    pub(in crate::session) fn report_provider_usage(
        &mut self,
        policy: &TurnPolicy,
        total_tokens: u64,
    ) -> Result<(), SessionReplayError> {
        let profile = self.context_policy(policy.model_role).profile.clone();
        let shape = self.request_shape(policy)?;
        let fixed_overhead = self.fixed_input_tokens(policy);
        self.context
            .on_provider_usage(profile, total_tokens, fixed_overhead, shape);
        Ok(())
    }

    /// Publish next-input coverage after a durably accepted completed response.
    /// Its total usage is a projection basis, never an exact next-input count.
    pub(in crate::session) async fn publish_usage_projection(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
    ) -> Result<(), SessionReplayError> {
        let context = self.context_policy(policy.model_role).clone();
        let shape = self.request_shape(policy)?;
        let Some(estimate) = self.context.projected_estimate(
            &context.profile,
            shape,
            self.fixed_input_tokens(policy),
        ) else {
            return Ok(());
        };
        let snapshot = ContextTokenSnapshot {
            profile: context.profile,
            model_role: policy.model_role,
            projected_input_tokens: estimate.complete_tokens(),
            known_input_tokens: estimate.known_tokens(),
            unresolved_media: estimate.unresolved_media,
            source: if estimate.complete_tokens().is_some() {
                ContextTokenSource::UsagePlusDelta
            } else {
                ContextTokenSource::IncompleteEstimate
            },
            automatic_trigger: self.compaction.trigger_tokens(policy.model_role),
            input_token_limit: context.input_token_limit,
            context_window_tokens: context.context_window_tokens,
        };
        if self.context.last_snapshots.get(&policy.model_role) != Some(&snapshot) {
            self.context
                .last_snapshots
                .insert(policy.model_role, snapshot.clone());
            let _ = progress
                .events()
                .send(SessionEvent::Turn(TurnEvent::ContextUsageUpdated {
                    turn_id: progress.turn().id,
                    snapshot: Box::new(snapshot),
                }))
                .await;
        }
        Ok(())
    }

    pub(in crate::session) fn reestimate_context_usage(&mut self) {
        let profiles = ModelRole::ALL.map(|role| self.context_policy(role).clone());
        self.context.reestimate(&self.conversation, profiles);
    }

    pub(in crate::session) fn append_committed_estimates(&mut self, range: std::ops::Range<usize>) {
        let provider = &self.provider;
        if self
            .context
            .on_committed(&self.conversation.items()[range], |profile| {
                provider.tool_append_preserves_prefix(profile)
            })
        {
            self.refresh_request_tools();
            self.invalidate_model_preview();
        }
    }

    #[cfg(feature = "test-support")]
    pub(in crate::session) fn append_context_estimates(&mut self, items: &[TranscriptItem]) {
        let provider = &self.provider;
        if self.context.on_committed(items, |profile| {
            provider.tool_append_preserves_prefix(profile)
        }) {
            self.refresh_request_tools();
            self.invalidate_model_preview();
        }
    }

    pub(in crate::session) fn local_context_candidates(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<LocalContextCandidates> {
        let plan = self.instruction_preparation(policy)?;
        let updates = plan.updates(self.directive_state()?, self.active_skills()?)?;
        self.local_context_candidates_for(
            &plan,
            &updates,
            self.conversation.model_input(),
            self.conversation.input_identity(),
        )
        .map(|(candidates, _)| candidates)
    }

    fn local_context_candidates_for<'a>(
        &self,
        plan: &'a RequestPlan,
        updates: &'a [DirectiveContent],
        mut input: Vec<ModelRequestItem<'a>>,
        committed_identity: Option<&'a ModelInputIdentity>,
    ) -> anyhow::Result<(LocalContextCandidates, Vec<ModelRequestItem<'a>>)> {
        let context = &plan.context;
        let policy = &plan.policy;
        let committed_len = input.len();
        input.extend(updates.iter().map(ModelRequestItem::DeveloperInstruction));
        input.extend(
            plan.tool_update
                .as_ref()
                .and_then(TranscriptItem::model_request_item),
        );
        let identity = if updates.is_empty() && plan.tool_update.is_none() {
            committed_identity
        } else {
            None
        };
        let request = ModelRequest {
            instructions: &plan.instructions,
            tools: &plan.tools,
            forbid_tool_calls: false,
            input,
            input_identity: identity,
            model_role: policy.model_role,
            allowed_tool_names: policy.allowed_tool_names.as_deref(),
        };
        let estimate = match self.provider.assess_request(context, &request)? {
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
        let update_estimate = zevria_model::estimates::estimate_model_input_with_policy(
            &request.input[committed_len..committed_len + updates.len()],
            &context.profile,
            &context.image_accounting,
        )?;
        let usage = if plan.tool_update.is_some()
            && !self.provider.tool_append_preserves_prefix(&context.profile)
        {
            None
        } else {
            self.context
                .projected_estimate(&context.profile, plan.shape, fixed_input_tokens_for(plan))
                .map(|estimate| estimate.saturating_add(update_estimate))
        };
        let mut input = request.input;
        input.truncate(committed_len);
        Ok(((estimate, usage), input))
    }

    pub(in crate::session) async fn assess_current_request(
        &mut self,
        policy: &TurnPolicy,
        progress: &ProgressReporter,
        allow_exact_count: bool,
        force_exact_count: bool,
    ) -> anyhow::Result<ContextTokenSnapshot> {
        let plan = self.instruction_preparation(policy)?;
        let context = &plan.context;
        let updates = plan.updates(self.directive_state()?, self.active_skills()?)?;
        let ((local, usage), input) = self.local_context_candidates_for(
            &plan,
            &updates,
            self.conversation.model_input(),
            self.conversation.input_identity(),
        )?;
        self.ensure_replay_valid()?;
        let identity = if let Some(identity) = self.conversation.input_identity() {
            prepared_identity_from_digest(plan.shape, &plan.instructions, identity.digest())
        } else {
            prepared_request_identity(plan.shape, &plan.instructions, &input)?
        };
        let pending_exact =
            self.context
                .input_count
                .take_prepared(progress.turn().id, policy.model_role, identity);
        let trigger = self.compaction.trigger_tokens(policy.model_role);
        let input_limit = context.input_token_limit;
        let assessment = self.context.assess(
            CapacityCandidates {
                local,
                usage,
                exact: pending_exact,
            },
            trigger,
            input_limit,
        );
        let (mut projected_input_tokens, mut source) = (assessment.tokens, assessment.source);
        let mut known_input_tokens = assessment.known_tokens;
        let mut unresolved_media = assessment.unresolved_media;
        let unmeasured_images = self.context.needs_image_count(
            &context.profile,
            local.image_count > 0 || !local.unresolved_media.is_empty(),
            usage.is_some(),
            false,
        );
        if allow_exact_count
            && (unmeasured_images || force_exact_count || assessment.needs_exact())
            && pending_exact.is_none()
        {
            let request = ModelRequest {
                input_identity: self.conversation.input_identity(),
                instructions: &plan.instructions,
                tools: &plan.tools,
                forbid_tool_calls: false,
                input,
                model_role: policy.model_role,
                allowed_tool_names: policy.allowed_tool_names.as_deref(),
            };
            match self
                .context
                .count_input_tokens(&mut self.provider, request, progress.turn())
                .await
            {
                Ok(count @ (InputTokenCount::Exact(_) | InputTokenCount::Approximate(_))) => {
                    let (tokens, measurement_source) = match count {
                        InputTokenCount::Exact(tokens) => (tokens, ContextTokenSource::Exact),
                        InputTokenCount::Approximate(tokens) => {
                            (tokens, ContextTokenSource::ProviderEstimate)
                        }
                        _ => unreachable!(),
                    };
                    projected_input_tokens = Some(tokens);
                    known_input_tokens = tokens;
                    unresolved_media = Default::default();
                    source = measurement_source;
                }
                Ok(InputTokenCount::Unsupported) => {
                    tracing::debug!(profile=%context.profile, count_endpoint="unsupported", "input counting unavailable; retaining local accounting");
                }
                Ok(InputTokenCount::Suppressed) => {
                    tracing::debug!(profile=%context.profile, count_endpoint="temporary_failure_suppressed", "input counting suppressed for this turn");
                }
                Err(error) if progress.turn().is_cancelled() => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        profile = %context.profile,
                        role = policy.model_role.name(),
                        error = %error,
                        "provider input-token counting failed; retaining local accounting (best effort if incomplete)"
                    );
                }
            }
        }

        let snapshot = ContextTokenSnapshot {
            profile: context.profile.clone(),
            model_role: policy.model_role,
            projected_input_tokens,
            known_input_tokens,
            unresolved_media,
            source,
            automatic_trigger: trigger,
            input_token_limit: input_limit,
            context_window_tokens: context.context_window_tokens,
        };
        let changed = self.context.last_snapshots.get(&policy.model_role) != Some(&snapshot);
        if changed {
            tracing::debug!(profile=%snapshot.profile,role=policy.model_role.name(),
                source=source.label(),projected_input_tokens=?snapshot.projected_input_tokens,
                known_input_tokens=snapshot.known_input_tokens,unresolved_media=?snapshot.unresolved_media,
                image_policy=context.image_accounting.label(),image_policy_version=context.image_accounting.version(),
                "prepared next-input assessment (not last response usage)");
            self.context
                .last_snapshots
                .insert(policy.model_role, snapshot.clone());
            let _ = progress
                .events()
                .send(SessionEvent::Turn(TurnEvent::ContextUsageUpdated {
                    turn_id: progress.turn().id,
                    snapshot: Box::new(snapshot.clone()),
                }))
                .await;
        }
        Ok(snapshot)
    }

    pub(in crate::session) fn dispatch_capacity_error(
        snapshot: &ContextTokenSnapshot,
        compaction: CapacityCompactionState,
    ) -> String {
        let measurement = if snapshot.projected_input_tokens.is_none() {
            format!(
                "has estimated known portion: {}; {} (best effort)",
                snapshot.known_input_tokens,
                snapshot.unresolved_media.description()
            )
        } else if snapshot.source.is_estimated() {
            format!(
                "is estimated at {} input tokens ({})",
                snapshot.known_input_tokens,
                snapshot.source.label()
            )
        } else {
            format!("requires {} input tokens", snapshot.known_input_tokens)
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
            CapacityCompactionState::DisabledForSideChat => {
                "automatic compaction is disabled for Side Chat to preserve its inherited prefix"
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
