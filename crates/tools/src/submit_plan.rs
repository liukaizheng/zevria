use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::SUBMIT_PLAN_TOOL_NAME;
use zevria_workflow::MAX_PLAN_ARTIFACT_BYTES;
use zevria_workflow::PlanCandidate;
use zevria_workflow::PlanValidationError;

const DESCRIPTION: &str = r#"Submit the complete canonical Plan artifact for explicit user approval.

Use this only when planning is complete. The title must contain 3–8 words. Markdown must begin with exactly `# <title>` and contain nonempty `## Goal`, `## Decisions`, `## Implementation`, `## Validation`, and `## Risks` sections. The configured Markdown limit is {max_artifact_bytes} bytes. After this tool accepts the artifact, make no more tool calls and provide only a short final confirmation."#;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitPlanArgs {
    pub title: String,
    pub markdown: String,
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
        schema_for!(SubmitPlanArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let candidate =
            PlanCandidate::validate(args.title, args.markdown, self.max_artifact_bytes)?;
        context.insert_result(candidate);
        Ok(
            "Plan artifact accepted. Make no more tool calls; provide a short final confirmation."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepted_candidate_is_typed_context_state() {
        let title = "Durable plan workflow";
        let markdown = format!(
            "# {title}\n\n## Goal\nOne.\n\n## Decisions\nTwo.\n\n## Implementation\nThree.\n\n## Validation\nFour.\n\n## Risks\nFive."
        );
        let mut context = ToolContext::new();
        SubmitPlanTool::new(131_072)
            .call(
                &mut context,
                SubmitPlanArgs {
                    title: title.to_string(),
                    markdown,
                },
            )
            .await
            .expect("valid candidate");
        assert!(context.result::<PlanCandidate>().is_some());
    }

    #[tokio::test]
    async fn oversized_candidate_is_rejected_without_context_state() {
        let title = "Durable plan workflow";
        let markdown = format!(
            "# {title}\n\n## Goal\nOne.\n\n## Decisions\nTwo.\n\n## Implementation\nThree.\n\n## Validation\nFour.\n\n## Risks\nFive."
        );
        let mut context = ToolContext::new();
        let error = SubmitPlanTool::new(markdown.len() - 1)
            .call(
                &mut context,
                SubmitPlanArgs {
                    title: title.to_string(),
                    markdown,
                },
            )
            .await
            .expect_err("oversized candidate must fail");

        assert!(error.to_string().contains("configured limit"));
        assert!(context.result::<PlanCandidate>().is_none());
        assert_eq!(
            SubmitPlanTool::new(usize::MAX).max_artifact_bytes,
            MAX_PLAN_ARTIFACT_BYTES
        );
    }
}
