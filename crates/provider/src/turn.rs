//! One OpenAI model request, response chaining, and bounded recovery.

use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use rig_core::{
    completion::{CompletionError, CompletionModel},
    message::Message,
    providers::openai::responses_api::{
        CompletionResponse as OpenAiCompletionResponse, Include, ResponseStatus,
        streaming::ResponseChunkKind,
    },
};
use rig_reqwest::openai_websocket::ResponsesWebSocketEvent;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};
use zevria_model::CompactResult;
use zevria_model::InputTokenCount;
use zevria_model::ModelInputTooLarge;
use zevria_model::ModelRequest;
use zevria_model::ModelResponse;
use zevria_model::ProviderReplay;
use zevria_session_api::CompactFuture;
use zevria_session_api::InputTokenCountFuture;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;

use crate::{
    accumulator::AttemptState,
    config::validate_additional_params,
    connection::{
        ContinuationState, OpenAiProvider, OpenAiTransport, is_http_upgrade_required,
        log_identifier, response_request_id,
    },
    protocol::websocket_message_to_text,
    replay::ReplaySource,
    websocket_session::{OpenAiWebSocketInbound, OpenAiWebSocketTerminalCategory},
};
use zevria_responses::{
    accumulator::{response_cache_write_tokens, response_token_usage},
    protocol::{into_openai_request_with_instructions, parse_server_event},
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
        "timed out",
    ]
    .iter()
    .any(|marker| message.contains(marker))
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
pub(crate) struct PreparedRequest {
    #[cfg(feature = "cache-diagnostics")]
    pub(crate) cache_diagnostics: Option<crate::cache_diagnostics::RequestSnapshot>,
    pub(crate) request_properties: serde_json::Value,
    pub(crate) full_input: Vec<serde_json::Value>,
    history_message_count: usize,
    replay_source: ReplaySource,
    search_advertised: bool,
}

struct Transmission {
    #[cfg(feature = "cache-diagnostics")]
    full_reason: Option<crate::cache_diagnostics::FullReason>,
    previous_response_id: Option<String>,
    input: Vec<serde_json::Value>,
    mode: RequestMode,
}

fn complete_provider_input(
    model_request: &ModelRequest<'_>,
    target_profile: &zevria_foundation::ModelProfileRef,
    developer_messages: bool,
) -> anyhow::Result<(Vec<serde_json::Value>, ReplaySource)> {
    let projected = zevria_responses::replay::project_with_compatibility(
        &model_request.input,
        target_profile,
        developer_messages,
    )?;
    anyhow::ensure!(
        !projected.0.is_empty(),
        "the OpenAI Responses request contained no input items"
    );
    Ok(projected)
}

