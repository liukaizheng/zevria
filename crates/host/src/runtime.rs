//! Frontend-neutral ownership of one Zevria session and its pending or saved transcript.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use rig_agent::tool::server::{ToolServer, ToolServerHandle};
#[cfg(test)]
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zevria_acp::ExecutionProfile;
use zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::ModelRole;
use zevria_foundation::QUESTION_TOOL_NAME;
use zevria_foundation::SessionMode;
use zevria_foundation::SessionPolicies;
use zevria_foundation::TASK_TOOL_NAME;
use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, READ_PLAN_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME, UPDATE_PLAN_TOOL_NAME,
};
use zevria_foundation::{TurnPolicy, WorkspaceContract};
use zevria_instructions::FixedSkillRoots;
#[cfg(test)]
use zevria_instructions::SkillCatalog;
use zevria_instructions::prompts::BUILD_MODE_INSTRUCTIONS;
use zevria_instructions::prompts::PLAN_MODE_INSTRUCTIONS;
use zevria_instructions::prompts::{
    ENSEMBLE_WORKER_PLAN_INSTRUCTIONS, ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS,
};
use zevria_instructions::skill::discover_skills;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEventReceiver;
use zevria_session_api::SessionEventSender;
use zevria_session_api::SubtaskLauncher;
#[cfg(test)]
use zevria_session_api::question_channels;
#[cfg(test)]
use zevria_session_api::session_event_channel;
use zevria_session_api::{CommandJobLimits, CommandJobs, QuestionRequester};
use zevria_tools::{
    CommandLimits, CommandStatusTool, CommandTerminateTool, CommandTool, CreatePlanTool,
    DeleteTool, EditTool, LaunchSubtasksTool, QuestionTool, ReadPlanTool, ReconcileReportsTool,
    SkillReadTool, SkillTool, SubmitPlanTool, TaskTool, UpdatePlanTool, WriteTool,
};
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanHandoff;
use zevria_workflow::PlanWorkflowState;

use crate::config::{CommandConfig, Config};

fn guidance_roots(workspace: &Path) -> zevria_instructions::GuidanceRoots {
    #[cfg(not(test))]
    {
        zevria_instructions::GuidanceRoots::capture(workspace)
    }
    // Unit fixtures never inspect a developer's actual HOME guidance. CLI tests
    // exercise the production path in a subprocess with an isolated HOME.
    #[cfg(test)]
    {
        zevria_instructions::GuidanceRoots::fixture(
            Some(&workspace.join(".test-global-guidance")),
            workspace,
        )
    }
}

fn append_guidance_notices(
    notices: &mut Vec<String>,
    diagnostics: Vec<zevria_instructions::GuidanceDiagnostic>,
) {
    for diagnostic in diagnostics {
        tracing::warn!(target: "zevria::runtime", scope = diagnostic.scope.as_str(), category = ?diagnostic.category, "{diagnostic}");
        notices.push(diagnostic.to_string());
    }
}

mod restore;
mod startup;
use restore::preflight as preflight_restoration;

#[cfg(test)]
#[path = "restoration_tests.rs"]
mod restoration_tests;

/// Owned startup intent. Only explicit root replacements inherit selections;
/// independent starts use configuration defaults and resume uses its transcript.
// Startup intents are infrequent owned values, not queued per-token traffic.
#[allow(clippy::large_enum_variant)]
pub enum SessionStart {
    New {
        inherited_models: Option<zevria_model::models::SessionModels>,
    },
    Resume(PathBuf),
    FromPlan {
        handoff: PlanHandoff,
        inherited_models: Option<zevria_model::models::SessionModels>,
    },
}

