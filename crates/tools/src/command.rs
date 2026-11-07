//! The unrestricted shell command tool.
use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::PathBuf, time::Duration};
use zevria_foundation::{
    CommandJobDetail, CommandJobDisposition, CommandJobId, CommandJobMode, CommandJobStatus,
    TerminationOrigin, ToolCallId, ToolCancelled, ToolResultDetail,
};
use zevria_session_api::{CommandJobLimits, CommandJobs, JobHandle, TurnContext};
mod supervise;
#[cfg(test)]
use supervise::capture_stream;
#[cfg(test)]
use zevria_foundation::ToolContextRuntimeExt;

pub(crate) const MAX_OUTPUT_STREAM_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_CAPTURE_BYTES: usize = 1024 * 1024;

pub(crate) fn elide_middle(stream: &str, max_bytes: usize) -> Cow<'_, str> {
    if stream.len() <= max_bytes {
        return Cow::Borrowed(stream);
    }
    let mut head_end = max_bytes / 2;
    while !stream.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = stream.len() - max_bytes / 2;
    while !stream.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted_bytes = tail_start - head_end;
    Cow::Owned(format!(
        "{}\n[... {omitted_bytes} bytes omitted: the output exceeded the {max_bytes}-byte per-stream limit; rerun a narrower command (a more specific pattern, a path filter, or `head`) to see the omitted middle ...]\n{}",
        &stream[..head_end],
        &stream[tail_start..]
    ))
}

