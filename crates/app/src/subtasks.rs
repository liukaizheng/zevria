//! Responses-backed subtask supervision: independent child subsessions.
//!
//! The supervisor accepts queued launch requests and runs each child in its
//! own Tokio task with a fresh provider connection (independent response-ID
//! chain), an independent `SessionEngine` and history, and its own
//! transcript under the root's subsession directory. Every child event is
//! forwarded to the parent event channel tagged with the child's id, and a
//! launch always eventually resolves its request's oneshot with exactly one
//! terminal outcome — including when connecting, transcript creation, or the
//! engine itself fails. An aborted supervisor drops the pending oneshots,
//! which the awaiting `launch_subtasks` calls observe as failures.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rig_agent::tool::server::ToolServerHandle;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc::Receiver};
use tokio::task::{JoinHandle, JoinSet};
use zevria_content::assistant_plain_text;
use zevria_core::SessionEngine;
use zevria_foundation::SessionMode;
use zevria_foundation::SessionPolicies;
use zevria_foundation::subtask::SubtaskDescriptor;
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskOutcome;
use zevria_foundation::subtask::SubtaskStatus;
use zevria_foundation::{TurnPolicy, WorkspaceBinding, WorkspaceContract};
use zevria_instructions::SkillCatalog;
use zevria_instructions::prompts::BUILD_SUBTASK_INSTRUCTIONS;
use zevria_instructions::prompts::EXPLORE_AGENT_INSTRUCTIONS;
use zevria_model::config::CompactionPolicy;
use zevria_provider::ResponsesRouterFactory;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionEventSender;
use zevria_session_api::SessionUpdate;
use zevria_session_api::session_event_channel;
use zevria_session_api::subtask::ChildWorkspace;
use zevria_session_api::subtask::SubtaskLaunchRequest;
use zevria_transcript::transcript::TranscriptWriter;

#[cfg(test)]
use zevria_foundation::ModelRole;

/// Everything one child run needs, cloned per worker.
#[derive(Clone)]
pub(crate) struct SubtaskSupervisorConfig {
    /// Session-opening assignments, independent of root model switching.
    pub explore_factory: ResponsesRouterFactory,
    pub builder_factory: ResponsesRouterFactory,
    pub explore_tools: ToolServerHandle,
    pub startup_workspace: PathBuf,
    pub command_config: crate::config::CommandConfig,
    /// The parent/frontend event channel receiving tagged child events.
    pub events: SessionEventSender,
    /// `{workspace}/.zevria/subsessions/{root_session_id}`.
    pub subsessions_dir: PathBuf,
    pub compaction: CompactionPolicy,
    pub max_concurrent_subtasks: usize,
    /// The parent's opening/resume snapshot, even for late or queued children.
    pub guidance: zevria_instructions::GuidanceSnapshot,
}

/// The composed Explore inspection/scratch overlay with a command-only local
/// allow-list, identical in every nominal mode so switching cannot add tools.
fn explore_policies() -> SessionPolicies {
    let policy = TurnPolicy::new(
        EXPLORE_AGENT_INSTRUCTIONS,
        Some(vec![
            "command".to_string(),
            zevria_foundation::WEB_SEARCH_TOOL_NAME.into(),
        ]),
        SubtaskKind::Explore.model_role(),
        false,
    )
    .with_contract(WorkspaceContract::SourceReadOnlyScratch);
    SessionPolicies::new(policy.clone(), policy)
}

fn build_policies(child_workspace: &Path, startup_workspace: &Path) -> SessionPolicies {
    let policy = TurnPolicy::new(
        BUILD_SUBTASK_INSTRUCTIONS,
        Some(
            [
                "command",
                "task",
                "edit",
                "write",
                "delete",
                zevria_foundation::WEB_SEARCH_TOOL_NAME,
            ]
            .map(str::to_string)
            .to_vec(),
        ),
        SubtaskKind::Build.model_role(),
        false,
    )
    .with_workspace(WorkspaceBinding {
        root: child_workspace.to_string_lossy().into_owned(),
        startup: startup_workspace.to_string_lossy().into_owned(),
    });
    SessionPolicies::new(policy.clone(), policy)
}

