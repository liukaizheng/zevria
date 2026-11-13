//! Responses websocket responsibilities.
use crate::diagnostics::Diagnostics;
use crate::recovery::{AttemptProgress, ProgressClock, RecoveryPolicy};
use rig_core::error::ProviderError as CompletionError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use zevria_model::ModelResponse;
use zevria_responses::protocol::ResponseProgress;
use zevria_session_api::ProgressReporter;
use zevria_session_api::event::NetworkStatus;

use crate::{
    accumulator::AttemptState,
    connection::{OpenAiProvider, OpenAiTransport, log_identifier},
    protocol::websocket_message_to_text,
    websocket_session::{OpenAiWebSocketInbound, OpenAiWebSocketTerminalCategory},
};

use super::continuation::Transmission;
use super::continuation::full_transmission;
use super::continuation::select_transmission;
use super::driver::{DriverContext, DriverFailure, DriverTransport, ResponseDriver};
use super::errors::WebSocketDisconnected;
use super::prepare::PreparedRequest;
pub(super) async fn send_prepared_request(
    prepared: &PreparedRequest,
    transmission: &Transmission<'_>,
    openai: &mut OpenAiProvider,
    send_timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(&super::serialize::RequestBody::websocket(
        prepared,
        transmission.input,
        transmission.previous_response_id.as_deref(),
    )?)?;

    tracing::info!(
        transport = OpenAiTransport::WebSocket.as_str(),
        mode = transmission.mode.as_str(),
        socket_generation = openai.ws.socket_generation,
        history_message_count = prepared.history_message_count,
        complete_input_item_count = prepared.full_input.len(),
        transmitted_input_item_count = transmission.input.len(),
        replay_source = prepared.replay_source.as_str(),
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(log_identifier),
        prompt_cache_key_fingerprint = ?openai.transmitted_prompt_cache_key_fingerprint(),
        "OpenAI websocket request"
    );
    Diagnostics::transmission(
        prepared,
        openai,
        transmission.mode.as_str(),
        transmission.input.len(),
        transmission.previous_response_id.as_deref(),
        transmission.full_reason,
        Some(payload.as_bytes()),
    );
    match tokio::time::timeout(
        send_timeout,
        openai.ws.session.send(WebSocketMessage::text(payload)),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            openai
                .ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::SendError);
            return Err(
                WebSocketDisconnected::new("sending the request", error, "send_error").into(),
            );
        }
        Err(_) => {
            openai
                .ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::SendTimeout);
            return Err(WebSocketDisconnected::without_source(
                "waiting for the request send to be acknowledged",
                OpenAiWebSocketTerminalCategory::SendTimeout.as_str(),
            )
            .into());
        }
    }

    Ok(())
}

/// Reset only the in-flight attempt and its late-`done` marker. A completed
/// continuation remains usable on this socket until a concrete invalidation
/// or a successful reconnect replaces the socket generation.
pub(super) fn abandon_attempt(openai: &mut OpenAiProvider, state: &mut AttemptState) {
    state.reset_result();
    openai.ws.pending_done_response_id = None;
}

pub(super) async fn run_model_request(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
) -> anyhow::Result<ModelResponse> {
    state.attempt = progress
        .records_attempts()
        .then(|| crate::attempt::AttemptStream::new(openai.profile.clone(), progress));
    let result =
        run_model_request_inner(prepared, openai, state, progress, policy, attempt_progress).await;
    state.flush_stream(progress);
    let result = match result {
        Ok(response) => response.with_attempt_id(
            state
                .attempt
                .as_ref()
                .map(|attempt| attempt.id().to_owned()),
        ),
        Err(error) => {
            if let Some(attempt) = &mut state.attempt {
                attempt.close_failed(progress).await?;
            }
            Err(error)
        }
    };
    Diagnostics::finish(prepared, openai, result.as_ref().ok());
    result
}

