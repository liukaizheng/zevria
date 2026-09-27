use crate::compaction::{DEFAULT_CONTEXT_WINDOW_TOKENS, SUMMARIZATION_PROMPT};
use serde::{Deserialize, Serialize};
use zevria_foundation::{ModelContextPolicy, ModelProfileRef, ModelRole};
/// Session-wide compaction behavior plus one resolved context policy per
/// provider-neutral model role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPolicy {
    config: CompactionConfig,
    contexts: [ModelContextPolicy; ModelRole::COUNT],
}

impl CompactionPolicy {
    pub fn new(
        config: CompactionConfig,
        contexts: [ModelContextPolicy; ModelRole::COUNT],
    ) -> anyhow::Result<Self> {
        config.validate()?;
        for role in ModelRole::ALL {
            let context = &contexts[role.index()];
            if context.context_window_tokens == 0 {
                anyhow::bail!(
                    "the {} model profile {} must have a nonzero context window",
                    role.name(),
                    context.profile
                );
            }
            if context.input_token_limit == 0
                || context.input_token_limit > context.context_window_tokens
            {
                anyhow::bail!(
                    "the {} model profile {} must have an input token limit between 1 and its {}-token physical context window",
                    role.name(),
                    context.profile,
                    context.context_window_tokens
                );
            }
            let trigger = context.trigger_tokens(config.auto_trigger_percent);
            if context.retained_user_tokens > trigger {
                anyhow::bail!(
                    "the {} model profile {} retains {} user tokens, exceeding its automatic trigger of {trigger}",
                    role.name(),
                    context.profile,
                    context.retained_user_tokens
                );
            }
        }
        Ok(Self { config, contexts })
    }

    /// Prepare a fully validated replacement without mutating installed limits.
    pub fn with_role(&self, role: ModelRole, context: ModelContextPolicy) -> anyhow::Result<Self> {
        let mut contexts = self.contexts.clone();
        contexts[role.index()] = context;
        Self::new(self.config.clone(), contexts)
    }

    pub fn for_role(&self, role: ModelRole) -> &ModelContextPolicy {
        &self.contexts[role.index()]
    }

    pub fn trigger_tokens(&self, role: ModelRole) -> u64 {
        self.for_role(role)
            .trigger_tokens(self.config.auto_trigger_percent)
    }

    pub fn auto_trigger_percent(&self) -> u64 {
        self.config.auto_trigger_percent
    }

    pub fn summary_prompt(&self) -> &str {
        self.config.summary_prompt()
    }

    pub fn serialized_config(&self) -> &CompactionConfig {
        &self.config
    }
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        let config = CompactionConfig::default();
        let context = |role: ModelRole| ModelContextPolicy {
            profile: ModelProfileRef::new("internal.default", role.name()),
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            input_token_limit: DEFAULT_CONTEXT_WINDOW_TOKENS,
            retained_user_tokens: 20_000,
        };
        Self::new(
            config,
            [
                context(ModelRole::Build),
                context(ModelRole::Plan),
                context(ModelRole::Review),
                context(ModelRole::Explore),
                context(ModelRole::Builder),
            ],
        )
        .expect("the internal default compaction policy must be valid")
    }
}

/// Session-wide compaction settings. Model-specific windows and retained-user
/// budgets live in provider model profiles and are resolved into
/// [`CompactionPolicy`] at application startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    pub auto_trigger_percent: u64,
    pub summary_prompt: Option<String>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto_trigger_percent: 90,
            summary_prompt: None,
        }
    }
}

impl CompactionConfig {
    pub fn summary_prompt(&self) -> &str {
        self.summary_prompt
            .as_deref()
            .unwrap_or(SUMMARIZATION_PROMPT)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !(1..=100).contains(&self.auto_trigger_percent) {
            anyhow::bail!("session.compaction.auto_trigger_percent must be between 1 and 100");
        }
        if self
            .summary_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            anyhow::bail!("session.compaction.summary_prompt must not be blank");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compaction_limits_and_custom_prompt_are_validated() {
        let mut config = CompactionConfig::default();
        config.validate().expect("defaults");

        config.auto_trigger_percent = 0;
        assert!(config.validate().is_err());
        config.auto_trigger_percent = 101;
        assert!(config.validate().is_err());
        config = CompactionConfig::default();
        config.summary_prompt = Some(" \n\t".to_string());
        assert!(config.validate().is_err());

        let policy = CompactionPolicy::default();
        assert_eq!(policy.trigger_tokens(ModelRole::Build), 244_800);
    }
}
