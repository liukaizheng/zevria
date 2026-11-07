//! Application session defaults composed from domain-specific policies.

use serde::{Deserialize, Serialize};
use zevria_instructions::prompts::DEFAULT_PREAMBLE;
use zevria_model::CompactionConfig;
use zevria_workflow::config::PlanConfig;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub max_concurrent_subtasks: usize,
    pub event_queue_capacity: usize,
    pub plan: PlanConfig,
    pub compaction: CompactionConfig,
    pub preamble: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_concurrent_subtasks: 10,
            event_queue_capacity: 256,
            plan: PlanConfig::default(),
            compaction: CompactionConfig::default(),
            preamble: DEFAULT_PREAMBLE.to_string(),
        }
    }
}

impl SessionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_concurrent_subtasks == 0 {
            anyhow::bail!("session.max_concurrent_subtasks must be greater than zero");
        }
        if self.event_queue_capacity == 0 {
            anyhow::bail!("session.event_queue_capacity must be greater than zero");
        }
        self.plan.validate()?;
        self.compaction.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_instructions::prompts::{BUILD_MODE_INSTRUCTIONS, PLAN_MODE_INSTRUCTIONS};
    use zevria_workflow::MAX_PLAN_ARTIFACT_BYTES;

    #[test]
    fn session_defaults_and_validation_are_provider_neutral() {
        let config: SessionConfig = toml::from_str("").expect("empty section should parse");
        assert_eq!(config.max_concurrent_subtasks, 10);
        assert_eq!(config.event_queue_capacity, 256);
        assert_eq!(config.plan, PlanConfig::default());
        assert_eq!(config.compaction, CompactionConfig::default());
        assert_eq!(config.preamble, DEFAULT_PREAMBLE);
        assert!(!BUILD_MODE_INSTRUCTIONS.trim().is_empty());
        assert!(!PLAN_MODE_INSTRUCTIONS.trim().is_empty());
        config.validate().expect("default session should be valid");

        let zero_subtasks: SessionConfig =
            toml::from_str("max_concurrent_subtasks = 0").expect("zero is syntactically valid");
        assert!(zero_subtasks.validate().is_err());
        let zero_events: SessionConfig =
            toml::from_str("event_queue_capacity = 0").expect("zero is syntactically valid");
        assert!(zero_events.validate().is_err());
        let oversized: SessionConfig = toml::from_str(&format!(
            "[plan]\nmax_artifact_bytes = {}",
            MAX_PLAN_ARTIFACT_BYTES + 1
        ))
        .expect("oversized limit is syntactically valid");
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn removed_model_call_limit_is_rejected_as_an_unknown_field() {
        for value in [0, 7, 9999] {
            let error = toml::from_str::<SessionConfig>(&format!("max_model_calls = {value}"))
                .expect_err("removed fields should fail");
            assert!(
                error
                    .to_string()
                    .contains("unknown field `max_model_calls`")
            );
        }
    }

    #[test]
    fn section_typos_are_rejected() {
        let error = toml::from_str::<SessionConfig>("event_queue_capcity = 2")
            .expect_err("unknown fields should fail");
        assert!(error.to_string().contains("event_queue_capcity"));
    }
}