/// Arguments accepted by the [`CommandTool`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandArgs {
    /// The shell command to execute.
    pub command: String,
    /// Start a session-owned background job instead of waiting for completion.
    #[serde(default)]
    pub background: Option<bool>,
}
#[derive(Debug, Clone)]
pub struct CommandTool {
    working_directory: PathBuf,
    jobs: CommandJobs,
    shell: Result<std::sync::Arc<zevria_foundation::shell::ShellSpec>, String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLimits {
    pub timeout: Duration,
    /// Combined retained stdout/stderr bytes. Each stream receives half.
    pub capture_bytes: usize,
}
impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            capture_bytes: DEFAULT_CAPTURE_BYTES,
        }
    }
}
impl From<CommandLimits> for CommandJobLimits {
    fn from(limits: CommandLimits) -> Self {
        Self {
            max_running_background: 0,
            retained_finished: 0,
            capture_bytes: limits.capture_bytes,
            foreground_timeout: limits.timeout,
            background_timeout: None,
        }
    }
}
impl CommandTool {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            jobs: CommandJobs::private(CommandLimits::default().into()),
            shell: zevria_foundation::shell::frozen().map_err(|e| format!("{e:#}")),
        }
    }
    pub fn with_limits(working_directory: PathBuf, limits: CommandLimits) -> anyhow::Result<Self> {
        if limits.timeout.is_zero() {
            anyhow::bail!("command.timeout_seconds must be greater than zero");
        }
        if limits.capture_bytes < 2 {
            anyhow::bail!("command.capture_bytes must be at least 2");
        }
        Ok(Self {
            working_directory,
            jobs: CommandJobs::private(limits.into()),
            shell: Ok(zevria_foundation::shell::frozen()?),
        })
    }
    pub fn with_jobs(working_directory: PathBuf, jobs: CommandJobs) -> Self {
        Self {
            working_directory,
            jobs,
            shell: zevria_foundation::shell::frozen().map_err(|e| format!("{e:#}")),
        }
    }
    async fn execute(
        &self,
        context: &mut ToolContext,
        args: CommandArgs,
    ) -> Result<String, CommandError> {
        if args.command.trim().is_empty() {
            return Err(CommandError::EmptyCommand);
        }
        if args.background == Some(true) && !self.jobs.background_enabled() {
            return Err(CommandError::InvalidArgs(
                "background commands are disabled for this tool".into(),
            ));
        }
        let shell = self
            .shell
            .as_ref()
            .map_err(|e| CommandError::Launch(std::io::Error::other(e.clone())))?;
        let mode = if args.background == Some(true) {
            CommandJobMode::Background
        } else {
            CommandJobMode::Foreground
        };
        let call_id = context.get::<ToolCallId>().ok().flatten().map(|id| id.0);
        let job = self
            .jobs
            .register(args.command.clone(), call_id, mode)
            .await
            .map_err(|e| CommandError::Job(e.to_string()))?;
        let mut guard = ForegroundGuard {
            job: job.clone(),
            armed: true,
        };
        if let Err(error) =
            supervise::spawn_job(shell, &self.working_directory, args.command, job.clone())
        {
            job.launch_failed(error.to_string()).await;
            return Err(CommandError::Launch(error));
        }
        if mode == CommandJobMode::Background {
            guard.armed = false;
            attach_job(context, &job, CommandJobDisposition::StartedInBackground);
            return Ok(format!(
                "status: running\nid: {}\nmode: background\nUse command_status to read incremental output or wait, and command_terminate to stop this job.",
                job.snapshot().id
            ));
        }
        let mut changed = job.changes();
        let cancellation = context
            .scope::<TurnContext>()
            .map(|t| t.cancellation().clone())
            .unwrap_or_default();
        loop {
            let snapshot = job.snapshot();
            if snapshot.status.is_terminal() {
                guard.armed = false;
                self.jobs.mark_reported(&snapshot.id);
                let (stdout, stderr) = job.retained();
                let output = render_output(&snapshot.status, &stdout, &stderr);
                return match snapshot.status {
                    CommandJobStatus::Exited { .. } => Ok(output),
                    CommandJobStatus::TimedOut { seconds } => {
                        Err(CommandError::Timeout { seconds, output })
                    }
                    CommandJobStatus::Terminated {
                        origin: TerminationOrigin::User,
                    } => Err(CommandError::Terminated { output }),
                    CommandJobStatus::Terminated {
                        origin: TerminationOrigin::TurnCancelled,
                    } => {
                        context
                            .insert_result(ToolCancelled)
                            .expect("cancellation markers serialize");
                        Err(CommandError::Cancelled { output })
                    }
                    CommandJobStatus::Terminated { .. } => Err(CommandError::Cancelled { output }),
                    CommandJobStatus::Failed { error } => Err(CommandError::Capture(error)),
                    CommandJobStatus::Running => unreachable!(),
                };
            }
            if snapshot.mode == CommandJobMode::Background {
                guard.armed = false;
                job.publish_mode_change().await;
                attach_job(context, &job, CommandJobDisposition::MovedToBackground);
                let (stdout, stderr) = job.take_unread();
                return Ok(format!(
                    "status: running\nid: {}\nmode: background\nmoved to the background by the user after {}s\nstdout:\n{}\nstderr:\n{}",
                    snapshot.id,
                    snapshot.started_at.elapsed().unwrap_or_default().as_secs(),
                    elide_middle(&stdout, MAX_OUTPUT_STREAM_BYTES),
                    elide_middle(&stderr, MAX_OUTPUT_STREAM_BYTES)
                ));
            }
            tokio::select! {
                biased;
                _ = changed.changed() => {},
                () = cancellation.cancelled() => {
                    if job.terminate_foreground(TerminationOrigin::TurnCancelled) {
                        // Once cancelled, wait without selecting the already-ready token.
                        while !job.snapshot().status.is_terminal() { let _ = changed.changed().await; }
                    }
                }
            }
        }
    }
}
struct ForegroundGuard {
    job: JobHandle,
    armed: bool,
}
impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        if self.armed {
            self.job.terminate_foreground(TerminationOrigin::Aborted);
        }
    }
}
fn attach_job(context: &mut ToolContext, job: &JobHandle, disposition: CommandJobDisposition) {
    context
        .insert_result(ToolResultDetail::CommandJob(CommandJobDetail {
            id: job.snapshot().id,
            disposition,
        }))
        .expect("job detail serializes");
}

