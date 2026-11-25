//! Application configuration composed from the selected compile-time adapters.

use std::collections::BTreeMap;

use crate::session_config::SessionConfig;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zevria_acp::AcpConfig;
use zevria_ensemble::config::EnsembleConfig;
use zevria_foundation::config::LogConfig;
use zevria_foundation::config::{create_default_file, models_path_for};
use zevria_foundation::{ModelProfileRef, ReasoningLevel};
use zevria_instructions::skill::SkillsConfig;
use zevria_model::CompactionPolicy;
use zevria_provider::{
    ModeAssignments, ModelRouting, ProviderConfig, ResolvedWebSearchConfig, SearchContextSize,
    SearchReturnTokenBudget, WebSearchConfig, WebSearchFilters, WebSearchLocation,
};

/// The config written on first run. It is kept as a literal so setup comments
/// remain useful, and tests pin its intentionally incomplete, non-secret shape.
pub(crate) const DEFAULT_CONFIG: &str = r#"# zevria configuration.
#
# Provider catalogs and supported reasoning_levels live in sibling models.jsonc.
# All five mode assignments belong here, with an explicit supported reasoning_level.
# ZEVRIA_CONFIG selects this file and its sibling models.jsonc.

[modes]
plan = { provider = "openai", model = "gpt-6.2-astra", reasoning_level = "max" }
build = { provider = "openai", model = "gpt-6.2-astra", reasoning_level = "max" }
review = { provider = "openai", model = "gpt-6.2-sol", reasoning_level = "max" }
explore = { provider = "openai", model = "gpt-6-luna", reasoning_level = "max" }
builder = { provider = "openai", model = "gpt-6-luna", reasoning_level = "max" }
# Native Claude alternative (configure the anthropic catalog in models.jsonc):
# build = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }
# plan = { provider = "anthropic", model = "claude-opus-5-5", reasoning_level = "high" }

# Optional external search, independent of the model assigned to each mode.
# [web_search]
# enabled = true
# provider = "openai"
# model = "gpt-6-luna"
# reasoning_level = "low"
# external_web_access = true
# search_context_size = "medium"   # standard depth: low | medium | high
# return_token_budget = "default"  # default | unlimited
# max_tool_calls = 8
# [web_search.filters]
# allowed_domains = ["openai.com"]
# blocked_domains = ["example.org"]
# [web_search.user_location]
# country = "GB"
# city = "London"
# region = "London"
# timezone = "Europe/London"

[session]
# Override only product identity and engineering practice (docs/instructions/system-prompt.md).
# Engine protocol and capability modules, including command conventions, stay fixed.
# preamble = "You are a custom agent"
# An empty preamble clears only Application guidance. AGENTS.md is captured
# separately at opening/resume; it never changes this value (docs/guidance.md).
max_concurrent_subtasks = 10
event_queue_capacity = 256
[session.plan]
max_artifact_bytes = 131072
allow_subtasks = true
allow_skills = true
[session.compaction]
auto_trigger_percent = 90
# summary_prompt = "Custom checkpoint instructions"

# Optional named theme, generated with `zevria theme generate`.
# Omit [theme] to keep the built-in Zevria Dark palette.
# [theme]
# name = "ocean"

[command]
# Unix: capture login exports once; "login" restores a login shell for every call.
shell_environment = "captured"
timeout_seconds = 300
# Optional command_status wait cap in seconds; omit to use timeout_seconds.
# status_wait_timeout_seconds = 240
capture_bytes = 1048576
# Running background job cap. Set 0 for the foreground-only command surface.
max_background_jobs = 8
# Maximum automatic job-completion turns between user requests; 0 disables.
max_automatic_continuations = 3
# Optional total wall-clock limit, at least timeout_seconds. Omit for no limit.
# background_timeout_seconds = 3600

# Optional: connect to a trusted MCP server (see docs/mcp.md).
# [mcp.servers.example]
# command = "my-mcp-server"
# plan_mode = "read_only"

[acp]
max_sessions = 4
expose_session_list = true

[ensemble]
plan_agents = ["codex", "claude", "zevria"]
review_agents = ["codex", "claude", "zevria"]
max_concurrent_agents = 4
review_startup_timeout_seconds = 120
review_turn_timeout_seconds = 18000
cancel_grace_seconds = 5
max_synthesis_bytes_per_agent = 131072

