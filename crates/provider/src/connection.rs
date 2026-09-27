//! Responses transport lifecycle and provider construction.

use rig_agent::tool::server::ToolServerHandle;
use rig_core::{
    client::CompletionClient,
    providers::openai::responses_api::{
        AdditionalParameters, Include, Reasoning, ReasoningSummaryLevel,
    },
};
use rig_reqwest::{
    client::DefaultTransportBuilder,
    providers::openai::{self, ResponsesCompletionModel},
};
use sha2::{Digest, Sha256};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Error as WebSocketError,
        client::IntoClientRequest,
        http::{HeaderMap, HeaderValue, StatusCode},
    },
};
use zevria_foundation::{ModelProfileRef, ReasoningLevel};

use crate::{
    config::{InputTokenCountConfig, ResolvedModelProfile, ResponsesCompatibilityConfig},
    websocket_session::OpenAiWebSocketSession,
};

pub(crate) type OpenAiWebSocketStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const MAX_PROMPT_CACHE_KEY_BYTES: usize = 64;

pub(crate) fn validate_prompt_cache_key(enabled: bool, cache_key: &str) -> anyhow::Result<()> {
    if !enabled {
        return Ok(());
    }
    if cache_key.is_empty() {
        anyhow::bail!("prompt_cache_key must not be empty when transmission is enabled");
    }
    if cache_key.len() > MAX_PROMPT_CACHE_KEY_BYTES {
        anyhow::bail!(
            "prompt_cache_key must be at most {MAX_PROMPT_CACHE_KEY_BYTES} bytes when transmission is enabled (found {} bytes)",
            cache_key.len()
        );
    }
    Ok(())
}

/// Build one optional provider-specific routing header. The value is separate
/// from the profile cache key and must never appear in debug header dumps.
pub(crate) fn session_routing_headers(
    profile: &ResolvedModelProfile,
    session_id: &str,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(name) = crate::config::parse_session_id_header(
        &profile.profile.provider,
        profile.endpoint.session_id_header.as_deref(),
    )? {
        anyhow::ensure!(
            !session_id.trim().is_empty(),
            "profile {} requires a nonblank session ID for its configured session_id_header",
            profile.profile
        );
        let mut value = HeaderValue::from_str(session_id).map_err(|_| {
            anyhow::anyhow!(
                "profile {} has an invalid session-ID header value",
                profile.profile
            )
        })?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

pub(crate) fn prompt_cache_key_fingerprint(cache_key: &str) -> String {
    let digest = crate::lowercase_hex(&Sha256::digest(cache_key.as_bytes()));
    format!("sha256:{}", &digest[..16])
}

// Diagnostic builds also bound existing identifier log fields. The actual
// headers, IDs, errors, and continuation values remain untouched.
pub(crate) fn log_identifier(value: &str) -> &str {
    #[cfg(feature = "cache-diagnostics")]
    {
        crate::cache_diagnostics::identifier(value)
    }
    #[cfg(not(feature = "cache-diagnostics"))]
    {
        value
    }
}

pub(crate) fn response_request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// The configured full Responses endpoint expressed for both transports.
/// Conversion changes only the URL scheme, preserving host, path, and query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenAiResponseEndpoints {
    pub(crate) http: reqwest::Url,
    pub(crate) websocket: reqwest::Url,
}

impl OpenAiResponseEndpoints {
    pub(crate) fn parse(base_url: &str) -> anyhow::Result<Self> {
        let endpoint = reqwest::Url::parse(base_url)
            .map_err(|error| anyhow::anyhow!("invalid Responses endpoint URL: {error}"))?;
        let (http_scheme, websocket_scheme) = match endpoint.scheme() {
            "http" | "ws" => ("http", "ws"),
            "https" | "wss" => ("https", "wss"),
            scheme => {
                anyhow::bail!(
                    "Responses endpoint URL must use http, https, ws, or wss (found {scheme})"
                )
            }
        };

        let mut http = endpoint.clone();
        http.set_scheme(http_scheme)
            .map_err(|()| anyhow::anyhow!("failed to derive the OpenAI HTTP endpoint"))?;
        let mut websocket = endpoint;
        websocket
            .set_scheme(websocket_scheme)
            .map_err(|()| anyhow::anyhow!("failed to derive the OpenAI WebSocket endpoint"))?;
        Ok(Self { http, websocket })
    }
}

pub(crate) fn resolve_input_token_count_url(
    responses_url: &reqwest::Url,
    config: &InputTokenCountConfig,
) -> anyhow::Result<Option<reqwest::Url>> {
    if !config.enabled {
        return Ok(None);
    }
    if let Some(url) = &config.url {
        return reqwest::Url::parse(url)
            .map(Some)
            .map_err(|error| anyhow::anyhow!("invalid input-token count URL: {error}"));
    }
    if !config.derive_url {
        return Ok(None);
    }
    let mut url = responses_url.clone();
    let path = format!("{}/input_tokens", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(Some(url))
}

/// Render only endpoint structure that is safe to include in diagnostics.
/// Authentication material may be supplied through URL user-info or query
/// parameters, so both are removed along with the client-only fragment.
pub(crate) fn redacted_url_for_logging(raw_url: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw_url) else {
        return "<invalid URL>".to_string();
    };
    let _ = url.set_password(None);
    let _ = url.set_username("");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiTransport {
    WebSocket,
    Http,
}

impl OpenAiTransport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::Http => "http",
        }
    }
}