pub(crate) async fn prepare_turn_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<PreparedRequest> {
    let messages = model_request.owned_messages();
    let prompt = messages
        .last()
        .cloned()
        .unwrap_or_else(|| Message::user("provider-native compacted context"));
    let history = messages
        .get(..messages.len().saturating_sub(1))
        .unwrap_or_default()
        .to_vec();
    let tool_definitions = openai.tools.static_tool_defs();
    let model = &openai.model;
    let mut completion_request = model
        .completion_request(prompt)
        .messages(history)
        .additional_params_opt(openai.prepared_responses_parameters())
        .tools(tool_definitions)
        .build();
    if let Some(allowed_tool_names) = model_request.allowed_tool_names {
        completion_request
            .tools
            .retain(|tool| allowed_tool_names.iter().any(|name| name == &tool.name));
    }

    let mut request = into_openai_request_with_instructions(
        model.model.clone(),
        completion_request,
        model_request.instructions,
    )?;
    request.stream = None;
    request.additional_parameters.background = None;
    request.additional_parameters.previous_response_id = None;
    request.additional_parameters.store = openai.compatibility.send_store.then_some(false);
    request.additional_parameters.include = openai
        .compatibility
        .send_reasoning_encrypted_content
        .then(|| vec![Include::ReasoningEncryptedContent]);
    if openai.compatibility.strict_tools {
        request.tools = request
            .tools
            .into_iter()
            .map(|tool| tool.with_strict())
            .collect();
    }

    let search_advertised = openai.web_search.enabled
        && model_request.allowed_tool_names.is_none_or(|names| {
            names
                .iter()
                .any(|name| name == zevria_foundation::WEB_SEARCH_TOOL_NAME)
        });
    if search_advertised {
        request.tools.push(openai.web_search.tool()?);
    }

    let mut request_properties = serde_json::to_value(request)?;
    let properties = request_properties
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the OpenAI request did not serialize as an object"))?;
    validate_additional_params(&openai.profile.provider, &openai.additional_params)?;
    for (key, value) in &openai.additional_params {
        properties.insert(key.clone(), value.clone());
    }
    properties.remove("input");
    properties.remove("previous_response_id");
    if model_request
        .allowed_tool_names
        .is_some_and(|names| names.is_empty())
    {
        // Maintenance is tool-free, even with a provider forcing override.
        properties.remove("tool_choice");
        properties.remove("parallel_tool_calls");
        properties.insert("tools".into(), serde_json::json!([]));
    } else {
        validate_tool_choice(properties).map_err(|error| anyhow::anyhow!(
            "profile {}: providers.{}.additional_params.tool_choice conflicts with the effective tools (check providers.{}.web_search.enabled and task permissions): {error}",
            openai.profile, openai.profile.provider, openai.profile.provider
        ))?;
    }

    let (full_input, replay_source) = complete_provider_input(
        model_request,
        &openai.profile,
        openai.compatibility.developer_messages,
    )?;
    Ok(PreparedRequest {
        #[cfg(feature = "cache-diagnostics")]
        cache_diagnostics: None,
        request_properties,
        full_input,
        history_message_count: model_request.input.len().saturating_sub(1),
        replay_source,
        search_advertised,
    })
}

