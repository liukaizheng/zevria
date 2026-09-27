//! Bounded text access to live auxiliary resources of active packages.

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::schema_for;
use zevria_foundation::SKILL_READ_TOOL_NAME;
use zevria_instructions::skill::SkillContext;
use zevria_instructions::skill::SkillReadRequest;

#[derive(Clone, Copy)]
pub struct SkillReadTool;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SkillReadError(String);

impl Tool for SkillReadTool {
    const NAME: &'static str = SKILL_READ_TOOL_NAME;
    type Args = SkillReadRequest;
    type Output = String;
    type Error = SkillReadError;

    fn description(&self) -> String {
        "Read a live UTF-8 auxiliary resource of an active, enabled package skill. Resources are package-relative; paths, symlinks, and aliases cannot escape the package or read its main SKILL.md. Files are capped at 1 MiB and paged at 32 KiB. Pagination binds the full content digest; restart if the file changes. This does not execute scripts or grant permissions.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(SkillReadRequest).to_value()
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        request: Self::Args,
    ) -> Result<String, Self::Error> {
        let trusted = context
            .get::<SkillContext>()
            .ok_or_else(|| {
                SkillReadError("skill_read requires an engine-supplied turn context".into())
            })?
            .clone();
        tokio::task::spawn_blocking(move || {
            let page = trusted.read_resource(&request)?;
            serde_json::to_string(&page).map_err(anyhow::Error::from)
        })
        .await
        .map_err(|error| SkillReadError(error.to_string()))?
        .map_err(|error| SkillReadError(format!("{error:#}").chars().take(1024).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn skill_read_cannot_accept_root_overrides_or_untrusted_dispatch() {
        assert!(
            serde_json::from_str::<SkillReadRequest>(
                r#"{"skill":"review","resource":"guide","root":"/tmp"}"#
            )
            .is_err()
        );
        let request =
            serde_json::from_str(r#"{"skill":"review","resource":"guide"}"#).expect("request");
        assert!(
            SkillReadTool
                .call(&mut ToolContext::new(), request)
                .await
                .is_err()
        );
    }
}
