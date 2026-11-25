//! Worker elicitation admission, host question lifetime, and reply settlement.

use super::SupervisorInner;
use super::attempt::AttemptShared;
use super::elicitation::{
    ElicitationConversionError, convert_elicitation, elicitation_field_count,
};
use super::failure::{acp_error, set_violation};
use super::logging::RunLog;
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, Error as AcpError,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use zevria_foundation::{QuestionRequestId, QuestionResponse};
use zevria_session_api::TurnContext;
use zevria_workflow::{
    AgentElicitationOutcome, AgentRunEvent, AgentUnavailableDecision, AgentUserDecisionBatch,
    MAX_NORMALIZED_USER_DECISION_BYTES,
};

/// Also runs when the ACP future is dropped or its driver is hard-aborted.
pub(super) struct WorkerElicitationCleanup {
    pub(super) shared: Arc<AttemptShared>,
    pub(super) supervisor: Arc<SupervisorInner>,
}
impl Drop for WorkerElicitationCleanup {
    fn drop(&mut self) {
        self.shared.lifetime.cancel();
        let request = self
            .shared
            .active_question
            .lock()
            .expect("ACP question state lock poisoned")
            .take();
        if let Some(request_id) = request {
            self.supervisor.questions.cancel_request(&request_id);
        }
    }
}

pub(super) struct ElicitationHandlerContext {
    pub(super) supervisor: Arc<SupervisorInner>,
    pub(super) shared: Arc<AttemptShared>,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) worker_lifetime: tokio_util::sync::CancellationToken,
    pub(super) session_ready: bool,
}

pub(super) async fn handle_elicitation_request(
    request: CreateElicitationRequest,
    responder: agent_client_protocol::Responder<CreateElicitationResponse>,
    context: ElicitationHandlerContext,
) -> Result<(), AcpError> {
    let peer_cancellation = responder.cancellation();
    // For questions that enter the broker, keep the fair global gate until the
    // terminal ACP response has entered the connection's outgoing queue.
    let (field_count, resolution, _gate) =
        request_elicitation(request, &context, &peer_cancellation).await;
    resolve_elicitation(responder, &context.shared.log, field_count, resolution).await
}

async fn request_elicitation<'a>(
    request: CreateElicitationRequest,
    context: &'a ElicitationHandlerContext,
    peer_cancellation: &agent_client_protocol::RequestCancellation,
) -> (
    usize,
    ElicitationResolution,
    Option<tokio::sync::MutexGuard<'a, ()>>,
) {
    if peer_cancellation.is_cancelled() {
        return (
            elicitation_field_count(&request),
            ElicitationResolution::RequestCancelled,
            None,
        );
    }
    if !context.session_ready {
        tracing::debug!(target: "zevria::ensemble", "declined ACP elicitation received before the prompt turn started");
        return (
            elicitation_field_count(&request),
            ElicitationResolution::Unsupported,
            None,
        );
    }
    let shared = &context.shared;
    let expected_session = shared
        .session_gate
        .lock()
        .expect("ACP session gate lock poisoned")
        .bound_id()
        .map(str::to_string);
    let request_id = QuestionRequestId::generate();
    let converted = match convert_elicitation(
        &request,
        expected_session.as_deref(),
        &shared.source_label,
        request_id.clone(),
    ) {
        Ok(converted) => converted,
        Err(error @ ElicitationConversionError::Decline { .. }) => {
            tracing::debug!(target: "zevria::ensemble", reason = %error.reason(), "declined unsupported ACP elicitation");
            return (
                error.field_count(),
                ElicitationResolution::Unsupported,
                None,
            );
        }
        Err(error @ ElicitationConversionError::ProtocolViolation { .. }) => {
            let violation = error.reason().to_string();
            set_violation(&shared.violation, violation.clone());
            context.cancellation.cancel();
            tracing::debug!(target: "zevria::ensemble", reason = %violation, "cancelled invalid ACP elicitation");
            return (
                error.field_count(),
                ElicitationResolution::ProtocolViolation,
                None,
            );
        }
    };
    let field_count = converted.fields.len();
    let gate = tokio::select! {
        biased;
        () = peer_cancellation.cancelled() => return (field_count, ElicitationResolution::RequestCancelled, None),
        () = context.cancellation.cancelled() => return (field_count, ElicitationResolution::Cancelled, None),
        () = context.worker_lifetime.cancelled() => return (field_count, ElicitationResolution::Cancelled, None),
        gate = context.supervisor.question_gate.lock() => gate,
    };
    if peer_cancellation.is_cancelled() {
        return (field_count, ElicitationResolution::RequestCancelled, None);
    }
    if context.cancellation.is_cancelled() || context.worker_lifetime.is_cancelled() {
        return (field_count, ElicitationResolution::Cancelled, None);
    }

    *shared
        .active_question
        .lock()
        .expect("ACP question state lock poisoned") = Some(request_id.clone());
    let question_cancellation = tokio_util::sync::CancellationToken::new();
    let question_turn = TurnContext::new(
        shared.turn.id,
        shared.turn.mode,
        question_cancellation.clone(),
    )
    .with_build_subtasks(shared.turn.build_subtasks);
    let mut question = Box::pin(
        context
            .supervisor
            .questions
            .ask_request(converted.request.clone(), question_turn),
    );
    let resolution = tokio::select! {
        biased;
        () = peer_cancellation.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::RequestCancelled
        }
        () = context.cancellation.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::Cancelled
        }
        () = context.worker_lifetime.cancelled() => {
            question_cancellation.cancel();
            let _ = question.as_mut().await;
            ElicitationResolution::Cancelled
        }
        response = question.as_mut() => match response {
            Ok(QuestionResponse::Dismissed) => ElicitationResolution::Declined,
            Ok(response @ QuestionResponse::Answered { .. }) => {
                let decision = converted.normalized_decision(&response);
                match (decision, converted.accepted_content(response)) {
                    (Ok(decision), Ok(content)) => ElicitationResolution::Accepted {
                        content,
                        decision: bounded_captured_decision(decision, field_count),
                    },
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::debug!(target: "zevria::ensemble", reason = %error, "cancelled invalid frontend elicitation response");
                        ElicitationResolution::Unavailable
                    }
                }
            }
            Err(error) => {
                tracing::debug!(target: "zevria::ensemble", reason = %error, "ACP elicitation question broker became unavailable");
                ElicitationResolution::Unavailable
            }
        },
    };
    clear_active_question(&shared.active_question, &request_id);

    (field_count, resolution, Some(gate))
}

