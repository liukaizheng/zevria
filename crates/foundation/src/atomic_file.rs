//! Same-directory staged file replacement shared by persistence and tools.

use std::{io::Write as _, path::Path};

use anyhow::Context as _;

/// Atomically replace `path` with `contents` after fully writing and syncing a
/// unique sibling temporary file. Existing permissions are preserved.
pub fn replace(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a staged file beside {}", path.display()))?;
    if let Ok(metadata) = std::fs::metadata(path) {
        staged
            .as_file()
            .set_permissions(metadata.permissions())
            .with_context(|| format!("failed to preserve permissions for {}", path.display()))?;
    }
    staged
        .write_all(contents)
        .and_then(|()| staged.flush())
        .and_then(|()| staged.as_file().sync_all())
        .with_context(|| format!("failed to write the staged file for {}", path.display()))?;
    staged.persist(path).map_err(|error| {
        anyhow::Error::from(error.error)
            .context(format!("failed to replace {} atomically", path.display()))
    })?;
    if let Err(error) = sync_directory(parent) {
        tracing::warn!(
            target: "zevria_core::atomic_file", path = %path.display(),
            %error,
            "file replacement committed but the parent directory could not be synced"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_contents_and_preserves_permissions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("file.txt");
        std::fs::write(&path, "before").expect("initial write");
        let permissions = std::fs::metadata(&path).expect("metadata").permissions();

        replace(&path, b"after").expect("atomic replacement");

        assert_eq!(std::fs::read_to_string(&path).expect("contents"), "after");
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").permissions(),
            permissions
        );
    }
}
