//! Responses transport lifecycle and provider construction.

use crate::diagnostics::Diagnostics;
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
use zevria_model::openai::ReasoningSummaryLevel;

use crate::{
    config::{InputTokenCountConfig, ResolvedModelProfile, ResponsesCompatibilityConfig},
    credential::{
        ApiKeyCommandError, ApiKeySource, CredentialRefresh, Unauthorized, rejected_after_refresh,
    },
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
        profile.session_id_header(),
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
    Diagnostics::identifier(value)
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
    resolve_input_token_count_url_with_suffix(responses_url, config, "/input_tokens")
}

pub(crate) fn resolve_input_token_count_url_with_suffix(
    responses_url: &reqwest::Url,
    config: &InputTokenCountConfig,
    suffix: &str,
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
    let path = format!("{}{suffix}", url.path().trim_end_matches('/'));
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
    pub(crate) request_properties: std::sync::Arc<serde_json::Value>,
    pub(crate) lineage: crate::responses::ContinuationLineage,
}

/// Everything needed to (re)establish the OpenAI Responses websocket. The base
/// URL and headers are captured once at startup so the adapter can rebuild the
/// connection after the server drops an idle socket. Authentication is resolved
/// only at handshake time; key expiry never interrupts a healthy socket.
pub(crate) struct OpenAiWebSocketConfig {
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    pub(crate) api_key: ApiKeySource,
    pub(crate) provider: String,
    pub(crate) connect_timeout: std::time::Duration,
    pub(crate) keepalive: std::time::Duration,
    pub(crate) prewarm: bool,
}

pub(crate) struct OpenAiWebSocketConnection {
    pub(crate) socket: OpenAiWebSocketStream,
    pub(crate) websocket_connection_request_id: Option<String>,
}

impl OpenAiWebSocketConfig {
    pub(crate) async fn connect(&self) -> anyhow::Result<OpenAiWebSocketConnection> {
        self.connect_with_refresh(&mut CredentialRefresh::default())
            .await
    }

