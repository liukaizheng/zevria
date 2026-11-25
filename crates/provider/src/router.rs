//! Lazy role-to-profile routing over independent protocol-specific runtimes.

use crate::diagnostics::Diagnostics;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use zevria_anthropic::project::ValidatedHistory;

use sha2::{Digest, Sha256};
use zevria_foundation::{ModelRole, ReasoningLevel};
use zevria_model::CompactResult;
use zevria_model::ModelRequest;
use zevria_model::models::{ModelCandidate, ModelSelection};
use zevria_session_api::CompactFuture;
use zevria_session_api::InputTokenCountFuture;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;

use crate::{
    ModelRouting, ProfileProtocol, ResolvedModelProfile, anthropic::AnthropicProvider,
    connection::OpenAiProvider, responses::ResponsesCountProbe,
};

#[derive(Clone, Copy)]
struct Route {
    slot: usize,
    reasoning_level: ReasoningLevel,
}

enum InputPurpose {
    Dispatch,
    Count,
}

#[derive(Default)]
struct ValidatedInput {
    anthropic_history: Option<Arc<ValidatedHistory>>,
    responses_count_probe: Option<ResponsesCountProbe>,
}

#[derive(Default)]
pub(crate) struct SlotCache {
    pub(crate) responses: crate::responses::ResponsesCache,
    anthropic_history: crate::anthropic::HistoryCache,
}

struct ProfileSlot {
    profile: ResolvedModelProfile,
    runtime: Option<ProfileRuntime>,
    cache: Arc<Mutex<SlotCache>>,
}

enum ProfileRuntime {
    Responses(Box<OpenAiProvider>),
    Anthropic(Box<AnthropicProvider>),
}
impl ProfileRuntime {
    fn set_reasoning_level(&mut self, level: ReasoningLevel) {
        match self {
            Self::Responses(runtime) => runtime.set_reasoning_level(level),
            Self::Anthropic(runtime) => runtime.set_reasoning_level(level),
        }
    }
    fn prewarm(&mut self, role: ModelRole) -> zevria_session_api::PrewarmFuture<'_> {
        match self {
            Self::Responses(runtime) => runtime.prewarm(role),
            Self::Anthropic(runtime) => runtime.prewarm(role),
        }
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
        history: Option<Arc<ValidatedHistory>>,
    ) -> ProviderFuture<'a> {
        match self {
            Self::Responses(runtime) => runtime.complete(request, progress),
            Self::Anthropic(runtime) => Box::pin(runtime.run(request, progress, history)),
        }
    }
    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        input: ValidatedInput,
    ) -> InputTokenCountFuture<'a> {
        match self {
            Self::Responses(runtime) => {
                runtime.count_with_probe(request, input.responses_count_probe)
            }
            Self::Anthropic(runtime) => {
                runtime.count_with_history(request, input.anthropic_history)
            }
        }
    }
    fn compact<'a>(&'a mut self, request: ModelRequest<'a>) -> CompactFuture<'a> {
        match self {
            Self::Responses(runtime) => runtime.compact(request),
            Self::Anthropic(runtime) => runtime.compact(request),
        }
    }
    fn reset(&mut self) {
        match self {
            Self::Responses(runtime) => runtime.reset(),
            Self::Anthropic(runtime) => runtime.reset(),
        }
    }
    fn cancel(&mut self) {
        match self {
            Self::Responses(runtime) => runtime.cancel(),
            Self::Anthropic(runtime) => runtime.cancel(),
        }
    }
}

/// One session's role router. Exact provider/model matches share a slot;
/// different profiles retain independent transport, retry, fallback, and
/// continuation state.
pub struct ProviderRouter {
    diagnostics: crate::diagnostics::RouterDiagnostics,
    routes: [Option<Route>; ModelRole::COUNT],
    slots: Vec<ProfileSlot>,
    preamble: String,
    session_id: String,
    last_dispatched: Option<usize>,
    requires_portable_compaction: bool,
}