/// Spawn the supervisor task. Children overlap freely: each request gets its
/// own worker. Aborting the returned handle cancels the accept loop and, by
/// dropping its `JoinSet`, every still-running child worker (whose dropped
/// oneshots fail the awaiting tool calls).
pub(crate) fn spawn_supervisor(
    mut requests: Receiver<SubtaskLaunchRequest>,
    config: SubtaskSupervisorConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut workers = JoinSet::new();
        let permits = Arc::new(Semaphore::new(config.max_concurrent_subtasks.max(1)));
        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => {
                        let permits = permits.clone();
                        let worker_config = config.clone();
                        workers.spawn(async move {
                            let permit = tokio::select! {
                                biased;
                                () = request.turn.cancellation().cancelled() => None,
                                permit = permits.acquire_owned() => permit.ok(),
                            };
                            if let Some(permit) = permit {
                                run_child(request, worker_config, permit).await;
                            } else {
                                finish_cancelled(request, &worker_config).await;
                            }
                        });
                    }
                    None => break,
                },
                Some(_) = workers.join_next() => {}
            }
        }
        while workers.join_next().await.is_some() {}
    })
}

/// Run one child to its terminal state and always resolve the launch oneshot.
async fn run_child(
    mut request: SubtaskLaunchRequest,
    config: SubtaskSupervisorConfig,
    permit: OwnedSemaphorePermit,
) {
    let id = request.descriptor.id.clone();
    let kind = request.descriptor.kind;
    tracing::info!(target: "zevria::subtasks", subtask = %id, title = %request.descriptor.title, %kind, "starting subtask");

    // The JoinSet worker owns the outcome sender, but a separate cleanup task
    // owns execution, its permit and its workspace. Aborting the supervisor
    // still fails the unresolved oneshot immediately. It cancels, rather than
    // aborts, execution so command process cleanup is awaited before reuse.
    // Rig's pinned ToolServerHandle directly awaits tool futures; none of the
    // five registered tools spawns detached mutations beyond command cleanup.
    let cancellation = request.turn.cancellation().child_token();
    let _cancel_execution = cancellation.clone().drop_guard();
    let abandoned = tokio_util::sync::CancellationToken::new();
    let _mark_abandoned = abandoned.clone().drop_guard();
    let child = ChildSession {
        descriptor: request.descriptor.clone(),
        prompt: request.prompt.clone(),
        // This preparation context has no delegation capability. The execution
        // context below is bound to the child's composed policy, never the parent's.
        turn: zevria_session_api::TurnContext::new(
            request.turn.id,
            SessionMode::Build,
            cancellation,
        ),
        workspace: request.workspace.take(),
    };
    let execution_config = config.clone();
    let execution = tokio::spawn(async move {
        let _permit = permit;
        let result = run_child_session(&child, &execution_config, abandoned).await;
        // Retain ownership through engine/tool cleanup, but never through
        // terminal outcome/status publication (which may be backpressured).
        drop(child.workspace);
        result
    });
    let result = match execution.await {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("child execution failed: {error}")),
    };
    let outcome = child_outcome(result, request.turn.is_cancelled());
    if let SubtaskOutcome::Failed { error } = &outcome {
        tracing::warn!(target: "zevria::subtasks", subtask = %id, %kind, "subtask failed: {error}");
    }
    let status = match &outcome {
        SubtaskOutcome::Completed { .. } => SubtaskStatus::Completed,
        SubtaskOutcome::Failed { .. } => SubtaskStatus::Failed,
        SubtaskOutcome::Cancelled => SubtaskStatus::Cancelled,
    };
    // The awaiting tool call may have been dropped with its turn; the status
    // event still keeps the frontend row truthful.
    let _ = request.outcome.send(outcome);
    let _ = config
        .events
        .send(SessionEvent::SubtaskStatus {
            turn_id: request.turn.id,
            id,
            status,
        })
        .await;
}

