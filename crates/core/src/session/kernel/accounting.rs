//! Single owner of usage baselines, count state, capacity assessment and arming.
use crate::session::kernel::state::RequestShapeFingerprint;
use crate::session::workflow::compaction::automatic_compaction_armed_for;
use std::collections::HashMap;
use zevria_foundation::config::ModelProfileRef;
use zevria_foundation::policy::{ModelRole, TurnId};
use zevria_model::compaction::ContextTokenEstimate;
use zevria_model::estimates::{
    estimate_model_input, estimate_model_input_for_profile, estimate_model_request_item,
    estimate_model_request_item_for_profile,
};
use zevria_model::request::{InputTokenCount, ModelRequest, ModelRequestItem};
use zevria_model::telemetry::{ContextTokenSnapshot, ContextTokenSource};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_transcript::transcript::{Conversation, TranscriptItem};

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::session) struct ContextUsageTracker {
    pub(in crate::session) last_provider_total: Option<u64>,
    pub(in crate::session) last_provider_fixed_overhead: u64,
    pub(in crate::session) last_request_shape: Option<RequestShapeFingerprint>,
    pub(in crate::session) appended_estimate: ContextTokenEstimate,
    /// Model-visible images not covered by this profile's latest usage baseline.
    pub(in crate::session) unmeasured_images: bool,
}

impl ContextUsageTracker {
    pub(in crate::session) fn compactable_tokens_for(
        self,
        shape: RequestShapeFingerprint,
    ) -> Option<u64> {
        self.last_provider_total
            .filter(|_| self.last_request_shape == Some(shape))
            .map(|total| {
                total
                    .saturating_sub(self.last_provider_fixed_overhead)
                    .saturating_add(self.appended_estimate.payload_tokens)
            })
    }

    pub(in crate::session) fn projected_tokens_for(
        self,
        shape: RequestShapeFingerprint,
        fixed_overhead: u64,
    ) -> Option<u64> {
        self.compactable_tokens_for(shape)
            .map(|tokens| tokens.saturating_add(fixed_overhead))
    }

    pub(in crate::session) fn provider_reported(
        &mut self,
        total_tokens: u64,
        fixed_overhead: u64,
        shape: RequestShapeFingerprint,
    ) {
        self.last_provider_total = Some(total_tokens);
        self.last_provider_fixed_overhead = fixed_overhead.min(total_tokens);
        self.last_request_shape = Some(shape);
        self.appended_estimate = ContextTokenEstimate::default();
        self.unmeasured_images = false;
    }