impl ProviderRouter {
    /// Root sessions route Build, Plan, and Review. Explore is deliberately
    /// absent because every child receives a fresh single-profile router.
    pub fn root(
        routing: &ModelRouting,
        preamble: impl Into<String>,
        session_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let mut router = Self::from_routes(
            [ModelRole::Build, ModelRole::Plan, ModelRole::Review]
                .into_iter()
                .map(|role| {
                    (
                        role,
                        routing.for_role(role).clone(),
                        routing.selection_for_role(role).reasoning_level,
                    )
                }),
            preamble,
            session_id,
        )?;
        for profile in routing.catalog() {
            if !router
                .slots
                .iter()
                .any(|slot| slot.profile.profile == profile.profile)
            {
                router.slots.push(ProfileSlot {
                    profile: profile.clone(),
                    runtime: None,
                    cache: Default::default(),
                });
            }
        }
        router.requires_portable_compaction = router.slots.len() > 1;
        Ok(router)
    }

    /// Construct a router from explicit role/profile routes. This is public so
    /// protocol integration tests and alternate composition roots can build a
    /// single-profile session without a user-facing provider catalog.
    pub fn from_routes(
        routes: impl IntoIterator<Item = (ModelRole, ResolvedModelProfile, ReasoningLevel)>,
        preamble: impl Into<String>,
        session_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let mut route_table = [None; ModelRole::COUNT];
        let mut slot_by_profile = BTreeMap::new();
        let mut slots = Vec::new();
        for (role, profile, reasoning_level) in routes {
            if route_table[role.index()].is_some() {
                anyhow::bail!("the {} model role was routed more than once", role.name());
            }
            anyhow::ensure!(
                profile.reasoning_levels.contains(&reasoning_level),
                "reasoning level {reasoning_level} is not configured for {}",
                profile.profile
            );
            let index = if let Some(index) = slot_by_profile.get(&profile.profile).copied() {
                index
            } else {
                let index = slots.len();
                slot_by_profile.insert(profile.profile.clone(), index);
                slots.push(ProfileSlot {
                    profile,
                    runtime: None,
                    cache: Default::default(),
                });
                index
            };
            route_table[role.index()] = Some(Route {
                slot: index,
                reasoning_level,
            });
        }
        if slots.is_empty() {
            anyhow::bail!("a provider router must contain at least one model profile");
        }
        let requires_portable_compaction = slots.len() > 1;
        Ok(Self {
            diagnostics: Default::default(),
            routes: route_table,
            slots,
            preamble: preamble.into(),
            session_id: session_id.into(),
            last_dispatched: None,
            requires_portable_compaction,
        })
    }

    /// Attach the actual transcript location without changing routing/cache keys.
    #[cfg(feature = "cache-diagnostics")]
    pub fn with_cache_diagnostics(mut self, context: crate::CacheDiagnosticContext) -> Self {
        Diagnostics::set_router_context(&mut self.diagnostics, context);
        self
    }

    pub fn requires_portable_compaction(&self) -> bool {
        self.requires_portable_compaction
    }

