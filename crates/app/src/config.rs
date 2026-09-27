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
use zevria_instructions::skill::SkillsConfig;
use zevria_model::CompactionPolicy;
use zevria_provider::{ModeAssignments, ModelRouting, ProviderConfig};

/// The config written on first run. It is kept as a literal so setup comments
/// remain useful, and tests pin its intentionally incomplete, non-secret shape.
pub(crate) const DEFAULT_CONFIG: &str = r#"# zevria configuration.
#
# Provider catalogs and supported reasoning_levels live in sibling models.jsonc.
# All five mode assignments belong here, with an explicit supported reasoning_level.
# ZEVRIA_CONFIG selects this file and its sibling models.jsonc.

[modes]
plan = { provider = "openai", model = "gpt-6-astra", reasoning_level = "max" }
build = { provider = "openai", model = "gpt-6-astra", reasoning_level = "max" }
review = { provider = "openai", model = "gpt-6-sol", reasoning_level = "max" }
explore = { provider = "openai", model = "gpt-6-luna", reasoning_level = "max" }
builder = { provider = "openai", model = "gpt-6-luna", reasoning_level = "max" }

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

[command]
timeout_seconds = 300
capture_bytes = 1048576

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
  //     // Exact full Responses endpoint; HTTP/WebSocket pairing changes only scheme.
  //     "base_url": "https://api.openai.com/v1/responses",
  //     // Literal credential only; no environment interpolation.
  //     "api_key": "replace-with-api-key",
  //     "supports_websockets": true,
  //     // Optional provider-specific routing header (OpenCode Go example):
  //     // "session_id_header": "x-opencode-session",
  //     "compatibility": {
  //       "send_reasoning": true, "send_reasoning_encrypted_content": true,
  //       "strict_tools": true, "send_prompt_cache_key": true, "send_store": true
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
  //     // Optional provider-hosted search; disabled unless explicitly enabled.
  //     "web_search": {
  //       "enabled": true, "external_web_access": true,
  //       "search_context_size": "medium", // low | medium | high
  //       "return_token_budget": "default" // default | unlimited (GPT-5+ reasoning)
  //       // , "filters": { "allowed_domains": ["openai.com"], "blocked_domains": ["example.org"] }
  //       // At most 100 domains per list, no schemes/paths; both lists may coexist.
  //       // , "user_location": { "country": "GB", "city": "London", "region": "London", "timezone": "Europe/London" }
  //       // Location is never discovered automatically.
  //     },
  //     // Optional gateway-specific top-level Responses fields:
  //     // "additional_params": { "gateway_routing": "preferred-pool" },
  //     "models": {
  //       // Exact model ID sent on the wire.
  //       "gpt-6-luna": {
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       },
  //       "gpt-6-sol": {
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       },
  //       "gpt-6-astra": {
  //         "context_window_tokens": 272000,
  //         "input_token_limit": 272000, // omit to use the physical context window
  //         "retained_user_tokens": 20000,
  //         "reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
  //         "reasoning_summary_level": "detailed"
  //       }
  //     }
  //   }
  // }
}
"#;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    modes: ModeAssignments,
    #[serde(default)]
    session: SessionConfig,
    #[serde(default)]
    skills: SkillsConfig,
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
    pub(crate) session: SessionConfig,
    pub(crate) skills: SkillsConfig,
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
        let source = crate::skills::load_skill_config(&path)?;
        let contents = source
            .contents
            .context("configuration disappeared while loading")?;
        let file: ConfigFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse configuration at {}", path.display()))?;
        anyhow::ensure!(
            file.skills == source.settings,
            "full and skill-only configuration loaders disagree"
        );
        let models = std::fs::read_to_string(&models_path).with_context(|| {
            format!(
                "failed to read models configuration at {}",
                models_path.display()
            )
        })?;
        let mut config = Self::from_files(file, parse_models(&models, &models_path)?)?;
        config.source_path = Some(path);
        config.models_path = Some(models_path);
        Ok(config)
    }

    fn from_files(mut file: ConfigFile, models: ModelsFile) -> anyhow::Result<Self> {
        file.ensemble.resolve_runtime_commands()?;
        file.session.validate()?;
        file.skills.validate()?;
        file.acp.validate()?;
        file.ensemble.validate()?;
        file.command.validate()?;
        let routing = ModelRouting::resolve(
            &models.providers,
            &file.modes,
            file.session.compaction.auto_trigger_percent,
        )?;
        let compaction_policy =
            CompactionPolicy::new(file.session.compaction.clone(), routing.context_policies())?;
        Ok(Self {
            source_path: None,
            models_path: None,
            providers: models.providers,
            modes: file.modes,
            session: file.session,
            skills: file.skills,
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
        crate::skills::parse_skill_config(contents)?;
        Self::from_files(
            toml::from_str(contents)?,
            parse_models(models, models_path)?,
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
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

fn parse_models(text: &str, path: &Path) -> anyhow::Result<ModelsFile> {
    (|| -> anyhow::Result<ModelsFile> {
        // Reject ambiguous keys before serde's map conversion can discard one.
        // CST writes and reads must always refer to the same unique property.
        let parsed = jsonc_parser::parse_to_ast(text, &Default::default(), &MODELS_PARSE_OPTIONS)?;
        let mut pending = parsed.value.as_ref().into_iter().collect::<Vec<_>>();
        while let Some(value) = pending.pop() {
            match value {
                jsonc_parser::ast::Value::Object(object) => {
                    let mut keys = std::collections::BTreeSet::new();
                    for property in &object.properties {
                        let key = property.name.as_str();
                        anyhow::ensure!(
                            keys.insert(key),
                            "duplicate property {key:?} in models configuration"
                        );
                        pending.push(&property.value);
                    }
                }
                jsonc_parser::ast::Value::Array(array) => pending.extend(&array.elements),
                _ => {}
            }
        }
        let value: serde_json::Value =
            jsonc_parser::parse_to_serde_value(text, &MODELS_PARSE_OPTIONS)?;
        Ok(serde_json::from_value(value)?)
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
    pub timeout_seconds: u64,
    pub capture_bytes: usize,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 300,
            capture_bytes: 1024 * 1024,
        }
    }
}

impl CommandConfig {
    fn validate(self) -> anyhow::Result<()> {
        if self.timeout_seconds == 0 {
            anyhow::bail!("command.timeout_seconds must be greater than zero");
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