/// Build the root production registry in its stable advertised order.
pub(crate) fn build_tools(
    workspace: &Path,
    launcher: SubtaskLauncher,
    questions: QuestionRequester,
    command_config: CommandConfig,
    max_plan_artifact_bytes: usize,
    jobs: Option<CommandJobs>,
) -> anyhow::Result<ToolServerHandle> {
    let workspace = workspace.to_path_buf();
    let server = if let Some(jobs) = jobs {
        ToolServer::new()
            .tool(CommandTool::with_jobs(workspace.clone(), jobs.clone()))
            .tool(CommandStatusTool::new(jobs.clone()))
            .tool(CommandTerminateTool::new(jobs))
    } else {
        ToolServer::new().tool(command_tool(workspace.clone(), command_config)?)
    };
    let server = server
        .tool(TaskTool)
        .tool(LaunchSubtasksTool::new(launcher, workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(DeleteTool::new(workspace))
        .tool(SkillTool)
        .tool(SkillReadTool);
    Ok(server
        .tool(ReconcileReportsTool)
        .tool(QuestionTool::new(questions))
        .tool(CreatePlanTool::new(max_plan_artifact_bytes))
        .tool(UpdatePlanTool::new(max_plan_artifact_bytes))
        .tool(ReadPlanTool)
        .tool(SubmitPlanTool::new(max_plan_artifact_bytes))
        .run())
}

/// Explore children register only the configured command tool. Structured
/// mutation, skills, planning completion, and nested delegation cannot be
/// reached even if a policy check is bypassed; source-read-only investigation
/// with scratch-contained execution is behavioral, not a command sandbox.
pub(crate) fn build_explore_tools(
    workspace: &Path,
    command_config: CommandConfig,
) -> anyhow::Result<ToolServerHandle> {
    let workspace = workspace.to_path_buf();
    Ok(ToolServer::new()
        .tool(command_tool(workspace, command_config)?)
        .run())
}

/// Private Build-child registry. Both provider and engine receive this same
/// handle; no privileged root tools are physically registered.
pub(crate) fn build_isolated_build_tools(
    child_workspace: &Path,
    command_config: CommandConfig,
) -> anyhow::Result<ToolServerHandle> {
    let workspace = child_workspace.to_path_buf();
    Ok(ToolServer::new()
        .tool(command_tool(workspace.clone(), command_config)?)
        .tool(TaskTool)
        .tool(EditTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(DeleteTool::new(workspace))
        .run())
}

pub(crate) fn command_job_limits(config: CommandConfig) -> CommandJobLimits {
    CommandJobLimits {
        max_running_background: config.max_background_jobs,
        retained_finished: 8,
        capture_bytes: config.capture_bytes,
        foreground_timeout: std::time::Duration::from_secs(config.timeout_seconds),
        background_timeout: config
            .background_timeout_seconds
            .map(std::time::Duration::from_secs),
    }
}

pub(crate) fn command_tool(
    workspace: PathBuf,
    config: CommandConfig,
) -> anyhow::Result<CommandTool> {
    CommandTool::with_limits(
        workspace,
        CommandLimits {
            timeout: std::time::Duration::from_secs(config.timeout_seconds),
            capture_bytes: config.capture_bytes,
        },
    )
}

pub(crate) fn build_session_policies(
    plan: zevria_workflow::config::PlanConfig,
    command_jobs: bool,
) -> SessionPolicies {
    let mut build_tools = [
        zevria_foundation::WEB_SEARCH_TOOL_NAME,
        "command",
        TASK_TOOL_NAME,
        "edit",
        "write",
        "delete",
        LAUNCH_SUBTASKS_TOOL_NAME,
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    build_tools.extend(zevria_foundation::SKILL_TOOL_NAMES.map(str::to_string));
    let mut plan_tools = vec![
        "command".to_string(),
        zevria_foundation::WEB_SEARCH_TOOL_NAME.into(),
    ];
    if command_jobs {
        build_tools.splice(
            2..2,
            zevria_foundation::COMMAND_JOB_TOOL_NAMES.map(str::to_string),
        );
        plan_tools.splice(
            1..1,
            zevria_foundation::COMMAND_JOB_TOOL_NAMES.map(str::to_string),
        );
    }
    if plan.allow_subtasks {
        plan_tools.push(LAUNCH_SUBTASKS_TOOL_NAME.to_string());
    }
    if plan.allow_skills {
        plan_tools.extend(zevria_foundation::SKILL_TOOL_NAMES.map(str::to_string));
    }
    plan_tools.push(QUESTION_TOOL_NAME.to_string());
    plan_tools.extend(
        [
            CREATE_PLAN_TOOL_NAME,
            UPDATE_PLAN_TOOL_NAME,
            READ_PLAN_TOOL_NAME,
            SUBMIT_PLAN_TOOL_NAME,
        ]
        .map(str::to_string),
    );
    SessionPolicies::new(
        TurnPolicy::new(
            BUILD_MODE_INSTRUCTIONS,
            Some(build_tools),
            ModelRole::Build,
            true,
        )
        .with_orchestration(),
        TurnPolicy::new(
            PLAN_MODE_INSTRUCTIONS,
            Some(plan_tools),
            ModelRole::Plan,
            plan.allow_skills,
        )
        .with_contract(WorkspaceContract::SourceReadOnlyScratch),
    )
}

pub(crate) fn worker_policies() -> SessionPolicies {
    let review = TurnPolicy::new(
        ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS,
        Some(vec![
            "command".into(),
            QUESTION_TOOL_NAME.into(),
            zevria_foundation::WEB_SEARCH_TOOL_NAME.into(),
        ]),
        ModelRole::Review,
        false,
    )
    .with_scope("worker:review")
    .with_contract(WorkspaceContract::SourceReadOnlyScratch);
    // Review uses nominal Build mode without root orchestration eligibility.
    SessionPolicies::new(
        review,
        TurnPolicy::new(
            ENSEMBLE_WORKER_PLAN_INSTRUCTIONS,
            Some(vec![
                zevria_foundation::WEB_SEARCH_TOOL_NAME.into(),
                "command".into(),
                QUESTION_TOOL_NAME.into(),
                CREATE_PLAN_TOOL_NAME.into(),
                UPDATE_PLAN_TOOL_NAME.into(),
                READ_PLAN_TOOL_NAME.into(),
                SUBMIT_PLAN_TOOL_NAME.into(),
            ]),
            ModelRole::Plan,
            false,
        )
        .with_scope("worker:plan")
        .with_contract(WorkspaceContract::SourceReadOnlyScratch),
    )
}

fn worker_tools(
    workspace: &Path,
    questions: QuestionRequester,
    config: &Config,
) -> anyhow::Result<ToolServerHandle> {
    Ok(ToolServer::new()
        .tool(command_tool(workspace.to_path_buf(), config.command)?)
        .tool(QuestionTool::new(questions))
        .tool(CreatePlanTool::new(config.session.plan.max_artifact_bytes))
        .tool(UpdatePlanTool::new(config.session.plan.max_artifact_bytes))
        .tool(ReadPlanTool)
        .tool(SubmitPlanTool::new(config.session.plan.max_artifact_bytes))
        .run())
}

/// Child native transcripts and leases never participate in root discovery.
pub fn sessions_dir(workspace: &Path, profile: ExecutionProfile) -> PathBuf {
    match profile {
        ExecutionProfile::Interactive => transcript::sessions_dir(workspace),
        ExecutionProfile::EnsembleWorker => {
            zevria_foundation::runtime_paths::workspace_state_root(workspace)
                .join("ensemble-sessions")
        }
    }
}

fn ensure_worker_ignore_guard(directory: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(directory)?;
    let path = directory.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file
            .write_all(b"*\n")
            .and_then(|()| file.sync_all())
            .with_context(|| {
                format!(
                    "failed to persist worker ignore guard at {}",
                    path.display()
                )
            }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to create worker ignore guard at {}", path.display())),
    }
}

/// Shared root/worker startup boundary. Scan only the selected namespace and
/// acquire ownership before transcript loading/repair or any engine work. Both
/// enumeration and bounded coordinator waits run off the async executor.
async fn prepare_session_lease(
    path: &Path,
) -> anyhow::Result<Arc<crate::session_lease::RootSessionLease>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let directory = path.parent().context("session transcript has no parent")?;
        std::fs::create_dir_all(directory)?;
        crate::session_lease::sweep_inactive(directory);
        crate::session_lease::RootSessionLease::acquire(&path).map(Arc::new)
    })
    .await
    .context("session lease startup task failed")?
}

pub async fn start_session_with_profile(
    config: &Config,
    workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
) -> anyhow::Result<RunningSession> {
    startup::start(config, workspace, start, profile).await
}

/// Capture startup discovery once: native global skills overridden by project
/// definitions. Reload uses LocalSkillService's captured roots, not this helper.
/// Missing HOME degrades to project-only.
pub(crate) fn load_session_skills(workspace: &Path) -> zevria_instructions::SkillDiscovery {
    let roots = FixedSkillRoots::capture(workspace);
    let discovery = discover_skills(&roots);
    for diagnostic in &discovery.diagnostics {
        tracing::warn!(target: "zevria::runtime",
            scope = %diagnostic.scope,
            source = diagnostic.source.as_ref().map(|path| path.display().to_string()),
            "{}",
            diagnostic.message
        );
    }
    if !discovery.selected.is_empty() {
        tracing::info!(target: "zevria::runtime",
            skills = discovery.selected.len(),
            diagnostics = discovery.diagnostics.len(),
            omitted_diagnostics = discovery.omitted_diagnostics,
            "loaded the session skill registry"
        );
    }
    discovery
}

pub(crate) fn transcript_recovery_notice(path: &Path, malformed_lines: usize) -> Option<String> {
    (malformed_lines > 0).then(|| {
        format!(
            "Recovered the session transcript at {} by removing {malformed_lines} malformed record(s).",
            path.display()
        )
    })
}

async fn join_with_grace(mut task: tokio::task::JoinHandle<()>, name: &str) -> anyhow::Result<()> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await {
        Ok(result) => result.with_context(|| format!("{name} task failed")),
        Err(_) => {
            tracing::warn!(target: "zevria::runtime",
                task = name,
                "task exceeded the shutdown grace period; aborting"
            );
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => Ok(()),
                Ok(()) => Ok(()),
                Err(error) => Err(anyhow::Error::from(error)
                    .context(format!("{name} task failed while being aborted"))),
            }
        }
    }
}