[ensemble.agents.codex]
label = "Codex"
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@latest"]
plan_mode = "read-only"
review_mode = "agent"
plan_config_options = { collaboration_mode = "plan" }
env = { CODEX_PATH = "codex", NO_BROWSER = "1" }
login_hint = "Authenticate Codex in a terminal before running an ensemble."

[ensemble.agents.claude]
label = "Claude Code"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@latest"]
plan_mode = "plan"
review_mode = "default"
plan_handoff_transport = "claude_code_exit_plan_mode"
review_system_prompt_transport = "claude_code_append"
env = {}
login_hint = "Authenticate Claude Code in a terminal before running an ensemble."

[ensemble.agents.zevria]
label = "Zevria"
command = "zevria"
args = ["--acp", "--ensemble-worker"]
plan_mode = "plan"
review_mode = "review"
env = {}
login_hint = "Configure Zevria's providers in models.jsonc and all five mode assignments with reasoning_level in config.toml before running an ensemble."

[log]
# Log level filter: trace, debug, info, warn, error, or off.
# The RUST_LOG environment variable takes precedence when set.
level = "info"
# Uncomment to write logs somewhere other than ~/.zevria/logs.
# directory = "/tmp/zevria-logs"
"#;

/// Intentionally incomplete: uncomment and configure all required identities.
pub(crate) const DEFAULT_MODELS: &str = r#"// Zevria model configuration. JSON with comments and trailing commas.
// Provider and model keys are stable, case-sensitive replay identities.
// Configure at least one provider and its model capabilities here.
// Configure all five mode assignments and their reasoning_level in config.toml.
{
  // "providers": {
  //   "openai": {
  //     "protocol": "openai.responses", // optional; this is the default
  //     // Exact full Responses endpoint; HTTP/WebSocket pairing changes only scheme.
  //     "base_url": "https://api.openai.com/v1/responses",
  //     // Literal string (no interpolation), or an explicit non-interactive helper:
  //     "api_key": "replace-with-api-key",
  //     // "api_key": { "command": "auth", "args": ["--key"],
  //     //   "timeout_seconds": 30, "refresh_interval_seconds": 300 },
  //     "supports_websockets": true,
  //     // Optional early authenticated handshake while idle (off by default):
  //     // "network": { "prewarm": false, "websocket_keepalive_seconds": 30 },
  //     // Optional provider-specific routing header (OpenCode Go example):
  //     // "session_id_header": "x-opencode-session",
  //     "compatibility": {
  //       "send_reasoning": true, "send_reasoning_encrypted_content": true,
  //       "strict_tools": true, "send_prompt_cache_key": true, "send_store": true,
  //       "parallel_tool_calls": true
  //     },
  //     "compaction": {
  //       // "url": "https://api.openai.com/v1/responses/compact",
  //       "request_timeout_seconds": 300
  //     },
  //     "input_token_count": {
  //       "enabled": true, "derive_url": true,
  //       // "url": "https://api.openai.com/v1/responses/input_tokens",
  //       "request_timeout_seconds": 30
  //     },
  //     // Optional gateway-specific top-level Responses fields:
  //     // "additional_params": { "gateway_routing": "preferred-pool" },
  //     "models": {
  //       // Exact model ID sent on the wire.
  //       "gpt-6-luna": {
  //         // "service_tier": "fast", // generation only; omit to inherit, null to suppress
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       },
  //       "gpt-6.2-sol": {
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       },
  //       "gpt-6.2-astra": {
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       }
  //     }
  //   },
  //   "anthropic": {
  //     "protocol": "anthropic.messages",
  //     "base_url": "https://api.anthropic.com/v1/messages",
  //     "api_key": "replace-with-api-key",
  //     // "auth": "bearer", // optional gateway/OAuth alternative to x-api-key
  //     // "compatibility": { "strict_thinking_binding": true },
  //     "models": {
  //       "claude-opus-5-5": {
  //         "context_window_tokens": 1000000,
  //         "max_output_tokens": 64000,
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "thinking_display": "summarized",
  //         "refusal_fallback": "default"
  //       }
  //     }
  //   }
  // }
}
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    modes: ModeAssignments,
    #[serde(default)]
    web_search: WebSearchSettings,
    #[serde(default)]
    session: SessionConfig,
    #[serde(default)]
    skills: SkillsConfig,
    #[serde(default, deserialize_with = "crate::mcp::deserialize_config")]
    mcp: zevria_mcp::McpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<ThemeSelector>,
    #[serde(default)]
    acp: AcpConfig,
    #[serde(default)]
    ensemble: EnsembleConfig,
    #[serde(default)]
    command: CommandConfig,
    #[serde(default)]
    log: LogConfig,
}