pub(super) async fn run_model_request_inner(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
) -> anyhow::Result<ModelResponse> {
    state.reset_result();
    let mut telemetry = crate::telemetry::RequestTelemetry::new();
    // Remember the prior terminal even if selecting this transmission must
    // invalidate continuation. Late duplicate terminals belong to that old
    // attempt, not to the new search trail or answer accumulator.
    let prior_terminal_id = openai.ws.pending_done_response_id.clone().or_else(|| {
        openai
            .ws
            .continuation
            .as_ref()
            .map(|continuation| continuation.response_id.clone())
    });

    let transmission = if std::mem::take(&mut state.stale_replay) {
        full_transmission(prepared)
    } else {
        select_transmission(prepared, openai)
    };
    let request_mode = transmission.mode;
    let socket_generation = openai.ws.socket_generation;
    let previous_response_id = transmission.previous_response_id.clone();
    if let Err(error) =
        send_prepared_request(prepared, &transmission, openai, policy.send_timeout).await
    {
        abandon_attempt(openai, state);
        return Err(error);
    }
    attempt_progress
        .status(NetworkStatus::AwaitingResponse)
        .await;
    let mut clock = ProgressClock::new(policy);
    let mut response_progress = ResponseProgress::new(prior_terminal_id.clone());
    for id in &state.response_ids {
        response_progress.exclude_response(id.clone());
    }

    let mut driver = ResponseDriver::new(
        DriverTransport::WebSocket {
            request_mode,
            socket_generation,
            previous_response: previous_response_id.is_some(),
        },
        prior_terminal_id,
        response_progress,
    );

    loop {
        let inbound = match state
            .next_event(
                clock.next(openai.ws.session.next(), attempt_progress),
                progress,
            )
            .await
        {
            Ok(inbound) => inbound,
            Err(error) => {
                openai
                    .ws
                    .session
                    .terminate(OpenAiWebSocketTerminalCategory::ResponseIdleTimeout);
                openai.ws.invalidate_continuation("response_idle_timeout");
                abandon_attempt(openai, state);
                return Err(error.into());
            }
        };

        let Some(inbound) = inbound else {
            let error = openai
                .ws
                .session
                .terminal_status()
                .map(|terminal| {
                    WebSocketDisconnected::from_terminal("waiting for the turn to finish", terminal)
                })
                .unwrap_or_else(|| {
                    WebSocketDisconnected::without_source(
                        "waiting for the turn to finish",
                        "pump_stopped",
                    )
                });
            abandon_attempt(openai, state);
            return Err(error.into());
        };

        let message = match inbound {
            OpenAiWebSocketInbound::Message(message) => message,
            OpenAiWebSocketInbound::Error(error) => {
                let terminal_category = openai
                    .ws
                    .session
                    .terminal_status()
                    .map(OpenAiWebSocketTerminalCategory::as_str)
                    .unwrap_or(OpenAiWebSocketTerminalCategory::ReadError.as_str());
                openai
                    .ws
                    .session
                    .terminate(OpenAiWebSocketTerminalCategory::ReadError);
                abandon_attempt(openai, state);
                return Err(WebSocketDisconnected::new(
                    "reading the response",
                    error,
                    terminal_category,
                )
                .into());
            }
        };

        if let WebSocketMessage::Close(frame) = &message {
            let reason = frame
                .as_ref()
                .map(|frame| frame.reason.to_string())
                .unwrap_or_default();
            openai
                .ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::CloseFrame);
            abandon_attempt(openai, state);
            let error = if reason.is_empty() {
                WebSocketDisconnected::without_source(
                    "reading the response (the server closed the connection)",
                    OpenAiWebSocketTerminalCategory::CloseFrame.as_str(),
                )
            } else {
                WebSocketDisconnected::from_server_reason(
                    "reading the response (the server closed the connection)",
                    reason,
                    OpenAiWebSocketTerminalCategory::CloseFrame.as_str(),
                )
            };
            return Err(error.into());
        }

        let message = match websocket_message_to_text(message) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => {
                abandon_attempt(openai, state);
                return Err(error.into());
            }
        };

        if message.trim().is_empty() || message.trim() == "[DONE]" {
            continue;
        }
        let raw: serde_json::Value =
            serde_json::from_str(&message).map_err(CompletionError::from)?;
        if let Some(response) = driver
            .process(
                DriverContext {
                    openai,
                    state,
                    prepared,
                    progress,
                    telemetry: &mut telemetry,
                    clock: &mut clock,
                    attempt_progress,
                },
                raw,
                message.len(),
            )
            .await
            .map_err(DriverFailure::into_websocket)?
        {
            return Ok(response);
        }
    }
}
