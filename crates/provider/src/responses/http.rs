//! Responses http responsibilities.
use crate::diagnostics::Diagnostics;
use crate::http::{HttpAttemptError, bounded_http_error_body, retry_hint};
use crate::recovery::{AttemptProgress, ProgressClock, RecoveryPolicy};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use rig_core::error::ProviderError as CompletionError;
use zevria_model::ModelResponse;
use zevria_model::ProviderReplay;
use zevria_responses::protocol::ResponseProgress;
use zevria_session_api::event::NetworkStatus;

use crate::{
    accumulator::AttemptState,
    connection::{OpenAiProvider, OpenAiTransport, response_request_id},
    credential::{CredentialRefresh, http_unauthorized, rejected_after_refresh},
};

use super::driver::RequestMode;
use super::driver::TerminalResponse;
use super::driver::log_terminal_response;
use super::driver::{DriverContext, DriverFailure, DriverTransport, ResponseDriver};
use super::errors::classify_input_size;
use super::errors::{structured_input_size, validation_input_size_rejection};
use super::prepare::PreparedRequest;
fn http_request_body(prepared: &PreparedRequest) -> anyhow::Result<&bytes::Bytes> {
    if let Some(body) = prepared.http_body.get() {
        return Ok(body);
    }
    // A WebSocket-only request never has to encode an HTTP body. If it falls
    // back, all attempts and credential refreshes use the same encoded bytes.
    let body = super::serialize::RequestBody::http(prepared)?;
    let body = bytes::Bytes::from(serde_json::to_vec(&body)?);
    Ok(prepared.http_body.get_or_init(|| body))
}

pub(super) fn successful_http_model_response(
    source_profile: &zevria_foundation::ModelProfileRef,
    state: &mut AttemptState,
    usage: Option<zevria_model::TokenUsage>,
) -> anyhow::Result<ModelResponse> {
    let response_output = state.take_native_output().ok_or_else(|| {
        anyhow::anyhow!("OpenAI HTTP response completed without complete native output")
    })?;
    let replay = ProviderReplay::openai_responses(source_profile.clone(), response_output);
    let response = ModelResponse::from_replay(replay).map_err(|error| {
        error.context("failed to convert captured OpenAI HTTP output into an assistant message")
    })?;
    Ok(response.with_usage(usage))
}

pub(super) async fn run_http_model_request(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    refresh: &mut CredentialRefresh,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
    request_start_deadline: std::time::Duration,
) -> Result<ModelResponse, HttpAttemptError> {
    let progress = attempt_progress.reporter;
    state.attempt = progress
        .records_attempts()
        .then(|| crate::attempt::AttemptStream::new(openai.profile.clone(), progress));
    let result = run_http_model_request_inner(
        prepared,
        openai,
        state,
        refresh,
        policy,
        attempt_progress,
        request_start_deadline,
    )
    .await;
    state.flush_stream(progress);
    let result = match result {
        Ok(response) => response
            .with_attempt_id(
                state
                    .attempt
                    .as_ref()
                    .map(|attempt| attempt.id().to_owned()),
            )
            .map_err(HttpAttemptError::terminal),
        Err(error) => {
            if let Some(attempt) = &mut state.attempt {
                attempt
                    .close_failed(progress)
                    .await
                    .map_err(HttpAttemptError::terminal)?;
            }
            Err(error)
        }
    };
    Diagnostics::finish(prepared, openai, result.as_ref().ok());
    result
}

