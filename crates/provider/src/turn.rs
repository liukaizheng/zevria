//! One OpenAI model request, response chaining, and bounded recovery.

use crate::http::{HttpAttemptError, bounded_http_error_body, diagnostic_detail, retry_hint};
use crate::recovery::{
    AttemptFailure, AttemptProgress, Decision, ProgressClock, Recovery, RecoveryPolicy,
    ResponseIdleTimeout, failure_class,
};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use rig_core::error::ProviderError as CompletionError;
#[cfg(test)]
use rig_core::message::Message;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};
use zevria_model::CompactResult;
use zevria_model::InputTokenCount;
use zevria_model::ModelInputTooLarge;
use zevria_model::ModelRequest;
use zevria_model::ModelResponse;
use zevria_model::ProviderReplay;
use zevria_model::openai::ResponseStatus;
use zevria_responses::protocol::{ProgressObservation, ResponseProgress};
use zevria_responses::tools::ToolRendererOptions;
use zevria_responses::wire::{
    CompletionResponse as OpenAiCompletionResponse, ResponseChunkKind, ResponsesWebSocketEvent,
};
use zevria_session_api::CompactFuture;
use zevria_session_api::InputTokenCountFuture;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;
use zevria_session_api::event::{NetworkStatus, NetworkTransport};

use crate::{
    accumulator::AttemptState,
    config::validate_additional_params,
    connection::{
        ContinuationState, HttpFallback, OpenAiProvider, OpenAiTransport, is_http_upgrade_required,
        log_identifier, response_request_id, retryable_connect_error,
    },
    credential::{authenticated, http_unauthorized, rejected_after_refresh},
    protocol::websocket_message_to_text,
    replay::ReplaySource,
    websocket_session::{OpenAiWebSocketInbound, OpenAiWebSocketTerminalCategory},
};
use zevria_responses::{
    accumulator::{response_cache_write_tokens_value, response_token_usage},
    protocol::parse_server_event_value,
};

/// Marks a turn that failed because a websocket connection was lost rather
/// than because the request itself was rejected — either our own transport, or
/// an upstream failure a relay reported in-band. The adapter uses this to
/// decide whether rebuilding the socket and replaying the prompt is worth
/// attempting.
#[derive(Debug)]
pub(crate) struct WebSocketDisconnected {
    context: &'static str,
    source: Option<WebSocketError>,
    server_reason: Option<String>,
    terminal_category: &'static str,
}

impl WebSocketDisconnected {
    fn new(context: &'static str, source: WebSocketError, terminal_category: &'static str) -> Self {
        Self {
            context,
            source: Some(source),
            server_reason: None,
            terminal_category,
        }
    }

    fn without_source(context: &'static str, terminal_category: &'static str) -> Self {
        Self {
            context,
            source: None,
            server_reason: None,
            terminal_category,
        }
    }

    fn from_server_reason(
        context: &'static str,
        server_reason: String,
        terminal_category: &'static str,
    ) -> Self {
        Self {
            context,
            source: None,
            server_reason: Some(diagnostic_detail(&server_reason)),
            terminal_category,
        }
    }

    fn from_terminal(context: &'static str, terminal: OpenAiWebSocketTerminalCategory) -> Self {
        Self::without_source(context, terminal.as_str())
    }

    fn terminal_category(&self) -> &'static str {
        self.terminal_category
    }
}

impl std::fmt::Display for WebSocketDisconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The OpenAI websocket connection was lost while {}",
            self.context
        )?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        if let Some(reason) = &self.server_reason {
            write!(f, ": {reason}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WebSocketDisconnected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Whether a failed turn was caused by a dropped websocket connection.
pub(crate) fn is_websocket_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<WebSocketDisconnected>().is_some()
}

/// Whether a server-sent failure message describes a dead transport rather
/// than a rejected request. Relays that proxy the Responses API report their
/// own upstream websocket dying in-band — for example gorilla/websocket's
/// "websocket: close 1006 (abnormal closure): unexpected EOF" — instead of
/// dropping our socket. Those turns are as recoverable as a local disconnect,
/// so they get the same reconnect-and-replay treatment. Anything else (quota,
/// context size, auth, ...) is a genuine rejection and must fail fast.
pub(crate) fn message_reports_upstream_disconnect(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "close 1006",
        "abnormal closure",
        "unexpected eof",
        "connection reset",
        "connection closed",
        "peer closed connection",
        "closed upstream",
        "requires http replay",
        "broken pipe",
    ]
    .iter()
    .any(|marker| message.contains(marker))
        || (message.contains("timed out")
            && ["upstream", "websocket", "connection"]
                .iter()
                .any(|marker| message.contains(marker)))
}

/// Whether a server error event says the chained `previous_response_id` no
/// longer exists server-side. OpenAI names the code
/// `previous_response_not_found`, but a relay may surface the same condition
/// under a generic code, so a clearly matching message is accepted too. A
/// false positive costs one full-history replay of a request that had
/// already failed — the same round trip a failed turn would force anyway.
pub(crate) fn error_reports_missing_previous_response(
    code: Option<&str>,
    message: Option<&str>,
) -> bool {
    if code == Some("previous_response_not_found") {
        return true;
    }
    let message = message.unwrap_or_default().to_ascii_lowercase();
    message.contains("previous response") || message.contains("no longer cached")
}

fn error_reports_websocket_connection_limit(code: Option<&str>) -> bool {
    code == Some("websocket_connection_limit_reached")
}

#[derive(Debug, Clone, Copy)]
enum RequestMode {
    Incremental,
    Full,
}

