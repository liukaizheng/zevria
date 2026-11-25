//! ACP permission policy, response delivery, and native artifact admission.

use super::attempt::{AttemptShared, PendingInteraction};
use super::failure::{acp_error, set_violation};
use super::handoff::{
    ClaudeHandoffPermission, ClaudePlanAdmission, ClaudePlanPermissionTicket, NativeCaptureGuard,
};
use super::logging::RunLog;
use super::wire::{permission_name, tool_kind_name};
use agent_client_protocol::schema::v1::{
    CancelNotification, Error as AcpError, PermissionOption, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, ToolKind,
};
use agent_client_protocol::{Agent, ConnectionTo};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zevria_workflow::{AgentRunEvent, EnsembleWorkflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkflowConstraintPolicy {
    EnforcedReadOnly,
    PromptOnlyReadOnly,
}

impl WorkflowConstraintPolicy {
    pub(super) const fn for_workflow(workflow: EnsembleWorkflow) -> Self {
        match workflow {
            EnsembleWorkflow::Plan => Self::EnforcedReadOnly,
            EnsembleWorkflow::Review => Self::PromptOnlyReadOnly,
        }
    }
}

pub(super) struct PermissionContext<'a> {
    pub(super) shared: &'a Arc<AttemptShared>,
    pub(super) cancellation: &'a tokio_util::sync::CancellationToken,
    pub(super) constraint_policy: WorkflowConstraintPolicy,
    pub(super) attempt_id: Option<u64>,
    pub(super) worker_lifetime: &'a tokio_util::sync::CancellationToken,
    pub(super) peer_cancellation: agent_client_protocol::RequestCancellation,
    pub(super) cancel_grace: Duration,
    pub(super) interaction: PendingInteraction,
}

pub(super) async fn handle_permission_request<R>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    context: PermissionContext<'_>,
) -> Result<(), AcpError>
where
    R: PermissionResponder + Send + 'static,
{
    let PermissionContext {
        shared,
        cancellation,
        constraint_policy,
        attempt_id,
        worker_lifetime,
        peer_cancellation,
        cancel_grace,
        interaction,
    } = context;
    let AttemptShared {
        log,
        violation,
        session_gate,
        plan_handoff,
        ..
    } = shared.as_ref();
    if cancellation.is_cancelled()
        || worker_lifetime.is_cancelled()
        || peer_cancellation.is_cancelled()
    {
        return respond_cancelled_permission(request, responder, log).await;
    }

    let request_session_id = request.session_id.to_string();
    let expected_session_id = session_gate
        .lock()
        .expect("ACP session gate lock poisoned")
        .bound_id()
        .map(str::to_string);
    if expected_session_id.as_deref() != Some(request_session_id.as_str()) {
        return reject_invalid_permission(
            request,
            responder,
            connection,
            log,
            cancellation,
            violation,
            format!(
                "agent requested permission for mismatched ACP session {request_session_id:?}; expected {expected_session_id:?}"
            ),
        )
        .await;
    }

    if let Some(plan_handoff) = plan_handoff {
        let (permission, ticket) = plan_handoff
            .queue_permission(
                &request,
                attempt_id.expect("native handoff has a connection attempt"),
            )
            .await;
        match permission {
            ClaudeHandoffPermission::Ordinary => {}
            ClaudeHandoffPermission::ArtifactMutation => {
                let Some(option) = artifact_mutation_permission_option(&request.options) else {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        "Claude plan artifact mutation offered no allow_once permission option"
                            .to_string(),
                    )
                    .await;
                };
                let option_id = option.option_id.clone();
                let ticket = ticket.expect("artifact permission is queued");
                let context = ArtifactPermissionContext {
                    shared: shared.clone(),
                    cancellation: cancellation.clone(),
                    worker_lifetime: worker_lifetime.clone(),
                    peer_cancellation,
                };
                let task_connection = connection.clone();
                // The ticket is already registered in receive order. Its Drop
                // guard removes it even if spawn fails or the task is abandoned.
                connection.spawn(async move {
                    let _interaction = interaction;
                    handle_artifact_permission(
                        request,
                        responder,
                        &task_connection,
                        context,
                        ticket,
                        option_id,
                    )
                    .await
                })?;
                return Ok(());
            }
            ClaudeHandoffPermission::ExitPlanMode { source } => {
                let Some(option) = keep_planning_permission_option(&request.options) else {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        "Claude ExitPlanMode offered no one-shot rejection option".to_string(),
                    )
                    .await;
                };
                let option_id = option.option_id.clone();
                let durable_option_id = Some(option_id.to_string());
                if cancellation.is_cancelled() {
                    return respond_cancelled_permission(request, responder, log).await;
                }
                let tool_call_id = request.tool_call.tool_call_id.to_string();
                if let Err(error) = plan_handoff.begin_settlement(&tool_call_id, source) {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        log,
                        cancellation,
                        violation,
                        error,
                    )
                    .await;
                }
                let guard = NativeCaptureGuard(plan_handoff.clone());
                responder.respond_permission(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))?;
                let permission = AgentRunEvent::Permission {
                    tool_kind: request
                        .tool_call
                        .fields
                        .kind
                        .as_ref()
                        .map(|kind| tool_kind_name(kind).to_owned()),
                    decision: "reject_once".to_string(),
                    option_id: durable_option_id,
                };
                let log = log.clone();
                let cancellation = cancellation.clone();
                let lifetime = worker_lifetime.clone();
                // Answer first: some adapters cannot deliver the Write terminal
                // until ExitPlanMode's rejection has been received.
                connection.spawn(async move {
                    let _interaction = interaction;
                    guard
                        .settle(
                            log,
                            permission,
                            cancellation,
                            lifetime,
                            peer_cancellation,
                            cancel_grace,
                        )
                        .await
                })?;
                return Ok(());
            }
            ClaudeHandoffPermission::Invalid(error) => {
                return reject_invalid_permission(
                    request,
                    responder,
                    connection,
                    log,
                    cancellation,
                    violation,
                    error,
                )
                .await;
            }
        }
    }

    let tool_kind = request.tool_call.fields.kind;
    let choice = select_permission_option(
        constraint_policy,
        &request.options,
        tool_kind,
        cancellation.is_cancelled(),
    );
    let kind_name = tool_kind.map(|kind| tool_kind_name(&kind).to_owned());
    let (outcome, decision, option_id) = match choice {
        Some(option) => (
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            permission_name(&option.kind).to_owned(),
            Some(option.option_id.to_string()),
        ),
        None => {
            let decision = if cancellation.is_cancelled() {
                "cancelled".to_string()
            } else {
                let error = match constraint_policy {
                    WorkflowConstraintPolicy::EnforcedReadOnly => {
                        "agent offered no safe permission response; cancelling worker"
                    }
                    WorkflowConstraintPolicy::PromptOnlyReadOnly => {
                        "review agent offered no allow_once permission response; cancelling worker"
                    }
                }
                .to_string();
                set_violation(violation, error);
                cancellation.cancel();
                connection
                    .send_notification(CancelNotification::new(request.session_id.clone()))?;
                "cancelled_no_safe_option".to_string()
            };
            (RequestPermissionOutcome::Cancelled, decision, None)
        }
    };
    // Respond synchronously before awaiting transcript I/O. This keeps a
    // Ctrl-C observed by the handler from turning into a late allow_once.
    responder.respond_permission(RequestPermissionResponse::new(outcome))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: kind_name,
        decision,
        option_id,
    })
    .await
    .map_err(acp_error)
}

