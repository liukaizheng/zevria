//! Deterministic bounded discovery inside exactly two fixed native roots.
//! Canonical alias targets are identities, never additional search roots.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::*;

const MAX_DEPTH: usize = 6;
const MAX_DIRECTORIES: usize = 2_000;
const MAX_ENTRIES: usize = 20_000;
const MAX_CANDIDATES: usize = 2_048;
const MAX_AGGREGATE_BYTES: usize = 32 * 1024 * 1024;

/// Captured once for a session. There is deliberately no root setter or
/// constructor accepting arbitrary source roots. Config location is irrelevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedSkillRoots {
    global: Option<PathBuf>,
    project: PathBuf,
    global_unavailable: Option<String>,
}

impl FixedSkillRoots {
    pub fn capture(startup_workspace: &Path) -> Self {
        let (global, global_unavailable) = match global_skills_dir() {
            Ok(path) => (Some(path), None),
            Err(error) => (
                None,
                Some(
                    format!("global skill root unavailable: {error:#}")
                        .chars()
                        .take(1024)
                        .collect(),
                ),
            ),
        };
        Self {
            global,
            project: workspace_skills_dir(startup_workspace),
            global_unavailable,
        }
    }

    pub fn global(&self) -> Option<&Path> {
        self.global.as_deref()
    }
    pub fn project(&self) -> &Path {
        &self.project
    }
    pub fn directory(&self, scope: FixedSkillScope) -> Option<&Path> {
        match scope {
            FixedSkillScope::Global => self.global(),
            FixedSkillScope::Project => Some(self.project()),
        }
    }

    #[cfg(test)]
    pub(super) fn fixture(home: Option<&Path>, workspace: &Path) -> Self {
        Self {
            global: home.map(|home| home.join(".zevria/skills")),
            project: workspace_skills_dir(workspace),
            global_unavailable: home
                .is_none()
                .then(|| "global skill root unavailable: HOME is unavailable".into()),
        }
    }
}

#[derive(Default)]
struct Diagnostics {
    records: Vec<SkillDiagnostic>,
    omitted: usize,
}

impl Diagnostics {
    fn push(&mut self, scope: FixedSkillScope, path: Option<&Path>, message: impl AsRef<str>) {
        if self.records.len() == 256 {
            self.omitted += 1;
            return;
        }
        self.records.push(SkillDiagnostic {
            scope: match scope {
                FixedSkillScope::Global => SkillScope::Global,
                FixedSkillScope::Project => SkillScope::Project,
            },
            source: path.map(Path::to_path_buf),
            message: message.as_ref().chars().take(1024).collect(),
        });
    }
}

struct Candidate {
    scope: FixedSkillScope,
    path: PathBuf,
    canonical: PathBuf,
    canonical_root: PathBuf,
    relative: SkillRelativePath,
    layout: SkillLayout,
}

struct Scan<'a> {
    scope: FixedSkillScope,
    canonical_root: PathBuf,
    visited: BTreeSet<PathBuf>,
    directories: usize,
    entries: usize,
    incomplete: bool,
    candidates: &'a mut Vec<Candidate>,
    diagnostics: &'a mut Diagnostics,
}

