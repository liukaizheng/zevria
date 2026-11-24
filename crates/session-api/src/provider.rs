//! Provider orchestration.

use super::*;
use zevria_model::estimates::estimate_model_input_for_profile;
/// The boxed future returned by a [`ModelProvider`].
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<ModelResponse>> + Send + 'a>>;

/// Cancellable best-effort idle preparation, never an accepted model request.
pub type PrewarmFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

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
/// directives and additional tool declarations at their recorded input positions,
/// and render `request.tools` verbatim in wire order (subject only to explicit
/// schema compatibility options). The allow-list enforces engine calls and gates
/// hosted capabilities; it never filters function declarations. `forbid_tool_calls`
/// suppresses calls without removing declarations. Preserve this immutable shape
/// across native retries and continuations.
/// `reset` reports that local history has diverged from any provider-side
/// continuation state — the turn failed — so the next `complete` must send
/// the full request history instead of continuing a server-side chain that
/// never saw those messages.
pub trait ModelProvider: Send + 'static {
    /// Application guidance captured by composition at startup/restoration.
    fn application_prompt(&self) -> &str {
        ""
    }

    /// Optional idle work for the selected role. The caller may drop this future
    /// when a command arrives; implementations must install state only after
    /// completion, perform no detached work and preserve completion recovery.
    fn prewarm(&mut self, _role: ModelRole) -> PrewarmFuture<'_> {
        Box::pin(async {})
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
            estimate_model_input_for_profile(input, target)?,
        ))
    }

    /// Optional local append lineage for adapters with incremental validation.
    /// Synthetic/prospective callers retain the ordinary full preflight path.
    fn preflight_input_with_identity(
        &self,
        target: &ModelProfileRef,
        input: &[ModelRequestItem<'_>],
        _identity: Option<&zevria_model::ModelInputIdentity>,
    ) -> anyhow::Result<crate::models::ReplayPreflight> {
        self.preflight_input(target, input)
    }

    /// Accounting compatibility only: inline declarations extend history;
    /// folded declarations replace the fixed tool prefix and invalidate usage.
    fn tool_append_preserves_prefix(&self, _target: &ModelProfileRef) -> bool {
        true
    }

    /// Pure assessment of the fully prepared logical request. Adapters override
    /// rendering, never request bytes. Incomplete media is compatible, not zero.
    fn assess_request(
        &self,
        context: &zevria_foundation::ModelContextPolicy,
        request: &ModelRequest<'_>,
    ) -> anyhow::Result<crate::models::ReplayPreflight> {
        let assessment = self.preflight_input_with_identity(
            &context.profile,
            &request.input,
            request.input_identity,
        )?;
        let crate::models::ReplayPreflight::Compatible(mut estimate) = assessment else {
            return Ok(assessment);
        };
        if context.image_accounting
            != zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved
        {
            estimate = zevria_model::estimates::estimate_model_input_with_policy(
                &request.input,
                &context.profile,
                &context.image_accounting,
            )?;
        }
        estimate.validate_media_constraints()?;
        let fixed = zevria_model::compaction::approximate_tokens(request.instructions)
            .saturating_add(zevria_model::compaction::approximate_tokens_from_bytes(
                rig_core::serde_json::to_vec(request.tools)?.len(),
            ));
        Ok(crate::models::ReplayPreflight::Compatible(
            estimate.saturating_add(zevria_model::ContextTokenEstimate::new(fixed, fixed)),
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

    /// Return completed usage in `ModelResponse`; do not await a completed-usage
    /// lifecycle publication here. Core commits the response before publishing
    /// attempt updates, usage and the intermediate/final outcome.
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
