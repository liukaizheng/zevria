//! Provider-centric OpenAI Responses and Responses-compatible configuration.

use std::{collections::BTreeMap, fmt};

use reqwest::header::HeaderName;
use rig_core::providers::openai::responses_api::{ReasoningEffort, ReasoningSummaryLevel};
use serde::{Deserialize, Serialize};
use zevria_foundation::ModelContextPolicy;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::{ModelRole, ReasoningLevel};
use zevria_responses::WebSearchConfig;

/// Literal API credential from the configuration file.
///
/// Zevria deliberately performs no environment interpolation or provider-key
/// lookup. Debug output is always redacted; only request construction inside
/// this crate can access the underlying value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LiteralApiKey(String);

impl LiteralApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LiteralApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// One Responses-compatible endpoint and its complete model catalog.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Exact full Responses endpoint. HTTP and WebSocket pairing changes only
    /// this URL's scheme.
    pub base_url: String,
    pub api_key: LiteralApiKey,
    /// Prefer a best-effort WebSocket preconnect before sticky HTTP fallback.
    pub supports_websockets: bool,
    /// Optional routing header whose value is the persistent conversation ID.
    /// Omission sends no session header; the name is provider-specific.
    #[serde(default)]
    pub session_id_header: Option<String>,
    /// The map key is the exact case-sensitive model ID sent on the wire.
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub compatibility: ResponsesCompatibilityConfig,
    /// Gateway-specific top-level Responses request fields. Structural fields
    /// owned by Zevria are reserved and rejected during validation.
    #[serde(default)]
    pub additional_params: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub compaction: RemoteCompactionConfig,
    #[serde(default)]
    pub input_token_count: InputTokenCountConfig,
    #[serde(default)]
    pub web_search: WebSearchConfig,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field(
                "base_url",
                &crate::connection::redacted_url_for_logging(&self.base_url),
            )
            .field("api_key", &self.api_key)
            .field("supports_websockets", &self.supports_websockets)
            .field("session_id_header", &self.session_id_header)
            .field("models", &self.models)
            .field("compatibility", &self.compatibility)
            .field("additional_params", &self.additional_params)
            .field("compaction", &self.compaction)
            .field("input_token_count", &self.input_token_count)
            .field("web_search", &self.web_search)
            .finish()
    }
}

impl ProviderConfig {
    fn validate(&self, provider: &str, auto_trigger_percent: u64) -> anyhow::Result<()> {
        validate_responses_url(&self.base_url, &format!("providers.{provider}.base_url"))?;
        validate_nonblank(
            self.api_key.expose(),
            &format!("providers.{provider}.api_key"),
        )?;
        if self.models.is_empty() {
            anyhow::bail!("providers.{provider}.models must define at least one model");
        }
        for (model, config) in &self.models {
            if model.trim().is_empty() {
                anyhow::bail!("providers.{provider}.models contains a blank model ID");
            }
            config.validate(provider, model, auto_trigger_percent)?;
        }
        parse_session_id_header(provider, self.session_id_header.as_deref())?;
        validate_additional_params(provider, &self.additional_params)?;
        self.web_search.validate(provider)?;
        self.compaction.validate(provider)?;
        self.input_token_count.validate(provider)
    }

    pub fn redacted_base_url(&self) -> String {
        crate::connection::redacted_url_for_logging(&self.base_url)
    }

    fn resolved_endpoint(&self) -> ProviderEndpoint {
        ProviderEndpoint {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            supports_websockets: self.supports_websockets,
            session_id_header: self.session_id_header.clone(),
            compatibility: self.compatibility.clone(),
            additional_params: self.additional_params.clone(),
            compaction: self.compaction.clone(),
            input_token_count: self.input_token_count.clone(),
            web_search: self.web_search.clone(),
        }
    }
}

/// Model-owned context and reasoning settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub context_window_tokens: u64,
    #[serde(default)]
    pub input_token_limit: Option<u64>,
    pub retained_user_tokens: u64,
    pub reasoning_levels: Vec<ReasoningLevel>,
    pub reasoning_summary_level: ReasoningSummaryLevel,
}

