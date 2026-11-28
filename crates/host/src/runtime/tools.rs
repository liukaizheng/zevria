//! Ordered tool registries and mode-specific access policies.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rig_agent::tool::server::{ToolServer, ToolServerHandle};
use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, FileMutationScope, LAUNCH_SUBTASKS_TOOL_NAME, ModelRole,
    QUESTION_TOOL_NAME, READ_IMAGE_TOOL_NAME, READ_PLAN_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME,
    SessionMode, SessionPolicies, TASK_TOOL_NAME, TurnPolicy, UPDATE_PLAN_TOOL_NAME,
    WEB_SEARCH_TOOL_NAME, WorkspaceContract,
};
use zevria_instructions::prompts::{
    BUILD_MODE_INSTRUCTIONS, ENSEMBLE_WORKER_PLAN_INSTRUCTIONS,
    ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS, PLAN_MODE_INSTRUCTIONS,
};
use zevria_session_api::{
    CommandJobLimits, CommandJobs, QuestionRequester, SubtaskLauncher, WebSearchExecutor,
};
use zevria_tools::{
    CommandLimits, CommandStatusTool, CommandTerminateTool, CommandTool, CreatePlanTool,
    DeleteTool, EditTool, LaunchSubtasksTool, QuestionTool, ReadImageTool, ReadPlanTool,
    ReconcileReportsTool, SkillReadTool, SkillTool, SubmitPlanTool, TaskTool, UpdatePlanTool,
    WebSearchTool, WriteTool,
};

use crate::config::{CommandConfig, Config};

const ROOT_BUILD_FILE_MUTATION_SCOPE: FileMutationScope = FileMutationScope::ProcessFilesystem;

/// Build the root production registry in its stable advertised order.
pub(crate) fn build_tools(
    workspace: &Path,
    launcher: SubtaskLauncher,
    questions: QuestionRequester,
    command_config: CommandConfig,
    max_plan_artifact_bytes: usize,
    jobs: Option<CommandJobs>,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
) -> anyhow::Result<ToolServerHandle> {
    let workspace = workspace.to_path_buf();
    let server = if let Some(jobs) = jobs {
        ToolServer::new()
            .tool(CommandTool::with_jobs(workspace.clone(), jobs.clone()))
            .tool(
                CommandStatusTool::new(jobs.clone()).with_wait_timeout(
                    command_config
                        .status_wait_timeout_seconds
                        .map(std::time::Duration::from_secs),
                ),
            )
            .tool(CommandTerminateTool::new(jobs))
    } else {
        ToolServer::new().tool(command_tool(workspace.clone(), command_config)?)
    };
    let server = server
        .tool(TaskTool)
        .tool(LaunchSubtasksTool::new(launcher, workspace.clone()))
        .tool(
            EditTool::new(workspace.clone())
                .with_file_mutation_scope(ROOT_BUILD_FILE_MUTATION_SCOPE),
        )
        .tool(
            WriteTool::new(workspace.clone())
                .with_file_mutation_scope(ROOT_BUILD_FILE_MUTATION_SCOPE),
        )
        .tool(
            DeleteTool::new(workspace.clone())
                .with_file_mutation_scope(ROOT_BUILD_FILE_MUTATION_SCOPE),
        )
        .tool(SkillTool)
        .tool(SkillReadTool);
    let server = server
        .tool(ReconcileReportsTool)
        .tool(QuestionTool::new(questions))
        .tool(CreatePlanTool::new(max_plan_artifact_bytes))
        .tool(UpdatePlanTool::new(max_plan_artifact_bytes))
        .tool(ReadPlanTool)
        .tool(SubmitPlanTool::new(max_plan_artifact_bytes))
        // Append new root declarations so existing cacheable bytes keep their order.
        .tool(ReadImageTool::new(workspace));
    Ok(append_web_search(server, web_search).run())
}

pub(crate) fn build_side_chat_tools(
    workspace: &Path,
    config: CommandConfig,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
) -> anyhow::Result<ToolServerHandle> {
    // Use private foreground-only jobs, never the parent's session registry.
    let server = ToolServer::new()
        .tool(command_tool(workspace.into(), config)?)
        .tool(ReadImageTool::new(workspace.into()))
        .tool(SkillReadTool);
    Ok(append_web_search(server, web_search).run())
}

fn append_web_search(
    server: ToolServer,
    executor: Option<Arc<dyn WebSearchExecutor>>,
) -> ToolServer {
    match executor {
        Some(executor) => server.tool(WebSearchTool::new(executor)),
        None => server,
    }
}

/// Keep the workflow declaration consistent with the static search registry.
pub(crate) fn with_web_search_policy(
    mut policies: SessionPolicies,
    enabled: bool,
) -> SessionPolicies {
    if !enabled {
        for mode in [SessionMode::Build, SessionMode::Plan] {
            if let Some(names) = &mut policies.policy_mut(mode).allowed_tool_names {
                names.retain(|name| name != WEB_SEARCH_TOOL_NAME);
            }
        }
    }
    policies
}

/// Explore children register command and, when enabled, web search. Structured
/// mutation, skills, planning completion, and nested delegation cannot be
/// reached even if a policy check is bypassed; source-read-only investigation
/// with scratch-contained execution is behavioral, not a command sandbox.
pub(crate) fn build_explore_tools(
    workspace: &Path,
    command_config: CommandConfig,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
) -> anyhow::Result<ToolServerHandle> {
    let server = ToolServer::new().tool(command_tool(workspace.to_path_buf(), command_config)?);
    Ok(append_web_search(server, web_search).run())
}

/// Private Build-child registry. Both provider and engine receive this same
/// handle; no privileged root tools are physically registered.
pub(crate) fn build_isolated_build_tools(
    child_workspace: &Path,
    command_config: CommandConfig,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
) -> anyhow::Result<ToolServerHandle> {
    let workspace = child_workspace.to_path_buf();
    let server = ToolServer::new()
        .tool(command_tool(workspace.clone(), command_config)?)
        .tool(TaskTool)
        .tool(EditTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(DeleteTool::new(workspace));
    Ok(append_web_search(server, web_search).run())
}

pub(crate) fn command_job_limits(config: CommandConfig) -> CommandJobLimits {
    CommandJobLimits {
        max_running_background: config.max_background_jobs,
        retained_finished: zevria_foundation::RETAINED_FINISHED_COMMAND_JOBS,
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
        READ_IMAGE_TOOL_NAME,
        LAUNCH_SUBTASKS_TOOL_NAME,
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    build_tools.extend(zevria_foundation::SKILL_TOOL_NAMES.map(str::to_string));
    let mut plan_tools = vec![
        "command".to_string(),
        zevria_foundation::WEB_SEARCH_TOOL_NAME.into(),
        READ_IMAGE_TOOL_NAME.into(),
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
        .with_orchestration()
        .with_file_mutation_scope(ROOT_BUILD_FILE_MUTATION_SCOPE),
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

pub(super) fn worker_tools(
    workspace: &Path,
    questions: QuestionRequester,
    config: &Config,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
) -> anyhow::Result<ToolServerHandle> {
    let server = ToolServer::new()
        .tool(command_tool(workspace.to_path_buf(), config.command)?)
        .tool(QuestionTool::new(questions))
        .tool(CreatePlanTool::new(config.session.plan.max_artifact_bytes))
        .tool(UpdatePlanTool::new(config.session.plan.max_artifact_bytes))
        .tool(ReadPlanTool)
        .tool(SubmitPlanTool::new(config.session.plan.max_artifact_bytes));
    Ok(append_web_search(server, web_search).run())
}
