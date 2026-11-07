use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::UPDATE_PLAN_TOOL_NAME;
use zevria_workflow::{MAX_PLAN_ARTIFACT_BYTES, PlanEdit, PlanValidationError, apply_plan_edits};

use crate::{
    plan_authoring,
    shared::{MAX_EDIT_FILE_CHANGE_BYTES, update_file_change},
};

const DESCRIPTION: &str = "Revise the stored Plan working copy with 1–32 atomic, ordered edits. `replace` uses literal old_string/new_string matching: exactly one match unless replace_all is true; old_string must not be empty. `replace_section` replaces the body under one exact ATX heading line of level 2 or deeper, outside fenced code, up to the next heading of the same or higher level; the heading line is kept. Any failure changes nothing; identical results are errors. The result must begin with exactly `# <title>` (one line, 3–8 words) and fit {max} bytes; other sections may be incomplete. Call read_plan after a match failure to get the exact stored text.";

#[derive(Debug, Clone, Copy)]
pub struct UpdatePlanTool {
    max_artifact_bytes: usize,
}
impl UpdatePlanTool {
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
pub struct UpdatePlanArgs {
    #[schemars(length(min = 1, max = 32))]
    pub edits: Vec<PlanEdit>,
}
impl Tool for UpdatePlanTool {
    const NAME: &'static str = UPDATE_PLAN_TOOL_NAME;
    type Error = PlanValidationError;
    type Args = UpdatePlanArgs;
    type Output = String;
    fn description(&self) -> String {
        DESCRIPTION.replace("{max}", &self.max_artifact_bytes.to_string())
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(UpdatePlanArgs).to_value()
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
        let original = plan_authoring::working_copy(&scope)?;
        let markdown = apply_plan_edits(original, &args.edits, self.max_artifact_bytes)?;
        let change = update_file_change(original, &markdown, None, MAX_EDIT_FILE_CHANGE_BYTES);
        Ok(plan_authoring::save(
            context,
            &scope,
            markdown,
            Some(change),
            self.max_artifact_bytes,
        ))
    }
}
