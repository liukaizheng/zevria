//! Staged composition shared by interactive roots and restricted native workers.
//! Read/validate every dependency before preparing a writable transcript. Root
//! capabilities and dependent-task lifetime remain explicit profile choices.
use super::{
    MCP_SHUTDOWN_GRACE, RestoredAgentRun, RestoredSession, RestoredSubtask, RunningSession,
    SessionIdentity, SessionStart, StartedSession, SubtaskSupervisor, append_guidance_notices,
    build_explore_tools, build_session_policies, build_tools, canonical_workspace,
    command_job_limits, ensure_worker_ignore_guard, guidance_roots, load_session_skills,
    preflight_restoration, prepare_session_lease, sessions_dir, spawn_observed,
    transcript_recovery_notice, worker_policies, worker_tools,
};
use crate::{
    config::Config,
    ensemble::EnsembleSupervisor,
    session_lease::RootSessionLease,
    session_models::ResolvedSessionModels,
    skills::LocalSkillService,
    subtasks::{SubtaskSupervisorConfig, run_supervisor},
};
use anyhow::Context as _;
use rig_agent::tool::server::ToolServerHandle;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc::{Receiver, unbounded_channel};
use zevria_acp::ExecutionProfile;
use zevria_core::SessionEngine;
use zevria_foundation::{
    AcpMcpServerSpec, McpManagementRequest, McpManagementResult, McpToolAccess, ModelRole,
    SessionMode, SessionPolicies,
};
use zevria_instructions::{SkillCatalog, SkillContext};
use zevria_mcp::{McpConfigSource, McpManager};
use zevria_provider::{ProviderRouter, ProviderRouterFactory};
use zevria_session_api::mcp::McpSessionService;
use zevria_session_api::{
    CommandJobs, QuestionRequester, QuestionResponder, SessionCommand, SessionEventReceiver,
    SessionEventSender, SubtaskLaunchRequest, SubtaskLauncher, question_channels,
    session_event_channel, subtask_channels_with_capacity,
};
use zevria_transcript::{
    agent_runs_dir,
    transcript::{self, TranscriptItem, TranscriptLoadOutcome, TranscriptWriter},
};

struct ResolvedStartup<'a> {
    config: &'a Config,
    refreshed_models: Option<Config>,
    workspace: PathBuf,
    profile: ExecutionProfile,
    start: SessionStart,
    directory: PathBuf,
    id: String,
    path: PathBuf,
}
impl ResolvedStartup<'_> {
    fn model_config(&self) -> &Config {
        self.refreshed_models.as_ref().unwrap_or(self.config)
    }
}

struct OpenedStorage<'a> {
    resolved: ResolvedStartup<'a>,
    lease: Arc<RootSessionLease>,
    loaded: Option<TranscriptLoadOutcome>,
    models: ResolvedSessionModels,
    initial_items: Vec<TranscriptItem>,
    restored_subtasks: Vec<RestoredSubtask>,
    restored_agent_runs: Vec<RestoredAgentRun>,
}
impl OpenedStorage<'_> {
    fn original_items(&self) -> &[TranscriptItem] {
        self.loaded
            .as_ref()
            .map_or(self.initial_items.as_slice(), TranscriptLoadOutcome::items)
    }
}

struct RootServices {
    requests: Receiver<SubtaskLaunchRequest>,
    keepalive: SubtaskLauncher,
    ensemble_questions: QuestionRequester,
    skill_service: Arc<LocalSkillService>,
}
struct SessionServices {
    provider: ProviderRouter,
    tools: ToolServerHandle,
    policies: SessionPolicies,
    skill_catalog: Arc<SkillCatalog>,
    event_tx: SessionEventSender,
    event_rx: SessionEventReceiver,
    responder: QuestionResponder,
    command_jobs: Option<CommandJobs>,
    startup_notices: Vec<String>,
    mcp: Option<McpStartup>,
    root: Option<RootServices>,
}
struct McpStartup {
    source: McpConfigSource,
    delegated: Vec<AcpMcpServerSpec>,
    tools: ToolServerHandle,
}
struct RootTasks {
    requests: Receiver<SubtaskLaunchRequest>,
    keepalive: SubtaskLauncher,
}
struct BuiltEngine {
    engine: SessionEngine<ProviderRouter>,
    skill_context: SkillContext,
    event_tx: SessionEventSender,
    event_rx: SessionEventReceiver,
    command_jobs: Option<CommandJobs>,
    startup_notices: Vec<String>,
    mcp: Option<Arc<McpManager>>,
    model_snapshots: Vec<zevria_model::ContextTokenSnapshot>,
    root: Option<(RootTasks, Arc<SubtaskSupervisorConfig>)>,
}