/// Flat TOML settings. Keep fields explicit so serde can reject unknown keys
/// at this boundary (deny_unknown_fields is not supported with flatten).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct WebSearchSettings {
    enabled: bool,
    provider: Option<String>,
    model: Option<String>,
    reasoning_level: Option<ReasoningLevel>,
    external_web_access: Option<bool>,
    search_context_size: Option<SearchContextSize>,
    return_token_budget: Option<SearchReturnTokenBudget>,
    max_tool_calls: Option<u32>,
    filters: Option<WebSearchFilters>,
    user_location: Option<WebSearchLocation>,
}

impl WebSearchSettings {
    fn resolve(self, routing: &ModelRouting) -> anyhow::Result<Option<ResolvedWebSearchConfig>> {
        let defaults = WebSearchConfig::default();
        let options = WebSearchConfig {
            external_web_access: self
                .external_web_access
                .unwrap_or(defaults.external_web_access),
            search_context_size: self
                .search_context_size
                .unwrap_or(defaults.search_context_size),
            return_token_budget: self
                .return_token_budget
                .unwrap_or(defaults.return_token_budget),
            max_tool_calls: self.max_tool_calls,
            filters: self.filters,
            user_location: self.user_location,
        };
        if !self.enabled {
            options.validate()?;
            return Ok(None);
        }
        let required = |value: Option<String>, field| {
            value
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!("web_search.{field} is required when web search is enabled")
                })
        };
        let profile = ModelProfileRef::new(
            required(self.provider, "provider")?,
            required(self.model, "model")?,
        );
        let reasoning_level = self
            .reasoning_level
            .context("web_search.reasoning_level is required when web search is enabled")?;
        let target = routing
            .catalog()
            .find(|target| target.profile == profile)
            .cloned()
            .with_context(|| {
                format!("web_search target {profile} is not defined in models.jsonc")
            })?;
        Ok(Some(ResolvedWebSearchConfig::new(
            target,
            reasoning_level,
            options,
        )?))
    }
}

/// Name-only selection. Parsing never opens files or touches renderer state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSelector {
    #[serde(deserialize_with = "deserialize_theme_name")]
    pub name: String,
}

fn deserialize_theme_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let name = String::deserialize(deserializer)?;
    crate::theme_store::validate_name(&name).map_err(serde::de::Error::custom)?;
    Ok(name)
}