impl Scan<'_> {
    fn incomplete(&mut self, path: &Path, reason: &str) {
        self.incomplete = true;
        self.diagnostics.push(
            self.scope,
            Some(path),
            format!("incomplete scope: {reason}; this scope cannot certify an unambiguous winner"),
        );
    }

    fn visit(&mut self, directory: &Path, depth: usize) {
        if self.incomplete {
            return;
        }
        if depth > MAX_DEPTH {
            self.incomplete(directory, "discovery depth exceeds 6");
            return;
        }
        let canonical = match resource::source_path(directory) {
            Ok(path) => path,
            Err(error) => {
                self.incomplete(directory, &format!("cannot resolve directory: {error}"));
                return;
            }
        };
        if !canonical.starts_with(&self.canonical_root) {
            self.diagnostics.push(self.scope, Some(directory), "outside-root directory alias rejected; install the package beneath one of the two fixed skill roots");
            return;
        }
        let relative = canonical
            .strip_prefix(&self.canonical_root)
            .expect("contained directory");
        if relative.components().any(|part| {
            part.as_os_str()
                .to_str()
                .is_some_and(|part| part.starts_with('.'))
        }) {
            self.diagnostics.push(
                self.scope,
                Some(directory),
                "hidden descendant alias skipped",
            );
            return;
        }
        if relative.components().count() > MAX_DEPTH {
            self.incomplete(directory, "canonical discovery depth exceeds 6");
            return;
        }
        if !self.visited.insert(canonical) {
            self.diagnostics.push(
                self.scope,
                Some(directory),
                "canonical directory alias or cycle deduplicated",
            );
            return;
        }
        self.directories += 1;
        if self.directories > MAX_DIRECTORIES {
            self.incomplete(directory, "directory limit exceeds 2000");
            return;
        }
        if depth > 0 {
            let manifest = directory.join("SKILL.md");
            match std::fs::symlink_metadata(&manifest) {
                Ok(_) => {
                    self.candidate(&manifest, SkillLayout::Package);
                    // A recognized package owns its content tree, even when
                    // malformed. Do not enumerate its auxiliary entries.
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    self.incomplete(
                        directory,
                        &format!("cannot inspect package manifest: {error}"),
                    );
                    return;
                }
            }
        }
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.incomplete(directory, &format!("cannot list directory: {error}"));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            self.entries += 1;
            if self.entries > MAX_ENTRIES {
                self.incomplete(directory, "entry limit exceeds 20000");
                return;
            }
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => {
                    self.incomplete(directory, &format!("cannot list entry: {error}"));
                    return;
                }
            }
        }
        paths.sort();
        for path in paths {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.diagnostics.push(
                        self.scope,
                        Some(&path),
                        format!("unavailable candidate: {error}"),
                    );
                    continue;
                }
            };
            if metadata.is_dir() {
                self.visit(&path, depth + 1);
            } else if depth == 0 && path.extension().is_some_and(|extension| extension == "md") {
                self.candidate(&path, SkillLayout::Flat);
            }
            if self.incomplete {
                break;
            }
        }
    }

    fn candidate(&mut self, path: &Path, layout: SkillLayout) {
        if self.candidates.len() == MAX_CANDIDATES {
            self.incomplete(path, "catalog candidate limit exceeds 2048");
            return;
        }
        let canonical = match resource::source_path(path) {
            Ok(path) => path,
            Err(error) => {
                self.diagnostics.push(
                    self.scope,
                    Some(path),
                    format!("unavailable manifest: {error}"),
                );
                return;
            }
        };
        if !canonical.starts_with(&self.canonical_root) {
            self.diagnostics.push(
                self.scope,
                Some(path),
                "outside-root manifest alias rejected; install beneath a fixed skill root",
            );
            return;
        }
        // Canonical root-relative identity deduplicates aliases before name
        // ambiguity. Advertised paths remain available for diagnostics.
        let relative = canonical
            .strip_prefix(&self.canonical_root)
            .expect("contained");
        if relative.parent().is_some_and(|parent| {
            parent.components().any(|part| {
                part.as_os_str()
                    .to_str()
                    .is_some_and(|part| part.starts_with('.'))
            })
        }) {
            self.diagnostics.push(
                self.scope,
                Some(path),
                "hidden descendant manifest alias skipped",
            );
            return;
        }
        if relative.components().count().saturating_sub(1) > MAX_DEPTH {
            self.incomplete(path, "canonical manifest depth exceeds 6");
            return;
        }
        let Some(relative) = relative.to_str() else {
            self.diagnostics
                .push(self.scope, Some(path), "manifest locator is not UTF-8");
            return;
        };
        let relative =
            match SkillRelativePath::new(relative.replace(std::path::MAIN_SEPARATOR, "/")) {
                Ok(relative) => relative,
                Err(error) => {
                    self.diagnostics
                        .push(self.scope, Some(path), error.to_string());
                    return;
                }
            };
        self.candidates.push(Candidate {
            scope: self.scope,
            path: path.to_path_buf(),
            canonical,
            canonical_root: self.canonical_root.clone(),
            relative,
            layout,
        });
    }
}