/// Metadata frontends use to restore their own presentation state.
pub struct SessionRestoration {
    pub model_contexts: [zevria_foundation::ModelContextPolicy; ModelRole::COUNT],
    pub reasoning_levels: [zevria_foundation::ReasoningLevel; ModelRole::COUNT],
    pub model_snapshots: Vec<zevria_model::ContextTokenSnapshot>,
    pub sessions_dir: PathBuf,
    pub session_id: String,
    pub subsessions_dir: PathBuf,
    pub agent_runs_root: PathBuf,
    /// Intended path; a new unused session has no transcript file yet.
    pub transcript_path: PathBuf,
    /// Authoritative startup items, including pending in-memory metadata.
    pub transcript_items: Vec<TranscriptItem>,
    pub skill_catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    /// Validated startup pins; frontends must not replay the transcript again.
    pub active_skills: zevria_instructions::skill::ActiveSkills,
    pub startup_notices: Vec<String>,
    /// Snapshot from the fully configured engine, captured before spawning it.
    pub plan_state: PlanWorkflowState,
    /// Authoritative selection from the engine, independent of Plan presentation.
    pub selected_mode: SessionMode,
}

/// One-time, host-loaded frontend restoration payload. Frontends never reopen
/// transcript files; the runtime retains paths and lease metadata for shutdown.
pub struct RestoredSession {
    pub model_contexts: [zevria_foundation::ModelContextPolicy; ModelRole::COUNT],
    pub reasoning_levels: [zevria_foundation::ReasoningLevel; ModelRole::COUNT],
    pub model_snapshots: Vec<zevria_model::ContextTokenSnapshot>,
    pub sessions_dir: PathBuf,
    pub session_id: String,
    pub subsessions_dir: PathBuf,
    pub agent_runs_root: PathBuf,
    /// Intended path; a new unused session has no transcript file yet.
    pub transcript_path: PathBuf,
    /// Authoritative startup items, including pending in-memory metadata.
    pub transcript_items: Vec<TranscriptItem>,
    pub skill_catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    /// Validated startup pins; frontends must not replay the transcript again.
    pub active_skills: zevria_instructions::skill::ActiveSkills,
    pub startup_notices: Vec<String>,
    /// Snapshot from the fully configured engine, captured before spawning it.
    pub plan_state: PlanWorkflowState,
    /// Authoritative selection from the engine, independent of Plan presentation.
    pub selected_mode: SessionMode,
    pub subtasks: Vec<RestoredSubtask>,
    pub agent_runs: Vec<RestoredAgentRun>,
}
pub struct RestoredSubtask {
    pub id: zevria_foundation::SubtaskId,
    pub launch: Option<zevria_foundation::SubtaskLaunchMetadata>,
    pub items: Vec<TranscriptItem>,
}
pub struct RestoredAgentRun {
    pub run_id: zevria_workflow::EnsembleRunId,
    pub descriptor: zevria_workflow::AgentRunDescriptor,
    pub path: PathBuf,
    pub records: Vec<zevria_transcript::AgentRunTranscriptRecord>,
}