pub(super) async fn start(
    config: &Config,
    workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
    mcp_servers: Vec<AcpMcpServerSpec>,
    include_children: bool,
    workspace_is_canonical: bool,
) -> anyhow::Result<StartedSession> {
    anyhow::ensure!(
        !profile.is_worker() || mcp_servers.is_empty(),
        "ensemble workers cannot connect to delegated MCP servers"
    );
    let resolved = resolve(config, workspace, start, profile, workspace_is_canonical).await?;
    let mut storage = open_storage(resolved, include_children).await?;
    let services = build_services(&storage, mcp_servers).await?;
    let engine = build_engine(&mut storage, services).await?;
    spawn(storage, engine)
}

async fn resolve<'a>(
    config: &'a Config,
    workspace: &Path,
    start: SessionStart,
    profile: ExecutionProfile,
    workspace_is_canonical: bool,
) -> anyhow::Result<ResolvedStartup<'a>> {
    if profile.is_worker() {
        match &start {
            SessionStart::New {
                inherited_models: Some(_),
            } => anyhow::bail!(
                "model inheritance is only supported for interactive root replacements"
            ),
            SessionStart::FromPlan { .. } => {
                anyhow::bail!("ensemble workers cannot implement a Plan handoff")
            }
            _ => {}
        }
    }
    zevria_foundation::shell::prepare_native().await?;
    zevria_foundation::shell::prepare_environment(config.command.shell_environment).await;
    let workspace = workspace.to_path_buf();
    let config_paths = config.source_path.clone().map(|path| {
        let models = config
            .models_path
            .clone()
            .unwrap_or_else(|| zevria_foundation::config::models_path_for(&path));
        (path, models)
    });
    let resume = match &start {
        SessionStart::Resume(path) => Some(path.clone()),
        _ => None,
    };
    let (workspace, directory, refreshed_models) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let workspace = if workspace_is_canonical {
                workspace
            } else {
                canonical_workspace(&workspace)?
            };
            let directory = sessions_dir(&workspace, profile);
            if let Some(path) = resume {
                if profile.is_worker() {
                    anyhow::ensure!(
                        path.parent() == Some(directory.as_path()),
                        "worker transcript must be in the ensemble-sessions namespace"
                    );
                }
                let metadata = std::fs::metadata(&path).with_context(|| {
                    format!("failed to inspect session transcript at {}", path.display())
                })?;
                anyhow::ensure!(
                    metadata.is_file(),
                    "session transcript at {} is not a regular file",
                    path.display()
                );
            }
            if profile.is_worker() {
                ensure_worker_ignore_guard(&directory)?;
            }
            let models = config_paths
                .map(|(path, models)| Config::load_existing(&path, &models))
                .transpose()?;
            Ok((workspace, directory, models))
        })
        .await
        .context("startup configuration and workspace task failed")??;
    let (id, path) = match &start {
        SessionStart::Resume(path) => {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .with_context(|| {
                    if profile.is_worker() {
                        "unreadable worker session ID".into()
                    } else {
                        format!("the session file {} has no readable name", path.display())
                    }
                })?
                .to_string();
            (id, path.clone())
        }
        SessionStart::New { .. } | SessionStart::FromPlan { .. } => {
            let id = transcript::pick_session_id();
            let path = directory.join(format!("{id}.jsonl"));
            (id, path)
        }
    };
    Ok(ResolvedStartup {
        config,
        refreshed_models,
        workspace,
        profile,
        start,
        directory,
        id,
        path,
    })
}

