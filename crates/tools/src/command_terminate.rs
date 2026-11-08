//! Stop one shell process tree, never the agent turn.
use crate::{
    command::{CommandError, render},
    command_status::{job_error, with_footer},
};
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zevria_foundation::{CommandJobId, TerminationOrigin};
use zevria_session_api::CommandJobs;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandTerminateArgs {
    /// The command job ID to terminate.
    pub id: String,
}
#[derive(Debug, Clone)]
pub struct CommandTerminateTool {
    jobs: CommandJobs,
}
impl CommandTerminateTool {
    pub fn new(jobs: CommandJobs) -> Self {
        Self { jobs }
    }
}
impl Tool for CommandTerminateTool {
    const NAME: &'static str = "command_terminate";
    type Args = CommandTerminateArgs;
    type Error = CommandError;
    type Output = String;
    fn description(&self) -> String {
        "Terminate one command job's entire process tree without cancelling the agent turn. Wait for graceful termination and forced cleanup, then return final status and unread output. Already-finished jobs return their final status. Use command_status without an id to list jobs; jobs do not survive restarts.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(CommandTerminateArgs).to_value()
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<String, CommandError> {
        let id = CommandJobId::new(args.id);
        let result = async {
            let job = self.jobs.job(&id).map_err(job_error)?;
            job.terminate(TerminationOrigin::Model);
            if !job.wait_terminal(Duration::from_secs(5), &Default::default()).await {
                return Err(CommandError::Job(format!("termination requested for {id}; cleanup is still in progress; use command_status to inspect it")));
            }
            let snapshot = job.snapshot();
            let streams = job.take_unread();
            self.jobs.mark_reported(&snapshot.id);
            Ok(render::job_report(&snapshot, &streams, None))
        }.await;
        with_footer(result, &self.jobs, Some(&id))
    }
}
