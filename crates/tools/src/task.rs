//! The Build-mode task-list progress tool.

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::schema_for;
use zevria_foundation::TASK_TOOL_NAME;
pub use zevria_foundation::TaskItem;
use zevria_foundation::TaskList;
pub use zevria_foundation::TaskStatus;
use zevria_foundation::TaskValidationError;

const DESCRIPTION: &str = r#"Maintain the live task list for substantial multi-step Build work.

Each call supplies the complete replacement list of 1-20 items, not a partial patch. Keep steps concise and outcome-oriented, move an item to `in_progress` before working on it, and mark it `completed` as soon as it is verified. At most one item may be `in_progress` at a time. Use `pending` for work that has not started.

Use this for work with several meaningful implementation or validation steps. Skip it for a simple request that can be completed directly. Update the list when the approach changes, and finish with every completed item marked `completed`. `explanation` is optional and should briefly explain a material list change."#;

/// Public model arguments for the task tool.
pub type TaskArgs = TaskList;

#[derive(Debug, Clone, Copy, Default)]
pub struct TaskTool;

impl Tool for TaskTool {
    const NAME: &'static str = TASK_TOOL_NAME;
    type Error = TaskValidationError;
    type Args = TaskArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(TaskArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::invalid_args(error.to_string())
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let tasks = args.validate()?;
        Ok(tasks.update_summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::tool::server::ToolServer;
    use rig_core::tool::ToolErrorKind;

    fn args() -> TaskArgs {
        TaskArgs {
            explanation: Some("Starting implementation.".to_string()),
            tasks: vec![
                TaskItem {
                    step: "Inspect the code".to_string(),
                    status: TaskStatus::Completed,
                },
                TaskItem {
                    step: "Implement the change".to_string(),
                    status: TaskStatus::InProgress,
                },
            ],
        }
    }

    #[test]
    fn schema_is_strict_and_description_pins_replacement_semantics() {
        let tool = TaskTool;
        let schema = tool.parameters();
        assert_eq!(TaskTool::NAME, "task");
        assert_eq!(schema["required"], serde_json::json!(["tasks"]));
        assert_eq!(schema["additionalProperties"], false);
        let schema_text = schema.to_string();
        assert!(schema_text.contains("in_progress"));
        assert!(schema_text.contains("completed"));
        assert!(schema_text.contains("\"minItems\":1"));
        assert!(schema_text.contains("\"maxItems\":20"));
        assert!(schema_text.contains("additionalProperties\":false"));
        assert!(tool.description().contains("complete replacement list"));
        assert!(tool.description().contains("At most one item"));
        assert!(tool.description().contains("Skip it for a simple request"));
    }

    #[tokio::test]
    async fn accepted_snapshot_returns_a_compact_acknowledgement() {
        let output = TaskTool
            .call(&mut ToolContext::new(), args())
            .await
            .expect("valid snapshot");
        assert_eq!(
            output,
            "Task list updated: 1/2 completed, 1 in progress, 0 pending."
        );
    }

    #[tokio::test]
    async fn invalid_snapshot_is_a_model_fixable_argument_error() {
        let server = ToolServer::new().tool(TaskTool).run();
        let result = server
            .execute(
                TASK_TOOL_NAME,
                &serde_json::json!({
                    "tasks": [
                        {"step": "One", "status": "in_progress"},
                        {"step": "Two", "status": "in_progress"}
                    ]
                })
                .to_string(),
                &mut ToolContext::new(),
            )
            .await;
        assert!(!result.is_success());
        assert!(result.is_error_kind(ToolErrorKind::InvalidArgs));
        assert!(result.output().render().contains("at most one"));
    }
}
