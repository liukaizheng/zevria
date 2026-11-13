//! Shared immutable request policy, independent of model-input ownership.
use crate::session::SessionEngine;
use crate::session::kernel::capacity::{prepared_fixed_tokens, prepared_request_shape};
use crate::session::kernel::state::RequestShapeFingerprint;
use std::sync::Arc;
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::policy::TurnPolicy;
use zevria_instructions::skill::SkillCatalog;
use zevria_model::ToolDeclarations;
use zevria_session_api::provider::ModelProvider;
use zevria_transcript::tool_window;
use zevria_transcript::transcript::{Conversation, TranscriptItem};

#[derive(Clone, Copy)]
pub(in crate::session) enum HistoryView<'a> {
    Committed,
    Prospective(&'a [TranscriptItem]),
}
impl<'a> HistoryView<'a> {
    pub(in crate::session) fn items(self, conversation: &'a Conversation) -> &'a [TranscriptItem] {
        match self {
            Self::Committed => conversation.items(),
            Self::Prospective(items) => items,
        }
    }
}

pub(in crate::session) struct RequestPlan {
    pub(in crate::session) policy: TurnPolicy,
    pub(in crate::session) application: String,
    pub(in crate::session) context: ModelContextPolicy,
    pub(in crate::session) instructions: Arc<str>,
    pub(in crate::session) tools: Arc<ToolDeclarations>,
    pub(in crate::session) tool_bytes: Arc<[u8]>,
    pub(in crate::session) tool_update: Option<TranscriptItem>,
    pub(in crate::session) shape: RequestShapeFingerprint,
    pub(in crate::session) fixed_tokens: u64,
    pub(in crate::session) irreducible_tokens: u64,
    pub(in crate::session) catalog: Arc<SkillCatalog>,
    pub(in crate::session) catalog_available: bool,
    pub(in crate::session) bodies_enabled: bool,
    validation: Option<String>,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn request_plan(&self, policy: &TurnPolicy) -> Arc<RequestPlan> {
        let mut cache = self
            .request_plans
            .lock()
            .expect("request plan cache poisoned");
        if let Some(plan) = cache.iter().find(|plan| {
            plan.policy == *policy
                && plan.application == self.application_prompt
                && plan.context == *self.context_policy(policy.model_role)
        }) {
            return plan.clone();
        }
        #[cfg(feature = "test-support")]
        crate::session::pipeline_probe::record(|counts| counts.instruction_renders += 1);
        let set = self.instruction_set(policy);
        let validation = set.validate().err().map(|error| format!("{error:#}"));
        let instructions: Arc<str> = set.render().into();
        let available = self.skill_activation_available(policy);
        let plan = self.build_request_plan(
            policy,
            HistoryView::Committed,
            instructions,
            validation,
            available,
        );
        if cache.len() >= 8 {
            cache.clear();
        }
        cache.push(plan.clone());
        plan
    }

    fn build_request_plan(
        &self,
        policy: &TurnPolicy,
        history: HistoryView<'_>,
        instructions: Arc<str>,
        validation: Option<String>,
        available: bool,
    ) -> Arc<RequestPlan> {
        let items = history.items(&self.conversation);
        let desired = self.desired_tool_declarations(policy);
        let tools = Arc::new(
            tool_window(items)
                .initial
                .cloned()
                .unwrap_or_else(|| desired.clone()),
        );
        let tool_bytes: Arc<[u8]> = serde_json::to_vec(tools.as_ref())
            .expect("tools serialize")
            .into();
        Arc::new(RequestPlan {
            policy: policy.clone(),
            application: self.application_prompt.clone(),
            context: self.context_policy(policy.model_role).clone(),
            shape: prepared_request_shape(
                policy,
                self.context_policy(policy.model_role),
                &instructions,
                &tool_bytes,
            ),
            fixed_tokens: prepared_fixed_tokens(&instructions, &tool_bytes),
            irreducible_tokens: zevria_model::compaction::approximate_tokens(&instructions)
                .saturating_add(desired.approximate_tokens()),
            instructions,
            tools,
            tool_bytes,
            tool_update: self.uncached_tool_declaration_update(items, policy),
            catalog: self.skills.catalog.clone(),
            catalog_available: available,
            bodies_enabled: available,
            validation,
        })
    }

    pub(in crate::session) fn request_plan_for(
        &self,
        history: HistoryView<'_>,
        policy: &TurnPolicy,
    ) -> Arc<RequestPlan> {
        let committed = self.request_plan(policy);
        match history {
            HistoryView::Committed => committed,
            HistoryView::Prospective(_) => self.build_request_plan(
                policy,
                history,
                committed.instructions.clone(),
                committed.validation.clone(),
                committed.catalog_available,
            ),
        }
    }

    pub(in crate::session) fn request_tools(
        &self,
        history: HistoryView<'_>,
        policy: &TurnPolicy,
    ) -> Arc<ToolDeclarations> {
        self.request_plan_for(history, policy).tools.clone()
    }

    pub(in crate::session) fn tool_declaration_update(
        &self,
        history: HistoryView<'_>,
        policy: &TurnPolicy,
    ) -> Option<TranscriptItem> {
        self.request_plan_for(history, policy).tool_update.clone()
    }

    pub(in crate::session) fn rendered_instructions(&self, policy: &TurnPolicy) -> Arc<str> {
        self.request_plan(policy).instructions.clone()
    }

    pub(in crate::session) fn instruction_preparation(
        &self,
        policy: &TurnPolicy,
    ) -> anyhow::Result<Arc<RequestPlan>> {
        #[cfg(feature = "test-support")]
        crate::session::pipeline_probe::record(|counts| counts.instruction_snapshots += 1);
        let plan = self.request_plan(policy);
        if let Some(error) = &plan.validation {
            anyhow::bail!("{error}");
        }
        Ok(plan)
    }

    /// Guidance/catalog/policy changes can change the fixed instruction prefix.
    pub(in crate::session) fn invalidate_request_plans(&mut self) {
        self.request_plans
            .get_mut()
            .expect("request plan cache poisoned")
            .clear();
    }

    /// Body directives and tool windows never change the rendered prefix. Rebuild
    /// only the tool projection while sharing its existing instruction allocation.
    pub(in crate::session) fn refresh_request_tools(&mut self) {
        let old = std::mem::take(
            self.request_plans
                .get_mut()
                .expect("request plan cache poisoned"),
        );
        let updated = old
            .into_iter()
            .filter(|plan| {
                plan.application == self.application_prompt
                    && plan.context == *self.context_policy(plan.policy.model_role)
            })
            .map(|plan| {
                self.build_request_plan(
                    &plan.policy,
                    HistoryView::Committed,
                    plan.instructions.clone(),
                    plan.validation.clone(),
                    plan.catalog_available,
                )
            })
            .collect();
        *self
            .request_plans
            .get_mut()
            .expect("request plan cache poisoned") = updated;
    }
}