/// One observed engine/supervisor task ending while a frontend is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTaskExit {
    pub component: &'static str,
    pub error: Option<String>,
}

/// Complete running-session ownership. Frontends receive cloned command
/// senders and take the sole event receiver, while this handle retains task,
/// transcript, and shutdown ownership.
pub struct RunningSession {
    command_jobs: Option<CommandJobs>,
    // Only existing histories use conservative legacy cleanup. A new pending
    // path may acquire a competing file after allocation; shutdown must not
    // delete someone else's file after rejected no-clobber publication.
    cleanup_abandoned_history: bool,
    restored_taken: bool,
    restored_subtasks: Vec<RestoredSubtask>,
    restored_agent_runs: Vec<RestoredAgentRun>,
    pub(crate) restoration: SessionRestoration,
    // Held until background shutdown and abandoned-root cleanup finish. The
    // engine also owns an Arc: dropping a frontend must not release a live engine.
    _lease: Option<Arc<crate::session_lease::RootSessionLease>>,
    command_tx: Option<UnboundedSender<SessionCommand>>,
    event_tx: Option<SessionEventSender>,
    event_rx: Option<SessionEventReceiver>,
    exit_rx: Option<UnboundedReceiver<RuntimeTaskExit>>,
    engine_task: Option<tokio::task::JoinHandle<()>>,
    supervisor_task: Option<tokio::task::JoinHandle<()>>,
}

impl RunningSession {
    /// Read-only frontend restoration data. The runtime retains the authoritative
    /// transcript path used during shutdown and abandoned-root cleanup.
    pub fn restoration(&self) -> &SessionRestoration {
        &self.restoration
    }

    /// Move the startup payload exactly once. Subsequent attempts are programmer
    /// errors, like taking the sole receiver twice, rather than empty restores.
    pub fn take_restored(&mut self) -> RestoredSession {
        assert!(
            !self.restored_taken,
            "session restoration payload was already taken"
        );
        self.restored_taken = true;
        let meta = &mut self.restoration;
        RestoredSession {
            model_contexts: meta.model_contexts.clone(),
            reasoning_levels: meta.reasoning_levels,
            model_snapshots: std::mem::take(&mut meta.model_snapshots),
            sessions_dir: meta.sessions_dir.clone(),
            session_id: meta.session_id.clone(),
            subsessions_dir: meta.subsessions_dir.clone(),
            agent_runs_root: meta.agent_runs_root.clone(),
            transcript_path: meta.transcript_path.clone(),
            transcript_items: std::mem::take(&mut meta.transcript_items),
            skill_catalog: meta.skill_catalog.clone(),
            active_skills: std::mem::take(&mut meta.active_skills),
            startup_notices: std::mem::take(&mut meta.startup_notices),
            plan_state: std::mem::take(&mut meta.plan_state),
            selected_mode: meta.selected_mode,
            subtasks: std::mem::take(&mut self.restored_subtasks),
            agent_runs: std::mem::take(&mut self.restored_agent_runs),
        }
    }

    pub fn command_sender(&self) -> UnboundedSender<SessionCommand> {
        self.command_tx
            .as_ref()
            .expect("running session command endpoint already closed")
            .clone()
    }

    pub fn take_event_receiver(&mut self) -> anyhow::Result<SessionEventReceiver> {
        self.event_rx
            .take()
            .context("running session event receiver was already taken")
    }

    pub fn take_exit_receiver(&mut self) -> anyhow::Result<UnboundedReceiver<RuntimeTaskExit>> {
        self.exit_rx
            .take()
            .context("running session background-exit receiver was already taken")
    }

    /// Preserve the established ownership sequence: signal Shutdown, drop
    /// command and event endpoints, await engine, await supervisor, then remove
    /// an eligible existing abandoned transcript best-effort. An absent pending
    /// transcript is normal and shutdown does not materialize it.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(command_tx) = self.command_tx.take() {
            let _ = command_tx.send(SessionCommand::Control(
                zevria_session_api::ControlCommand::Shutdown,
            ));
            drop(command_tx);
        }
        self.event_rx.take();
        self.event_tx.take();
        self.exit_rx.take();

        let engine_shutdown = if let Some(engine_task) = self.engine_task.take() {
            join_with_grace(engine_task, "session engine").await
        } else {
            Ok(())
        };
        // Completion publication cannot schedule work after the engine exits.
        if let Some(jobs) = &self.command_jobs {
            jobs.close();
        }
        let supervisor_shutdown = if let Some(supervisor_task) = self.supervisor_task.take() {
            join_with_grace(supervisor_task, "subtask supervisor").await
        } else {
            Ok(())
        };

        if let Some(jobs) = self.command_jobs.take() {
            jobs.shutdown(std::time::Duration::from_secs(5)).await;
        }

        let transcript_is_empty = self.cleanup_abandoned_history
            && transcript::is_abandoned_root(&self.restoration.transcript_path);
        if transcript_is_empty
            && let Err(error) = std::fs::remove_file(&self.restoration.transcript_path)
        {
            tracing::warn!(target: "zevria::runtime",
                "failed to remove the empty session file at {}: {error}",
                self.restoration.transcript_path.display()
            );
        }

        engine_shutdown.and(supervisor_shutdown)
    }
}