impl RequestMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn log_terminal_response(
    openai: &mut OpenAiProvider,
    transport: OpenAiTransport,
    request_mode: RequestMode,
    retry_number: Option<usize>,
    socket_generation: Option<u64>,
    replay_source: ReplaySource,
    response_status: &str,
    response_id: Option<&str>,
    upstream_request_id: Option<&str>,
    usage: Option<zevria_model::TokenUsage>,
    cache_write_tokens: Option<u64>,
) {
    #[cfg(feature = "cache-diagnostics")]
    crate::cache_diagnostics::terminal(
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

/// A complete logical request. Every retry and stale-ID fallback derives its
/// transmitted payload from this snapshot, so recovery cannot accidentally
/// rebuild a different history or tool set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContinuationLineage {
    input_count: usize,
    input_digest: [u8; 32],
    projected_prefix_len: usize,
}

pub(crate) struct ProjectionCache {
    profile: zevria_foundation::ModelProfileRef,
    compatibility: [bool; 3],
    input_count: usize,
    input_digest: [u8; 32],
    projection: zevria_responses::replay::InputProjection,
    input: std::sync::Arc<Vec<serde_json::Value>>,
}

pub(crate) struct PreparedRequest {
    #[cfg(feature = "cache-diagnostics")]
    pub(crate) cache_diagnostics: Option<crate::cache_diagnostics::RequestSnapshot>,
    pub(crate) request_properties: serde_json::Value,
    pub(crate) full_input: std::sync::Arc<Vec<serde_json::Value>>,
    input_count: usize,
    input_digest: [u8; 32],
    input_matches_continuation: bool,
    history_message_count: usize,
    replay_source: ReplaySource,
}

struct Transmission {
    #[cfg(feature = "cache-diagnostics")]
    full_reason: Option<crate::cache_diagnostics::FullReason>,
    previous_response_id: Option<String>,
    input: Vec<serde_json::Value>,
    mode: RequestMode,
}

pub(crate) async fn prepare_turn_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<PreparedRequest> {
    let tool_options = ToolRendererOptions {
        strict_tools: openai.compatibility.strict_tools,
    };
    let declarations = zevria_model::ToolDeclarations::new(model_request.tools.to_vec())?;
    let additions = model_request
        .input
        .iter()
        .filter_map(|item| match item {
            zevria_model::ModelRequestItem::AdditionalTools(tools) => Some(*tools),
            _ => None,
        })
        .collect::<Vec<_>>();
    let folded = if openai.compatibility.additional_tools {
        declarations
    } else {
        declarations.fold(additions.iter().copied())
    };
    let mut tools = zevria_responses::tools::render(&folded, tool_options)?;

    let search_advertised = openai.web_search.enabled
        && model_request.allowed_tool_names.is_none_or(|names| {
            names
                .iter()
                .any(|name| name == zevria_foundation::WEB_SEARCH_TOOL_NAME)
        });
    if search_advertised {
        tools.push(serde_json::to_value(openai.web_search.tool()?)?);
    }

    let mut request_properties = openai
        .prepared_responses_parameters()
        .unwrap_or_else(|| serde_json::json!({}));
    request_properties["model"] = openai.profile.model.clone().into();
    request_properties["instructions"] = model_request.instructions.into();
    if !tools.is_empty() {
        request_properties["tools"] = tools.into();
    }
    if openai.compatibility.send_store {
        request_properties["store"] = false.into();
    }
    let properties = request_properties
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the OpenAI request did not serialize as an object"))?;
    validate_additional_params(&openai.profile.provider, &openai.additional_params)?;
    for (key, value) in &openai.additional_params {
        properties.insert(key.clone(), value.clone());
    }
    properties.remove("input");
    properties.remove("previous_response_id");
    if model_request.forbid_tool_calls {
        properties.insert("tool_choice".into(), serde_json::json!("none"));
    } else {
        let mut declared = properties
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if openai.compatibility.additional_tools {
            for addition in &additions {
                declared.extend(zevria_responses::tools::render(addition, tool_options)?);
            }
        }
        validate_tool_choice(properties, &declared).map_err(|error| anyhow::anyhow!(
            "profile {}: providers.{}.additional_params.tool_choice conflicts with declared tools or hosted capabilities (check providers.{}.web_search.enabled and hosted-search task permissions): {error}",
            openai.profile, openai.profile.provider, openai.profile.provider
        ))?;
    }

    let fallback_identity;
    let identity = match model_request.input_identity {
        Some(identity) if identity.len() == model_request.input.len() => identity,
        _ => {
            fallback_identity = zevria_model::ModelInputIdentity::from_items(&model_request.input)?;
            &fallback_identity
        }
    };
    let input_matches_continuation = openai
        .ws
        .continuation
        .as_ref()
        .and_then(|continuation| continuation.lineage)
        .is_some_and(|lineage| {
            identity.len() > lineage.input_count
                && identity.prefix(lineage.input_count) == Some(lineage.input_digest)
        });
    let compatibility = [
        openai.compatibility.developer_messages,
        tool_options.strict_tools,
        openai.compatibility.additional_tools,
    ];
    let cached = openai.input_projection.take().filter(|cache| {
        cache.profile == openai.profile
            && cache.compatibility == compatibility
            && identity.prefix(cache.input_count) == Some(cache.input_digest)
    });
    let (mut projection, start, projection_extended) = match cached {
        Some(cache)
            if cache
                .projection
                .can_extend(&model_request.input[cache.input_count..], &openai.profile)? =>
        {
            let mut projection = cache.projection;
            projection.restore_input(
                std::sync::Arc::try_unwrap(cache.input)
                    .unwrap_or_else(|shared| shared.as_ref().clone()),
            );
            (projection, cache.input_count, true)
        }
        _ => (
            zevria_responses::replay::InputProjection::default(),
            0,
            false,
        ),
    };
    projection.extend(
        &model_request.input[start..],
        &openai.profile,
        openai.compatibility.developer_messages,
        tool_options,
        openai.compatibility.additional_tools,
    )?;
    let replay_source = projection.source();
    let full_input = std::sync::Arc::new(projection.take_input());
    anyhow::ensure!(
        !full_input.is_empty(),
        "the OpenAI Responses request contained no input items"
    );
    openai.input_projection = Some(ProjectionCache {
        profile: openai.profile.clone(),
        compatibility,
        input_count: identity.len(),
        input_digest: identity.digest(),
        projection,
        input: full_input.clone(),
    });
    Ok(PreparedRequest {
        #[cfg(feature = "cache-diagnostics")]
        cache_diagnostics: None,
        request_properties,
        full_input,
        input_count: identity.len(),
        input_digest: identity.digest(),
        // Rebuilding may rename portable call IDs to reserve new native
        // handles, or change projection compatibility. Never chain that prefix.
        input_matches_continuation: input_matches_continuation && projection_extended,
        history_message_count: model_request.input.len().saturating_sub(1),
        replay_source,
    })
}

fn validate_tool_choice(
    properties: &serde_json::Map<String, serde_json::Value>,
    tools: &[serde_json::Value],
) -> anyhow::Result<()> {
    let Some(choice) = properties.get("tool_choice") else {
        return Ok(());
    };
    let permitted = |choice: &serde_json::Value| {
        tools.iter().any(|tool| {
            tool.get("type") == choice.get("type")
                && (tool.get("type").and_then(serde_json::Value::as_str) != Some("function")
                    || tool.get("name") == choice.get("name"))
        })
    };
    match choice.as_str() {
        Some("auto" | "none") => return Ok(()),
        Some("required") => {
            anyhow::ensure!(
                !tools.is_empty(),
                "required tool choice has no permitted tools"
            );
            return Ok(());
        }
        _ => {}
    }
    if choice.get("type").and_then(serde_json::Value::as_str) == Some("allowed_tools") {
        let choices = choice
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("allowed_tools must contain tools"))?;
        anyhow::ensure!(
            !choices.is_empty() && choices.iter().all(permitted),
            "allowed_tools contains an unavailable or disallowed tool"
        );
    } else {
        anyhow::ensure!(
            choice.is_object() && permitted(choice),
            "forced tool is unavailable or disallowed"
        );
    }
    Ok(())
}