fn read_text(
    path: &Path,
    root: &Path,
    cap: usize,
    remaining: &mut usize,
) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(metadata.is_file(), "skill source is not a regular file");
    anyhow::ensure!(
        metadata.len() <= cap as u64,
        "skill source exceeds its {cap}-byte cap"
    );
    if *remaining < metadata.len() as usize {
        *remaining = 0;
        anyhow::bail!("aggregate skill source budget exhausted");
    }
    let relative = path
        .strip_prefix(root)?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("source locator is not UTF-8"))?;
    let relative = SkillRelativePath::new(relative.replace(std::path::MAIN_SEPARATOR, "/"))?;
    let bytes = resource::read_contained(root, &relative, cap.min(*remaining))?;
    let length = bytes.len();
    let exhausted = length > *remaining;
    *remaining = remaining.saturating_sub(length);
    anyhow::ensure!(
        !exhausted && length <= cap,
        "skill source byte budget exceeded"
    );
    Ok(String::from_utf8(bytes)?)
}

fn load_candidate(
    candidate: &Candidate,
    remaining: &mut usize,
    diagnostics: &mut Diagnostics,
) -> anyhow::Result<SkillDefinition> {
    let source = read_text(
        &candidate.canonical,
        &candidate.canonical_root,
        MAX_SKILL_BYTES as usize,
        remaining,
    )?;
    let derived = match candidate.layout {
        SkillLayout::Flat => candidate.canonical.file_stem(),
        SkillLayout::Package => candidate.canonical.parent().and_then(Path::file_name),
    }
    .and_then(|name| name.to_str())
    .ok_or_else(|| anyhow::anyhow!("skill filename is not UTF-8"))?;
    let mut parsed = parser::parse_document(&source, derived, candidate.layout)?;
    let mut invalid_policy = parsed.invalid_policy;
    if candidate.layout == SkillLayout::Package {
        let package = candidate.canonical.parent().expect("manifest parent");
        for name in ["agents/openai.yaml", "agents/zevria.yaml"] {
            let path = package.join(name);
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {}
            }
            let result = resource::source_path(&path)
                .map_err(anyhow::Error::from)
                .and_then(|canonical| {
                    anyhow::ensure!(
                        canonical.starts_with(package),
                        "sidecar escapes package containment"
                    );
                    read_text(&canonical, &candidate.canonical_root, 32 * 1024, remaining)
                });
            match result {
                Ok(source) => {
                    invalid_policy |=
                        parser::merge_sidecar(&mut parsed.metadata, &source, &mut parsed.warnings);
                }
                Err(error) => {
                    invalid_policy = true;
                    parsed.warnings.push(format!("cannot read policy-bearing sidecar {name}: {error:#}; fresh activation is explicit-only"));
                }
            }
        }
    }
    if invalid_policy {
        parsed.metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
    }
    for warning in parsed.warnings {
        diagnostics.push(candidate.scope, Some(&candidate.path), warning);
    }
    let provenance = SkillProvenance {
        scope: candidate.scope,
        layout: candidate.layout,
        source_binding: SourceBinding::for_source(
            candidate.scope,
            &candidate.canonical_root,
            &candidate.relative,
        )?,
        manifest: candidate.relative.clone(),
    };
    let origin = SkillOrigin {
        provenance,
        advertised_document: candidate.path.clone(),
        canonical_document: candidate.canonical.clone(),
        canonical_package: (candidate.layout == SkillLayout::Package)
            .then(|| candidate.canonical.parent().expect("package").to_path_buf()),
        canonical_root: candidate.canonical_root.clone(),
    };
    SkillDefinition::new(
        parsed.name,
        parsed.metadata.description.clone(),
        parsed.body,
        SkillSource::File(candidate.path.clone()),
    )?
    .with_metadata(parsed.metadata, Some(origin))
}