impl ModelConfig {
    fn validate(
        &self,
        provider: &str,
        model: &str,
        auto_trigger_percent: u64,
    ) -> anyhow::Result<()> {
        let prefix = format!("providers.{provider}.models.{model}");
        anyhow::ensure!(
            !self.reasoning_levels.is_empty(),
            "{prefix}.reasoning_levels must not be empty"
        );
        let unique = self
            .reasoning_levels
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        anyhow::ensure!(
            unique.len() == self.reasoning_levels.len(),
            "{prefix}.reasoning_levels must not contain duplicates"
        );
        if self.context_window_tokens == 0 {
            anyhow::bail!("{prefix}.context_window_tokens must be greater than zero");
        }
        let input_token_limit = self.input_token_limit.unwrap_or(self.context_window_tokens);
        if input_token_limit == 0 {
            anyhow::bail!("{prefix}.input_token_limit must be greater than zero");
        }
        if input_token_limit > self.context_window_tokens {
            anyhow::bail!(
                "{prefix}.input_token_limit ({input_token_limit}) may not exceed context_window_tokens ({})",
                self.context_window_tokens
            );
        }
        let trigger = automatic_trigger(input_token_limit, auto_trigger_percent);
        if self.retained_user_tokens > trigger {
            anyhow::bail!(
                "{prefix}.retained_user_tokens ({}) may not exceed the automatic trigger ({trigger})",
                self.retained_user_tokens
            );
        }
        Ok(())
    }
}

// Both enums live in other crates, so Rust's orphan rules rule out a From impl here.
pub(crate) fn reasoning_effort(level: ReasoningLevel) -> ReasoningEffort {
    match level {
        ReasoningLevel::None => ReasoningEffort::None,
        ReasoningLevel::Minimal => ReasoningEffort::Minimal,
        ReasoningLevel::Low => ReasoningEffort::Low,
        ReasoningLevel::Medium => ReasoningEffort::Medium,
        ReasoningLevel::High => ReasoningEffort::High,
        ReasoningLevel::Xhigh => ReasoningEffort::Xhigh,
        ReasoningLevel::Max => ReasoningEffort::Max,
    }
}

/// One mode's exact provider/model selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAssignment {
    pub provider: String,
    pub model: String,
    pub reasoning_level: ReasoningLevel,
}

impl ModelAssignment {
    pub fn selection(&self) -> zevria_model::models::ModelSelection {
        zevria_model::models::ModelSelection::new(self.profile_ref(), self.reasoning_level)
    }

    pub fn profile_ref(&self) -> ModelProfileRef {
        ModelProfileRef::new(self.provider.clone(), self.model.clone())
    }
}

/// Required independent selections for every model role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeAssignments {
    pub build: ModelAssignment,
    pub plan: ModelAssignment,
    pub review: ModelAssignment,
    pub explore: ModelAssignment,
    pub builder: ModelAssignment,
}

impl ModeAssignments {
    pub fn for_role(&self, role: ModelRole) -> &ModelAssignment {
        match role {
            ModelRole::Build => &self.build,
            ModelRole::Plan => &self.plan,
            ModelRole::Review => &self.review,
            ModelRole::Explore => &self.explore,
            ModelRole::Builder => &self.builder,
        }
    }
}

/// Optional Responses request features that compatible gateways may not
/// implement. Defaults preserve Zevria's established request shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponsesCompatibilityConfig {
    /// Explicit gateway workaround; user-role projection has weaker authority.
    pub developer_messages: bool,
    pub send_reasoning: bool,
    pub send_reasoning_encrypted_content: bool,
    pub strict_tools: bool,
    pub send_prompt_cache_key: bool,
    pub send_store: bool,
}

impl Default for ResponsesCompatibilityConfig {
    fn default() -> Self {
        Self {
            developer_messages: true,
            send_reasoning: true,
            send_reasoning_encrypted_content: true,
            strict_tools: true,
            send_prompt_cache_key: true,
            send_store: true,
        }
    }
}