    pub fn initialized_profile_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.runtime.is_some())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn continuation_response_id(
        &self,
        profile: &zevria_foundation::ModelProfileRef,
    ) -> Option<&str> {
        self.slots
            .iter()
            .find(|slot| &slot.profile.profile == profile)
            .and_then(|slot| slot.runtime.as_ref())
            .and_then(|runtime| match runtime {
                ProfileRuntime::Responses(runtime) => {
                    runtime.responses_transport.ws.continuation.as_ref()
                }
                ProfileRuntime::Anthropic(_) => None,
            })
            .map(|continuation| continuation.response_id.as_str())
    }

    fn route(&self, role: ModelRole) -> anyhow::Result<Route> {
        self.routes[role.index()].ok_or_else(|| {
            anyhow::anyhow!(
                "the {} model role is not available in this session router",
                role.name()
            )
        })
    }

    fn profile_slot(&self, profile: &zevria_foundation::ModelProfileRef) -> anyhow::Result<usize> {
        self.slots.iter().position(|slot| &slot.profile.profile == profile)
            .ok_or_else(|| anyhow::anyhow!("profile {profile} is not in the session catalog; stay with its source model or start a fresh session"))
    }

    fn validate_input(
        &self,
        index: usize,
        request: &ModelRequest<'_>,
        purpose: InputPurpose,
    ) -> anyhow::Result<ValidatedInput> {
        let slot = &self.slots[index];
        let profile = &slot.profile;
        let probe = matches!(purpose, InputPurpose::Count) && request.input_identity.is_none();
        match &profile.protocol {
            ProfileProtocol::Responses { endpoint, .. } if probe => {
                let probe = ResponsesCountProbe::new(
                    &request.input,
                    endpoint
                        .compatibility
                        .preflight_options(&profile.profile, &profile.image_accounting),
                )?;
                return Ok(ValidatedInput {
                    responses_count_probe: Some(probe),
                    ..Default::default()
                });
            }
            ProfileProtocol::Responses { .. } => {
                let fallback;
                let identity = match request.input_identity {
                    Some(identity) => identity,
                    None => {
                        fallback = zevria_model::ModelInputIdentity::from_items(&request.input)?;
                        &fallback
                    }
                };
                let assessment = self.preflight_input_with_identity(
                    &profile.profile,
                    &request.input,
                    Some(identity),
                )?;
                anyhow::ensure!(
                    matches!(
                        assessment,
                        zevria_model::models::ReplayPreflight::Compatible(_)
                    ),
                    "cannot send replay-only history to target profile {}: conversion required; select its source with /model, confirm a portable summary, or start a fresh session",
                    profile.profile
                );
            }
            ProfileProtocol::Anthropic { endpoint, .. } => {
                let mut scratch;
                let mut live;
                let cache = if probe {
                    scratch = crate::anthropic::HistoryCache::default();
                    &mut scratch
                } else {
                    live = slot
                        .cache
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Anthropic history cache lock poisoned"))?;
                    &mut live.anthropic_history
                };
                let prepared = cache.prepare(
                    &request.input,
                    request.input_identity,
                    &profile.profile,
                    &endpoint.compatibility,
                    &profile.image_accounting,
                )?;
                anyhow::ensure!(
                    matches!(
                        prepared.assessment,
                        zevria_model::models::ReplayPreflight::Compatible(_)
                    ),
                    "cannot send opaque history to profile {}: conversion required",
                    profile.profile
                );
                return Ok(ValidatedInput {
                    anthropic_history: prepared.history,
                    ..Default::default()
                });
            }
        }
        Ok(ValidatedInput::default())
    }

    fn initialize_slot(
        &mut self,
        index: usize,
        level: ReasoningLevel,
    ) -> anyhow::Result<&mut ProfileRuntime> {
        let slot = self
            .slots
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("invalid provider router slot {index}"))?;
        if slot.runtime.is_none() {
            let cache_key = profile_cache_key(&self.session_id, &slot.profile);
            let runtime = match &slot.profile.protocol {
                ProfileProtocol::Responses { .. } => {
                    let mut runtime = OpenAiProvider::unconnected(
                        &slot.profile,
                        level,
                        &self.preamble,
                        &self.session_id,
                        &cache_key,
                    )?;
                    runtime.slot_cache = slot.cache.clone();
                    let runtime = Diagnostics::configure_runtime(runtime, &self.diagnostics);
                    ProfileRuntime::Responses(Box::new(runtime))
                }
                ProfileProtocol::Anthropic { .. } => {
                    ProfileRuntime::Anthropic(Box::new(AnthropicProvider::unconnected(
                        &slot.profile,
                        level,
                        &self.preamble,
                        &self.session_id,
                    )?))
                }
            };
            slot.runtime = Some(runtime);
        }
        slot.runtime
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("provider slot was not initialized"))
    }
}

impl ModelProvider for ProviderRouter {
    fn application_prompt(&self) -> &str {
        &self.preamble
    }

