//! One deterministic preparation path for admission, workflows, and continuations.
use crate::session::SessionEngine;
use crate::session::kernel::accounting::TurnInputCountState;
use crate::session::kernel::request_plan::{HistoryView, RequestPlan};
use zevria_foundation::SKILL_TOOL_NAME;
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::policy::TurnPolicy;
use zevria_instructions::skill::ActiveSkills;
use zevria_session_api::provider::ModelProvider;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;

use zevria_instructions::{DirectiveContent, DirectiveState};

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn request_boundaries_enabled(&self) -> bool {
        // Root composition captures scheduler capacity. Unsupported children and
        // maintenance engines have no request contract to reset. Typed resumed
        // history still requires a reset even if the current capability is absent.
        self.capabilities.subtask_concurrency.is_some()
            || self
                .instruction_replay()
                .is_ok_and(|state| state.has_request_boundaries())
    }

    pub(in crate::session) fn standard_request_boundary(&self) -> Option<TranscriptItem> {
        self.request_boundaries_enabled().then(|| {
            TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(
                zevria_foundation::RequestMetadata::new(
                    zevria_foundation::RequestBehavior::Standard,
                ),
            ))
        })
    }
}

pub(in crate::session) struct PreparedInstructionUpdates {
    pub(in crate::session) records: Vec<TranscriptItem>,
    pub(in crate::session) state: DirectiveState,
    pub(in crate::session) instruction_tokens: u64,
}

impl PreparedInstructionUpdates {
    pub(in crate::session) fn ensure_capacity(
        &self,
        policy: &TurnPolicy,
        context: &ModelContextPolicy,
        fixed_tokens: u64,
    ) -> anyhow::Result<()> {
        let tokens = fixed_tokens.saturating_add(self.instruction_tokens);
        anyhow::ensure!(
            tokens < context.input_token_limit,
            "the prepared {} request has {tokens} irreducible instruction-state and tool tokens that exhaust the {}-token input limit for profile {}; compaction cannot make this request fit; disable skills through management or increase the input limit",
            policy.model_role.name(),
            context.input_token_limit,
            context.profile
        );
        Ok(())
    }
}

impl RequestPlan {
    pub(in crate::session) fn skill_activation_available(&self) -> bool {
        self.catalog_available
    }

    /// State-only reconciliation shared by prompt staging and source-based callers.
    /// The resulting snapshot is validated after the entire replacement batch.
    pub(in crate::session) fn prepare_updates(
        &self,
        state: &DirectiveState,
        active: &ActiveSkills,
    ) -> anyhow::Result<PreparedInstructionUpdates> {
        #[cfg(feature = "test-support")]
        crate::session::pipeline_probe::record(|counts| counts.instruction_updates += 1);
        let updates = self.updates(state, active)?;
        let mut required = state.clone();
        for update in &updates {
            required.apply(update)?;
        }
        required.snapshot().validate()?;
        Ok(PreparedInstructionUpdates {
            instruction_tokens: instruction_state_tokens(&required),
            records: updates.into_iter().map(TranscriptItem::Directive).collect(),
            state: required,
        })
    }

    pub(in crate::session) fn updates(
        &self,
        state: &DirectiveState,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<DirectiveContent>> {
        Ok(state.reconcile(active, |name| {
            self.bodies_enabled && self.catalog.config().name_enabled(name)
        }))
    }
}

fn instruction_state_tokens(state: &DirectiveState) -> u64 {
    state
        .snapshot()
        .directives
        .iter()
        .map(|directive| {
            zevria_model::compaction::approximate_tokens(&directive.text).saturating_add(16)
        })
        .fold(0, u64::saturating_add)
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Drain opening diagnostics once. Captured guidance is rendered in the
    /// request instruction set; this is NOT a live filesystem reload API.
    pub fn refresh_application_guidance(
        &mut self,
    ) -> anyhow::Result<Vec<zevria_instructions::GuidanceDiagnostic>> {
        Ok(std::mem::take(&mut self.guidance.startup_diagnostics))
    }

    pub(in crate::session) fn guidance_components(&self) -> impl Iterator<Item = (&str, &str)> {
        self.guidance
            .snapshot
            .iter()
            .flat_map(zevria_instructions::GuidanceSnapshot::components)
    }

    pub(in crate::session) fn directive_state(
        &self,
    ) -> Result<&DirectiveState, SessionReplayError> {
        Ok(self.conversation.replay()?.instructions.directives())
    }

    pub(in crate::session) fn instruction_updates(
        &self,
        source: &[TranscriptItem],
        policy: &TurnPolicy,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<TranscriptItem>> {
        let state = zevria_transcript::replay_directives(source)?;
        self.instruction_updates_from_state(&state, policy, active)
    }

    pub(in crate::session) fn instruction_updates_from_state(
        &self,
        state: &DirectiveState,
        policy: &TurnPolicy,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<TranscriptItem>> {
        let preparation = self.instruction_preparation(policy)?;
        let updates = preparation.prepare_updates(state, active)?;
        updates.ensure_capacity(
            policy,
            self.context_policy(policy.model_role),
            preparation.irreducible_tokens,
        )?;
        Ok(updates.records)
    }

    pub(in crate::session) fn reconcile_instruction_state(
        &self,
        state: &DirectiveState,
        policy: &TurnPolicy,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<DirectiveContent>> {
        self.instruction_preparation(policy)?.updates(state, active)
    }

    pub(in crate::session) fn skill_activation_available(&self, policy: &TurnPolicy) -> bool {
        policy.skills_enabled
            && policy.allows_tool(SKILL_TOOL_NAME)
            && self.tools.snapshot().contains(SKILL_TOOL_NAME)
    }

    pub(in crate::session) fn reconcile_before_dispatch(
        &mut self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<()> {
        let mut records = self
            .tool_declaration_update(HistoryView::Committed, policy)
            .into_iter()
            .collect::<Vec<_>>();
        records.extend(self.instruction_updates_from_state(
            self.directive_state()?,
            policy,
            self.active_skills()?,
        )?);
        if !records.is_empty() {
            let counted = self.context.input_count;
            self.commit(records)?;
            // Admission already counted prospective declarations at this exact
            // position. Keep that result only if the committed request matches.
            if let TurnInputCountState::Prepared { role, identity, .. } = counted
                && role == policy.model_role
                && self.logical_request_identity(policy)? == identity
            {
                self.context.input_count = counted;
            }
        }
        Ok(())
    }
}