/// Explicit Responses compaction endpoint. It is deliberately not inferred
/// from the ordinary Responses URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteCompactionConfig {
    pub url: Option<String>,
    pub request_timeout_seconds: u64,
}

impl Default for RemoteCompactionConfig {
    fn default() -> Self {
        Self {
            url: None,
            request_timeout_seconds: 300,
        }
    }
}

impl RemoteCompactionConfig {
    fn validate(&self, provider: &str) -> anyhow::Result<()> {
        let prefix = format!("providers.{provider}.compaction");
        if self.request_timeout_seconds == 0 {
            anyhow::bail!("{prefix}.request_timeout_seconds must be greater than zero");
        }
        if let Some(url) = &self.url {
            let parsed = reqwest::Url::parse(url)
                .map_err(|error| anyhow::anyhow!("invalid {prefix}.url: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                anyhow::bail!("{prefix}.url must use http or https");
            }
        }
        Ok(())
    }
}

/// Optional exact Responses input-token counting. Standard OpenAI-compatible
/// deployments derive `/responses/input_tokens`; gateways can override or
/// disable that derivation independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputTokenCountConfig {
    pub enabled: bool,
    pub derive_url: bool,
    pub url: Option<String>,
    pub request_timeout_seconds: u64,
}

impl Default for InputTokenCountConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            derive_url: true,
            url: None,
            request_timeout_seconds: 30,
        }
    }
}

impl InputTokenCountConfig {
    fn validate(&self, provider: &str) -> anyhow::Result<()> {
        let prefix = format!("providers.{provider}.input_token_count");
        if self.request_timeout_seconds == 0 {
            anyhow::bail!("{prefix}.request_timeout_seconds must be greater than zero");
        }
        if let Some(url) = &self.url {
            let parsed = reqwest::Url::parse(url)
                .map_err(|error| anyhow::anyhow!("invalid {prefix}.url: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                anyhow::bail!("{prefix}.url must use http or https");
            }
        }
        Ok(())
    }
}

pub(crate) const RESERVED_ADDITIONAL_PARAM_KEYS: [&str; 11] = [
    "model",
    "input",
    "instructions",
    "tools",
    "stream",
    "include",
    "previous_response_id",
    "store",
    "background",
    "reasoning",
    "prompt_cache_key",
];

pub(crate) fn validate_additional_params(
    provider: &str,
    additional_params: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    for key in additional_params.keys() {
        if RESERVED_ADDITIONAL_PARAM_KEYS.contains(&key.as_str()) {
            anyhow::bail!("providers.{provider}.additional_params.{key} is reserved by Zevria");
        }
    }
    Ok(())
}

/// Parse a provider's opt-in routing header without allowing it to replace
/// authentication, payload framing, or WebSocket negotiation headers.
pub(crate) fn parse_session_id_header(
    provider: &str,
    configured: Option<&str>,
) -> anyhow::Result<Option<HeaderName>> {
    let Some(configured) = configured else {
        return Ok(None);
    };
    let field = format!("providers.{provider}.session_id_header");
    let name = HeaderName::from_bytes(configured.as_bytes())
        .map_err(|_| anyhow::anyhow!("{field} must be a valid, nonempty HTTP header name"))?;
    if matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "host"
            | "accept"
            | "content-type"
            | "content-length"
            | "content-encoding"
            | "connection"
            | "upgrade"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "expect"
    ) || name.as_str().starts_with("sec-websocket-")
    {
        anyhow::bail!("{field} conflicts with an authentication or transport-owned header");
    }
    Ok(Some(name))
}

/// Provider-level runtime settings shared by all models in one catalog.
#[derive(Clone)]
pub struct ProviderEndpoint {
    pub base_url: String,
    pub(crate) api_key: LiteralApiKey,
    pub supports_websockets: bool,
    /// Optional provider-specific header carrying the persistent conversation ID.
    pub session_id_header: Option<String>,
    pub compatibility: ResponsesCompatibilityConfig,
    pub additional_params: BTreeMap<String, serde_json::Value>,
    pub compaction: RemoteCompactionConfig,
    pub input_token_count: InputTokenCountConfig,
    pub web_search: WebSearchConfig,
}

