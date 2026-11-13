//! Responses recovery loop responsibilities.
use crate::diagnostics::Diagnostics;
use crate::http::HttpAttemptError;
use crate::recovery::{
    AttemptFailure, AttemptProgress, Decision, Recovery, RecoveryPolicy, ResponseIdleTimeout,
    failure_class,
};
use zevria_model::ModelRequest;
use zevria_model::ModelResponse;
use zevria_session_api::ProgressReporter;
use zevria_session_api::event::{NetworkStatus, NetworkTransport};

use crate::{
    accumulator::AttemptState,
    connection::{
        HttpFallback, OpenAiProvider, OpenAiTransport, is_http_upgrade_required, log_identifier,
        retryable_connect_error,
    },
};

use super::errors::StaleResponseId;
use super::errors::TransientResponseError;
use super::errors::WebSocketDisconnected;
use super::errors::is_websocket_disconnect;
use super::http::run_http_model_request;
use super::prepare::PreparedRequest;
use super::prepare::prepare_turn_request;
use super::websocket::run_model_request;
pub(super) async fn run_model_request_with_reconnect(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<ModelResponse> {
    let policy = openai.recovery.clone();
    run_model_request_with_recovery(model_request, openai, state, progress, &policy).await
}

#[tracing::instrument(skip_all, fields(profile = %openai.profile))]
pub(super) async fn run_model_request_with_recovery(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<ModelResponse> {
    let prepared = prepare_turn_request(model_request, openai).await?;
    let prepared = Diagnostics::dispatch(prepared, openai);
    run_prepared_with_recovery(&prepared, openai, state, progress, policy).await
}

/// Exactly one controller for connection establishment, dispatch, and streaming.
/// A failed attempt cannot mutate prepared input or restart the tool loop.
pub(super) async fn run_prepared_with_recovery(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<ModelResponse> {
    let recovery = Recovery::new(policy);
    let reprobe = openai.http_fallback == Some(HttpFallback::Provisional);
    if reprobe {
        openai.transport = OpenAiTransport::WebSocket;
        openai.http_fallback = None;
        tracing::info!("OpenAI WebSocket restored after a network-wide outage");
        Diagnostics::transport_restored(openai);
    }
    let mut websocket_attempts = 0;
    // Only the *first* HTTP outcome can commit a provisional fallback. A
    // later success after HTTP also failed is evidence of a network outage.
    let mut first_fallback_http = false;
    while let Some(plan) = recovery.start_attempt() {
        let attempt = plan.attempt;
        let transport = openai.transport;
        let attempt_progress = AttemptProgress {
            reporter: progress,
            recovery: &recovery,
            attempt,
            transport: match transport {
                OpenAiTransport::Http => NetworkTransport::Http,
                OpenAiTransport::WebSocket => NetworkTransport::WebSocket,
            },
        };
        attempt_progress.status(NetworkStatus::AttemptStarted).await;
        let result = if transport == OpenAiTransport::Http {
            attempt_progress.status(NetworkStatus::Connecting).await;
            run_http_model_request(
                prepared,
                openai,
                state,
                progress,
                policy,
                attempt_progress,
                plan.request_start_deadline,
            )
            .await
        } else {
            websocket_attempts += 1;
            let connected = if openai.ws.session.terminal_status().is_some() {
                Diagnostics::disconnected_recovery(prepared, openai);
                attempt_progress.status(NetworkStatus::Connecting).await;
                openai.ws.config.connect_timeout = policy.connect_timeout;
                let result = openai.ws.reconnect().await;
                Diagnostics::connection(
                    openai,
                    if result.is_ok() {
                        "reconnected"
                    } else {
                        "reconnect_failed"
                    },
                );
                result
            } else {
                Ok(())
            };
            match connected {
                Err(error) if is_http_upgrade_required(&error) => Err(HttpAttemptError::retryable(
                    "websocket_upgrade_required",
                    error,
                )),
                Err(error) if retryable_connect_error(&error) => {
                    Err(HttpAttemptError::retryable("connect_error", error))
                }
                Err(error) => Err(HttpAttemptError::terminal(error)),
                Ok(()) => match run_model_request(
                    prepared,
                    openai,
                    state,
                    progress,
                    policy,
                    attempt_progress,
                )
                .await
                {
                    Ok(response) => Ok(response),
                    Err(error) if error.is::<StaleResponseId>() => {
                        Err(HttpAttemptError::retryable("stale_response_id", error))
                    }
                    Err(error) if error.is::<ResponseIdleTimeout>() => {
                        Err(HttpAttemptError::retryable("response_idle_timeout", error))
                    }
                    Err(error) if error.is::<TransientResponseError>() => {
                        let category = error
                            .downcast_ref::<TransientResponseError>()
                            .expect("matched transient")
                            .category;
                        Err(HttpAttemptError::retryable(category, error))
                    }
                    Err(error) if is_websocket_disconnect(&error) => {
                        let category = error
                            .downcast_ref::<WebSocketDisconnected>()
                            .map(WebSocketDisconnected::terminal_category)
                            .unwrap_or("disconnect");
                        Err(HttpAttemptError::retryable(category, error))
                    }
                    Err(error) => Err(HttpAttemptError::terminal(error)),
                },
            }
        };
        let last = match result {
            Ok(response) => {
                if transport == OpenAiTransport::Http
                    && first_fallback_http
                    && openai.http_fallback == Some(HttpFallback::Provisional)
                {
                    openai.http_fallback =
                        Some(HttpFallback::Sticky("websocket_retries_exhausted"));
                }
                return Ok(response);
            }
            Err(HttpAttemptError::Terminal {
                error,
                upstream_request_id,
            }) => {
                state.reset_result();
                tracing::debug!(profile = %openai.profile, attempt, transport = transport.as_str(),
                    upstream_request_id = ?upstream_request_id.as_deref().map(log_identifier), "completion ended with a terminal error");
                return Err(error);
            }
            Err(HttpAttemptError::Retryable {
                category,
                error,
                server_delay,
                upstream_request_id,
            }) => AttemptFailure {
                category,
                class: failure_class(category),
                error,
                server_delay: if category == "websocket_upgrade_required" {
                    Some(std::time::Duration::ZERO)
                } else {
                    server_delay
                },
                upstream_request_id,
            },
        };
        if transport == OpenAiTransport::Http {
            first_fallback_http = false;
        }
        Diagnostics::recovery(prepared, openai);
        state.reset_result();
        openai.ws.invalidate_continuation("attempt_failed");
        state.stale_replay = last.category == "stale_response_id";
        progress.stream_cleared();
        tracing::debug!(
            transport = transport.as_str(),
            since_last_completion_ms =
                openai.ws.idle_for().as_millis().min(u64::MAX as u128) as u64,
            "completion recovery transport state"
        );
        // 426 is definitive transport configuration, even if this request has
        // no remaining attempts. Other fallback is committed only below.
        if last.category == "websocket_upgrade_required" {
            openai.switch_to_http(last.category);
            first_fallback_http = true;
        }
        let decision = recovery.decide(&last);
        recovery.log_failure(&last, decision);
        let Decision::Retry {
            next_attempt,
            delay,
            retrying_for,
        } = decision
        else {
            let Decision::Exhausted(reason) = decision else {
                unreachable!()
            };
            return Err(recovery.exhausted_error(reason, last, &openai.profile.provider));
        };
        if transport == OpenAiTransport::WebSocket
            && last.category != "websocket_upgrade_required"
            && websocket_attempts >= if reprobe { 1 } else { 2 }
        {
            openai.switch_to_http("websocket_retries_exhausted");
            first_fallback_http = true;
        }
        progress
            .retrying(
                next_attempt,
                delay,
                retrying_for,
                format!("{}: {:#}", last.category, last.error),
            )
            .await;
        // Cancellation drops both waits and the entire completion future.
        tokio::time::sleep(delay).await;
    }
    anyhow::bail!("completion recovery requires a positive attempt cap")
}