async fn open_storage(
    resolved: ResolvedStartup<'_>,
    include_children: bool,
) -> anyhow::Result<OpenedStorage<'_>> {
    let lease = prepare_session_lease(&resolved.path).await?;
    let loaded = if matches!(resolved.start, SessionStart::Resume(_)) {
        let path = resolved.path.clone();
        Some(
            tokio::task::spawn_blocking(move || transcript::load_report(&path))
                .await
                .context(if resolved.profile.is_worker() {
                    "worker transcript loader failed"
                } else {
                    "transcript loader failed"
                })??,
        )
    } else {
        None
    };
    let inherited = match &resolved.start {
        SessionStart::New { inherited_models }
        | SessionStart::FromPlan {
            inherited_models, ..
        } => inherited_models.as_ref(),
        SessionStart::Resume(_) => None,
    };
    let models = crate::session_models::resolve(
        resolved.model_config(),
        &resolved.id,
        loaded.as_ref(),
        inherited,
    )?;
    let (restored_subtasks, restored_agent_runs) =
        if include_children && !resolved.profile.is_worker() {
            match &loaded {
                Some(outcome) => {
                    preflight_restoration(
                        &transcript::subsessions_dir(&resolved.workspace, &resolved.id),
                        &agent_runs_dir(&resolved.workspace, &resolved.id),
                        outcome.items(),
                    )
                    .await?
                }
                None => (Vec::new(), Vec::new()),
            }
        } else {
            (Vec::new(), Vec::new())
        };
    let mut initial_items = Vec::new();
    if loaded.is_none() {
        initial_items.push(TranscriptItem::SessionModels(Box::new(
            models.selections.clone(),
        )));
        if !resolved.profile.is_worker() {
            initial_items.push(TranscriptItem::SessionMode(SessionMode::Build));
        }
    }
    Ok(OpenedStorage {
        resolved,
        lease,
        loaded,
        models,
        initial_items,
        restored_subtasks,
        restored_agent_runs,
    })
}