/// Start one provider connection, possibly pending transcript writer, engine, ensemble
/// supervisor, question broker, and subtask supervisor without constructing a
/// terminal frontend.
pub async fn start_session(
    app_config: &Config,
    workspace: &Path,
    start: SessionStart,
) -> anyhow::Result<RunningSession> {
    startup::start(app_config, workspace, start, ExecutionProfile::Interactive).await
}

trait RuntimeTaskOutcome {
    fn error(self) -> Option<String>;
}

impl RuntimeTaskOutcome for () {
    fn error(self) -> Option<String> {
        None
    }
}

impl RuntimeTaskOutcome for Result<(), zevria_transcript::SessionReplayError> {
    fn error(self) -> Option<String> {
        self.err().map(|error| error.to_string())
    }
}

fn observe_task<T: RuntimeTaskOutcome + Send + 'static>(
    component: &'static str,
    task: tokio::task::JoinHandle<T>,
    exits: UnboundedSender<RuntimeTaskExit>,
) -> tokio::task::JoinHandle<()> {
    observe_task_retaining(component, task, exits, ())
}

fn observe_task_retaining<T: RuntimeTaskOutcome + Send + 'static, G: Send + 'static>(
    component: &'static str,
    task: tokio::task::JoinHandle<T>,
    exits: UnboundedSender<RuntimeTaskExit>,
    guard: G,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _guard = guard;
        match task.await {
            Ok(outcome) => {
                let _ = exits.send(RuntimeTaskExit {
                    component,
                    error: outcome.error(),
                });
            }
            Err(error) => {
                let detail = error.to_string();
                let _ = exits.send(RuntimeTaskExit {
                    component,
                    error: Some(detail.clone()),
                });
                panic!("{component} task failed: {detail}");
            }
        }
    })
}

#[cfg(all(test, unix))]
#[path = "guidance_runtime_tests.rs"]
mod guidance_tests;

#[cfg(all(test, unix))]
#[path = "instruction_smoke_tests.rs"]
mod instruction_smoke_tests;

#[cfg(all(test, unix, feature = "cache-diagnostics"))]
#[path = "cache_diagnostics_runtime_tests.rs"]
mod cache_diagnostic_tests;

