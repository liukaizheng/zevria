use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::CREATE_PLAN_TOOL_NAME;
use zevria_workflow::{MAX_PLAN_ARTIFACT_BYTES, PlanValidationError, validate_draft_markdown};

use crate::plan_authoring;

const DESCRIPTION: &str = "Save the initial Plan draft in engine storage; it persists across turns, restarts and context compaction and is not submitted. `markdown` must begin with exactly `# <title>` (one line, 3–8 words); other sections may be incomplete; limit {max} bytes. Fails when a draft or a submitted plan under revision already exists unless `replace` is true; prefer update_plan for changes.";

#[derive(Debug, Clone, Copy)]
pub struct CreatePlanTool {
    max_artifact_bytes: usize,
}
impl CreatePlanTool {
    pub const fn new(max_artifact_bytes: usize) -> Self {
        Self {
            max_artifact_bytes: if max_artifact_bytes > MAX_PLAN_ARTIFACT_BYTES {
                MAX_PLAN_ARTIFACT_BYTES
            } else {
                max_artifact_bytes
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePlanArgs {
    pub markdown: String,
    pub replace: bool,
}

impl Tool for CreatePlanTool {
    const NAME: &'static str = CREATE_PLAN_TOOL_NAME;
    type Error = PlanValidationError;
    type Args = CreatePlanArgs;
    type Output = String;
    fn description(&self) -> String {
        DESCRIPTION.replace("{max}", &self.max_artifact_bytes.to_string())
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(CreatePlanArgs).to_value()
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<String, Self::Error> {
        let scope = plan_authoring::scope(context)?;
        if scope.markdown.is_some() && !args.replace {
            return Err(PlanValidationError("a Plan working copy already exists; prefer update_plan, or use create_plan with replace: true to explicitly restart".into()));
        }
        validate_draft_markdown(&args.markdown, self.max_artifact_bytes)?;
        Ok(plan_authoring::save(
            context,
            &scope,
            args.markdown,
            None,
            self.max_artifact_bytes,
        ))
    }
}
