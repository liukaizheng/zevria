//! Stateless Messages HTTP/SSE runtime. The codec owns all wire semantics;
//! this module owns credentials, deadlines, recovery and publication.
use crate::{
    AnthropicAuth, AnthropicEndpoint, AnthropicModelConfig, ProfileProtocol, ResolvedModelProfile,
    connection::{resolve_input_token_count_url_with_suffix, session_routing_headers},
    http::{bounded_http_error_body, retry_hint},
    recovery::{
        AttemptFailure, AttemptProgress, Decision, ProgressClock, Recovery, RecoveryPolicy,
        failure_class,
    },
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use rig_agent::tool::server::ToolServerHandle;
use serde_json::{Value, json};
use std::time::Duration;
use zevria_anthropic::{
    body::{self, PreparedRequest, RequestOptions},
    error::{ApiError, ErrorKind},
    stream::{FinishedMessage, MessageAccumulator},
};
use zevria_foundation::{ModelProfileRef, ReasoningLevel};
use zevria_model::{
    CompactResult, InputTokenCount, ModelInputTooLarge, ModelRequest, ModelResponse, ProviderReplay,
};
use zevria_session_api::{
    CompactFuture, InputTokenCountFuture, ModelProvider, ProgressReporter, ProviderFuture,
    event::{NetworkStatus, NetworkTransport},
};

pub struct AnthropicProvider {
    profile: ModelProfileRef,
    endpoint: AnthropicEndpoint,
    model: AnthropicModelConfig,
    reasoning_level: ReasoningLevel,
    preamble: String,
    tools: ToolServerHandle,
    http: reqwest::Client,
    url: reqwest::Url,
    count_url: Option<reqwest::Url>,
    count_unsupported: bool,
    pub(crate) recovery: RecoveryPolicy,
}
impl AnthropicProvider {
    pub(crate) fn validate_profile_settings(
        profile: &ResolvedModelProfile,
        level: ReasoningLevel,
        session_id: &str,
    ) -> anyhow::Result<HeaderMap> {
        let ProfileProtocol::Anthropic { endpoint, model } = &profile.protocol else {
            anyhow::bail!(
                "profile {} is not an anthropic.messages profile",
                profile.profile
            );
        };
        anyhow::ensure!(
            profile.reasoning_levels.contains(&level)
                && !matches!(level, ReasoningLevel::None | ReasoningLevel::Minimal),
            "reasoning level {level} is not configured for {}",
            profile.profile
        );
        endpoint.validate(&profile.profile.provider)?;
        // The routing catalog already applied its configured compaction
        // percentage. Runtime setup revalidates absolute model constraints.
        model.validate(&profile.profile.provider, &profile.profile.model, 100)?;
        let url = reqwest::Url::parse(&endpoint.base_url)?;
        resolve_input_token_count_url_with_suffix(
            &url,
            &endpoint.input_token_count,
            "/count_tokens",
        )?;
        let mut headers = session_routing_headers(profile, session_id)?;
        let (name, auth) = match endpoint.auth {
            AnthropicAuth::ApiKey => ("x-api-key", endpoint.api_key.expose().to_owned()),
            AnthropicAuth::Bearer => (
                "authorization",
                format!("Bearer {}", endpoint.api_key.expose()),
            ),
        };
        let mut auth = HeaderValue::from_str(&auth).map_err(|_| {
            anyhow::anyhow!(
                "profile {} has an invalid credential header",
                profile.profile
            )
        })?;
        auth.set_sensitive(true);
        headers.insert(name, auth);
        headers.insert("anthropic-version", HeaderValue::from_static("2026-10-21"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        let betas = body::beta_headers(
            &endpoint.beta_headers,
            model.thinking_display,
            model.refusal_fallback,
            endpoint.compatibility.strict_thinking_binding,
        );
        if !betas.is_empty() {
            headers.insert("anthropic-beta", HeaderValue::from_str(&betas)?);
        }
        Ok(headers)
    }
    pub(crate) fn unconnected(
        profile: &ResolvedModelProfile,
        level: ReasoningLevel,
        preamble: &str,
        tools: ToolServerHandle,
        session_id: &str,
    ) -> anyhow::Result<Self> {
        let headers = Self::validate_profile_settings(profile, level, session_id)?;
        let ProfileProtocol::Anthropic { endpoint, model } = &profile.protocol else {
            unreachable!("validated Anthropic profile")
        };
        let recovery = RecoveryPolicy::from(&endpoint.network);
        let url = reqwest::Url::parse(&endpoint.base_url)?;
        let count_url = resolve_input_token_count_url_with_suffix(
            &url,
            &endpoint.input_token_count,
            "/count_tokens",
        )?;
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(recovery.connect_timeout)
            .build()?;
        Ok(Self {
            profile: profile.profile.clone(),
            endpoint: endpoint.clone(),
            model: model.clone(),
            reasoning_level: level,
            preamble: preamble.to_owned(),
            tools,
            http,
            url,
            count_url,
            count_unsupported: false,
            recovery,
        })
    }
    pub(crate) fn set_reasoning_level(&mut self, level: ReasoningLevel) {
        self.reasoning_level = level;
    }
    fn prepare(&self, request: &ModelRequest<'_>) -> anyhow::Result<PreparedRequest> {
        let catalog = self.tools.snapshot();
        let registry = catalog.definitions().iter().map(|tool| json!({"name":tool.name, "description":tool.description, "input_schema":tool.parameters})).collect::<Vec<_>>();
        body::build(
            request,
            &registry,
            &RequestOptions {
                target: &self.profile,
                max_tokens: self.model.max_output_tokens,
                effort: self.reasoning_level,
                display: self.model.thinking_display,
                fallback: self.model.refusal_fallback,
                compatibility: &self.endpoint.compatibility,
                additional_params: &self.endpoint.additional_params,
            },
        )
    }
    async fn attempt(
        &self,
        prepared: &PreparedRequest,
        progress: AttemptProgress<'_>,
        request_start_deadline: Duration,
    ) -> Result<(FinishedMessage, Option<String>), (ApiError, Option<String>)> {
        progress.status(NetworkStatus::Connecting).await;
        let response = progress
            .request_start(
                &self.recovery,
                request_start_deadline,
                self.http.post(self.url.clone()).json(&prepared.body).send(),
            )
            .await
            .map_err(|error| {
                (
                    ApiError::with_source(
                        ErrorKind::Retryable("request_start_timeout"),
                        "Anthropic request-start deadline expired before response headers",
                        error,
                    ),
                    None,
                )
            })?
            .map_err(|e| {
                (
                    ApiError::with_source(
                        ErrorKind::Retryable("transport"),
                        "Anthropic HTTP request failed",
                        e.without_url(),
                    ),
                    None,
                )
            })?;
        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|id| id.to_str().ok())
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let hint = retry_hint(response.headers());
            let detail = tokio::time::timeout(
                self.recovery.send_timeout,
                bounded_http_error_body(response),
            )
            .await
            .unwrap_or_else(|_| "<error body timed out>".into());
            return Err((
                zevria_anthropic::error::classify(
                    status,
                    &detail,
                    None,
                    hint.delay,
                    hint.should_retry,
                ),
                request_id,
            ));
        }
        progress.status(NetworkStatus::AwaitingResponse).await;
        let mut events = response.bytes_stream().eventsource();
        let mut accumulator = MessageAccumulator::default();
        let mut clock = ProgressClock::new(&self.recovery);
        loop {
            let event = clock.next(events.next(), progress).await.map_err(|e| {
                (
                    ApiError::new(ErrorKind::Retryable("response_idle_timeout"), e.to_string()),
                    request_id.clone(),
                )
            })?;
            let Some(event) = event else {
                break;
            };
            let event = event.map_err(|e| {
                (
                    match e {
                        eventsource_stream::EventStreamError::Transport(error) => {
                            ApiError::with_source(
                                ErrorKind::Retryable("stream"),
                                "Anthropic HTTP event stream failed",
                                error.without_url(),
                            )
                        }
                        error => ApiError::with_source(
                            ErrorKind::Retryable("stream"),
                            "Anthropic HTTP event stream failed",
                            error,
                        ),
                    },
                    request_id.clone(),
                )
            })?;
            if event.data.trim().is_empty() {
                continue;
            }
            let advanced = accumulator
                .push(&event.data)
                .map_err(|e| (e, request_id.clone()))?;
            if advanced {
                clock.advanced(progress).await;
                if let Some(message) = accumulator.preview() {
                    progress.reporter.stream_updated(message);
                }
            }
            if accumulator.is_stopped() {
                break;
            }
        }
        if accumulator.is_stopped() {
            let transformations = accumulator.input_transformations();
            if !transformations.is_null()
                && transformations != &json!([])
                && transformations != &json!({})
            {
                tracing::warn!(provider=%self.profile.provider, input_transformations=%transformations, "Anthropic transformed the input");
            }
            tracing::info!(provider=%self.profile.provider, model=%self.profile.model,
                transport="http", served_model=%accumulator.served_model, stop_reason=?accumulator.stop_reason(),
                upstream_request_id=?request_id, input_tokens=accumulator.usage.input_tokens,
                cache_creation=accumulator.usage.cache_creation_input_tokens, cache_read=accumulator.usage.cache_read_input_tokens,
                output_tokens=accumulator.usage.output_tokens, stripped_thinking_turns=prepared.stripped_thinking_turns,
                replay_source=prepared.replay_source, "Anthropic terminal response");
        }
        accumulator
            .finish()
            .map(|finished| (finished, request_id.clone()))
            .map_err(|e| (e, request_id))
    }
    #[tracing::instrument(skip_all, fields(profile = %self.profile))]
    async fn run(
        &mut self,
        request: ModelRequest<'_>,
        progress: ProgressReporter,
    ) -> anyhow::Result<ModelResponse> {
        let mut prepared = self.prepare(&request)?;
        let recovery = Recovery::new(&self.recovery);
        let mut stripped_retry = false;
        while let Some(plan) = recovery.start_attempt() {
            let attempt_progress = AttemptProgress {
                reporter: &progress,
                recovery: &recovery,
                attempt: plan.attempt,
                transport: NetworkTransport::Http,
            };
            attempt_progress.status(NetworkStatus::AttemptStarted).await;
            match self
                .attempt(&prepared, attempt_progress, plan.request_start_deadline)
                .await
            {
                Ok((finished, _request_id)) => {
                    let usage = finished.usage.canonical();
                    let response = ModelResponse::from_replay(ProviderReplay::anthropic_messages(
                        self.profile.clone(),
                        finished.blocks,
                        Some(prepared.prefix_binding),
                    ))?
                    .with_usage(Some(usage));
                    progress.usage_updated(usage).await;
                    return Ok(response);
                }
                Err((mut error, request_id)) => {
                    progress.stream_cleared();
                    let kind = error.kind;
                    let delay = error.retry_after;
                    if let Some(id) = &request_id {
                        error.message.push_str(&format!(" (request-id: {id})"));
                    }
                    let error: anyhow::Error = error.into();
                    if kind == ErrorKind::InputTooLarge {
                        return Err(ModelInputTooLarge::new(error).into());
                    }
                    let strip_thinking =
                        kind == ErrorKind::ThinkingBindingMismatch && !stripped_retry;
                    let category = match kind {
                        ErrorKind::Retryable(category) => category,
                        _ if strip_thinking => "thinking_binding_mismatch",
                        _ => return Err(error),
                    };
                    let last = AttemptFailure {
                        category,
                        class: failure_class(category),
                        error,
                        server_delay: if strip_thinking {
                            Some(Duration::ZERO)
                        } else {
                            delay
                        },
                        upstream_request_id: request_id,
                    };
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
                        return Err(recovery.exhausted_error(reason, last, &self.profile.provider));
                    };
                    if strip_thinking {
                        stripped_retry = true;
                        tracing::warn!(provider=%self.profile.provider, "Anthropic thinking binding mismatch; retrying once without historical thinking");
                        // Existing one-time semantic repair, not transient recovery.
                        prepared.stripped_thinking_turns += prepared.body["messages"]
                            .as_array()
                            .expect("prepared messages")
                            .iter()
                            .filter(|message| {
                                message["content"]
                                    .as_array()
                                    .expect("content")
                                    .iter()
                                    .any(zevria_anthropic::project::is_thinking)
                            })
                            .count();
                        prepared.prefix_binding =
                            zevria_anthropic::project::strip_thinking(&mut prepared.body);
                        body::place_cache_breakpoints(
                            &mut prepared.body,
                            self.endpoint.compatibility.prompt_caching,
                        );
                        zevria_anthropic::project::validate_messages(
                            prepared.body["messages"]
                                .as_array()
                                .expect("prepared messages"),
                        )?;
                    }
                    progress
                        .retrying(
                            next_attempt,
                            delay,
                            retrying_for,
                            format!("{}: {:#}", last.category, last.error),
                        )
                        .await;
                    tokio::time::sleep(delay).await;
                }
            }
        }
        anyhow::bail!("completion recovery requires a positive attempt cap")
    }
}
impl ModelProvider for AnthropicProvider {
    fn application_prompt(&self) -> &str {
        &self.preamble
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(self.run(request, progress))
    }
    fn compact<'a>(&'a mut self, _request: ModelRequest<'a>) -> CompactFuture<'a> {
        Box::pin(async { Ok(CompactResult::Unsupported) })
    }
    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            let Some(url) = self.count_url.clone().filter(|_| !self.count_unsupported) else {
                return Ok(InputTokenCount::Unsupported);
            };
            let body = body::count_tokens_body(&self.prepare(&request)?.body);
            tokio::time::timeout(
                Duration::from_secs(self.endpoint.input_token_count.request_timeout_seconds),
                async {
                    let response = self
                        .http
                        .post(url)
                        .header("accept", "application/json")
                        .json(&body)
                        .send()
                        .await
                        .map_err(reqwest::Error::without_url)?;
                    if matches!(response.status().as_u16(), 404 | 405 | 501) {
                        self.count_unsupported = true;
                        return Ok(InputTokenCount::Unsupported);
                    }
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let request_id = response
                            .headers()
                            .get("request-id")
                            .and_then(|id| id.to_str().ok())
                            .map(str::to_owned);
                        let detail = bounded_http_error_body(response).await;
                        let error = zevria_anthropic::error::classify(
                            status,
                            &detail,
                            request_id.as_deref(),
                            None,
                            None,
                        );
                        if error.kind == ErrorKind::InputTooLarge {
                            return Err(ModelInputTooLarge::new(error).into());
                        }
                        return Err(error.into());
                    }
                    let value: Value = response.json().await?;
                    Ok(InputTokenCount::Exact(
                        value["input_tokens"].as_u64().ok_or_else(|| {
                            anyhow::anyhow!("Anthropic count_tokens response omitted input_tokens")
                        })?,
                    ))
                },
            )
            .await
            .map_err(|_| anyhow::anyhow!("Anthropic count_tokens request timed out"))?
        })
    }
    fn reset(&mut self) {}
}
