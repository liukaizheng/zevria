//! Frontend-neutral ownership of one Zevria session and its pending or saved transcript.

use std::path::{Path, PathBuf};

use zevria_acp::ExecutionProfile;
use zevria_foundation::{ModelRole, SessionMode};
use zevria_instructions::{FixedSkillRoots, skill::discover_skills};
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::{PlanHandoff, PlanWorkflowState};

use crate::config::Config;

mod running;
mod storage;
pub(crate) mod tools;

use running::{MCP_SHUTDOWN_GRACE, SubtaskSupervisor, spawn_observed};
pub use running::{RunningSession, RuntimeTaskExit};
pub use storage::sessions_dir;
pub(crate) use storage::{canonical_workspace, transcript_recovery_notice};
use storage::{ensure_worker_ignore_guard, prepare_session_lease};
#[cfg(test)]
pub(crate) use tools::command_tool;
use tools::worker_tools;
pub(crate) use tools::{
    build_explore_tools, build_isolated_build_tools, build_session_policies, build_tools,
    command_job_limits, with_web_search_policy, worker_policies,
};

#[cfg(test)]
use crate::config::CommandConfig;
#[cfg(test)]
use tokio::sync::mpsc::unbounded_channel;
#[cfg(test)]
use zevria_foundation::{READ_IMAGE_TOOL_NAME, TurnPolicy, WorkspaceContract};
#[cfg(test)]
use zevria_instructions::{
    SkillCatalog,
    prompts::{
        BUILD_MODE_INSTRUCTIONS, ENSEMBLE_WORKER_PLAN_INSTRUCTIONS,
        ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS, PLAN_MODE_INSTRUCTIONS,
    },
};
#[cfg(test)]
use zevria_session_api::{
    CommandJobs, SessionCommand, TurnEvent, question_channels, session_event_channel,
};
#[cfg(test)]
use zevria_tools::CommandTool;
#[cfg(test)]
use zevria_transcript::transcript;

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

pub async fn start_session_with_profile(
    config: &Config,
    workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
) -> anyhow::Result<StartedSession> {
    start_session_with_mcp(config, workspace, start, profile, Vec::new()).await
}

pub async fn start_session_with_mcp(
    config: &Config,
    workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
    mcp_servers: Vec<zevria_foundation::AcpMcpServerSpec>,
) -> anyhow::Result<StartedSession> {
    startup::start(config, workspace, start, profile, mcp_servers, true, false).await
}

pub(crate) async fn start_acp_session(
    config: &Config,
    canonical_workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
    mcp_servers: Vec<zevria_foundation::AcpMcpServerSpec>,
) -> anyhow::Result<StartedSession> {
    startup::start(
        config,
        canonical_workspace,
        start,
        profile,
        mcp_servers,
        false,
        true,
    )
    .await
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

pub struct StartedSession {
    pub running: RunningSession,
    pub restored: RestoredSession,
}

#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub session_id: String,
    pub transcript_path: PathBuf,
}

