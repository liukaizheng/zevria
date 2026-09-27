//! Schema registration only. The session engine alone resolves and admits skills.
use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::schema_for;
use zevria_foundation::SKILL_TOOL_NAME;
pub use zevria_instructions::skill::SkillRequest as SkillArgs;
use zevria_instructions::skill::SkillRequest;

const DESCRIPTION: &str = "Apply a skill by its exact validated name from the complete engine-provided eligible catalog. `args` optionally restates this application. Invoke/reapply only skill calls in this response and wait for the resulting instructions before task tools or completion; mixed batches execute nothing. The accepted result is bodyless. A direct typed invocation already supplies application. On failure inspect the error and do not claim application.";
#[derive(Debug)]
pub struct SkillError(String);
impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SkillError {}
#[derive(Clone, Default)]
pub struct SkillTool;
impl Tool for SkillTool {
    const NAME: &'static str = SKILL_TOOL_NAME;
    type Error = SkillError;
    type Args = SkillRequest;
    type Output = String;
    fn description(&self) -> String {
        DESCRIPTION.into()
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(SkillRequest).to_value()
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        _request: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Err(SkillError(
            "only the session engine can admit skill activation; raw dispatch is unsupported"
                .into(),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::tool::server::ToolServer;
    use rig_core::tool::ToolErrorKind;
    #[test]
    fn schema_is_strict_and_description_is_bodyless() {
        let schema = SkillTool.parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!(["skill"]));
        assert!(
            SkillTool
                .description()
                .contains("complete engine-provided eligible catalog")
        );
        for mechanic in ["`args`", "only skill calls", "bodyless", "On failure"] {
            assert!(SkillTool.description().contains(mechanic));
        }
        for owned_elsewhere in [
            "At the start",
            "minimal applicable",
            "never invent",
            "remain authoritative",
        ] {
            assert!(!SkillTool.description().contains(owned_elsewhere));
        }
    }
    #[tokio::test]
    async fn raw_and_malformed_dispatch_fail_without_application() {
        let tools = ToolServer::new().tool(SkillTool).run();
        for arguments in [
            r#"{"skill":"commit"}"#,
            r#"{"skill":"commit","extra":true}"#,
            r#"{"skill":"UPPER"}"#,
        ] {
            let result = tools
                .execute("skill", arguments, &mut ToolContext::new())
                .await;
            assert!(result.is_error_kind(ToolErrorKind::InvalidArgs));
        }
    }
}
