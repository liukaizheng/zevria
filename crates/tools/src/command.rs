//! The unrestricted shell command tool.

use std::{
    borrow::Cow,
    collections::VecDeque,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use rig_agent::tool::{Tool, ToolContext, ToolExecutionError};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use zevria_foundation::ToolCancelled;
use zevria_session_api::TurnContext;

/// The largest number of bytes from one captured stream that is returned to
/// the model. A single unbounded result (for example a broad recursive search)
/// enters the conversation history permanently and can exceed the model's
/// context window on every following request, so oversized streams are elided
/// in the middle while the leading output and the trailing errors or summary
/// survive.
const MAX_OUTPUT_STREAM_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_CAPTURE_BYTES: usize = 1024 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Keep the head and tail of an oversized stream within `max_bytes`, replacing
/// the middle with a marker that states how much was omitted. Split points are
/// moved to `char` boundaries so the result stays valid UTF-8.
fn elide_middle(stream: &str, max_bytes: usize) -> Cow<'_, str> {
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
        "{}\n[... {omitted_bytes} bytes omitted: the output exceeded the {max_bytes}-byte \
         per-stream limit; rerun a narrower command (a more specific pattern, a path filter, \
         or `head`) to see the omitted middle ...]\n{}",
        &stream[..head_end],
        &stream[tail_start..],
    ))
}

/// Arguments accepted by the [`CommandTool`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandArgs {
    /// The shell command to execute.
    pub command: String,
}

/// Runs non-interactive shell commands with Zevria's startup workspace as the
/// working directory.
#[derive(Debug, Clone)]
pub struct CommandTool {
    working_directory: PathBuf,
    limits: CommandLimits,
    shell: Result<std::sync::Arc<zevria_foundation::shell::ShellSpec>, String>,
}

/// Resource limits for one shell command.
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

impl CommandTool {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            limits: CommandLimits::default(),
            shell: zevria_foundation::shell::frozen().map_err(|error| format!("{error:#}")),
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
            limits,
            shell: Ok(zevria_foundation::shell::frozen()?),
        })
    }
}

/// A command could not be executed. These errors are returned to the model as
/// tool results so it can correct the command or choose another approach.
#[derive(Debug)]
pub enum CommandError {
    EmptyCommand,
    Launch(std::io::Error),
    Wait(std::io::Error),
    Capture(String),
    Timeout { seconds: u64, output: String },
    Cancelled { output: String },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCommand => write!(formatter, "command must not be empty"),
            Self::Launch(error) => write!(formatter, "failed to launch command: {error}"),
            Self::Wait(error) => write!(formatter, "failed while waiting for command: {error}"),
            Self::Capture(error) => {
                write!(formatter, "failed while capturing command output: {error}")
            }
            Self::Timeout { seconds, output } => {
                write!(formatter, "command timed out after {seconds}s\n{output}")
            }
            Self::Cancelled { output } => write!(formatter, "command cancelled\n{output}"),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyCommand
            | Self::Capture(_)
            | Self::Timeout { .. }
            | Self::Cancelled { .. } => None,
            Self::Launch(error) | Self::Wait(error) => Some(error),
        }
    }
}

impl Tool for CommandTool {
    const NAME: &'static str = "command";

