//! Ensemble worker launch configuration, defaults, and validation.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};
use zevria_workflow::EnsembleWorkflow;

const CODEX_ACP_PACKAGE: &str = "@agentclientprotocol/codex-acp@latest";
const CODEX_PATH_ENV: &str = "CODEX_PATH";
const CODEX_PATH_COMMAND: &str = "codex";

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnsembleAgentConfig {
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub plan_mode: Option<String>,
    pub review_mode: Option<String>,
    pub plan_config_options: BTreeMap<String, String>,
    pub review_config_options: BTreeMap<String, String>,
    pub plan_handoff_transport: Option<PlanHandoffTransport>,
    pub review_system_prompt_transport: Option<ReviewSystemPromptTransport>,
    pub env: BTreeMap<String, String>,
    pub login_hint: String,
}

impl EnsembleAgentConfig {
    fn built_in_zevria() -> Self {
        Self {
            label: "Zevria".to_string(),
            command: "zevria".to_string(),
            args: vec!["--acp".to_string(), "--ensemble-worker".to_string()],
            plan_mode: Some("plan".to_string()),
            review_mode: Some("review".to_string()),
            login_hint:
                "Configure Zevria's providers in models.jsonc and all five mode assignments with reasoning_level in config.toml before running an ensemble."
                    .to_string(),
            ..Self::default()
        }
    }

    fn is_builtin_zevria_launcher(&self) -> bool {
        self.command == "zevria" && self.args == ["--acp", "--ensemble-worker"]
    }

    pub fn mode_for(&self, workflow: EnsembleWorkflow) -> Option<&str> {
        match workflow {
            EnsembleWorkflow::Plan => self.plan_mode.as_deref(),
            EnsembleWorkflow::Review => self.review_mode.as_deref(),
        }
    }

    pub fn config_options_for(&self, workflow: EnsembleWorkflow) -> &BTreeMap<String, String> {
        match workflow {
            EnsembleWorkflow::Plan => &self.plan_config_options,
            EnsembleWorkflow::Review => &self.review_config_options,
        }
    }
}

impl fmt::Debug for EnsembleAgentConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnsembleAgentConfig")
            .field("label", &self.label)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("plan_mode", &self.plan_mode)
            .field("review_mode", &self.review_mode)
            .field(
                "plan_config_option_ids",
                &self.plan_config_options.keys().collect::<Vec<_>>(),
            )
            .field(
                "review_config_option_ids",
                &self.review_config_options.keys().collect::<Vec<_>>(),
            )
            .field("plan_handoff_transport", &self.plan_handoff_transport)
            .field(
                "review_system_prompt_transport",
                &self.review_system_prompt_transport,
            )
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("login_hint", &self.login_hint)
            .finish()
    }
}

/// Optional provider extension used to capture a native planning lifecycle
/// without allowing the provider to transition into implementation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanHandoffTransport {
    ClaudeCodeExitPlanMode,
}

/// Optional provider extension used to reinforce the ordinary review prompt.
/// The ordinary prompt always remains authoritative fallback evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSystemPromptTransport {
    ClaudeCodeAppend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnsembleConfig {
    pub plan_agents: Vec<String>,
    pub review_agents: Vec<String>,
    pub max_concurrent_agents: usize,
    /// Review-only operation deadlines. Plan work and user review have no host deadline.
    pub review_startup_timeout_seconds: u64,
    pub review_turn_timeout_seconds: u64,
    pub cancel_grace_seconds: u64,
    pub max_synthesis_bytes_per_agent: usize,
    pub agents: BTreeMap<String, EnsembleAgentConfig>,
}