#[derive(Debug)]
pub struct Config {
    /// Captured once at startup, including ZEVRIA_CONFIG. Test-only in-memory
    /// configurations have no persistence service.
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) models_path: Option<PathBuf>,
    pub(crate) providers: BTreeMap<String, ProviderConfig>,
    pub(crate) modes: ModeAssignments,
    pub(crate) web_search: Option<ResolvedWebSearchConfig>,
    pub(crate) session: SessionConfig,
    pub(crate) skills: SkillsConfig,
    pub(crate) mcp: zevria_mcp::McpConfig,
    pub(crate) mcp_revision: String,
    pub(crate) theme: Option<ThemeSelector>,
    pub(crate) acp: AcpConfig,
    pub(crate) ensemble: EnsembleConfig,
    pub(crate) command: CommandConfig,
    pub(crate) log: LogConfig,
    routing: ModelRouting,
    compaction_policy: CompactionPolicy,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = zevria_foundation::config::config_path()?;
        let models_path = models_path_for(&path);
        let mut created = Vec::new();
        for (path, contents) in [(&path, DEFAULT_CONFIG), (&models_path, DEFAULT_MODELS)] {
            if !path.try_exists()? {
                create_default_file(path, contents)?;
                created.push(path.display().to_string());
            }
        }
        anyhow::ensure!(
            created.is_empty(),
            "created the Zevria configuration skeleton at {}. Configure providers and model capabilities in models.jsonc for the mode assignments in config.toml, then start Zevria again",
            created.join(" and ")
        );
        Self::load_existing(&path, &models_path)
    }

    pub(crate) fn load_existing(path: &Path, models_path: &Path) -> anyhow::Result<Self> {
        let contents = crate::settings::read_regular(path)
            .with_context(|| format!("failed to read configuration at {}", path.display()))?;
        let models = crate::settings::read_regular(models_path).with_context(|| {
            format!(
                "failed to read models configuration at {}",
                models_path.display()
            )
        })?;
        let mut config = Self::parse_at(&contents, &models, models_path)
            .with_context(|| format!("failed to load configuration at {}", path.display()))?;
        config
            .mcp
            .bind_config_directory(path.parent().unwrap_or(Path::new(".")));
        config.mcp_revision = crate::mcp::config_revision(&contents);
        config.source_path = Some(path.to_path_buf());
        config.models_path = Some(models_path.to_path_buf());
        Ok(config)
    }

    fn from_files(mut file: ConfigFile, models: ModelsFile) -> anyhow::Result<Self> {
        file.ensemble.resolve_runtime_commands()?;
        file.session.validate()?;
        file.skills.validate()?;
        file.mcp.validate()?;
        file.acp.validate()?;
        file.ensemble.validate()?;
        file.command.validate()?;
        let routing = ModelRouting::resolve(
            &models.providers,
            &file.modes,
            file.session.compaction.auto_trigger_percent,
        )?;
        let web_search = file.web_search.resolve(&routing)?;
        let compaction_policy =
            CompactionPolicy::new(file.session.compaction.clone(), routing.context_policies())?;
        Ok(Self {
            source_path: None,
            models_path: None,
            providers: models.providers,
            modes: file.modes,
            web_search,
            session: file.session,
            skills: file.skills,
            mcp: file.mcp,
            mcp_revision: "memory".into(),
            theme: file.theme,
            acp: file.acp,
            ensemble: file.ensemble,
            command: file.command,
            log: file.log,
            routing,
            compaction_policy,
        })
    }

    /// Validated startup values are read-only so the derived routing and
    /// compaction policy cannot diverge from their source configuration.
    pub fn providers(&self) -> &BTreeMap<String, ProviderConfig> {
        &self.providers
    }

    pub fn modes(&self) -> &ModeAssignments {
        &self.modes
    }

    pub fn log(&self) -> &LogConfig {
        &self.log
    }

    pub fn acp(&self) -> AcpConfig {
        self.acp
    }

    pub fn theme(&self) -> Option<&ThemeSelector> {
        self.theme.as_ref()
    }

    pub fn routing(&self) -> &ModelRouting {
        &self.routing
    }

    pub fn compaction_policy(&self) -> &CompactionPolicy {
        &self.compaction_policy
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn parse(contents: &str, models: &str) -> anyhow::Result<Self> {
        Self::parse_at(contents, models, Path::new("models.jsonc"))
    }

    pub(crate) fn parse_at(
        contents: &str,
        models: &str,
        models_path: &Path,
    ) -> anyhow::Result<Self> {
        let mut config = Self::from_files(
            partial_document(contents)?.try_into()?,
            parse_models(models, models_path)?,
        )?;
        config
            .mcp
            .bind_config_directory(models_path.parent().unwrap_or(Path::new(".")));
        Ok(config)
    }
}

/// Read only the shared configuration envelope, permitting partial files for offline commands.
pub(crate) fn partial_document(contents: &str) -> anyhow::Result<toml::Table> {
    // Parse unrelated settings as data only, not provider/credential models.
    // Match the full application's top-level schema, while allowing those
    // unrelated tables to remain intentionally incomplete for offline use.
    let document: toml::Table = toml::from_str(contents).map_err(|error: toml::de::Error| {
        let offset = error.span().map_or(0, |span| span.start);
        anyhow::anyhow!("invalid config.toml syntax near byte {offset}")
    })?;
    for key in document.keys() {
        anyhow::ensure!(
            key != "providers",
            "configuration key {key:?} moved to models.jsonc next to this file"
        );
        anyhow::ensure!(
            matches!(
                key.as_str(),
                "session"
                    | "skills"
                    | "mcp"
                    | "theme"
                    | "acp"
                    | "ensemble"
                    | "command"
                    | "log"
                    | "modes"
                    | "web_search"
            ),
            "unsupported configuration key {key:?}"
        );
    }
    Ok(document)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
    #[serde(deserialize_with = "zevria_provider::deserialize_provider_catalog")]
    providers: BTreeMap<String, ProviderConfig>,
}

