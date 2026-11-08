//! Incremental model-facing command job inspection.
use crate::command::{CommandError, render};
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zevria_foundation::{CommandJobId, ToolCancelled};
use zevria_session_api::{CommandJobs, TurnContext};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandStatusArgs {
    /// Job ID; omit or use null to list this session's jobs.
    #[serde(default)]
    pub id: Option<String>,
    /// Optionally wait for this job to finish, capped at the foreground timeout.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}
#[derive(Debug, Clone)]
pub struct CommandStatusTool {
    jobs: CommandJobs,
}
impl CommandStatusTool {
    pub fn new(jobs: CommandJobs) -> Self {
        Self { jobs }
    }
    async fn execute(
        &self,
        context: &mut ToolContext,
        args: CommandStatusArgs,
    ) -> Result<String, CommandError> {
        let Some(id) = args.id else {
            if args.wait_seconds.is_some() {
                return Err(CommandError::InvalidArgs(
                    "wait_seconds requires a job id".into(),
                ));
            }
            let jobs = self.jobs.list_model_jobs();
            if jobs.is_empty() {
                return Ok("No command jobs in this session. Jobs do not survive restarts or session replacement.".into());
            }
            let mut lines = Vec::new();
            for job in jobs {
                lines.push(render::job_line(&job));
                if job.status.is_terminal() {
                    self.jobs.mark_reported(&job.id);
                }
            }
            return Ok(lines.join("\n"));
        };
        let id = CommandJobId::new(id);
        let job = self.jobs.job(&id).map_err(job_error)?;
        if let Some(seconds) = args.wait_seconds {
            let cancellation = context
                .scope::<TurnContext>()
                .map(|t| t.cancellation().clone())
                .unwrap_or_default();
            let timeout = Duration::from_secs(seconds).min(self.jobs.limits().foreground_timeout);
            job.wait_terminal(timeout, &cancellation).await;
            if cancellation.is_cancelled() {
                context
                    .insert_result(ToolCancelled)
                    .expect("cancellation markers serialize");
                return Err(CommandError::Cancelled { output: "Status wait cancelled; the command job is still running or has completed independently.".into() });
            }
        }
        let snapshot = job.snapshot();
        let streams = job.take_unread();
        if snapshot.status.is_terminal() {
            self.jobs.mark_reported(&snapshot.id);
        }
        Ok(render::job_report(&snapshot, &streams, None))
    }
}
impl Tool for CommandStatusTool {
    const NAME: &'static str = "command_status";
    type Args = CommandStatusArgs;
    type Error = CommandError;
    type Output = String;
    fn description(&self) -> String {
        format!(
            "List this session's model-visible command jobs, or inspect one job and read stdout/stderr produced since the last model read. With an id, optionally wait up to {} seconds for completion; waiting never stops the job and turn cancellation only cancels the wait. Jobs do not survive restarts. Frontend inspection does not consume model output.",
            self.jobs.limits().foreground_timeout.as_secs()
        )
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schema_for!(CommandStatusArgs).to_value();
        schema["required"] = serde_json::json!([]);
        // The portable schema needs only integer/null, not Rust's uint64 format.
        schema["properties"]["wait_seconds"]
            .as_object_mut()
            .unwrap()
            .remove("format");
        schema
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            CommandError::InvalidArgs(_) => ToolExecutionError::invalid_args(error.to_string()),
            _ => ToolExecutionError::other(error.to_string()),
        }
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<String, CommandError> {
        let id = args.id.as_ref().map(CommandJobId::new);
        let result = self.execute(context, args).await;
        with_footer(result, &self.jobs, id.as_ref())
    }
}
pub(crate) fn job_error(error: impl std::fmt::Display) -> CommandError {
    CommandError::Job(error.to_string())
}
pub(crate) fn with_footer(
    result: Result<String, CommandError>,
    jobs: &CommandJobs,
    except: Option<&CommandJobId>,
) -> Result<String, CommandError> {
    let footer = render::job_footer(&jobs.take_unreported_notices(except));
    match result {
        Ok(s) => Ok(format!("{s}{footer}")),
        Err(e) if footer.is_empty() => Err(e),
        Err(e) => Err(CommandError::Job(format!("{e}{footer}"))),
    }
}
