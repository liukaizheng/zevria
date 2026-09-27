//! Captured catalog, historical pins and effective permission share one resolver.
use super::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

pub const SKILL_CATALOG_DESCRIPTION_BYTES: usize = 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPromptEntry {
    pub name: SkillName,
    pub description: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPromptCatalog {
    pub enabled: bool,
    pub entries: Vec<SkillPromptEntry>,
}
impl SkillPromptCatalog {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.enabled || self.entries.is_empty(),
            "disabled catalog must be cleared"
        );
        anyhow::ensure!(
            self.entries.windows(2).all(|p| p[0].name < p[1].name),
            "catalog names must be uniquely ordered"
        );
        for entry in &self.entries {
            anyhow::ensure!(
                entry.description.len() <= SKILL_CATALOG_DESCRIPTION_BYTES
                    && shorten(&entry.description, SKILL_CATALOG_DESCRIPTION_BYTES).0
                        == entry.description,
                "noncanonical catalog description"
            );
        }
        Ok(())
    }
    pub(crate) fn render(&self) -> String {
        if !self.enabled {
            return crate::prompts::SKILL_SELECTION_UNAVAILABLE.into();
        }
        format!(
            "{}\n{}",
            crate::prompts::SKILL_SELECTION_INSTRUCTIONS,
            serde_json::to_string(&self.entries).expect("catalog serializes")
        )
    }
}
#[derive(Debug, Clone)]
pub struct SkillContext {
    pub catalog: Arc<SkillCatalog>,
    pub pins: ActiveSkills,
    /// Effective permission, including the actual activation capability for model projections.
    pub mode_enabled: bool,
}
impl SkillContext {
    pub fn ensure_enabled(&self, name: &SkillName) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mode_enabled,
            "skills are unavailable in the current workflow or tool capability"
        );
        anyhow::ensure!(
            self.catalog.config().name_enabled(name),
            "skill {name} is disabled by name"
        );
        Ok(())
    }
    pub fn resolve(
        &self,
        name: &SkillName,
        origin: SkillInvocationOrigin,
    ) -> anyhow::Result<SkillSnapshot> {
        self.ensure_enabled(name)?;
        if let Some(pin) = self.pins.get(name) {
            return Ok(pin.clone());
        }
        let definition = self.catalog.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown or unavailable skill {name}; inspect installed skills in management"
            )
        })?;
        anyhow::ensure!(
            origin == SkillInvocationOrigin::Explicit
                || definition.metadata().invocation_policy == SkillInvocationPolicy::ModelAllowed,
            "skill {name} requires an explicit typed user invocation before model reapplication"
        );
        Ok(definition.snapshot())
    }
    fn names(&self) -> BTreeSet<SkillName> {
        self.catalog
            .names()
            .cloned()
            .chain(self.pins.snapshots().map(|s| s.name().clone()))
            .collect()
    }
    pub fn completions(&self) -> Vec<SkillMeta> {
        self.names()
            .into_iter()
            .filter_map(|name| {
                self.resolve(&name, SkillInvocationOrigin::Explicit)
                    .ok()
                    .map(|s| SkillMeta {
                        name,
                        description: shorten(s.description(), 256).0,
                    })
            })
            .collect()
    }
    pub fn prompt_catalog(&self) -> SkillPromptCatalog {
        // Eligibility and descriptions are installation metadata, never pin state.
        // Explicit-only and removed pins remain reapplicable but do not alter the prefix.
        let context = Self {
            catalog: self.catalog.clone(),
            pins: ActiveSkills::default(),
            mode_enabled: self.mode_enabled,
        };
        SkillPromptCatalog {
            enabled: self.mode_enabled && self.catalog.config().enabled,
            entries: self
                .catalog
                .names()
                .cloned()
                .filter_map(|name| {
                    context
                        .resolve(&name, SkillInvocationOrigin::Model)
                        .ok()
                        .map(|s| SkillPromptEntry {
                            name,
                            description: shorten(s.description(), SKILL_CATALOG_DESCRIPTION_BYTES)
                                .0,
                        })
                })
                .collect(),
        }
    }
    pub fn pin_diagnostics(&self) -> Vec<SkillDiagnostic> {
        self.pins.snapshots().filter_map(|pin| {
            let message = match self.catalog.get(pin.name()) {
                Some(d) if d.digest() == pin.digest() => return None,
                Some(_) => format!("skill {} retains its pinned snapshot because the installed definition changed", pin.name()),
                None => format!("skill {} retains its pinned snapshot because its installed definition is unavailable", pin.name()),
            };
            Some(SkillDiagnostic { scope: SkillScope::PersistedSession, source: None, message })
        }).collect()
    }
    pub fn render_active_context(&self) -> Option<String> {
        if !self.mode_enabled {
            return None;
        }
        let bodies = self
            .pins
            .snapshots()
            .filter(|s| self.catalog.config().name_enabled(s.name()))
            .map(|s| crate::DirectiveContent::skill(s).text)
            .collect::<Vec<_>>();
        (!bodies.is_empty()).then(|| bodies.join("\n\n"))
    }
}