async fn build_services(
    storage: &OpenedStorage<'_>,
    mcp_servers: Vec<AcpMcpServerSpec>,
) -> anyhow::Result<SessionServices> {
    let resolved = &storage.resolved;
    let config = resolved.config;
    let (event_tx, event_rx) = session_event_channel(config.session.event_queue_capacity);
    let questions = question_channels(event_tx.clone());
    if resolved.profile.is_worker() {
        // Restricted workers never discover/manage skills, mutate models,
        // launch subtasks/ensembles, or project Markdown Plans.
        let skill_catalog = Arc::new(SkillCatalog::default());
        let tools = worker_tools(&resolved.workspace, questions.requester, config)?;
        let provider = ProviderRouter::root(
            &storage.models.routing,
            &resolved.model_config().session.preamble,
            &resolved.id,
        )?;
        return Ok(SessionServices {
            provider,
            tools,
            policies: worker_policies(),
            skill_catalog,
            event_tx,
            event_rx,
            responder: questions.responder,
            command_jobs: None,
            startup_notices: Vec::new(),
            mcp: None,
            root: None,
        });
    }
    let channels = subtask_channels_with_capacity(
        resolved.id.clone(),
        event_tx.clone(),
        config.session.max_concurrent_subtasks,
    );
    let ensemble_questions = questions.requester.clone();
    let skill_workspace = resolved.workspace.clone();
    let skill_config_path = match &config.source_path {
        Some(path) => path.clone(),
        None => zevria_foundation::config::config_path()?,
    };
    let settings = resolved.model_config().skills.clone();
    let skill_catalog = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let discovery = load_session_skills(&skill_workspace);
        Ok(Arc::new(SkillCatalog::from_discovery(discovery, settings)?))
    })
    .await
    .context("skill catalog loader failed")??;
    let command_jobs = (config.command.max_background_jobs > 0)
        .then(|| CommandJobs::new(command_job_limits(config.command), Some(event_tx.clone())));
    if let Some(jobs) = &command_jobs {
        jobs.reserve_ids(
            storage
                .original_items()
                .iter()
                .filter_map(|item| match item {
                    TranscriptItem::ToolResults { metadata, .. } => Some(metadata),
                    _ => None,
                })
                .flatten()
                .filter_map(|meta| meta.command_job().map(|job| job.id.clone())),
        );
    }
    let mut policies = build_session_policies(config.session.plan, command_jobs.is_some());
    let skill_service = Arc::new(LocalSkillService {
        roots: skill_catalog
            .roots
            .clone()
            .expect("startup discovery captured fixed roots"),
        config_path: skill_config_path,
    });
    let keepalive = channels.launcher.clone();
    let tools = build_tools(
        &resolved.workspace,
        channels.launcher,
        questions.requester,
        config.command,
        config.session.plan.max_artifact_bytes,
        command_jobs.clone(),
    )?;
    let provider = ProviderRouter::root(
        &storage.models.routing,
        &resolved.model_config().session.preamble,
        &resolved.id,
    )?;
    let startup_notices = startup_preflight(storage, &provider);
    let source = McpConfigSource {
        config: resolved.model_config().mcp.clone(),
        revision: resolved.model_config().mcp_revision.clone(),
    };
    let mcp = if source.config.enabled
        && (!source.config.servers.is_empty() || !mcp_servers.is_empty())
    {
        policies.policy_mut(SessionMode::Build).mcp_access = McpToolAccess::AllEnabled;
        policies.policy_mut(SessionMode::Plan).mcp_access = McpToolAccess::PlanScoped;
        Some(McpStartup {
            source,
            delegated: mcp_servers,
            tools: tools.clone(),
        })
    } else {
        None
    };
    Ok(SessionServices {
        provider,
        tools,
        policies,
        skill_catalog,
        event_tx,
        event_rx,
        responder: questions.responder,
        command_jobs,
        startup_notices,
        mcp,
        root: Some(RootServices {
            requests: channels.requests,
            keepalive,
            ensemble_questions,
            skill_service,
        }),
    })
}

fn startup_preflight(storage: &OpenedStorage<'_>, provider: &ProviderRouter) -> Vec<String> {
    let roles = [ModelRole::Build, ModelRole::Plan, ModelRole::Review];
    let assessments = startup_assessments(
        provider,
        storage.original_items(),
        roles
            .iter()
            .map(|role| &storage.models.routing.for_role(*role).profile),
    );
    let mut notices = Vec::new();
    for role in roles {
        let profile = &storage.models.routing.for_role(role).profile;
        let origin = match (&storage.resolved.start, role) {
            (SessionStart::Resume(_), ModelRole::Build | ModelRole::Plan) => "restored selection",
            (
                SessionStart::New {
                    inherited_models: Some(_),
                }
                | SessionStart::FromPlan {
                    inherited_models: Some(_),
                    ..
                },
                ModelRole::Build | ModelRole::Plan,
            ) => "inherited selection",
            _ => "global default",
        };
        match &assessments[profile] {
            Ok(zevria_model::models::ReplayPreflight::Compatible(_)) => {},
            Ok(zevria_model::models::ReplayPreflight::ConversionRequired { sources }) => notices.push(format!("{} {origin} {} cannot read the checkpoint from {}. History remains inspectable. Use /model to select its source or confirm conversion, or start a fresh session. No conversion call was made.", role.name(), profile, sources.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))),
            Err(error) => notices.push(format!("{} replay preflight failed; generation is blocked until the history is repaired: {error}", role.name())),
        }
    }
    notices
}

fn startup_assessments<'a>(
    provider: &impl zevria_session_api::ModelProvider,
    items: &[TranscriptItem],
    profiles: impl IntoIterator<Item = &'a zevria_foundation::ModelProfileRef>,
) -> std::collections::BTreeMap<
    zevria_foundation::ModelProfileRef,
    Result<zevria_model::models::ReplayPreflight, String>,
