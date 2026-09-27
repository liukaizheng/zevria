//! One deterministic preparation path for admission, workflows, and continuations.
use super::*;
use zevria_instructions::DirectiveContent;
use zevria_instructions::DirectiveState;
use zevria_instructions::InstructionSet;

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) fn request_boundaries_enabled(&self) -> bool {
        // Root composition captures scheduler capacity. Unsupported children and
        // maintenance engines have no request contract to reset. Typed resumed
        // history still requires a reset even if the current capability is absent.
        self.capabilities.subtask_concurrency.is_some()
            || self
                .instruction_replay()
                .is_ok_and(|state| state.has_request_boundaries())
    }

    pub(super) fn standard_request_boundary(&self) -> Option<TranscriptItem> {
        self.request_boundaries_enabled().then(|| {
            TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(
                zevria_foundation::RequestMetadata::new(
                    zevria_foundation::RequestBehavior::Standard,
                ),
            ))
        })
    }
}

/// Owned request policy inputs shared by dispatch, previews, and prospective
/// tool activation. The fixed set is pin-independent; only bodies are reconciled.
pub(super) struct InstructionPreparation {
    pub(super) rendered: String,
    catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    catalog_available: bool,
    bodies_enabled: bool,
}

pub(super) struct PreparedInstructionUpdates {
    pub(super) records: Vec<TranscriptItem>,
    pub(super) state: DirectiveState,
    pub(super) instruction_tokens: u64,
}

impl PreparedInstructionUpdates {
    pub(super) fn ensure_capacity(
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

impl InstructionPreparation {
    pub(super) fn skill_activation_available(&self) -> bool {
        self.catalog_available
    }

    /// State-only reconciliation shared by prompt staging and source-based callers.
    /// The resulting snapshot is validated after the entire replacement batch.
    pub(super) fn prepare_updates(
        &self,
        state: &DirectiveState,
        active: &ActiveSkills,
    ) -> anyhow::Result<PreparedInstructionUpdates> {
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.instruction_updates += 1);
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

    pub(super) fn updates(
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

    pub(super) fn guidance_components(&self) -> impl Iterator<Item = (&str, &str)> {
        self.guidance
            .snapshot
            .iter()
            .flat_map(zevria_instructions::GuidanceSnapshot::components)
    }

    /// Maintenance uses captured guidance independently of transcript replay.
    pub(super) fn prepare_maintenance_guidance(&self) -> InstructionSet {
        InstructionSet::maintenance(&self.application_prompt, self.guidance_components())
    }

    pub(super) fn directive_state(&self) -> Result<&DirectiveState, SessionReplayError> {
        match &self.replay {
            SessionReplayState::Valid { instructions, .. } => Ok(instructions.directives()),
            SessionReplayState::Failed(error) => Err(error.clone()),
        }
    }

    pub(super) fn instruction_updates(
        &self,
        source: &[TranscriptItem],
        policy: &TurnPolicy,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<TranscriptItem>> {
        let state = zevria_transcript::replay_directives(source)?;
        let preparation = self.instruction_preparation(policy)?;
        let updates = preparation.prepare_updates(&state, active)?;
        updates.ensure_capacity(
            policy,
            self.context_policy(policy.model_role),
            prepared_fixed_tokens(&preparation.rendered, &self.filtered_tool_bytes(policy)),
        )?;
        Ok(updates.records)
    }

    pub(super) fn reconcile_instruction_state(
        &self,
        state: &DirectiveState,
        policy: &TurnPolicy,
        active: &ActiveSkills,
    ) -> anyhow::Result<Vec<DirectiveContent>> {
        self.instruction_preparation(policy)?.updates(state, active)
    }

    pub(super) fn skill_activation_available(&self, policy: &TurnPolicy) -> bool {
        policy.skills_enabled
            && policy.allows_tool(SKILL_TOOL_NAME)
            && self
                .tools
                .static_tool_defs()
                .iter()
                .any(|tool| tool.name == SKILL_TOOL_NAME)
    }

    pub(super) fn instruction_preparation(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<InstructionPreparation> {
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.instruction_snapshots += 1);
        let available = self.skill_activation_available(policy);
        let set = self.instruction_set(policy);
        set.validate()?;
        Ok(InstructionPreparation {
            rendered: set.render(),
            catalog: self.skills.catalog.clone(),
            catalog_available: available,
            bodies_enabled: available,
        })
    }

    pub(super) fn reconcile_before_dispatch(&mut self, policy: &TurnPolicy) -> anyhow::Result<()> {
        let records =
            self.instruction_updates(self.conversation.items(), policy, self.active_skills()?)?;
        if !records.is_empty() {
            self.record_required_items(records)?;
        }
        Ok(())
    }
}