struct ArtifactPermissionContext {
    shared: Arc<AttemptShared>,
    cancellation: tokio_util::sync::CancellationToken,
    worker_lifetime: tokio_util::sync::CancellationToken,
    peer_cancellation: agent_client_protocol::RequestCancellation,
}

impl ArtifactPermissionContext {
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
            || self.worker_lifetime.is_cancelled()
            || self.peer_cancellation.is_cancelled()
    }
}

async fn handle_artifact_permission<R: PermissionResponder>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    context: ArtifactPermissionContext,
    ticket: ClaudePlanPermissionTicket,
    option_id: agent_client_protocol::schema::v1::PermissionOptionId,
) -> Result<(), AcpError> {
    // Subscribe BEFORE checking state. watch retains a change between that
    // check and changed().await, including a terminal sent immediately after
    // the first permission response. No mutex guard crosses an await.
    let mut changed = ticket.handoff.changed.subscribe();
    let result = async {
        loop {
            if context.is_cancelled() {
                return respond_cancelled_permission(request, responder, &context.shared.log).await;
            }
            let session_matches = context
                .shared
                .session_gate
                .lock()
                .expect("ACP session gate lock poisoned")
                .bound_id()
                == Some(ticket.key.session_id.as_str());
            let admission = if session_matches {
                ticket
                    .try_admit_async(&request, || context.is_cancelled())
                    .await
            } else {
                Err("queued Claude plan permission no longer matches the ACP session".to_string())
            };
            match admission {
                Ok(ClaudePlanAdmission::Waiting) => {}
                Ok(ClaudePlanAdmission::Cancelled) => {
                    return respond_cancelled_permission(request, responder, &context.shared.log)
                        .await;
                }
                Ok(ClaudePlanAdmission::Granted) => {
                    // Ownership is logical state, not a task-scoped lock. There
                    // is no await between the final cancellation check/reserve
                    // and queuing the exact offered one-shot response.
                    return deliver_artifact_permission(
                        &request,
                        responder,
                        &context.shared.log,
                        &ticket,
                        option_id,
                    )?
                    .await;
                }
                Err(error) => {
                    return reject_invalid_permission(
                        request,
                        responder,
                        connection,
                        &context.shared.log,
                        &context.cancellation,
                        &context.shared.violation,
                        error,
                    )
                    .await;
                }
            }
            tokio::select! {
                biased;
                () = context.cancellation.cancelled() => {},
                () = context.worker_lifetime.cancelled() => {},
                () = context.peer_cancellation.cancelled() => {},
                result = changed.changed() => { result.map_err(acp_error)?; },
            }
        }
    }
    .await;
    if let Err(error) = &result {
        // In particular, ambiguous response delivery or failed Permission sync
        // must fail closed before another waiter can be admitted.
        set_violation(
            &context.shared.violation,
            format!("Claude plan permission task failed: {error}"),
        );
        context.cancellation.cancel();
    }
    result
}