    type Error = CommandError;
    type Args = CommandArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "Run a non-interactive shell command with Zevria's startup workspace as its working directory and return its exit status, stdout, and stderr. Commands execute with the application's permissions and are not sandboxed, read-only, or confined to the workspace. Commands time out after {} seconds. Captured output is bounded to {} bytes total, and stdout/stderr are each elided in the middle beyond {} bytes before entering model history.",
            self.limits.timeout.as_secs(),
            self.limits.capture_bytes,
            MAX_OUTPUT_STREAM_BYTES,
        )
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(CommandArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match &error {
            CommandError::EmptyCommand => ToolExecutionError::invalid_args(error.to_string()),
            CommandError::Launch(_)
            | CommandError::Wait(_)
            | CommandError::Capture(_)
            | CommandError::Timeout { .. }
            | CommandError::Cancelled { .. } => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if args.command.trim().is_empty() {
            return Err(CommandError::EmptyCommand);
        }

        let shell = self
            .shell
            .as_ref()
            .map_err(|error| CommandError::Launch(std::io::Error::other(error.clone())))?;
        let mut command = shell.command(args.command);
        command
            .current_dir(&self.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the worker task is aborted, dropping the pending child kills
            // it instead of leaving it detached in the workspace.
            .kill_on_drop(true);

        #[cfg(unix)]
        command.process_group(0);

        let (mut child, mut process_tree) =
            ProcessTreeGuard::spawn(&mut command).map_err(CommandError::Launch)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let per_stream = self.limits.capture_bytes / 2;
        let stdout_task = tokio::spawn(capture_stream(stdout, per_stream));
        let stderr_task = tokio::spawn(capture_stream(stderr, per_stream));
        let turn = context.get::<TurnContext>().cloned();

        enum Completion {
            Exited(Result<ExitStatus, std::io::Error>),
            Timeout,
            Cancelled,
        }
        let completion = if let Some(turn) = &turn {
            tokio::select! {
                status = child.wait() => Completion::Exited(status),
                () = tokio::time::sleep(self.limits.timeout) => Completion::Timeout,
                () = turn.cancellation().cancelled() => Completion::Cancelled,
            }
        } else {
            tokio::select! {
                status = child.wait() => Completion::Exited(status),
                () = tokio::time::sleep(self.limits.timeout) => Completion::Timeout,
            }
        };

        #[derive(Clone, Copy)]
        enum CompletionKind {
            Exited,
            Timeout,
            Cancelled,
        }
        let (status, kind) = match completion {
            Completion::Exited(status) => {
                let status = status.map_err(CommandError::Wait)?;
                // A non-interactive shell may exit after spawning a
                // background descendant that still owns the capture pipes.
                // End the rest of its process group before awaiting drains.
                process_tree.kill_remaining();
                (Some(status), CompletionKind::Exited)
            }
            Completion::Timeout => {
                terminate_process_tree(&mut child, &process_tree).await;
                (None, CompletionKind::Timeout)
            }
            Completion::Cancelled => {
                terminate_process_tree(&mut child, &process_tree).await;
                (None, CompletionKind::Cancelled)
            }
        };
        let stdout = stdout_task
            .await
            .map_err(|error| CommandError::Capture(error.to_string()))?
            .map_err(|error| CommandError::Capture(error.to_string()))?;
        let stderr = stderr_task
            .await
            .map_err(|error| CommandError::Capture(error.to_string()))?
            .map_err(|error| CommandError::Capture(error.to_string()))?;
        process_tree.disarm();
        let rendered = render_output(status.as_ref(), &stdout, &stderr);

        match kind {
            CompletionKind::Exited => Ok(rendered),
            CompletionKind::Timeout => Err(CommandError::Timeout {
                seconds: self.limits.timeout.as_secs(),
                output: rendered,
            }),
            CompletionKind::Cancelled => {
                context.insert_result(ToolCancelled);
                Err(CommandError::Cancelled { output: rendered })
            }
        }
    }
}

fn render_output(status: Option<&ExitStatus>, stdout: &str, stderr: &str) -> String {
    let state = if status.is_some_and(ExitStatus::success) {
        "success"
    } else if status.is_some() {
        "failure"
    } else {
        "terminated"
    };
    let exit_code = status
        .and_then(ExitStatus::code)
        .map_or_else(|| "unavailable".to_string(), |code| code.to_string());
    let stdout = elide_middle(stdout, MAX_OUTPUT_STREAM_BYTES);
    let stderr = elide_middle(stderr, MAX_OUTPUT_STREAM_BYTES);

    format!("status: {state}\nexit_code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}")
}

async fn capture_stream(
    mut stream: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> std::io::Result<String> {
    let head_limit = max_bytes / 2;
    let tail_limit = max_bytes.saturating_sub(head_limit);
    let mut head = Vec::with_capacity(head_limit);
    let mut tail = VecDeque::with_capacity(tail_limit);
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        let mut bytes = &buffer[..read];
        if head.len() < head_limit {
            let take = (head_limit - head.len()).min(bytes.len());
            head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        for byte in bytes {
            if tail.len() == tail_limit && tail_limit > 0 {
                tail.pop_front();
            }
            if tail_limit > 0 {
                tail.push_back(*byte);
            }
        }
    }
    let retained = head.len() + tail.len();
    let mut output = String::from_utf8_lossy(&head).into_owned();
    if total > retained {
        output.push_str(&format!(
            "\n[... {} bytes discarded while draining bounded command output ...]\n",
            total - retained
        ));
    }
    output.push_str(&String::from_utf8_lossy(tail.make_contiguous()));
    Ok(output)
}

struct ProcessTreeGuard {
    #[cfg(windows)]
    job: zevria_foundation::windows_process::Job,
    #[cfg(unix)]
    process_group: Option<i32>,
    armed: bool,
}

impl ProcessTreeGuard {
    fn spawn(
        command: &mut tokio::process::Command,
    ) -> std::io::Result<(tokio::process::Child, Self)> {
        #[cfg(windows)]
        let (child, job) = zevria_foundation::windows_process::Job::spawn(command)?;
        #[cfg(not(windows))]
        let child = command.spawn()?;
        let guard = Self {
            #[cfg(windows)]
            job,
            #[cfg(unix)]
            process_group: child.id().and_then(|pid| i32::try_from(pid).ok()),
            armed: true,
        };
        Ok((child, guard))
    }

    fn terminate_gracefully(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: the child was launched into a process group whose id is
            // the recorded leader pid. A negative pid targets only that group.
            unsafe {
                libc::kill(-process_group, libc::SIGTERM);
            }
        }
    }

    fn kill_remaining(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: same process-group invariant as `terminate_gracefully`.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            // Synchronous best effort is intentional: this guard also runs
            // when the async command worker itself is aborted.
            self.kill_remaining();
        }
    }
}

