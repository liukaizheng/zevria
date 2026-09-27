//! Opening/resume-only supplemental file guidance. Discovery is exactly the
//! global Zevria directory and the supplied startup workspace, never a search.
use std::path::{Path, PathBuf};

use zevria_foundation::contained_read::{OpenedRoot, ReadError, RelativePath, read_regular};
#[cfg(not(windows))]
use zevria_foundation::contained_read::{metadata_identity, same_version};

pub const GUIDANCE_FILE_NAME: &str = "AGENTS.md";
pub const MAX_GUIDANCE_BYTES: usize = 128 * 1024;
pub const GLOBAL_GUIDANCE_COMPONENT: &str = "guidance:global";
pub const PROJECT_GUIDANCE_COMPONENT: &str = "guidance:project";
const MAX_DIAGNOSTIC_PATH_CHARS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceScope {
    Global,
    Project,
}
impl GuidanceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceErrorCategory {
    GlobalUnavailable,
    Io,
    UnsafePath,
    NonRegular,
    Oversized,
    Changed,
    InvalidUtf8,
    UnsupportedPlatform,
}
impl GuidanceErrorCategory {
    fn message(self) -> &'static str {
        match self {
            Self::GlobalUnavailable => "user home is unavailable; cannot locate ~/.zevria",
            Self::Io => "file or owning root could not be safely accessed or resolved",
            Self::UnsafePath => "target escapes its owning root or has an unsafe relative path",
            Self::NonRegular => "target is not a regular file",
            Self::Oversized => "raw file exceeds the 128 KiB limit (no truncation)",
            Self::Changed => "file or root changed during resolution, opening, or reading",
            Self::InvalidUtf8 => "file is not valid UTF-8",
            Self::UnsupportedPlatform => {
                "safe handle-relative reads are unavailable on this platform"
            }
        }
    }
}
impl From<ReadError> for GuidanceErrorCategory {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Io(_) => Self::Io,
            ReadError::InvalidPath => Self::UnsafePath,
            ReadError::NonRegular => Self::NonRegular,
            ReadError::Oversized { .. } => Self::Oversized,
            ReadError::Changed => Self::Changed,
            #[cfg(not(unix))]
            ReadError::Unsupported => Self::UnsupportedPlatform,
        }
    }
}
impl From<std::io::Error> for GuidanceErrorCategory {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// At most one diagnostic per scope. Paths are escaped, display-only and
/// bounded; messages are fixed text and never include file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceDiagnostic {
    pub scope: GuidanceScope,
    pub source: Option<String>,
    pub category: GuidanceErrorCategory,
    pub message: String,
}
impl GuidanceDiagnostic {
    fn new(scope: GuidanceScope, source: Option<&Path>, category: GuidanceErrorCategory) -> Self {
        Self {
            scope,
            source: source.map(|path| {
                escaped_path(path)
                    .chars()
                    .take(MAX_DIAGNOSTIC_PATH_CHARS)
                    .collect()
            }),
            category,
            message: category.message().into(),
        }
    }
}
impl std::fmt::Display for GuidanceDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Skipped {} AGENTS.md guidance", self.scope.as_str())?;
        if let Some(source) = &self.source {
            write!(f, " at {source}")?;
        }
        write!(
            f,
            ": {}. Previous guidance from this scope is not retained; repair it and open/resume another session to capture it.",
            self.message
        )
    }
}

/// Fixed locations, not configurable search roots. ZEVRIA_CONFIG is irrelevant.
#[derive(Debug, Clone)]
pub struct GuidanceRoots {
    global: Option<PathBuf>,
    project: PathBuf,
}
impl GuidanceRoots {
    pub fn capture(startup_workspace: &Path) -> Self {
        Self {
            global: crate::config::zevria_dir().ok(),
            project: startup_workspace.to_path_buf(),
        }
    }
    /// Tests never change process HOME or read the developer's global guidance.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture(global: Option<&Path>, workspace: &Path) -> Self {
        Self {
            global: global.map(Path::to_path_buf),
            project: workspace.to_path_buf(),
        }
    }
}

/// Both desired components are always present, even when exactly empty. An
/// engine's None snapshot, unlike this default, means no guidance integration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuidanceSnapshot {
    global: String,
    project: String,
    diagnostics: Vec<GuidanceDiagnostic>,
}
impl GuidanceSnapshot {
    pub fn components(&self) -> [(&str, &str); 2] {
        [
            (GLOBAL_GUIDANCE_COMPONENT, &self.global),
            (PROJECT_GUIDANCE_COMPONENT, &self.project),
        ]
    }
    pub fn diagnostics(&self) -> &[GuidanceDiagnostic] {
        &self.diagnostics
    }
    pub fn take_diagnostics(&mut self) -> Vec<GuidanceDiagnostic> {
        std::mem::take(&mut self.diagnostics)
    }
}