impl ProviderEndpoint {
    pub(crate) fn api_key(&self) -> &str {
        self.api_key.expose()
    }

    pub fn redacted_base_url(&self) -> String {
        crate::connection::redacted_url_for_logging(&self.base_url)
    }
}

impl fmt::Debug for ProviderEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderEndpoint")
            .field("base_url", &self.redacted_base_url())
            .field("api_key", &self.api_key)
            .field("supports_websockets", &self.supports_websockets)
            .field("session_id_header", &self.session_id_header)
            .field("compatibility", &self.compatibility)
            .field("additional_params", &self.additional_params)
            .field("compaction", &self.compaction)
            .field("input_token_count", &self.input_token_count)
            .field("web_search", &self.web_search)
            .finish()
    }
}

/// Fully owned runtime profile selected by one or more model roles.
#[derive(Debug, Clone)]
pub struct ResolvedModelProfile {
    pub profile: ModelProfileRef,
    pub endpoint: ProviderEndpoint,
    pub context_window_tokens: u64,
    pub input_token_limit: u64,
    pub retained_user_tokens: u64,
    pub reasoning_levels: Vec<ReasoningLevel>,
    pub reasoning_summary_level: ReasoningSummaryLevel,
}

impl ResolvedModelProfile {
    pub fn context_policy(&self) -> ModelContextPolicy {
        ModelContextPolicy {
            profile: self.profile.clone(),
            context_window_tokens: self.context_window_tokens,
            input_token_limit: self.input_token_limit,
            retained_user_tokens: self.retained_user_tokens,
        }
    }
}

/// Validated immutable selectable catalog and initial role assignments.
#[derive(Debug, Clone)]
pub struct ModelRouting {
    routes: [zevria_model::models::ModelSelection; ModelRole::COUNT],
    catalog: BTreeMap<ModelProfileRef, ResolvedModelProfile>,
}

impl ModelRouting {
    pub fn resolve(
        providers: &BTreeMap<String, ProviderConfig>,
        modes: &ModeAssignments,
        auto_trigger_percent: u64,
    ) -> anyhow::Result<Self> {
        if providers.is_empty() {
            anyhow::bail!("providers must define at least one provider");
        }
        if !(1..=100).contains(&auto_trigger_percent) {
            anyhow::bail!("session.compaction.auto_trigger_percent must be between 1 and 100");
        }
        for (provider, config) in providers {
            if provider.trim().is_empty() {
                anyhow::bail!("providers contains a blank provider key");
            }
            config.validate(provider, auto_trigger_percent)?;
        }

        let mut routes = Vec::with_capacity(ModelRole::COUNT);
        for role in ModelRole::ALL {
            let assignment = modes.for_role(role);
            validate_nonblank(
                &assignment.provider,
                &format!("modes.{}.provider", role.name()),
            )?;
            validate_nonblank(&assignment.model, &format!("modes.{}.model", role.name()))?;
            let Some(provider) = providers.get(&assignment.provider) else {
                let available = providers.keys().cloned().collect::<Vec<_>>().join(", ");
                anyhow::bail!(
                    "modes.{} references unknown provider {:?}; available providers: {}",
                    role.name(),
                    assignment.provider,
                    display_candidates(&available)
                );
            };
            let Some(model) = provider.models.get(&assignment.model) else {
                let available = provider
                    .models
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "modes.{} references unknown model {:?} for provider {:?}; available models: {}",
                    role.name(),
                    assignment.model,
                    assignment.provider,
                    display_candidates(&available)
                );
            };
            anyhow::ensure!(
                model.reasoning_levels.contains(&assignment.reasoning_level),
                "modes.{}.reasoning_level {} is unsupported for {}; supported reasoning_levels: {}",
                role.name(),
                assignment.reasoning_level,
                assignment.profile_ref(),
                model
                    .reasoning_levels
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            routes.push(assignment.selection());
        }
        let routes = routes.try_into().map_err(|_| {
            anyhow::anyhow!("internal error while resolving the five required model roles")
        })?;
        let mut catalog = BTreeMap::new();
        for (provider_key, provider) in providers {
            for (model_key, model) in &provider.models {
                let profile = ModelProfileRef::new(provider_key, model_key);
                catalog.insert(
                    profile.clone(),
                    ResolvedModelProfile {
                        profile,
                        endpoint: provider.resolved_endpoint(),
                        context_window_tokens: model.context_window_tokens,
                        input_token_limit: model
                            .input_token_limit
                            .unwrap_or(model.context_window_tokens),
                        retained_user_tokens: model.retained_user_tokens,
                        reasoning_levels: model.reasoning_levels.clone(),
                        reasoning_summary_level: model.reasoning_summary_level.clone(),
                    },
                );
            }
        }
        Ok(Self { routes, catalog })
    }