fn child_outcome(result: anyhow::Result<String>, cancelled: bool) -> SubtaskOutcome {
    match result {
        Ok(report) => SubtaskOutcome::Completed { report },
        Err(error) if cancelled && !error.is::<zevria_transcript::SessionReplayError>() => {
            SubtaskOutcome::Cancelled
        }
        Err(error) => SubtaskOutcome::Failed {
            error: format!("{error:#}"),
        },
    }
}

async fn finish_cancelled(mut request: SubtaskLaunchRequest, config: &SubtaskSupervisorConfig) {
    let id = request.descriptor.id.clone();
    drop(request.workspace.take());
    let _ = request.outcome.send(SubtaskOutcome::Cancelled);
    let _ = config
        .events
        .send(SessionEvent::SubtaskStatus {
            turn_id: request.turn.id,
            id,
            status: SubtaskStatus::Cancelled,
        })
        .await;
}

struct ChildSession {
    descriptor: SubtaskDescriptor,
    prompt: String,
    turn: zevria_session_api::TurnContext,
    workspace: Option<ChildWorkspace>,
}

async fn run_child_session(
    request: &ChildSession,
    config: &SubtaskSupervisorConfig,
    abandoned: tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    let id = request.descriptor.id.clone();
    let transcript = TranscriptWriter::create_with_id(&config.subsessions_dir, id.as_str())?;
    if request.turn.is_cancelled() {
        anyhow::bail!("subtask cancelled before its model turn started");
    }
    let (tools, factory, policies) = match request.descriptor.kind {
        SubtaskKind::Explore => {
            anyhow::ensure!(
                request.workspace.is_none(),
                "explore child cannot own a workspace"
            );
            (
                config.explore_tools.clone(),
                &config.explore_factory,
                explore_policies(),
            )
        }
        SubtaskKind::Build => {
            let workspace = request
                .workspace
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("build child requires a reserved workspace"))?;
            (
                crate::runtime::build_isolated_build_tools(&workspace.path, config.command_config)?,
                &config.builder_factory,
                build_policies(&workspace.path, &config.startup_workspace),
            )
        }
    };
    let provider = factory.create(id.as_str(), tools.clone())?;
    tokio::select! {
        () = abandoned.cancelled() => anyhow::bail!("subtask supervisor aborted"),
        () = set_running(&id, request.turn.id, config) => {}
    }

    let mut engine = SessionEngine::new(
        provider,
        tools,
        policies,
        transcript,
        Arc::new(SkillCatalog::default()),
    )?
    .with_guidance_snapshot(config.guidance.clone())
    .with_compaction_policy(config.compaction.clone());

    // Forward live child events while the submission runs; the forwarder also
    // extracts the terminal result so the report comes from the same stream
    // the frontend saw.
    let (child_events, mut child_updates) = session_event_channel(64);
    let forwarder = {
        let parent_events = config.events.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let mut outcome: Option<Result<String, String>> = None;
            while let Some(update) = child_updates.recv().await {
                match update {
                    SessionUpdate::Lifecycle(event) => {
                        match &event {
                            SessionEvent::TurnCompleted { message, .. } => {
                                outcome = Some(Ok(assistant_plain_text(message)));
                            }
                            SessionEvent::TurnFailed { error, .. }
                            | SessionEvent::TurnRejected { error, .. } => {
                                outcome = Some(Err(error.clone()));
                            }
                            SessionEvent::TurnCancelled { .. } => {
                                outcome = Some(Err("subtask cancelled".to_string()));
                            }
                            _ => {}
                        }
                        // Preserve normal forwarding/backpressure. Only an
                        // abandoned worker bypasses UI delivery so cleanup
                        // cannot be stranded after supervisor abort.
                        tokio::select! {
                            biased;
                            () = abandoned.cancelled() => {},
                            _ = parent_events.send(SessionEvent::SubtaskSession {
                                id: id.clone(), event: Box::new(event),
                            }) => {}
                        }
                    }
                    SessionUpdate::Streams(batch) => {
                        if let Some(stream) = batch.root {
                            parent_events.set_subtask_stream(id.clone(), stream);
                        }
                    }
                }
            }
            outcome
        })
    };

    let child_turn = zevria_session_api::TurnContext::new(
        request.turn.id,
        SessionMode::Build,
        request.turn.cancellation().clone(),
    );
    let execution = engine
        .handle_turn(
            zevria_session_api::TurnCommand::Submit {
                text: zevria_content::UserPrompt::from_text(request.prompt.clone()),
                mode: SessionMode::Build,
                behavior: zevria_foundation::RequestBehavior::Standard,
            },
            &child_turn,
            &child_events,
        )
        .await;
    drop(child_events);

    let forwarded = forwarder.await;
    child_session_result(execution, forwarded, request.descriptor.kind)
}