impl Default for EnsembleConfig {
    fn default() -> Self {
        let codex = EnsembleAgentConfig {
            label: "Codex".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), CODEX_ACP_PACKAGE.to_string()],
            plan_mode: Some("read-only".to_string()),
            review_mode: Some("agent".to_string()),
            plan_config_options: BTreeMap::from([(
                "collaboration_mode".to_string(),
                "plan".to_string(),
            )]),
            review_config_options: BTreeMap::new(),
            plan_handoff_transport: None,
            review_system_prompt_transport: None,
            env: BTreeMap::from([
                (CODEX_PATH_ENV.to_string(), CODEX_PATH_COMMAND.to_string()),
                ("NO_BROWSER".to_string(), "1".to_string()),
            ]),
            login_hint: "Authenticate Codex in a terminal before running an ensemble.".to_string(),
        };
        let claude = EnsembleAgentConfig {
            label: "Claude Code".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp@latest".to_string(),
            ],
            plan_mode: Some("plan".to_string()),
            review_mode: Some("default".to_string()),
            plan_config_options: BTreeMap::new(),
            review_config_options: BTreeMap::new(),
            plan_handoff_transport: Some(PlanHandoffTransport::ClaudeCodeExitPlanMode),
            review_system_prompt_transport: Some(ReviewSystemPromptTransport::ClaudeCodeAppend),
            env: BTreeMap::new(),
            login_hint: "Authenticate Claude Code in a terminal before running an ensemble."
                .to_string(),
        };
        Self {
            plan_agents: vec![
                "codex".to_string(),
                "claude".to_string(),
                "zevria".to_string(),
            ],
            review_agents: vec![
                "codex".to_string(),
                "claude".to_string(),
                "zevria".to_string(),
            ],
            max_concurrent_agents: 4,
            review_startup_timeout_seconds: 120,
            review_turn_timeout_seconds: 18000,
            cancel_grace_seconds: 5,
            max_synthesis_bytes_per_agent: 128 * 1024,
            agents: BTreeMap::from([
                ("claude".to_string(), claude),
                ("codex".to_string(), codex),
                ("zevria".to_string(), EnsembleAgentConfig::built_in_zevria()),
            ]),
        }
    }
}

impl EnsembleConfig {
    fn built_in_codex_acp_mut(&mut self) -> Option<&mut EnsembleAgentConfig> {
        self.agents
            .get_mut("codex")
            .filter(|codex| codex.command == "npx" && codex.args == ["-y", CODEX_ACP_PACKAGE])
    }

    pub fn resolve_runtime_commands(&mut self) -> anyhow::Result<()> {
        if let Ok(path) = which::which(CODEX_PATH_COMMAND)
            && let Some(path) = path.to_str()
        {
            self.apply_resolved_codex_path(path);
        }
        self.resolve_zevria_executable(std::env::current_exe)
    }

    fn resolve_zevria_executable(
        &mut self,
        resolve: impl FnOnce() -> std::io::Result<std::path::PathBuf>,
    ) -> anyhow::Result<()> {
        let selected: HashSet<_> = self
            .plan_agents
            .iter()
            .chain(&self.review_agents)
            .cloned()
            .collect();
        if !self
            .agents
            .iter()
            .any(|(name, agent)| selected.contains(name) && agent.is_builtin_zevria_launcher())
        {
            return Ok(());
        }
        // Resolve by the distinctive launcher, not its configurable map key.
        // Never fall back to a possibly different installation on PATH.
        let path = resolve().map_err(|error| anyhow::anyhow!(
            "cannot resolve the running Zevria executable for ensemble workers: {error}. Configure an explicit ensemble agent executable path or restore this installation."
        ))?;
        let path = path.to_str().filter(|path| !path.is_empty()).ok_or_else(|| anyhow::anyhow!(
            "the running Zevria executable cannot be represented as a UTF-8 ensemble command. Configure an explicit UTF-8 executable path."
        ))?;
        for (name, agent) in &mut self.agents {
            if selected.contains(name) && agent.is_builtin_zevria_launcher() {
                agent.command = path.to_string();
                #[cfg(windows)]
                agent
                    .args
                    .extend(["--runtime".to_string(), "native".to_string()]);
            }
        }
        Ok(())
    }