async fn resolve_elicitation(
    responder: agent_client_protocol::Responder<CreateElicitationResponse>,
    log: &RunLog,
    field_count: usize,
    resolution: ElicitationResolution,
) -> Result<(), AcpError> {
    let (outcome, response, decision, decision_unavailable) = match resolution {
        ElicitationResolution::Accepted { content, decision } => {
            let (decision, decision_unavailable) = match decision {
                CapturedDecision::Available(decision) => (Some(decision), None),
                CapturedDecision::Unavailable(unavailable) => (None, Some(unavailable)),
            };
            (
                AgentElicitationOutcome::Accepted,
                Ok(CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(content),
                ))),
                decision,
                decision_unavailable,
            )
        }
        ElicitationResolution::Declined => (
            AgentElicitationOutcome::Declined,
            Ok(CreateElicitationResponse::new(ElicitationAction::Decline)),
            None,
            None,
        ),
        ElicitationResolution::Cancelled => (
            AgentElicitationOutcome::Cancelled,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
            None,
            None,
        ),
        ElicitationResolution::RequestCancelled => (
            AgentElicitationOutcome::RequestCancelled,
            Err(AcpError::request_cancelled()),
            None,
            None,
        ),
        ElicitationResolution::Unsupported => (
            AgentElicitationOutcome::Unsupported,
            Ok(CreateElicitationResponse::new(ElicitationAction::Decline)),
            None,
            None,
        ),
        ElicitationResolution::ProtocolViolation => (
            AgentElicitationOutcome::ProtocolViolation,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
            None,
            None,
        ),
        ElicitationResolution::Unavailable => (
            AgentElicitationOutcome::Unavailable,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
            None,
            None,
        ),
    };
    log.emit(AgentRunEvent::Elicitation {
        field_count,
        outcome,
        decision,
        decision_unavailable,
    })
    .await
    .map_err(acp_error)?;
    responder.respond_with_result(response)
}

enum ElicitationResolution {
    Accepted {
        content: BTreeMap<String, ElicitationContentValue>,
        decision: CapturedDecision,
    },
    Declined,
    Cancelled,
    RequestCancelled,
    Unsupported,
    ProtocolViolation,
    Unavailable,
}

pub(super) enum CapturedDecision {
    Available(AgentUserDecisionBatch),
    Unavailable(AgentUnavailableDecision),
}

pub(super) fn bounded_captured_decision(
    decision: AgentUserDecisionBatch,
    field_count: usize,
) -> CapturedDecision {
    if zevria_foundation::BoundedCountingWriter::json_fits(
        &decision,
        MAX_NORMALIZED_USER_DECISION_BYTES,
    )
    .unwrap_or(false)
    {
        CapturedDecision::Available(decision)
    } else {
        CapturedDecision::Unavailable(AgentUnavailableDecision::normalized_payload_too_large(
            decision.request_id,
            field_count,
        ))
    }
}

fn clear_active_question(
    active: &Mutex<Option<QuestionRequestId>>,
    request_id: &QuestionRequestId,
) {
    let mut active = active.lock().expect("ACP question state lock poisoned");
    if active.as_ref() == Some(request_id) {
        *active = None;
    }
}