    fn prewarm(&mut self, role: ModelRole) -> zevria_session_api::PrewarmFuture<'_> {
        Box::pin(async move {
            let Ok(route) = self.route(role) else { return };
            // Avoid even local slot initialization for an opt-out, HTTP-only or
            // Anthropic profile. Router construction itself still performs no I/O.
            if !matches!(&self.slots[route.slot].profile.protocol,
                ProfileProtocol::Responses { endpoint, .. }
                    if endpoint.network.prewarm && endpoint.supports_websockets)
            {
                return;
            }
            match self.initialize_slot(route.slot, route.reasoning_level) {
                Ok(runtime) => runtime.prewarm(role).await,
                Err(error) => tracing::warn!(
                    role = role.name(), error = %error,
                    "provider idle prewarm setup failed"
                ),
            }
        })
    }
    fn model_catalog(&self) -> Vec<ModelCandidate> {
        self.slots
            .iter()
            .map(|slot| ModelCandidate {
                context: slot.profile.context_policy(),
                reasoning_levels: slot.profile.reasoning_levels.clone(),
            })
            .collect()
    }

    fn prepare_model_update(
        &self,
        role: ModelRole,
        target: &ModelSelection,
    ) -> anyhow::Result<zevria_foundation::ModelContextPolicy> {
        anyhow::ensure!(
            matches!(role, ModelRole::Build | ModelRole::Plan),
            "only root Build and Plan assignments may change"
        );
        self.route(role)?;
        let profile = &self.slots[self.profile_slot(&target.profile)?].profile;
        match &profile.protocol {
            ProfileProtocol::Responses { .. } => {
                OpenAiProvider::validate_profile_settings(
                    profile,
                    target.reasoning_level,
                    &self.session_id,
                    &profile_cache_key(&self.session_id, profile),
                )?;
            }
            ProfileProtocol::Anthropic { .. } => {
                AnthropicProvider::validate_profile_settings(
                    profile,
                    target.reasoning_level,
                    &self.session_id,
                )?;
            }
        }
        Ok(profile.context_policy())
    }

    fn install_model_update(&mut self, role: ModelRole, target: &ModelSelection) {
        let slot = self
            .profile_slot(&target.profile)
            .expect("prepared immutable catalog route");
        let same_profile = self.routes[role.index()].is_some_and(|route| route.slot == slot);
        self.routes[role.index()] = Some(Route {
            slot,
            reasoning_level: target.reasoning_level,
        });
        // Same-profile reasoning updates leave sockets, cache keys, instructions,
        // tools and input untouched. Request properties invalidate continuation.
        if !same_profile {
            self.reset();
        }
    }

    fn model_selection(&self, role: ModelRole) -> Option<ModelSelection> {
        let route = self.route(role).ok()?;
        Some(ModelSelection::new(
            self.slots[route.slot].profile.profile.clone(),
            route.reasoning_level,
        ))
    }

    fn preflight_input(
        &self,
        target: &zevria_foundation::ModelProfileRef,
        input: &[zevria_model::ModelRequestItem<'_>],
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        let index = self.profile_slot(target)?;
        match &self.slots[index].profile.protocol {
            ProfileProtocol::Responses { endpoint, .. } => zevria_responses::replay::preflight(
                input,
                endpoint
                    .compatibility
                    .preflight_options(target, &self.slots[index].profile.image_accounting),
            ),
            ProfileProtocol::Anthropic { endpoint, .. } => Ok(self.slots[index]
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("Anthropic history cache lock poisoned"))?
                .anthropic_history
                .assess(
                    input,
                    None,
                    target,
                    &endpoint.compatibility,
                    &self.slots[index].profile.image_accounting,
                )?),
        }
    }

    fn preflight_input_with_identity(
        &self,
        target: &zevria_foundation::ModelProfileRef,
        input: &[zevria_model::ModelRequestItem<'_>],
        identity: Option<&zevria_model::ModelInputIdentity>,
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        let Some(identity) = identity else {
            return self.preflight_input(target, input);
        };
        let slot = &self.slots[self.profile_slot(target)?];
        match &slot.profile.protocol {
            ProfileProtocol::Anthropic { endpoint, .. } => Ok(slot
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("Anthropic history cache lock poisoned"))?
                .anthropic_history
                .assess(
                    input,
                    Some(identity),
                    target,
                    &endpoint.compatibility,
                    &slot.profile.image_accounting,
                )?),
            ProfileProtocol::Responses { endpoint, .. } => slot
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("Responses slot cache lock poisoned"))?
                .responses
                .input
                .assess(
                    input,
                    identity,
                    endpoint
                        .compatibility
                        .preflight_options(target, &slot.profile.image_accounting),
                ),
        }
    }

    fn tool_append_preserves_prefix(&self, target: &zevria_foundation::ModelProfileRef) -> bool {
        let Ok(index) = self.profile_slot(target) else {
            return false;
        };
        match &self.slots[index].profile.protocol {
            ProfileProtocol::Responses { endpoint, .. } => endpoint.compatibility.additional_tools,
            ProfileProtocol::Anthropic { endpoint, .. } => {
                endpoint.compatibility.inline_tools && endpoint.compatibility.system_messages
            }
        }
    }

    fn assess_request(
        &self,
        context: &zevria_foundation::ModelContextPolicy,
        request: &ModelRequest<'_>,
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        let slot = &self.slots[self.profile_slot(&context.profile)?];
        anyhow::ensure!(
            context.image_accounting == slot.profile.image_accounting,
            "accounting policy differs from resolved destination"
        );
        let assessment = self.preflight_input_with_identity(
            &context.profile,
            &request.input,
            request.input_identity,
        )?;
        let zevria_model::models::ReplayPreflight::Compatible(mut estimate) = assessment else {
            return Ok(assessment);
        };
        estimate.validate_media_constraints()?;
        let additions = || {
            request.input.iter().filter_map(|item| match item {
                zevria_model::ModelRequestItem::AdditionalTools(tools) => Some(*tools),
                _ => None,
            })
        };
        let (instructions, tool_tokens, folded_tokens) = match &slot.profile.protocol {
            ProfileProtocol::Responses { endpoint, .. } => {
                let mut cache = slot
                    .cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Responses slot cache lock poisoned"))?;
                let cache = &mut cache.responses;
                let tools =
                    cache.rendered_tools(request, &endpoint.compatibility, &endpoint.web_search)?;
                let folded = if endpoint.compatibility.additional_tools {
                    0
                } else if let Some(tokens) = request
                    .input_identity
                    .and_then(|id| cache.input.folded_tool_tokens(id))
                {
                    tokens
                } else {
                    // Synthetic views without a trusted identity use a pure,
                    // uncached assessment so they cannot evict the live prefix.
                    let options = zevria_responses::tools::ToolRendererOptions {
                        strict_tools: endpoint.compatibility.strict_tools,
                    };
                    additions().try_fold(0u64, |total, tools| -> anyhow::Result<_> {
                        let rendered = zevria_responses::tools::render(tools, options)?;
                        Ok(total.saturating_add(
                            zevria_model::compaction::approximate_tokens_from_bytes(
                                serde_json::to_vec(&rendered)?.len(),
                            ),
                        ))
                    })?
                };
                (
                    zevria_model::compaction::approximate_tokens(request.instructions),
                    tools.tokens,
                    folded,
                )
            }
            ProfileProtocol::Anthropic { endpoint, .. } => {
                let tools =
                    zevria_anthropic::body::rendered_tools(request, &endpoint.compatibility)?;
                let mut folded = 0u64;
                if !(endpoint.compatibility.inline_tools && endpoint.compatibility.system_messages)
                {
                    for tools in additions() {
                        let tools = tools
                            .iter()
                            .map(|tool| {
                                zevria_anthropic::body::tool_definition(
                                    tool,
                                    endpoint.compatibility.eager_tool_input_streaming,
                                )
                            })
                            .collect::<Vec<_>>();
                        folded = folded.saturating_add(
                            zevria_model::compaction::approximate_tokens_from_bytes(
                                serde_json::to_vec(&tools)?.len(),
                            ),
                        );
                    }
                }
                (
                    zevria_model::compaction::approximate_tokens_from_bytes(
                        serde_json::to_vec(&zevria_anthropic::body::rendered_system(
                            request.instructions,
                        ))?
                        .len(),
                    ),
                    if tools.is_null() {
                        0
                    } else {
                        zevria_model::compaction::approximate_tokens_from_bytes(
                            serde_json::to_vec(&tools)?.len(),
                        )
                    },
                    folded,
                )
            }
        };
        // History-only management preflight includes provisional folded additions.
        // Replace that contribution with the actual top-level folded declaration set,
        // including name replacements, exactly once using the transport renderer.
        estimate.payload_tokens = estimate.payload_tokens.saturating_sub(folded_tokens);
        estimate.estimated_tokens = estimate.estimated_tokens.saturating_sub(folded_tokens);
        let fixed = instructions.saturating_add(tool_tokens);
        Ok(zevria_model::models::ReplayPreflight::Compatible(
            estimate.saturating_add(zevria_model::ContextTokenEstimate::new(fixed, fixed)),
        ))
    }

    fn complete_profile<'a>(
        &'a mut self,
        selection: &'a ModelSelection,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let index = self.profile_slot(&selection.profile)?;
            anyhow::ensure!(
                self.slots[index]
                    .profile
                    .reasoning_levels
                    .contains(&selection.reasoning_level),
                "reasoning level {} is not configured for {}",
                selection.reasoning_level,
                selection.profile
            );
            let history = self.validate_input(index, &request, InputPurpose::Dispatch)?;
            self.last_dispatched = Some(index);
            let level = selection.reasoning_level;
            let runtime = self.initialize_slot(index, level)?;
            runtime.set_reasoning_level(level);
            runtime
                .complete(request, progress, history.anthropic_history)
                .await
        })
    }

    fn count_profile<'a>(
        &'a mut self,
        selection: &'a ModelSelection,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            let index = self.profile_slot(&selection.profile)?;
            anyhow::ensure!(
                self.slots[index]
                    .profile
                    .reasoning_levels
                    .contains(&selection.reasoning_level),
                "reasoning level {} is not configured for {}",
                selection.reasoning_level,
                selection.profile
            );
            let input = self.validate_input(index, &request, InputPurpose::Count)?;
            let level = selection.reasoning_level;
            let runtime = self.initialize_slot(index, level)?;
            runtime.set_reasoning_level(level);
            runtime.count_input_tokens(request, input).await
        })
    }

    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        let route = self.route(request.model_role);
        self.last_dispatched = route.as_ref().ok().map(|route| route.slot);
        Box::pin(async move {
            let route = route?;
            let history = self.validate_input(route.slot, &request, InputPurpose::Dispatch)?;
            let runtime = self.initialize_slot(route.slot, route.reasoning_level)?;
            runtime.set_reasoning_level(route.reasoning_level);
            runtime
                .complete(request, progress, history.anthropic_history)
                .await
        })
    }

    fn compact<'a>(&'a mut self, request: ModelRequest<'a>) -> CompactFuture<'a> {
        if self.requires_portable_compaction {
            return Box::pin(async { Ok(CompactResult::Unsupported) });
        }
        let route = self.route(request.model_role);
        self.last_dispatched = route.as_ref().ok().map(|route| route.slot);
        Box::pin(async move {
            let route = route?;
            // Native compaction receives a completed history, which may end
            // in an assistant. Messages completion-shape validation belongs
            // to the subsequent local summary request, not this capability check.
            if matches!(
                self.slots[route.slot].profile.protocol,
                ProfileProtocol::Anthropic { .. }
            ) {
                return Ok(CompactResult::Unsupported);
            }
            self.validate_input(route.slot, &request, InputPurpose::Dispatch)?;
            let runtime = self.initialize_slot(route.slot, route.reasoning_level)?;
            runtime.set_reasoning_level(route.reasoning_level);
            runtime.compact(request).await
        })
    }

    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        let route = self.route(request.model_role);
        Box::pin(async move {
            let route = route?;
            let input = self.validate_input(route.slot, &request, InputPurpose::Count)?;
            let runtime = self.initialize_slot(route.slot, route.reasoning_level)?;
            runtime.set_reasoning_level(route.reasoning_level);
            runtime.count_input_tokens(request, input).await
        })
    }

    fn reset(&mut self) {
        Diagnostics::router_reset(&mut self.diagnostics);
        for slot in &mut self.slots {
            Diagnostics::uninitialized(
                &self.diagnostics,
                &slot.profile.profile,
                slot.runtime.is_none(),
                matches!(slot.profile.protocol, ProfileProtocol::Responses { .. }),
            );
            if let Some(runtime) = &mut slot.runtime {
                runtime.reset();
            }
        }
        self.last_dispatched = None;
    }

    fn cancel(&mut self) {
        let Some(index) = self.last_dispatched.take() else {
            return;
        };
        if let Some(runtime) = self
            .slots
            .get_mut(index)
            .and_then(|slot| slot.runtime.as_mut())
        {
            runtime.cancel();
        }
    }
}