    async fn connect_with_refresh(
        &self,
        refresh: &mut CredentialRefresh,
    ) -> anyhow::Result<OpenAiWebSocketConnection> {
        let started = std::time::Instant::now();
        let log_url = redacted_url_for_logging(&self.url);
        tracing::debug!(url = %log_url, "connecting to the OpenAI websocket");
        let (socket, response) = refresh.exchange(
            &self.api_key,
            &self.provider,
            |lease| async move {
                let mut request = self.url.as_str().into_client_request()?;
                for (name, value) in &self.headers {
                    request.headers_mut().insert(name, value.clone());
                }
                let mut authorization = HeaderValue::from_str(&format!("Bearer {}", lease.expose()))?;
                authorization.set_sensitive(true);
                request.headers_mut().insert("authorization", authorization);
                tokio::time::timeout(self.connect_timeout, connect_async(request))
                    .await
                    .map_err(|_| anyhow::anyhow!(
                        "connecting to the OpenAI websocket timed out after {}s",
                        self.connect_timeout.as_secs()
                    ))?
                    .map_err(anyhow::Error::from)
            },
            |result| {
                match result.as_ref().err()?.downcast_ref::<WebSocketError>()? {
                    WebSocketError::Http(response) if response.status() == StatusCode::UNAUTHORIZED => {
                        Some(Unauthorized { upstream_request_id: response_request_id(response.headers()) })
                    }
                    _ => None,
                }
            },
        ).await?.map_err(|error| {
            if self.api_key.is_refreshable() && matches!(error.downcast_ref::<WebSocketError>(),
                Some(WebSocketError::Http(response)) if response.status() == StatusCode::UNAUTHORIZED)
            {
                error.context(rejected_after_refresh(&self.provider))
            } else { error }
        })?;
        let websocket_connection_request_id = response_request_id(response.headers());
        tracing::info!(
            url = %log_url,
            connect_ms = started.elapsed().as_millis() as u64,
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

/// Upstream auth rejection and invalid routing/configuration are not recovery
/// signals. Local helper failures follow the helper's own classification.
pub(crate) fn retryable_connect_error(error: &anyhow::Error) -> bool {
    if let Some(helper) = error.downcast_ref::<ApiKeyCommandError>() {
        return helper.is_transient();
    }
    match error.downcast_ref::<WebSocketError>() {
        Some(WebSocketError::Http(response)) => response.status().is_server_error(),
        Some(WebSocketError::Url(_) | WebSocketError::HttpFormat(_)) => false,
        _ => true,
    }
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
    /// One-shot reconnect outside completion recovery.
    pub(crate) async fn reconnect(&mut self) -> anyhow::Result<()> {
        self.reconnect_with_refresh(&mut CredentialRefresh::default())
            .await
    }

    /// Replace the parked socket with a freshly connected one.
    /// Continuation IDs are socket-scoped, so a successful replacement starts
    /// a new generation and forces the next request to replay complete local
    /// history. A failed handshake leaves the old socket and all its state
    /// intact.
    pub(crate) async fn reconnect_with_refresh(
        &mut self,
        refresh: &mut CredentialRefresh,
    ) -> anyhow::Result<()> {
        let next_generation = if self.session.terminal_status()
            == Some(crate::websocket_session::OpenAiWebSocketTerminalCategory::NotConnected)
        {
            // Lazy first establishment is generation zero, not a replacement.
            self.socket_generation
        } else {
            self.socket_generation.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("the OpenAI websocket generation counter was exhausted")
            })?
        };
        let connection = self.config.connect_with_refresh(refresh).await?;
        let session = OpenAiWebSocketSession::new(connection.socket, self.config.keepalive);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpFallback {
    Sticky(&'static str),
    Provisional,
}

/// The parked socket state is kept across a provisional HTTP fallback, but
/// only HTTP can carry a fallback classification.
#[derive(Clone, Copy)]
enum ResponsesRoute {
    WebSocket,
    Http { fallback: Option<HttpFallback> },
}

pub(crate) struct ResponsesTransport {
    pub(crate) ws: OpenAiParkedWebSocket,
    route: ResponsesRoute,
}

impl ResponsesTransport {
    pub(crate) fn new(ws: OpenAiParkedWebSocket, transport: OpenAiTransport) -> Self {
        let route = match transport {
            OpenAiTransport::WebSocket => ResponsesRoute::WebSocket,
            OpenAiTransport::Http => ResponsesRoute::Http { fallback: None },
        };
        Self { ws, route }
    }

    pub(crate) fn kind(&self) -> OpenAiTransport {
        match self.route {
            ResponsesRoute::WebSocket => OpenAiTransport::WebSocket,
            ResponsesRoute::Http { .. } => OpenAiTransport::Http,
        }
    }

    pub(crate) fn fallback(&self) -> Option<HttpFallback> {
        match self.route {
            ResponsesRoute::WebSocket => None,
            ResponsesRoute::Http { fallback } => fallback,
        }
    }

    pub(crate) fn prefer_websocket(&mut self) {
        self.route = ResponsesRoute::WebSocket;
    }

    pub(crate) fn confirm_fallback(&mut self) {
        if self.fallback() == Some(HttpFallback::Provisional) {
            self.route = ResponsesRoute::Http {
                fallback: Some(HttpFallback::Sticky("websocket_retries_exhausted")),
            };
        }
    }
}

/// A live single-profile Responses runtime with an independent continuation
/// chain and transport recovery state.
pub struct OpenAiProvider {
    pub(crate) cache_diagnostics: crate::diagnostics::Diagnostics,
    pub(crate) profile: ModelProfileRef,
    pub(crate) slot_cache: std::sync::Arc<std::sync::Mutex<crate::router::SlotCache>>,
    pub(crate) request_properties_cache: Option<crate::responses::PropertiesCache>,
    pub(crate) image_accounting: zevria_foundation::image_accounting::ImageAccountingPolicy,
    #[allow(dead_code)]
    pub(crate) context_window_tokens: u64,
    pub(crate) preamble: String,
    pub(crate) responses_parameters: Option<serde_json::Value>,
    pub(crate) reasoning_level: ReasoningLevel,
    pub(crate) reasoning_summary_level: ReasoningSummaryLevel,
    pub(crate) compatibility: ResponsesCompatibilityConfig,
    pub(crate) web_search: crate::WebSearchConfig,
    pub(crate) additional_params: std::collections::BTreeMap<String, serde_json::Value>,
    pub(crate) prompt_cache_key: String,
    pub(crate) responses_transport: ResponsesTransport,
    pub(crate) responses_url: reqwest::Url,
    pub(crate) recovery: crate::recovery::RecoveryPolicy,
    pub(crate) compaction_url: Option<reqwest::Url>,
    pub(crate) compaction_timeout: std::time::Duration,
    pub(crate) input_token_count_url: Option<reqwest::Url>,
    pub(crate) input_token_count_timeout: std::time::Duration,
    pub(crate) input_token_count_unsupported: bool,
    pub(crate) api_key: ApiKeySource,
    pub(crate) http: reqwest::Client,
    /// Token counting/remote compaction retain their own request deadlines.
    pub(crate) maintenance_http: std::sync::OnceLock<reqwest::Client>,
}

impl OpenAiProvider {
    pub(crate) fn transport(&self) -> OpenAiTransport {
        self.responses_transport.kind()
    }

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
        let endpoint = profile.responses_endpoint()?;
        endpoint.network.validate(&profile.profile.provider)?;
        endpoint.web_search.validate(&profile.profile.provider)?;
        validate_prompt_cache_key(endpoint.compatibility.send_prompt_cache_key, cache_key)?;
        let session_headers = session_routing_headers(profile, session_id)?;
        let endpoints = OpenAiResponseEndpoints::parse(&endpoint.base_url)?;
        endpoint.api_key().validate(&profile.profile.provider)?;
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

    /// Prepare the transport and make one cancellable, best-effort WebSocket
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
        session_id: &str,
        cache_key: &str,
    ) -> anyhow::Result<Self> {
        let mut provider =
            Self::unconnected(profile, reasoning_level, preamble, session_id, cache_key)?;
        if provider.transport() == OpenAiTransport::WebSocket {
            match provider.responses_transport.ws.config.connect().await {
                Ok(connection) => {
                    provider.responses_transport.ws.session = OpenAiWebSocketSession::new(
                        connection.socket,
                        provider.recovery.websocket_keepalive,
                    );
                    provider
                        .responses_transport
                        .ws
                        .websocket_connection_request_id =
                        connection.websocket_connection_request_id;
                }
                Err(error) if is_http_upgrade_required(&error) => {
                    provider.switch_to_http("websocket_upgrade_required")
                }
                Err(error) if !retryable_connect_error(&error) => return Err(error),
                Err(error) => {
                    tracing::warn!(error = %error, "OpenAI startup WebSocket preconnect failed; the first request will retry");
                    provider.responses_transport.ws.session = OpenAiWebSocketSession::disconnected(crate::websocket_session::OpenAiWebSocketTerminalCategory::StartupConnectFailed);
                }
            }
        }
        Ok(provider)
    }

    /// Local validation only; the first completion owns the handshake.
    pub(crate) fn unconnected(
        profile: &ResolvedModelProfile,
        reasoning_level: ReasoningLevel,
        preamble: &str,
        session_id: &str,
        cache_key: &str,
    ) -> anyhow::Result<Self> {
        let session_headers =
            Self::validate_profile_settings(profile, reasoning_level, session_id, cache_key)?;
        let endpoint = profile.responses_endpoint()?;
        let endpoints = OpenAiResponseEndpoints::parse(&endpoint.base_url)?;
        let recovery = crate::recovery::RecoveryPolicy::from(&endpoint.network);
        let http = reqwest::Client::builder()
            .connect_timeout(recovery.connect_timeout)
            .default_headers(session_headers.clone())
            .build()?;
        let websocket_config = OpenAiWebSocketConfig {
            url: endpoints.websocket.to_string(),
            headers: session_headers,
            api_key: endpoint.api_key().clone(),
            provider: profile.profile.provider.clone(),
            connect_timeout: recovery.connect_timeout,
            keepalive: recovery.websocket_keepalive,
            prewarm: endpoint.network.prewarm,
        };
        let (terminal, transport) = if endpoint.supports_websockets {
            (
                crate::websocket_session::OpenAiWebSocketTerminalCategory::NotConnected,
                OpenAiTransport::WebSocket,
            )
        } else {
            tracing::info!(http_url = %redacted_url_for_logging(endpoints.http.as_str()), "OpenAI WebSocket support disabled; selecting HTTP transport");
            (
                crate::websocket_session::OpenAiWebSocketTerminalCategory::HttpFallback,
                OpenAiTransport::Http,
            )
        };

        let mut responses_parameters = serde_json::json!({});
        if endpoint.compatibility.send_reasoning_encrypted_content {
            responses_parameters["include"] = serde_json::json!(["reasoning.encrypted_content"]);
        }
        if endpoint.compatibility.send_prompt_cache_key {
            responses_parameters["prompt_cache_key"] = cache_key.into();
        }
        responses_parameters["parallel_tool_calls"] =
            endpoint.compatibility.parallel_tool_calls.into();
        let input_token_count_url =
            resolve_input_token_count_url(&endpoints.http, &endpoint.input_token_count)?;

        Ok(Self {
            cache_diagnostics: crate::diagnostics::Diagnostics::configured(
                endpoint.session_id_header.as_deref(),
                transport == OpenAiTransport::Http && endpoint.supports_websockets,
            ),
            profile: profile.profile.clone(),
            slot_cache: Default::default(),
            request_properties_cache: None,
            image_accounting: profile.image_accounting.clone(),
            context_window_tokens: profile.context_window_tokens,
            preamble: preamble.to_string(),
            responses_parameters: Some(responses_parameters),
            reasoning_level,
            reasoning_summary_level: match &profile.protocol {
                crate::config::ProfileProtocol::Responses {
                    reasoning_summary_level,
                    ..
                } => *reasoning_summary_level,
                _ => unreachable!("validated Responses profile"),
            },
            compatibility: endpoint.compatibility.clone(),
            web_search: endpoint.web_search.clone(),
            additional_params: endpoint.additional_params.clone(),
            prompt_cache_key: cache_key.to_string(),
            responses_transport: ResponsesTransport::new(
                OpenAiParkedWebSocket {
                    session: OpenAiWebSocketSession::disconnected(terminal),
                    config: websocket_config,
                    socket_generation: 0,
                    websocket_connection_request_id: None,
                    continuation: None,
                    pending_done_response_id: None,
                    last_activity: std::time::Instant::now(),
                },
                transport,
            ),
            responses_url: endpoints.http,
            recovery,
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
            api_key: endpoint.api_key().clone(),
            http,
            maintenance_http: Default::default(),
        })
    }

    pub(crate) fn maintenance_client(&self) -> anyhow::Result<&reqwest::Client> {
        if let Some(client) = self.maintenance_http.get() {
            return Ok(client);
        }
        let client = reqwest::Client::builder()
            .default_headers(self.responses_transport.ws.config.headers.clone())
            .build()?;
        Ok(self.maintenance_http.get_or_init(|| client))
    }

    /// One best-effort handshake, owned by the engine's idle select. Dropping
    /// this future before completion cannot install a socket or spend the next
    /// completion's recovery budget. HTTP-only/fallback profiles do nothing.
    pub(crate) async fn prewarm_connection(&mut self) {
        if !self.responses_transport.ws.config.prewarm
            || self.transport() != OpenAiTransport::WebSocket
            || self
                .responses_transport
                .ws
                .session
                .terminal_status()
                .is_none()
        {
            return;
        }
        match self.responses_transport.ws.reconnect().await {
            Ok(()) => {}
            Err(error) if is_http_upgrade_required(&error) => {
                self.switch_to_http("websocket_upgrade_required");
            }
            Err(error) => {
                tracing::warn!(
                    provider = %self.profile.provider,
                    model = %self.profile.model,
                    error = %error,
                    "OpenAI WebSocket prewarm failed; the first request retains its recovery budget"
                );
            }
        }
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
            parameters["reasoning"] = serde_json::json!({
                "effort": crate::config::reasoning_effort(self.reasoning_level),
                "summary": self.reasoning_summary_level,
            });
        }
        Some(parameters)
    }

    pub(crate) fn transmitted_prompt_cache_key_fingerprint(&self) -> Option<String> {
        self.compatibility
            .send_prompt_cache_key
            .then(|| prompt_cache_key_fingerprint(&self.prompt_cache_key))
    }

    pub(crate) fn switch_to_http(&mut self, reason: &'static str) {
        if self.transport() == OpenAiTransport::Http {
            return;
        }
        self.responses_transport.route = ResponsesRoute::Http {
            fallback: Some(if reason == "websocket_retries_exhausted" {
                HttpFallback::Provisional
            } else {
                HttpFallback::Sticky(reason)
            }),
        };
        self.responses_transport
            .ws
            .session
            .terminate(crate::websocket_session::OpenAiWebSocketTerminalCategory::HttpFallback);
        self.responses_transport.ws.invalidate_continuation(reason);
        self.responses_transport.ws.pending_done_response_id = None;
        Diagnostics::transport_fallback(self);
        let http_log_url = redacted_url_for_logging(self.responses_url.as_str());
        tracing::warn!(
            reason,
            sticky = matches!(self.responses_transport.fallback(), Some(HttpFallback::Sticky(_))),
            http_url = %http_log_url,
            "OpenAI transport switched to HTTP for this provider"
        );
    }
}
