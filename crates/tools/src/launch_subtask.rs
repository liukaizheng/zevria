//! The blocking `launch_subtasks` tool.
//!
//! Whole-batch validation precedes concurrent enqueueing. Each child runs in
//! an independent subsession supervised by the composition root; the blocking
//! call collects all terminal outcomes into one input-ordered result. The tool holds only the provider-neutral
//! [`SubtaskLauncher`] channel handle, so this crate stays free of provider
//! coupling.

use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use anyhow::Context as _;
use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::SessionMode;
use zevria_foundation::ToolCancelled;
use zevria_foundation::ToolResultDetail;
use zevria_foundation::TurnId;
use zevria_foundation::subtask::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskOutcome;
use zevria_foundation::subtask::{SubtaskEntryMetadata, SubtaskStatus};
use zevria_foundation::tool_result::ToolCallId;
use zevria_session_api::TurnContext;
use zevria_session_api::subtask::ChildWorkspace;
use zevria_session_api::subtask::SubtaskLauncher;

const DESCRIPTION: &str = r#"Run a nonempty batch of independent Explore or Build subtasks and return all final outcomes in input order.

Include ALL ready independent tasks in one `tasks` array so they run concurrently, subject to the configured execution limit. Do not make separate blocking calls expecting them to overlap. A single-entry array delegates one task. Use later calls for tasks that depend on earlier results or when sequential execution was explicitly requested.

Only the final report comes back. Intermediate conversation and findings are discarded and never enter your context. Build artifacts remain on disk, including partial artifacts after failure or cancellation; no rollback or automatic cleanup is performed.

When to use:
- Delegate independent directory-owning generation or implementation to Build children, one non-overlapping workspace per child. Existing populated directories are supported; children must preserve unrelated content.
- For Explore, delegate only complex, self-contained investigations: broad or open-ended questions spanning several files or subsystems, where many searches would be needed and only the conclusion matters to you.
- Do simple lookups yourself with direct commands: finding a file or symbol, reading a known file, or anything one or two targeted searches can answer. A subtask is slower and wasteful for these.
- Do not delegate investigation whose underlying details you will need yourself — e.g. code you are about to modify. The report cannot substitute for context you must hold anyway, and you would repeat the same reading.

Usage:
- Consult the workflow policy's `subtasks` declaration and typed request directive for permitted kinds. Conditional Builder eligibility alone is not authorization: without an explicit orchestration request, use `explore` where permitted or ask the user to submit `/orchestrate <prompt>` in Build. An orchestrated request requires at least two meaningful independent accepted children in one batch; the tool itself still permits later single-entry dependency batches. Build requires `workspace`, a relative strict subdirectory of the startup workspace (not `.zevria`, an absolute path, or a path containing `.` or `..` segments). Missing directories are created without resetting existing content. Equal, ancestor, and descendant workspaces conflict; siblings do not. Leave reserved child trees untouched until their blocking calls finish. Ownership is per parent, not a cross-process lock or sandbox.
- Build children implement inside their workspace and cannot ask questions, use skills, or delegate; state any git, package, parent-build, or network authorization in the prompt.
- `type: "explore"` must omit `workspace` or set it to null. Explore children run under their own source-read-only inspection policy and cannot mutate the project.
- `title` must be nonblank and is shown to the user. Prefer a concise 3-5 word label; this word count is guidance, not a requirement.
- `prompt` is the complete, self-contained task. The subtask does not see this conversation, so include every path, term, and question it needs, and state how broad or exhaustive the investigation should be.

Both kinds share the configured execution limit (default ten). Accepted children may wait as Starting; a full launch queue waits for capacity rather than rejecting a burst.

Each launch_subtasks call blocks until every entry has a terminal outcome. The complete batch is validated before any child launches. After launch begins, a per-entry failure does not discard successful sibling reports or stop waiting for accepted children. Parent-turn cancellation cancels queued and running children; all available reports and accepted identities are retained. The result states requested/launched counts and each entry's status. Build directory creation is not transactional: preparation failure may leave newly created empty directories.

Keep each assistant tool-call response homogeneous: if it contains launch_subtasks, it contains only launch_subtasks calls. Do ordinary tool work before the launch response or after all reports return."#;

/// Arguments accepted by the [`LaunchSubtasksTool`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchSubtasksArgs {
    /// All ready independent tasks, in desired result order. Use one entry for a single child.
    #[schemars(length(min = 1))]
    pub tasks: Vec<LaunchSubtaskSpec>,
}

