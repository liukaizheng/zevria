//! TEMPORARY: opt-in cache investigation. Delete using docs/cache-diagnostics.md.
//! No result from this module participates in provider decisions.
mod fingerprint;
mod persistence;
mod response;
pub(crate) mod socket;
#[cfg(test)]
mod tests;

use crate::{
    connection::{OpenAiProvider, OpenAiTransport},
    turn::PreparedRequest,
};
use fingerprint::{Comparison, Fingerprint, ItemFingerprint, Properties, compare, ordered};
pub use persistence::CacheDiagnosticContext;
use persistence::{Invalidation, MAX_ITEMS, Store};
use response::{ComparisonOutcome, Echo, NativeCounts, Observation, ReportedInput, Usage};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};
use zevria_model::{ModelResponse, TokenUsage};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    authority: CompletionAuthority,
    profile: Fingerprint,
    properties: Properties,
    items: Vec<ItemFingerprint>,
    meta: Completion,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompletionAuthority {
    ValidatedProviderCompletionNotTranscriptDurability,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    runtime: uuid::Uuid,
    request_id: u64,
    completed_at_ms: u64,
    input_count: usize,
    output_count: usize,
    historical_native: NativeCounts,
    current_native: NativeCounts,
    socket_generation: u64,
    wire: Option<Wire>,
    route: Route,
    raw: Option<Observation>,
    usage: Option<Usage>,
    upstream_request_id: Option<String>,
    connection_request_id: Option<String>,
}
impl Completion {
    fn valid(&self) -> bool {
        self.input_count <= MAX_ITEMS
            && self.output_count <= MAX_ITEMS
            && self.historical_native.valid(self.input_count)
            && self.current_native.valid(self.output_count)
            && self.request_id != 0
            && !self.runtime.is_nil()
            && self.completed_at_ms != 0
            && self.raw.as_ref().is_none_or(Observation::valid)
            && [
                &self.upstream_request_id,
                &self.connection_request_id,
                &self.route.header_name,
            ]
            .into_iter()
            .flatten()
            .all(|s| response::safe(s))
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    endpoint: Fingerprint,
    header_name: Option<String>,
    header_identity: Option<Fingerprint>,
    header_value: Option<Fingerprint>,
}
impl Route {
    fn new(openai: &OpenAiProvider) -> Self {
        let name = openai.cache_diagnostics.routing_header.as_deref();
        Self {
            endpoint: fingerprint::bytes(openai.responses_url.as_str().as_bytes()),
            header_name: name.map(|s| identifier(s).to_owned()),
            header_identity: name.map(|s| fingerprint::bytes(s.as_bytes())),
            header_value: name
                .and_then(|n| openai.ws.config.headers.get(n))
                .map(|v| fingerprint::bytes(v.as_bytes())),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FullReason {
    FreshRuntime,
    NoContinuation,
    Reconnect,
    RequestPropertiesChanged,
    HistoryMismatch,
    MissingNativeOutput,
    StaleResponseId,
    Http,
    HttpRetry,
    HttpFallback,
}
impl FullReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::FreshRuntime => "fresh_runtime_no_continuation",
            Self::NoContinuation => "no_continuation",
            Self::Reconnect => "reconnect",
            Self::RequestPropertiesChanged => "request_properties_changed",
            Self::HistoryMismatch => "history_mismatch",
            Self::MissingNativeOutput => "missing_native_output",
            Self::StaleResponseId => "stale_response_id",
            Self::Http => "http_full",
            Self::HttpRetry => "http_retry",
            Self::HttpFallback => "http_fallback",
        }
    }
    pub(crate) fn selection(reason: &str) -> Self {
        match reason {
            "reconnect" => Self::Reconnect,
            "request_properties_changed" => Self::RequestPropertiesChanged,
            "history_mismatch" => Self::HistoryMismatch,
            "missing_native_output" => Self::MissingNativeOutput,
            _ => Self::NoContinuation,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Transport {
    Websocket,
    Http,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    transport: Transport,
    incremental: bool,
    full_reason: Option<FullReason>,
    consistent: Option<bool>,
    cache_key: Option<Fingerprint>,
}
struct SeenTerminal {
    request_id: u64,
    socket_generation: u64,
    raw: Observation,
}

pub(crate) struct DiagnosticState {
    runtime: uuid::Uuid,
    store: Option<Store>,
    baseline: Option<Baseline>,
    source: &'static str,
    epoch: u64,
    missing_reason: &'static str,
    current_request: Option<u64>,
    routing_header: Option<String>,
    http_fallback: bool,
    wire: Option<Wire>,
    sends: u64,
    raw: Option<Observation>,
    usage: Option<Usage>,
    projection_difference: Option<bool>,
    upstream_request_id: Option<String>,
    recent: VecDeque<SeenTerminal>,
}
impl Default for DiagnosticState {
    fn default() -> Self {
        Self {
            runtime: uuid::Uuid::new_v4(),
            store: None,
            baseline: None,
            source: "none",
            epoch: 0,
            missing_reason: "new_provider",
            current_request: None,
            routing_header: None,
            http_fallback: false,
            wire: None,
            sends: 0,
            raw: None,
            usage: None,
            projection_difference: None,
            upstream_request_id: None,
            recent: VecDeque::new(),
        }
    }
}
pub(crate) struct RequestSnapshot {
    id: u64,
    epoch: u64,
    properties: Properties,
    items: Vec<ItemFingerprint>,
    input_count: usize,
    full_input_hash: Option<Fingerprint>,
    historical_native: NativeCounts,
    provider_comparison_requested: Echo,
    comparison: Comparison,
    route: Route,
}

// Never format whole errors, properties, headers, native items, or cache keys.
pub(crate) fn identifier(value: &str) -> &str {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
    {
        value
    } else {
        "<redacted>"
    }
}

pub(crate) fn configure(openai: &mut OpenAiProvider, context: &CacheDiagnosticContext) {
    let store = context.store(&openai.profile);
    let state = &mut openai.cache_diagnostics;
    state.runtime = context.runtime_id;
    match store.load() {
        Ok(baseline) => {
            state.baseline = Some(baseline);
            state.source = "persisted";
        }
        Err(reason) => {
            state.baseline = None;
            state.source = "none";
            state.missing_reason = reason;
        }
    }
    state.store = Some(store);
    connection(openai, "initialized");
}
impl DiagnosticState {
    pub(crate) fn configured(name: Option<&str>, http_fallback: bool) -> Self {
        Self {
            routing_header: name.map(|s| s.to_ascii_lowercase()),
            http_fallback,
            ..Self::default()
        }
    }
}
pub(crate) fn invalidate_uninitialized(
    context: &CacheDiagnosticContext,
    profile: &zevria_foundation::ModelProfileRef,
) {
    let result = context.store(profile).invalidate(Invalidation::Reset);
    tracing::info!(diagnostic_event = "reset", runtime_id = %context.runtime_id,
        provider = identifier(&profile.provider), model = identifier(&profile.model),
        reason = "engine_reset_uninitialized", persistence_error = ?result.err(), "cache diagnostics");
}

pub(crate) fn dispatch(
    mut prepared: PreparedRequest,
    openai: &mut OpenAiProvider,
) -> PreparedRequest {
    connection(openai, "request_connection");
    let properties = Properties::new(&prepared.request_properties);
    let items: Vec<_> = if prepared.full_input.len() <= MAX_ITEMS {
        prepared
            .full_input
            .iter()
            .map(ItemFingerprint::new)
            .collect()
    } else {
        Vec::new()
    };
    if openai
        .cache_diagnostics
        .baseline
        .as_ref()
        .is_some_and(|old| old.profile != persistence::profile_identity(&openai.profile))
    {
        reset(openai, "profile_changed");
    }
    let route = Route::new(openai);
    let state = &mut openai.cache_diagnostics;
    let within_limit = prepared.full_input.len() <= MAX_ITEMS;
    let comparison = compare(
        state
            .baseline
            .as_ref()
            .filter(|_| within_limit)
            .map(|old| (old.items.as_slice(), old.properties)),
        &items,
        properties,
    );
    let snapshot = RequestSnapshot {
        id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
        epoch: state.epoch,
        properties,
        input_count: prepared.full_input.len(),
        full_input_hash: within_limit.then(|| ordered(&items)),
        historical_native: NativeCounts::from_items(&prepared.full_input),
        provider_comparison_requested: Echo::at(
            prepared
                .request_properties
                .get("prompt_cache_options")
                .and_then(|v| v.get("comparison_response_id")),
        ),
        items,
        comparison,
        route,
    };
    state.current_request = Some(snapshot.id);
    state.wire = None;
    state.sends = 0;
    state.raw = None;
    state.usage = None;
    state.projection_difference = None;
    state.upstream_request_id = None;
    tracing::info!(
        diagnostic_event = "prepared", runtime_id = %state.runtime, local_request_id = snapshot.id, reset_epoch = snapshot.epoch,
        baseline_source = state.source, previous_runtime_id = ?state.baseline.as_ref().map(|b| b.meta.runtime),
        baseline_request_id = ?state.baseline.as_ref().map(|b| b.meta.request_id),
        missing_baseline_reason = state.baseline.is_none().then_some(state.missing_reason),
        comparison_unavailable = (!within_limit).then_some("snapshot_item_limit"),
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        transport = openai.transport.as_str(), socket_generation = openai.ws.socket_generation,
        instructions_hash = %properties.instructions, tools_hash = %properties.tools, remaining_properties_hash = %properties.remaining,
        full_input_hash = ?snapshot.full_input_hash.map(|h| h.to_string()), input_count = snapshot.input_count,
        prefix_status = comparison.status, baseline_count = ?comparison.baseline_count,
        matched_item_count = comparison.matched_count, first_difference = ?comparison.first_difference,
        previous_item_kind = ?comparison.previous_kind, next_item_kind = ?comparison.next_kind,
        instructions_changed = ?comparison.instructions_changed, tools_changed = ?comparison.tools_changed,
        remaining_properties_changed = ?comparison.properties_changed, "cache diagnostics"
    );
    prepared.cache_diagnostics = Some(snapshot);
    prepared
}

pub(crate) fn http_reason(openai: &OpenAiProvider, attempt: usize) -> FullReason {
    if attempt > 1 {
        FullReason::HttpRetry
    } else if openai.cache_diagnostics.http_fallback {
        FullReason::HttpFallback
    } else {
        FullReason::Http
    }
}
pub(crate) fn transport_fallback(openai: &mut OpenAiProvider) {
    openai.cache_diagnostics.http_fallback = true;
    connection(openai, "http_fallback");
}

pub(crate) fn no_continuation_reason(openai: &OpenAiProvider) -> FullReason {
    if openai.ws.socket_generation > 0 {
        FullReason::Reconnect
    } else if openai.cache_diagnostics.recent.is_empty()
        && openai.cache_diagnostics.source != "memory"
    {
        FullReason::FreshRuntime
    } else {
        FullReason::NoContinuation
    }
}

// Inspect the already serialized payload, not an alternative serialization.
// Large bodies are still hashed in full; parsing is bounded and explicitly unknown.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transmission(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    mode: &'static str,
    count: usize,
    previous: Option<&str>,
    reason: Option<FullReason>,
    payload: Option<&[u8]>,
) {
    let Some(snapshot) = &prepared.cache_diagnostics else {
        return;
    };
    let http = openai.transport == OpenAiTransport::Http;
    let parsed = payload
        .filter(|p| p.len() <= 16 * 1024 * 1024)
        .and_then(|p| serde_json::from_slice::<serde_json::Value>(p).ok());
    let mut actual = parsed.and_then(|v| match v {
        serde_json::Value::Object(object) => Some(object),
        _ => None,
    });
    let cache_key = actual
        .as_ref()
        .map(|v| fingerprint::optional(v.get("prompt_cache_key")));
    let consistent = actual.as_mut().map(|actual| {
        let input = actual.remove("input");
        let prev = actual.remove("previous_response_id");
        let kind = actual.remove("type");
        let mut expected = prepared
            .request_properties
            .as_object()
            .cloned()
            .unwrap_or_default();
        let framing = if http {
            expected.remove("type");
            expected.remove("stream");
            kind.is_none() && actual.remove("stream") == Some(serde_json::json!(true))
        } else {
            kind == Some(serde_json::json!("response.create"))
        };
        let expected_input = if mode == "incremental" {
            prepared
                .full_input
                .get(prepared.full_input.len().saturating_sub(count)..)
        } else {
            Some(prepared.full_input.as_slice())
        };
        framing
            && expected_input.is_some_and(|items| items.len() == count)
            && previous.is_some() == (mode == "incremental")
            && *actual == expected
            && input.as_ref().and_then(|v| v.as_array()).map(Vec::as_slice) == expected_input
            && prev.as_ref().and_then(|v| v.as_str()) == previous
            && (previous.is_some() || prev.is_none())
    });
    let state = &mut openai.cache_diagnostics;
    state.sends += 1;
    state.raw = None;
    state.usage = None;
    state.projection_difference = None;
    state.upstream_request_id = None;
    state.wire = Some(Wire {
        transport: if http {
            Transport::Http
        } else {
            Transport::Websocket
        },
        incremental: mode == "incremental",
        full_reason: reason,
        consistent,
        cache_key,
    });
    tracing::info!(diagnostic_event = "transmission", runtime_id = %state.runtime, local_request_id = snapshot.id,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        transport = openai.transport.as_str(), socket_generation = openai.ws.socket_generation, transmission_number = state.sends,
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier),
        request_mode = mode, full_replay_reason = reason.map(FullReason::as_str), transmitted_item_count = count,
        previous_response_id = ?previous.map(identifier), prefix_status = snapshot.comparison.status,
        wire_bytes = ?payload.map(<[u8]>::len), wire_hash = ?payload.map(|p| fingerprint::bytes(p).to_string()),
        transmitted_cache_key_fingerprint = ?cache_key.map(|h| h.to_string()), preparation_to_wire_consistent = ?consistent,
        wire_projection_mismatch = consistent == Some(false),
        wire_inspection_unavailable = consistent.is_none().then_some("body_unavailable_or_inspection_limit"), "cache diagnostics");
}

pub(crate) fn connection(openai: &OpenAiProvider, event: &'static str) {
    let route = Route::new(openai);
    // Paths too may contain secrets. Reveal only scheme/host/port, hash the full URL.
    let endpoint = if openai.transport == OpenAiTransport::WebSocket {
        reqwest::Url::parse(&openai.ws.config.url)
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_else(|_| "<invalid URL>".into())
    } else {
        openai.responses_url.origin().ascii_serialization()
    };
    tracing::info!(diagnostic_event = "connection", connection_event = event, runtime_id = %openai.cache_diagnostics.runtime,
        local_request_id = ?openai.cache_diagnostics.current_request,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        endpoint = identifier(&endpoint), endpoint_fingerprint = %route.endpoint,
        session_routing_header_configured = route.header_name.is_some(), session_routing_header = ?route.header_name,
        session_routing_value_fingerprint = ?route.header_value.map(|h| h.to_string()),
        transport = openai.transport.as_str(), socket_generation = openai.ws.socket_generation,
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier), "cache diagnostics");
}

// Invoked before *existing* suppression. It neither consumes events nor changes
// operational markers/usage. A bounded response-ID ledger owns late terminals.
pub(crate) fn observe_raw(
    openai: &mut OpenAiProvider,
    message: &str,
    prior_id: Option<&str>,
    upstream: Option<&str>,
) {
    if message.len() > 16 * 1024 * 1024 {
        tracing::info!(diagnostic_event = "response_observation_unavailable", runtime_id = %openai.cache_diagnostics.runtime,
            local_request_id = ?openai.cache_diagnostics.current_request, reason = "response_inspection_limit", "cache diagnostics");
        return;
    }
    let Some(raw) = Observation::parse(message) else {
        return;
    };
    let prior_terminal = prior_id.is_some()
        && raw.response_identity == prior_id.map(|id| fingerprint::bytes(id.as_bytes()));
    let state = &mut openai.cache_diagnostics;
    let seen = state.recent.iter().rev().find(|old| {
        raw.response_identity.is_some()
            && old.raw.response_identity == raw.response_identity
            && (Some(old.request_id) == state.current_request
                || (openai.transport == OpenAiTransport::WebSocket
                    && old.socket_generation == openai.ws.socket_generation))
    });
    let owner = seen
        .map(|s| s.request_id)
        .or_else(|| (!prior_terminal).then_some(state.current_request).flatten());
    let generation = seen
        .map(|s| s.socket_generation)
        .unwrap_or(openai.ws.socket_generation);
    let duplicate = prior_terminal || seen.is_some();
    tracing::info!(diagnostic_event = "raw_terminal", runtime_id = %state.runtime, local_request_id = ?owner,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        socket_generation = generation, observed_socket_generation = openai.ws.socket_generation,
        transport = openai.transport.as_str(), event_type = ?raw.event, response_id = ?raw.response_id, returned_model = ?raw.model,
        response_status = ?raw.status, service_tier = ?raw.service_tier,
        raw_input = ?raw.input, raw_cached = ?raw.cached, raw_cache_write = ?raw.cache_write, raw_output = ?raw.output, raw_total = ?raw.total,
        echoed_instructions = ?raw.instructions, echoed_tools = ?raw.tools, echoed_cache_key = ?raw.cache_key,
        upstream_request_id = ?upstream.map(identifier),
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier),
        duplicate_terminal = duplicate, duplicate_counters_differ = ?seen.map(|old| raw.counters_differ(&old.raw)), "cache diagnostics");
    let accounting = raw.accounting();
    tracing::info!(diagnostic_event = "reported_input", runtime_id = %state.runtime, local_request_id = ?owner,
        response_id = ?raw.response_id, duplicate_terminal = duplicate,
        raw_input = ?raw.input, raw_cached = ?raw.cached, raw_cache_write = ?raw.cache_write,
        derived_uncached_input = ?accounting.uncached, cached_input_fraction = ?accounting.fraction,
        reported_input_category = ?accounting.category, "cache diagnostics");
    tracing::info!(diagnostic_event = "provider_comparison", runtime_id = %state.runtime, local_request_id = ?owner,
        response_id = ?raw.response_id, duplicate_terminal = duplicate,
        outcome = ?raw.comparison.outcome, reason = ?raw.comparison.reason,
        comparison_reusable_tokens = ?raw.comparison.comparison_reusable_tokens,
        cache_missed_tokens = ?raw.comparison.cache_missed_tokens,
        conclusive = raw.comparison.conclusive(),
        duplicate_comparison_differs = ?seen.map(|old| old.raw.comparison != raw.comparison), "cache diagnostics");
    if !duplicate {
        state.raw = Some(raw.clone());
        if let Some(request_id) = owner {
            if state.recent.len() == 8 {
                state.recent.pop_front();
            }
            state.recent.push_back(SeenTerminal {
                request_id,
                socket_generation: generation,
                raw,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn terminal(
    openai: &mut OpenAiProvider,
    transport: OpenAiTransport,
    status: &str,
    response_id: Option<&str>,
    upstream_request_id: Option<&str>,
    usage: Option<TokenUsage>,
    cache_write_tokens: Option<u64>,
) {
    let state = &mut openai.cache_diagnostics;
    let Some(id) = state.current_request else {
        return;
    };
    state.usage = usage.map(Usage::from);
    state.upstream_request_id = upstream_request_id.map(|s| identifier(s).to_owned());
    state.projection_difference = state
        .raw
        .as_ref()
        .map(|raw| raw.projection_differs(state.usage, cache_write_tokens));
    tracing::info!(diagnostic_event = "terminal", runtime_id = %state.runtime, local_request_id = id,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        transport = transport.as_str(), socket_generation = openai.ws.socket_generation,
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier),
        response_status = identifier(status), response_id = ?response_id.map(identifier), upstream_request_id = ?state.upstream_request_id,
        input_tokens = ?usage.map(|u| u.input_tokens), cached_tokens = ?usage.map(|u| u.cached_tokens), output_tokens = ?usage.map(|u| u.output_tokens),
        total_tokens = ?usage.map(|u| u.total_tokens), cache_write_tokens = ?cache_write_tokens,
        usage_projection_difference = ?state.projection_difference, "cache diagnostics");
}

fn summary(snapshot: &RequestSnapshot, openai: &OpenAiProvider, current_native: NativeCounts) {
    let state = &openai.cache_diagnostics;
    let prior = state.baseline.as_ref().map(|b| &b.meta);
    let comparison = snapshot.comparison;
    let properties_changed = [
        comparison.instructions_changed,
        comparison.tools_changed,
        comparison.properties_changed,
    ]
    .contains(&Some(true));
    let local_changed = matches!(comparison.status, "mismatch" | "truncated");
    let wire_consistent = state.wire.as_ref().and_then(|w| w.consistent);
    let previous_wire_consistent = prior.and_then(|p| p.wire.as_ref().and_then(|w| w.consistent));
    let routing_changed = prior.map(|p| p.route != snapshot.route);
    let raw_cached = state.raw.as_ref().map(|r| r.cached);
    let unchanged =
        matches!(comparison.status, "exact_equal" | "exact_extension") && !properties_changed;
    let provider_comparison = state.raw.as_ref().map(|r| r.comparison);
    let conclusive = provider_comparison.is_some_and(|c| c.conclusive());
    let reported_category = state.raw.as_ref().map(|r| r.accounting().category);
    let reported_uncached = matches!(
        reported_category,
        Some(ReportedInput::ZeroCached | ReportedInput::Partial)
    );
    let upstream_miss =
        provider_comparison.is_some_and(|c| c.outcome == ComparisonOutcome::CacheMiss);
    let unexplained = reported_uncached && !conclusive;
    // New input can legitimately be uncached; this is an investigation flag,
    // not a claim of a missed prefix or an estimate of hosted-search tokens.
    let unresolved = unexplained
        && unchanged
        && wire_consistent == Some(true)
        && previous_wire_consistent == Some(true)
        && routing_changed == Some(false);
    tracing::info!(diagnostic_event = "comparison_summary", runtime_id = %state.runtime, local_request_id = snapshot.id,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        previous_runtime_id = ?prior.map(|p| p.runtime), previous_request_id = ?prior.map(|p| p.request_id),
        baseline_source = state.source, missing_baseline_reason = prior.is_none().then_some(state.missing_reason),
        baseline_age_ms = ?prior.and_then(|p| now_ms().checked_sub(p.completed_at_ms)), previous_completed_at_ms = ?prior.map(|p| p.completed_at_ms),
        completed_at_ms = now_ms(), previous_input_count = ?prior.map(|p| p.input_count), previous_output_count = ?prior.map(|p| p.output_count),
        input_count = snapshot.input_count, matched_item_count = comparison.matched_count, first_difference = ?comparison.first_difference,
        previous_item_kind = ?comparison.previous_kind, next_item_kind = ?comparison.next_kind, prefix_status = comparison.status,
        local_input_changed = ?comparison.baseline_count.map(|_| local_changed), request_properties_changed = ?comparison.baseline_count.map(|_| properties_changed),
        comparison_unavailable = (snapshot.input_count > MAX_ITEMS).then_some("snapshot_item_limit"),
        instructions_changed = ?comparison.instructions_changed, tools_changed = ?comparison.tools_changed, remaining_properties_changed = ?comparison.properties_changed,
        routing_metadata_changed = ?routing_changed,
        connection_changed = ?prior.map(|p| p.runtime != state.runtime || p.socket_generation != openai.ws.socket_generation || p.wire.as_ref().map(|w| w.transport) != state.wire.as_ref().map(|w| w.transport)),
        socket_generation = openai.ws.socket_generation, request_mode = state.wire.as_ref().map(|w| if w.incremental { "incremental" } else { "full" }),
        full_replay_reason = state.wire.as_ref().and_then(|w| w.full_reason).map(FullReason::as_str),
        preparation_to_wire_consistent = ?wire_consistent, previous_preparation_to_wire_consistent = ?previous_wire_consistent,
        wire_projection_mismatch = wire_consistent == Some(false) || previous_wire_consistent == Some(false),
        usage_projection_difference = ?state.projection_difference,
        previous_cached_tokens = ?prior.and_then(|p| p.usage.map(|u| u.cached)), cached_tokens = ?state.usage.map(|u| u.cached),
        previous_raw_cached = ?prior.and_then(|p| p.raw.as_ref().map(|r| r.cached)), raw_cached = ?raw_cached,
        provider_reported_miss_with_unchanged_local_prefix = upstream_miss && unchanged,
        reported_input_category = ?reported_category, provider_comparison_conclusive = conclusive,
        partial_reuse_unexplained = reported_category == Some(ReportedInput::Partial) && !conclusive,
        unresolved_beyond_client_boundary = unresolved, "cache diagnostics");
    tracing::info!(diagnostic_event = "native_context", runtime_id = %state.runtime, local_request_id = snapshot.id,
        historical_native = ?snapshot.historical_native, current_native = ?current_native,
        provider_comparison_requested = ?snapshot.provider_comparison_requested, "cache diagnostics");
    // Separate bounded identity record keeps the summary size independent of IDs.
    tracing::info!(diagnostic_event = "comparison_identity", runtime_id = %state.runtime, local_request_id = snapshot.id,
        previous_response_id = ?prior.and_then(|p| p.raw.as_ref().and_then(|r| r.response_id.as_deref())),
        response_id = ?state.raw.as_ref().and_then(|r| r.response_id.as_deref()),
        previous_returned_model = ?prior.and_then(|p| p.raw.as_ref().and_then(|r| r.model.as_deref())),
        returned_model = ?state.raw.as_ref().and_then(|r| r.model.as_deref()),
        previous_upstream_request_id = ?prior.and_then(|p| p.upstream_request_id.as_deref()), upstream_request_id = ?state.upstream_request_id,
        previous_connection_request_id = ?prior.and_then(|p| p.connection_request_id.as_deref()),
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier), "cache diagnostics");
}

// Only after native validation AND search finalization. Not transcript durability.
pub(crate) fn finish(
    prepared: &PreparedRequest,
    openai: &mut OpenAiProvider,
    response: Option<&ModelResponse>,
) {
    let Some(snapshot) = &prepared.cache_diagnostics else {
        return;
    };
    let output = response.and_then(|r| r.record().provider_replay());
    let valid = output.is_some() && snapshot.epoch == openai.cache_diagnostics.epoch;
    let current_native = output
        .map(|o| NativeCounts::from_items(&o.items))
        .unwrap_or_default();
    if valid {
        summary(snapshot, openai, current_native);
    }
    let state = &mut openai.cache_diagnostics;
    let mut persistence_error = None;
    let mut promoted = false;
    if let Some(output) = output.filter(|_| valid) {
        if snapshot.input_count.saturating_add(output.items.len()) > MAX_ITEMS {
            state.baseline = None;
            state.source = "none";
            state.missing_reason = "snapshot_item_limit";
            persistence_error = state
                .store
                .as_ref()
                .and_then(|s| s.invalidate(Invalidation::ItemLimit).err());
        } else {
            let mut items = snapshot.items.clone();
            items.extend(output.items.iter().map(ItemFingerprint::new));
            let baseline = Baseline {
                authority: CompletionAuthority::ValidatedProviderCompletionNotTranscriptDurability,
                profile: persistence::profile_identity(&openai.profile),
                properties: snapshot.properties,
                items,
                meta: Completion {
                    runtime: state.runtime,
                    request_id: snapshot.id,
                    completed_at_ms: now_ms(),
                    input_count: snapshot.input_count,
                    output_count: output.items.len(),
                    historical_native: snapshot.historical_native,
                    current_native,
                    socket_generation: openai.ws.socket_generation,
                    wire: state.wire.clone(),
                    route: snapshot.route.clone(),
                    raw: state.raw.clone(),
                    usage: state.usage,
                    upstream_request_id: state.upstream_request_id.clone(),
                    connection_request_id: openai
                        .ws
                        .websocket_connection_request_id
                        .as_deref()
                        .map(|s| identifier(s).to_owned()),
                },
            };
            persistence_error = state.store.as_ref().and_then(|s| s.save(&baseline).err());
            state.baseline = Some(baseline);
            state.source = "memory";
            promoted = true;
        }
    }
    tracing::info!(diagnostic_event = "attempt_finished", runtime_id = %state.runtime, local_request_id = snapshot.id,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model),
        transport = openai.transport.as_str(), socket_generation = openai.ws.socket_generation,
        validated_completion = response.is_some(), baseline_promoted = promoted, persistence_configured = state.store.is_some(), persistence_error = ?persistence_error,
        baseline_authority = "validated_provider_completion_not_transcript_durability", prefix_status = snapshot.comparison.status,
        retained_baseline_bytes = retained_bytes(state), "cache diagnostics");
}

pub(crate) fn reset(openai: &mut OpenAiProvider, reason: &'static str) {
    let state = &mut openai.cache_diagnostics;
    state.baseline = None;
    state.source = "none";
    state.current_request = None;
    state.epoch = state.epoch.wrapping_add(1);
    state.missing_reason = reason;
    let error = state
        .store
        .as_ref()
        .and_then(|s| s.invalidate(Invalidation::Reset).err());
    tracing::info!(diagnostic_event = "reset", runtime_id = %state.runtime, reason, reset_epoch = state.epoch, persistence_error = ?error,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model), "cache diagnostics");
}

pub(crate) fn recovery(prepared: &PreparedRequest, openai: &OpenAiProvider) {
    let Some(snapshot) = &prepared.cache_diagnostics else {
        return;
    };
    let observation = openai.ws.session.cache_diagnostics.observation();
    tracing::info!(diagnostic_event = "socket_recovery", runtime_id = %openai.cache_diagnostics.runtime, local_request_id = snapshot.id,
        provider = identifier(&openai.profile.provider), model = identifier(&openai.profile.model), transport = openai.transport.as_str(), socket_generation = openai.ws.socket_generation,
        websocket_connection_request_id = ?openai.ws.websocket_connection_request_id.as_deref().map(identifier),
        terminal_category = openai.ws.session.terminal_status().map(|c| c.as_str()), observed_category = ?observation.map(|o| o.category.as_str()),
        observation_source = observation.map_or("inferred", |o| o.source),
        termination_age_ms = ?observation.and_then(|o| o.at.map(|at| at.elapsed().as_millis().min(u64::MAX as u128) as u64)),
        since_last_completion_ms = openai.ws.idle_for().as_millis().min(u64::MAX as u128) as u64,
        error_class = ?observation.and_then(|o| o.error_class), io_error_kind = ?observation.and_then(|o| o.io_kind), "cache diagnostics");
}
fn retained_bytes(state: &DiagnosticState) -> usize {
    std::mem::size_of::<DiagnosticState>()
        + state.baseline.as_ref().map_or(0, |b| {
            b.items.capacity() * std::mem::size_of::<ItemFingerprint>()
        })
}
#[cfg(test)]
pub(crate) fn inspect(openai: &OpenAiProvider) -> (Option<u64>, usize, u64, Option<u64>) {
    let state = &openai.cache_diagnostics;
    (
        state.baseline.as_ref().map(|b| b.meta.request_id),
        state.baseline.as_ref().map_or(0, |b| b.items.len()),
        state.epoch,
        state.current_request,
    )
}
#[cfg(test)]
pub(crate) fn memory(prepared: &PreparedRequest, openai: &OpenAiProvider) -> (usize, usize) {
    (
        prepared.cache_diagnostics.as_ref().map_or(0, |s| {
            std::mem::size_of::<RequestSnapshot>()
                + s.items.capacity() * std::mem::size_of::<ItemFingerprint>()
        }),
        retained_bytes(&openai.cache_diagnostics),
    )
}