pub(crate) const MODELS_PARSE_OPTIONS: jsonc_parser::ParseOptions = jsonc_parser::ParseOptions {
    allow_comments: true,
    allow_trailing_commas: true,
    allow_loose_object_property_names: false,
    allow_missing_commas: false,
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
};

fn jsonc_value(value: jsonc_parser::ast::Value<'_>) -> anyhow::Result<serde_json::Value> {
    use jsonc_parser::ast::Value as Ast;
    use serde_json::Value;
    Ok(match value {
        Ast::StringLit(lit) => Value::String(lit.value.into_owned()),
        Ast::NumberLit(lit) => Value::Number(lit.value.parse()?),
        Ast::BooleanLit(lit) => Value::Bool(lit.value),
        Ast::NullKeyword(_) => Value::Null,
        Ast::Array(array) => Value::Array(
            array
                .elements
                .into_iter()
                .map(jsonc_value)
                .collect::<anyhow::Result<_>>()?,
        ),
        Ast::Object(object) => {
            let mut map = serde_json::Map::new();
            for property in object.properties {
                let key = property.name.as_str();
                anyhow::ensure!(
                    !map.contains_key(key),
                    "duplicate property {key:?} in models configuration"
                );
                map.insert(key.to_owned(), jsonc_value(property.value)?);
            }
            Value::Object(map)
        }
    })
}

fn parse_models(text: &str, path: &Path) -> anyhow::Result<ModelsFile> {
    (|| -> anyhow::Result<ModelsFile> {
        // Reject ambiguous keys before serde's map conversion can discard one.
        // CST writes and reads must always refer to the same unique property.
        let parsed = jsonc_parser::parse_to_ast(text, &Default::default(), &MODELS_PARSE_OPTIONS)?;
        let value = jsonc_value(parsed.value.context("empty models configuration")?)?;
        let mut models: ModelsFile = serde_json::from_value(value)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = std::path::absolute(parent)?;
        for provider in models.providers.values_mut() {
            provider.set_config_directory(&directory);
        }
        Ok(models)
    })()
    .with_context(|| format!("failed to parse models configuration at {}", path.display()))
}

#[cfg(test)]
pub(crate) fn test_config() -> Config {
    crate::test_support::parse_fixture(
        r#"
[providers.test]
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "test-key"
supports_websockets = false
[providers.test.models."test-model"]
context_window_tokens = 272000
retained_user_tokens = 20000
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[modes]
build = { provider = "test", model = "test-model", reasoning_level = "medium" }
plan = { provider = "test", model = "test-model", reasoning_level = "medium" }
review = { provider = "test", model = "test-model", reasoning_level = "medium" }
explore = { provider = "test", model = "test-model", reasoning_level = "medium" }
builder = { provider = "test", model = "test-model", reasoning_level = "medium" }
"#,
    )
    .expect("the built-in test configuration must be valid")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandConfig {
    pub shell_environment: zevria_foundation::shell::ShellEnvironment,
    pub timeout_seconds: u64,
    pub status_wait_timeout_seconds: Option<u64>,
    pub capture_bytes: usize,
    pub max_background_jobs: usize,
    pub max_automatic_continuations: u32,
    pub background_timeout_seconds: Option<u64>,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            shell_environment: zevria_foundation::shell::ShellEnvironment::Captured,
            timeout_seconds: 300,
            status_wait_timeout_seconds: None,
            capture_bytes: 1024 * 1024,
            max_background_jobs: 8,
            max_automatic_continuations: 3,
            background_timeout_seconds: None,
        }
    }
}

impl CommandConfig {
    fn validate(self) -> anyhow::Result<()> {
        if self.timeout_seconds == 0 {
            anyhow::bail!("command.timeout_seconds must be greater than zero");
        }
        if self.status_wait_timeout_seconds == Some(0) {
            anyhow::bail!("command.status_wait_timeout_seconds must be greater than zero");
        }
        if self
            .background_timeout_seconds
            .is_some_and(|seconds| seconds < self.timeout_seconds)
        {
            anyhow::bail!(
                "command.background_timeout_seconds must be at least command.timeout_seconds"
            );
        }
        if self.capture_bytes < 2 {
            anyhow::bail!("command.capture_bytes must be at least 2");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod provider_config_tests;