async fn terminate_process_tree(
    child: &mut tokio::process::Child,
    process_tree: &ProcessTreeGuard,
) {
    process_tree.terminate_gracefully();
    if matches!(
        tokio::time::timeout(TERMINATION_GRACE, child.wait()).await,
        Ok(Ok(_))
    ) {
        // The leader exited, but descendants may still own output pipes.
        process_tree.kill_remaining();
        return;
    }
    process_tree.kill_remaining();
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
#[path = "../tests/support/inspection.rs"]
mod inspection_fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::SessionMode;
    use zevria_foundation::TurnId;

    async fn run(tool: &CommandTool, command: impl Into<String>) -> Result<String, CommandError> {
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: command.into(),
            },
        )
        .await
    }

    #[test]
    fn schema_is_a_strict_single_field_object() {
        let tool = CommandTool::new(PathBuf::from("."));
        assert_eq!(CommandTool::NAME, "command");
        let description = tool.description();
        assert!(description.contains("working directory"));
        assert!(description.contains("not sandboxed, read-only, or confined"));
        assert_eq!(
            tool.parameters(),
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "CommandArgs",
                "description": "Arguments accepted by the [`CommandTool`].",
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            })
        );
    }

    #[tokio::test]
    async fn uses_the_configured_working_directory() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let expected = directory
            .path()
            .canonicalize()
            .expect("temporary directory should canonicalize");
        let output = run(
            &CommandTool::new(expected.clone()),
            "pwd; printf verified > cwd-marker",
        )
        .await
        .expect("pwd should run");

        assert!(output.contains(expected.file_name().unwrap().to_str().unwrap()));
        assert_eq!(
            std::fs::read_to_string(expected.join("cwd-marker")).unwrap(),
            "verified"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_absolute_home_parent_and_symlink_reads_keep_default_cwd() {
        let fixture = inspection_fixture::Fixture::new();
        let tool = CommandTool::new(fixture.workspace.clone());
        let result = run(&tool, fixture.read_command()).await.unwrap();
        assert!(result.contains("status: success"), "{result}");
        assert_eq!(
            result
                .matches(inspection_fixture::DIAGNOSTIC.trim())
                .count(),
            4
        );
        assert!(result.contains("project with spaces"));
        let home = run(&tool, fixture.home_read_command()).await.unwrap();
        assert!(home.contains(inspection_fixture::DIAGNOSTIC));
        let missing = run(&tool, fixture.missing_command()).await.unwrap();
        assert!(missing.contains("status: failure"));
        assert!(missing.contains("missing-diagnostic"));
        fixture.assert_unchanged();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_scratch_download_transform_script_and_local_build_preserve_sources() {
        // Execution success is NOT evidence that arbitrary writes are confined.
        // The policy tests separately pin the authoritative behavioral contract.
        let fixture = inspection_fixture::Fixture::new();
        let server = inspection_fixture::HttpFixture::start();
        let tool = CommandTool::new(fixture.workspace.clone());
        let inspection = run(&tool, fixture.read_command()).await.unwrap();
        assert!(inspection.contains("py_compile.compile"));
        let created = run(&tool, fixture.create_command()).await.unwrap();
        let scratch = fixture.own_scratch(&created);
        for command in [
            fixture.download_command(&scratch.0, &server.url),
            fixture.prepare_command(&scratch.0),
            fixture.execute_command(&scratch.0),
        ] {
            let result = run(&tool, command).await.unwrap();
            assert!(result.contains("status: success"), "{result}");
        }
        scratch.assert_results(&fixture);
        // A previous shell cd is not persistent, even while scratch exists.
        let cwd = run(&tool, "pwd").await.unwrap();
        assert!(cwd.contains(fixture.workspace.to_str().unwrap()));
        fixture.assert_unchanged();
        let cleanup = run(&tool, fixture.cleanup_command(&scratch.0))
            .await
            .unwrap();
        assert!(cleanup.contains("owned scratch cleaned"));
        assert!(!scratch.0.exists());
        fixture.assert_unchanged();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_command_environment_runs_rtk_git_and_native_development_tools() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fixture.txt"), "first\nsecond\n").unwrap();
        let tool = CommandTool::new(directory.path().to_path_buf());
        let output = run(&tool, "rtk read fixture.txt && rtk sed -n '2p' fixture.txt && git --version && rustc --version && printf '%s' \"$USERPROFILE\"").await.unwrap();
        assert!(output.contains("status: success"), "{output}");
        for expected in ["second", "git version", "rustc"] {
            assert!(output.contains(expected), "{output}");
        }
        assert!(
            output.contains(&std::env::var("USERPROFILE").unwrap()),
            "native environment is inherited"
        );
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "printf 'hello stdout'; printf 'hello stderr' >&2",
        )
        .await
        .expect("command should run");

        assert!(output.contains("status: success"));
        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("stdout:\nhello stdout"));
        assert!(output.contains("stderr:\nhello stderr"));
    }

    #[tokio::test]
    async fn inherits_the_parent_environment() {
        let expected = env!("CARGO_MANIFEST_DIR");
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "printf '%s' \"$CARGO_MANIFEST_DIR\"",
        )
        .await
        .expect("command should run");

        assert!(output.contains(expected));
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_normal_result() {
        let output = run(&CommandTool::new(PathBuf::from(".")), "exit 7")
            .await
            .expect("non-zero status should still be a result");

        assert!(output.contains("status: failure"));
        assert!(output.contains("exit_code: 7"));
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let error = run(&CommandTool::new(PathBuf::from(".")), "  \n\t")
            .await
            .expect_err("empty command should fail");
        assert!(matches!(error, CommandError::EmptyCommand));
    }

    #[tokio::test]
    async fn returns_output_larger_than_the_ui_preview_in_full() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i + 1)); done",
        )
        .await
        .expect("large-output command should run");

        let stdout = output
            .split_once("stdout:\n")
            .and_then(|(_, output)| output.split_once("\nstderr:\n"))
            .map(|(stdout, _)| stdout)
            .expect("result should contain stdout and stderr sections");
        assert_eq!(stdout.chars().count(), 5000);
        assert!(output.len() > 4096);
    }

    #[test]
    fn elide_middle_returns_streams_within_the_limit_unchanged() {
        let stream = "x".repeat(64);
        let result = elide_middle(&stream, 64);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, stream);
    }

    #[test]
    fn elide_middle_keeps_head_and_tail_and_reports_the_omission() {
        let stream = format!("{}{}", "a".repeat(1000), "b".repeat(1000));
        let result = elide_middle(&stream, 100);
        assert!(result.starts_with(&"a".repeat(50)));
        assert!(result.ends_with(&"b".repeat(50)));
        assert!(result.contains("1900 bytes omitted"));
    }

    #[test]
    fn elide_middle_respects_multibyte_character_boundaries() {
        // Two bytes per character, with an odd budget landing mid-character on
        // both sides of the elision.
        let stream = "é".repeat(200);
        let result = elide_middle(&stream, 99);
        assert!(result.starts_with(&"é".repeat(24)));
        assert!(result.ends_with(&"é".repeat(24)));
        assert!(result.contains("304 bytes omitted"));
    }

    #[tokio::test]
    async fn oversized_output_is_elided_before_reaching_the_model() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "awk 'BEGIN { for (i = 0; i < 200000; i++) printf \"x\" }'",
        )
        .await
        .expect("oversized-output command should run");

        assert!(output.contains("status: success"));
        assert!(output.contains("183616 bytes omitted"));
        assert!(
            output.len() < MAX_OUTPUT_STREAM_BYTES + 1024,
            "result should stay near the per-stream limit, got {} bytes",
            output.len()
        );
        // The trailing stdout and the stderr section both survive the elision.
        assert!(output.ends_with("\nstderr:\n"));
    }

    #[tokio::test]
    async fn capture_stream_drains_past_its_limit_under_backpressure() {
        use tokio::io::AsyncWriteExt as _;

        const RETAINED_BYTES: usize = 1024;
        let payload = format!("HEAD{}TAIL", "x".repeat(128 * 1024));
        let (mut writer, reader) = tokio::io::duplex(64);
        let write = async {
            writer.write_all(payload.as_bytes()).await?;
            writer.shutdown().await
        };
        tokio::pin!(write);
        // Fill the small pipe before starting the reader: the writer cannot
        // finish unless capture keeps draining well past its retained limit.
        assert!(futures_util::poll!(&mut write).is_pending());
        let (_, captured) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(write, capture_stream(reader, RETAINED_BYTES))
        })
        .await
        .expect("bounded capture must drain a backpressured writer to EOF")
        .expect("in-memory capture should succeed");

        let padding = "x".repeat(RETAINED_BYTES / 2 - 4);
        assert_eq!(
            captured,
            format!(
                "HEAD{padding}\n[... {} bytes discarded while draining bounded command output ...]\n{padding}TAIL",
                payload.len() - RETAINED_BYTES,
            )
        );
    }

    #[tokio::test]
    async fn configured_capture_limit_drains_but_does_not_retain_unbounded_output() {
        let tool = CommandTool::with_limits(
            PathBuf::from("."),
            CommandLimits {
                // This is a capture test, not a shell-startup benchmark. Leave
                // headroom for loaded native Windows hosts; the in-memory test
                // above exercises backpressure without subprocess startup.
                timeout: Duration::from_secs(30),
                capture_bytes: 4 * 1024,
            },
        )
        .expect("valid limits");
        // Shell builtins avoid starting an external output generator. Emit
        // 256 KiB on EACH pipe in chunks, with distinct heads and tails.
        let output = run(
            &tool,
            "printf OUT_HEAD; printf ERR_HEAD >&2; \
             i=0; while [ \"$i\" -lt 64 ]; do \
                 printf '%4096s' ''; printf '%4096s' '' >&2; i=$((i + 1)); \
             done; printf OUT_TAIL; printf ERR_TAIL >&2",
        )
        .await
        .expect("large-output command should complete without pipe deadlock");

        let padding = " ".repeat(1024 - "OUT_HEAD".len());
        let discarded = 64 * 4096 + "OUT_HEAD".len() + "OUT_TAIL".len() - 2048;
        let expected_stream = |prefix| {
            format!(
                "{prefix}_HEAD{padding}\n[... {discarded} bytes discarded while draining bounded command output ...]\n{padding}{prefix}_TAIL"
            )
        };
        assert_eq!(
            output,
            format!(
                "status: success\nexit_code: 0\nstdout:\n{}\nstderr:\n{}",
                expected_stream("OUT"),
                expected_stream("ERR"),
            )
        );
        assert!(
            output.len() < 6 * 1024,
            "bounded capture should stay close to its configured limit: {} bytes",
            output.len()
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn timeout_terminates_the_process_group_and_returns_partial_output() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::with_limits(
            directory.path().to_path_buf(),
            CommandLimits {
                // Leave enough time for a loaded CI host to spawn the shell
                // and flush its first bytes before exercising termination.
                timeout: Duration::from_millis(if cfg!(windows) { 1500 } else { 250 }),
                capture_bytes: 4 * 1024,
            },
        )
        .expect("valid limits");

        let error = run(
            &tool,
            "printf started; sleep 10; printf finished > finished",
        )
        .await
        .expect_err("command should time out");
        let CommandError::Timeout { output, .. } = error else {
            panic!("expected timeout error, got {error:?}");
        };
        assert!(output.contains("status: terminated"));
        assert!(output.contains("stdout:\nstarted"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "a timed-out descendant must not survive");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn turn_cancellation_terminates_the_process_group_and_marks_the_tool() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::new(directory.path().to_path_buf());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let turn = TurnContext::new(TurnId::new(4), SessionMode::Build, cancellation.clone());
        let worker = tokio::spawn(async move {
            let mut context = ToolContext::new();
            context.insert(turn);
            let result = tool
                .call(
                    &mut context,
                    CommandArgs {
                        command: "printf started; sleep 10; printf finished > finished".to_string(),
                    },
                )
                .await;
            (result, context)
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        let (result, context) = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("cancelled command should resolve promptly")
            .expect("command worker should not panic");
        let error = result.expect_err("cancelled command is a tool error");
        assert!(matches!(error, CommandError::Cancelled { .. }));
        assert!(context.result::<ToolCancelled>().is_some());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "a cancelled descendant must not survive");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_completed_shell_cannot_leave_a_background_descendant_or_open_capture_pipe() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("leaked");
        let tool = CommandTool::new(directory.path().to_path_buf());

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            run(
                &tool,
                "(sleep 0.3; printf leaked > leaked) & printf foreground-done",
            ),
        )
        .await
        .expect("background-held pipes must not wedge completed commands")
        .expect("foreground shell should succeed");
        assert!(output.contains("foreground-done"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "background descendants must be reaped");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn aborting_the_worker_kills_the_command_child() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::new(directory.path().to_path_buf());
        let worker =
            tokio::spawn(async move { run(&tool, "sleep 0.4; printf finished > finished").await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        worker.abort();
        let _ = worker.await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(
            !marker.exists(),
            "the shell should not survive an aborted command worker"
        );
    }
}