fn child_session_result(
    execution: Result<(), zevria_transcript::SessionReplayError>,
    forwarded: Result<Option<Result<String, String>>, tokio::task::JoinError>,
    kind: SubtaskKind,
) -> anyhow::Result<String> {
    // A fatal replay result is authoritative even if cancellation raced it or
    // the child correctly emitted no recoverable terminal lifecycle event.
    execution?;
    match forwarded {
        Ok(Some(Ok(report))) => Ok(report),
        Ok(Some(Err(error))) => anyhow::bail!("{error}"),
        Ok(None) => anyhow::bail!("the {kind} agent finished without a final response"),
        Err(join_error) => anyhow::bail!("child event forwarding failed: {join_error}"),
    }
}

async fn set_running(
    id: &SubtaskId,
    turn_id: zevria_foundation::TurnId,
    config: &SubtaskSupervisorConfig,
) {
    let _ = config
        .events
        .send(SessionEvent::SubtaskStatus {
            turn_id,
            id: id.clone(),
            status: SubtaskStatus::Running,
        })
        .await;
}

#[cfg(test)]
#[path = "subtasks_build_tests.rs"]
mod build_tests;

#[cfg(test)]
mod tests {
    use rig_agent::tool::server::ToolServer;
    use rig_core::{message::Message, providers::openai::responses_api::ReasoningSummaryLevel};
    use zevria_foundation::ModelContextPolicy;
    use zevria_foundation::TurnId;
    use zevria_foundation::subtask::SubtaskKind;
    use zevria_foundation::subtask::SubtaskOutcome;
    use zevria_provider::{
        LiteralApiKey, ModelConfig, ProviderConfig, RemoteCompactionConfig,
        ResponsesCompatibilityConfig,
    };
    use zevria_session_api::TurnContext;
    use zevria_session_api::subtask::subtask_channels;

    use super::*;

    #[test]
    fn fatal_child_diagnostic_survives_missing_response_and_cancellation_once() {
        let error =
            zevria_transcript::SessionReplayError::Skills("original child diagnostic".into());
        let expected = error.to_string();
        let result = child_session_result(Err(error), Ok(None), SubtaskKind::Explore);
        assert!(
            matches!(child_outcome(result, true), SubtaskOutcome::Failed { error } if error == expected)
        );
        assert!(matches!(
            child_outcome(Err(anyhow::anyhow!("ordinary failure")), true),
            SubtaskOutcome::Cancelled
        ));
    }

