use crate::plan_authoring;
use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zevria_foundation::READ_PLAN_TOOL_NAME;
use zevria_workflow::PlanValidationError;

const DESCRIPTION: &str = "Return the exact stored Plan draft (or the submitted plan under revision) with its draft number.";
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadPlanTool;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPlanArgs {}
impl Tool for ReadPlanTool {
    const NAME: &'static str = READ_PLAN_TOOL_NAME;
    type Error = PlanValidationError;
    type Args = ReadPlanArgs;
    type Output = String;
    fn description(&self) -> String {
        DESCRIPTION.into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }
    async fn call(&self, context: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        let scope = plan_authoring::scope(context)?;
        let markdown = plan_authoring::working_copy(&scope)?;
        Ok(format!(
            "Plan {} ({} bytes).\n{}",
            scope.label.as_deref().expect("working copy label"),
            markdown.len(),
            markdown
        ))
    }
}