async fn count_full_input_tokens(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<InputTokenCount> {
    if openai.input_token_count_unsupported {
        return Ok(InputTokenCount::Unsupported);
    }
    let Some(url) = openai.input_token_count_url.clone() else {
        return Ok(InputTokenCount::Unsupported);
    };

    let prepared = prepare_turn_request(model_request, openai).await?;
    let mut body = prepared.request_properties;
    crate::prompt_cache::remove_comparison(&mut body);
    let properties = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the input-token request did not serialize as an object"))?;
    properties.remove("stream");
    properties.remove("previous_response_id");
    properties.remove("background");
    properties.remove("include");
    properties.remove("store");
    properties.remove("prompt_cache_key");
    properties.insert(
        "input".to_string(),
        serde_json::Value::Array(
            std::sync::Arc::try_unwrap(prepared.full_input)
                .unwrap_or_else(|shared| shared.as_ref().clone()),
        ),
    );

    let log_url = crate::connection::redacted_url_for_logging(url.as_str());
    let response = authenticated(
        &openai.api_key,
        &openai.profile.provider,
        |lease| {
            openai
                .http
                .post(url.clone())
                .bearer_auth(lease.expose())
                .timeout(openai.input_token_count_timeout)
                .json(&body)
                .send()
        },
        http_unauthorized,
    )
    .await?
    .map_err(|error| {
        anyhow::anyhow!(
            "OpenAI input-token request to {log_url} failed: {}",
            error.without_url()
        )
    })?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 405 | 501) {
        openai.input_token_count_unsupported = true;
        tracing::info!(url = %log_url, %status, "input-token counting is unsupported");
        return Ok(InputTokenCount::Unsupported);
    }
    if !status.is_success() {
        let detail = bounded_http_error_body(response).await;
        let error = anyhow::anyhow!(
            "OpenAI input-token request to {log_url} failed with HTTP {status}: {detail}"
        );
        return Err(
            if status == reqwest::StatusCode::UNAUTHORIZED && openai.api_key.is_refreshable() {
                error.context(rejected_after_refresh(&openai.profile.provider))
            } else {
                error
            },
        );
    }
    let value: serde_json::Value = response.json().await.map_err(|error| {
        anyhow::anyhow!(
            "malformed OpenAI input-token response from {log_url}: {}",
            error.without_url()
        )
    })?;
    let input_tokens = value
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "malformed OpenAI input-token response from {log_url}: missing nonnegative integer input_tokens"
            )
        })?;
    Ok(InputTokenCount::Exact(input_tokens))
}