    pub fn with_role_reasoning_level(
        mut self,
        role: ModelRole,
        level: ReasoningLevel,
    ) -> anyhow::Result<Self> {
        let profile = self.for_role(role);
        anyhow::ensure!(
            profile.reasoning_levels.contains(&level),
            "reasoning level {level} is not configured for {}",
            profile.profile
        );
        self.routes[role.index()].reasoning_level = level;
        Ok(self)
    }

    pub fn catalog(&self) -> impl Iterator<Item = &ResolvedModelProfile> {
        self.catalog.values()
    }

    pub fn for_role(&self, role: ModelRole) -> &ResolvedModelProfile {
        &self.catalog[&self.selection_for_role(role).profile]
    }

    pub fn selection_for_role(&self, role: ModelRole) -> &zevria_model::models::ModelSelection {
        &self.routes[role.index()]
    }

    pub fn context_policies(&self) -> [ModelContextPolicy; ModelRole::COUNT] {
        std::array::from_fn(|index| self.for_role(ModelRole::ALL[index]).context_policy())
    }

    pub fn assignments(&self) -> impl Iterator<Item = (ModelRole, &ResolvedModelProfile)> {
        ModelRole::ALL
            .into_iter()
            .map(|role| (role, self.for_role(role)))
    }
}

fn validate_responses_url(value: &str, field: &str) -> anyhow::Result<()> {
    let parsed =
        reqwest::Url::parse(value).map_err(|error| anyhow::anyhow!("invalid {field}: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        anyhow::bail!("{field} must use http, https, ws, or wss");
    }
    Ok(())
}

fn validate_nonblank(value: &str, field: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{field} must not be blank");
    }
    Ok(())
}

fn automatic_trigger(context_window_tokens: u64, auto_trigger_percent: u64) -> u64 {
    let product =
        u128::from(context_window_tokens).saturating_mul(u128::from(auto_trigger_percent));
    u64::try_from(product / 100).unwrap_or(u64::MAX)
}

