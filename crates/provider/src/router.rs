//! Lazy role-to-profile routing over independent Responses runtimes.

use std::collections::BTreeMap;

use rig_agent::tool::server::ToolServerHandle;
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

use crate::{ModelRouting, ResolvedModelProfile, connection::OpenAiProvider};

#[derive(Clone, Copy)]
struct Route {
    slot: usize,
    reasoning_level: ReasoningLevel,
}

struct ProfileSlot {
    profile: ResolvedModelProfile,
    runtime: Option<OpenAiProvider>,
}

/// One session's role router. Exact provider/model matches share a slot;
/// different profiles retain independent transport, retry, fallback, and
/// continuation state.
pub struct ResponsesRouter {
    #[cfg(feature = "cache-diagnostics")]
    diagnostic_context: Option<crate::CacheDiagnosticContext>,
    #[cfg(feature = "cache-diagnostics")]
    diagnostics_reset: bool,
    routes: [Option<Route>; ModelRole::COUNT],
    slots: Vec<ProfileSlot>,
    preamble: String,
    tools: ToolServerHandle,
    session_id: String,
    last_dispatched: Option<usize>,
    requires_portable_compaction: bool,
}

impl ResponsesRouter {
    /// Root sessions route Build, Plan, and Review. Explore is deliberately
    /// absent because every child receives a fresh single-profile router.
    pub fn root(
        routing: &ModelRouting,
        preamble: impl Into<String>,
        tools: ToolServerHandle,
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
            tools,
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
        tools: ToolServerHandle,
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
                });
                index
            };
            route_table[role.index()] = Some(Route {
                slot: index,
                reasoning_level,
            });
        }
        if slots.is_empty() {
            anyhow::bail!("a Responses router must contain at least one model profile");
        }
        let requires_portable_compaction = slots.len() > 1;
        Ok(Self {
            #[cfg(feature = "cache-diagnostics")]
            diagnostic_context: None,
            #[cfg(feature = "cache-diagnostics")]
            diagnostics_reset: false,
            routes: route_table,
            slots,
            preamble: preamble.into(),
            tools,
            session_id: session_id.into(),
            last_dispatched: None,
            requires_portable_compaction,
        })
    }

    /// Attach the actual transcript location without changing routing/cache keys.
    #[cfg(feature = "cache-diagnostics")]
    pub fn with_cache_diagnostics(mut self, context: crate::CacheDiagnosticContext) -> Self {
        self.diagnostic_context = Some(context);
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
            .and_then(|runtime| runtime.ws.continuation.as_ref())
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

    fn validate_input(&self, index: usize, request: &ModelRequest<'_>) -> anyhow::Result<()> {
        zevria_responses::replay::project_with_compatibility(
            &request.input,
            &self.slots[index].profile.profile,
            self.slots[index]
                .profile
                .endpoint
                .compatibility
                .developer_messages,
        )?;
        Ok(())
    }

    async fn initialize_slot(
        &mut self,
        index: usize,
        level: ReasoningLevel,
    ) -> anyhow::Result<&mut OpenAiProvider> {
        let slot = self
            .slots
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("invalid Responses router slot {index}"))?;
        if slot.runtime.is_none() {
            let cache_key = profile_cache_key(&self.session_id, &slot.profile);
            let runtime = OpenAiProvider::unconnected(
                &slot.profile,
                level,
                &self.preamble,
                self.tools.clone(),
                &self.session_id,
                &cache_key,
            )
            .await?;
            #[cfg(feature = "cache-diagnostics")]
            let runtime = {
                let mut runtime = runtime;
                if let Some(context) = &self.diagnostic_context {
                    crate::cache_diagnostics::configure(&mut runtime, context);
                    // Do not resurrect disk state after a reset before lazy initialization,
                    // including when writing the reset tombstone failed.
                    if self.diagnostics_reset {
                        crate::cache_diagnostics::reset(&mut runtime, "engine_reset");
                    }
                }
                runtime
            };
            slot.runtime = Some(runtime);
        }
        Ok(slot
            .runtime
            .as_mut()
            .expect("a successfully initialized slot contains a runtime"))
    }
}

impl ModelProvider for ResponsesRouter {
    fn application_prompt(&self) -> &str {
        &self.preamble
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
        OpenAiProvider::validate_profile_settings(
            profile,
            target.reasoning_level,
            &self.session_id,
            &profile_cache_key(&self.session_id, profile),
        )?;
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
        zevria_responses::replay::preflight_with_compatibility(
            input,
            target,
            self.slots[index]
                .profile
                .endpoint
                .compatibility
                .developer_messages,
        )
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
            self.validate_input(index, &request)?;
            self.last_dispatched = Some(index);
            let level = selection.reasoning_level;
            let runtime = self.initialize_slot(index, level).await?;
            runtime.set_reasoning_level(level);
            runtime.complete(request, progress).await
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
            self.validate_input(index, &request)?;
            let level = selection.reasoning_level;
            let runtime = self.initialize_slot(index, level).await?;
            runtime.set_reasoning_level(level);
            runtime.count_input_tokens(request).await
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
            self.validate_input(route.slot, &request)?;
            let runtime = self
                .initialize_slot(route.slot, route.reasoning_level)
                .await?;
            runtime.set_reasoning_level(route.reasoning_level);
            runtime.complete(request, progress).await
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
            self.validate_input(route.slot, &request)?;
            let runtime = self
                .initialize_slot(route.slot, route.reasoning_level)
                .await?;
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
            self.validate_input(route.slot, &request)?;
            let runtime = self
                .initialize_slot(route.slot, route.reasoning_level)
                .await?;
            runtime.set_reasoning_level(route.reasoning_level);
            runtime.count_input_tokens(request).await
        })
    }

    fn reset(&mut self) {
        #[cfg(feature = "cache-diagnostics")]
        {
            self.diagnostics_reset = true;
        }
        for slot in &mut self.slots {
            #[cfg(feature = "cache-diagnostics")]
            if slot.runtime.is_none()
                && let Some(context) = &self.diagnostic_context
            {
                crate::cache_diagnostics::invalidate_uninitialized(context, &slot.profile.profile);
            }
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
/// Explore and Builder child subsessions, with a registry supplied per child.
#[derive(Clone)]
pub struct ResponsesRouterFactory {
    role: ModelRole,
    profile: ResolvedModelProfile,
    reasoning_level: ReasoningLevel,
    preamble: String,
}

impl ResponsesRouterFactory {
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

    pub fn create(
        &self,
        session_id: &str,
        tools: ToolServerHandle,
    ) -> anyhow::Result<ResponsesRouter> {
        ResponsesRouter::from_routes(
            [(self.role, self.profile.clone(), self.reasoning_level)],
            self.preamble.clone(),
            tools,
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