#[cfg(test)]
pub(crate) fn rendered_instructions(policy: &TurnPolicy) -> String {
    let set = zevria_instructions::InstructionSet {
        application: String::new(),
        system: Vec::new(),
        workflow: zevria_instructions::DirectivePolicy::new(&policy.scope, policy),
        catalog: None,
    };
    set.validate().unwrap();
    set.render()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use zevria_instructions::SkillCatalog;
    use zevria_instructions::SkillDefinition;
    use zevria_session_api::SessionCommand;
    use zevria_workflow::PlanWorkflowState;

    use super::*;

    async fn assert_session_lease_reclaimed(path: &Path) {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || crate::session_lease::tests::assert_reclaimed(&path))
            .await
            .expect("session lease reclamation check");
    }

    #[test]
    fn ordinary_plan_keeps_shared_inspection_policy_with_every_optional_setting() {
        use zevria_instructions::prompts::INSPECTION_POLICY_INSTRUCTIONS;
        for allow_subtasks in [false, true] {
            for allow_skills in [false, true] {
                let policies = build_session_policies(
                    zevria_workflow::config::PlanConfig {
                        allow_subtasks,
                        allow_skills,
                        ..Default::default()
                    },
                    true,
                );
                let plan = policies.policy(SessionMode::Plan);
                assert_eq!(plan.instructions, PLAN_MODE_INSTRUCTIONS);
                assert_eq!(
                    rendered_instructions(plan)
                        .matches(INSPECTION_POLICY_INSTRUCTIONS)
                        .count(),
                    1
                );
                assert_eq!(plan.model_role, ModelRole::Plan);
                assert_eq!(plan.skills_enabled, allow_skills);
                assert_eq!(plan.allows_tool("launch_subtasks"), allow_subtasks);
                for tool in zevria_foundation::SKILL_TOOL_NAMES {
                    assert_eq!(plan.allows_tool(tool), allow_skills);
                }
                for tool in ["command", "web_search", "question", "submit_plan"] {
                    assert!(plan.allows_tool(tool));
                }
                for tool in ["edit", "write", "delete", "task", "reconcile_reports"] {
                    assert!(!plan.allows_tool(tool));
                }
                assert_eq!(
                    policies.policy(SessionMode::Build).instructions,
                    BUILD_MODE_INSTRUCTIONS
                );
                assert!(policies.policy(SessionMode::Build).orchestration);
            }
        }
    }

    #[tokio::test]
    async fn worker_registry_and_policies_are_least_privilege_with_independent_roles() {
        let workspace = tempfile::tempdir().unwrap();
        let (events, _receiver) = session_event_channel(8);
        let questions = question_channels(events);
        let tools = worker_tools(
            workspace.path(),
            questions.requester,
            &crate::config::test_config(),
        )
        .unwrap();
        let definitions = tools.snapshot().take_definitions();
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [
                "command",
                "question",
                "create_plan",
                "update_plan",
                "read_plan",
                "submit_plan"
            ]
        );
        let policies = worker_policies();
        for (mode, role, expected) in [
            (
                zevria_foundation::SessionMode::Build,
                ModelRole::Review,
                vec!["command", "question", "web_search"],
            ),
            (
                zevria_foundation::SessionMode::Plan,
                ModelRole::Plan,
                vec![
                    "web_search",
                    "command",
                    "question",
                    "create_plan",
                    "update_plan",
                    "read_plan",
                    "submit_plan",
                ],
            ),
        ] {
            let policy = policies.policy(mode);
            assert_eq!(policy.model_role, role);
            assert!(!policy.skills_enabled);
            assert_eq!(policy.allowed_tool_names.as_ref().unwrap(), &expected);
            let rendered = rendered_instructions(policy);
            assert!(rendered.contains("prefix shell commands with `rtk`"));
            assert!(rendered.contains("not an OS sandbox"));
            assert_eq!(
                policy.instructions,
                if mode == SessionMode::Plan {
                    ENSEMBLE_WORKER_PLAN_INSTRUCTIONS
                } else {
                    ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS
                }
            );
            assert_eq!(
                rendered
                    .matches(zevria_instructions::prompts::INSPECTION_POLICY_INSTRUCTIONS)
                    .count(),
                1
            );
            assert!(!policy.orchestration);
            assert_eq!(policy.contract, WorkspaceContract::SourceReadOnlyScratch);
            if mode == SessionMode::Plan {
                assert_eq!(policy.scope, "worker:plan");
                assert!(policy.instructions.contains("Only host `/confirm`"));
                assert!(policy.instructions.contains(
                    "adapter publishes only Ready artifacts on the structured ACP Plan channel"
                ));
            } else {
                assert_eq!(policy.scope, "worker:review");
                assert!(!policy.instructions.contains("Only host `/confirm`"));
            }
            for absent in [
                "edit",
                "write",
                "delete",
                "task",
                "skill",
                "skill_read",
                "launch_subtasks",
                "reconcile_reports",
            ] {
                assert!(!policy.allows_tool(absent), "{absent}");
                assert!(!definitions.iter().any(|tool| tool.name == absent));
            }
        }
    }

    #[tokio::test]
    async fn unused_worker_stays_unmaterialized_and_shutdown_releases_isolated_leases() {
        let workspace = tempfile::tempdir().unwrap();
        let config = crate::config::test_config();
        let worker = start_session_with_profile(
            &config,
            workspace.path(),
            SessionStart::New {
                inherited_models: None,
            },
            ExecutionProfile::EnsembleWorker,
        )
        .await
        .unwrap();
        let path = worker.restoration.transcript_path.clone();
        assert!(!path.exists());
        assert_eq!(worker.restoration.plan_state, PlanWorkflowState::Idle);
        assert_eq!(worker.restoration.selected_mode, SessionMode::Build);
        assert!(worker.supervisor_task.is_none());
        assert!(crate::session_lease::RootSessionLease::acquire(&path).is_err());
        assert!(
            transcript::list_sessions(&transcript::sessions_dir(workspace.path()))
                .unwrap()
                .is_empty()
        );
        assert!(
            transcript::latest_session_file(&transcript::sessions_dir(workspace.path()))
                .unwrap()
                .is_none()
        );
        worker.shutdown().await.unwrap();
        assert!(!path.exists());
        assert_session_lease_reclaimed(&path).await;
        assert!(!path.with_extension("jsonl.lock").exists());
        assert!(crate::session_lease::RootSessionLease::acquire(&path).is_ok());
        let directory = path.parent().unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join(".gitignore")).unwrap(),
            "*\n"
        );
        std::fs::write(directory.join(".gitignore"), "user guard\n").unwrap();
        ensure_worker_ignore_guard(directory).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join(".gitignore")).unwrap(),
            "user guard\n"
        );
        assert!(!transcript::plans_dir(workspace.path()).exists());
        assert!(!workspace.path().join(".zevria/agent-runs").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn engine_fatal_exit_precedes_dependent_supervisor_channel_exhaustion() {
        for _ in 0..20 {
            let (requests, mut receive) = unbounded_channel::<()>();
            let keepalive = requests.clone();
            let (exits, mut observed) = unbounded_channel();
            let engine = observe_task_retaining(
                "engine",
                tokio::spawn(async move {
                    drop(requests);
                    Err(zevria_transcript::SessionReplayError::Plan(
                        "original failure".into(),
                    ))
                }),
                exits.clone(),
                keepalive,
            );
            let supervisor = observe_task(
                "supervisor",
                tokio::spawn(async move {
                    assert!(receive.recv().await.is_none());
                }),
                exits,
            );
            let first = observed.recv().await.unwrap();
            assert_eq!(first.component, "engine");
            assert_eq!(
                first.error.as_deref(),
                Some("invalid Plan replay: original failure")
            );
            assert_eq!(observed.recv().await.unwrap().component, "supervisor");
            engine.await.unwrap();
            supervisor.await.unwrap();
        }
    }

    #[tokio::test]
    async fn returned_replay_failure_is_reported_without_panic_and_releases_engine_lease() {
        for profile in [
            ExecutionProfile::Interactive,
            ExecutionProfile::EnsembleWorker,
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let directory = sessions_dir(workspace.path(), profile);
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("invalid.jsonl");
            let bytes = b"invalid committed evidence\n";
            std::fs::write(&path, bytes).unwrap();
            let lease = Arc::new(crate::session_lease::RootSessionLease::acquire(&path).unwrap());
            let weak = Arc::downgrade(&lease);
            let error = zevria_transcript::SessionReplayError::Both {
                skills: "skill diagnostic".into(),
                plan: "Plan diagnostic".into(),
            };
            let expected = error.to_string();
            let (exits, mut receive) = unbounded_channel();
            let observer = observe_task(
                "session engine",
                tokio::spawn(async move {
                    let _lease = lease;
                    Err(error)
                }),
                exits,
            );
            observer
                .await
                .expect("returned fatal error is not a task panic");
            assert_eq!(
                receive.recv().await.unwrap().error.as_deref(),
                Some(expected.as_str())
            );
            assert!(receive.recv().await.is_none(), "one runtime exit");
            assert_eq!(weak.strong_count(), 0);
            assert_session_lease_reclaimed(&path).await;
            assert!(!path.with_extension("jsonl.lock").exists());
            assert!(!transcript::is_abandoned_root(&path));
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[tokio::test]
    async fn pending_worker_shutdown_never_deletes_a_competing_abandoned_file() {
        for models_only in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let mut worker = start_session_with_profile(
                &crate::config::test_config(),
                workspace.path(),
                SessionStart::New {
                    inherited_models: None,
                },
                ExecutionProfile::EnsembleWorker,
            )
            .await
            .unwrap();
            let path = worker.restoration.transcript_path.clone();
            assert!(!path.exists());
            let bytes = if models_only {
                let mut bytes =
                    serde_json::to_vec(&worker.restoration.transcript_items[0]).unwrap();
                bytes.push(b'\n');
                bytes
            } else {
                Vec::new()
            };
            std::fs::write(&path, &bytes).unwrap();
            assert!(transcript::is_abandoned_root(&path));
            let mut events = worker.take_event_receiver().unwrap();
            worker
                .command_sender()
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        text: "must not be accepted".into(),
                        mode: SessionMode::Build,
                        behavior: zevria_foundation::RequestBehavior::Standard,
                    },
                ))
                .unwrap();
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(
                    event,
                    zevria_session_api::SessionUpdate::Lifecycle(
                        zevria_session_api::SessionEvent::TurnRejected { .. }
                    )
                ),
                "{event:?}"
            );
            worker.shutdown().await.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_session_lease_reclaimed(&path).await;
            assert!(!path.with_extension("jsonl.lock").exists());
        }
    }

    #[tokio::test]
    async fn dropping_frontend_or_aborting_observer_keeps_a_surviving_engine_leased() {
        for abort_observer in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let mut worker = start_session_with_profile(
                &crate::config::test_config(),
                workspace.path(),
                SessionStart::New {
                    inherited_models: None,
                },
                ExecutionProfile::EnsembleWorker,
            )
            .await
            .unwrap();
            let path = worker.restoration.transcript_path.clone();
            let sidecar = path.with_extension("jsonl.lock");
            let commands = worker.command_sender();
            let _events = worker.take_event_receiver().unwrap();
            let lease = Arc::downgrade(worker._lease.as_ref().unwrap());
            let mut observer = worker.engine_task.take();
            if abort_observer {
                let task = observer.take().unwrap();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            drop(worker);
            assert_eq!(lease.strong_count(), 1, "only the engine retains ownership");
            assert!(sidecar.exists());
            assert!(
                prepare_session_lease(&path).await.is_err(),
                "startup sweep must preserve the engine"
            );
            commands
                .send(SessionCommand::Control(
                    zevria_session_api::ControlCommand::Shutdown,
                ))
                .unwrap();
            // On this current-thread executor, channel closure is observed only
            // after the engine future and its lease have finished dropping.
            commands.closed().await;
            if let Some(observer) = observer {
                observer.await.unwrap();
            }
            assert_eq!(lease.strong_count(), 0);
            assert_session_lease_reclaimed(&path).await;
            assert!(!sidecar.exists());
            assert!(
                !path.exists(),
                "a leased but unused worker never creates history"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_signals_engine_before_awaiting_supervisor_and_removes_empty_transcript() {
        let directory = tempfile::tempdir().expect("session directory");
        let transcript_path = directory.path().join("empty.jsonl");
        std::fs::File::create(&transcript_path).expect("empty transcript");
        let lease =
            Arc::new(crate::session_lease::RootSessionLease::acquire(&transcript_path).unwrap());
        let sidecar = transcript_path.with_extension("jsonl.lock");
        let engine_lease = lease.clone();
        let (command_tx, mut command_rx) = unbounded_channel();
        let (event_tx, event_rx) = session_event_channel(4);
        let (_exit_tx, exit_rx) = unbounded_channel();
        let order = Arc::new(Mutex::new(Vec::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let engine_order = Arc::clone(&order);
        let engine_notify = Arc::clone(&notify);
        let engine_task = tokio::spawn(async move {
            let _lease = engine_lease;
            while let Some(command) = command_rx.recv().await {
                if command == SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown)
                {
                    engine_order
                        .lock()
                        .expect("shutdown order lock poisoned")
                        .push("engine");
                    engine_notify.notify_one();
                    break;
                }
            }
        });
        let supervisor_order = Arc::clone(&order);
        let supervisor_task = tokio::spawn(async move {
            notify.notified().await;
            supervisor_order
                .lock()
                .expect("shutdown order lock poisoned")
                .push("supervisor");
        });
        let skills = Arc::new(
            SkillCatalog::new(Vec::<SkillDefinition>::new()).expect("empty skill registry"),
        );
        let jobs = CommandJobs::new(
            command_job_limits(CommandConfig::default()),
            Some(event_tx.clone()),
        );
        use rig_agent::tool::Tool as _;
        CommandTool::with_jobs(directory.path().into(), jobs.clone())
            .call(
                &mut rig_agent::tool::ToolContext::new(),
                zevria_tools::CommandArgs {
                    command: "echo $$ > job-pid; sleep 0.5; printf leaked > leaked".into(),
                    background: Some(true),
                },
            )
            .await
            .unwrap();
        // Shell redirection creates the file before echo writes the PID. Do
        // not kill the job in that interval and mistake an empty file for a
        // completed readiness handshake.
        let _pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(directory.path().join("job-pid")) {
                    Ok(text) if text.ends_with('\n') => {
                        let pid = text
                            .trim()
                            .parse::<i32>()
                            .expect("valid background job PID");
                        assert!(pid > 0, "background job PID must be positive");
                        break pid;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("cannot read background job PID: {error}"),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background job must publish its PID before shutdown");
        let running = RunningSession {
            command_jobs: Some(jobs),
            cleanup_abandoned_history: true,
            restored_taken: false,
            restored_subtasks: Vec::new(),
            restored_agent_runs: Vec::new(),
            _lease: Some(lease),
            restoration: SessionRestoration {
                model_contexts: crate::config::test_config().routing().context_policies(),
                reasoning_levels: [zevria_foundation::ReasoningLevel::Medium; ModelRole::COUNT],
                model_snapshots: Vec::new(),
                sessions_dir: directory.path().to_path_buf(),
                session_id: "empty".to_string(),
                subsessions_dir: directory.path().join("subsessions"),
                agent_runs_root: directory.path().join("agent-runs"),
                transcript_path: transcript_path.clone(),
                transcript_items: Vec::new(),
                skill_catalog: skills.clone(),
                active_skills: Default::default(),
                startup_notices: Vec::new(),
                plan_state: PlanWorkflowState::Idle,
                selected_mode: SessionMode::Build,
            },
            command_tx: Some(command_tx),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
            exit_rx: Some(exit_rx),
            engine_task: Some(engine_task),
            supervisor_task: Some(supervisor_task),
        };

        running.shutdown().await.expect("clean shutdown");
        assert_eq!(
            order
                .lock()
                .expect("shutdown order lock poisoned")
                .as_slice(),
            ["engine", "supervisor"]
        );
        assert!(!transcript_path.exists());
        assert_session_lease_reclaimed(&transcript_path).await;
        assert!(!sidecar.exists());
        tokio::time::sleep(std::time::Duration::from_millis(650)).await;
        assert!(
            !directory.path().join("leaked").exists(),
            "shutdown must kill background commands"
        );
        #[cfg(unix)]
        {
            // SAFETY: signal zero only probes whether this recorded child remains.
            assert_eq!(unsafe { libc::kill(_pid, 0) }, -1);
        }
    }

    #[tokio::test]
    async fn shutdown_propagates_background_task_failure() {
        let directory = tempfile::tempdir().expect("session directory");
        let transcript_path = directory.path().join("nonempty.jsonl");
        std::fs::write(&transcript_path, b"record\n").expect("transcript");
        let (command_tx, _command_rx) = unbounded_channel();
        let (event_tx, event_rx) = session_event_channel(4);
        let (_exit_tx, exit_rx) = unbounded_channel();
        let engine_task = tokio::spawn(async move {
            panic!("scripted engine failure");
        });
        let supervisor_task = tokio::spawn(async {});
        let skills = Arc::new(
            SkillCatalog::new(Vec::<SkillDefinition>::new()).expect("empty skill registry"),
        );
        let running = RunningSession {
            command_jobs: None,
            cleanup_abandoned_history: true,
            restored_taken: false,
            restored_subtasks: Vec::new(),
            restored_agent_runs: Vec::new(),
            _lease: None,
            restoration: SessionRestoration {
                model_contexts: crate::config::test_config().routing().context_policies(),
                reasoning_levels: [zevria_foundation::ReasoningLevel::Medium; ModelRole::COUNT],
                model_snapshots: Vec::new(),
                sessions_dir: directory.path().to_path_buf(),
                session_id: "failed".to_string(),
                subsessions_dir: directory.path().join("subsessions"),
                agent_runs_root: directory.path().join("agent-runs"),
                transcript_path,
                transcript_items: Vec::new(),
                skill_catalog: skills.clone(),
                active_skills: Default::default(),
                startup_notices: Vec::new(),
                plan_state: PlanWorkflowState::Idle,
                selected_mode: SessionMode::Build,
            },
            command_tx: Some(command_tx),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
            exit_rx: Some(exit_rx),
            engine_task: Some(engine_task),
            supervisor_task: Some(supervisor_task),
        };

        let error = running
            .shutdown()
            .await
            .expect_err("engine panic must propagate through cleanup");
        assert!(error.to_string().contains("session engine task failed"));
    }
}