fn display_candidates(candidates: &str) -> &str {
    if candidates.is_empty() {
        "<none>"
    } else {
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(window: u64) -> ModelConfig {
        ModelConfig {
            context_window_tokens: window,
            input_token_limit: None,
            retained_user_tokens: window / 10,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            reasoning_summary_level: ReasoningSummaryLevel::Detailed,
        }
    }

    fn provider(base_url: &str, key: &str) -> ProviderConfig {
        ProviderConfig {
            base_url: base_url.to_string(),
            api_key: LiteralApiKey::new(key),
            supports_websockets: true,
            session_id_header: None,
            models: BTreeMap::from([
                ("gpt-5.6-sol".to_string(), model(272_000)),
                ("vendor.model.v2".to_string(), model(128_000)),
            ]),
            compatibility: ResponsesCompatibilityConfig::default(),
            additional_params: BTreeMap::new(),
            compaction: RemoteCompactionConfig::default(),
            input_token_count: InputTokenCountConfig::default(),
            web_search: WebSearchConfig::default(),
        }
    }

    fn modes() -> ModeAssignments {
        ModeAssignments {
            build: ModelAssignment {
                provider: "openai".to_string(),
                model: "gpt-5.6-sol".to_string(),
                reasoning_level: ReasoningLevel::Medium,
            },
            plan: ModelAssignment {
                provider: "gateway".to_string(),
                model: "vendor.model.v2".to_string(),
                reasoning_level: ReasoningLevel::Medium,
            },
            review: ModelAssignment {
                provider: "openai".to_string(),
                model: "gpt-5.6-sol".to_string(),
                reasoning_level: ReasoningLevel::Medium,
            },
            explore: ModelAssignment {
                provider: "gateway".to_string(),
                model: "vendor.model.v2".to_string(),
                reasoning_level: ReasoningLevel::Medium,
            },
            builder: ModelAssignment {
                provider: "openai".to_string(),
                model: "gpt-5.6-sol".to_string(),
                reasoning_level: ReasoningLevel::Medium,
            },
        }
    }

    #[test]
    fn resolves_two_provider_catalogs_and_dotted_wire_ids() {
        let mut gateway = provider("http://gateway.test/v1/responses", "gateway-key");
        gateway.supports_websockets = false;
        gateway.compatibility.strict_tools = false;
        gateway
            .additional_params
            .insert("gateway_routing".to_string(), json!("pool-a"));
        let providers = BTreeMap::from([
            (
                "openai".to_string(),
                provider("https://api.openai.com/v1/responses", "openai-key"),
            ),
            ("gateway".to_string(), gateway),
        ]);

        let routing = ModelRouting::resolve(&providers, &modes(), 90).expect("valid routing");
        assert_eq!(
            routing.for_role(ModelRole::Build).profile.model,
            "gpt-5.6-sol"
        );
        assert_eq!(
            routing.for_role(ModelRole::Plan).profile.provider,
            "gateway"
        );
        assert_eq!(
            routing.for_role(ModelRole::Plan).context_window_tokens,
            128_000
        );
        assert!(
            !routing
                .for_role(ModelRole::Plan)
                .endpoint
                .supports_websockets
        );
    }

    #[test]
    fn hosted_settings_propagate_through_every_selected_profile() {
        let mut enabled = provider("https://api.openai.com/v1/responses", "key");
        enabled.web_search.enabled = true;
        enabled.web_search.external_web_access = false;
        enabled.web_search.return_token_budget = crate::SearchReturnTokenBudget::Unlimited;
        let providers = BTreeMap::from([
            ("openai".to_string(), enabled.clone()),
            ("gateway".to_string(), enabled.clone()),
        ]);
        let routing = ModelRouting::resolve(&providers, &modes(), 90).unwrap();
        for role in ModelRole::ALL {
            assert_eq!(
                routing.for_role(role).endpoint.web_search,
                enabled.web_search
            );
        }
    }

    #[test]
    fn the_same_wire_model_id_is_legal_under_different_providers() {
        let providers = BTreeMap::from([
            (
                "a".to_string(),
                provider("https://a.test/responses", "a-key"),
            ),
            (
                "b".to_string(),
                provider("https://b.test/responses", "b-key"),
            ),
        ]);
        let assignment = |provider: &str| ModelAssignment {
            provider: provider.to_string(),
            model: "gpt-5.6-sol".to_string(),
            reasoning_level: ReasoningLevel::Medium,
        };
        let modes = ModeAssignments {
            build: assignment("a"),
            plan: assignment("b"),
            review: assignment("a"),
            explore: assignment("b"),
            builder: assignment("a"),
        };
        ModelRouting::resolve(&providers, &modes, 90).expect("same model ID is provider-scoped");
    }

    #[test]
    fn api_keys_and_url_credentials_are_redacted_from_debug() {
        let config = provider(
            "https://user:password@example.test/responses?token=url-secret#fragment",
            "literal-secret",
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("literal-secret"));
        assert!(!debug.contains("password"));
        assert!(!debug.contains("url-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn reasoning_sets_are_required_unique_nonempty_and_contain_the_default() {
        let valid = model(1000);
        let mut value = serde_json::to_value(&valid).unwrap();
        value.as_object_mut().unwrap().remove("reasoning_levels");
        assert!(
            serde_json::from_value::<ModelConfig>(value)
                .unwrap_err()
                .to_string()
                .contains("reasoning_levels")
        );
        for levels in [vec![], vec![ReasoningLevel::Medium, ReasoningLevel::Medium]] {
            let mut invalid = valid.clone();
            invalid.reasoning_levels = levels;
            assert!(
                invalid
                    .validate("p", "m", 90)
                    .unwrap_err()
                    .to_string()
                    .starts_with("providers.p.models.m.reasoning_levels")
            );
        }
        for level in ReasoningLevel::ALL {
            let mut valid = valid.clone();
            valid.reasoning_levels = vec![level];
            valid.validate("p", "m", 90).unwrap();
            assert_eq!(
                serde_json::to_value(reasoning_effort(level)).unwrap(),
                serde_json::to_value(level).unwrap()
            );
        }
    }

    #[test]
    fn optional_provider_tables_keep_behavioral_defaults() {
        let parsed: ProviderConfig = toml::from_str(
            r#"
base_url = "https://example.test/v1/responses"
api_key = "secret"
supports_websockets = true
[models.model]
context_window_tokens = 1000
retained_user_tokens = 100
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
"#,
        )
        .expect("optional provider tables");
        assert_eq!(
            parsed.compatibility,
            ResponsesCompatibilityConfig::default()
        );
        assert_eq!(parsed.compaction, RemoteCompactionConfig::default());
        assert_eq!(parsed.input_token_count, InputTokenCountConfig::default());
        assert_eq!(parsed.models["model"].input_token_limit, None);
        assert_eq!(parsed.session_id_header, None);
        assert!(parsed.additional_params.is_empty());
    }

    #[test]
    fn session_id_headers_are_optional_provider_scoped_and_round_trip() {
        let mut gateway = provider("https://gateway.test/responses", "gateway-key");
        gateway.session_id_header = Some("X-Conversation-ID".to_string());
        let providers = BTreeMap::from([
            (
                "openai".to_string(),
                provider("https://openai.test/responses", "openai-key"),
            ),
            ("gateway".to_string(), gateway.clone()),
        ]);
        let routing = ModelRouting::resolve(&providers, &modes(), 90).expect("custom header");
        assert_eq!(
            routing
                .for_role(ModelRole::Build)
                .endpoint
                .session_id_header,
            None
        );
        assert_eq!(
            routing
                .for_role(ModelRole::Plan)
                .endpoint
                .session_id_header
                .as_deref(),
            Some("X-Conversation-ID")
        );
        let restored: ProviderConfig = toml::from_str(&toml::to_string(&gateway).unwrap()).unwrap();
        assert_eq!(restored.session_id_header, gateway.session_id_header);
        assert!(format!("{gateway:?}").contains("X-Conversation-ID"));
        assert!(
            format!("{:?}", routing.for_role(ModelRole::Plan).endpoint)
                .contains("X-Conversation-ID")
        );
        for name in [
            "x-opencode-session",
            "X-Conversation-ID",
            "session-id",
            "thread-id",
        ] {
            let parsed = parse_session_id_header("gateway", Some(name))
                .unwrap()
                .unwrap();
            assert_eq!(parsed.as_str(), name.to_ascii_lowercase());
        }
    }

    #[test]
    fn session_id_header_validation_rejects_malformed_and_transport_owned_names() {
        assert!(parse_session_id_header("gateway", None).unwrap().is_none());
        for name in [
            "",
            " ",
            " session-id",
            "session-id ",
            "bad:name",
            "bad name",
            "x-会话",
            "x\r\ninjected",
            "x\0",
        ] {
            let error = parse_session_id_header("gateway", Some(name)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("providers.gateway.session_id_header")
            );
        }
        for name in [
            "authorization",
            "proxy-authorization",
            "host",
            "accept",
            "content-type",
            "content-length",
            "content-encoding",
            "connection",
            "upgrade",
            "transfer-encoding",
            "te",
            "trailer",
            "expect",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
            "sec-websocket-future",
        ] {
            for name in [name.to_string(), name.to_ascii_uppercase()] {
                let error = parse_session_id_header("gateway", Some(&name)).unwrap_err();
                assert!(
                    error.to_string().contains("transport-owned"),
                    "{name}: {error}"
                );
            }
        }

        // Even unreferenced catalogs must be locally valid.
        let mut invalid = provider("https://unused.test/responses", "key");
        invalid.session_id_header = Some("Authorization".to_string());
        let providers = BTreeMap::from([
            (
                "openai".to_string(),
                provider("https://a.test/responses", "a-key"),
            ),
            (
                "gateway".to_string(),
                provider("https://b.test/responses", "b-key"),
            ),
            ("unused".to_string(), invalid),
        ]);
        let error = ModelRouting::resolve(&providers, &modes(), 90).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("providers.unused.session_id_header")
        );
    }

    #[test]
    fn validation_rejects_catalog_and_assignment_errors() {
        let empty = BTreeMap::new();
        assert!(ModelRouting::resolve(&empty, &modes(), 90).is_err());

        let mut providers = BTreeMap::from([(
            "openai".to_string(),
            provider("https://example.test/responses", "key"),
        )]);
        let error = ModelRouting::resolve(&providers, &modes(), 90)
            .expect_err("dangling provider must fail");
        assert!(error.to_string().contains("modes.plan"));
        assert!(error.to_string().contains("openai"));

        providers
            .get_mut("openai")
            .expect("provider")
            .models
            .clear();
        let error = ModelRouting::resolve(&providers, &modes(), 90)
            .expect_err("empty model catalog must fail");
        assert!(error.to_string().contains("providers.openai.models"));
    }

    #[test]
    fn validation_rejects_bad_endpoint_limits_and_reserved_params() {
        let mut providers = BTreeMap::from([
            (
                "openai".to_string(),
                provider("ftp://example.test/responses", "key"),
            ),
            (
                "gateway".to_string(),
                provider("https://gateway.test/responses", "key"),
            ),
        ]);
        assert!(ModelRouting::resolve(&providers, &modes(), 90).is_err());

        let openai = providers.get_mut("openai").expect("provider");
        openai.base_url = "https://example.test/responses".to_string();
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .context_window_tokens = 0;
        assert!(ModelRouting::resolve(&providers, &modes(), 90).is_err());

        let openai = providers.get_mut("openai").expect("provider");
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .context_window_tokens = 100;
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .input_token_limit = Some(0);
        assert!(ModelRouting::resolve(&providers, &modes(), 90).is_err());

        let openai = providers.get_mut("openai").expect("provider");
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .input_token_limit = Some(101);
        assert!(ModelRouting::resolve(&providers, &modes(), 90).is_err());

        let openai = providers.get_mut("openai").expect("provider");
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .input_token_limit = Some(100);
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .retained_user_tokens = 91;
        assert!(ModelRouting::resolve(&providers, &modes(), 90).is_err());

        let openai = providers.get_mut("openai").expect("provider");
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .retained_user_tokens = 10;
        openai
            .models
            .get_mut("gpt-5.6-sol")
            .expect("model")
            .input_token_limit = None;
        openai
            .additional_params
            .insert("model".to_string(), json!("override"));
        let error =
            ModelRouting::resolve(&providers, &modes(), 90).expect_err("reserved field must fail");
        assert!(
            error
                .to_string()
                .contains("providers.openai.additional_params.model")
        );

        let openai = providers.get_mut("openai").expect("provider");
        openai.additional_params.remove("model");
        openai.additional_params.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
        let error = ModelRouting::resolve(&providers, &modes(), 90)
            .expect_err("Zevria-owned include must fail");
        assert!(
            error
                .to_string()
                .contains("providers.openai.additional_params.include")
        );
    }

    #[test]
    fn strict_model_and_provider_shapes_reject_missing_or_unknown_fields() {
        let missing = toml::from_str::<ProviderConfig>(
            "base_url = \"https://example.test/responses\"\napi_key = \"key\"\nsupports_websockets = true",
        )
        .expect_err("models is required");
        assert!(missing.to_string().contains("models"));

        let unknown = toml::from_str::<ModelConfig>(
            r#"
context_window_tokens = 1000
retained_user_tokens = 100
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
id = "alias"
"#,
        )
        .expect_err("model aliases are not supported");
        assert!(unknown.to_string().contains("id"));
    }
}