/// Cloneable recipe for a fresh independent single-profile router, used by
/// Explore and Builder child subsessions. Declarations belong to each request.
#[derive(Clone)]
pub struct ProviderRouterFactory {
    role: ModelRole,
    profile: ResolvedModelProfile,
    reasoning_level: ReasoningLevel,
    preamble: String,
}

impl ProviderRouterFactory {
    pub fn new(
        role: ModelRole,
        profile: ResolvedModelProfile,
        reasoning_level: ReasoningLevel,
        preamble: impl Into<String>,
    ) -> Self {
        Self {
            role,
            profile,
            reasoning_level,
            preamble: preamble.into(),
        }
    }

    pub fn create(&self, session_id: &str) -> anyhow::Result<ProviderRouter> {
        ProviderRouter::from_routes(
            [(self.role, self.profile.clone(), self.reasoning_level)],
            self.preamble.clone(),
            session_id.to_string(),
        )
    }
}

pub(crate) fn profile_cache_key(session_id: &str, profile: &ResolvedModelProfile) -> String {
    let mut hasher = Sha256::new();
    for component in [
        "zevria.prompt-cache-key.v1",
        session_id,
        profile.profile.provider.as_str(),
        profile.profile.model.as_str(),
    ] {
        let component_len = u64::try_from(component.len())
            .expect("prompt-cache key components fit in an unsigned 64-bit length");
        hasher.update(component_len.to_be_bytes());
        hasher.update(component.as_bytes());
    }
    crate::lowercase_hex(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModeAssignments, ModelAssignment, ProviderConfig};
    use rig_core::message::Message;
    use serde_json::json;
    use zevria_model::{ModelInputIdentity, ModelRequestItem};

    #[test]
    fn identity_less_anthropic_count_does_not_replace_the_live_history() {
        let provider: ProviderConfig = serde_json::from_value(json!({
            "protocol":"anthropic.messages", "base_url":"http://127.0.0.1:1/v1/messages", "api_key":"test",
            "models":{"test-model":{"context_window_tokens":1000000,"max_output_tokens":64000,"retained_user_tokens":20000,"reasoning_levels":["high"]}}
        })).unwrap();
        let assignment = ModelAssignment {
            provider: "anthropic".into(),
            model: "test-model".into(),
            reasoning_level: ReasoningLevel::High,
        };
        let assignments = ModeAssignments {
            build: assignment.clone(),
            plan: assignment.clone(),
            review: assignment.clone(),
            explore: assignment.clone(),
            builder: assignment,
        };
        let routing = ModelRouting::resolve(
            &BTreeMap::from([("anthropic".into(), provider)]),
            &assignments,
            90,
        )
        .unwrap();
        let profile = routing.for_role(ModelRole::Build).clone();
        let router = ProviderRouter::from_routes(
            [(ModelRole::Build, profile, ReasoningLevel::High)],
            "stable",
            "test-session",
        )
        .unwrap();

        let first = Message::user("first");
        let second = Message::user("second");
        let input = vec![
            ModelRequestItem::message(&first),
            ModelRequestItem::message(&second),
        ];
        let identity = ModelInputIdentity::from_items(&input).unwrap();
        let live_request = ModelRequest {
            input_identity: Some(&identity),
            input,
            instructions: "stable",
            tools: &[],
            forbid_tool_calls: false,
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        let live = router
            .validate_input(0, &live_request, InputPurpose::Dispatch)
            .unwrap()
            .anthropic_history
            .unwrap();
        let probe_request = ModelRequest {
            input_identity: None,
            input: live_request.input[..1].to_vec(),
            ..live_request.clone()
        };
        let probe = router
            .validate_input(0, &probe_request, InputPurpose::Count)
            .unwrap()
            .anthropic_history
            .unwrap();
        assert!(!Arc::ptr_eq(&live, &probe));
        let reused = router
            .validate_input(0, &live_request, InputPurpose::Count)
            .unwrap()
            .anthropic_history
            .unwrap();
        assert!(Arc::ptr_eq(&live, &reused));
    }
}