> {
    let input = transcript::model_input(items);
    let mut assessments = std::collections::BTreeMap::new();
    for profile in profiles {
        assessments.entry(profile.clone()).or_insert_with(|| {
            provider
                .preflight_input(profile, &input)
                .map_err(|error| format!("{error:#}"))
        });
    }
    assessments
}

async fn prepare_writer(
    storage: &mut OpenedStorage<'_>,
) -> anyhow::Result<(TranscriptWriter, Option<TranscriptLoadOutcome>)> {
    match storage.loaded.take() {
        Some(outcome) => tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            Ok((TranscriptWriter::append_loaded(&outcome)?, Some(outcome)))
        })
        .await
        .context(if storage.resolved.profile.is_worker() {
            "worker writer preparation failed"
        } else {
            "transcript writer preparation failed"
        })?,
        None => {
            let directory = storage.resolved.directory.clone();
            let id = storage.resolved.id.clone();
            let writer = tokio::task::spawn_blocking(move || {
                TranscriptWriter::allocate_with_id(&directory, &id)
            })
            .await
            .context("transcript allocation task failed")??;
            Ok((writer, None))
        }
    }
}

async fn build_engine(
    storage: &mut OpenedStorage<'_>,
    services: SessionServices,
) -> anyhow::Result<BuiltEngine> {
    let (writer, loaded) = prepare_writer(storage).await?;
    let resolved = &storage.resolved;
    let config = resolved.config;
    let SessionServices {
        provider,
        tools,
        policies,
        skill_catalog,
        event_tx,
        event_rx,
        responder,
        command_jobs,
        mut startup_notices,
        mcp,
        root,
    } = services;
    #[cfg(feature = "cache-diagnostics")]
    let provider = provider.with_cache_diagnostics(zevria_provider::CacheDiagnosticContext::new(
        writer.path(),
        &resolved.id,
    ));
    startup_notices.extend(transcript_recovery_notice(
        writer.path(),
        writer.recovered_malformed_lines(),
    ));
    if !resolved.profile.is_worker() {
        tracing::info!(target: "zevria::runtime", session_id = writer.session_id(), path = %writer.path().display(), pending = writer.is_pending(), "opened session transcript storage");
    }
    // The ensemble validates after writer preparation, at the same boundary as
    // before staging. It is never constructed for a restricted worker.
    let root = if let Some(root) = root {
        let ensemble_config = config.ensemble.clone();
        let workspace = resolved.workspace.clone();
        let agent_runs_root = agent_runs_dir(&workspace, &resolved.id);
        let ensemble = tokio::task::spawn_blocking(move || {
            EnsembleSupervisor::new(
                ensemble_config,
                &workspace,
                agent_runs_root,
                root.ensemble_questions,
            )
        })
        .await
        .context("ensemble setup task failed")??;
        Some((
            Arc::new(ensemble),
            root.requests,
            root.keepalive,
            root.skill_service,
        ))
    } else {
        None
    };
    let engine = match loaded {
        Some(outcome) => SessionEngine::builder_from_loaded(
            provider,
            tools,
            policies,
            writer,
            skill_catalog,
            outcome,
        )?,
        None => SessionEngine::builder(provider, tools, policies, writer, skill_catalog)?
            .with_transcript_items(std::mem::take(&mut storage.initial_items))?,
    };
    let workspace = resolved.workspace.clone();
    let guidance = tokio::task::spawn_blocking(move || {
        zevria_instructions::load_guidance(&guidance_roots(&workspace))
    })
    .await
    .context("workspace guidance loader failed")?;
    let (engine, root) = match root {
        Some((ensemble, requests, keepalive, skill_service)) => {
            let mut engine = engine
                .with_incremental_tools(config.session.incremental_tools || mcp.is_some())
                .with_mode_management()
                .with_idle_notifications()
                .with_subtask_concurrency(config.session.max_concurrent_subtasks)
                .with_loaded_guidance(guidance)
                .with_skill_management(skill_service, [true, config.session.plan.allow_skills])?
                .with_compaction_policy(storage.models.compaction.clone())
                .with_question_responder(responder)
                .with_ensemble_launcher(ensemble)
                .with_plans_dir(transcript::plans_dir(&resolved.workspace));
            if let Some(jobs) = &command_jobs {
                engine = engine
                    .with_command_jobs(jobs.clone())
                    .with_job_continuations(config.command.max_automatic_continuations);
            }
            if let Some(path) = &config.source_path {
                engine = engine.with_model_management(
                    Arc::new(crate::models::LocalModelSettings {
                        config_path: path.clone(),
                        models_path: resolved
                            .model_config()
                            .models_path
                            .clone()
                            .unwrap_or_else(|| zevria_foundation::config::models_path_for(path)),
                    }),
                    crate::models::revision(resolved.model_config())?,
                );
            }
            (
                engine,
                Some(RootTasks {
                    requests,
                    keepalive,
                }),
            )
        }
        None => (
            engine
                .with_incremental_tools(config.session.incremental_tools)
                .with_loaded_guidance(guidance)
                .with_compaction_policy(storage.models.compaction.clone())
                .with_question_responder(responder),
            None,
        ),
    };
    let mut engine = engine.build()?;
    append_guidance_notices(&mut startup_notices, engine.refresh_application_guidance()?);
    let skill_context = engine.management_skill_context()?;
    if root.is_some() {
        for diagnostic in skill_context.pin_diagnostics() {
            tracing::warn!(target: "zevria::runtime", "{}", diagnostic.message);
        }
    }
    let root = root
        .map(|root| -> anyhow::Result<_> {
            let config = SubtaskSupervisorConfig {
                explore_factory: ProviderRouterFactory::new(
                    ModelRole::Explore,
                    resolved
                        .model_config()
                        .routing()
                        .for_role(ModelRole::Explore)
                        .clone(),
                    resolved
                        .model_config()
                        .routing()
                        .selection_for_role(ModelRole::Explore)
                        .reasoning_level,
                    &resolved.model_config().session.preamble,
                ),
                builder_factory: ProviderRouterFactory::new(
                    ModelRole::Builder,
                    resolved
                        .model_config()
                        .routing()
                        .for_role(ModelRole::Builder)
                        .clone(),
                    resolved
                        .model_config()
                        .routing()
                        .selection_for_role(ModelRole::Builder)
                        .reasoning_level,
                    &resolved.model_config().session.preamble,
                ),
                explore_tools: build_explore_tools(&resolved.workspace, config.command)?,
                startup_workspace: resolved.workspace.clone(),
                empty_skills: Arc::new(SkillCatalog::default()),
                command_config: config.command,
                events: event_tx.clone(),
                subsessions_dir: transcript::subsessions_dir(&resolved.workspace, &resolved.id),
                compaction: storage.models.compaction.clone(),
                max_concurrent_subtasks: config.session.max_concurrent_subtasks,
                incremental_tools: config.session.incremental_tools,
                guidance: engine
                    .guidance_snapshot()
                    .expect("root captured guidance")
                    .clone(),
            };
            Ok((root, Arc::new(config)))
        })
        .transpose()?;
    // Start external processes only after the writer, guidance, ensemble and
    // child workspace have all been prepared. No runtime tasks are spawned
    // until the MCP-dependent restoration snapshot is ready.
    let mcp = if let Some(McpStartup {
        source,
        delegated,
        tools,
    }) = mcp
    {
        let store = config.source_path.clone().map(|path| {
            Arc::new(crate::mcp::LocalMcpConfigStore::new(path))
                as Arc<dyn zevria_mcp::McpConfigPersister>
        });
        let manager =
            McpManager::start(source, delegated, resolved.workspace.clone(), tools, store).await?;
        if let McpManagementResult::View { view } = manager.query_view(&McpManagementRequest::List)
        {
            startup_notices.extend(view.diagnostics);
        }
        engine = engine.with_mcp(manager.clone());
        Some(manager)
    } else {
        None
    };
    let model_snapshots = match engine.restored_model_contexts() {
        Ok(snapshots) => snapshots,
        Err(error) => {
            if let Some(mcp) = &mcp {
                mcp.shutdown(MCP_SHUTDOWN_GRACE).await;
            }
            return Err(error.into());
        }
    };

    Ok(BuiltEngine {
        engine,
        skill_context,
        event_tx,
        event_rx,
        command_jobs,
        startup_notices,
        mcp,
        model_snapshots,
        root,
    })
}

