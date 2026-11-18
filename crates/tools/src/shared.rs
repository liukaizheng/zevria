use std::{
    fs::{self, Metadata},
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use zevria_foundation::{FileChange, FileChangeOperation, FileMutationScope};

use crate::FileToolError;

pub(crate) const MAX_EDIT_FILE_CHANGE_BYTES: usize = 512 * 1024;

#[derive(Debug)]
pub(crate) struct MutationPath {
    /// Absolute path with a canonical parent, but the original final entry.
    pub path: PathBuf,
    /// Existing file or symlink referent metadata. A dangling symlink is an error, not missing.
    pub metadata: Option<Metadata>,
}

impl MutationPath {
    pub(crate) fn regular_file(&self) -> Result<Option<&Metadata>, FileToolError> {
        match &self.metadata {
            Some(metadata) if !metadata.is_file() => {
                let kind = if metadata.is_dir() {
                    "a directory"
                } else {
                    "not a regular file"
                };
                Err(FileToolError::invalid_arguments(format!(
                    "{} is {kind}",
                    self.path.display()
                )))
            }
            metadata => Ok(metadata.as_ref()),
        }
    }

    pub(crate) fn require_regular_file(&self) -> Result<&Metadata, FileToolError> {
        self.regular_file()?.ok_or_else(|| {
            FileToolError::io(format!("file {} does not exist", self.path.display()))
        })
    }
}

pub(crate) fn resolve_path_for_write(
    workspace: &Path,
    path: &str,
    scope: FileMutationScope,
) -> Result<MutationPath, FileToolError> {
    if path.is_empty() {
        return Err(FileToolError::invalid_arguments(
            "file_path must not be empty",
        ));
    }
    let input = Path::new(path);
    // Path::file_name may discard a trailing separator or `.`. Do not turn a
    // directory-form argument into a mutation of the file before that suffix.
    if path.contains('\0')
        || matches!(
            path.rsplit(std::path::is_separator).next(),
            Some("" | "." | "..")
        )
    {
        return Err(FileToolError::invalid_arguments(format!(
            "expected a file path, got {path:?}"
        )));
    }
    // On Windows, drive-relative and rooted-but-not-absolute paths would
    // override part of the workspace instead of resolving relative to it.
    if !input.is_absolute()
        && (input.has_root() || matches!(input.components().next(), Some(Component::Prefix(_))))
    {
        return Err(FileToolError::invalid_arguments(format!(
            "expected an absolute or workspace-relative path, got {path:?}"
        )));
    }
    // Don't reinterpret an NTFS stream, a drive-like filename, or a slash
    // in a verbatim component when joining it to a canonical Windows path.
    #[cfg(windows)]
    if input.components().any(|part| {
        matches!(part, Component::Normal(name) if name.as_encoded_bytes().iter().any(|b| matches!(b, b':' | b'/')))
    }) {
        return Err(FileToolError::invalid_arguments(format!(
            "unsupported Windows file path component in {path:?}"
        )));
    }
    let parent = input.parent().ok_or_else(|| {
        FileToolError::path_resolution(format!("path {path:?} has no parent directory"))
    })?;
    let file_name = input
        .file_name()
        .ok_or_else(|| FileToolError::path_resolution(format!("path {path:?} has no file name")))?;
    let canonical_parent = resolve_parent(workspace, parent)?;
    let boundary = match scope {
        FileMutationScope::WorkspaceOnly => Some(canonicalize(workspace)?),
        FileMutationScope::ProcessFilesystem => None,
    };
    check_scope(boundary.as_deref(), &canonical_parent, input)?;

    let path = canonical_parent.join(file_name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_symlink() => {
            let referent = canonicalize(&path)?;
            check_scope(boundary.as_deref(), &referent, &path)?;
            Some(fs::metadata(referent).map_err(|error| inspection_error(&path, error))?)
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(inspection_error(&path, error)),
    };
    Ok(MutationPath { path, metadata })
}

fn resolve_parent(workspace: &Path, parent: &Path) -> Result<PathBuf, FileToolError> {
    // Windows path joining (for a verbatim workspace) and DOS path resolution
    // can cancel `link/..` lexically. Resolve the directory at each `..` first.
    #[cfg(windows)]
    if parent.components().any(|part| part == Component::ParentDir) {
        let mut resolved = if parent.is_absolute() {
            PathBuf::new()
        } else {
            workspace.to_path_buf()
        };
        for part in parent.components() {
            if part == Component::ParentDir {
                resolved = canonicalize_directory(&resolved)?;
                resolved.pop();
            } else {
                resolved.push(part.as_os_str());
            }
        }
        return canonicalize_directory(&resolved);
    }
    // On Unix the filesystem resolves `..` after following symlinks.
    canonicalize_directory(&workspace.join(parent))
}

fn canonicalize_directory(path: &Path) -> Result<PathBuf, FileToolError> {
    let resolved = canonicalize(path)?;
    if !fs::metadata(&resolved)
        .map_err(|error| inspection_error(&resolved, error))?
        .is_dir()
    {
        return Err(FileToolError::path_resolution(format!(
            "parent {} is not a directory",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn canonicalize(path: &Path) -> Result<PathBuf, FileToolError> {
    path.canonicalize().map_err(|error| {
        FileToolError::path_resolution(format!("failed to resolve {}: {error}", path.display()))
    })
}

fn check_scope(
    workspace: Option<&Path>,
    resolved: &Path,
    requested: &Path,
) -> Result<(), FileToolError> {
    if workspace.is_some_and(|workspace| !resolved.starts_with(workspace)) {
        return Err(FileToolError::path_resolution(format!(
            "path {} escapes the workspace",
            requested.display()
        )));
    }
    Ok(())
}

fn inspection_error(path: &Path, error: std::io::Error) -> FileToolError {
    FileToolError::io(format!("failed to inspect {}: {error}", path.display()))
}

pub(crate) fn diff_line_counts(unified_diff: &str) -> (usize, usize) {
    diffy::Patch::from_str(unified_diff)
        .map(|patch| {
            patch.hunks().iter().flat_map(diffy::Hunk::lines).fold(
                (0, 0),
                |(added, removed), line| match line {
                    diffy::Line::Insert(_) => (added + 1, removed),
                    diffy::Line::Delete(_) => (added, removed + 1),
                    diffy::Line::Context(_) => (added, removed),
                },
            )
        })
        .unwrap_or((0, 0))
}

/// Build one unified patch whose context spans both complete UTF-8 texts.
pub(crate) fn full_file_update_patch(original_content: &str, new_content: &str) -> String {
    if original_content == new_content {
        return String::new();
    }

    let context_len = original_content
        .split_inclusive('\n')
        .count()
        .max(new_content.split_inclusive('\n').count());
    let mut options = diffy::DiffOptions::new();
    options.set_context_len(context_len);
    options
        .create_patch(original_content, new_content)
        .to_string()
}

pub(crate) fn update_file_change(
    original_content: &str,
    new_content: &str,
    move_path: Option<PathBuf>,
    max_file_change_bytes: usize,
) -> FileChange {
    let unified_diff = if original_content == new_content {
        String::new()
    } else {
        diffy::create_patch(original_content, new_content).to_string()
    };
    let (added, removed) = diff_line_counts(&unified_diff);
    if unified_diff.len() > max_file_change_bytes {
        FileChange::Omitted {
            operation: FileChangeOperation::Update,
            reason: format!("diff omitted because it exceeds {max_file_change_bytes} bytes"),
            added,
            removed,
            bytes: unified_diff.len(),
        }
    } else {
        FileChange::Update {
            unified_diff,
            move_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolves_relative_and_absolute_paths_inside_workspace() {
        let workspace = TempDir::new().expect("workspace");
        std::fs::create_dir(workspace.path().join("sub")).expect("subdirectory");
        let root = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");

        assert_eq!(
            resolve_path_for_write(
                workspace.path(),
                "sub/file.txt",
                FileMutationScope::WorkspaceOnly
            )
            .expect("relative path")
            .path,
            root.join("sub/file.txt")
        );
        assert_eq!(
            resolve_path_for_write(
                workspace.path(),
                &root.join("file.txt").to_string_lossy(),
                FileMutationScope::WorkspaceOnly,
            )
            .expect("absolute path")
            .path,
            root.join("file.txt")
        );
    }

    #[test]
    fn rejects_traversal_and_missing_parents() {
        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        let outside_path = outside.path().join("file.txt");

        let error = resolve_path_for_write(
            workspace.path(),
            outside_path.to_str().expect("utf-8 temp path"),
            FileMutationScope::WorkspaceOnly,
        )
        .expect_err("outside path should fail");
        assert!(error.to_string().contains("escapes the workspace"));

        let error = resolve_path_for_write(
            workspace.path(),
            "missing/file.txt",
            FileMutationScope::ProcessFilesystem,
        )
        .expect_err("missing parent should fail");
        assert!(error.to_string().contains("failed to resolve"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_symlinks_outside_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "hello").expect("outside file");
        symlink(&outside_file, workspace.path().join("link.txt")).expect("symlink");

        let error = resolve_path_for_write(
            workspace.path(),
            "link.txt",
            FileMutationScope::WorkspaceOnly,
        )
        .expect_err("symlink escape should fail");
        assert!(error.to_string().contains("escapes the workspace"));
    }
}