    #[test]
    fn explore_policies_are_command_only_in_all_modes() {
        let policies = explore_policies();
        for mode in [SessionMode::Build, SessionMode::Plan] {
            let policy = policies.policy(mode);
            assert_eq!(policy.model_role, ModelRole::Explore);
            assert_eq!(policy.instructions, EXPLORE_AGENT_INSTRUCTIONS);
            let rendered = crate::runtime::rendered_instructions(policy);
            assert_eq!(
                rendered
                    .matches(zevria_instructions::prompts::INSPECTION_POLICY_INSTRUCTIONS)
                    .count(),
                1
            );
            assert!(rendered.contains("Read task-relevant files anywhere the process can read"));
            assert!(rendered.contains("Scratch-contained execution"));
            assert!(!policy.orchestration);
            assert!(!policy.skills_enabled);
            assert_eq!(
                policy.allowed_tool_names.as_deref(),
                Some(
                    &[
                        "command".to_string(),
                        zevria_foundation::WEB_SEARCH_TOOL_NAME.into()
                    ][..]
                )
            );
            assert!(policy.allows_tool("command"));
            for denied in [
                "launch_subtasks",
                "skill",
                "skill_read",
                "task",
                "question",
                "reconcile_reports",
                "submit_plan",
                "edit",
                "write",
                "delete",
            ] {
                assert!(!policy.allows_tool(denied), "{denied} must be denied");
            }
        }
    }