/// Validate one native source without registering it or scanning an arbitrary
/// directory. Containment and native layout are checked before document reads.
pub fn validate_skill_path(
    roots: &FixedSkillRoots,
    path: &Path,
) -> anyhow::Result<(SkillDefinition, Vec<SkillDiagnostic>)> {
    anyhow::ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "validation requires a path without parent traversal beneath a fixed skill root"
    );
    #[cfg(windows)]
    let normalized_path = zevria_foundation::windows_io::normalize_disk_path(path)?;
    #[cfg(windows)]
    let path = normalized_path.as_path();
    let (scope, root_path) = [FixedSkillScope::Project, FixedSkillScope::Global]
        .into_iter()
        .find_map(|scope| {
            let root = roots.directory(scope)?;
            #[cfg(windows)]
            let root = zevria_foundation::windows_io::normalize_disk_path(root).ok()?;
            #[cfg(not(windows))]
            let root = root.to_path_buf();
            path.starts_with(&root).then_some((scope, root))
        })
        .ok_or_else(|| anyhow::anyhow!("validation path is outside the two fixed skill roots"))?;
    let root = root_path.as_path();
    let canonical_root = resource::source_path(root)?;
    let canonical = resource::source_path(path)?;
    anyhow::ensure!(
        canonical.starts_with(&canonical_root),
        "validation path escapes the fixed skill root"
    );
    let path = if canonical.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_path_buf()
    };
    let relative = path.strip_prefix(root)?;
    anyhow::ensure!(
        relative.components().count() <= MAX_DEPTH + 1
            && !relative.components().any(|part| part
                .as_os_str()
                .to_str()
                .is_some_and(|part| part.starts_with('.'))),
        "validation path is hidden or beyond native discovery depth"
    );
    let layout = if relative.components().count() == 1
        && relative.extension().is_some_and(|ext| ext == "md")
    {
        SkillLayout::Flat
    } else if relative.components().count() >= 2
        && relative.file_name().is_some_and(|name| name == "SKILL.md")
    {
        SkillLayout::Package
    } else {
        anyhow::bail!("validation requires a root-level .md document or a package SKILL.md");
    };
    // Nested auxiliary documents cannot become independent package authority.
    let mut ancestor = path.parent().and_then(Path::parent);
    while let Some(parent) = ancestor.filter(|parent| *parent != root && parent.starts_with(root)) {
        anyhow::ensure!(
            std::fs::symlink_metadata(parent.join("SKILL.md")).is_err(),
            "validation path is nested inside another skill package"
        );
        ancestor = parent.parent();
    }
    let mut diagnostics = Diagnostics::default();
    let mut candidates = Vec::new();
    let mut scan = Scan {
        scope,
        canonical_root,
        visited: BTreeSet::new(),
        directories: 0,
        entries: 0,
        incomplete: false,
        candidates: &mut candidates,
        diagnostics: &mut diagnostics,
    };
    scan.candidate(&path, layout);
    let candidate = candidates.first().ok_or_else(|| {
        anyhow::anyhow!(
            "validation candidate rejected: {}",
            diagnostics
                .records
                .first()
                .map(|d| d.message.as_str())
                .unwrap_or("invalid native source")
        )
    })?;
    let mut remaining = MAX_AGGREGATE_BYTES;
    let definition = load_candidate(candidate, &mut remaining, &mut diagnostics)?;
    Ok((definition, diagnostics.records))
}

