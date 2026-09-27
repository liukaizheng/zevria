use crate::MAX_PLAN_ARTIFACT_BYTES;
use serde::{Deserialize, Serialize};
/// Plan-mode limits. These switches can only remove optional delegation and
/// instruction capabilities; no configuration path adds structured mutation
/// tools. Plan's source-read-only inspection and private scratch execution are
/// a behavioral contract, not a shell sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanConfig {
    pub max_artifact_bytes: usize,
    pub allow_subtasks: bool,
    pub allow_skills: bool,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            max_artifact_bytes: MAX_PLAN_ARTIFACT_BYTES,
            allow_subtasks: true,
            allow_skills: true,
        }
    }
}

impl PlanConfig {
    pub fn validate(self) -> anyhow::Result<()> {
        if self.max_artifact_bytes == 0 {
            anyhow::bail!("session.plan.max_artifact_bytes must be greater than zero");
        }
        if self.max_artifact_bytes > MAX_PLAN_ARTIFACT_BYTES {
            anyhow::bail!(
                "session.plan.max_artifact_bytes may not exceed {MAX_PLAN_ARTIFACT_BYTES}"
            );
        }
        Ok(())
    }
}
