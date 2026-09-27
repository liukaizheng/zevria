//! Complete data-only management views. Only the local service reloads fixed
//! roots or writes configuration; queries use the captured immutable context.
use super::*;
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillManagementRequest {
    List {
        #[serde(default)]
        query: String,
    },
    Inspect {
        name: SkillName,
    },
    Reload {
        expected_revision: String,
    },
    SetEnabled {
        expected_revision: String,
        name: SkillName,
        enabled: bool,
    },
}
impl SkillManagementRequest {
    pub fn is_mutation(&self) -> bool {
        matches!(self, Self::Reload { .. } | Self::SetEnabled { .. })
    }
    pub fn expected_revision(&self) -> Option<&str> {
        match self {
            Self::Reload { expected_revision }
            | Self::SetEnabled {
                expected_revision, ..
            } => Some(expected_revision),
            _ => None,
        }
    }
}
pub trait SkillManagementService: Send + Sync {
    fn update<'a>(
        &'a self,
        request: SkillManagementRequest,
        installed: Arc<SkillCatalog>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Arc<SkillCatalog>>> + Send + 'a>>;
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCatalogCounts {
    pub candidates: usize,
    pub enabled_names: usize,
    pub active: usize,
    pub diagnostics: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillManagementEntry {
    pub name: SkillName,
    pub digest: SkillDigest,
    pub scope: Option<FixedSkillScope>,
    pub manifest: Option<SkillRelativePath>,
    pub status: String,
    pub enabled: bool,
    pub active: bool,
    pub pinned: bool,
    pub resources: String,
    pub metadata: SkillMetadata,
    pub metadata_shortened: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillManagementView {
    pub revision: String,
    pub global_location: Option<String>,
    pub project_location: Option<String>,
    pub globally_enabled: bool,
    pub counts: SkillCatalogCounts,
    pub entries: Vec<SkillManagementEntry>,
    pub invalid_entries: Vec<SkillInvalidCandidate>,
    pub diagnostics: Vec<String>,
    pub omitted_diagnostics: usize,
    /// Complete, unique explicit-invocation completions, independent of candidate filtering.
    pub completions: Vec<SkillMeta>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillManagementResult {
    View {
        view: SkillManagementView,
    },
    Changed {
        revision: String,
        counts: SkillCatalogCounts,
        unchanged: bool,
    },
    Error {
        code: String,
        message: String,
    },
}
impl SkillManagementResult {
    pub fn error(code: &str, message: impl AsRef<str>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.as_ref().chars().take(1024).collect(),
        }
    }
}
fn clip(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("...");
    true
}
impl SkillContext {
    pub fn management_counts(&self) -> SkillCatalogCounts {
        SkillCatalogCounts {
            candidates: self.catalog.entries.len() + self.catalog.invalid_candidates.len(),
            enabled_names: self.completions().len(),
            active: self.pins.len(),
            diagnostics: self.catalog.diagnostics.len()
                + self.catalog.omitted_diagnostics
                + self.pin_diagnostics().len(),
        }
    }
    pub fn management_view(
        &self,
        request: &SkillManagementRequest,
    ) -> anyhow::Result<SkillManagementView> {
        let (query, inspect) = match request {
            SkillManagementRequest::List { query } => (query.as_str(), false),
            SkillManagementRequest::Inspect { name } => (name.as_str(), true),
            _ => anyhow::bail!("not a skill query"),
        };
        anyhow::ensure!(
            query.len() <= 1024 && !query.chars().any(char::is_control),
            "skill query must be at most 1024 bytes without controls"
        );
        let query = query.trim().to_lowercase();
        let mut entries = Vec::new();
        for candidate in &self.catalog.entries {
            let status = match candidate.selection {
                SkillSelectionStatus::Selected => "selected",
                SkillSelectionStatus::Shadowed => "shadowed",
                SkillSelectionStatus::Ambiguous => "ambiguous",
                SkillSelectionStatus::Unavailable => "unavailable",
            };
            let active = self
                .pins
                .get(&candidate.name)
                .is_some_and(|s| s.digest() == candidate.digest);
            entries.push(self.management_entry(candidate.clone(), status, active));
        }
        for snapshot in self.pins.snapshots() {
            if entries
                .iter()
                .any(|e| e.name == *snapshot.name() && e.active && e.digest == snapshot.digest())
            {
                continue;
            }
            let p = snapshot.provenance();
            entries.push(self.management_entry(
                SkillCatalogEntry {
                    name: snapshot.name().clone(),
                    metadata: snapshot.metadata().clone(),
                    scope: p.map(|p| p.scope),
                    manifest: p.map(|p| p.manifest.clone()),
                    selection: SkillSelectionStatus::Unavailable,
                    enabled: self.catalog.config().name_enabled(snapshot.name()),
                    digest: snapshot.digest(),
                },
                "historical",
                true,
            ));
        }
        entries.retain(|e| {
            if inspect {
                e.name.as_str() == query
            } else {
                e.name.as_str().contains(&query)
                    || e.metadata.description.to_lowercase().contains(&query)
                    || e.manifest
                        .as_ref()
                        .is_some_and(|p| p.as_str().to_lowercase().contains(&query))
                    || e.scope
                        .is_some_and(|s| format!("{s:?}").to_lowercase().contains(&query))
            }
        });
        let pin_diagnostics = self.pin_diagnostics();
        let diagnostics = self
            .catalog
            .diagnostics
            .iter()
            .chain(pin_diagnostics.iter())
            .filter_map(|d| {
                let mut text = format!(
                    "{}: {}: {}",
                    d.scope,
                    d.source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    d.message
                );
                let matched = query.is_empty() || text.to_lowercase().contains(&query);
                clip(&mut text, 1536);
                matched.then_some(text)
            })
            .collect();
        // Nameless malformed candidates cannot be inspected by an invented name.
        let invalid_entries = self
            .catalog
            .invalid_candidates
            .iter()
            .filter(|e| {
                !inspect
                    && (query.is_empty()
                        || e.manifest.as_str().to_lowercase().contains(&query)
                        || e.diagnostic.to_lowercase().contains(&query)
                        || format!("{:?}", e.scope).to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        let location = |p: &std::path::Path| {
            let mut text = p.display().to_string();
            clip(&mut text, 2048);
            text
        };
        Ok(SkillManagementView {
            revision: self.catalog.revision().into(),
            global_location: self
                .catalog
                .roots
                .as_ref()
                .and_then(|r| r.global().map(location)),
            project_location: self.catalog.roots.as_ref().map(|r| location(r.project())),
            globally_enabled: self.catalog.config().enabled,
            counts: self.management_counts(),
            entries,
            invalid_entries,
            diagnostics,
            omitted_diagnostics: self.catalog.omitted_diagnostics,
            completions: self.completions(),
        })
    }
    fn management_entry(
        &self,
        candidate: SkillCatalogEntry,
        status: &str,
        active: bool,
    ) -> SkillManagementEntry {
        let mut metadata = candidate.metadata;
        let mut shortened = clip(&mut metadata.description, 1024);
        for text in [
            &mut metadata.short_description,
            &mut metadata.interface.display_name,
            &mut metadata.interface.default_prompt,
            &mut metadata.interface.brand_color,
        ]
        .into_iter()
        .flatten()
        {
            shortened |= clip(text, 256);
        }
        shortened |= metadata.dependencies.len() > 8;
        metadata.dependencies.truncate(8);
        for dependency in &mut metadata.dependencies {
            shortened |= clip(&mut dependency.kind, 64);
            shortened |= clip(&mut dependency.value, 128);
        }
        let package = candidate
            .manifest
            .as_ref()
            .is_some_and(|p| p.as_str().ends_with("/SKILL.md"));
        // Availability is authoritatively checked by each live contained read.
        let resources = if !candidate.enabled {
            "disabled"
        } else if !package {
            "body_only"
        } else if !active {
            "requires_activation"
        } else {
            "live_on_read"
        };
        SkillManagementEntry {
            name: candidate.name,
            digest: candidate.digest,
            scope: candidate.scope,
            manifest: candidate.manifest,
            status: status.into(),
            enabled: candidate.enabled,
            active,
            pinned: active,
            resources: resources.into(),
            metadata,
            metadata_shortened: shortened,
        }
    }
}