/// The exact request/response boundary represented by one server-side
/// continuation ID. IDs are valid only on the socket generation that created
/// them, and only for an exact extension of this native lineage.
#[derive(Debug, Clone)]
pub(crate) struct ContinuationState {
    pub(crate) socket_generation: u64,
    pub(crate) response_id: String,
    pub(crate) request_properties: serde_json::Value,
    pub(crate) request_input: Vec<serde_json::Value>,
    pub(crate) response_output: Vec<serde_json::Value>,
}

/// How long a websocket handshake may take before the attempt is abandoned,
/// so a black-holed connect can't hang a turn indefinitely.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything needed to (re)establish the OpenAI Responses websocket. The base
/// URL and headers are captured once at startup so the adapter can rebuild the
/// connection after the server drops an idle socket.
pub(crate) struct OpenAiWebSocketConfig {
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
}

pub(crate) struct OpenAiWebSocketConnection {
    pub(crate) socket: OpenAiWebSocketStream,
    pub(crate) websocket_connection_request_id: Option<String>,
}

impl OpenAiWebSocketConfig {
    pub(crate) async fn connect(&self) -> anyhow::Result<OpenAiWebSocketConnection> {
        let log_url = redacted_url_for_logging(&self.url);
        tracing::debug!(url = %log_url, "connecting to the OpenAI websocket");
        let mut request = self.url.as_str().into_client_request()?;
        for (name, value) in &self.headers {
            request.headers_mut().insert(name, value.clone());
        }
        let (socket, response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "connecting to the OpenAI websocket timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )
            })??;
        let websocket_connection_request_id = response_request_id(response.headers());
        tracing::info!(
            url = %log_url,
            websocket_connection_request_id = ?websocket_connection_request_id.as_deref().map(log_identifier),
            "OpenAI websocket connected"
        );
        Ok(OpenAiWebSocketConnection {
            socket,
            websocket_connection_request_id,
        })
    }
}

pub(crate) fn is_http_upgrade_required(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<WebSocketError>(),
        Some(WebSocketError::Http(response))
            if response.status() == StatusCode::UPGRADE_REQUIRED
    )
}

pub(crate) struct OpenAiParkedWebSocket {
    pub(crate) session: OpenAiWebSocketSession,
    pub(crate) config: OpenAiWebSocketConfig,
    pub(crate) socket_generation: u64,
    pub(crate) websocket_connection_request_id: Option<String>,
    pub(crate) continuation: Option<ContinuationState>,
    pub(crate) pending_done_response_id: Option<String>,
    /// When the socket last carried a completed request. Idle duration is
    /// observational; a healthy pumped socket is trusted until it closes.
    pub(crate) last_activity: std::time::Instant,
}