async fn run_remote_compaction(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<CompactResult> {
    let Some(url) = openai.compaction_url.clone() else {
        return Ok(CompactResult::Unsupported);
    };
    let prepared = prepare_turn_request(model_request, openai).await?;
    let mut body = prepared.request_properties;
    crate::prompt_cache::remove_comparison(&mut body);
    let properties = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the compact request did not serialize as an object"))?;
    properties.remove("stream");
    properties.remove("previous_response_id");
    properties.remove("include");
    properties
        .entry("parallel_tool_calls".to_string())
        .or_insert(serde_json::Value::Bool(true));
    properties.insert(
        "input".to_string(),
        serde_json::Value::Array(
            std::sync::Arc::try_unwrap(prepared.full_input)
                .unwrap_or_else(|shared| shared.as_ref().clone()),
        ),
    );

    let log_url = crate::connection::redacted_url_for_logging(url.as_str());
    let response = authenticated(
        &openai.api_key,
        &openai.profile.provider,
        |lease| {
            openai
                .maintenance_http
                .post(url.clone())
                .bearer_auth(lease.expose())
                .timeout(openai.compaction_timeout)
                .json(&body)
                .send()
        },
        http_unauthorized,
    )
    .await?
    .map_err(|error| {
        anyhow::anyhow!(
            "OpenAI compaction request to {log_url} failed: {}",
            error.without_url()
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let detail = bounded_http_error_body(response).await;
        let error =
            anyhow::anyhow!("OpenAI compaction request failed with HTTP {status}: {detail}");
        return Err(
            if status == reqwest::StatusCode::UNAUTHORIZED && openai.api_key.is_refreshable() {
                error.context(rejected_after_refresh(&openai.profile.provider))
            } else {
                error
            },
        );
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| anyhow::anyhow!("malformed OpenAI compaction response: {error}"))?;
    let output = value
        .get("output")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("OpenAI compaction response omitted an output array"))?;
    if output.is_empty() {
        anyhow::bail!("OpenAI compaction response contained an empty output array");
    }
    if output.iter().any(|item| !item.is_object()) {
        anyhow::bail!("OpenAI compaction output items must be JSON objects");
    }
    let output = output
        .iter()
        .filter(|item| {
            item.get("role").and_then(serde_json::Value::as_str) != Some("developer")
                && item.get("type").and_then(serde_json::Value::as_str) != Some("additional_tools")
        })
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !output.is_empty(),
        "OpenAI compaction output was empty after filtering instruction and tool declarations"
    );
    let replay = ProviderReplay::openai_responses(openai.profile.clone(), output);
    Ok(CompactResult::Replacement(vec![
        zevria_model::OwnedModelRequestItem::replay_only(replay)?,
    ]))
}

fn select_transmission(prepared: &PreparedRequest, openai: &mut OpenAiProvider) -> Transmission {
    let decision = openai.ws.continuation.as_ref().map(|continuation| {
        if continuation.socket_generation != openai.ws.socket_generation {
            return Err("reconnect");
        }
        if !crate::prompt_cache::compatible(
            &continuation.request_properties,
            &prepared.request_properties,
        ) {
            return Err("request_properties_changed");
        }
        if continuation.lineage.is_none() && continuation.response_output.is_empty() {
            return Err("missing_native_output");
        }

        let prefix_len = continuation.lineage.map_or_else(
            || continuation.request_input.len() + continuation.response_output.len(),
            |lineage| lineage.projected_prefix_len,
        );
        let has_exact_prefix = if continuation.lineage.is_some() {
            prepared.input_matches_continuation && prepared.full_input.len() > prefix_len
        } else {
            prepared.full_input.len() > prefix_len
                && prepared.full_input.get(..continuation.request_input.len())
                    == Some(continuation.request_input.as_slice())
                && prepared
                    .full_input
                    .get(continuation.request_input.len()..prefix_len)
                    == Some(continuation.response_output.as_slice())
        };
        if !has_exact_prefix {
            return Err("history_mismatch");
        }

        Ok((
            continuation.response_id.clone(),
            prepared.full_input[prefix_len..].to_vec(),
        ))
    });

    match decision {
        Some(Ok((response_id, suffix))) => Transmission {
            #[cfg(feature = "cache-diagnostics")]
            full_reason: None,
            previous_response_id: Some(response_id),
            input: suffix,
            mode: RequestMode::Incremental,
        },
        Some(Err(reason)) => {
            openai.ws.invalidate_continuation(reason);
            Transmission {
                #[cfg(feature = "cache-diagnostics")]
                full_reason: Some(crate::cache_diagnostics::FullReason::selection(reason)),
                previous_response_id: None,
                input: prepared.full_input.as_ref().clone(),
                mode: RequestMode::Full,
            }
        }
        None => Transmission {
            #[cfg(feature = "cache-diagnostics")]
            full_reason: Some(crate::cache_diagnostics::no_continuation_reason(openai)),
            previous_response_id: None,
            input: prepared.full_input.as_ref().clone(),
            mode: RequestMode::Full,
        },
    }
}

async fn send_prepared_request(
    prepared: &PreparedRequest,
    transmission: &Transmission,
    openai: &mut OpenAiProvider,
    send_timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let mut payload = prepared
        .request_properties
        .as_object()
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("the prepared OpenAI request properties are not an object")
        })?;
    payload.insert(
        "type".to_string(),
        serde_json::Value::String("response.create".to_string()),
    );
    payload.insert(
        "input".to_string(),
        serde_json::Value::Array(transmission.input.clone()),
    );
    if let Some(previous_response_id) = &transmission.previous_response_id {
        payload.insert(
            "previous_response_id".to_string(),
            serde_json::Value::String(previous_response_id.clone()),
        );
    }
    let payload = serde_json::to_string(&payload)?;

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

    #[cfg(feature = "cache-diagnostics")]
    crate::cache_diagnostics::transmission(
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

fn full_transmission(prepared: &PreparedRequest) -> Transmission {
    Transmission {
        #[cfg(feature = "cache-diagnostics")]
        full_reason: Some(crate::cache_diagnostics::FullReason::StaleResponseId),
        previous_response_id: None,
        input: prepared.full_input.as_ref().clone(),
        mode: RequestMode::Full,
    }
}

fn successful_model_response(
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    prepared: &PreparedRequest,
    response_id: String,
    usage: Option<zevria_model::TokenUsage>,
) -> anyhow::Result<ModelResponse> {
    openai.ws.touch();
    let Some(response_output) = state.take_native_output() else {
        openai.ws.invalidate_continuation("missing_native_output");
        tracing::warn!(
            reason = "missing_native_output",
            socket_generation = openai.ws.socket_generation,
            "rejecting OpenAI response without complete native output"
        );
        anyhow::bail!("OpenAI response completed without complete native output");
    };

    let replay = ProviderReplay::openai_responses(openai.profile.clone(), response_output.clone());
    let response = match ModelResponse::from_replay(replay) {
        Ok(response) => response,
        Err(error) => {
            openai.ws.invalidate_continuation("invalid_native_output");
            tracing::warn!(
                reason = "invalid_native_output",
                socket_generation = openai.ws.socket_generation,
                error = %error,
                "rejecting OpenAI response whose native output cannot be converted"
            );
            return Err(
                error.context("failed to convert captured OpenAI output into an assistant message")
            );
        }
    };
    let response = response.with_usage(usage);
    openai.ws.continuation = Some(ContinuationState {
        socket_generation: openai.ws.socket_generation,
        response_id,
        request_properties: prepared.request_properties.clone(),
        lineage: Some(ContinuationLineage {
            input_count: prepared.input_count + 1,
            input_digest: zevria_model::ModelInputIdentity::appended_digest(
                prepared.input_digest,
                response.record().model_request_item(),
            )?,
            projected_prefix_len: prepared.full_input.len() + response_output.len(),
        }),
        request_input: Vec::new(),
        response_output: Vec::new(),
    });
    Ok(response)
}

/// Reset only the in-flight attempt and its late-`done` marker. A completed
/// continuation remains usable on this socket until a concrete invalidation
/// or a successful reconnect replaces the socket generation.
fn abandon_attempt(openai: &mut OpenAiProvider, state: &mut AttemptState) {
    state.reset_result();
    openai.ws.pending_done_response_id = None;
}

/// Only known structured input-size codes are retry signals for core. In
/// particular output exhaustion, quota/auth failures and prose are not codes.
fn input_size_code(code: Option<&str>) -> bool {
    matches!(code, Some("context_too_large" | "context_length_exceeded"))
}

fn transient_response_code(code: Option<&str>) -> bool {
    matches!(code, Some("server_error" | "rate_limit_exceeded"))
}

#[derive(Debug)]
struct TransientResponseError {
    category: &'static str,
    source: anyhow::Error,
}
impl std::fmt::Display for TransientResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}
impl std::error::Error for TransientResponseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn classify_response_failure(size: bool, transient: bool, error: anyhow::Error) -> anyhow::Error {
    if size {
        ModelInputTooLarge::new(error).into()
    } else if transient {
        TransientResponseError {
            category: "response_failed_transient",
            source: error,
        }
        .into()
    } else {
        error
    }
}

fn response_attempt_error(error: anyhow::Error, request_id: &Option<String>) -> HttpAttemptError {
    if let Some(transient) = error.downcast_ref::<TransientResponseError>() {
        HttpAttemptError::retryable_response(transient.category, error, request_id)
    } else {
        HttpAttemptError::terminal_response(error, request_id)
    }
}

fn classify_input_size(size_rejection: bool, error: anyhow::Error) -> anyhow::Error {
    if size_rejection {
        ModelInputTooLarge::new(error).into()
    } else {
        error
    }
}

fn structured_input_size(value: &serde_json::Value) -> bool {
    input_size_code(
        value
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str),
    ) || input_size_code(
        value
            .pointer("/incomplete_details/reason")
            .and_then(serde_json::Value::as_str),
    )
}

