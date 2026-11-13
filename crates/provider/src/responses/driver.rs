//! Responses driver responsibilities.

use crate::diagnostics::Diagnostics;
use crate::{
    connection::{OpenAiProvider, OpenAiTransport, log_identifier},
    replay::ReplaySource,
};

use crate::accumulator::AttemptState;
use crate::http::{HttpAttemptError, diagnostic_detail};
use crate::recovery::{AttemptProgress, ProgressClock};
use crate::telemetry::RequestTelemetry;
use crate::websocket_session::OpenAiWebSocketTerminalCategory;
use rig_core::error::ProviderError as CompletionError;
use zevria_model::{ModelResponse, TokenUsage, openai::ResponseStatus};
use zevria_responses::{
    accumulator::{response_cache_write_tokens_value, response_token_usage},
    protocol::{ProgressObservation, ResponseProgress, parse_server_event_value},
    wire::{
        CompletionResponse as OpenAiCompletionResponse, ErrorEvent, ResponseChunkKind,
        ResponsesWebSocketEvent,
    },
};
use zevria_session_api::ProgressReporter;

#[derive(Debug, Clone, Copy)]
pub(super) enum RequestMode {
    Incremental,
    Full,
}

impl RequestMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

pub(super) struct TerminalResponse<'a> {
    pub(super) transport: OpenAiTransport,
    pub(super) request_mode: RequestMode,
    pub(super) retry_number: Option<usize>,
    pub(super) socket_generation: Option<u64>,
    pub(super) replay_source: ReplaySource,
    pub(super) response_status: &'a str,
    pub(super) response_id: Option<&'a str>,
    pub(super) upstream_request_id: Option<&'a str>,
    pub(super) usage: Option<zevria_model::TokenUsage>,
    pub(super) cache_write_tokens: Option<u64>,
    pub(super) telemetry: &'a crate::telemetry::RequestTelemetry,
}

pub(super) fn log_terminal_response(openai: &mut OpenAiProvider, terminal: TerminalResponse<'_>) {
    let TerminalResponse {
        transport,
        request_mode,
        retry_number,
        socket_generation,
        replay_source,
        response_status,
        response_id,
        upstream_request_id,
        usage,
        cache_write_tokens,
        telemetry,
    } = terminal;
    Diagnostics::terminal(
        openai,
        transport,
        response_status,
        response_id,
        upstream_request_id,
        usage,
        cache_write_tokens,
    );
    let prompt_cache_key_fingerprint = openai.transmitted_prompt_cache_key_fingerprint();
    let websocket_connection_request_id = (transport == OpenAiTransport::WebSocket)
        .then_some(openai.ws.websocket_connection_request_id.as_deref())
        .flatten();
    tracing::info!(
        provider = %openai.profile.provider,
        model = %openai.profile.model,
        transport = transport.as_str(),
        request_mode = request_mode.as_str(),
        retry_number = ?retry_number,
        socket_generation = ?socket_generation,
        replay_source = replay_source.as_str(),
        response_status,
        request_ms = telemetry.request_ms(),
        first_event_ms = ?telemetry.first_event_ms(),
        function_calls = telemetry.function_calls(),
        response_id = ?response_id.map(log_identifier),
        upstream_request_id = ?upstream_request_id.map(log_identifier),
        websocket_connection_request_id = ?websocket_connection_request_id.map(log_identifier),
        prompt_cache_key_fingerprint = ?prompt_cache_key_fingerprint,
        input_tokens_present = usage.is_some(),
        input_tokens = usage.map_or(0, |usage| usage.input_tokens),
        cached_tokens = usage.map_or(0, |usage| usage.cached_tokens),
        cache_write_tokens_present = cache_write_tokens.is_some(),
        cache_write_tokens = cache_write_tokens.unwrap_or(0),
        output_tokens = usage.map_or(0, |usage| usage.output_tokens),
        total_tokens = usage.map_or(0, |usage| usage.total_tokens),
        "OpenAI terminal response"
    );
}

use super::{
    continuation::successful_model_response,
    errors::{
        StaleResponseId, TransientResponseError, WebSocketDisconnected, classify_input_size,
        done_failure_error, error_reports_missing_previous_response,
        error_reports_websocket_connection_limit, input_size_code,
        message_reports_upstream_disconnect, response_attempt_error, response_failure_error,
        transient_response_code,
    },
    http::successful_http_model_response,
    prepare::PreparedRequest,
    websocket::abandon_attempt,
};
/// These differences are transport policy, not codec duplication.
#[derive(Clone, Copy)]
pub(super) enum DriverTransport<'a> {
    WebSocket {
        request_mode: RequestMode,
        socket_generation: u64,
        previous_response: bool,
    },
    Http {
        request_attempt: usize,
        upstream_request_id: &'a Option<String>,
    },
}