pub(super) async fn run_http_model_request_inner(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    refresh: &mut CredentialRefresh,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
    request_start_deadline: std::time::Duration,
) -> Result<ModelResponse, HttpAttemptError> {
    let progress = attempt_progress.reporter;
    state.reset_result();
    let request_attempt = attempt_progress.attempt;
    let mut telemetry = crate::telemetry::RequestTelemetry::new();
    let body = http_request_body(prepared).map_err(HttpAttemptError::terminal)?;

    tracing::info!(
        transport = OpenAiTransport::Http.as_str(),
        mode = RequestMode::Full.as_str(),
        attempt = request_attempt,
        history_message_count = prepared.history_message_count,
        complete_input_item_count = prepared.full_input.len(),
        replay_source = prepared.replay_source.as_str(),
        prompt_cache_key_fingerprint = ?openai.transmitted_prompt_cache_key_fingerprint(),
        "OpenAI HTTP request"
    );

    let request = openai
        .http
        .post(openai.responses_url.clone())
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone());
    Diagnostics::http_transmission(prepared, openai, &request, request_attempt)?;
    let response = refresh
        .exchange(
            &openai.api_key,
            &openai.profile.provider,
            |lease| {
                let request = request
                    .try_clone()
                    .expect("JSON request is cloneable")
                    .bearer_auth(lease.expose())
                    .send();
                async move {
                    match attempt_progress
                        .request_start(policy, request_start_deadline, request)
                        .await
                    {
                        Ok(Ok(response)) => Ok(response),
                        Ok(Err(error)) => Err(HttpAttemptError::retryable(
                            "request_error",
                            anyhow::Error::new(error.without_url())
                                .context("OpenAI HTTP request failed"),
                        )),
                        Err(error) => Err(HttpAttemptError::retryable("request_timeout", error)),
                    }
                }
            },
            http_unauthorized,
        )
        .await
        .map_err(|error| {
            if error.is_transient() {
                HttpAttemptError::retryable("credential_helper", error)
            } else {
                HttpAttemptError::terminal(error)
            }
        })??;

    let upstream_request_id = response_request_id(response.headers());
    let status = response.status();
    let hint = retry_hint(response.headers());
    if !status.is_success() {
        log_terminal_response(
            openai,
            TerminalResponse {
                transport: OpenAiTransport::Http,
                request_mode: RequestMode::Full,
                retry_number: Some(request_attempt.saturating_sub(1)),
                socket_generation: None,
                replay_source: prepared.replay_source,
                response_status: status.as_str(),
                response_id: None,
                upstream_request_id: upstream_request_id.as_deref(),
                usage: None,
                cache_write_tokens: None,
                telemetry: &telemetry,
            },
        );
        let detail = tokio::time::timeout(policy.send_timeout, bounded_http_error_body(response))
            .await
            .unwrap_or_else(|_| "<error body read timed out>".to_string());
        let structured = serde_json::from_str::<serde_json::Value>(&detail).ok();
        let size_rejection = structured.as_ref().is_some_and(structured_input_size);
        let body_rejection = status == reqwest::StatusCode::PAYLOAD_TOO_LARGE;
        let validation_rejection = structured
            .as_ref()
            .is_some_and(validation_input_size_rejection);
        let insufficient_quota = structured.as_ref().is_some_and(|value| {
            ["code", "type"]
                .iter()
                .any(|key| value["error"][key].as_str() == Some("insufficient_quota"))
        });
        let error = CompletionError::from_http_response(status, detail)
            .with_provider_request_id(upstream_request_id.clone());
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let error = anyhow::Error::from(error);
            return Err(HttpAttemptError::Terminal {
                error: if openai.api_key.is_refreshable() {
                    error.context(rejected_after_refresh(&openai.profile.provider))
                } else {
                    error
                },
                upstream_request_id,
            });
        }
        if size_rejection {
            return Err(HttpAttemptError::Terminal {
                error: if request_attempt == 1
                    && validation_rejection
                    && matches!(status.as_u16(), 400 | 413 | 422)
                {
                    zevria_model::ModelInputTooLarge::pre_execution(error).into()
                } else {
                    classify_input_size(true, error.into())
                },
                upstream_request_id,
            });
        }
        if body_rejection {
            return Err(HttpAttemptError::Terminal {
                error: zevria_model::ModelRequestBodyTooLarge(error.into()).into(),
                upstream_request_id,
            });
        }
        let category = match status.as_u16() {
            408 => Some("request_timeout_status"),
            429 if !insufficient_quota => Some("rate_limited"),
            500..=599 => Some("server_error"),
            _ => None,
        };
        let category = if status.as_u16() == 429 && insufficient_quota {
            None
        } else {
            match hint.should_retry {
                Some(false) => None,
                Some(true) => Some(category.unwrap_or("server_error")),
                None => category,
            }
        };
        return Err(HttpAttemptError::from_http_status(
            category,
            error,
            upstream_request_id,
            hint.delay,
        ));
    }

    let mut events = response.bytes_stream().eventsource();
    attempt_progress
        .status(NetworkStatus::AwaitingResponse)
        .await;
    let mut clock = ProgressClock::new(policy, attempt_progress);
    let response_progress = ResponseProgress::default();
    let mut driver = ResponseDriver::new(
        DriverTransport::Http {
            request_attempt,
            upstream_request_id: &upstream_request_id,
        },
        None,
        response_progress,
    );

    loop {
        let event = state
            .next_event(clock.next(events.next()), progress)
            .await
            .map_err(|error| {
                HttpAttemptError::retryable_response(
                    "response_idle_timeout",
                    error,
                    &upstream_request_id,
                )
            })?;

        let payload = match event {
            Some(Ok(event)) => event.data,
            Some(Err(EventStreamError::Transport(error))) => {
                return Err(HttpAttemptError::retryable_response(
                    "sse_transport_error",
                    anyhow::Error::new(error.without_url())
                        .context("OpenAI HTTP event stream failed"),
                    &upstream_request_id,
                ));
            }
            Some(Err(error)) => {
                return Err(HttpAttemptError::terminal_response(
                    anyhow::anyhow!("malformed OpenAI HTTP event stream: {error}"),
                    &upstream_request_id,
                ));
            }
            None => {
                return Err(HttpAttemptError::retryable_response(
                    "premature_eof",
                    anyhow::anyhow!(
                        "OpenAI HTTP event stream ended before a terminal response event"
                    ),
                    &upstream_request_id,
                ));
            }
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }

        let raw: serde_json::Value = serde_json::from_str(payload).map_err(|error| {
            HttpAttemptError::terminal_response(CompletionError::from(error), &upstream_request_id)
        })?;
        if let Some(response) = driver
            .process(
                DriverContext {
                    openai,
                    state,
                    prepared,
                    progress,
                    telemetry: &mut telemetry,
                    clock: &mut clock,
                },
                raw,
                payload.len(),
            )
            .await
            .map_err(DriverFailure::into_http)?
        {
            return Ok(response);
        }
    }
}
