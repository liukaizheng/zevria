//! Frontend-neutral application composition, persistence services, and session lifecycle.

use zevria_ensemble as ensemble;
pub mod acp_host;
pub mod clean;
pub mod config;
mod models;
pub mod runtime;
pub mod session_config;
mod session_lease;
mod session_models;
mod settings;
pub mod skills;
mod subtasks;
pub mod theme_store;

pub use config::Config;
pub use session_config::SessionConfig;

#[cfg(test)]
mod characterization_tests;
#[cfg(test)]
mod composition_tests;

/// Opt-in hooks for cross-owner lifecycle tests, never enabled by default.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    pub use crate::models::{load as load_config, revision as model_revision};
    pub use crate::session_lease::RootSessionLease;

    /// Compact TOML fixture notation only: production accepts models.jsonc,
    /// never provider tables in the ordinary config. All model fields remain required.
    pub fn split_fixture(source: &str) -> anyhow::Result<(String, String)> {
        let mut document = source.parse::<toml_edit::DocumentMut>()?;
        let table: toml::Table = toml::from_str(source)?;
        let mut models = serde_json::Map::new();
        document
            .remove("providers")
            .ok_or_else(|| anyhow::anyhow!("fixture missing providers"))?;
        models.insert(
            "providers".into(),
            serde_json::to_value(&table["providers"])?,
        );
        Ok((document.to_string(), serde_json::to_string_pretty(&models)?))
    }

    pub fn parse_fixture(source: &str) -> anyhow::Result<crate::Config> {
        let (ordinary, models) = split_fixture(source)?;
        crate::Config::parse(&ordinary, &models)
    }

    pub fn write_fixture(path: &std::path::Path, source: &str) -> anyhow::Result<crate::Config> {
        let (ordinary, models) = split_fixture(source)?;
        std::fs::write(path, ordinary)?;
        std::fs::write(zevria_foundation::config::models_path_for(path), models)?;
        load_config(path)
    }
}
