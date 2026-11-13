//! Capacity orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::accounting::CapacityCandidates;
use crate::session::kernel::request_plan::HistoryView;
use crate::session::kernel::state::{RequestShapeFingerprint, request_shape_fingerprint};
use sha2::{Digest as _, Sha256};
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::policy::{ModelRole, TurnPolicy};
use zevria_model::estimates::{estimate_model_input, estimate_model_request_item};
use zevria_model::request::{InputTokenCount, ModelInputIdentity, ModelRequest, ModelRequestItem};
use zevria_model::telemetry::{ContextTokenSnapshot, ContextTokenSource};
use zevria_session_api::event::{ProgressReporter, SessionEvent};
use zevria_session_api::provider::ModelProvider;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;

pub(in crate::session) enum CapacityCompactionState {
    Completed,
    PreparedForEdit,
    NotAttempted,
    Unavailable,
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
        let plan = self.request_plan(policy);
        plan.fixed_tokens.saturating_add(
            plan.tool_update
                .as_ref()
                .and_then(TranscriptItem::model_request_item)
                .map(estimate_model_request_item)
                .map_or(0, |estimate| estimate.conservative_tokens),
        )
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

    pub(in crate::session) fn compactable_context_tokens(
        &self,
        policy: &TurnPolicy,
    ) -> Result<u64, SessionReplayError> {
        let profile = &self.context_policy(policy.model_role).profile;
        let shape = self.request_shape(policy)?;
        Ok(self
            .context
            .compactable_tokens(&self.conversation, profile, shape))
    }

    pub(in crate::session) fn projected_context_tokens(
        &self,
        policy: &TurnPolicy,
    ) -> Result<u64, SessionReplayError> {
        Ok(self
            .compactable_context_tokens(policy)?
            .saturating_add(self.fixed_input_tokens(policy)))
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

    pub(in crate::session) fn reestimate_context_usage(&mut self) {
        let profiles = ModelRole::ALL.map(|role| self.context_policy(role).profile.clone());
        self.context.reestimate(&self.conversation, profiles);
    }

    pub(in crate::session) fn append_committed_estimates(&mut self, range: std::ops::Range<usize>) {
        if self.context.on_committed(&self.conversation.items()[range]) {
            self.refresh_request_tools();
            self.invalidate_model_preview();
        }
    }

    #[cfg(feature = "test-support")]
    pub(in crate::session) fn append_context_estimates(&mut self, items: &[TranscriptItem]) {
        if self.context.on_committed(items) {
            self.refresh_request_tools();
            self.invalidate_model_preview();
        }
    }

    pub(in crate::session) fn local_context_candidates(
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
        let identity = updates
            .is_empty()
            .then(|| self.conversation.input_identity())
            .flatten();
        let estimate = match self.provider.preflight_input_with_identity(
            &context.profile,
            &input,
            identity,
        )? {
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
            .projected_tokens(&context.profile, shape, instructions)
            .map(|tokens| tokens.saturating_add(update_tokens));
        Ok((payload, conservative, usage))
    }

    pub(in crate::session) async fn assess_current_request(
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
        let assessment = self.context.assess(
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
        let unmeasured_images = self.context.needs_image_count(
            &context.profile,
            self.conversation.has_images(),
            usage.is_some(),
            false,
        );
        if allow_exact_count
            && (unmeasured_images || force_exact_count || assessment.needs_exact())
            && pending_exact.is_none()
        {
            let input = self.conversation.model_input();
            let instructions = self.rendered_instructions(policy);
            let tools = self.request_tools(HistoryView::Committed, policy);
            let request = ModelRequest {
                input_identity: self.conversation.input_identity(),
                instructions: &instructions,
                tools: &tools,
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

    pub(in crate::session) fn dispatch_capacity_error(
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