pub(super) fn deliver_artifact_permission<'a, R: PermissionResponder>(
    request: &'a RequestPermissionRequest,
    responder: R,
    log: &'a RunLog,
    ticket: &'a ClaudePlanPermissionTicket,
    option_id: agent_client_protocol::schema::v1::PermissionOptionId,
) -> Result<impl Future<Output = Result<(), AcpError>> + Send + 'a, AcpError> {
    // Deliberately not an async fn: queue the response synchronously, before
    // returning the transcript future to the caller.
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.clone())),
    ))?;
    Ok(async move {
        log.emit(AgentRunEvent::Permission {
            tool_kind: request
                .tool_call
                .fields
                .kind
                .as_ref()
                .map(|kind| tool_kind_name(kind).to_owned()),
            decision: "allow_once".to_string(),
            option_id: Some(option_id.to_string()),
        })
        .await
        .map_err(acp_error)?;
        ticket.delivered();
        Ok(())
    })
}

async fn respond_cancelled_permission<R>(
    request: RequestPermissionRequest,
    responder: R,
    log: &RunLog,
) -> Result<(), AcpError>
where
    R: PermissionResponder,
{
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Cancelled,
    ))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: request
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(|kind| tool_kind_name(kind).to_owned()),
        decision: "cancelled".to_string(),
        option_id: None,
    })
    .await
    .map_err(acp_error)
}

async fn reject_invalid_permission<R>(
    request: RequestPermissionRequest,
    responder: R,
    connection: &ConnectionTo<Agent>,
    log: &RunLog,
    cancellation: &tokio_util::sync::CancellationToken,
    violation: &Mutex<Option<String>>,
    error: String,
) -> Result<(), AcpError>
where
    R: PermissionResponder,
{
    set_violation(violation, error);
    cancellation.cancel();
    responder.respond_permission(RequestPermissionResponse::new(
        RequestPermissionOutcome::Cancelled,
    ))?;
    connection.send_notification(CancelNotification::new(request.session_id))?;
    log.emit(AgentRunEvent::Permission {
        tool_kind: request
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(|kind| tool_kind_name(kind).to_owned()),
        decision: "cancelled_invalid_handoff".to_string(),
        option_id: None,
    })
    .await
    .map_err(acp_error)
}

pub(super) fn select_permission_option(
    constraint_policy: WorkflowConstraintPolicy,
    options: &[PermissionOption],
    tool_kind: Option<ToolKind>,
    cancelled: bool,
) -> Option<&PermissionOption> {
    if cancelled {
        return None;
    }
    if constraint_policy == WorkflowConstraintPolicy::PromptOnlyReadOnly {
        return options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::AllowOnce);
    }
    let safe_kind = matches!(
        tool_kind,
        Some(ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Execute)
    );
    if safe_kind
        && let Some(option) = options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::AllowOnce)
    {
        return Some(option);
    }
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectAlways)
        })
}

pub(super) fn artifact_mutation_permission_option(
    options: &[PermissionOption],
) -> Option<&PermissionOption> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
}

pub(super) fn keep_planning_permission_option(
    options: &[PermissionOption],
) -> Option<&PermissionOption> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
}

/// Small adapter around the SDK responder so the permission policy remains
/// directly unit-testable without naming its internal generic type.
pub(super) trait PermissionResponder {
    fn respond_permission(self, response: RequestPermissionResponse) -> Result<(), AcpError>;
}

impl PermissionResponder for agent_client_protocol::Responder<RequestPermissionResponse> {
    fn respond_permission(self, response: RequestPermissionResponse) -> Result<(), AcpError> {
        self.respond(response)
    }
}