/// One-time, host-loaded frontend restoration payload. Frontends never reopen
/// transcript files; the runtime retains paths and lease metadata for shutdown.
pub struct RestoredSession {
    pub side_chats: Vec<crate::RestoredSideChat>,
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
    /// Validated startup catalog, pins, and effective eligibility in the selected mode.
    pub skill_context: zevria_instructions::SkillContext,
    pub startup_notices: Vec<String>,
    pub mcp_prompts: Vec<(
        zevria_foundation::McpServerName,
        zevria_foundation::McpPromptInfo,
    )>,
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

/// Start one provider connection, possibly pending transcript writer, engine, ensemble
/// supervisor, question broker, and subtask supervisor without constructing a
/// terminal frontend.
pub async fn start_session(
    app_config: &Config,
    workspace: &Path,
    start: SessionStart,
) -> anyhow::Result<StartedSession> {
    start_session_with_profile(app_config, workspace, start, ExecutionProfile::Interactive).await
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
                for tool in [
                    "command",
                    "web_search",
                    READ_IMAGE_TOOL_NAME,
                    "question",
                    "submit_plan",
                ] {
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
                assert!(
                    policies
                        .policy(SessionMode::Build)
                        .allows_tool(READ_IMAGE_TOOL_NAME)
                );
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
            None,
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
                READ_IMAGE_TOOL_NAME,
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
        let path = worker.restored.transcript_path.clone();
        assert!(!path.exists());
        assert_eq!(worker.restored.plan_state, PlanWorkflowState::Idle);
        assert_eq!(worker.restored.selected_mode, SessionMode::Build);
        assert!(worker.running.supervisor.is_none());
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
        worker.running.shutdown().await.unwrap();
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

    #[tokio::test]
    async fn resume_rejects_a_non_file_before_creating_the_worker_guard_or_lease() {
        let workspace = tempfile::tempdir().unwrap();
        let canonical = canonical_workspace(workspace.path()).unwrap();
        let profile = ExecutionProfile::EnsembleWorker;
        let directory = sessions_dir(&canonical, profile);
        let path = directory.join("not-a-file.jsonl");
        std::fs::create_dir_all(&path).unwrap();
        let Err(error) = start_session_with_profile(
            &crate::config::test_config(),
            &canonical,
            SessionStart::Resume(path.clone()),
            profile,
        )
        .await
        else {
            panic!("startup accepted a directory as a transcript");
        };
        assert!(
            error.to_string().contains("is not a regular file"),
            "{error:#}"
        );
        assert!(!directory.join(".gitignore").exists());
        assert!(!directory.join(".leases.lock").exists());
        assert!(!path.with_extension("jsonl.lock").exists());
    }

    #[tokio::test]
    async fn invalid_local_startup_does_not_open_mcp_connections() {
        let workspace = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = crate::config::test_config();
        config.ensemble.max_concurrent_agents = 0;
        config.mcp = serde_json::from_value(serde_json::json!({
            "servers": { "fixture": { "url": format!("http://{}/mcp", listener.local_addr().unwrap()) } },
        })).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            start_session(
                &config,
                workspace.path(),
                SessionStart::New {
                    inherited_models: None,
                },
            ),
        )
        .await
        .expect("local validation must not wait for the MCP handshake");
        let Err(error) = result else {
            panic!("invalid ensemble config was accepted");
        };
        assert!(
            error.to_string().contains("ensemble.max_concurrent_agents"),
            "{error:#}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn engine_fatal_exit_precedes_dependent_supervisor_channel_exhaustion() {
        for _ in 0..20 {
            let (requests, mut receive) = unbounded_channel::<()>();
            let keepalive = requests.clone();
            let (exits, mut observed) = unbounded_channel();
            let shutdown = tokio_util::sync::CancellationToken::new();
            let engine = spawn_observed(
                "engine",
                async move {
                    drop(requests);
                    Err(zevria_transcript::SessionReplayError::Plan(
                        "original failure".into(),
                    ))
                },
                exits.clone(),
                (keepalive, shutdown.clone().drop_guard()),
            );
            let supervisor = spawn_observed(
                "supervisor",
                async move {
                    shutdown.cancelled().await;
                    assert!(receive.recv().await.is_none());
                },
                exits,
                (),
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
            let observer = spawn_observed("session engine", async { Err(error) }, exits, lease);
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
            let path = worker.restored.transcript_path.clone();
            assert!(!path.exists());
            let bytes = if models_only {
                let mut bytes = serde_json::to_vec(&worker.restored.transcript_items[0]).unwrap();
                bytes.push(b'\n');
                bytes
            } else {
                Vec::new()
            };
            std::fs::write(&path, &bytes).unwrap();
            assert!(transcript::is_abandoned_root(&path));
            let mut events = worker.running.take_event_receiver().unwrap();
            worker
                .running
                .command_sender()
                .send(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        text: "must not be accepted".into(),
                        mode: SessionMode::Build,
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        references: Vec::new(),
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
                        zevria_session_api::SessionEvent::Turn(TurnEvent::Rejected { .. })
                    )
                ),
                "{event:?}"
            );
            worker.running.shutdown().await.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_session_lease_reclaimed(&path).await;
            assert!(!path.with_extension("jsonl.lock").exists());
        }
    }

    #[tokio::test]
    async fn engine_lease_survives_frontend_drop_but_not_engine_abort() {
        for abort_engine in [false, true] {
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
            let path = worker.restored.transcript_path.clone();
            let sidecar = path.with_extension("jsonl.lock");
            let commands = worker.running.command_sender();
            let _events = worker.running.take_event_receiver().unwrap();
            let lease = Arc::downgrade(worker.running._lease.as_ref().unwrap());
            let task = worker.running.engine_task.take().unwrap();
            drop(worker);
            assert_eq!(lease.strong_count(), 1, "only the engine retains ownership");
            assert!(sidecar.exists());
            assert!(
                prepare_session_lease(&path).await.is_err(),
                "startup sweep must preserve the engine"
            );
            if abort_engine {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                commands
                    .send(SessionCommand::Control(
                        zevria_session_api::ControlCommand::Shutdown,
                    ))
                    .unwrap();
                task.await.unwrap();
            }
            assert!(commands.is_closed());
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
            side_chats: None,
            mcp: None,
            command_jobs: Some(jobs),
            cleanup_abandoned_history: true,
            _lease: Some(lease),
            identity: SessionIdentity {
                session_id: "empty".to_string(),
                transcript_path: transcript_path.clone(),
            },
            command_tx: Some(command_tx),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
            exit_rx: Some(exit_rx),
            engine_task: Some(engine_task),
            supervisor: Some(SubtaskSupervisor {
                shutdown: tokio_util::sync::CancellationToken::new(),
                task: supervisor_task,
            }),
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
        let running = RunningSession {
            side_chats: None,
            mcp: None,
            command_jobs: None,
            cleanup_abandoned_history: true,
            _lease: None,
            identity: SessionIdentity {
                session_id: "failed".to_string(),
                transcript_path: transcript_path,
            },
            command_tx: Some(command_tx),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
            exit_rx: Some(exit_rx),
            engine_task: Some(engine_task),
            supervisor: Some(SubtaskSupervisor {
                shutdown: tokio_util::sync::CancellationToken::new(),
                task: supervisor_task,
            }),
        };

        let error = running
            .shutdown()
            .await
            .expect_err("engine panic must propagate through cleanup");
        assert!(error.to_string().contains("session engine task failed"));
    }

    #[tokio::test]
    async fn shutdown_with_mcp_exits_promptly_without_waiting_for_supervisor_grace() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = crate::config::test_config();
        config.mcp.enabled = true;
        config.mcp.servers.insert(
            "disabled".parse().unwrap(),
            zevria_mcp::config::McpServerConfig {
                enabled: false,
                command: Some("disabled-mcp".into()),
                ..Default::default()
            },
        );
        let running = start_session(
            &config,
            workspace.path(),
            SessionStart::New {
                inherited_models: None,
            },
        )
        .await
        .unwrap();
        assert!(running.running.mcp.is_some());
        assert!(running.running.supervisor.is_some());
        let path = running.restored.transcript_path.clone();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            running.running.shutdown(),
        )
        .await
        .expect("shutdown with active McpManager must not stall for supervisor grace")
        .expect("clean shutdown");
        assert_session_lease_reclaimed(&path).await;
    }
}
