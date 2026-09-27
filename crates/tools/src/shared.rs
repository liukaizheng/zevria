use std::path::{Path, PathBuf};

use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOperation;

use crate::FileToolError;

pub(crate) const MAX_EDIT_FILE_CHANGE_BYTES: usize = 512 * 1024;

pub(crate) fn resolve_path_for_write(
    workspace: &Path,
    path: &str,
) -> Result<PathBuf, FileToolError> {
    if path.is_empty() {
        return Err(FileToolError::invalid_arguments(
            "file_path must not be empty",
        ));
    }
    let canonical_workspace = workspace.canonicalize().map_err(|error| {
        FileToolError::path_resolution(format!(
            "failed to resolve {}: {error}",
            workspace.display()
        ))
    })?;
    let joined = workspace.join(path);
    let parent = joined.parent().ok_or_else(|| {
        FileToolError::path_resolution(format!("path {} has no parent directory", joined.display()))
    })?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        FileToolError::path_resolution(format!("failed to resolve {}: {error}", parent.display()))
    })?;
    if !canonical_parent.starts_with(&canonical_workspace) {
        return Err(FileToolError::path_resolution(format!(
            "path {} escapes the workspace",
            joined.display()
        )));
    }
    let file_name = joined.file_name().ok_or_else(|| {
        FileToolError::path_resolution(format!("path {} has no file name", joined.display()))
    })?;
    let resolved_path = canonical_parent.join(file_name);

    if let Ok(metadata) = resolved_path.symlink_metadata()
        && metadata.file_type().is_symlink()
    {
        let canonical_path = resolved_path.canonicalize().map_err(|error| {
            FileToolError::path_resolution(format!(
                "failed to resolve {}: {error}",
                resolved_path.display()
            ))
        })?;
        if !canonical_path.starts_with(&canonical_workspace) {
            return Err(FileToolError::path_resolution(format!(
                "path {} escapes the workspace",
                resolved_path.display()
            )));
        }
    }

    Ok(resolved_path)
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
            resolve_path_for_write(workspace.path(), "sub/file.txt").expect("relative path"),
            root.join("sub/file.txt")
        );
        assert_eq!(
            resolve_path_for_write(workspace.path(), &root.join("file.txt").to_string_lossy())
                .expect("absolute path"),
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
        )
        .expect_err("outside path should fail");
        assert!(error.to_string().contains("escapes the workspace"));

        let error = resolve_path_for_write(workspace.path(), "missing/file.txt")
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

        let error = resolve_path_for_write(workspace.path(), "link.txt")
            .expect_err("symlink escape should fail");
        assert!(error.to_string().contains("escapes the workspace"));
    }
}