    #[test]
    fn assistant_reports_join_only_text_content() {
        use rig_core::message::{AssistantContent, Reasoning};
        let message = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::summaries(vec!["thinking".to_string()])),
                AssistantContent::text("first part"),
                AssistantContent::text("second part"),
            ],
        };
        assert_eq!(assistant_plain_text(&message), "first part\nsecond part");
        assert_eq!(assistant_plain_text(&Message::user("nope")), "");
    }

    fn test_supervisor_config(
        base_url: String,
        events: SessionEventSender,
        subsessions_dir: PathBuf,
    ) -> SubtaskSupervisorConfig {
        let provider = ProviderConfig {
            base_url,
            api_key: LiteralApiKey::new("test-key"),
            supports_websockets: true,
            network: Default::default(),
            session_id_header: None,
            models: std::collections::BTreeMap::from([(
                "gpt-test".to_string(),
                ModelConfig {
                    context_window_tokens: 272_000,
                    input_token_limit: None,
                    retained_user_tokens: 20_000,
                    reasoning_levels: zevria_foundation::ReasoningLevel::ALL.to_vec(),
                    reasoning_summary_level: ReasoningSummaryLevel::Detailed,
                },
            )]),
            compatibility: ResponsesCompatibilityConfig::default(),
            additional_params: Default::default(),
            compaction: RemoteCompactionConfig::default(),
            input_token_count: zevria_provider::InputTokenCountConfig::default(),
            web_search: zevria_responses::WebSearchConfig::default(),
        };
        let modes = zevria_provider::ModeAssignments {
            build: zevria_provider::ModelAssignment {
                provider: "test".to_string(),
                model: "gpt-test".to_string(),
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            },
            plan: zevria_provider::ModelAssignment {
                provider: "test".to_string(),
                model: "gpt-test".to_string(),
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            },
            review: zevria_provider::ModelAssignment {
                provider: "test".to_string(),
                model: "gpt-test".to_string(),
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            },
            explore: zevria_provider::ModelAssignment {
                provider: "test".to_string(),
                model: "gpt-test".to_string(),
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            },
            builder: zevria_provider::ModelAssignment {
                provider: "test".to_string(),
                model: "gpt-test".to_string(),
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
            },
        };
        let routing = zevria_provider::ModelRouting::resolve(
            &std::collections::BTreeMap::from([("test".to_string(), provider)]),
            &modes,
            90,
        )
        .expect("test routing");
        let profile = routing.for_role(ModelRole::Explore).clone();
        let context_profile = profile.profile.clone();
        let context_window_tokens = profile.context_window_tokens;
        let input_token_limit = profile.input_token_limit;
        let retained_user_tokens = profile.retained_user_tokens;
        let context = || ModelContextPolicy {
            profile: context_profile.clone(),
            context_window_tokens,
            input_token_limit,
            retained_user_tokens,
        };
        SubtaskSupervisorConfig {
            explore_factory: ResponsesRouterFactory::new(
                ModelRole::Explore,
                profile.clone(),
                routing
                    .selection_for_role(ModelRole::Explore)
                    .reasoning_level,
                "Test preamble",
            ),
            builder_factory: ResponsesRouterFactory::new(
                ModelRole::Builder,
                profile,
                routing
                    .selection_for_role(ModelRole::Builder)
                    .reasoning_level,
                "Test preamble",
            ),
            explore_tools: ToolServer::new().run(),
            startup_workspace: std::fs::canonicalize(subsessions_dir.parent().unwrap()).unwrap(),
            command_config: crate::config::CommandConfig::default(),
            events,
            subsessions_dir,
            compaction: CompactionPolicy::new(
                zevria_model::config::CompactionConfig::default(),
                [context(), context(), context(), context(), context()],
            )
            .expect("test compaction policy"),
            max_concurrent_subtasks: 4,
            guidance: zevria_instructions::GuidanceSnapshot::default(),
        }
    }

    fn turn() -> TurnContext {
        TurnContext::new(
            TurnId::new(1),
            SessionMode::Build,
            tokio_util::sync::CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn a_child_that_cannot_connect_still_resolves_a_failed_outcome() {
        let workspace = tempfile::tempdir().expect("workspace");
        let subsessions_dir = workspace.path().join("subsessions");
        let (events_tx, mut events_rx) = session_event_channel(32);
        let channels = subtask_channels("root-id", events_tx.clone());

        // Port 1 refuses connections, so the fresh provider handshake fails.
        let supervisor = spawn_supervisor(
            channels.requests,
            test_supervisor_config(
                "ws://127.0.0.1:1".to_string(),
                events_tx,
                subsessions_dir.clone(),
            ),
        );
        let (metadata, outcome) = channels
            .launcher
            .launch(
                "call-1",
                0,
                "doomed explore task",
                SubtaskKind::Explore,
                "a doomed prompt",
                turn(),
                None,
            )
            .await
            .expect("launch queues");

        assert!(matches!(
            outcome.await.expect("resolved outcome"),
            SubtaskOutcome::Failed { .. }
        ));
        // The child transcript was created before the connection attempt.
        assert!(
            subsessions_dir
                .join(format!("{}.jsonl", metadata.id))
                .exists()
        );
        let mut saw_failed_status = false;
        while let Ok(event) = events_rx.try_recv() {
            if matches!(
                event,
                SessionUpdate::Lifecycle(SessionEvent::SubtaskStatus {
                    status: SubtaskStatus::Failed,
                    ..
                })
            ) {
                saw_failed_status = true;
            }
        }
        assert!(saw_failed_status, "terminal status is forwarded to the UI");
        supervisor.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_guidance_does_not_persist_in_failed_child_transcripts() {
        let workspace = tempfile::tempdir().unwrap();
        let global = workspace.path().join("global");
        std::fs::create_dir(&global).unwrap();
        std::fs::write(global.join("AGENTS.md"), [0xff]).unwrap();
        std::fs::write(
            workspace.path().join("AGENTS.md"),
            "PARENT_PROJECT_SNAPSHOT",
        )
        .unwrap();
        let snapshot = zevria_instructions::load_guidance(
            &zevria_instructions::GuidanceRoots::fixture(Some(&global), workspace.path()),
        );
        assert_eq!(snapshot.diagnostics().len(), 1);
        let subsessions = workspace.path().join("subsessions");
        let (events, _receiver) = session_event_channel(128);
        let channels = subtask_channels("root", events.clone());
        let mut config =
            test_supervisor_config("ws://127.0.0.1:1".into(), events, subsessions.clone());
        config.guidance = snapshot;
        let supervisor = spawn_supervisor(channels.requests, config);
        // A repaired global file is deliberately not retried; a removed project
        // file does not remove the inherited contribution from late children.
        std::fs::write(global.join("AGENTS.md"), "REPAIRED_MUST_NOT_LOAD").unwrap();
        std::fs::remove_file(workspace.path().join("AGENTS.md")).unwrap();
        let (metadata, outcome) = channels
            .launcher
            .launch(
                "late",
                0,
                "late child",
                SubtaskKind::Explore,
                "inspect",
                turn(),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome.await.unwrap(), SubtaskOutcome::Failed { .. }),
            "offline fixture still persists admitted input"
        );
        let items = zevria_transcript::transcript::load(
            &subsessions.join(format!("{}.jsonl", metadata.id)),
        )
        .unwrap();
        let state = zevria_transcript::replay_directives(&items)
            .unwrap()
            .snapshot();
        let text = format!("{state:?}");
        assert!(!text.contains("PARENT_PROJECT_SNAPSHOT"));
        assert!(!format!("{items:?}").contains("PARENT_PROJECT_SNAPSHOT"));
        assert!(!text.contains("REPAIRED_MUST_NOT_LOAD"));
        assert!(!text.contains("Skipped global"));
        assert_eq!(state.directives.len(), 0);
        supervisor.abort();
        let _ = supervisor.await;
    }

    #[tokio::test]
    async fn aborting_the_supervisor_fails_parked_child_launches() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let address = listener.local_addr().expect("listener address");
        let workspace = tempfile::tempdir().expect("workspace");
        let (events_tx, _events_rx) = session_event_channel(32);
        let channels = subtask_channels("root-id", events_tx.clone());

        let supervisor = spawn_supervisor(
            channels.requests,
            test_supervisor_config(
                format!("ws://{address}"),
                events_tx,
                workspace.path().join("subsessions"),
            ),
        );
        let (_metadata, outcome) = channels
            .launcher
            .launch(
                "call-1",
                0,
                "parked explore task",
                SubtaskKind::Explore,
                "a parked prompt",
                turn(),
                None,
            )
            .await
            .expect("launch queues");

        // The worker reaches the TCP connect but the handshake never
        // completes, parking it mid-run.
        let (_stream, _) = listener.accept().await.expect("child dials in");
        supervisor.abort();
        assert!(
            supervisor
                .await
                .expect_err("aborted supervisor")
                .is_cancelled()
        );
        // Dropping the supervisor's JoinSet cancels the worker, so its
        // unresolved oneshot fails the awaiting launch instead of wedging it.
        assert!(outcome.await.is_err());
    }

    #[tokio::test]
    async fn cancelling_a_parent_interrupts_a_parked_child_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let address = listener.local_addr().expect("listener address");
        let workspace = tempfile::tempdir().expect("workspace");
        let (events_tx, mut events_rx) = session_event_channel(32);
        let channels = subtask_channels("root-id", events_tx.clone());
        let supervisor = spawn_supervisor(
            channels.requests,
            test_supervisor_config(
                format!("ws://{address}"),
                events_tx,
                workspace.path().join("subsessions"),
            ),
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        let parent_turn =
            TurnContext::new(TurnId::new(7), SessionMode::Build, cancellation.clone());
        let (_metadata, outcome) = channels
            .launcher
            .launch(
                "call-1",
                0,
                "parked child handshake",
                SubtaskKind::Explore,
                "wait for a provider handshake",
                parent_turn,
                None,
            )
            .await
            .expect("launch queues");

        // Accept TCP but never complete the WebSocket handshake. Parent
        // cancellation must drop the in-progress connection attempt instead
        // of waiting for the provider's connection deadline.
        let (parked_stream, _) = listener.accept().await.expect("child dials in");
        cancellation.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), outcome)
                .await
                .expect("cancellation should resolve promptly")
                .expect("supervisor resolves the launch"),
            SubtaskOutcome::Cancelled
        ));
        drop(parked_stream);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let event = events_rx.recv().await.expect("event channel remains open");
                if matches!(
                    event,
                    SessionUpdate::Lifecycle(SessionEvent::SubtaskStatus {
                        turn_id,
                        status: SubtaskStatus::Cancelled,
                        ..
                    }) if turn_id == TurnId::new(7)
                ) {
                    break;
                }
            }
        })
        .await
        .expect("cancelled status should be published promptly");
        supervisor.abort();
        let _ = supervisor.await;
    }
}