fn done_failure_error(response: &serde_json::Value, status: &ResponseStatus) -> anyhow::Error {
    let id = response
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing response ID>");
    let detail = response
        .get("error")
        .filter(|value| !value.is_null())
        .or_else(|| response.get("incomplete_details"));
    let detail = detail.map(|value| value.to_string()).unwrap_or_default();
    let detail = diagnostic_detail(&detail);
    classify_response_failure(
        matches!(status, ResponseStatus::Failed | ResponseStatus::Incomplete)
            && structured_input_size(response),
        matches!(status, ResponseStatus::Failed)
            && transient_response_code(
                response
                    .pointer("/error/code")
                    .and_then(serde_json::Value::as_str),
            ),
        anyhow::anyhow!("OpenAI response {id} ended with status {status:?}: {detail}"),
    )
}

fn response_failure_error(response: &OpenAiCompletionResponse) -> anyhow::Error {
    let detail = response
        .error
        .as_ref()
        .map(|error| format!("{}: {}", error.code, error.message))
        .or_else(|| {
            response
                .incomplete_details
                .as_ref()
                .map(|details| details.reason.clone())
        })
        .unwrap_or_else(|| "no failure details were provided".to_string());

    let detail = diagnostic_detail(&detail);
    classify_response_failure(
        matches!(
            response.status,
            ResponseStatus::Failed | ResponseStatus::Incomplete
        ) && (input_size_code(response.error.as_ref().map(|error| error.code.as_str()))
            || input_size_code(
                response
                    .incomplete_details
                    .as_ref()
                    .map(|details| details.reason.as_str()),
            )),
        matches!(response.status, ResponseStatus::Failed)
            && transient_response_code(response.error.as_ref().map(|error| error.code.as_str())),
        anyhow::anyhow!(
            "OpenAI response {} ended with status {:?}: {detail}",
            response.id,
            response.status
        ),
    )
}

async fn run_model_request(
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
    #[cfg(feature = "cache-diagnostics")]
    crate::cache_diagnostics::finish(prepared, openai, result.as_ref().ok());
    result
}

async fn run_model_request_inner(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
) -> anyhow::Result<ModelResponse> {
    state.reset_result();
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
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::observe_value(
            openai,
            &raw,
            message.len(),
            prior_terminal_id.as_deref(),
            None,
        );
        let cache_write_tokens = response_cache_write_tokens_value(&raw);
        match response_progress.observe(&raw) {
            ProgressObservation::OtherResponse => continue,
            ProgressObservation::Advanced => {
                if let Some(id) = raw["response"]["id"]
                    .as_str()
                    .or_else(|| raw["response_id"].as_str())
                {
                    state.response_ids.insert(id.to_string());
                }
                clock.advanced(attempt_progress).await;
            }
            ProgressObservation::Unchanged => {}
        }
        match raw["type"].as_str() {
            Some("response.output_item.done") => {
                state.native_output.record_output_item_done_value(&raw)
            }
            Some("response.completed" | "response.done") => {
                state.native_output.record_terminal_output_value(&raw)
            }
            _ => {}
        }
        state.observe_attempt_value(&raw);
        let event = match parse_server_event_value(raw) {
            Ok(event) => event,
            Err(error) => {
                abandon_attempt(openai, state);
                return Err(error.into());
            }
        };
        let Some(event) = event else {
            state.publish_stream(progress);
            continue;
        };

        match event {
            ResponsesWebSocketEvent::Item(item) => {
                if let Err(error) = state.result.record_item(item) {
                    abandon_attempt(openai, state);
                    return Err(error);
                }
                state.publish_stream(progress);
            }
            ResponsesWebSocketEvent::Response(chunk) => {
                let chunk = *chunk;
                match chunk.kind {
                    ResponseChunkKind::ResponseCompleted => {
                        let response_id = chunk.response.id.clone();
                        let token_usage = response_token_usage(&chunk.response);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::WebSocket,
                            request_mode,
                            None,
                            Some(openai.ws.socket_generation),
                            prepared.replay_source,
                            "completed",
                            Some(&response_id),
                            None,
                            token_usage,
                            cache_write_tokens,
                        );

                        if let Some(usage) = token_usage {
                            progress.usage_updated(usage).await;
                        }
                        openai.ws.pending_done_response_id = Some(response_id.clone());
                        return successful_model_response(
                            openai,
                            state,
                            prepared,
                            response_id,
                            token_usage,
                        );
                    }
                    ResponseChunkKind::ResponseFailed | ResponseChunkKind::ResponseIncomplete => {
                        let response_id = chunk.response.id.clone();
                        let token_usage = response_token_usage(&chunk.response);
                        let response_status = match chunk.kind {
                            ResponseChunkKind::ResponseFailed => "failed",
                            ResponseChunkKind::ResponseIncomplete => "incomplete",
                            _ => unreachable!("matched terminal failure chunk"),
                        };
                        log_terminal_response(
                            openai,
                            OpenAiTransport::WebSocket,
                            request_mode,
                            None,
                            Some(openai.ws.socket_generation),
                            prepared.replay_source,
                            response_status,
                            Some(&response_id),
                            None,
                            token_usage,
                            cache_write_tokens,
                        );
                        let error = response_failure_error(&chunk.response);
                        state.reset_result();
                        openai.ws.invalidate_continuation("response_failed");
                        openai.ws.pending_done_response_id = Some(response_id);
                        return Err(error);
                    }
                    ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => {}
                }
            }
            ResponsesWebSocketEvent::Error(error) => {
                // Size rejection wins even when a gateway adds disconnect prose.
                if input_size_code(error.error.code.as_deref()) {
                    abandon_attempt(openai, state);
                    return Err(classify_input_size(
                        true,
                        CompletionError::Provider(diagnostic_detail(&error.to_string())).into(),
                    ));
                }
                if error_reports_websocket_connection_limit(error.error.code.as_deref()) {
                    let message = diagnostic_detail(&error.to_string());
                    openai
                        .ws
                        .session
                        .terminate(OpenAiWebSocketTerminalCategory::ConnectionLimit);
                    abandon_attempt(openai, state);
                    return Err(WebSocketDisconnected::from_server_reason(
                                "generating the response (the server reached its websocket connection limit)",
                                message,
                                "connection_limit",
                            )
                            .into());
                }

                if previous_response_id.is_some()
                    && error_reports_missing_previous_response(
                        error.error.code.as_deref(),
                        error.error.message.as_deref(),
                    )
                {
                    openai.ws.invalidate_continuation("stale_response_id");
                    abandon_attempt(openai, state);
                    // Finalize this attempt before the shared controller decides
                    // whether a full-input replay is permitted.
                    return Err(StaleResponseId.into());
                }

                abandon_attempt(openai, state);
                let message = diagnostic_detail(&error.to_string());
                if message_reports_upstream_disconnect(&message) {
                    openai
                        .ws
                        .session
                        .terminate(OpenAiWebSocketTerminalCategory::UpstreamDisconnect);
                    return Err(WebSocketDisconnected::from_server_reason(
                                "generating the response (the server reported an upstream transport failure)",
                                message,
                                "upstream_disconnect",
                            )
                            .into());
                }
                if transient_response_code(error.error.code.as_deref()) {
                    return Err(TransientResponseError {
                        category: if error.error.code.as_deref() == Some("rate_limit_exceeded") {
                            "rate_limit_exceeded"
                        } else {
                            "server_error"
                        },
                        source: CompletionError::Provider(message).into(),
                    }
                    .into());
                }
                return Err(CompletionError::Provider(message).into());
            }
            ResponsesWebSocketEvent::Done(done) => {
                let response_id = done.response_id().map(ToOwned::to_owned);

                let status = match done.response.get("status").cloned() {
                    Some(status) => match serde_json::from_value::<ResponseStatus>(status) {
                        Ok(status) => status,
                        Err(error) => {
                            abandon_attempt(openai, state);
                            return Err(error.into());
                        }
                    },
                    None => {
                        abandon_attempt(openai, state);
                        return Err(anyhow::anyhow!(
                            "OpenAI response.done did not include a response status"
                        ));
                    }
                };

                match status {
                    ResponseStatus::Completed => {
                        let Some(response_id) = response_id else {
                            abandon_attempt(openai, state);
                            openai.ws.invalidate_continuation("response_failed");
                            return Err(anyhow::anyhow!(
                                "OpenAI response.done did not include a response ID"
                            ));
                        };
                        let response =
                            serde_json::from_value::<OpenAiCompletionResponse>(done.response).ok();
                        let token_usage = response.as_ref().and_then(response_token_usage);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::WebSocket,
                            request_mode,
                            None,
                            Some(openai.ws.socket_generation),
                            prepared.replay_source,
                            "completed",
                            Some(&response_id),
                            None,
                            token_usage,
                            cache_write_tokens,
                        );
                        if let Some(usage) = token_usage {
                            progress.usage_updated(usage).await;
                        }
                        openai.ws.pending_done_response_id = None;
                        return successful_model_response(
                            openai,
                            state,
                            prepared,
                            response_id,
                            token_usage,
                        );
                    }
                    ResponseStatus::Failed
                    | ResponseStatus::Incomplete
                    | ResponseStatus::Cancelled => {
                        let response_id = response_id.as_deref().unwrap_or("<missing response ID>");
                        let response_status = match status {
                            ResponseStatus::Failed => "failed",
                            ResponseStatus::Incomplete => "incomplete",
                            ResponseStatus::Cancelled => "cancelled",
                            _ => unreachable!("matched terminal status"),
                        };
                        let response = serde_json::from_value::<OpenAiCompletionResponse>(
                            done.response.clone(),
                        )
                        .ok();
                        let token_usage = response.as_ref().and_then(response_token_usage);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::WebSocket,
                            request_mode,
                            None,
                            Some(openai.ws.socket_generation),
                            prepared.replay_source,
                            response_status,
                            Some(response_id),
                            None,
                            token_usage,
                            cache_write_tokens,
                        );
                        abandon_attempt(openai, state);
                        openai.ws.invalidate_continuation("response_failed");
                        return Err(done_failure_error(&done.response, &status));
                    }
                    ResponseStatus::InProgress
                    | ResponseStatus::Queued
                    | ResponseStatus::Other(_) => {
                        abandon_attempt(openai, state);
                        return Err(anyhow::anyhow!(
                            "OpenAI response.done carried non-terminal status {status:?}"
                        ));
                    }
                }
            }
        }
    }
}

