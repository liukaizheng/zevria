//! Provider orchestration.

use super::*;
use zevria_model::estimates::estimate_model_input_for_profile;
/// The boxed future returned by a [`ModelProvider`].
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<ModelResponse>> + Send + 'a>>;

/// The boxed future returned by [`ModelProvider::compact`].
pub type CompactFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<CompactResult>> + Send + 'a>>;

/// The boxed future returned by [`ModelProvider::count_input_tokens`].
pub type InputTokenCountFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<InputTokenCount>> + Send + 'a>>;

/// Compile-time model-provider extension point.
///
/// Implementations translate [`ModelRequest`] into their native protocol. They
/// must map `request.model_role` to a concrete provider model, send
/// `request.instructions` as the sole top-level instructions, project
/// developer instructions in their recorded input positions, advertise only
/// `request.allowed_tool_names` when an allow-list is present, and preserve
/// that policy across native retries and continuations.
/// `reset` reports that local history has diverged from any provider-side
/// continuation state — the turn failed — so the next `complete` must send
/// the full request history instead of continuing a server-side chain that
/// never saw those messages.
pub trait ModelProvider: Send + 'static {
    /// Application guidance captured by composition at startup/restoration.
    fn application_prompt(&self) -> &str {
        ""
    }

    /// Immutable selectable descriptors, without credentials or endpoint settings.
    fn model_catalog(&self) -> Vec<crate::models::ModelCandidate> {
        Vec::new()
    }

    fn prepare_model_update(
        &self,
        _role: ModelRole,
        target: &crate::models::ModelSelection,
    ) -> anyhow::Result<ModelContextPolicy> {
        let candidate = self
            .model_catalog()
            .into_iter()
            .find(|candidate| candidate.context.profile == target.profile)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "profile {} is unavailable in this session catalog",
                    target.profile
                )
            })?;
        anyhow::ensure!(
            candidate.reasoning_levels.contains(&target.reasoning_level),
            "reasoning level {} is not configured for {}",
            target.reasoning_level,
            target.profile
        );
        Ok(candidate.context)
    }

    /// Called only with a successfully prepared update from this immutable catalog.
    fn install_model_update(&mut self, _role: ModelRole, _target: &crate::models::ModelSelection) {
        unreachable!("provider does not support model updates")
    }

    fn model_selection(&self, _role: ModelRole) -> Option<crate::models::ModelSelection> {
        None
    }

    fn preflight_input(
        &self,
        target: &ModelProfileRef,
        input: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<crate::models::ReplayPreflight> {
        Ok(crate::models::ReplayPreflight::Compatible(
            estimate_model_input_for_profile(input.to_vec(), target)?,
        ))
    }

    /// Explicit-profile maintenance never temporarily changes a role route.
    fn complete_profile<'a>(
        &'a mut self,
        _selection: &'a crate::models::ModelSelection,
        _request: ModelRequest<'a>,
        _progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async { anyhow::bail!("source-profile completion is unavailable") })
    }

    fn count_profile<'a>(
        &'a mut self,
        _selection: &'a crate::models::ModelSelection,
        _request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async { Ok(InputTokenCount::Unsupported) })
    }

    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a>;

    /// Ask the provider's explicitly configured native endpoint to replace
    /// this ordered history. Unsupported discovery performs no model call.
    fn compact<'a>(&'a mut self, _request: ModelRequest<'a>) -> CompactFuture<'a> {
        Box::pin(async { Ok(CompactResult::Unsupported) })
    }

    /// Count the exact full logical request without a completion call or
    /// mutating completion continuation state.
    fn count_input_tokens<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async { Ok(InputTokenCount::Unsupported) })
    }

    fn reset(&mut self);

    /// Stop an in-flight request after its completion future has been
    /// dropped. Providers with connection-scoped continuation state should
    /// terminate or invalidate it; the default is the ordinary reset path.
    fn cancel(&mut self) {
        self.reset();
    }
}