pub(super) enum DriverFailure {
    WebSocket(anyhow::Error),
    Http(HttpAttemptError),
}
impl DriverFailure {
    pub(super) fn into_websocket(self) -> anyhow::Error {
        match self {
            Self::WebSocket(error) => error,
            Self::Http(_) => unreachable!("HTTP failure in WebSocket driver"),
        }
    }
    pub(super) fn into_http(self) -> HttpAttemptError {
        match self {
            Self::Http(error) => error,
            Self::WebSocket(_) => unreachable!("WebSocket failure in HTTP driver"),
        }
    }
}

pub(super) struct DriverContext<'a> {
    pub(super) openai: &'a mut OpenAiProvider,
    pub(super) state: &'a mut AttemptState,
    pub(super) prepared: &'a PreparedRequest,
    pub(super) progress: &'a ProgressReporter,
    pub(super) telemetry: &'a mut RequestTelemetry,
    pub(super) clock: &'a mut ProgressClock,
    pub(super) attempt_progress: AttemptProgress<'a>,
}

pub(super) struct ResponseDriver<'a> {
    transport: DriverTransport<'a>,
    response_progress: ResponseProgress,
    prior_terminal_id: crate::diagnostics::ObserverDiagnostics,
}
impl<'a> ResponseDriver<'a> {
    pub(super) fn new(
        transport: DriverTransport<'a>,
        _prior_terminal_id: Option<String>,
        response_progress: ResponseProgress,
    ) -> Self {
        Self {
            transport,
            response_progress,
            prior_terminal_id: Diagnostics::observer(_prior_terminal_id),
        }
    }

    fn terminal(
        &self,
        ctx: &mut DriverContext<'_>,
        status: &str,
        id: Option<&str>,
        usage: Option<TokenUsage>,
        cache_write_tokens: Option<u64>,
    ) {
        let (transport, request_mode, retry_number, socket_generation, upstream_request_id) =
            match self.transport {
                DriverTransport::WebSocket {
                    request_mode,
                    socket_generation,
                    ..
                } => (
                    OpenAiTransport::WebSocket,
                    request_mode,
                    None,
                    Some(socket_generation),
                    None,
                ),
                DriverTransport::Http {
                    request_attempt,
                    upstream_request_id,
                } => (
                    OpenAiTransport::Http,
                    RequestMode::Full,
                    Some(request_attempt.saturating_sub(1)),
                    None,
                    upstream_request_id.as_deref(),
                ),
            };
        log_terminal_response(
            ctx.openai,
            TerminalResponse {
                transport,
                request_mode,
                retry_number,
                socket_generation,
                replay_source: ctx.prepared.replay_source,
                response_status: status,
                response_id: id,
                upstream_request_id,
                usage,
                cache_write_tokens,
                telemetry: ctx.telemetry,
            },
        );
    }

    fn terminal_error(&self, ctx: &mut DriverContext<'_>, error: anyhow::Error) -> DriverFailure {
        match self.transport {
            DriverTransport::WebSocket { .. } => {
                abandon_attempt(ctx.openai, ctx.state);
                DriverFailure::WebSocket(error)
            }
            DriverTransport::Http {
                upstream_request_id,
                ..
            } => DriverFailure::Http(HttpAttemptError::terminal_response(
                error,
                upstream_request_id,
            )),
        }
    }

    fn success(
        &self,
        ctx: &mut DriverContext<'_>,
        response_id: String,
        usage: Option<TokenUsage>,
        done: bool,
    ) -> Result<ModelResponse, DriverFailure> {
        match self.transport {
            DriverTransport::WebSocket { .. } => {
                ctx.openai.ws.pending_done_response_id = if done {
                    None
                } else {
                    Some(response_id.clone())
                };
                successful_model_response(ctx.openai, ctx.state, ctx.prepared, response_id, usage)
                    .map_err(DriverFailure::WebSocket)
            }
            DriverTransport::Http {
                upstream_request_id,
                ..
            } => successful_http_model_response(&ctx.openai.profile, ctx.state, usage).map_err(
                |error| {
                    DriverFailure::Http(HttpAttemptError::terminal_response(
                        error,
                        upstream_request_id,
                    ))
                },
            ),
        }
    }

    fn failed_terminal(
        &self,
        ctx: &mut DriverContext<'_>,
        id: Option<String>,
        done: bool,
        error: anyhow::Error,
    ) -> DriverFailure {
        match self.transport {
            DriverTransport::WebSocket { .. } => {
                if done {
                    abandon_attempt(ctx.openai, ctx.state);
                } else {
                    ctx.state.reset_result();
                }
                ctx.openai.ws.invalidate_continuation("response_failed");
                if !done {
                    ctx.openai.ws.pending_done_response_id = id;
                }
                DriverFailure::WebSocket(error)
            }
            DriverTransport::Http {
                upstream_request_id,
                ..
            } => {
                ctx.state.reset_result();
                ctx.openai.ws.invalidate_continuation("response_failed");
                DriverFailure::Http(response_attempt_error(error, upstream_request_id))
            }
        }
    }

