use super::*;
use zevria_anthropic::{Compatibility, RefusalFallback, ThinkingDisplay};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicAuth {
    #[default]
    #[serde(rename = "x-api-key")]
    ApiKey,
    Bearer,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicProviderConfig {
    pub base_url: String,
    pub api_key: ApiKeySource,
    #[serde(default)]
    pub auth: AnthropicAuth,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub session_id_header: Option<String>,
    pub models: BTreeMap<String, AnthropicModelConfig>,
    #[serde(default)]
    pub compatibility: Compatibility,
    #[serde(default)]
    pub additional_params: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub input_token_count: InputTokenCountConfig,
    #[serde(default)]
    pub beta_headers: Vec<String>,
}
impl fmt::Debug for AnthropicProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicProviderConfig")
            .field("endpoint", &self.endpoint())
            .field("models", &self.models)
            .finish()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicModelConfig {
    pub context_window_tokens: u64,
    #[serde(default)]
    pub input_token_limit: Option<u64>,
    pub max_output_tokens: u64,
    pub retained_user_tokens: u64,
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub thinking_display: ThinkingDisplay,
    #[serde(default)]
    pub refusal_fallback: Option<RefusalFallback>,
}
impl AnthropicModelConfig {
    pub fn effective_input_limit(&self) -> u64 {
        self.input_token_limit.unwrap_or_else(|| {
            self.context_window_tokens
                .saturating_sub(self.max_output_tokens)
        })
    }
    pub(crate) fn validate(&self, provider: &str, key: &str, percent: u64) -> anyhow::Result<()> {
        let prefix = format!("providers.{provider}.models.{key}");
        anyhow::ensure!(
            self.max_output_tokens > 0 && self.max_output_tokens < self.context_window_tokens,
            "{prefix}.max_output_tokens must be positive and smaller than context_window_tokens"
        );
        anyhow::ensure!(
            !self
                .reasoning_levels
                .iter()
                .any(|level| matches!(level, ReasoningLevel::None | ReasoningLevel::Minimal)),
            "{prefix}.reasoning_levels must be a subset of low, medium, high, xhigh, max (none/minimal are unsupported)"
        );
        anyhow::ensure!(
            self.effective_input_limit() <= self.context_window_tokens - self.max_output_tokens,
            "{prefix}.input_token_limit may not exceed context_window_tokens - max_output_tokens"
        );
        ModelConfig {
            context_window_tokens: self.context_window_tokens,
            input_token_limit: Some(self.effective_input_limit()),
            retained_user_tokens: self.retained_user_tokens,
            reasoning_levels: self.reasoning_levels.clone(),
            reasoning_summary_level: ReasoningSummaryLevel::Auto,
        }
        .validate(provider, key, percent)
    }
}
#[derive(Clone)]
pub struct AnthropicEndpoint {
    pub base_url: String,
    pub(crate) api_key: ApiKeySource,
    pub auth: AnthropicAuth,
    pub network: NetworkConfig,
    pub session_id_header: Option<String>,
    pub compatibility: Compatibility,
    pub additional_params: BTreeMap<String, serde_json::Value>,
    pub input_token_count: InputTokenCountConfig,
    pub beta_headers: Vec<String>,
}
impl AnthropicEndpoint {
    pub(crate) fn validate(&self, provider: &str) -> anyhow::Result<()> {
        let url = reqwest::Url::parse(&self.base_url)
            .map_err(|e| anyhow::anyhow!("providers.{provider}.base_url: {e}"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "providers.{provider}.base_url must use http or https"
        );
        self.api_key.validate(provider)?;
        parse_session_id_header(provider, self.session_id_header.as_deref())?;
        self.network.validate(provider)?;
        self.input_token_count.validate(provider)?;
        validate_additional_params(provider, &self.additional_params)?;
        for beta in &self.beta_headers {
            anyhow::ensure!(
                !beta.is_empty() && HeaderName::from_bytes(beta.as_bytes()).is_ok(),
                "providers.{provider}.beta_headers must contain valid nonempty HTTP tokens"
            );
        }
        Ok(())
    }
    pub fn redacted_base_url(&self) -> String {
        crate::connection::redacted_url_for_logging(&self.base_url)
    }
}
impl fmt::Debug for AnthropicEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicEndpoint")
            .field("base_url", &self.redacted_base_url())
            .field("api_key", &self.api_key)
            .field("auth", &self.auth)
            .field("network", &self.network)
            .field("session_id_header", &self.session_id_header)
            .field("compatibility", &self.compatibility)
            .field("additional_params", &self.additional_params)
            .field("input_token_count", &self.input_token_count)
            .field("beta_headers", &self.beta_headers)
            .finish()
    }
}
pub(crate) fn validate_additional_params(
    provider: &str,
    params: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    for key in params.keys() {
        anyhow::ensure!(
            !matches!(
                key.as_str(),
                "model"
                    | "messages"
                    | "system"
                    | "tools"
                    | "tool_choice"
                    | "stream"
                    | "max_tokens"
                    | "thinking"
                    | "output_config"
                    | "cache_control"
                    | "fallbacks"
                    | "temperature"
                    | "top_p"
                    | "top_k"
                    | "context_management"
            ),
            "providers.{provider}.additional_params.{key} is reserved by Zevria"
        );
    }
    Ok(())
}
impl AnthropicProviderConfig {
    fn endpoint(&self) -> AnthropicEndpoint {
        AnthropicEndpoint {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            auth: self.auth,
            network: self.network.clone(),
            session_id_header: self.session_id_header.clone(),
            compatibility: self.compatibility.clone(),
            additional_params: self.additional_params.clone(),
            input_token_count: self.input_token_count.clone(),
            beta_headers: self.beta_headers.clone(),
        }
    }
    pub(super) fn validate(&self, provider: &str, percent: u64) -> anyhow::Result<()> {
        self.endpoint().validate(provider)?;
        anyhow::ensure!(
            !self.models.is_empty(),
            "providers.{provider}.models must define at least one model"
        );
        for (key, model) in &self.models {
            validate_nonblank(key, &format!("providers.{provider}.models key"))?;
            model.validate(provider, key, percent)?;
        }
        Ok(())
    }
    pub(super) fn resolved_profiles(&self, provider: &str) -> Vec<ResolvedModelProfile> {
        self.models
            .iter()
            .map(|(key, model)| ResolvedModelProfile {
                profile: ModelProfileRef::new(provider, key),
                protocol: ProfileProtocol::Anthropic {
                    endpoint: self.endpoint(),
                    model: model.clone(),
                },
                context_window_tokens: model.context_window_tokens,
                input_token_limit: model.effective_input_limit(),
                retained_user_tokens: model.retained_user_tokens,
                reasoning_levels: model.reasoning_levels.clone(),
            })
            .collect()
    }
}
