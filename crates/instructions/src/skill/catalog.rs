//! Immutable installed definitions, bounded discovery candidates and configuration.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    borrow::Borrow,
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub rules: Vec<SkillEnableRule>,
}
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rules: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEnableRule {
    pub name: SkillName,
    pub enabled: bool,
}
impl SkillsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.rules.len() <= 4096,
            "skills.rules exceeds 4096 name rules"
        );
        Ok(())
    }
    pub fn name_enabled(&self, name: &SkillName) -> bool {
        self.enabled
            && self
                .rules
                .iter()
                .rev()
                .find(|r| &r.name == name)
                .is_none_or(|r| r.enabled)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSelectionStatus {
    Selected,
    Shadowed,
    Ambiguous,
    Unavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillCatalogEntry {
    pub name: SkillName,
    pub metadata: SkillMetadata,
    pub scope: Option<FixedSkillScope>,
    pub manifest: Option<SkillRelativePath>,
    pub selection: SkillSelectionStatus,
    pub enabled: bool,
    pub digest: SkillDigest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInvalidCandidate {
    pub scope: FixedSkillScope,
    pub manifest: SkillRelativePath,
    pub status: String,
    pub diagnostic: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    Global,
    Project,
    PersistedSession,
}
impl std::fmt::Display for SkillScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::PersistedSession => "persisted session",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub scope: SkillScope,
    pub source: Option<PathBuf>,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiscovery {
    pub selected: BTreeMap<SkillName, SkillDefinition>,
    pub diagnostics: Vec<SkillDiagnostic>,
    pub omitted_diagnostics: usize,
    pub definitions: Vec<SkillDefinition>,
    pub invalid_candidates: Vec<SkillInvalidCandidate>,
    pub incomplete_scopes: Vec<FixedSkillScope>,
    pub roots: FixedSkillRoots,
    pub(crate) canonical_roots: BTreeMap<FixedSkillScope, PathBuf>,
}

/// One captured installed view. Reload replaces this Arc, never a session pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalog {
    revision: String,
    definitions: BTreeMap<SkillName, SkillDefinition>,
    config: SkillsConfig,
    canonical_roots: BTreeMap<FixedSkillScope, PathBuf>,
    pub roots: Option<FixedSkillRoots>,
    pub entries: Vec<SkillCatalogEntry>,
    pub invalid_candidates: Vec<SkillInvalidCandidate>,
    pub diagnostics: Vec<SkillDiagnostic>,
    pub omitted_diagnostics: usize,
}
impl Default for SkillCatalog {
    fn default() -> Self {
        Self::new([]).expect("empty catalog")
    }
}
impl SkillCatalog {
    pub fn new(definitions: impl IntoIterator<Item = SkillDefinition>) -> anyhow::Result<Self> {
        let mut selected = BTreeMap::new();
        for definition in definitions {
            definition.snapshot().validate()?;
            let name = definition.name().clone();
            anyhow::ensure!(
                !selected.contains_key(&name),
                "duplicate skill name {name:?}"
            );
            selected.insert(name, definition);
        }
        let config = SkillsConfig::default();
        let entries = selected
            .values()
            .map(|d| entry(d, SkillSelectionStatus::Selected, &config))
            .collect();
        let mut catalog = Self {
            revision: String::new(),
            definitions: selected,
            config,
            canonical_roots: BTreeMap::new(),
            roots: None,
            entries,
            invalid_candidates: Vec::new(),
            diagnostics: Vec::new(),
            omitted_diagnostics: 0,
        };
        catalog.refresh_revision();
        Ok(catalog)
    }
    pub fn from_discovery(discovery: SkillDiscovery, config: SkillsConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let mut multiplicities = BTreeMap::new();
        for definition in &discovery.definitions {
            definition.snapshot().validate()?;
            if let Some(origin) = definition.origin() {
                *multiplicities
                    .entry((origin.provenance.scope, definition.name().clone()))
                    .or_insert(0usize) += 1;
            }
        }
        let entries = discovery
            .definitions
            .iter()
            .map(|definition| {
                let scope = definition
                    .origin()
                    .expect("discovered origin")
                    .provenance
                    .scope;
                let status = if discovery.incomplete_scopes.contains(&scope) {
                    SkillSelectionStatus::Unavailable
                } else if multiplicities[&(scope, definition.name().clone())] > 1 {
                    SkillSelectionStatus::Ambiguous
                } else if discovery
                    .selected
                    .get(definition.name())
                    .is_some_and(|d| d.digest() == definition.digest())
                {
                    SkillSelectionStatus::Selected
                } else {
                    SkillSelectionStatus::Shadowed
                };
                entry(definition, status, &config)
            })
            .collect();
        let mut catalog = Self {
            revision: String::new(),
            definitions: discovery.selected,
            config,
            canonical_roots: discovery.canonical_roots,
            roots: Some(discovery.roots),
            entries,
            invalid_candidates: discovery.invalid_candidates,
            diagnostics: discovery.diagnostics,
            omitted_diagnostics: discovery.omitted_diagnostics,
        };
        catalog.refresh_revision();
        Ok(catalog)
    }
    pub fn with_config(mut self, config: SkillsConfig) -> anyhow::Result<Self> {
        config.validate()?;
        for entry in &mut self.entries {
            entry.enabled = config.name_enabled(&entry.name);
        }
        self.config = config;
        self.refresh_revision();
        Ok(self)
    }
    fn refresh_revision(&mut self) {
        self.entries.sort_by(|a, b| {
            (&a.name, &a.scope, &a.manifest).cmp(&(&b.name, &b.scope, &b.manifest))
        });
        let mut hasher = Sha256::new();
        hasher.update(b"zevria.skill.catalog.v1");
        hasher.update(serde_json::to_vec(&self.entries).expect("catalog serializes"));
        hasher.update(serde_json::to_vec(&self.invalid_candidates).expect("candidates serialize"));
        hasher.update(serde_json::to_vec(&self.config).expect("config serializes"));
        if let Some(roots) = &self.roots {
            for scope in [FixedSkillScope::Global, FixedSkillScope::Project] {
                if let Some(path) = roots.directory(scope) {
                    update_length_delimited(&mut hasher, path.as_os_str().as_encoded_bytes());
                } else {
                    update_length_delimited(&mut hasher, b"unavailable");
                }
            }
        }
        for (scope, root) in &self.canonical_roots {
            hasher.update([match scope {
                FixedSkillScope::Global => 0,
                FixedSkillScope::Project => 1,
            }]);
            update_length_delimited(&mut hasher, root.as_os_str().as_encoded_bytes());
        }
        for diagnostic in &self.diagnostics {
            update_length_delimited(&mut hasher, diagnostic.scope.to_string().as_bytes());
            update_length_delimited(&mut hasher, diagnostic.message.as_bytes());
            if let Some(path) = &diagnostic.source {
                update_length_delimited(&mut hasher, path.as_os_str().as_encoded_bytes());
            }
        }
        hasher.update((self.omitted_diagnostics as u64).to_be_bytes());
        self.revision = SkillDigest(hasher.finalize().into()).to_string();
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn config(&self) -> &SkillsConfig {
        &self.config
    }
    pub fn get<Q>(&self, name: &Q) -> Option<&SkillDefinition>
    where
        SkillName: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.definitions.get(name)
    }
    pub fn iter(&self) -> impl Iterator<Item = &SkillDefinition> {
        self.definitions.values()
    }
    pub fn names(&self) -> impl Iterator<Item = &SkillName> {
        self.definitions.keys()
    }
    pub fn has_enabled(&self) -> bool {
        self.names().any(|n| self.config.name_enabled(n))
    }
    pub fn len(&self) -> usize {
        self.definitions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}
fn entry(
    definition: &SkillDefinition,
    selection: SkillSelectionStatus,
    config: &SkillsConfig,
) -> SkillCatalogEntry {
    let provenance = definition.origin().map(|o| &o.provenance);
    SkillCatalogEntry {
        name: definition.name().clone(),
        metadata: definition.metadata().clone(),
        scope: provenance.map(|p| p.scope),
        manifest: provenance.map(|p| p.manifest.clone()),
        selection,
        enabled: config.name_enabled(definition.name()),
        digest: definition.digest(),
    }
}
pub fn workspace_skills_dir(workspace: &Path) -> PathBuf {
    workspace.join(".zevria").join("skills")
}
pub fn global_skills_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::zevria_dir()?.join("skills"))
}
pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    SkillDigest(Sha256::digest(bytes).into()).to_string()
}
pub(crate) fn shorten(value: &str, max_bytes: usize) -> (String, bool) {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= max_bytes {
        return (normalized, false);
    }
    let mut end = max_bytes.saturating_sub(3).min(normalized.len());
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if max_bytes >= 3 { "..." } else { "" };
    (format!("{}{suffix}", &normalized[..end]), true)
}
