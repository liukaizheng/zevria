//! Mode assignment transactions. The shared lock and atomic TOML replacement
//! preserve unrelated settings; the sibling provider catalog is read-only.
use crate::{config::Config, settings};
use anyhow::Context as _;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use zevria_foundation::{ModelRole, config::models_path_for};
use zevria_model::models::{ModelSelection, ModelSettingsService};

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;

pub fn load(config_path: &Path) -> anyhow::Result<Config> {
    let models_path = models_path_for(config_path);
    let mut config = Config::parse_at(
        &settings::read_regular(config_path)?,
        &settings::read_regular(&models_path).with_context(|| {
            format!(
                "failed to read models configuration at {}",
                models_path.display()
            )
        })?,
        &models_path,
    )?;
    config.source_path = Some(config_path.to_path_buf());
    config.models_path = Some(models_path);
    Ok(config)
}

pub fn revision(config: &Config) -> anyhow::Result<String> {
    let bytes =
        serde_json::to_vec(&(&config.providers, &config.modes, &config.session.compaction))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) struct LocalModelSettings {
    pub config_path: PathBuf,
    pub models_path: PathBuf,
}

impl LocalModelSettings {
    fn transaction(
        &self,
        expected_revision: &str,
        prepare: impl FnOnce(&Config, &str, &str) -> anyhow::Result<(String, String)>,
    ) -> anyhow::Result<String> {
        settings::transaction(&self.config_path, |before| {
            let catalog = settings::read_regular(&self.models_path)?;
            let current = Config::parse_at(before, &catalog, &self.models_path)?;
            anyhow::ensure!(
                revision(&current)? == expected_revision,
                "model configuration changed externally; reopen this session before writing"
            );
            let result = prepare(&current, before, &catalog)?;
            anyhow::ensure!(
                settings::read_regular(&self.models_path)? == catalog,
                "model catalog changed while preparing the update; nothing was written"
            );
            // The outer transaction rechecks TOML immediately before publishing.
            Ok(result)
        })
    }
}

impl ModelSettingsService for LocalModelSettings {
    fn validate(&self, expected_revision: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            revision(&load(&self.config_path)?)? == expected_revision,
            "model configuration changed externally; reopen this session to adopt the new catalog/assignments"
        );
        Ok(())
    }

    fn save(
        &self,
        expected_revision: &str,
        role: ModelRole,
        target: &ModelSelection,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            matches!(role, ModelRole::Build | ModelRole::Plan),
            "only Build or Plan may be changed"
        );
        self.transaction(expected_revision, |current, before, catalog| {
            let candidate = current
                .routing()
                .catalog()
                .find(|entry| entry.profile == target.profile)
                .context("selected model is no longer configured")?;
            anyhow::ensure!(
                candidate.reasoning_levels.contains(&target.reasoning_level),
                "reasoning level {} is not configured for {}",
                target.reasoning_level,
                target.profile
            );
            current
                .compaction_policy()
                .with_role(role, candidate.context_policy())?;
            let (after, next_revision) = if current.modes.for_role(role).selection() == *target {
                (before.to_string(), expected_revision.to_string())
            } else {
                let mut document = before.parse::<toml_edit::DocumentMut>()?;
                let assignment = document
                    .get_mut("modes")
                    .and_then(toml_edit::Item::as_table_like_mut)
                    .and_then(|modes| modes.get_mut(role.name()))
                    .and_then(toml_edit::Item::as_table_like_mut)
                    .context("cannot locate existing mode assignment")?;
                for (key, text) in [
                    ("provider", target.profile.provider.clone()),
                    ("model", target.profile.model.clone()),
                    ("reasoning_level", target.reasoning_level.to_string()),
                ] {
                    let value = assignment
                        .get_mut(key)
                        .and_then(toml_edit::Item::as_value_mut)
                        .context("cannot locate existing mode assignment field")?;
                    // Do not reformat unchanged values (including literal/quoted strings).
                    if value.as_str() != Some(&text) {
                        let decor = value.decor().clone();
                        *value = toml_edit::Value::from(text);
                        *value.decor_mut() = decor;
                    }
                }
                let after = document.to_string();
                let prepared = Config::parse_at(&after, catalog, &self.models_path)?;
                anyhow::ensure!(
                    prepared.modes.for_role(role).selection() == *target,
                    "prepared assignment differs from selection"
                );
                let revision = revision(&prepared)?;
                (after, revision)
            };
            Ok((after, next_revision))
        })
    }
}