impl OpenAiParkedWebSocket {
    /// Replace the parked socket with a freshly connected one.
    /// Continuation IDs are socket-scoped, so a successful replacement starts
    /// a new generation and forces the next request to replay complete local
    /// history. A failed handshake leaves the old socket and all its state
    /// intact.
    pub(crate) async fn reconnect(&mut self) -> anyhow::Result<()> {
        let next_generation = self.socket_generation.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("the OpenAI websocket generation counter was exhausted")
        })?;
        let connection = self.config.connect().await?;
        let session = OpenAiWebSocketSession::new(connection.socket);
        self.session = session;
        self.socket_generation = next_generation;
        self.websocket_connection_request_id = connection.websocket_connection_request_id;
        self.invalidate_continuation("reconnect");
        self.pending_done_response_id = None;
        Ok(())
    }

    /// Clear a usable continuation and record only a bounded reason, never
    /// request or response content.
    pub(crate) fn invalidate_continuation(&mut self, reason: &'static str) {
        if self.continuation.take().is_some() {
            tracing::info!(
                reason,
                socket_generation = self.socket_generation,
                "OpenAI continuation invalidated"
            );
        }
    }

    /// Mark the socket as freshly used.
    pub(crate) fn touch(&mut self) {
        self.last_activity = std::time::Instant::now();
    }

    /// How long the socket has sat unused.
    pub(crate) fn idle_for(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }
}

/// A live single-profile Responses runtime with an independent continuation
/// chain and sticky transport state.
pub struct OpenAiProvider {
    #[cfg(feature = "cache-diagnostics")]
    pub(crate) cache_diagnostics: crate::cache_diagnostics::DiagnosticState,
    pub(crate) profile: ModelProfileRef,
    pub(crate) model: ResponsesCompletionModel,
    #[allow(dead_code)]
    pub(crate) context_window_tokens: u64,
    pub(crate) input_token_limit: u64,
    pub(crate) preamble: String,
    pub(crate) responses_parameters: Option<serde_json::Value>,
    pub(crate) reasoning_level: ReasoningLevel,
    pub(crate) reasoning_summary_level: ReasoningSummaryLevel,
    pub(crate) compatibility: ResponsesCompatibilityConfig,
    pub(crate) web_search: crate::WebSearchConfig,
    pub(crate) additional_params: std::collections::BTreeMap<String, serde_json::Value>,
    pub(crate) prompt_cache_key: String,
    pub(crate) tools: ToolServerHandle,
    pub(crate) ws: OpenAiParkedWebSocket,
    pub(crate) responses_url: reqwest::Url,
    pub(crate) transport: OpenAiTransport,
    pub(crate) compaction_url: Option<reqwest::Url>,
    pub(crate) compaction_timeout: std::time::Duration,
    pub(crate) input_token_count_url: Option<reqwest::Url>,
    pub(crate) input_token_count_timeout: std::time::Duration,
    pub(crate) input_token_count_unsupported: bool,
    pub(crate) api_key: String,
    pub(crate) http: reqwest::Client,
}

impl OpenAiProvider {
    /// All local profile setup checks, without opening a destination connection.
    /// Return the validated routing headers for both HTTP and WebSocket setup.
    pub(crate) fn validate_profile_settings(
        profile: &ResolvedModelProfile,
        reasoning_level: ReasoningLevel,
        session_id: &str,
        cache_key: &str,
    ) -> anyhow::Result<HeaderMap> {
        anyhow::ensure!(
            profile.reasoning_levels.contains(&reasoning_level),
            "reasoning level {reasoning_level} is not configured for {}",
            profile.profile
        );
        let endpoint = &profile.endpoint;
        endpoint.web_search.validate(&profile.profile.provider)?;
        validate_prompt_cache_key(endpoint.compatibility.send_prompt_cache_key, cache_key)?;
        let session_headers = session_routing_headers(profile, session_id)?;
        let endpoints = OpenAiResponseEndpoints::parse(&endpoint.base_url)?;
        HeaderValue::from_str(&format!("Bearer {}", endpoint.api_key())).map_err(|_| {
            anyhow::anyhow!(
                "profile {} has an invalid credential header",
                profile.profile
            )
        })?;
        resolve_input_token_count_url(&endpoints.http, &endpoint.input_token_count)?;
        if let Some(url) = &endpoint.compaction.url {
            reqwest::Url::parse(url)?;
        }
        crate::config::validate_additional_params(
            &profile.profile.provider,
            &endpoint.additional_params,
        )?;
        Ok(session_headers)
    }

