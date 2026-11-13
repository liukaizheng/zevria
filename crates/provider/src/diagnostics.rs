//! Observational cache diagnostics. Disabled builds carry only zero-sized data
//! and never inspect, hash, clone, parse or persist diagnostic history.
//!
//! Context/export feature gates remain on the public opt-in API; runtime
//! call sites use this facade in either build configuration.
use crate::{
    connection::{OpenAiProvider, OpenAiTransport},
    http::HttpAttemptError,
    responses::PreparedRequest,
    websocket_session::OpenAiWebSocketTerminalCategory as Category,
};
use zevria_model::{ModelResponse, TokenUsage};

#[derive(Default)]
pub(crate) struct Diagnostics {
    #[cfg(feature = "cache-diagnostics")]
    state: crate::cache_diagnostics::DiagnosticState,
}

#[cfg(feature = "cache-diagnostics")]
impl std::ops::Deref for Diagnostics {
    type Target = crate::cache_diagnostics::DiagnosticState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
#[cfg(feature = "cache-diagnostics")]
impl std::ops::DerefMut for Diagnostics {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

macro_rules! diagnostic_data {
    ($name:ident, $enabled:ty) => {
        #[cfg(feature = "cache-diagnostics")]
        pub(crate) type $name = $enabled;
        #[cfg(not(feature = "cache-diagnostics"))]
        #[derive(Default, Clone, Copy)]
        pub(crate) struct $name {
            _disabled: (),
        }
    };
}
diagnostic_data!(
    RequestDiagnostics,
    Option<crate::cache_diagnostics::RequestSnapshot>
);
diagnostic_data!(
    TransmissionDiagnostics,
    Option<crate::cache_diagnostics::FullReason>
);
diagnostic_data!(ObserverDiagnostics, Option<String>);

#[derive(Default)]
pub(crate) struct RouterDiagnostics {
    #[cfg(feature = "cache-diagnostics")]
    context: Option<crate::CacheDiagnosticContext>,
    #[cfg(feature = "cache-diagnostics")]
    reset: bool,
}

#[cfg(feature = "cache-diagnostics")]
pub(crate) use crate::cache_diagnostics::socket::SocketDiagnostics;
#[cfg(not(feature = "cache-diagnostics"))]
#[derive(Default, Clone)]
pub(crate) struct SocketDiagnostics {
    _disabled: (),
}
#[cfg(not(feature = "cache-diagnostics"))]
impl SocketDiagnostics {
    pub(crate) fn disconnected(_category: Category) -> Self {
        Self::default()
    }
    pub(crate) fn observed(&self, _category: Category, _source: &'static str) {}
    pub(crate) fn error(
        &self,
        _category: Category,
        _error: &tokio_tungstenite::tungstenite::Error,
    ) {
    }
    pub(crate) fn finished(&self, _category: Category) {}
}

impl Diagnostics {
    pub(crate) fn empty_request() -> RequestDiagnostics {
        RequestDiagnostics::default()
    }
    pub(crate) fn incremental() -> TransmissionDiagnostics {
        TransmissionDiagnostics::default()
    }
}

#[cfg(feature = "cache-diagnostics")]
impl Diagnostics {
    pub(crate) fn configured(name: Option<&str>, http_fallback: bool) -> Self {
        Self {
            state: crate::cache_diagnostics::DiagnosticState::configured(name, http_fallback),
        }
    }
    pub(crate) fn identifier(value: &str) -> &str {
        crate::cache_diagnostics::identifier(value)
    }
    pub(crate) fn observer(prior: Option<String>) -> ObserverDiagnostics {
        prior
    }
    pub(crate) fn selection(reason: &str) -> TransmissionDiagnostics {
        Some(crate::cache_diagnostics::FullReason::selection(reason))
    }
    pub(crate) fn no_continuation(openai: &OpenAiProvider) -> TransmissionDiagnostics {
        Some(crate::cache_diagnostics::no_continuation_reason(openai))
    }
    pub(crate) fn stale_id() -> TransmissionDiagnostics {
        Some(crate::cache_diagnostics::FullReason::StaleResponseId)
    }
    pub(crate) fn dispatch(
        prepared: PreparedRequest,
        openai: &mut OpenAiProvider,
    ) -> PreparedRequest {
        crate::cache_diagnostics::dispatch(prepared, openai)
    }
    pub(crate) fn finish(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        response: Option<&ModelResponse>,
    ) {
        crate::cache_diagnostics::finish(prepared, openai, response);
    }
    pub(crate) fn reset(openai: &mut OpenAiProvider, reason: &'static str) {
        crate::cache_diagnostics::reset(openai, reason);
    }
    pub(crate) fn transport_fallback(openai: &mut OpenAiProvider) {
        crate::cache_diagnostics::transport_fallback(openai);
    }
    pub(crate) fn transport_restored(openai: &mut OpenAiProvider) {
        crate::cache_diagnostics::transport_restored(openai);
    }
    pub(crate) fn recovery(prepared: &PreparedRequest, openai: &OpenAiProvider) {
        crate::cache_diagnostics::recovery(prepared, openai);
    }
    pub(crate) fn disconnected_recovery(prepared: &PreparedRequest, openai: &OpenAiProvider) {
        if openai.ws.session.terminal_status() != Some(Category::NotConnected) {
            crate::cache_diagnostics::recovery(prepared, openai);
        }
    }
    pub(crate) fn connection(openai: &OpenAiProvider, event: &'static str) {
        crate::cache_diagnostics::connection(openai, event);
    }
    pub(crate) fn transmission(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        mode: &'static str,
        count: usize,
        previous: Option<&str>,
        reason: TransmissionDiagnostics,
        payload: Option<&[u8]>,
    ) {
        crate::cache_diagnostics::transmission(
            prepared, openai, mode, count, previous, reason, payload,
        );
    }
    pub(crate) fn observe_value(
        openai: &mut OpenAiProvider,
        event: &serde_json::Value,
        bytes: usize,
        prior: &ObserverDiagnostics,
        upstream: Option<&str>,
    ) {
        crate::cache_diagnostics::observe_value(openai, event, bytes, prior.as_deref(), upstream);
    }
    pub(crate) fn terminal(
        openai: &mut OpenAiProvider,
        transport: OpenAiTransport,
        status: &str,
        response_id: Option<&str>,
        upstream: Option<&str>,
        usage: Option<TokenUsage>,
        cache_write_tokens: Option<u64>,
    ) {
        crate::cache_diagnostics::terminal(
            openai,
            transport,
            status,
            response_id,
            upstream,
            usage,
            cache_write_tokens,
        );
    }
    pub(crate) fn http_transmission(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        request: &reqwest::RequestBuilder,
        attempt: usize,
    ) -> Result<(), HttpAttemptError> {
        let diagnostic = request
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
            Some(crate::cache_diagnostics::http_reason(openai, attempt)),
            diagnostic.body().and_then(reqwest::Body::as_bytes),
        );
        Ok(())
    }
    pub(crate) fn set_router_context(
        router: &mut RouterDiagnostics,
        context: crate::CacheDiagnosticContext,
    ) {
        router.context = Some(context);
    }
    pub(crate) fn configure_runtime(
        mut runtime: OpenAiProvider,
        router: &RouterDiagnostics,
    ) -> OpenAiProvider {
        if let Some(context) = &router.context {
            crate::cache_diagnostics::configure(&mut runtime, context);
            if router.reset {
                crate::cache_diagnostics::reset(&mut runtime, "engine_reset");
            }
        }
        runtime
    }
    pub(crate) fn router_reset(router: &mut RouterDiagnostics) {
        router.reset = true;
    }
    pub(crate) fn uninitialized(
        router: &RouterDiagnostics,
        profile: &zevria_foundation::ModelProfileRef,
        uninitialized: bool,
        responses: bool,
    ) {
        if uninitialized
            && responses
            && let Some(context) = &router.context
        {
            crate::cache_diagnostics::invalidate_uninitialized(context, profile);
        }
    }
}

#[cfg(not(feature = "cache-diagnostics"))]
#[allow(unused_variables)]
impl Diagnostics {
    pub(crate) fn configured(name: Option<&str>, http_fallback: bool) -> Self {
        Self::default()
    }
    pub(crate) fn identifier(value: &str) -> &str {
        value
    }
    pub(crate) fn observer(prior: Option<String>) -> ObserverDiagnostics {
        ObserverDiagnostics::default()
    }
    pub(crate) fn selection(reason: &str) -> TransmissionDiagnostics {
        TransmissionDiagnostics::default()
    }
    pub(crate) fn no_continuation(openai: &OpenAiProvider) -> TransmissionDiagnostics {
        TransmissionDiagnostics::default()
    }
    pub(crate) fn stale_id() -> TransmissionDiagnostics {
        TransmissionDiagnostics::default()
    }
    pub(crate) fn dispatch(
        prepared: PreparedRequest,
        openai: &mut OpenAiProvider,
    ) -> PreparedRequest {
        // Keep the two zero-sized owners at the same hook boundary without
        // inspecting, copying or allocating any disabled diagnostic data.
        let _ = (&prepared.cache_diagnostics, &openai.cache_diagnostics);
        prepared
    }
    pub(crate) fn finish(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        response: Option<&ModelResponse>,
    ) {
    }
    pub(crate) fn reset(openai: &mut OpenAiProvider, reason: &'static str) {}
    pub(crate) fn transport_fallback(openai: &mut OpenAiProvider) {}
    pub(crate) fn transport_restored(openai: &mut OpenAiProvider) {}
    pub(crate) fn recovery(prepared: &PreparedRequest, openai: &OpenAiProvider) {}
    pub(crate) fn disconnected_recovery(prepared: &PreparedRequest, openai: &OpenAiProvider) {}
    pub(crate) fn connection(openai: &OpenAiProvider, event: &'static str) {}
    pub(crate) fn transmission(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        mode: &'static str,
        count: usize,
        previous: Option<&str>,
        reason: TransmissionDiagnostics,
        payload: Option<&[u8]>,
    ) {
    }
    pub(crate) fn observe_value(
        openai: &mut OpenAiProvider,
        event: &serde_json::Value,
        bytes: usize,
        prior: &ObserverDiagnostics,
        upstream: Option<&str>,
    ) {
    }
    pub(crate) fn terminal(
        openai: &mut OpenAiProvider,
        transport: OpenAiTransport,
        status: &str,
        response_id: Option<&str>,
        upstream: Option<&str>,
        usage: Option<TokenUsage>,
        cache_write_tokens: Option<u64>,
    ) {
    }
    pub(crate) fn http_transmission(
        prepared: &PreparedRequest,
        openai: &mut OpenAiProvider,
        request: &reqwest::RequestBuilder,
        attempt: usize,
    ) -> Result<(), HttpAttemptError> {
        Ok(())
    }
    pub(crate) fn configure_runtime(
        runtime: OpenAiProvider,
        router: &RouterDiagnostics,
    ) -> OpenAiProvider {
        runtime
    }
    pub(crate) fn router_reset(router: &mut RouterDiagnostics) {}
    pub(crate) fn uninitialized(
        router: &RouterDiagnostics,
        profile: &zevria_foundation::ModelProfileRef,
        uninitialized: bool,
        responses: bool,
    ) {
    }
}

#[cfg(all(test, not(feature = "cache-diagnostics")))]
mod tests {
    use super::*;
    #[test]
    fn disabled_diagnostic_owners_and_request_metadata_are_zero_sized() {
        assert_eq!(std::mem::size_of::<Diagnostics>(), 0);
        assert_eq!(std::mem::size_of::<RouterDiagnostics>(), 0);
        assert_eq!(std::mem::size_of::<SocketDiagnostics>(), 0);
        assert_eq!(std::mem::size_of::<RequestDiagnostics>(), 0);
        assert_eq!(std::mem::size_of::<TransmissionDiagnostics>(), 0);
        assert_eq!(std::mem::size_of::<ObserverDiagnostics>(), 0);
    }
}