    pub(in crate::session) fn appended_tokens(&mut self, estimate: ContextTokenEstimate) {
        self.appended_estimate = self.appended_estimate.saturating_add(estimate);
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum TurnInputCountState {
    #[default]
    Empty,
    Prepared {
        turn_id: TurnId,
        role: ModelRole,
        identity: RequestShapeFingerprint,
        tokens: u64,
    },
    Failed {
        turn_id: TurnId,
    },
}

impl TurnInputCountState {
    pub(in crate::session) fn invalidate_prepared(&mut self) {
        if matches!(self, Self::Prepared { .. }) {
            *self = Self::Empty;
        }
    }

    pub(in crate::session) fn take_prepared(
        &mut self,
        turn_id: TurnId,
        role: ModelRole,
        identity: RequestShapeFingerprint,
    ) -> Option<u64> {
        let prepared = match *self {
            Self::Prepared {
                turn_id: counted_turn,
                role: counted_role,
                identity: counted_identity,
                tokens,
            } if counted_turn == turn_id
                && counted_role == role
                && counted_identity == identity =>
            {
                Some(tokens)
            }
            _ => None,
        };
        self.invalidate_prepared();
        prepared
    }
}

pub(in crate::session) struct ContextAccounting {
    pub(in crate::session) usage: HashMap<ModelProfileRef, ContextUsageTracker>,
    pub(in crate::session) last_snapshots: HashMap<ModelRole, ContextTokenSnapshot>,
    pub(in crate::session) input_count: TurnInputCountState,
    pub(in crate::session) count_failures: std::collections::HashSet<ModelProfileRef>,
    pub(in crate::session) automatic_compaction_armed: [bool; ModelRole::COUNT],
}
impl Default for ContextAccounting {
    fn default() -> Self {
        Self {
            usage: HashMap::new(),
            last_snapshots: HashMap::new(),
            input_count: TurnInputCountState::Empty,
            count_failures: std::collections::HashSet::new(),
            automatic_compaction_armed: [true; ModelRole::COUNT],
        }
    }
}
impl ContextAccounting {
    pub(in crate::session) fn arm_all(&mut self) {
        self.automatic_compaction_armed.fill(true);
    }
    pub(in crate::session) fn arm(&mut self, role: ModelRole) {
        self.automatic_compaction_armed[role.index()] = true;
    }
    pub(in crate::session) fn recompute_arming(&mut self, items: &[TranscriptItem]) {
        self.automatic_compaction_armed
            .fill(automatic_compaction_armed_for(items));
    }
    pub(in crate::session) fn is_armed(&self, role: ModelRole) -> bool {
        self.automatic_compaction_armed[role.index()]
    }
    pub(in crate::session) fn invalidate_prepared_count(&mut self) {
        self.input_count.invalidate_prepared();
    }
    pub(in crate::session) fn reset_for_model_update(&mut self, role: ModelRole) {
        self.usage.clear();
        self.last_snapshots.clear();
        self.count_failures.clear();
        self.input_count = TurnInputCountState::Empty;
        self.arm(role);
    }
}

/// Bands are inclusive at the trigger and at the input limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum CapacityBand {
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
pub(in crate::session) struct CapacityCandidates {
    pub(in crate::session) payload: u64,
    pub(in crate::session) conservative: u64,
    pub(in crate::session) usage: Option<u64>,
    pub(in crate::session) exact: Option<u64>,
}
#[derive(Debug, Clone, Copy)]
pub(in crate::session) struct CapacityAssessment {
    pub(in crate::session) tokens: u64,
    pub(in crate::session) source: ContextTokenSource,
    pub(in crate::session) band: CapacityBand,
    pub(in crate::session) measurements_disagree: bool,
}
impl CapacityAssessment {
    pub(in crate::session) fn from_candidates(
        c: CapacityCandidates,
        trigger: u64,
        limit: u64,
    ) -> Self {
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
    pub(in crate::session) fn needs_exact(&self) -> bool {
        self.source != ContextTokenSource::Exact
            && (self.band != CapacityBand::Fits || self.measurements_disagree)
    }
}

pub(in crate::session) async fn count_input_tokens_for_turn<P: ModelProvider>(
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

impl ContextAccounting {
    pub(in crate::session) fn compactable_tokens(
        &self,
        conversation: &Conversation,
        profile: &ModelProfileRef,
        shape: RequestShapeFingerprint,
    ) -> u64 {
        self.usage
            .get(profile)
            .and_then(|tracker| tracker.compactable_tokens_for(shape))
            .unwrap_or_else(|| {
                estimate_model_input_for_profile(conversation.model_input(), profile)
                    .unwrap_or_else(|_| estimate_model_input(conversation.model_input()))
                    .conservative_tokens
            })
    }

    pub(in crate::session) fn projected_tokens(
        &self,
        profile: &ModelProfileRef,
        shape: RequestShapeFingerprint,
        fixed: u64,
    ) -> Option<u64> {
        self.usage
            .get(profile)
            .and_then(|tracker| tracker.projected_tokens_for(shape, fixed))
    }

    pub(in crate::session) fn on_provider_usage(
        &mut self,
        profile: ModelProfileRef,
        total: u64,
        fixed: u64,
        shape: RequestShapeFingerprint,
    ) {
        self.usage
            .entry(profile)
            .or_default()
            .provider_reported(total, fixed, shape);
    }

    pub(in crate::session) fn reestimate(
        &mut self,
        conversation: &Conversation,
        profiles: impl IntoIterator<Item = ModelProfileRef>,
    ) {
        #[cfg(feature = "test-support")]
        crate::session::pipeline_probe::record(|counts| counts.accounting_passes += 1);
        let mut usage = HashMap::new();
        for profile in profiles {
            if usage.contains_key(&profile) {
                continue;
            }
            let estimate = estimate_model_input_for_profile(conversation.model_input(), &profile)
                .unwrap_or_else(|_| estimate_model_input(conversation.model_input()));
            usage.insert(
                profile,
                ContextUsageTracker {
                    appended_estimate: estimate,
                    unmeasured_images: conversation.has_images(),
                    ..ContextUsageTracker::default()
                },
            );
        }
        self.usage = usage;
    }

    pub(in crate::session) fn on_history_replaced(
        &mut self,
        conversation: &Conversation,
        profiles: impl IntoIterator<Item = ModelProfileRef>,
    ) {
        self.invalidate_prepared_count();
        self.reestimate(conversation, profiles);
        self.recompute_arming(conversation.items());
    }

    pub(in crate::session) fn assess(
        &self,
        candidates: CapacityCandidates,
        trigger: u64,
        limit: u64,
    ) -> CapacityAssessment {
        CapacityAssessment::from_candidates(candidates, trigger, limit)
    }

    pub(in crate::session) fn needs_image_count(
        &self,
        profile: &ModelProfileRef,
        has_images: bool,
        compatible_baseline: bool,
        added_images: bool,
    ) -> bool {
        has_images
            && (!compatible_baseline
                || added_images
                || self
                    .usage
                    .get(profile)
                    .is_none_or(|tracker| tracker.unmeasured_images))
    }

    pub(in crate::session) async fn count_input_tokens<P: ModelProvider>(
        &mut self,
        provider: &mut P,
        request: ModelRequest<'_>,
        turn: &TurnContext,
    ) -> anyhow::Result<InputTokenCount> {
        count_input_tokens_for_turn(provider, &mut self.input_count, request, turn).await
    }

    /// Account borrowed committed items, never detached transcript copies.
    pub(in crate::session) fn on_committed(&mut self, items: &[TranscriptItem]) -> bool {
        let changed = items.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Directive(_)
                    | TranscriptItem::ToolDeclarations(_)
                    | TranscriptItem::Compaction(_)
            )
        });
        if changed {
            self.last_snapshots.clear();
        }
        if items.iter().any(|item| item.model_request_item().is_some()) {
            self.invalidate_prepared_count();
        }
        let added_images = items
            .iter()
            .filter_map(TranscriptItem::model_request_item)
            .filter_map(ModelRequestItem::message_ref)
            .any(zevria_content::prompt::message_has_images);
        for (profile, tracker) in &mut self.usage {
            let estimate = items
                .iter()
                .filter_map(TranscriptItem::model_request_item)
                .map(|item| {
                    estimate_model_request_item_for_profile(item, profile)
                        .unwrap_or_else(|_| estimate_model_request_item(item))
                })
                .fold(
                    ContextTokenEstimate::default(),
                    ContextTokenEstimate::saturating_add,
                );
            tracker.appended_tokens(estimate);
            tracker.unmeasured_images |= added_images;
        }
        changed
    }
}