    fn apply_resolved_codex_path(&mut self, path: &str) {
        let Some(codex) = self.built_in_codex_acp_mut() else {
            return;
        };
        // npx prepends its temporary node_modules/.bin directory to PATH. A
        // bare `codex` would therefore select codex-acp's transitive wrapper
        // again, so freeze the path resolved in Zevria's original environment.
        if codex
            .env
            .get(CODEX_PATH_ENV)
            .is_some_and(|configured| configured == CODEX_PATH_COMMAND)
        {
            codex
                .env
                .insert(CODEX_PATH_ENV.to_string(), path.to_string());
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, agent) in &self.agents {
            if name.trim().is_empty() {
                anyhow::bail!("ensemble.agents contains an empty agent name");
            }
            if agent.label.trim().is_empty() {
                anyhow::bail!("ensemble.agents.{name}.label must not be empty");
            }
            if agent.command.trim().is_empty() {
                anyhow::bail!("ensemble.agents.{name}.command must not be empty");
            }
            for (field, mode) in [
                ("plan_mode", agent.plan_mode.as_deref()),
                ("review_mode", agent.review_mode.as_deref()),
            ] {
                if mode.is_some_and(|mode| mode.trim().is_empty()) {
                    anyhow::bail!("ensemble.agents.{name}.{field} must not be empty");
                }
            }
            for (field, options) in [
                ("plan_config_options", &agent.plan_config_options),
                ("review_config_options", &agent.review_config_options),
            ] {
                for (id, value) in options {
                    if id.trim().is_empty() {
                        anyhow::bail!("ensemble.agents.{name}.{field} contains a blank option ID");
                    }
                    if value.trim().is_empty() {
                        anyhow::bail!(
                            "ensemble.agents.{name}.{field}.{id} must not have a blank desired value"
                        );
                    }
                }
            }
        }
        self.validate_agent_list("plan_agents", &self.plan_agents, EnsembleWorkflow::Plan)?;
        self.validate_agent_list(
            "review_agents",
            &self.review_agents,
            EnsembleWorkflow::Review,
        )?;
        if self.max_concurrent_agents == 0 {
            anyhow::bail!("ensemble.max_concurrent_agents must be greater than zero");
        }
        if self.review_startup_timeout_seconds == 0 {
            anyhow::bail!("ensemble.review_startup_timeout_seconds must be greater than zero");
        }
        if self.review_turn_timeout_seconds == 0 {
            anyhow::bail!("ensemble.review_turn_timeout_seconds must be greater than zero");
        }
        if self.cancel_grace_seconds == 0 {
            anyhow::bail!("ensemble.cancel_grace_seconds must be greater than zero");
        }
        if self.max_synthesis_bytes_per_agent == 0 {
            anyhow::bail!("ensemble.max_synthesis_bytes_per_agent must be greater than zero");
        }
        Ok(())
    }

    fn validate_agent_list(
        &self,
        field: &str,
        names: &[String],
        workflow: EnsembleWorkflow,
    ) -> anyhow::Result<()> {
        if names.is_empty() {
            anyhow::bail!("ensemble.{field} must not be empty");
        }
        let mut seen = HashSet::new();
        for name in names {
            if name.trim().is_empty() {
                anyhow::bail!("ensemble.{field} contains an empty agent name");
            }
            if !seen.insert(name) {
                anyhow::bail!("ensemble.{field} contains duplicate agent {name:?}");
            }
            let Some(agent) = self.agents.get(name) else {
                anyhow::bail!("ensemble.{field} references unknown agent {name:?}");
            };
            if agent.mode_for(workflow).is_none() {
                let workflow_field = match workflow {
                    EnsembleWorkflow::Plan => "plan_mode",
                    EnsembleWorkflow::Review => "review_mode",
                };
                anyhow::bail!(
                    "ensemble.agents.{name} must configure {workflow_field} because it is selected by ensemble.{field}"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
