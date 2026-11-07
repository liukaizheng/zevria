use crate::plan_authoring;
use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zevria_foundation::SUBMIT_PLAN_TOOL_NAME;
use zevria_workflow::{MAX_PLAN_ARTIFACT_BYTES, PlanCandidate, PlanValidationError};

const DESCRIPTION: &str = r#"Submit the stored Plan draft unchanged; takes no arguments.

Use this only when planning is complete. The title must contain 3–8 words. Markdown must begin with exactly `# <title>` and contain nonempty `## Goal`, `## Decisions`, `## Implementation`, `## Validation`, and `## Risks` sections. The configured Markdown limit is {max_artifact_bytes} bytes. Fix the stored draft with update_plan first if validation fails. After this tool accepts the artifact, make no more tool calls and provide only a short final confirmation."#;

#[derive(Debug, Clone, Copy)]
pub struct SubmitPlanTool {
    max_artifact_bytes: usize,
}
impl SubmitPlanTool {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitPlanArgs {}
impl<'de> Deserialize<'de> for SubmitPlanArgs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.as_object().is_some_and(|object| object.is_empty()) {
            Ok(Self {})
        } else {
            Err(serde::de::Error::custom(
                "submit_plan submits the stored draft; change it with update_plan first",
            ))
        }
    }
}

impl Tool for SubmitPlanTool {
    const NAME: &'static str = SUBMIT_PLAN_TOOL_NAME;
    type Error = PlanValidationError;
    type Args = SubmitPlanArgs;
    type Output = String;
    fn description(&self) -> String {
        DESCRIPTION.replace("{max_artifact_bytes}", &self.max_artifact_bytes.to_string())
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
        let candidate = PlanCandidate::from_markdown(markdown.to_owned(), self.max_artifact_bytes)?;
        context
            .insert_result(plan_authoring::detail(
                markdown,
                &scope,
                scope.sequence,
                None,
            ))
            .expect("Plan details serialize");
        context
            .insert_result(candidate)
            .expect("validated Plan text serializes");
        Ok(
            "Plan artifact accepted. Make no more tool calls; provide a short final confirmation."
                .into(),
        )
    }
}