fn spawn(storage: OpenedStorage<'_>, built: BuiltEngine) -> anyhow::Result<StartedSession> {
    let OpenedStorage {
        resolved,
        lease,
        models,
        restored_subtasks,
        restored_agent_runs,
        ..
    } = storage;
    let BuiltEngine {
        engine,
        skill_context,
        event_tx,
        event_rx,
        command_jobs,
        startup_notices,
        mcp,
        model_snapshots,
        root,
    } = built;
    let transcript_items = engine.conversation().items().to_vec();
    let plan_state = engine.plan_state()?.clone();
    let selected_mode = engine.selected_mode();
    let (command_tx, command_rx) = unbounded_channel();
    let cleanup_abandoned_history = matches!(resolved.start, SessionStart::Resume(_));
    if let SessionStart::FromPlan { handoff, .. } = resolved.start {
        // Queue the opening command while we still own the receiver and have
        // not started any background services.
        command_tx
            .send(SessionCommand::Turn(
                zevria_session_api::TurnCommand::StartFromPlan {
                    handoff: Box::new(handoff),
                },
            ))
            .expect("the opening command receiver is still held");
    }

    let (exit_tx, exit_rx) = unbounded_channel();
    let (supervisor, child_guard) = if let Some((root, config)) = root {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = spawn_observed(
            "subtask supervisor",
            run_supervisor(root.requests, config, shutdown.clone()),
            exit_tx.clone(),
            (),
        );
        // An engine exit (including panic/abort) cancels children even when
        // another owner retains a launcher or the original turn token.
        let guard = (shutdown.clone().drop_guard(), root.keepalive);
        (Some(SubtaskSupervisor { shutdown, task }), Some(guard))
    } else {
        (None, None)
    };
    let engine_events = event_tx.clone();
    let engine_task = spawn_observed(
        "session engine",
        engine.run(command_rx, engine_events),
        exit_tx,
        (child_guard, lease.clone()),
    );
    let mcp_prompts = mcp
        .as_ref()
        .map_or_else(Vec::new, |mcp| mcp.prompt_catalog());
    let identity = SessionIdentity {
        session_id: resolved.id.clone(),
        transcript_path: resolved.path.clone(),
    };
    Ok(StartedSession {
        restored: RestoredSession {
            model_contexts: models.routing.context_policies(),
            reasoning_levels: std::array::from_fn(|index| {
                models
                    .routing
                    .selection_for_role(ModelRole::ALL[index])
                    .reasoning_level
            }),
            model_snapshots,
            sessions_dir: resolved.directory,
            session_id: resolved.id.clone(),
            subsessions_dir: transcript::subsessions_dir(&resolved.workspace, &resolved.id),
            agent_runs_root: agent_runs_dir(&resolved.workspace, &resolved.id),
            transcript_path: resolved.path,
            transcript_items,
            skill_context,
            startup_notices,
            mcp_prompts,
            plan_state,
            selected_mode,
            subtasks: restored_subtasks,
            agent_runs: restored_agent_runs,
        },
        running: RunningSession {
            command_jobs,
            mcp,
            cleanup_abandoned_history,
            identity,
            _lease: Some(lease),
            command_tx: Some(command_tx),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
            exit_rx: Some(exit_rx),
            engine_task: Some(engine_task),
            supervisor,
        },
    })
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