    fn server_error(&self, ctx: &mut DriverContext<'_>, error: ErrorEvent) -> DriverFailure {
        if let DriverTransport::Http {
            upstream_request_id,
            ..
        } = self.transport
        {
            let message = diagnostic_detail(&error.to_string());
            let code = error.error.code.as_deref();
            let category = if input_size_code(code) {
                None
            } else if message_reports_upstream_disconnect(&message) {
                Some("upstream_disconnect")
            } else if transient_response_code(code) {
                Some(if code == Some("rate_limit_exceeded") {
                    "rate_limit_exceeded"
                } else {
                    "server_error"
                })
            } else {
                None
            };
            let error = classify_input_size(
                input_size_code(code),
                CompletionError::Provider(message).into(),
            );
            return DriverFailure::Http(match category {
                Some(category) => {
                    HttpAttemptError::retryable_response(category, error, upstream_request_id)
                }
                None => HttpAttemptError::terminal_response(error, upstream_request_id),
            });
        }
        // Size rejection still wins over disconnect prose, connection limits,
        // stale IDs and generic transient classifications on WebSocket.
        if input_size_code(error.error.code.as_deref()) {
            return self.terminal_error(
                ctx,
                classify_input_size(
                    true,
                    CompletionError::Provider(diagnostic_detail(&error.to_string())).into(),
                ),
            );
        }
        if error_reports_websocket_connection_limit(error.error.code.as_deref()) {
            let message = diagnostic_detail(&error.to_string());
            ctx.openai
                .ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::ConnectionLimit);
            abandon_attempt(ctx.openai, ctx.state);
            return DriverFailure::WebSocket(
                WebSocketDisconnected::from_server_reason(
                    "generating the response (the server reached its websocket connection limit)",
                    message,
                    "connection_limit",
                )
                .into(),
            );
        }
        if matches!(
            self.transport,
            DriverTransport::WebSocket {
                previous_response: true,
                ..
            }
        ) && error_reports_missing_previous_response(
            error.error.code.as_deref(),
            error.error.message.as_deref(),
        ) {
            ctx.openai.ws.invalidate_continuation("stale_response_id");
            abandon_attempt(ctx.openai, ctx.state);
            return DriverFailure::WebSocket(StaleResponseId.into());
        }
        abandon_attempt(ctx.openai, ctx.state);
        let message = diagnostic_detail(&error.to_string());
        if message_reports_upstream_disconnect(&message) {
            ctx.openai
                .ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::UpstreamDisconnect);
            return DriverFailure::WebSocket(
                WebSocketDisconnected::from_server_reason(
                    "generating the response (the server reported an upstream transport failure)",
                    message,
                    "upstream_disconnect",
                )
                .into(),
            );
        }
        if transient_response_code(error.error.code.as_deref()) {
            return DriverFailure::WebSocket(
                TransientResponseError {
                    category: if error.error.code.as_deref() == Some("rate_limit_exceeded") {
                        "rate_limit_exceeded"
                    } else {
                        "server_error"
                    },
                    source: CompletionError::Provider(message).into(),
                }
                .into(),
            );
        }
        DriverFailure::WebSocket(CompletionError::Provider(message).into())
    }

    /// One shared event sequence. Transport loops retain framing, watchdog reads,
    /// HTTP status/auth recovery, socket fencing setup and attempt lifetime.
    pub(super) async fn process(
        &mut self,
        mut ctx: DriverContext<'_>,
        raw: serde_json::Value,
        _payload_len: usize,
    ) -> Result<Option<ModelResponse>, DriverFailure> {
        Diagnostics::observe_value(
            ctx.openai,
            &raw,
            _payload_len,
            &self.prior_terminal_id,
            match self.transport {
                DriverTransport::Http {
                    upstream_request_id,
                    ..
                } => upstream_request_id.as_deref(),
                _ => None,
            },
        );
        ctx.telemetry.observe_event();
        let cache_write_tokens = response_cache_write_tokens_value(&raw);
        match self.response_progress.observe(&raw) {
            ProgressObservation::OtherResponse => return Ok(None),
            ProgressObservation::Advanced => {
                if matches!(self.transport, DriverTransport::WebSocket { .. })
                    && let Some(id) = raw["response"]["id"]
                        .as_str()
                        .or_else(|| raw["response_id"].as_str())
                {
                    ctx.state.response_ids.insert(id.to_owned());
                }
                ctx.clock.advanced(ctx.attempt_progress).await;
            }
            ProgressObservation::Unchanged => {}
        }
        ctx.telemetry.observe_openai(&raw);
        match raw["type"].as_str() {
            Some("response.output_item.done") => {
                ctx.state.native_output.record_output_item_done_value(&raw)
            }
            Some("response.completed" | "response.done") => {
                ctx.state.native_output.record_terminal_output_value(&raw)
            }
            _ => {}
        }
        ctx.state.observe_attempt_value(&raw);
        let event = parse_server_event_value(raw)
            .map_err(|error| self.terminal_error(&mut ctx, error.into()))?;
        let Some(event) = event else {
            ctx.state.publish_stream(ctx.progress);
            return Ok(None);
        };
        match event {
            ResponsesWebSocketEvent::Item(item) => {
                ctx.state
                    .result
                    .record_item(item)
                    .map_err(|error| self.terminal_error(&mut ctx, error))?;
                ctx.state.publish_stream(ctx.progress);
            }
            ResponsesWebSocketEvent::Response(chunk) => {
                let chunk = *chunk;
                match chunk.kind {
                    ResponseChunkKind::ResponseCompleted => {
                        let id = chunk.response.id.clone();
                        let usage = response_token_usage(&chunk.response);
                        self.terminal(&mut ctx, "completed", Some(&id), usage, cache_write_tokens);
                        return self.success(&mut ctx, id, usage, false).map(Some);
                    }
                    ResponseChunkKind::ResponseFailed | ResponseChunkKind::ResponseIncomplete => {
                        let id = chunk.response.id.clone();
                        let status = if matches!(chunk.kind, ResponseChunkKind::ResponseFailed) {
                            "failed"
                        } else {
                            "incomplete"
                        };
                        self.terminal(
                            &mut ctx,
                            status,
                            Some(&id),
                            response_token_usage(&chunk.response),
                            cache_write_tokens,
                        );
                        return Err(self.failed_terminal(
                            &mut ctx,
                            Some(id),
                            false,
                            response_failure_error(&chunk.response),
                        ));
                    }
                    ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => {}
                }
            }
            ResponsesWebSocketEvent::Error(error) => return Err(self.server_error(&mut ctx, error)),
            ResponsesWebSocketEvent::Done(done) => {
                let response_id = done.response_id().map(ToOwned::to_owned);
                let value = done.response.get("status").cloned().ok_or_else(|| {
                    self.terminal_error(
                        &mut ctx,
                        anyhow::anyhow!("OpenAI response.done did not include a response status"),
                    )
                })?;
                let status = serde_json::from_value::<ResponseStatus>(value)
                    .map_err(|error| self.terminal_error(&mut ctx, error.into()))?;
                match status {
                    ResponseStatus::Completed => {
                        let Some(id) = response_id else {
                            if matches!(self.transport, DriverTransport::Http { .. }) {
                                self.terminal(
                                    &mut ctx,
                                    "completed",
                                    None,
                                    None,
                                    cache_write_tokens,
                                );
                            }
                            let error = self.terminal_error(
                                &mut ctx,
                                anyhow::anyhow!(
                                    "OpenAI response.done did not include a response ID"
                                ),
                            );
                            if matches!(self.transport, DriverTransport::WebSocket { .. }) {
                                ctx.openai.ws.invalidate_continuation("response_failed");
                            }
                            return Err(error);
                        };
                        let response =
                            serde_json::from_value::<OpenAiCompletionResponse>(done.response).ok();
                        let usage = response.as_ref().and_then(response_token_usage);
                        self.terminal(&mut ctx, "completed", Some(&id), usage, cache_write_tokens);
                        return self.success(&mut ctx, id, usage, true).map(Some);
                    }
                    ResponseStatus::Failed
                    | ResponseStatus::Incomplete
                    | ResponseStatus::Cancelled => {
                        let status_name = match status {
                            ResponseStatus::Failed => "failed",
                            ResponseStatus::Incomplete => "incomplete",
                            _ => "cancelled",
                        };
                        let response = serde_json::from_value::<OpenAiCompletionResponse>(
                            done.response.clone(),
                        )
                        .ok();
                        self.terminal(
                            &mut ctx,
                            status_name,
                            Some(response_id.as_deref().unwrap_or("<missing response ID>")),
                            response.as_ref().and_then(response_token_usage),
                            cache_write_tokens,
                        );
                        return Err(self.failed_terminal(
                            &mut ctx,
                            response_id,
                            true,
                            done_failure_error(&done.response, &status),
                        ));
                    }
                    ResponseStatus::InProgress
                    | ResponseStatus::Queued
                    | ResponseStatus::Other(_) => {
                        return Err(self.terminal_error(
                            &mut ctx,
                            anyhow::anyhow!(
                                "OpenAI response.done carried non-terminal status {status:?}"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(None)
    }
}