    /// Build the Rig model and make one cancellable, best-effort WebSocket
    /// preconnect. HTTP 426 selects HTTP immediately; other handshake failures
    /// leave a disconnected WebSocket preference so the first request gets the
    /// normal bounded reconnect budget before falling back. The `cache_key` is
    /// retained per provider and sent as `prompt_cache_key` when compatibility
    /// settings allow it. The separate `session_id` is the persistent conversation
    /// ID, sent only under the provider's configured `session_id_header` name.
    pub async fn connect(
        profile: &ResolvedModelProfile,
        reasoning_level: ReasoningLevel,
        preamble: &str,
        tools: ToolServerHandle,
        session_id: &str,
        cache_key: &str,
    ) -> anyhow::Result<Self> {
        let session_headers =
            Self::validate_profile_settings(profile, reasoning_level, session_id, cache_key)?;
        let endpoint = &profile.endpoint;
        let endpoints = OpenAiResponseEndpoints::parse(&endpoint.base_url)?;
        let client = openai::Client::builder()
            .base_url(endpoints.http.as_str())
            .api_key(endpoint.api_key())
            .build()
            .map_err(anyhow::Error::from)?;

        let http = reqwest::Client::builder()
            .default_headers(session_headers.clone())
            .build()?;
        let mut websocket_headers = client.headers().clone();
        websocket_headers.extend(session_headers);
        let websocket_config = OpenAiWebSocketConfig {
            url: endpoints.websocket.to_string(),
            headers: websocket_headers,
        };
        let websocket_log_url = redacted_url_for_logging(&websocket_config.url);
        let http_log_url = redacted_url_for_logging(endpoints.http.as_str());
        let (session, transport, websocket_connection_request_id) = if endpoint.supports_websockets
        {
            match websocket_config.connect().await {
                Ok(connection) => (
                    OpenAiWebSocketSession::new(connection.socket),
                    OpenAiTransport::WebSocket,
                    connection.websocket_connection_request_id,
                ),
                Err(error) if is_http_upgrade_required(&error) => {
                    tracing::info!(
                        websocket_url = %websocket_log_url,
                        http_url = %http_log_url,
                        status = 426,
                        "OpenAI WebSocket upgrade unavailable; selecting HTTP transport"
                    );
                    (
                        OpenAiWebSocketSession::disconnected(
                            crate::websocket_session::OpenAiWebSocketTerminalCategory::HttpFallback,
                        ),
                        OpenAiTransport::Http,
                        None,
                    )
                }
                Err(error) => {
                    tracing::warn!(
                        websocket_url = %websocket_log_url,
                        error = %error,
                        "OpenAI startup WebSocket preconnect failed; the first request will retry"
                    );
                    (
                        OpenAiWebSocketSession::disconnected(
                            crate::websocket_session::OpenAiWebSocketTerminalCategory::StartupConnectFailed,
                        ),
                        OpenAiTransport::WebSocket,
                        None,
                    )
                }
            }
        } else {
            tracing::info!(
                http_url = %http_log_url,
                "OpenAI WebSocket support disabled; selecting HTTP transport"
            );
            (
                OpenAiWebSocketSession::disconnected(
                    crate::websocket_session::OpenAiWebSocketTerminalCategory::HttpFallback,
                ),
                OpenAiTransport::Http,
                None,
            )
        };

        let responses_parameters = AdditionalParameters {
            include: endpoint
                .compatibility
                .send_reasoning_encrypted_content
                .then(|| vec![Include::ReasoningEncryptedContent]),
            prompt_cache_key: endpoint
                .compatibility
                .send_prompt_cache_key
                .then(|| cache_key.to_string()),
            ..Default::default()
        };
        let model = client.completion_model(&profile.profile.model);
        let input_token_count_url =
            resolve_input_token_count_url(&endpoints.http, &endpoint.input_token_count)?;

        Ok(Self {
            #[cfg(feature = "cache-diagnostics")]
            cache_diagnostics: crate::cache_diagnostics::DiagnosticState::configured(
                endpoint.session_id_header.as_deref(),
                transport == OpenAiTransport::Http && endpoint.supports_websockets,
            ),
            profile: profile.profile.clone(),
            model,
            context_window_tokens: profile.context_window_tokens,
            input_token_limit: profile.input_token_limit,
            preamble: preamble.to_string(),
            responses_parameters: Some(responses_parameters.to_json()),
            reasoning_level,
            reasoning_summary_level: profile.reasoning_summary_level.clone(),
            compatibility: endpoint.compatibility.clone(),
            web_search: endpoint.web_search.clone(),
            additional_params: endpoint.additional_params.clone(),
            prompt_cache_key: cache_key.to_string(),
            tools,
            ws: OpenAiParkedWebSocket {
                session,
                config: websocket_config,
                socket_generation: 0,
                websocket_connection_request_id,
                continuation: None,
                pending_done_response_id: None,
                last_activity: std::time::Instant::now(),
            },
            responses_url: endpoints.http,
            transport,
            compaction_url: endpoint
                .compaction
                .url
                .as_deref()
                .map(reqwest::Url::parse)
                .transpose()?,
            compaction_timeout: std::time::Duration::from_secs(
                endpoint.compaction.request_timeout_seconds,
            ),
            input_token_count_url,
            input_token_count_timeout: std::time::Duration::from_secs(
                endpoint.input_token_count.request_timeout_seconds,
            ),
            input_token_count_unsupported: false,
            api_key: endpoint.api_key().to_string(),
            http,
        })
    }

