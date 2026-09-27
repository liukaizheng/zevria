//! Frozen fallback source selected exclusively from validated native tool evidence.
use super::*;
use zevria_foundation::contained_read::{
    FileSnapshot, OpenedRoot, RelativePath, file_snapshot, read_regular,
};

pub(super) struct ArtifactSnapshot {
    pub(super) path: PathBuf,
    pub(super) artifact_tool_id: String,
    pub(super) evidence: ArtifactEvidence,
    pub(super) artifact_directory: PathBuf,
    pub(super) workspace_artifact_directory: PathBuf,
    pub(super) workspace: PathBuf,
}

impl ArtifactSnapshot {
    pub(super) fn read(self) -> Result<(String, NativePlanSource), String> {
        self.read_inner_with(|| {}).map_err(|error| {
            format!(
                "unsafe or unavailable Claude plan artifact {}: {error}",
                self.path.display()
            )
        })
    }

    fn read_inner_with(
        &self,
        after_read: impl FnOnce(),
    ) -> Result<(String, NativePlanSource), String> {
        let validate = || {
            validate_claude_plan_artifact_path(
                &self.path,
                &self.artifact_directory,
                &self.workspace_artifact_directory,
                &self.workspace,
            )
        };
        if validate()? != self.path {
            return Err("artifact path changed".into());
        }
        let directory = self.path.parent().ok_or("artifact has no directory")?;
        let root = OpenedRoot::open_absolute(directory).map_err(|error| error.to_string())?;
        let directory_identity = root.identity().map_err(|error| error.to_string())?;
        if self.evidence.directory_identity.as_ref() != Some(&directory_identity) {
            return Err("artifact directory was replaced or has no validated identity".into());
        }
        let relative = RelativePath::new(Path::new(
            self.path.file_name().ok_or("artifact has no filename")?,
        ))
        .map_err(|error| error.to_string())?;
        let file = root
            .open_file(&relative)
            .map_err(|error| error.to_string())?;
        let before = file_snapshot(&file).map_err(|error| error.to_string())?;
        require_single_link(&before)?;
        let observed = self
            .evidence
            .file_version
            .as_ref()
            .ok_or("artifact had no regular-file identity at terminal observation")?;
        if !observed.same_version(&before) {
            return Err("artifact was replaced or changed after its successful mutation".into());
        }
        let bytes = read_regular(file, zevria_workflow::MAX_PLAN_ARTIFACT_BYTES)
            .map_err(|error| error.to_string())?;
        after_read();
        // Reopen relative to the pinned handle: replacement cannot silently alter
        // the snapshot. Also revalidate the directory's ambient binding afterward.
        let after = file_snapshot(
            &root
                .open_file(&relative)
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        require_single_link(&after)?;
        if !before.same_version(&after) {
            return Err("artifact was replaced or changed while reading".into());
        }
        validate()?;
        let rebound = OpenedRoot::open_absolute(directory).map_err(|error| error.to_string())?;
        if rebound.identity().map_err(|error| error.to_string())? != directory_identity {
            return Err("artifact directory was replaced while reading".into());
        }
        let markdown = String::from_utf8(bytes).map_err(|_| "artifact is not UTF-8".to_string())?;
        if markdown.trim().is_empty() {
            return Err("artifact Markdown is empty".into());
        }
        if self
            .evidence
            .whole_file_content
            .as_ref()
            .is_some_and(|expected| expected != &markdown)
        {
            return Err(
                "artifact snapshot disagrees with the latest complete-content evidence".into(),
            );
        }
        let source = NativePlanSource::Artifact {
            artifact_tool_id: self.artifact_tool_id.clone(),
            path: self.path.clone(),
            content_digest: NativePlanCapture::content_digest(markdown.as_bytes()),
        };
        Ok((markdown, source))
    }
}

fn require_single_link(metadata: &FileSnapshot) -> Result<(), String> {
    if !metadata.metadata.is_file() || metadata.links != 1 {
        return Err("artifact must be an unlinked-to regular file (one hard link)".into());
    }
    Ok(())
}

pub(super) fn artifact_file_snapshot(path: &Path) -> Option<FileSnapshot> {
    let root = OpenedRoot::open_absolute(path.parent()?).ok()?;
    let relative = RelativePath::new(Path::new(path.file_name()?)).ok()?;
    file_snapshot(&root.open_file(&relative).ok()?).ok()
}

#[cfg(test)]
#[path = "artifact_snapshot_tests.rs"]
mod tests;

pub(super) fn extract_handoff_path_hint(
    raw_input: Option<&serde_json::Value>,
) -> Result<Option<PathBuf>, String> {
    raw_input
        .and_then(|input| input.get("planFilePath"))
        .map(|path| {
            path.as_str()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| "Claude ExitPlanMode planFilePath must be a nonempty string".into())
        })
        .transpose()
}
