//! Per-child workspace, least-privilege tool policy, engine execution, and event forwarding.

use std::{path::Path, sync::Arc};

use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;
use zevria_content::assistant_plain_text;
use zevria_core::SessionEngine;
use zevria_foundation::{
    SessionMode, SessionPolicies, TurnPolicy, WorkspaceBinding, WorkspaceContract,
    subtask::{SubtaskDescriptor, SubtaskId, SubtaskKind, SubtaskOutcome, SubtaskStatus},
};
use zevria_instructions::prompts::{BUILD_SUBTASK_INSTRUCTIONS, EXPLORE_AGENT_INSTRUCTIONS};
use zevria_session_api::{
    SessionEvent, SessionEventSender, SessionUpdate, SubtaskEvent, TurnEvent,
    session_event_channel,
    subtask::{ChildWorkspace, SubtaskLaunchRequest},
};
use zevria_transcript::transcript::TranscriptWriter;

use super::SubtaskSupervisorConfig;

/// The composed Explore inspection/scratch overlay with a command-only local
/// allow-list, identical in every nominal mode so switching cannot add tools.
pub(super) fn explore_policies() -> SessionPolicies {
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

pub(super) fn build_policies(child_workspace: &Path, startup_workspace: &Path) -> SessionPolicies {
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

/// Run one child to its terminal state and always resolve the launch oneshot.
pub(super) async fn run_child(
    mut request: SubtaskLaunchRequest,
    config: Arc<SubtaskSupervisorConfig>,
    permit: OwnedSemaphorePermit,
    cancellation: &CancellationToken,
    stop_delivery: &CancellationToken,
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
    let child = ChildSession {
        descriptor: request.descriptor.clone(),
        prompt: std::mem::take(&mut request.prompt),
        // This preparation context has no delegation capability. The execution
        // context below is bound to the child's composed policy, never the parent's.
        turn: zevria_session_api::TurnContext::new(
            request.turn.id,
            SessionMode::Build,
            cancellation.clone(),
        ),
        workspace: request.workspace.take(),
    };
    let execution_config = config.clone();
    let stop_forwarding = stop_delivery.clone();
    let execution = tokio::spawn(async move {
        let _permit = permit;
        run_child_session(child, &execution_config, stop_forwarding).await
    });
    let result = match execution.await {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("child execution failed: {error}")),
    };
    let outcome = child_outcome(result, cancellation.is_cancelled());
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
    send_status(&config.events, request.turn.id, id, status, stop_delivery).await;
}

pub(super) fn child_outcome(result: anyhow::Result<String>, cancelled: bool) -> SubtaskOutcome {
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

pub(super) async fn finish_cancelled(
    mut request: SubtaskLaunchRequest,
    config: &SubtaskSupervisorConfig,
    stop_delivery: &CancellationToken,
) {
    let id = request.descriptor.id.clone();
    drop(request.workspace.take());
    let _ = request.outcome.send(SubtaskOutcome::Cancelled);
    send_status(
        &config.events,
        request.turn.id,
        id,
        SubtaskStatus::Cancelled,
        stop_delivery,
    )
    .await;
}

pub(super) struct ChildSession {
    pub(super) descriptor: SubtaskDescriptor,
    pub(super) prompt: String,
    pub(super) turn: zevria_session_api::TurnContext,
    pub(super) workspace: Option<ChildWorkspace>,
}

pub(super) async fn run_child_session(
    request: ChildSession,
    config: &SubtaskSupervisorConfig,
    stop_delivery: CancellationToken,
) -> anyhow::Result<String> {
    let id = request.descriptor.id.clone();
    let directory = config.subsessions_dir.clone();
    let child_id = id.clone();
    let transcript = tokio::task::spawn_blocking(move || {
        TranscriptWriter::allocate_with_id(&directory, child_id.as_str())
    })
    .await??;
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
    let provider = factory.create(id.as_str())?;
    send_status(
        &config.events,
        request.turn.id,
        id.clone(),
        SubtaskStatus::Running,
        &stop_delivery,
    )
    .await;

    let mut engine = SessionEngine::builder(
        provider,
        tools,
        policies,
        transcript,
        config.empty_skills.clone(),
    )?
    .with_incremental_tools(config.incremental_tools)
    .with_guidance_snapshot(config.guidance.clone())
    .with_compaction_policy(config.compaction.clone())
    .build()?;

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
                            SessionEvent::Turn(TurnEvent::Completed { message, .. }) => {
                                outcome = Some(Ok(assistant_plain_text(message)));
                            }
                            SessionEvent::Turn(TurnEvent::Failed { error, .. })
                            | SessionEvent::Turn(TurnEvent::Rejected { error, .. }) => {
                                outcome = Some(Err(error.clone()));
                            }
                            SessionEvent::Turn(TurnEvent::Cancelled { .. }) => {
                                outcome = Some(Err("subtask cancelled".to_string()));
                            }
                            _ => {}
                        }
                        // Preserve normal forwarding/backpressure, but do
                        // not strand execution cleanup on a stopped runtime.
                        tokio::select! {
                            biased;
                            () = stop_delivery.cancelled() => {},
                            _ = parent_events.send(SessionEvent::Subtask(SubtaskEvent::Session {
                                id: id.clone(), event: Box::new(event),
                            })) => {}
                        }
                    }
                    SessionUpdate::Streams(batch) => {
                        if let Some(stream) = batch.root
                            && !stop_delivery.is_cancelled()
                        {
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
                text: zevria_content::UserPrompt::from_text(request.prompt),
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

pub(super) fn child_session_result(
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

async fn send_status(
    events: &SessionEventSender,
    turn_id: zevria_foundation::TurnId,
    id: SubtaskId,
    status: SubtaskStatus,
    stop_delivery: &CancellationToken,
) {
    tokio::select! {
        biased;
        // Send when immediately possible, even at shutdown, without letting
        // a full frontend queue prevent the supervisor from draining.
        _ = events.send(SessionEvent::Subtask(SubtaskEvent::Status { turn_id, id, status })) => {},
        () = stop_delivery.cancelled() => {},
    }
}