#[derive(Debug)]
pub enum CommandError {
    EmptyCommand,
    InvalidArgs(String),
    Job(String),
    Launch(std::io::Error),
    Wait(std::io::Error),
    Capture(String),
    Timeout { seconds: u64, output: String },
    Cancelled { output: String },
    Terminated { output: String },
}
impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCommand => write!(f, "command must not be empty"),
            Self::InvalidArgs(s) | Self::Job(s) => f.write_str(s),
            Self::Launch(e) => write!(f, "failed to launch command: {e}"),
            Self::Wait(e) => write!(f, "failed while waiting for command: {e}"),
            Self::Capture(e) => write!(f, "failed while capturing command output: {e}"),
            Self::Timeout { seconds, output } => {
                write!(f, "command timed out after {seconds}s\n{output}")
            }
            Self::Cancelled { output } => write!(f, "command cancelled\n{output}"),
            Self::Terminated { output } => write!(f, "command terminated by the user\n{output}"),
        }
    }
}
impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Launch(e) | Self::Wait(e) => Some(e),
            _ => None,
        }
    }
}
impl Tool for CommandTool {
    const NAME: &'static str = "command";
    type Error = CommandError;
    type Args = CommandArgs;
    type Output = String;
    fn description(&self) -> String {
        let limits = self.jobs.limits();
        let description = format!(
            "Run a non-interactive shell command with Zevria's startup workspace as its working directory and return its exit status, stdout, and stderr. Commands execute with the application's permissions and are not sandboxed, read-only, or confined to the workspace. Commands time out after {} seconds. Captured output is bounded to {} bytes total, and stdout/stderr are each elided in the middle beyond {} bytes before entering model history.",
            limits.foreground_timeout.as_secs(),
            limits.capture_bytes,
            MAX_OUTPUT_STREAM_BYTES
        );
        if !self.jobs.background_enabled() {
            return description;
        }
        let description =
            description.replacen("Commands time out", "Foreground commands time out", 1);
        format!(
            "{description} Set background: true for long builds, test suites, or servers, not quick inspection commands. Background jobs return a job ID immediately; use command_status for incremental output and optional waiting, or command_terminate to stop one process tree. At most {} background jobs can run. Jobs persist across turns and Ctrl+C but end with the session; {}. A user can move a foreground command to the background or terminate it without cancelling the turn. Descendants started using & or nohup in a foreground command are still terminated when its shell exits.",
            limits.max_running_background,
            limits.background_timeout.map_or_else(
                || "there is no default background time limit".into(),
                |d| format!("the background wall-clock limit is {} seconds", d.as_secs())
            )
        )
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schema_for!(CommandArgs).to_value();
        if !self.jobs.background_enabled() {
            schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("background");
        }
        schema
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match &error {
            CommandError::EmptyCommand | CommandError::InvalidArgs(_) => {
                ToolExecutionError::invalid_args(error.to_string())
            }
            _ => ToolExecutionError::other(error.to_string()),
        }
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<String, CommandError> {
        let result = self.execute(context, args).await;
        let own = context
            .result::<ToolResultDetail>()
            .ok()
            .flatten()
            .and_then(|detail| detail.command_job().map(|job| job.id.clone()));
        let footer = job_footer(&self.jobs, own.as_ref());
        match result {
            Ok(mut output) => {
                output.push_str(&footer);
                Ok(output)
            }
            Err(mut error) => {
                if !footer.is_empty() {
                    match &mut error {
                        CommandError::Timeout { output, .. }
                        | CommandError::Cancelled { output }
                        | CommandError::Terminated { output } => output.push_str(&footer),
                        _ => return Err(CommandError::Job(format!("{error}{footer}"))),
                    }
                }
                Err(error)
            }
        }
    }
}
pub(crate) fn render_output(status: &CommandJobStatus, stdout: &str, stderr: &str) -> String {
    let (state, code) = match status {
        CommandJobStatus::Exited { code, success } => {
            (if *success { "success" } else { "failure" }, *code)
        }
        _ => ("terminated", None),
    };
    let code = code.map_or_else(|| "unavailable".into(), |c| c.to_string());
    format!(
        "status: {state}\nexit_code: {code}\nstdout:\n{}\nstderr:\n{}",
        elide_middle(stdout, MAX_OUTPUT_STREAM_BYTES),
        elide_middle(stderr, MAX_OUTPUT_STREAM_BYTES)
    )
}
pub(crate) fn job_footer(jobs: &CommandJobs, except: Option<&CommandJobId>) -> String {
    let notices = jobs.take_unreported_notices(except);
    if notices.is_empty() {
        return String::new();
    }
    let lines = notices
        .iter()
        .map(|j| format!("{}: {}", j.id, j.status))
        .collect::<Vec<_>>()
        .join("\n");
    format!("\njob updates:\n{lines}")
}

#[cfg(test)]
#[path = "command/edge_tests.rs"]
mod edge_tests;
#[cfg(test)]
#[path = "command/job_tests.rs"]
mod job_tests;
#[cfg(test)]
#[path = "command/tests.rs"]
mod tests;