/// Synchronous bounded scan; async composition roots must call it off executor.
pub fn discover_skills(roots: &FixedSkillRoots) -> SkillDiscovery {
    let mut diagnostics = Diagnostics::default();
    if let Some(error) = &roots.global_unavailable {
        diagnostics.push(FixedSkillScope::Global, None, error);
    }
    let mut candidates = Vec::new();
    let mut incomplete_scopes = BTreeSet::new();
    let mut canonical_roots = BTreeMap::new();
    for scope in [FixedSkillScope::Project, FixedSkillScope::Global] {
        let Some(root) = roots.directory(scope) else {
            continue;
        };
        let canonical_root = match resource::source_path(root) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                incomplete_scopes.insert(scope);
                diagnostics.push(
                    scope,
                    Some(root),
                    format!("incomplete scope: fixed root unavailable: {error}"),
                );
                continue;
            }
        };
        canonical_roots.insert(scope, canonical_root.clone());
        let mut scan = Scan {
            scope,
            canonical_root,
            visited: BTreeSet::new(),
            directories: 0,
            entries: 0,
            incomplete: false,
            candidates: &mut candidates,
            diagnostics: &mut diagnostics,
        };
        scan.visit(root, 0);
        if scan.incomplete {
            incomplete_scopes.insert(scope);
        }
    }
    let mut remaining = MAX_AGGREGATE_BYTES;
    let mut seen = BTreeMap::new();
    let mut valid: BTreeMap<(FixedSkillScope, SkillName), Vec<SkillDefinition>> = BTreeMap::new();
    let mut definitions = Vec::new();
    let mut invalid_candidates = Vec::new();
    for candidate in candidates {
        // Project is processed first, retaining its provenance if both roots
        // alias the same physical manifest.
        if seen.get(&candidate.canonical).is_some_and(|previous| {
            *previous == candidate.scope || !incomplete_scopes.contains(previous)
        }) {
            continue;
        }
        match load_candidate(&candidate, &mut remaining, &mut diagnostics) {
            Ok(definition) => {
                seen.insert(candidate.canonical.clone(), candidate.scope);
                valid
                    .entry((candidate.scope, definition.name().clone()))
                    .or_default()
                    .push(definition.clone());
                definitions.push(definition);
            }
            Err(error) => {
                if error.to_string().contains("aggregate") || remaining == 0 {
                    incomplete_scopes.insert(candidate.scope);
                }
                let message: String = format!("invalid skill: {error:#}")
                    .chars()
                    .take(1024)
                    .collect();
                invalid_candidates.push(SkillInvalidCandidate {
                    scope: candidate.scope,
                    manifest: candidate.relative.clone(),
                    status: "invalid".into(),
                    diagnostic: message.clone(),
                });
                diagnostics.push(candidate.scope, Some(&candidate.path), message);
            }
        }
        if remaining == 0 {
            incomplete_scopes.insert(candidate.scope);
        }
    }
    let mut selected = BTreeMap::new();
    for ((scope, name), mut candidates) in valid {
        if incomplete_scopes.contains(&scope) {
            continue;
        }
        if candidates.len() == 1 {
            selected.insert(name, candidates.pop().expect("one candidate"));
        } else {
            diagnostics.push(scope, None, format!("ambiguous skill name {name:?}: {} distinct valid definitions; none selected from this scope", candidates.len()));
        }
    }
    SkillDiscovery {
        selected,
        diagnostics: diagnostics.records,
        omitted_diagnostics: diagnostics.omitted,
        definitions,
        invalid_candidates,
        incomplete_scopes: incomplete_scopes.into_iter().collect(),
        roots: roots.clone(),
        canonical_roots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(path: &Path, name: &str) {
        std::fs::create_dir_all(path).expect("package directory");
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Description\n---\nExact body"),
        )
        .expect("manifest");
    }

    #[test]
    fn skill_fixed_roots_scan_nested_packages_not_ancestors_or_content() {
        let directory = tempfile::tempdir().expect("directory");
        let home = directory.path().join("home");
        let workspace = directory.path().join("ancestor/workspace");
        let roots = FixedSkillRoots::fixture(Some(&home), &workspace);
        package(&home.join(".zevria/skills/team/review"), "review");
        package(&workspace.join(".zevria/skills/team/build"), "build");
        package(
            &workspace.join(".zevria/skills/team/build/references/nested"),
            "not-a-skill",
        );
        package(&workspace.join(".agents/skills/wrong"), "wrong");
        package(
            &directory.path().join("ancestor/.zevria/skills/parent"),
            "parent",
        );
        package(&workspace.join(".zevria/skills/.hidden/hidden"), "hidden");
        let discovery = discover_skills(&roots);
        assert_eq!(
            discovery
                .selected
                .keys()
                .map(SkillName::as_str)
                .collect::<Vec<_>>(),
            ["build", "review"]
        );
        assert_eq!(discovery, discover_skills(&roots));
        let missing_home = discover_skills(&FixedSkillRoots::fixture(None, &workspace));
        assert_eq!(missing_home.selected.len(), 1);
        assert!(
            missing_home
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("HOME"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_fixed_root_aliases_deduplicate_and_outside_candidates_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().expect("directory");
        let home = directory.path().join("home");
        let workspace = directory.path().join("workspace");
        let roots = FixedSkillRoots::fixture(Some(&home), &workspace);
        package(&roots.project().join("team/review"), "review");
        package(&directory.path().join("outside"), "outside");
        symlink(
            directory.path().join("outside"),
            roots.project().join("outside"),
        )
        .expect("outside alias");
        symlink(roots.project().join("team"), roots.project().join("alias")).expect("inside alias");
        std::fs::create_dir_all(home.join(".zevria")).expect("home");
        symlink(roots.project(), roots.global().expect("global")).expect("shared native location");
        let discovery = discover_skills(&roots);
        assert_eq!(discovery.selected.len(), 1);
        assert_eq!(
            discovery
                .selected
                .get("review")
                .expect("review")
                .origin()
                .expect("origin")
                .provenance
                .scope,
            FixedSkillScope::Project
        );
        assert!(
            discovery
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("outside-root"))
        );
    }

    #[test]
    fn skill_invalid_policy_cannot_be_masked_by_an_affirmative_sidecar() {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(None, directory.path());
        let package = roots.project().join("review");
        std::fs::create_dir_all(package.join("agents")).expect("package");
        std::fs::write(
            package.join("SKILL.md"),
            "---\ndescription: Review\npolicy: invalid\n---\nBody",
        )
        .expect("invalid main policy");
        std::fs::write(
            package.join("agents/zevria.yaml"),
            "policy:\n  allow_implicit_invocation: true",
        )
        .expect("affirmative native override");
        let policy = || {
            discover_skills(&roots)
                .selected
                .get("review")
                .expect("candidate remains available for explicit selection")
                .metadata()
                .invocation_policy
        };
        assert_eq!(policy(), SkillInvocationPolicy::ExplicitOnly);
        std::fs::write(
            package.join("SKILL.md"),
            "---\ndescription: Review\n---\nBody",
        )
        .expect("valid main");
        std::fs::write(package.join("agents/openai.yaml"), "policy: [").expect("malformed sidecar");
        assert_eq!(policy(), SkillInvocationPolicy::ExplicitOnly);
        std::fs::write(
            package.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false",
        )
        .expect("valid lower restriction");
        assert_eq!(
            policy(),
            SkillInvocationPolicy::ModelAllowed,
            "valid explicitly present native overrides still merge normally"
        );
    }

    #[test]
    fn skill_discovery_candidate_and_diagnostic_limits_are_explicit() {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(None, directory.path());
        std::fs::create_dir_all(roots.project()).expect("root");
        for index in 0..=MAX_CANDIDATES {
            std::fs::write(
                roots.project().join(format!("bad-{index:04}.md")),
                "malformed",
            )
            .expect("candidate");
        }
        let discovery = discover_skills(&roots);
        assert_eq!(discovery.incomplete_scopes, [FixedSkillScope::Project]);
        assert!(discovery.selected.is_empty());
        assert_eq!(discovery.diagnostics.len(), 256);
        assert!(discovery.omitted_diagnostics > 0);
        assert!(
            discovery
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("candidate limit"))
        );
    }

    #[test]
    fn skill_aggregate_exhaustion_cannot_publish_a_partial_winner() {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(None, directory.path());
        std::fs::create_dir_all(roots.project()).expect("root");
        let source = format!(
            "---\ndescription: Bounded source\n---\n{}",
            "x".repeat(64_000)
        );
        for index in 0..530 {
            std::fs::write(
                roots.project().join(format!("skill-{index:04}.md")),
                &source,
            )
            .expect("candidate");
        }
        let discovery = discover_skills(&roots);
        assert_eq!(discovery.incomplete_scopes, [FixedSkillScope::Project]);
        assert!(discovery.selected.is_empty());
        assert!(
            discovery
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("aggregate"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_flat_alias_deduplication_preserves_the_manifest_name() {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(None, directory.path());
        std::fs::create_dir_all(roots.project()).expect("root");
        std::fs::write(
            roots.project().join("review.md"),
            "---\nname: review\ndescription: Review\n---\nBody",
        )
        .expect("manifest");
        std::os::unix::fs::symlink("review.md", roots.project().join("a-alias.md"))
            .expect("alias sorts first");
        let discovery = discover_skills(&roots);
        assert_eq!(discovery.selected.len(), 1);
        assert!(discovery.selected.contains_key("review"));
        assert_eq!(discovery.definitions.len(), 1);
    }

    #[test]
    fn skill_incomplete_project_scope_cannot_shadow_global() {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(
            Some(&directory.path().join("home")),
            &directory.path().join("workspace"),
        );
        package(&roots.global().expect("global").join("review"), "review");
        package(&roots.project().join("review"), "review");
        std::fs::create_dir_all(roots.project().join("a/b/c/d/e/f/g")).expect("too deep");
        let discovery = discover_skills(&roots);
        assert_eq!(discovery.incomplete_scopes, [FixedSkillScope::Project]);
        assert_eq!(
            discovery
                .selected
                .get("review")
                .expect("global fallback")
                .origin()
                .expect("origin")
                .provenance
                .scope,
            FixedSkillScope::Global
        );
    }
}
