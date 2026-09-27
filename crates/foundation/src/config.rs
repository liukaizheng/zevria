//! Provider-neutral profile values and generic config discovery.
use anyhow::Context as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// Provider-neutral reasoning effort, ordered from least to most effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningLevel {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub const MODELS_FILE_NAME: &str = "models.jsonc";

/// The model catalog always lives beside the resolved ordinary configuration.
pub fn models_path_for(config: &Path) -> PathBuf {
    config.with_file_name(MODELS_FILE_NAME)
}

/// Stable configured identity of one provider/model profile.
///
/// Both components are user-owned, case-sensitive durable identities. The
/// provider crate validates that they are nonblank and resolve to a declared
/// catalog entry before constructing a runtime policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfileRef {
    pub provider: String,
    pub model: String,
}

impl ModelProfileRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

impl fmt::Display for ModelProfileRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

/// Context limits owned by one resolved model profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelContextPolicy {
    pub profile: ModelProfileRef,
    /// Physical/provider context capacity.
    pub context_window_tokens: u64,
    /// Hard ceiling used for input-cost admission and automatic compaction.
    pub input_token_limit: u64,
    pub retained_user_tokens: u64,
}

impl ModelContextPolicy {
    pub fn trigger_tokens(&self, auto_trigger_percent: u64) -> u64 {
        let product =
            u128::from(self.input_token_limit).saturating_mul(u128::from(auto_trigger_percent));
        u64::try_from(product / 100).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
    pub directory: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            directory: None,
        }
    }
}

/// Outcome of loading a config file or creating its first-run template.
#[derive(Debug)]
pub enum LoadOrCreate<T> {
    Created { path: PathBuf },
    Loaded(T),
}

/// Load a compile-time-composed config from `$ZEVRIA_CONFIG` or
/// `~/.zevria/config.toml`, creating the supplied setup template on first run.
/// The caller decides how to report the intentional first-run stop.
pub fn load_or_create<T>(default_template: &str) -> anyhow::Result<LoadOrCreate<T>>
where
    T: DeserializeOwned,
{
    let path = config_path()?;
    if !path.exists() {
        create_default_file(&path, default_template)?;
        return Ok(LoadOrCreate::Created { path });
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read the config file at {}", path.display()))?;
    toml::from_str(&contents)
        .map(LoadOrCreate::Loaded)
        .with_context(|| format!("failed to parse the config file at {}", path.display()))
}

/// The directory holding everything Zevria-related: `~/.zevria`.
pub fn zevria_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::runtime_paths::home_dir()?.join(".zevria"))
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("ZEVRIA_CONFIG").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(zevria_dir()?.join("config.toml"))
}

pub fn create_default_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create the config directory at {}",
                parent.display()
            )
        })?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create the default config at {}", path.display()))?;
    use std::io::Write as _;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .with_context(|| format!("failed to write the default config to {}", path.display()))
}