/// A self-contained independent task within one launch batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct LaunchSubtaskSpec {
    /// A nonblank title for the subtask, shown to the user. Prefer 3-5 words for brevity; the word count is advisory.
    pub title: String,
    /// The complete, self-contained task for the subtask agent.
    pub prompt: String,
    /// The kind of subtask agent to launch.
    pub r#type: LaunchSubtaskType,
    /// Build-only owned directory, relative to startup. Omit or use null for Explore.
    pub workspace: Option<String>,
}

/// The launchable child kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum LaunchSubtaskType {
    Explore,
    Build,
}

impl From<LaunchSubtaskType> for SubtaskKind {
    fn from(kind: LaunchSubtaskType) -> Self {
        match kind {
            LaunchSubtaskType::Explore => Self::Explore,
            LaunchSubtaskType::Build => Self::Build,
        }
    }
}

/// A subtask could not produce a report. Validation problems are model-fixable
/// invalid arguments; a failed child or missing supervisor is an internal
/// failure.
#[derive(Debug)]
pub enum LaunchSubtasksError {
    InvalidArguments(String),
    Failed(String),
    Cancelled(String),
}

impl std::fmt::Display for LaunchSubtasksError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArguments(message) | Self::Failed(message) | Self::Cancelled(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for LaunchSubtasksError {}

/// Runs independent subtasks to completion through the core subtask channels.
#[derive(Clone)]
pub struct LaunchSubtasksTool {
    launcher: SubtaskLauncher,
    startup_workspace: PathBuf,
}

impl LaunchSubtasksTool {
    pub fn new(launcher: SubtaskLauncher, startup_workspace: PathBuf) -> Self {
        Self {
            launcher,
            startup_workspace,
        }
    }

    async fn execute(
        &self,
        args: LaunchSubtasksArgs,
        call_id: &str,
        turn: TurnContext,
        result_context: Option<&mut ToolContext>,
    ) -> Result<(String, Vec<SubtaskEntryMetadata>), LaunchSubtasksError> {
        if args.tasks.is_empty() {
            return Err(LaunchSubtasksError::InvalidArguments(
                "tasks must not be empty".into(),
            ));
        }
        // Complete authorization/shape validation precedes even read-only path
        // resolution. No earlier valid entry may mutate or enqueue on rejection.
        let tasks = args
            .tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| {
                validate_task(task, turn.build_subtasks).map_err(|error| {
                    LaunchSubtasksError::InvalidArguments(format!("tasks[{index}]: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cancellation = turn.cancellation().clone();
        let workspaces = if cancellation.is_cancelled() {
            // Still retain an input-indexed terminal slot for every unlaunched entry.
            std::iter::repeat_with(|| None).take(tasks.len()).collect()
        } else {
            self.prepare_workspaces(&tasks)
                .map_err(|error| LaunchSubtasksError::InvalidArguments(format!("{error:#}")))?
        };

        // Poll every enqueue before waiting on any report. Queue capacity is
        // backpressure, not a batch-size limit. join_all preserves input order.
        let launches =
            futures_util::future::join_all(tasks.iter().zip(workspaces).enumerate().map(
                |(index, (task, workspace))| {
                    let turn = &turn;
                    let cancellation = &cancellation;
                    async move {
                        if cancellation.is_cancelled() {
                            return Err(SubtaskOutcome::Cancelled);
                        }
                        self.launcher
                            .launch(
                                call_id,
                                index,
                                task.title.clone(),
                                task.r#type.into(),
                                task.prompt.clone(),
                                turn.clone(),
                                workspace,
                            )
                            .await
                            .map_err(|error| {
                                // Classify an unaccepted slot when its enqueue finishes,
                                // not after an unrelated sibling later cancels the turn.
                                if cancellation.is_cancelled() {
                                    SubtaskOutcome::Cancelled
                                } else {
                                    SubtaskOutcome::Failed {
                                        error: error.to_string(),
                                    }
                                }
                            })
                    }
                },
            ))
            .await;
        let entries = futures_util::future::join_all(launches.into_iter().enumerate().map(
            |(index, launch)| async move {
                let (identity, outcome) = match launch {
                    Ok((identity, receiver)) => (
                        Some(identity),
                        receiver.await.unwrap_or_else(|_| SubtaskOutcome::Failed {
                            error: "the subtask supervisor shut down before the subtask reported"
                                .into(),
                        }),
                    ),
                    Err(outcome) => (None, outcome),
                };
                let status = match &outcome {
                    SubtaskOutcome::Completed { .. } => SubtaskStatus::Completed,
                    SubtaskOutcome::Failed { .. } => SubtaskStatus::Failed,
                    SubtaskOutcome::Cancelled => SubtaskStatus::Cancelled,
                };
                (
                    SubtaskEntryMetadata {
                        index,
                        status,
                        launch: identity,
                    },
                    outcome,
                )
            },
        ))
        .await;
        let launched = entries
            .iter()
            .filter(|(entry, _)| entry.launch.is_some())
            .count();
        let mut output = format!(
            "subtasks: {launched}/{} launched\nrequested: {}\nlaunched: {launched}",
            tasks.len(),
            tasks.len()
        );
        for (task, (entry, outcome)) in tasks.iter().zip(&entries) {
            let identity = entry
                .launch
                .as_ref()
                .map(|launch| format!(", id {}", launch.id))
                .unwrap_or_default();
            output.push_str(&format!(
                "\n\nentry: {}\nsubtask: {} ({}{identity})\n",
                entry.index,
                task.title,
                SubtaskKind::from(task.r#type)
            ));
            if let Some(workspace) = entry
                .launch
                .as_ref()
                .and_then(|launch| launch.workspace.as_ref())
                .or(task.workspace.as_ref())
            {
                output.push_str(&format!("workspace: {workspace}\n"));
            }
            output.push_str(&format!("status: {}\n", entry.status));
            match outcome {
                SubtaskOutcome::Completed { report } => {
                    output.push_str(&format!("report:\n{report}"))
                }
                SubtaskOutcome::Failed { error } => output.push_str(&format!("error: {error}")),
                SubtaskOutcome::Cancelled => {
                    output.push_str("error: subtask cancelled with its parent turn")
                }
            }
        }
        let metadata: Vec<_> = entries.into_iter().map(|(entry, _)| entry).collect();
        if let Some(context) = result_context {
            context.insert_result(ToolResultDetail::Subtasks(metadata.clone()));
        }
        // Cancellation is a property of the parent turn, not inferred from a
        // single child's status. Retain all successful reports on either error.
        if cancellation.is_cancelled() {
            Err(LaunchSubtasksError::Cancelled(output))
        } else if metadata
            .iter()
            .all(|entry| entry.status == SubtaskStatus::Completed)
        {
            Ok((output, metadata))
        } else {
            Err(LaunchSubtasksError::Failed(output))
        }
    }

    fn prepare_workspaces(
        &self,
        tasks: &[LaunchSubtaskSpec],
    ) -> anyhow::Result<Vec<Option<ChildWorkspace>>> {
        let resolved = tasks
            .iter()
            .map(|task| {
                task.workspace
                    .as_deref()
                    .map(|raw| resolve_workspace(&self.startup_workspace, raw))
                    .transpose()
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let targets: Vec<_> = resolved
            .iter()
            .flatten()
            .map(|workspace| workspace.target.clone())
            .collect();
        let mut reservations = self.launcher.reserve_workspaces(&targets)?.into_iter();
        resolved
            .into_iter()
            .map(|resolved| {
                resolved
                    .map(|resolved| {
                        let reservation =
                            reservations.next().expect("one reservation per workspace");
                        fs::create_dir_all(&resolved.target).with_context(|| {
                            format!("failed to create workspace {}", resolved.target.display())
                        })?;
                        let canonical = fs::canonicalize(&resolved.target)
                            .context("failed to verify created workspace")?;
                        validate_target(&resolved.startup, &resolved.protected, &canonical)?;
                        ChildWorkspace::new(canonical, resolved.display, reservation)
                    })
                    .transpose()
            })
            .collect()
        // On any failure all untransferred guards drop, including previously
        // prepared workspaces. Newly created directories deliberately remain.
    }
}

struct ResolvedWorkspace {
    startup: PathBuf,
    protected: PathBuf,
    target: PathBuf,
    display: String,
}

fn validate_target(startup: &Path, protected: &Path, target: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        target != startup && target.starts_with(startup),
        "workspace must resolve to a strict subdirectory of the startup workspace"
    );
    anyhow::ensure!(
        !target.starts_with(startup.join(".zevria")) && !target.starts_with(protected),
        "workspace must not resolve into protected startup .zevria storage"
    );
    Ok(())
}

/// Entirely read-only preparation. Inspect raw segments before `components`,
/// which normalizes internal dots away. Only native path syntax is interpreted.
fn resolve_workspace(startup: &Path, raw: &str) -> anyhow::Result<ResolvedWorkspace> {
    anyhow::ensure!(
        !raw.trim().is_empty(),
        "workspace must be a nonblank relative subdirectory"
    );
    anyhow::ensure!(
        !raw.split(|c| c == '/' || (cfg!(windows) && c == '\\'))
            .any(|part| part == "." || part == ".."),
        "workspace must not contain explicit . or .. segments"
    );
    let components = Path::new(raw)
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => anyhow::bail!(
                "workspace must be a relative subdirectory, not a root, absolute, or prefixed path"
            ),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !components.is_empty(),
        "workspace must not be the startup root"
    );
    anyhow::ensure!(
        components[0] != ".zevria",
        "workspace must not use protected startup .zevria storage"
    );
    let display = components
        .iter()
        .map(|name| name.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let startup = fs::canonicalize(startup).context("failed to resolve startup workspace")?;
    anyhow::ensure!(
        fs::metadata(&startup)?.is_dir(),
        "startup workspace is not a directory"
    );
    let protected = match fs::symlink_metadata(startup.join(".zevria")) {
        Ok(_) => fs::canonicalize(startup.join(".zevria"))
            .context("failed to resolve protected startup .zevria storage")?,
        Err(error) if error.kind() == ErrorKind::NotFound => startup.join(".zevria"),
        Err(error) => {
            return Err(error).context("failed to inspect protected startup .zevria storage");
        }
    };
    let mut ancestor = startup.clone();
    let mut missing = false;
    for component in components {
        ancestor.push(component);
        if missing {
            continue;
        }
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => {
                ancestor = fs::canonicalize(&ancestor).with_context(|| {
                    format!(
                        "failed to resolve workspace ancestor {} (possibly a dangling symlink)",
                        ancestor.display()
                    )
                })?;
                anyhow::ensure!(
                    fs::metadata(&ancestor)?.is_dir(),
                    "workspace ancestor {} is not a directory",
                    ancestor.display()
                );
                validate_target(&startup, &protected, &ancestor)?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => missing = true,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect workspace ancestor {}",
                        ancestor.display()
                    )
                });
            }
        }
    }
    validate_target(&startup, &protected, &ancestor)?;
    Ok(ResolvedWorkspace {
        startup,
        protected,
        target: ancestor,
        display,
    })
}

fn validate_task(
    mut task: LaunchSubtaskSpec,
    build_subtasks: bool,
) -> Result<LaunchSubtaskSpec, LaunchSubtasksError> {
    task.title = validate_title(&task.title)?;
    if task.prompt.trim().is_empty() {
        return Err(LaunchSubtasksError::InvalidArguments(
            "prompt must not be empty; provide the complete, self-contained task".into(),
        ));
    }
    if task.r#type == LaunchSubtaskType::Build && !build_subtasks {
        return Err(LaunchSubtasksError::InvalidArguments(
            "Builder subtasks require an explicit /orchestrate <prompt> request; use `explore` where available or ask the user to submit `/orchestrate <prompt>` in Build".into(),
        ));
    }
    match (task.r#type, &task.workspace) {
        (LaunchSubtaskType::Build, None) => {
            return Err(LaunchSubtasksError::InvalidArguments(
                "build subtasks require a non-null workspace subdirectory".into(),
            ));
        }
        (LaunchSubtaskType::Explore, Some(_)) => {
            return Err(LaunchSubtasksError::InvalidArguments(
                "explore subtasks must omit workspace or set it to null".into(),
            ));
        }
        _ => {}
    }
    Ok(task)
}

fn validate_title(title: &str) -> Result<String, LaunchSubtasksError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(LaunchSubtasksError::InvalidArguments(
            "title must not be empty; provide a nonblank label for the subtask".to_string(),
        ));
    }
    Ok(title.to_string())
}

impl Tool for LaunchSubtasksTool {
    const NAME: &'static str = LAUNCH_SUBTASKS_TOOL_NAME;

    type Error = LaunchSubtasksError;
    type Args = LaunchSubtasksArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(LaunchSubtasksArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match &error {
            LaunchSubtasksError::InvalidArguments(_) => {
                ToolExecutionError::invalid_args(error.to_string())
            }
            LaunchSubtasksError::Failed(_) | LaunchSubtasksError::Cancelled(_) => {
                ToolExecutionError::other(error.to_string())
            }
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        // A missing id degrades the frontend correlation, never the call.
        let call_id = context
            .get::<ToolCallId>()
            .map(|id| id.0.clone())
            .unwrap_or_default();
        let turn = context.get::<TurnContext>().cloned().unwrap_or_else(|| {
            TurnContext::new(
                TurnId::new(0),
                SessionMode::Build,
                tokio_util::sync::CancellationToken::new(),
            )
        });
        let execution = self.execute(args, &call_id, turn, Some(context)).await;
        if matches!(execution, Err(LaunchSubtasksError::Cancelled(_))) {
            context.insert_result(ToolCancelled);
        }
        let (output, _) = execution?;
        Ok(output)
    }
}

#[cfg(test)]
#[path = "subtask_batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
#[path = "build_subtask_tests.rs"]
mod build_tests;

#[cfg(test)]
mod tests {
    use rig_agent::tool::server::ToolServer;
    use rig_core::tool::ToolErrorKind;
    use zevria_session_api::session_event_channel;
    use zevria_session_api::subtask::SubtaskChannels;
    use zevria_session_api::subtask::subtask_channels;

    use super::*;

    #[test]
    fn explore_description_permits_investigation_without_relaxing_builder_ownership() {
        for pin in [
            "own source-read-only inspection policy",
            "cannot mutate the project",
            "workflow policy's `subtasks` declaration",
            "state any git, package, parent-build, or network authorization in the prompt",
            "Leave reserved child trees untouched",
        ] {
            assert!(DESCRIPTION.contains(pin), "missing tool contract: {pin}");
        }
        assert!(!DESCRIPTION.contains("no-mutation and startup-workspace limits"));
    }

    fn channels() -> SubtaskChannels {
        let (events_tx, _events_rx) = session_event_channel(32);
        subtask_channels("root-session", events_tx)
    }

    fn turn() -> TurnContext {
        TurnContext::new(
            TurnId::new(1),
            SessionMode::Build,
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn args(title: &str, prompt: &str) -> LaunchSubtasksArgs {
        LaunchSubtasksArgs {
            tasks: vec![LaunchSubtaskSpec {
                title: title.to_string(),
                prompt: prompt.to_string(),
                r#type: LaunchSubtaskType::Explore,
                workspace: None,
            }],
        }
    }

    /// Resolve every incoming launch request with a canned completed report.
    fn autocomplete_requests(
        mut requests: tokio::sync::mpsc::Receiver<
            zevria_session_api::subtask::SubtaskLaunchRequest,
        >,
        report: &'static str,
    ) {
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let _ = request.outcome.send(SubtaskOutcome::Completed {
                    report: report.to_string(),
                });
            }
        });
    }

    #[test]
    fn schema_is_strict_and_pins_the_type_enum() {
        let channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher, startup.path().to_path_buf());
        assert_eq!(LaunchSubtasksTool::NAME, "launch_subtasks");
        let batch_schema = tool.parameters();
        assert_eq!(batch_schema["additionalProperties"], false);
        assert_eq!(batch_schema["required"], serde_json::json!(["tasks"]));
        assert_eq!(batch_schema["properties"]["tasks"]["minItems"], 1);
        let schema = &batch_schema["properties"]["tasks"]["items"];
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["required"],
            serde_json::json!(["title", "prompt", "type"])
        );
        let type_schema = serde_json::to_string(&schema["properties"]["type"]).expect("schema");
        assert_eq!(
            schema["properties"]["type"]["enum"],
            serde_json::json!(["explore", "build"]),
            "got: {type_schema}"
        );
        let title_description = schema["properties"]["title"]["description"]
            .as_str()
            .expect("title description");
        assert!(title_description.contains("nonblank"));
        assert!(title_description.contains("3-5 words"));
        assert!(title_description.contains("advisory"));
        let description = tool.description();
        assert!(description.contains("`title` must be nonblank"));
        assert!(description.contains("guidance, not a requirement"));
        assert!(description.contains("workflow policy's `subtasks` declaration"));
        assert!(description.contains("use `explore` where permitted"));
        assert!(description.contains("ask the user to submit `/orchestrate <prompt>`"));
        assert!(!description.contains("available only in Build mode"));
        for pin in [
            "ALL ready independent tasks in one `tasks` array",
            "run concurrently",
            "Do not make separate blocking calls expecting them to overlap",
            "single-entry array",
            "every entry has a terminal outcome",
            "configured execution limit",
            "Starting",
            "per-entry failure",
            "Parent-turn cancellation",
            "requested/launched counts",
        ] {
            assert!(description.contains(pin), "missing batch contract: {pin}");
        }
        assert!(!description.contains("arrives automatically"));
        // The delegation gate is prompt-critical too: simple lookups stay
        // inline, and intermediate findings never reach the parent.
        assert!(description.contains("Do simple lookups yourself"));
        assert!(description.contains("discarded"));
        assert!(description.contains("Do not delegate investigation whose underlying details"));
        assert!(description.contains("children implement inside their workspace"));
        for owned_elsewhere in ["RTK", "`eza`", "OS-temp", "receives only `command`"] {
            assert!(!description.contains(owned_elsewhere));
        }
        assert!(!description.contains("workspace-confined"));
        assert!(!description.contains("`read`, `search`, and `list`"));
    }

    #[tokio::test]
    async fn the_call_blocks_and_returns_the_report_with_metadata() {
        let channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher.clone(), startup.path().to_path_buf());
        autocomplete_requests(channels.requests, "found it in config.rs");

        let mut context = ToolContext::new();
        context.insert(ToolCallId("call-7".to_string()));
        let returned = tool
            .call(
                &mut context,
                args(
                    "map config loading",
                    "Find where the app config file is loaded and parsed.",
                ),
            )
            .await
            .expect("the subtask should complete");

        let metadata = context
            .result::<ToolResultDetail>()
            .and_then(|detail| detail.subtasks().first())
            .and_then(|entry| entry.launch.as_ref())
            .expect("launch metadata extension");
        assert_eq!(metadata.title, "map config loading");
        assert_eq!(metadata.kind, SubtaskKind::Explore);
        for expected in [
            "subtask: map config loading",
            &format!("id {}", metadata.id),
            "status: completed",
            "found it in config.rs",
        ] {
            assert!(
                returned.contains(expected),
                "output missing {expected:?}: {}",
                returned
            );
        }
    }

    #[tokio::test]
    async fn a_failed_child_is_a_tool_error_not_a_report() {
        let mut channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher.clone(), startup.path().to_path_buf());
        let call = tokio::spawn(async move {
            let mut context = ToolContext::new();
            let result = tool
                .call(
                    &mut context,
                    args("map config loading", "a broad look at config loading"),
                )
                .await;
            (result, context)
        });
        let request = loop {
            match channels.requests.try_recv() {
                Ok(request) => break request,
                Err(_) => tokio::task::yield_now().await,
            }
        };
        let _ = request.outcome.send(SubtaskOutcome::Failed {
            error: "websocket refused".to_string(),
        });

        let (result, context) = call.await.expect("call task");
        let error = result.expect_err("a failed child fails the call");
        assert!(matches!(error, LaunchSubtasksError::Failed(_)));
        assert!(error.to_string().contains("websocket refused"));
        assert!(error.to_string().contains("map config loading"));
        assert_eq!(
            context
                .result::<ToolResultDetail>()
                .and_then(|detail| detail.subtasks().first())
                .and_then(|entry| entry.launch.as_ref())
                .map(|metadata| metadata.title.as_str()),
            Some("map config loading")
        );
    }

    #[tokio::test]
    async fn a_dropped_request_surfaces_as_unavailable() {
        let mut channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher.clone(), startup.path().to_path_buf());
        let call = tokio::spawn(async move {
            tool.call(
                &mut ToolContext::new(),
                args("map config loading", "a broad look at config loading"),
            )
            .await
        });
        // Aborted supervisor: the request is dropped unresolved.
        let request = loop {
            match channels.requests.try_recv() {
                Ok(request) => break request,
                Err(_) => tokio::task::yield_now().await,
            }
        };
        drop(request);

        let error = call
            .await
            .expect("call task")
            .expect_err("a dropped request fails the call");
        assert!(matches!(error, LaunchSubtasksError::Failed(_)));
        assert!(error.to_string().contains("supervisor"));
    }

    #[tokio::test]
    async fn nonblank_titles_preserve_wording_with_advisory_word_counts() {
        let channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher, startup.path().to_path_buf());
        autocomplete_requests(channels.requests, "synthetic report");

        for (title, expected) in [
            ("Investigate", "Investigate"),
            ("two words", "two words"),
            ("one two three", "one two three"),
            ("one two three four five", "one two three four five"),
            ("one two three four five six", "one two three four five six"),
            (
                "Summarize UI and CLI model changes",
                "Summarize UI and CLI model changes",
            ),
            (
                "Summarize all the relevant model lifecycle and configuration changes",
                "Summarize all the relevant model lifecycle and configuration changes",
            ),
            ("調査 🔎 café", "調査 🔎 café"),
            ("\u{2003}  調査概要\n\t", "調査概要"),
            ("  preserve  inner\tspacing \n", "preserve  inner\tspacing"),
        ] {
            let mut context = ToolContext::new();
            let report = tool
                .call(&mut context, args(title, "a scoped check"))
                .await
                .expect("any nonblank title should launch");
            let metadata = context
                .result::<ToolResultDetail>()
                .and_then(|detail| detail.subtasks().first())
                .and_then(|entry| entry.launch.as_ref())
                .expect("launch metadata");
            assert_eq!(metadata.title, expected);
            assert!(report.contains(&format!("subtask: {expected} (")));
        }
    }

    #[tokio::test]
    async fn blank_titles_and_prompts_reject_without_launching_or_attaching_metadata() {
        let mut channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tool = LaunchSubtasksTool::new(channels.launcher, startup.path().to_path_buf());

        for blank in ["", "   \n\t", "\u{2003}\u{a0}"] {
            for (title, prompt, field) in [
                (blank, "a scoped check", "title"),
                ("a valid title here", blank, "prompt"),
            ] {
                let mut context = ToolContext::new();
                let error = tool
                    .call(&mut context, args(title, prompt))
                    .await
                    .expect_err("blank title or prompt should fail");
                assert!(matches!(error, LaunchSubtasksError::InvalidArguments(_)));
                assert!(
                    error
                        .to_string()
                        .contains(&format!("{field} must not be empty"))
                );
                assert_eq!(tool.map_error(error).kind(), ToolErrorKind::InvalidArgs);
                assert!(context.result::<ToolResultDetail>().is_none());
                assert!(channels.requests.try_recv().is_err(), "nothing was queued");
            }
        }
    }

    #[tokio::test]
    async fn unknown_fields_are_rejected_and_failures_classified() {
        let channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        let tools = ToolServer::new()
            .tool(LaunchSubtasksTool::new(
                channels.launcher,
                startup.path().to_path_buf(),
            ))
            .run();

        let unknown = tools
            .execute(
                "launch_subtasks",
                &serde_json::json!({"tasks": [{
                    "title": "explore the codebase",
                    "prompt": "a broad look",
                    "type": "explore",
                    "extra": true
                }]})
                .to_string(),
                &mut ToolContext::new(),
            )
            .await;
        assert!(
            unknown.is_error_kind(ToolErrorKind::InvalidArgs),
            "unknown fields should be invalid arguments"
        );

        let bad_type = tools
            .execute(
                "launch_subtasks",
                &serde_json::json!({"tasks": [{
                    "title": "explore the codebase",
                    "prompt": "a broad look",
                    "type": "review"
                }]})
                .to_string(),
                &mut ToolContext::new(),
            )
            .await;
        assert!(
            bad_type.is_error_kind(ToolErrorKind::InvalidArgs),
            "unrecognized type should be invalid arguments"
        );
    }

    #[tokio::test]
    async fn closed_supervisor_is_an_internal_failure_not_invalid_args() {
        let mut channels = channels();
        let startup = tempfile::tempdir().expect("startup workspace");
        channels.requests.close();
        let tool = LaunchSubtasksTool::new(channels.launcher, startup.path().to_path_buf());

        let error = tool
            .execute(
                args("explore the codebase", "a broad look at the tree"),
                "call-1",
                turn(),
                None,
            )
            .await
            .expect_err("closed channel should fail");
        assert!(matches!(error, LaunchSubtasksError::Failed(_)));
        assert_eq!(tool.map_error(error).kind(), ToolErrorKind::Other);
        let validation = LaunchSubtasksError::InvalidArguments("bad".to_string());
        assert_eq!(
            tool.map_error(validation).kind(),
            ToolErrorKind::InvalidArgs
        );
        let failed = LaunchSubtasksError::Failed("child died".to_string());
        assert_eq!(tool.map_error(failed).kind(), ToolErrorKind::Other);
    }
}