    pub(crate) fn set_reasoning_level(&mut self, level: ReasoningLevel) {
        self.reasoning_level = level;
    }

    pub(crate) fn prepared_responses_parameters(&self) -> Option<serde_json::Value> {
        debug_assert_eq!(
            self.responses_parameters
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .and_then(|parameters| parameters.get("prompt_cache_key"))
                .and_then(serde_json::Value::as_str),
            self.compatibility
                .send_prompt_cache_key
                .then_some(self.prompt_cache_key.as_str())
        );
        let mut parameters = self
            .responses_parameters
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        if self.compatibility.send_reasoning {
            let reasoning = AdditionalParameters {
                reasoning: Some(
                    Reasoning::new()
                        .with_effort(crate::config::reasoning_effort(self.reasoning_level))
                        .with_summary_level(self.reasoning_summary_level.clone()),
                ),
                ..Default::default()
            }
            .to_json();
            parameters["reasoning"] = reasoning["reasoning"].clone();
        }
        Some(parameters)
    }

    pub(crate) fn transmitted_prompt_cache_key_fingerprint(&self) -> Option<String> {
        self.compatibility
            .send_prompt_cache_key
            .then(|| prompt_cache_key_fingerprint(&self.prompt_cache_key))
    }

    pub(crate) fn switch_to_http(&mut self, reason: &'static str) {
        if self.transport == OpenAiTransport::Http {
            return;
        }
        self.transport = OpenAiTransport::Http;
        self.ws
            .session
            .terminate(crate::websocket_session::OpenAiWebSocketTerminalCategory::HttpFallback);
        self.ws.invalidate_continuation(reason);
        self.ws.pending_done_response_id = None;
        #[cfg(feature = "cache-diagnostics")]
        crate::cache_diagnostics::transport_fallback(self);
        let http_log_url = redacted_url_for_logging(self.responses_url.as_str());
        tracing::warn!(
            reason,
            http_url = %http_log_url,
            "OpenAI transport switched permanently to HTTP for this provider"
        );
    }
}