pub fn load_guidance(roots: &GuidanceRoots) -> GuidanceSnapshot {
    let mut snapshot = GuidanceSnapshot::default();
    for (scope, root, value) in [
        (
            GuidanceScope::Global,
            roots.global.as_deref(),
            &mut snapshot.global,
        ),
        (
            GuidanceScope::Project,
            Some(roots.project.as_path()),
            &mut snapshot.project,
        ),
    ] {
        let Some(root) = root else {
            snapshot.diagnostics.push(GuidanceDiagnostic::new(
                scope,
                None,
                GuidanceErrorCategory::GlobalUnavailable,
            ));
            continue;
        };
        let source = root.join(GUIDANCE_FILE_NAME);
        match read_source(root, &source, || {}) {
            Ok(Some(body)) if !body.is_empty() => *value = render(scope, &source, &body),
            Ok(_) => {}
            Err(category) => {
                snapshot
                    .diagnostics
                    .push(GuidanceDiagnostic::new(scope, Some(&source), category))
            }
        }
    }
    snapshot
}

#[cfg(not(windows))]
fn read_source(
    root: &Path,
    source: &Path,
    after_resolution: impl FnOnce(),
) -> Result<Option<String>, GuidanceErrorCategory> {
    // lstat distinguishes normal absence from a present dangling/cyclic alias.
    let entry = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !cfg!(unix) {
        return Err(GuidanceErrorCategory::UnsupportedPlatform);
    }
    let canonical_root = std::fs::canonicalize(root)?;
    let root_metadata = std::fs::metadata(&canonical_root)?;
    let target = std::fs::canonicalize(source)?;
    let relative = target
        .strip_prefix(&canonical_root)
        .map_err(|_| GuidanceErrorCategory::UnsafePath)?;
    let relative = RelativePath::new(relative)?;
    let resolved = std::fs::metadata(&target)?;
    if !resolved.is_file() {
        return Err(GuidanceErrorCategory::NonRegular);
    }
    if !entry.file_type().is_symlink() && !same_version(&entry, &resolved)? {
        return Err(GuidanceErrorCategory::Changed);
    }
    after_resolution();
    let opened = OpenedRoot::open(&canonical_root)?;
    if metadata_identity(&root_metadata)? != metadata_identity(&opened.metadata()?)? {
        return Err(GuidanceErrorCategory::Changed);
    }
    let file = opened.open_file(&relative)?;
    if !same_version(&resolved, &file.metadata()?)? {
        return Err(GuidanceErrorCategory::Changed);
    }
    let bytes = read_regular(file, MAX_GUIDANCE_BYTES)?;
    // Detect observable alias/root/target replacements as well as descriptor
    // changes. These checks are not a fully transactional filesystem snapshot.
    if !same_version(&entry, &std::fs::symlink_metadata(source)?)?
        || !same_version(&resolved, &std::fs::metadata(&target)?)?
        || std::fs::canonicalize(source)? != target
        || std::fs::canonicalize(root)? != canonical_root
        || metadata_identity(&root_metadata)?
            != metadata_identity(&std::fs::metadata(&canonical_root)?)?
    {
        return Err(GuidanceErrorCategory::Changed);
    }
    let text = String::from_utf8(bytes).map_err(|_| GuidanceErrorCategory::InvalidUtf8)?;
    Ok(Some(text.trim_end().to_string()))
}

#[cfg(windows)]
fn read_source(
    root: &Path,
    source: &Path,
    after_resolution: impl FnOnce(),
) -> Result<Option<String>, GuidanceErrorCategory> {
    use zevria_foundation::contained_read::file_snapshot;
    let opened = match OpenedRoot::open_absolute(root) {
        Ok(root) => root,
        Err(ReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let identity = opened.identity()?;
    let relative = RelativePath::new(
        source
            .strip_prefix(root)
            .map_err(|_| GuidanceErrorCategory::UnsafePath)?,
    )?;
    let file = match opened.open_file(&relative) {
        Ok(file) => file,
        Err(ReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let before = file_snapshot(&file)?;
    after_resolution();
    let bytes = read_regular(file, MAX_GUIDANCE_BYTES)?;
    let rebound = OpenedRoot::open_absolute(root)?;
    if rebound.identity()? != identity
        || !before.same_version(&file_snapshot(&rebound.open_file(&relative)?)?)
    {
        return Err(GuidanceErrorCategory::Changed);
    }
    let text = String::from_utf8(bytes).map_err(|_| GuidanceErrorCategory::InvalidUtf8)?;
    Ok(Some(text.trim_end().to_string()))
}

fn escaped_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .chars()
            .flat_map(char::escape_debug)
            .collect::<String>()
    )
}
fn render(scope: GuidanceScope, source: &Path, body: &str) -> String {
    format!(
        "Scope: {}\nSource: {}\n--- BEGIN USER-CONTROLLED AGENTS.md BODY ---\n{body}\n--- END USER-CONTROLLED AGENTS.md BODY ---",
        scope.as_str(),
        escaped_path(source)
    )
}

#[cfg(test)]
#[path = "guidance_tests.rs"]
mod tests;