fn http_request_body(prepared: &PreparedRequest) -> anyhow::Result<serde_json::Value> {
    let mut body = prepared
        .request_properties
        .as_object()
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("the prepared OpenAI request properties are not an object")
        })?;
    body.remove("type");
    body.remove("previous_response_id");
    body.insert(
        "input".to_string(),
        serde_json::Value::Array(prepared.full_input.as_ref().clone()),
    );
    body.insert("stream".to_string(), serde_json::Value::Bool(true));
    Ok(serde_json::Value::Object(body))
}

fn successful_http_model_response(
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

async fn run_http_model_request(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
    request_start_deadline: std::time::Duration,
) -> Result<ModelResponse, HttpAttemptError> {
    state.attempt = progress
        .records_attempts()
        .then(|| crate::attempt::AttemptStream::new(openai.profile.clone(), progress));
    let result = run_http_model_request_inner(
        prepared,
        openai,
        state,
        progress,
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
    #[cfg(feature = "cache-diagnostics")]
    crate::cache_diagnostics::finish(prepared, openai, result.as_ref().ok());
    result
}

async fn run_http_model_request_inner(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    attempt_progress: AttemptProgress<'_>,
    request_start_deadline: std::time::Duration,
) -> Result<ModelResponse, HttpAttemptError> {
    state.reset_result();
    let request_attempt = attempt_progress.attempt;
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
        .json(&body);
    #[cfg(feature = "cache-diagnostics")]
    {
        let diagnostic_request = request
            .try_clone()
            .expect("JSON request is cloneable")
            .build()
            .map_err(|error| {
                HttpAttemptError::retryable(
                    "request_error",
                    anyhow::Error::new(error.without_url()).context("OpenAI HTTP request failed"),
                )
            })?;
        crate::cache_diagnostics::transmission(
            prepared,
            openai,
            "full",
            prepared.full_input.len(),
            None,
            Some(crate::cache_diagnostics::http_reason(
                openai,
                request_attempt,
            )),
            diagnostic_request.body().and_then(reqwest::Body::as_bytes),
        );
    }
    let response = authenticated(
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
    .map_err(HttpAttemptError::terminal)??;

    let upstream_request_id = response_request_id(response.headers());
    let status = response.status();
    let hint = retry_hint(response.headers());
    if !status.is_success() {
        log_terminal_response(
            openai,
            OpenAiTransport::Http,
            RequestMode::Full,
            Some(request_attempt.saturating_sub(1)),
            None,
            prepared.replay_source,
            status.as_str(),
            None,
            upstream_request_id.as_deref(),
            None,
            None,
        );
        let detail = tokio::time::timeout(policy.send_timeout, bounded_http_error_body(response))
            .await
            .unwrap_or_else(|_| "<error body read timed out>".to_string());
        let size_rejection = status == reqwest::StatusCode::PAYLOAD_TOO_LARGE
            || serde_json::from_str::<serde_json::Value>(&detail)
                .is_ok_and(|value| structured_input_size(&value));
        let insufficient_quota =
            serde_json::from_str::<serde_json::Value>(&detail).is_ok_and(|value| {
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
                error: classify_input_size(true, error.into()),
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
    let mut clock = ProgressClock::new(policy);
    let mut response_progress = ResponseProgress::default();

    loop {
        let event = state
            .next_event(clock.next(events.next(), attempt_progress), progress)
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
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::observe_value(
            openai,
            &raw,
            payload.len(),
            None,
            upstream_request_id.as_deref(),
        );
        let cache_write_tokens = response_cache_write_tokens_value(&raw);
        match response_progress.observe(&raw) {
            ProgressObservation::OtherResponse => continue,
            ProgressObservation::Advanced => clock.advanced(attempt_progress).await,
            ProgressObservation::Unchanged => {}
        }
        match raw["type"].as_str() {
            Some("response.output_item.done") => {
                state.native_output.record_output_item_done_value(&raw)
            }
            Some("response.completed" | "response.done") => {
                state.native_output.record_terminal_output_value(&raw)
            }
            _ => {}
        }
        state.observe_attempt_value(&raw);
        let event = parse_server_event_value(raw).map_err(|error| {
            HttpAttemptError::terminal_response(anyhow::Error::from(error), &upstream_request_id)
        })?;
        let Some(event) = event else {
            state.publish_stream(progress);
            continue;
        };

        match event {
            ResponsesWebSocketEvent::Item(item) => {
                state.result.record_item(item).map_err(|error| {
                    HttpAttemptError::terminal_response(error, &upstream_request_id)
                })?;
                state.publish_stream(progress);
            }
            ResponsesWebSocketEvent::Response(chunk) => {
                let chunk = *chunk;
                match chunk.kind {
                    ResponseChunkKind::ResponseCompleted => {
                        let response_id = chunk.response.id.clone();
                        let token_usage = response_token_usage(&chunk.response);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::Http,
                            RequestMode::Full,
                            Some(request_attempt.saturating_sub(1)),
                            None,
                            prepared.replay_source,
                            "completed",
                            Some(&response_id),
                            upstream_request_id.as_deref(),
                            token_usage,
                            cache_write_tokens,
                        );
                        if let Some(usage) = token_usage {
                            progress.usage_updated(usage).await;
                        }
                        return successful_http_model_response(&openai.profile, state, token_usage)
                            .map_err(|error| {
                                HttpAttemptError::terminal_response(error, &upstream_request_id)
                            });
                    }
                    ResponseChunkKind::ResponseFailed | ResponseChunkKind::ResponseIncomplete => {
                        let response_id = chunk.response.id.clone();
                        let response_status = match chunk.kind {
                            ResponseChunkKind::ResponseFailed => "failed",
                            ResponseChunkKind::ResponseIncomplete => "incomplete",
                            _ => unreachable!("matched terminal failure chunk"),
                        };
                        let token_usage = response_token_usage(&chunk.response);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::Http,
                            RequestMode::Full,
                            Some(request_attempt.saturating_sub(1)),
                            None,
                            prepared.replay_source,
                            response_status,
                            Some(&response_id),
                            upstream_request_id.as_deref(),
                            token_usage,
                            cache_write_tokens,
                        );
                        state.reset_result();
                        openai.ws.invalidate_continuation("response_failed");
                        return Err(response_attempt_error(
                            response_failure_error(&chunk.response),
                            &upstream_request_id,
                        ));
                    }
                    ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => {}
                }
            }
            ResponsesWebSocketEvent::Error(error) => {
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
                return Err(match category {
                    Some(category) => {
                        HttpAttemptError::retryable_response(category, error, &upstream_request_id)
                    }
                    None => HttpAttemptError::terminal_response(error, &upstream_request_id),
                });
            }
            ResponsesWebSocketEvent::Done(done) => {
                let response_id = done.response_id().map(ToOwned::to_owned);
                let status = done
                    .response
                    .get("status")
                    .cloned()
                    .ok_or_else(|| {
                        HttpAttemptError::terminal_response(
                            anyhow::anyhow!(
                                "OpenAI response.done did not include a response status"
                            ),
                            &upstream_request_id,
                        )
                    })
                    .and_then(|status| {
                        serde_json::from_value::<ResponseStatus>(status).map_err(|error| {
                            HttpAttemptError::terminal_response(error, &upstream_request_id)
                        })
                    })?;
                match status {
                    ResponseStatus::Completed => {
                        if response_id.is_none() {
                            log_terminal_response(
                                openai,
                                OpenAiTransport::Http,
                                RequestMode::Full,
                                Some(request_attempt.saturating_sub(1)),
                                None,
                                prepared.replay_source,
                                "completed",
                                None,
                                upstream_request_id.as_deref(),
                                None,
                                cache_write_tokens,
                            );
                            return Err(HttpAttemptError::terminal_response(
                                anyhow::anyhow!(
                                    "OpenAI response.done did not include a response ID"
                                ),
                                &upstream_request_id,
                            ));
                        }
                        let response =
                            serde_json::from_value::<OpenAiCompletionResponse>(done.response).ok();
                        let token_usage = response.as_ref().and_then(response_token_usage);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::Http,
                            RequestMode::Full,
                            Some(request_attempt.saturating_sub(1)),
                            None,
                            prepared.replay_source,
                            "completed",
                            response_id.as_deref(),
                            upstream_request_id.as_deref(),
                            token_usage,
                            cache_write_tokens,
                        );
                        if let Some(usage) = token_usage {
                            progress.usage_updated(usage).await;
                        }
                        return successful_http_model_response(&openai.profile, state, token_usage)
                            .map_err(|error| {
                                HttpAttemptError::terminal_response(error, &upstream_request_id)
                            });
                    }
                    ResponseStatus::Failed
                    | ResponseStatus::Incomplete
                    | ResponseStatus::Cancelled => {
                        let response_id = response_id.as_deref().unwrap_or("<missing response ID>");
                        let response_status = match status {
                            ResponseStatus::Failed => "failed",
                            ResponseStatus::Incomplete => "incomplete",
                            ResponseStatus::Cancelled => "cancelled",
                            _ => unreachable!("matched terminal status"),
                        };
                        let response = serde_json::from_value::<OpenAiCompletionResponse>(
                            done.response.clone(),
                        )
                        .ok();
                        let token_usage = response.as_ref().and_then(response_token_usage);
                        log_terminal_response(
                            openai,
                            OpenAiTransport::Http,
                            RequestMode::Full,
                            Some(request_attempt.saturating_sub(1)),
                            None,
                            prepared.replay_source,
                            response_status,
                            Some(response_id),
                            upstream_request_id.as_deref(),
                            token_usage,
                            cache_write_tokens,
                        );
                        state.reset_result();
                        openai.ws.invalidate_continuation("response_failed");
                        return Err(response_attempt_error(
                            done_failure_error(&done.response, &status),
                            &upstream_request_id,
                        ));
                    }
                    ResponseStatus::InProgress
                    | ResponseStatus::Queued
                    | ResponseStatus::Other(_) => {
                        return Err(HttpAttemptError::terminal_response(
                            anyhow::anyhow!(
                                "OpenAI response.done carried non-terminal status {status:?}"
                            ),
                            &upstream_request_id,
                        ));
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct StaleResponseId;
impl std::fmt::Display for StaleResponseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("previous response is unavailable; replaying authoritative local input")
    }
}
impl std::error::Error for StaleResponseId {}

async fn run_model_request_with_reconnect(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<ModelResponse> {
    let policy = openai.recovery.clone();
    run_model_request_with_recovery(model_request, openai, state, progress, &policy).await
}

#[tracing::instrument(skip_all, fields(profile = %openai.profile))]
async fn run_model_request_with_recovery(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<ModelResponse> {
    let prepared = prepare_turn_request(model_request, openai).await?;
    #[cfg(feature = "cache-diagnostics")]
    let prepared = crate::cache_diagnostics::dispatch(prepared, openai);
    run_prepared_with_recovery(&prepared, openai, state, progress, policy).await
}

/// Exactly one controller for connection establishment, dispatch, and streaming.
/// A failed attempt cannot mutate prepared input or restart the tool loop.
async fn run_prepared_with_recovery(
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
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::transport_restored(openai);
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
                #[cfg(feature = "cache-diagnostics")]
                if openai.ws.session.terminal_status()
                    != Some(OpenAiWebSocketTerminalCategory::NotConnected)
                {
                    crate::cache_diagnostics::recovery(prepared, openai);
                }
                attempt_progress.status(NetworkStatus::Connecting).await;
                openai.ws.config.connect_timeout = policy.connect_timeout;
                let result = openai.ws.reconnect().await;
                #[cfg(feature = "cache-diagnostics")]
                crate::cache_diagnostics::connection(
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
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::recovery(prepared, openai);
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

/// Single-response helper used by the protocol-focused tests.
#[cfg(test)]
pub(crate) async fn run_turn(
    user_message: impl Into<String>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    // Cloned into locals so the borrowed request does not alias `state`,
    // which the helper below needs mutably.
    let prompt = Message::user(user_message.into());
    let history = state.history.clone();
    let history_replays = state.history_replays.clone();
    let mut input = history
        .iter()
        .enumerate()
        .map(
            |(index, message)| match history_replays.get(index).and_then(Option::as_ref) {
                Some(replay) => zevria_model::ModelRequestItem::replay_backed(replay),
                None => zevria_model::ModelRequestItem::message(message),
            },
        )
        .collect::<Vec<_>>();
    input.push(zevria_model::ModelRequestItem::message(&prompt));
    run_turn_request(
        ModelRequest {
            input_identity: None,
            tools: &[],
            forbid_tool_calls: false,
            instructions: crate::tests::test_instructions(),
            input,
            model_role: zevria_foundation::ModelRole::Build,
            allowed_tool_names: None,
        },
        openai,
        state,
        progress,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_turn_request(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    let prompt = model_request
        .input
        .iter()
        .rev()
        .find_map(|item| item.message_ref())
        .cloned()
        .expect("test request prompt");
    let prepared = prepare_turn_request(&model_request, openai).await?;
    let policy = RecoveryPolicy::default();
    let response = if openai.transport == OpenAiTransport::Http {
        run_prepared_with_recovery(&prepared, openai, state, progress, &policy).await?
    } else {
        let recovery = Recovery::new(&policy);
        let plan = recovery.start_attempt().unwrap();
        let attempt_progress = AttemptProgress {
            reporter: progress,
            recovery: &recovery,
            attempt: plan.attempt,
            transport: NetworkTransport::WebSocket,
        };
        run_model_request(
            &prepared,
            openai,
            state,
            progress,
            &policy,
            attempt_progress,
        )
        .await?
    };
    state.history.push(prompt);
    state.history_replays.push(None);
    let content = response.into_record().into_model_request_item();
    state
        .history
        .push(content.message_ref().expect("completed message").clone());
    state.history_replays.push(match content {
        zevria_model::OwnedModelRequestItem::ReplayBacked(content) => Some(content),
        _ => None,
    });
    Ok(())
}

#[cfg(test)]
pub(crate) async fn run_turn_request_with_reconnect(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    run_turn_request_with_recovery(
        model_request,
        openai,
        state,
        progress,
        &RecoveryPolicy::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_turn_request_with_recovery(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<()> {
    let prompt = model_request
        .input
        .iter()
        .rev()
        .find_map(|item| item.message_ref())
        .cloned()
        .expect("test request prompt");
    let response =
        run_model_request_with_recovery(&model_request, openai, state, progress, policy).await?;
    state.history.push(prompt);
    state.history_replays.push(None);
    let content = response.into_record().into_model_request_item();
    state
        .history
        .push(content.message_ref().expect("completed message").clone());
    state.history_replays.push(match content {
        zevria_model::OwnedModelRequestItem::ReplayBacked(content) => Some(content),
        _ => None,
    });
    Ok(())
}

impl ModelProvider for OpenAiProvider {
    fn application_prompt(&self) -> &str {
        &self.preamble
    }

    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let mut state = AttemptState::default();
            run_model_request_with_reconnect(&request, self, &mut state, &progress)
                .await
                .map_err(|error| {
                    let context = format!(
                        "profile {} (providers.{}.web_search.enabled={}): {error}",
                        self.profile, self.profile.provider, self.web_search.enabled
                    );
                    error.context(context)
                })
        })
    }

    fn compact<'a>(&'a mut self, request: ModelRequest<'a>) -> CompactFuture<'a> {
        Box::pin(async move { run_remote_compaction(&request, self).await })
    }

    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move { count_full_input_tokens(&request, self).await })
    }

    fn reset(&mut self) {
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::reset(self, "engine_reset");
        if self.transport == OpenAiTransport::WebSocket {
            self.ws.invalidate_continuation("engine_reset");
            self.ws.pending_done_response_id = None;
        }
    }

    fn cancel(&mut self) {
        if self.transport == OpenAiTransport::WebSocket {
            self.ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::LocalCancellation);
            self.ws.invalidate_continuation("turn_cancelled");
            self.ws.pending_done_response_id = None;
        }
    }
}

#[cfg(test)]
#[path = "instruction_set_tests.rs"]
mod instruction_set_tests;

// Temporary test-only preparation probe; included in both feature configurations.
#[cfg(test)]
#[path = "cache_diagnostics/probe_support.rs"]
pub(crate) mod cache_preparation_probe_support;