fn validate_tool_choice(
    properties: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let Some(choice) = properties.get("tool_choice") else {
        return Ok(());
    };
    let tools = properties
        .get("tools")
        .and_then(serde_json::Value::as_array);
    let permitted = |choice: &serde_json::Value| {
        tools.is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type") == choice.get("type")
                    && (tool.get("type").and_then(serde_json::Value::as_str) != Some("function")
                        || tool.get("name") == choice.get("name"))
            })
        })
    };
    match choice.as_str() {
        Some("auto" | "none") => return Ok(()),
        Some("required") => {
            anyhow::ensure!(
                tools.is_some_and(|tools| !tools.is_empty()),
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

const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";

/// Rewrite only correlated tool outputs, newest first, when a compact request
/// exceeds the configured input ceiling. Calls, output envelopes, IDs, status
/// fields, and order stay byte-for-byte equivalent apart from the output body
/// itself.
pub(crate) fn trim_correlated_tool_outputs(
    input: &[serde_json::Value],
    request_property_bytes: usize,
    input_token_limit: u64,
) -> Vec<serde_json::Value> {
    let mut rewritten = input.to_vec();
    fn request_tokens(input: &[serde_json::Value], request_property_bytes: usize) -> u64 {
        zevria_model::compaction::estimate_responses_input_tokens(input, request_property_bytes)
    }
    if request_tokens(&rewritten, request_property_bytes) <= input_token_limit {
        return rewritten;
    }

    let calls = input
        .iter()
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?;
            matches!(kind, "function_call" | "custom_tool_call")
                .then(|| item.get("call_id")?.as_str().map(ToOwned::to_owned))?
        })
        .collect::<std::collections::HashSet<_>>();

    for index in (0..rewritten.len()).rev() {
        if request_tokens(&rewritten, request_property_bytes) <= input_token_limit {
            break;
        }
        let item = &mut rewritten[index];
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let eligible = matches!(
            object.get("type").and_then(serde_json::Value::as_str),
            Some("function_call_output" | "custom_tool_call_output")
        ) && object
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|call_id| calls.contains(call_id));
        if !eligible {
            continue;
        }
        let Some(output) = object.get_mut("output") else {
            continue;
        };
        let old_len = serde_json::to_vec(output).map_or(0, |value| value.len());
        if old_len <= CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.len() {
            continue;
        }
        match output {
            serde_json::Value::Object(payload) => {
                if let Some(body) = payload.get_mut("body") {
                    *body = serde_json::Value::String(
                        CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string(),
                    );
                } else if let Some(text) = payload.get_mut("text") {
                    *text = serde_json::Value::String(
                        CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string(),
                    );
                } else {
                    *output = serde_json::Value::String(
                        CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string(),
                    );
                }
            }
            _ => {
                *output =
                    serde_json::Value::String(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string());
            }
        }
    }
    rewritten
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
        serde_json::Value::Array(prepared.full_input),
    );

    let log_url = crate::connection::redacted_url_for_logging(url.as_str());
    let response = openai
        .http
        .post(url)
        .bearer_auth(&openai.api_key)
        .timeout(openai.input_token_count_timeout)
        .json(&body)
        .send()
        .await
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
        anyhow::bail!(
            "OpenAI input-token request to {log_url} failed with HTTP {status}: {detail}"
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
    zevria_model::maintenance::validate_maintenance_input(&model_request.input)?;
    anyhow::ensure!(
        model_request
            .allowed_tool_names
            .is_some_and(|names| names.is_empty()),
        "maintenance requests must disable tools"
    );
    let Some(url) = openai.compaction_url.clone() else {
        return Ok(CompactResult::Unsupported);
    };
    let prepared = prepare_turn_request(model_request, openai).await?;
    let mut body = prepared.request_properties;
    crate::prompt_cache::remove_comparison(&mut body);
    let request_property_bytes = serde_json::to_vec(&body).map_or(usize::MAX, |bytes| bytes.len());
    let properties = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the compact request did not serialize as an object"))?;
    properties.remove("stream");
    properties.remove("previous_response_id");
    properties.remove("include");
    properties
        .entry("parallel_tool_calls".to_string())
        .or_insert(serde_json::Value::Bool(true));
    properties
        .entry("tools".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    properties.insert(
        "input".to_string(),
        serde_json::Value::Array(trim_correlated_tool_outputs(
            &prepared.full_input,
            request_property_bytes,
            openai.input_token_limit,
        )),
    );

    let response = openai
        .http
        .post(url.clone())
        .bearer_auth(&openai.api_key)
        .timeout(openai.compaction_timeout)
        .json(&body)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("OpenAI compaction request to {url} failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = bounded_http_error_body(response).await;
        anyhow::bail!("OpenAI compaction request failed with HTTP {status}: {detail}");
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
    let replay = ProviderReplay::openai_responses(openai.profile.clone(), output.clone());
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
        if continuation.response_output.is_empty() {
            return Err("missing_native_output");
        }

        let prefix_len = continuation.request_input.len() + continuation.response_output.len();
        let has_exact_prefix = prepared.full_input.len() > prefix_len
            && prepared.full_input.get(..continuation.request_input.len())
                == Some(continuation.request_input.as_slice())
            && prepared
                .full_input
                .get(continuation.request_input.len()..prefix_len)
                == Some(continuation.response_output.as_slice());
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
                input: prepared.full_input.clone(),
                mode: RequestMode::Full,
            }
        }
        None => Transmission {
            #[cfg(feature = "cache-diagnostics")]
            full_reason: Some(crate::cache_diagnostics::no_continuation_reason(openai)),
            previous_response_id: None,
            input: prepared.full_input.clone(),
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
        input: prepared.full_input.clone(),
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
    if let Some(search) = &mut state.search {
        search.reconcile(&response_output);
    }
    let response = response
        .with_usage(usage)
        .with_display_attempt(
            state
                .search
                .as_ref()
                .filter(|search| search.attempt().has_display())
                .map(|search| search.attempt().id.clone()),
        )
        .inspect_err(|_| openai.ws.invalidate_continuation("invalid_native_output"))?;
    openai.ws.continuation = Some(ContinuationState {
        socket_generation: openai.ws.socket_generation,
        response_id,
        request_properties: prepared.request_properties.clone(),
        request_input: prepared.full_input.clone(),
        response_output,
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
    classify_input_size(
        matches!(status, ResponseStatus::Failed | ResponseStatus::Incomplete)
            && structured_input_size(response),
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
    classify_input_size(
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
) -> anyhow::Result<ModelResponse> {
    state.search = prepared
        .search_advertised
        .then(|| crate::search::SearchStream::new(openai.profile.clone(), progress));
    let result = run_model_request_inner(prepared, openai, state, progress, policy).await;
    state.finish_search(result.is_ok(), progress).await?;
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

    let transmission = select_transmission(prepared, openai);
    let mut request_mode = transmission.mode;
    let mut previous_response_id = transmission.previous_response_id.clone();
    let mut retried_with_history = false;
    if let Err(error) =
        send_prepared_request(prepared, &transmission, openai, policy.send_timeout).await
    {
        abandon_attempt(openai, state);
        return Err(error);
    }
    let mut first_event_deadline = Some(tokio::time::Instant::now() + policy.first_event_timeout);

    loop {
        let inbound = if let Some(deadline) = first_event_deadline {
            match tokio::time::timeout_at(deadline, openai.ws.session.next()).await {
                Ok(inbound) => inbound,
                Err(_) => {
                    openai
                        .ws
                        .session
                        .terminate(OpenAiWebSocketTerminalCategory::FirstEventTimeout);
                    abandon_attempt(openai, state);
                    return Err(WebSocketDisconnected::without_source(
                        "waiting for the first response event",
                        OpenAiWebSocketTerminalCategory::FirstEventTimeout.as_str(),
                    )
                    .into());
                }
            }
        } else {
            openai.ws.session.next().await
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

        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::observe_raw(openai, &message, prior_terminal_id.as_deref(), None);

        if prepared.search_advertised
            && prior_terminal_id.is_some()
            && let Ok(event) = serde_json::from_str::<serde_json::Value>(&message)
            && matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.done")
            )
            && event["response"]["id"].as_str() == prior_terminal_id.as_deref()
        {
            if event["type"] == "response.done" {
                openai.ws.pending_done_response_id = None;
            }
            continue;
        }

        // Capture lossless native output from the original JSON before Rig
        // deserializes modeled output fields. `response.done` is captured in
        // its typed branch after a late duplicate has been filtered.
        match serde_json::from_str::<serde_json::Value>(&message)
            .ok()
            .and_then(|event| {
                event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .as_deref()
        {
            Some("response.output_item.done") => {
                state.record_native_output_item_done(&message);
            }
            Some("response.completed") => {
                state.record_native_terminal_output(&message);
            }
            _ => {}
        }

        // Do not attach a preceding response's late done event to this attempt.
        let pending_done = serde_json::from_str::<serde_json::Value>(&message)
            .ok()
            .is_some_and(|event| {
                event["type"] == "response.done"
                    && event["response"]["id"]
                        .as_str()
                        .is_some_and(|id| openai.ws.pending_done_response_id.as_deref() == Some(id))
            });
        if !pending_done {
            state.observe_search(&message, progress).await;
        }
        let event = match parse_server_event(&message) {
            Ok(event) => event,
            Err(error) => {
                abandon_attempt(openai, state);
                return Err(error.into());
            }
        };

        let is_pending_done = match event.as_ref() {
            Some(ResponsesWebSocketEvent::Done(done)) => {
                let response_id = done.response_id();
                openai
                    .ws
                    .pending_done_response_id
                    .as_deref()
                    .is_some_and(|pending_id| response_id == Some(pending_id))
            }
            _ => false,
        };
        if is_pending_done {
            openai.ws.pending_done_response_id = None;
            continue;
        }

        // Any syntactically valid current-generation server event, including
        // an unknown event type, starts the response stream. A late
        // `response.done` for the preceding response is the sole exception
        // and leaves the original deadline running.
        first_event_deadline = None;

        let Some(event) = event else {
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
                        let cache_write_tokens = response_cache_write_tokens(&message);
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
                        let cache_write_tokens = response_cache_write_tokens(&message);
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
                        CompletionError::ProviderError(diagnostic_detail(&error.to_string()))
                            .into(),
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

                let can_retry_with_history = previous_response_id.is_some()
                    && !retried_with_history
                    && error_reports_missing_previous_response(
                        error.error.code.as_deref(),
                        error.error.message.as_deref(),
                    );

                if can_retry_with_history {
                    tracing::warn!(
                        "previous response not found on the server; replaying the full local history"
                    );
                    state.finish_search(false, progress).await?;
                    state.reset_result();
                    progress.stream_cleared();
                    openai.ws.invalidate_continuation("stale_response_id");
                    previous_response_id = None;
                    retried_with_history = true;

                    state.search = prepared.search_advertised.then(|| {
                        crate::search::SearchStream::new(openai.profile.clone(), progress)
                    });
                    let full = full_transmission(prepared);
                    request_mode = full.mode;
                    if let Err(error) =
                        send_prepared_request(prepared, &full, openai, policy.send_timeout).await
                    {
                        abandon_attempt(openai, state);
                        return Err(error);
                    }
                    first_event_deadline =
                        Some(tokio::time::Instant::now() + policy.first_event_timeout);
                    continue;
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
                return Err(CompletionError::ProviderError(message).into());
            }
            ResponsesWebSocketEvent::Done(done) => {
                let response_id = done.response_id().map(ToOwned::to_owned);
                state.record_native_terminal_output(&message);

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
                        let cache_write_tokens = response_cache_write_tokens(&message);
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
                        let cache_write_tokens = response_cache_write_tokens(&message);
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
            ResponsesWebSocketEvent::Unknown(_) => {}
        }
    }
}

const HTTP_ERROR_BODY_LIMIT: usize = 4 * 1024;

enum HttpAttemptError {
    Retryable {
        category: &'static str,
        error: anyhow::Error,
        upstream_request_id: Option<String>,
    },
    Terminal {
        error: anyhow::Error,
        upstream_request_id: Option<String>,
    },
}

impl HttpAttemptError {
    fn retryable(category: &'static str, error: impl Into<anyhow::Error>) -> Self {
        Self::Retryable {
            category,
            error: error.into(),
            upstream_request_id: None,
        }
    }

    fn terminal(error: impl Into<anyhow::Error>) -> Self {
        Self::Terminal {
            error: error.into(),
            upstream_request_id: None,
        }
    }

    fn retryable_response(
        category: &'static str,
        error: impl Into<anyhow::Error>,
        upstream_request_id: &Option<String>,
    ) -> Self {
        Self::Retryable {
            category,
            error: with_http_response_request_id(error.into(), upstream_request_id),
            upstream_request_id: upstream_request_id.clone(),
        }
    }

    fn terminal_response(
        error: impl Into<anyhow::Error>,
        upstream_request_id: &Option<String>,
    ) -> Self {
        Self::Terminal {
            error: with_http_response_request_id(error.into(), upstream_request_id),
            upstream_request_id: upstream_request_id.clone(),
        }
    }

    fn from_http_status(
        category: Option<&'static str>,
        error: CompletionError,
        upstream_request_id: Option<String>,
    ) -> Self {
        match category {
            Some(category) => Self::Retryable {
                category,
                error: error.into(),
                upstream_request_id,
            },
            None => Self::Terminal {
                error: error.into(),
                upstream_request_id,
            },
        }
    }
}

fn with_http_response_request_id(
    error: anyhow::Error,
    upstream_request_id: &Option<String>,
) -> anyhow::Error {
    match upstream_request_id {
        Some(request_id) => error.context(format!(
            "OpenAI HTTP response carried x-request-id {request_id}"
        )),
        None => error,
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
        serde_json::Value::Array(prepared.full_input.clone()),
    );
    body.insert("stream".to_string(), serde_json::Value::Bool(true));
    Ok(serde_json::Value::Object(body))
}

fn diagnostic_detail(text: &str) -> String {
    zevria_content::image_diagnostics::text_copy(text)
        .chars()
        .take(HTTP_ERROR_BODY_LIMIT)
        .collect()
}

async fn bounded_http_error_body(response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while bytes.len() < HTTP_ERROR_BODY_LIMIT {
        match stream.next().await {
            Some(Ok(chunk)) => {
                let remaining = HTTP_ERROR_BODY_LIMIT - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Some(Err(error)) => {
                if bytes.is_empty() {
                    return format!("<failed to read error body: {error}>");
                }
                break;
            }
            None => break,
        }
    }
    let mut detail = String::from_utf8_lossy(&bytes).into_owned();
    if detail.len() > HTTP_ERROR_BODY_LIMIT {
        let mut end = HTTP_ERROR_BODY_LIMIT;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    diagnostic_detail(&detail)
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
    if let Some(search) = &mut state.search {
        search.reconcile(
            &response
                .record()
                .provider_replay()
                .expect("native completion")
                .items,
        );
    }
    response.with_usage(usage).with_display_attempt(
        state
            .search
            .as_ref()
            .filter(|search| search.attempt().has_display())
            .map(|search| search.attempt().id.clone()),
    )
}

async fn run_http_model_request(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
    request_attempt: usize,
) -> Result<ModelResponse, HttpAttemptError> {
    state.search = prepared
        .search_advertised
        .then(|| crate::search::SearchStream::new(openai.profile.clone(), progress));
    let result =
        run_http_model_request_inner(prepared, openai, state, progress, policy, request_attempt)
            .await;
    state
        .finish_search(result.is_ok(), progress)
        .await
        .map_err(HttpAttemptError::terminal)?;
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
    request_attempt: usize,
) -> Result<ModelResponse, HttpAttemptError> {
    state.reset_result();
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
        .bearer_auth(&openai.api_key)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&body);
    #[cfg(feature = "cache-diagnostics")]
    let request = {
        let request = request.build().map_err(|error| {
            HttpAttemptError::retryable(
                "request_error",
                anyhow::anyhow!("OpenAI HTTP request failed: {error}"),
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
            request.body().and_then(reqwest::Body::as_bytes),
        );
        openai.http.execute(request)
    };
    #[cfg(not(feature = "cache-diagnostics"))]
    let request = request.send();
    let response = match tokio::time::timeout(policy.send_timeout, request).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Err(HttpAttemptError::retryable(
                "request_error",
                anyhow::anyhow!("OpenAI HTTP request failed: {error}"),
            ));
        }
        Err(_) => {
            return Err(HttpAttemptError::retryable(
                "request_timeout",
                anyhow::anyhow!(
                    "OpenAI HTTP request timed out after {}s",
                    policy.send_timeout.as_secs_f64()
                ),
            ));
        }
    };

    let upstream_request_id = response_request_id(response.headers());
    let status = response.status();
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
        let error = CompletionError::from_http_response_with_request_id(
            status,
            detail,
            upstream_request_id.clone(),
        );
        if size_rejection {
            return Err(HttpAttemptError::Terminal {
                error: classify_input_size(true, error.into()),
                upstream_request_id,
            });
        }
        return Err(HttpAttemptError::from_http_status(
            status.is_server_error().then_some("server_error"),
            error,
            upstream_request_id,
        ));
    }

    let mut events = response.bytes_stream().eventsource();
    let mut first_event_deadline = Some(tokio::time::Instant::now() + policy.first_event_timeout);

    loop {
        let event = if let Some(deadline) = first_event_deadline {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(event) => event,
                Err(_) => {
                    return Err(HttpAttemptError::retryable_response(
                        "first_event_timeout",
                        anyhow::anyhow!(
                            "OpenAI HTTP response produced no event within {}s",
                            policy.first_event_timeout.as_secs_f64()
                        ),
                        &upstream_request_id,
                    ));
                }
            }
        } else {
            events.next().await
        };

        let payload = match event {
            Some(Ok(event)) => event.data,
            Some(Err(EventStreamError::Transport(error))) => {
                return Err(HttpAttemptError::retryable_response(
                    "sse_transport_error",
                    anyhow::anyhow!("OpenAI HTTP event stream failed: {error}"),
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

        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::observe_raw(
            openai,
            payload,
            None,
            upstream_request_id.as_deref(),
        );

        match serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|event| {
                event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .as_deref()
        {
            Some("response.output_item.done") => {
                state.record_native_output_item_done(payload);
            }
            Some("response.completed") => {
                state.record_native_terminal_output(payload);
            }
            _ => {}
        }

        state.observe_search(payload, progress).await;
        let event = parse_server_event(payload).map_err(|error| {
            HttpAttemptError::terminal_response(anyhow::Error::from(error), &upstream_request_id)
        })?;
        // Any syntactically valid, nonempty server event, including an unknown
        // event type, proves the SSE stream has started. Empty frames and the
        // `[DONE]` sentinel above intentionally leave the deadline running.
        first_event_deadline = None;
        let Some(event) = event else {
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
                        let cache_write_tokens = response_cache_write_tokens(payload);
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
                        let cache_write_tokens = response_cache_write_tokens(payload);
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
                        return Err(HttpAttemptError::terminal_response(
                            response_failure_error(&chunk.response),
                            &upstream_request_id,
                        ));
                    }
                    ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => {}
                }
            }
            ResponsesWebSocketEvent::Error(error) => {
                return Err(HttpAttemptError::terminal_response(
                    classify_input_size(
                        input_size_code(error.error.code.as_deref()),
                        CompletionError::ProviderError(diagnostic_detail(&error.to_string()))
                            .into(),
                    ),
                    &upstream_request_id,
                ));
            }
            ResponsesWebSocketEvent::Done(done) => {
                let response_id = done.response_id().map(ToOwned::to_owned);
                state.record_native_terminal_output(payload);
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
                                response_cache_write_tokens(payload),
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
                        let cache_write_tokens = response_cache_write_tokens(payload);
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
                        let cache_write_tokens = response_cache_write_tokens(payload);
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
                        return Err(HttpAttemptError::terminal_response(
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
            ResponsesWebSocketEvent::Unknown(_) => {}
        }
    }
}

async fn run_http_model_request_with_recovery(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<ModelResponse> {
    let mut retries = 0usize;
    loop {
        match run_http_model_request(
            prepared,
            openai,
            state,
            progress,
            policy,
            retries.saturating_add(1),
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(HttpAttemptError::Terminal {
                error,
                upstream_request_id,
            }) => {
                state.reset_result();
                tracing::debug!(
                    provider = %openai.profile.provider,
                    model = %openai.profile.model,
                    transport = OpenAiTransport::Http.as_str(),
                    upstream_request_id = ?upstream_request_id.as_deref().map(log_identifier),
                    "OpenAI HTTP request ended with a terminal error"
                );
                return Err(error);
            }
            Err(HttpAttemptError::Retryable {
                category,
                error,
                upstream_request_id,
            }) => {
                state.reset_result();
                progress.stream_cleared();
                if retries >= policy.max_reconnect_attempts {
                    return Err(error.context(format!(
                        "the turn failed after {} HTTP retry attempts",
                        policy.max_reconnect_attempts
                    )));
                }
                retries += 1;
                let retry_after = policy.backoff(retries);
                tracing::warn!(
                    transport = OpenAiTransport::Http.as_str(),
                    retry = retries,
                    max_retries = policy.max_reconnect_attempts,
                    retry_category = category,
                    upstream_request_id = ?upstream_request_id.as_deref().map(log_identifier),
                    "OpenAI HTTP request failed; replaying complete local input"
                );
                progress
                    .retrying(
                        retries,
                        policy.max_reconnect_attempts,
                        retry_after,
                        error.to_string(),
                    )
                    .await;
                tokio::time::sleep(retry_after).await;
            }
        }
    }
}

/// How a turn recovers from transport failures: bounded request and
/// first-event waits plus reconnect/replay attempts with capped backoff.
pub(crate) struct RecoveryPolicy {
    /// Recovery cycles after the initial attempt. WebSocket and HTTP each get
    /// a fresh budget when a logical request falls back between transports.
    pub(crate) max_reconnect_attempts: usize,
    /// Delay before the first recovery cycle; doubles per cycle.
    pub(crate) backoff_base: std::time::Duration,
    /// Upper bound on the per-cycle backoff delay.
    pub(crate) backoff_cap: std::time::Duration,
    /// How long a socket write acknowledgement or HTTP response headers may
    /// take.
    pub(crate) send_timeout: std::time::Duration,
    /// How long a request may wait for its first syntactically valid server
    /// event. Unknown event types still prove the stream has started; transport
    /// control frames, empty SSE data, and `[DONE]` do not. Mid-stream
    /// inactivity after that event is unbounded.
    pub(crate) first_event_timeout: std::time::Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_reconnect_attempts: 5,
            backoff_base: std::time::Duration::from_millis(500),
            backoff_cap: std::time::Duration::from_secs(8),
            send_timeout: std::time::Duration::from_secs(30),
            first_event_timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl RecoveryPolicy {
    /// The delay before recovery cycle `attempt` (1-based): exponential from
    /// the base, capped.
    fn backoff(&self, attempt: usize) -> std::time::Duration {
        let doublings = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        self.backoff_base
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(self.backoff_cap)
    }
}

/// Run one model request, transparently recovering its preferred transport and
/// falling back to sticky HTTP when WebSocket recovery is exhausted. Tool
/// execution lives outside this scope, so replaying one prepared request can
/// never repeat a completed command. Recovery attempts remain bounded by the
/// transport recovery policy for each logical request.
async fn run_model_request_with_reconnect(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<ModelResponse> {
    run_model_request_with_recovery(
        model_request,
        openai,
        state,
        progress,
        &RecoveryPolicy::default(),
    )
    .await
}

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

    if openai.transport == OpenAiTransport::Http {
        return run_http_model_request_with_recovery(&prepared, openai, state, progress, policy)
            .await;
    }

    let mut attempt = 0usize;
    let mut pending_disconnect = None;
    loop {
        // A parked connection is replaced only after its continuously polled
        // pump has definitively stopped. Think-time alone is never a reason to
        // rotate a healthy socket or discard its continuation.
        let (error, reconnect_immediately) = if let Some(error) = pending_disconnect.take() {
            (error, false)
        } else if let Some(terminal) = openai.ws.session.terminal_status() {
            (
                anyhow::Error::from(WebSocketDisconnected::from_terminal(
                    "starting the next request on a terminated socket",
                    terminal,
                )),
                attempt == 0,
            )
        } else {
            match run_model_request(&prepared, openai, state, progress, policy).await {
                Ok(message) => return Ok(message),
                Err(error) if is_websocket_disconnect(&error) => (error, false),
                Err(error) => return Err(error),
            }
        };

        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::recovery(&prepared, openai);
        if attempt >= policy.max_reconnect_attempts {
            let websocket_error = error.context(format!(
                "the turn failed after {} reconnect attempts",
                policy.max_reconnect_attempts
            ));
            state.reset_result();
            progress.stream_cleared();
            openai.switch_to_http("websocket_retries_exhausted");
            return run_http_model_request_with_recovery(
                &prepared, openai, state, progress, policy,
            )
            .await
            .map_err(|http_error| {
                http_error.context(format!(
                    "WebSocket transport was unavailable before HTTP fallback: {websocket_error:#}"
                ))
            });
        }
        attempt += 1;

        let terminal_category = error
            .downcast_ref::<WebSocketDisconnected>()
            .map(WebSocketDisconnected::terminal_category)
            .unwrap_or("unknown_disconnect");
        let socket_generation = openai.ws.socket_generation;
        let idle_ms = u64::try_from(openai.ws.idle_for().as_millis()).unwrap_or(u64::MAX);
        tracing::warn!(
            socket_generation,
            attempt,
            max_reconnect_attempts = policy.max_reconnect_attempts,
            idle_ms,
            terminal_category,
            "OpenAI websocket disconnected; reconnecting for full replay"
        );
        let retry_after = if reconnect_immediately {
            std::time::Duration::ZERO
        } else {
            policy.backoff(attempt)
        };
        progress.stream_cleared();
        progress
            .retrying(
                attempt,
                policy.max_reconnect_attempts,
                retry_after,
                error.to_string(),
            )
            .await;
        if !reconnect_immediately {
            tokio::time::sleep(retry_after).await;
        }

        if let Err(reconnect_error) = openai.ws.reconnect().await {
            if is_http_upgrade_required(&reconnect_error) {
                openai.switch_to_http("websocket_upgrade_required");
                return run_http_model_request_with_recovery(
                    &prepared, openai, state, progress, policy,
                )
                .await;
            }
            tracing::warn!(
                socket_generation,
                attempt,
                idle_ms,
                terminal_category,
                reconnect_error = %reconnect_error,
                "OpenAI websocket reconnect handshake failed; preserving the prior session"
            );
            // Do not send another model request on a connection already known
            // to require replacement (notably the 60-minute limit). Retry only
            // the handshake; the failed swap left every old field intact.
            pending_disconnect = Some(error);
            #[cfg(feature = "cache-diagnostics")]
            crate::cache_diagnostics::connection(openai, "reconnect_failed");
        } else {
            #[cfg(feature = "cache-diagnostics")]
            crate::cache_diagnostics::connection(openai, "reconnected");
        }
    }
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
        run_http_model_request_with_recovery(&prepared, openai, state, progress, &policy).await?
    } else {
        run_model_request(&prepared, openai, state, progress, &policy).await?
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
