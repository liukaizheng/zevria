//! Responses recovery loop responsibilities.
use crate::diagnostics::Diagnostics;
use crate::recovery::AttemptError;
use crate::recovery::{
    AttemptFailure, AttemptPlan, AttemptProgress, FailureClass, Recovery, RecoveryDriver,
    RecoveryPolicy, failure_class, run_recovery_loop,
};
use zevria_model::ModelRequest;
use zevria_model::ModelResponse;
use zevria_model::RetryCategory;
use zevria_session_api::ProgressReporter;
use zevria_session_api::event::{NetworkStatus, NetworkTransport};

use crate::{
    accumulator::AttemptState,
    connection::{
        HttpFallback, OpenAiProvider, OpenAiTransport, is_http_upgrade_required, log_identifier,
        retryable_connect_error,
    },
    credential::{CredentialRefresh, is_transient_helper_failure},
};

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
    let reprobe = openai.responses_transport.fallback() == Some(HttpFallback::Provisional);
    if reprobe {
        openai.responses_transport.prefer_websocket();
        tracing::info!("OpenAI WebSocket restored after a network-wide outage");
        Diagnostics::transport_restored(openai);
    }
    let provider = openai.profile.provider.clone();
    let mut attempt = ResponsesRecovery {
        prepared,
        openai,
        state,
        reprobe,
        policy,
        progress,
        refresh: CredentialRefresh::default(),
        websocket_attempts: 0,
        first_fallback_http: false,
        attempt_transport: OpenAiTransport::WebSocket,
    };
    run_recovery_loop(policy, Some(progress), &provider, &mut attempt).await
}

struct ResponsesRecovery<'a> {
    prepared: &'a PreparedRequest,
    openai: &'a mut OpenAiProvider,
    state: &'a mut AttemptState,
    policy: &'a RecoveryPolicy,
    progress: &'a ProgressReporter,
    refresh: CredentialRefresh,
    reprobe: bool,
    websocket_attempts: usize,
    // Only the first HTTP outcome commits a provisional fallback. A later
    // success after HTTP failed is evidence of a network-wide outage.
    first_fallback_http: bool,
    attempt_transport: OpenAiTransport,
}

impl RecoveryDriver for ResponsesRecovery<'_> {
    type Output = ModelResponse;

    async fn attempt<'a>(
        &'a mut self,
        plan: AttemptPlan,
        recovery: &'a Recovery<'a>,
    ) -> Result<ModelResponse, AttemptError> {
        let Self {
            prepared,
            openai,
            state,
            refresh,
            websocket_attempts,
            first_fallback_http,
            attempt_transport,
            policy,
            progress,
            ..
        } = self;

        let attempt = plan.attempt;
        let transport = openai.transport();
        *attempt_transport = transport;
        let attempt_progress = AttemptProgress {
            reporter: progress,
            recovery,
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
                refresh,
                policy,
                attempt_progress,
                plan.request_start_deadline,
            )
            .await
        } else {
            let connected = if openai
                .responses_transport
                .ws
                .session
                .terminal_status()
                .is_some()
            {
                Diagnostics::disconnected_recovery(prepared, openai);
                attempt_progress.status(NetworkStatus::Connecting).await;
                openai.responses_transport.ws.config.connect_timeout = policy.connect_timeout;
                let result = openai
                    .responses_transport
                    .ws
                    .reconnect_with_refresh(refresh)
                    .await;
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
                Err(error) if is_http_upgrade_required(&error) => Err(AttemptError::Retryable {
                    category: RetryCategory::WebsocketUpgradeRequired,
                    error,
                    server_delay: Some(std::time::Duration::ZERO),
                    upstream_request_id: None,
                }),
                Err(error) if is_transient_helper_failure(&error) => Err(AttemptError::retryable(
                    RetryCategory::CredentialHelper,
                    error,
                )),
                Err(error) if retryable_connect_error(&error) => {
                    Err(AttemptError::retryable(RetryCategory::ConnectError, error))
                }
                Err(error) => Err(AttemptError::terminal(error)),
                Ok(()) => {
                    run_model_request(prepared, openai, state, progress, policy, attempt_progress)
                        .await
                }
            }
        };
        match &result {
            Ok(_) => {
                if transport == OpenAiTransport::Http && *first_fallback_http {
                    openai.responses_transport.confirm_fallback();
                }
            }
            Err(AttemptError::Terminal {
                upstream_request_id,
                ..
            }) => {
                state.reset_result();
                tracing::debug!(profile = %openai.profile, attempt, transport = transport.as_str(),
                    upstream_request_id = ?upstream_request_id.as_deref().map(log_identifier), "completion ended with a terminal error");
            }
            Err(AttemptError::Retryable { category, .. }) => {
                // Credential failure consumes an attempt but says nothing about the transport.
                if failure_class(*category) != FailureClass::Credential {
                    match transport {
                        OpenAiTransport::WebSocket => *websocket_attempts += 1,
                        OpenAiTransport::Http => *first_fallback_http = false,
                    }
                }
                Diagnostics::recovery(prepared, openai);
                state.reset_result();
                openai
                    .responses_transport
                    .ws
                    .invalidate_continuation("attempt_failed");
                state.stale_replay = *category == RetryCategory::StaleResponseId;
                progress.stream_cleared();
                tracing::debug!(
                    transport = transport.as_str(),
                    since_last_completion_ms = openai
                        .responses_transport
                        .ws
                        .idle_for()
                        .as_millis()
                        .min(u64::MAX as u128)
                        as u64,
                    "completion recovery transport state"
                );
                // 426 is definitive even if the recovery budget is exhausted.
                if *category == RetryCategory::WebsocketUpgradeRequired {
                    openai.switch_to_http(category.as_str());
                    *first_fallback_http = true;
                }
            }
        }
        result
    }

    fn before_retry(&mut self, failure: &AttemptFailure) -> anyhow::Result<()> {
        if self.attempt_transport == OpenAiTransport::WebSocket
            && failure.class != FailureClass::Credential
            && failure.category != RetryCategory::WebsocketUpgradeRequired
            && self.websocket_attempts >= if self.reprobe { 1 } else { 2 }
        {
            self.openai.switch_to_http("websocket_retries_exhausted");
            self.first_fallback_http = true;
        }
        Ok(())
    }
}
