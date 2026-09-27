//! Filesystem fixtures for exercising real transcript rewrite failures.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Obstructs replacement of one transcript without moving its parent directory.
///
/// The transcript is saved in a unique sibling backup directory and an empty
/// directory takes its filename. This blocks **rewrites**, not ordinary writes
/// through the writer's existing append handle: those still reach the backup.
/// It works with both newly opened handles and handles retained after a rewrite.
///
/// Keep this fixture alive until failure handling has finished. In particular,
/// provider fixtures must own it rather than leave it local to a completion
/// future that may return or be cancelled before core records the result.
/// Recovery tests should call [`Self::restore`] explicitly and check its result.
/// Drop makes a best-effort, non-panicking restoration; if that fails, the saved
/// transcript is left at [`Self::backup_path`], never automatically deleted.
#[derive(Debug)]
pub struct TranscriptRewriteBlocker {
    path: PathBuf,
    backup_path: PathBuf,
    backup_directory: Option<PathBuf>,
    obstructed: bool,
    saved: bool,
}

impl TranscriptRewriteBlocker {
    pub fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect transcript at {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_file(),
            "rewrite blocker requires a regular transcript file at {}",
            path.display()
        );
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let backup_directory = tempfile::Builder::new()
            .prefix(".transcript-rewrite-backup-")
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "failed to create transcript backup beside {}",
                    path.display()
                )
            })?;
        let backup_path = backup_directory.path().join("transcript.jsonl");
        std::fs::rename(&path, &backup_path).with_context(|| {
            format!(
                "failed to save transcript {} at {}",
                path.display(),
                backup_path.display()
            )
        })?;
        // From this point on, no TempDir destructor may discard the saved file.
        let mut blocker = Self {
            path,
            backup_path,
            backup_directory: Some(backup_directory.keep()),
            obstructed: false,
            saved: true,
        };
        if let Err(error) = std::fs::create_dir(&blocker.path) {
            let error = anyhow::Error::from(error).context(format!(
                "failed to block transcript replacement at {}",
                blocker.path.display()
            ));
            if let Err(restore_error) = blocker.restore() {
                return Err(error.context(format!(
                    "failed to roll back rewrite blocker; transcript saved at {}: {restore_error:#}",
                    blocker.backup_path.display()
                )));
            }
            return Err(error);
        }
        blocker.obstructed = true;
        Ok(blocker)
    }

    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }

    /// Remove only the empty obstruction and put the saved transcript back.
    /// Safe to repeat, including after a partially successful restoration.
    /// Refuses to delete unexpected contents or overwrite a new destination.
    pub fn restore(&mut self) -> anyhow::Result<()> {
        if self.obstructed {
            match std::fs::remove_dir(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::Error::from(error).context(format!(
                        "failed to remove transcript rewrite obstruction at {}",
                        self.path.display()
                    )));
                }
            }
            self.obstructed = false;
        }
        if self.saved {
            match std::fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                result => {
                    result.with_context(|| {
                        format!(
                            "failed to inspect restore destination {}",
                            self.path.display()
                        )
                    })?;
                    anyhow::bail!(
                        "refusing to overwrite restore destination {}; transcript saved at {}",
                        self.path.display(),
                        self.backup_path.display()
                    );
                }
            }
            std::fs::rename(&self.backup_path, &self.path).with_context(|| {
                format!(
                    "failed to restore transcript from {} to {}",
                    self.backup_path.display(),
                    self.path.display()
                )
            })?;
            self.saved = false;
        }
        if let Some(directory) = &self.backup_directory {
            std::fs::remove_dir(directory).with_context(|| {
                format!(
                    "failed to remove empty transcript backup at {}",
                    directory.display()
                )
            })?;
            self.backup_directory = None;
        }
        Ok(())
    }
}

impl Drop for TranscriptRewriteBlocker {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rig_core::message::Message;

    use super::*;
    use crate::{TranscriptItem, TranscriptWriter, transcript::load};

    fn entries(directory: &Path) -> BTreeSet<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    fn original_items() -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::SessionMode(crate::SessionMode::Build),
            TranscriptItem::Message(Message::user("durable request")),
        ]
    }

    fn exercise_writer(rewritten: bool) {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        let original = original_items();
        for item in &original {
            writer.append(item).unwrap();
        }
        if rewritten {
            writer.rewrite(&original).unwrap();
        }
        let path = writer.path().to_path_buf();
        let bytes = std::fs::read(&path).unwrap();
        let mut other = TranscriptWriter::create(directory.path()).unwrap();
        for item in &original {
            other.append(item).unwrap();
        }
        let other_bytes = std::fs::read(other.path()).unwrap();
        let unrelated = directory.path().join("unrelated.txt");
        std::fs::write(&unrelated, "leave me alone").unwrap();
        let before = entries(directory.path());

        let mut blocker = TranscriptRewriteBlocker::new(&path).unwrap();
        assert!(path.is_dir());
        assert!(entries(&path).is_empty());
        assert_eq!(std::fs::read(blocker.backup_path()).unwrap(), bytes);
        assert_eq!(load(blocker.backup_path()).unwrap(), original);
        let blocked_entries = entries(directory.path());
        let error = writer.rewrite(&original[..1]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to replace the session file"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(blocker.backup_path()).unwrap(), bytes);
        assert_eq!(load(blocker.backup_path()).unwrap(), original);
        assert_eq!(
            entries(directory.path()),
            blocked_entries,
            "no leaked stage"
        );
        assert_eq!(std::fs::read(other.path()).unwrap(), other_bytes);

        let appended = TranscriptItem::Message(Message::assistant("append still works"));
        writer.append(&appended).unwrap();
        let mut expected = original.clone();
        expected.push(appended.clone());
        assert_eq!(load(blocker.backup_path()).unwrap(), expected);
        assert!(
            std::fs::read(blocker.backup_path())
                .unwrap()
                .starts_with(&bytes)
        );
        other.rewrite(&original[..1]).unwrap();
        other.append(&appended).unwrap();
        assert_eq!(load(other.path()).unwrap(), [original[0].clone(), appended]);
        assert_eq!(entries(directory.path()), blocked_entries);
        assert_eq!(
            std::fs::read_to_string(&unrelated).unwrap(),
            "leave me alone"
        );

        blocker.restore().unwrap();
        blocker.restore().unwrap();
        assert!(!blocker.backup_path().exists());
        assert_eq!(load(&path).unwrap(), expected);
        assert_eq!(entries(directory.path()), before);
        writer.rewrite(&original).unwrap();
        let recovered = TranscriptItem::Message(Message::assistant("recovered"));
        writer.append(&recovered).unwrap();
        let mut expected = original;
        expected.push(recovered);
        assert_eq!(load(&path).unwrap(), expected);
        assert_eq!(
            entries(directory.path()),
            before,
            "no leaked stage or backup"
        );
    }

    #[test]
    fn blocker_preserves_a_fresh_writer_and_other_files() {
        exercise_writer(false);
    }

    #[test]
    fn blocker_preserves_a_rewritten_writer_and_other_files() {
        exercise_writer(true);
    }

    #[test]
    fn drop_restores_the_transcript_and_cleans_the_backup() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&original_items()).unwrap();
        let before = entries(directory.path());
        let bytes = std::fs::read(writer.path()).unwrap();
        let blocker = TranscriptRewriteBlocker::new(writer.path()).unwrap();
        let backup = blocker.backup_path().to_path_buf();
        drop(blocker);
        assert_eq!(std::fs::read(writer.path()).unwrap(), bytes);
        assert!(!backup.exists());
        assert_eq!(entries(directory.path()), before);
        writer
            .append(&TranscriptItem::Message(Message::assistant(
                "after cleanup",
            )))
            .unwrap();
        assert_eq!(load(writer.path()).unwrap().len(), 3);
    }

    #[test]
    fn failed_restore_can_be_retried_without_losing_the_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&original_items()).unwrap();
        let bytes = std::fs::read(writer.path()).unwrap();
        let before = entries(directory.path());
        let mut blocker = TranscriptRewriteBlocker::new(writer.path()).unwrap();
        let unexpected = writer.path().join("unexpected.txt");
        std::fs::write(&unexpected, "preserve this too").unwrap();
        assert!(blocker.restore().is_err());
        assert_eq!(std::fs::read(blocker.backup_path()).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(&unexpected).unwrap(),
            "preserve this too"
        );
        std::fs::remove_file(unexpected).unwrap();
        blocker.restore().unwrap();
        assert_eq!(std::fs::read(writer.path()).unwrap(), bytes);
        assert_eq!(entries(directory.path()), before);
    }

    #[test]
    fn partial_restore_refuses_to_overwrite_a_new_destination_and_can_be_retried() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&original_items()).unwrap();
        let bytes = std::fs::read(writer.path()).unwrap();
        let before = entries(directory.path());
        let mut blocker = TranscriptRewriteBlocker::new(writer.path()).unwrap();
        let held = blocker.backup_path().with_file_name("held.jsonl");
        std::fs::rename(blocker.backup_path(), &held).unwrap();
        let error = blocker.restore().unwrap_err();
        assert!(error.to_string().contains("failed to restore transcript"));
        assert!(!writer.path().exists(), "the empty obstruction was removed");
        assert_eq!(std::fs::read(&held).unwrap(), bytes);

        std::fs::rename(&held, blocker.backup_path()).unwrap();
        std::fs::write(writer.path(), "new destination").unwrap();
        let error = blocker.restore().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to overwrite restore destination")
        );
        assert_eq!(std::fs::read(blocker.backup_path()).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(writer.path()).unwrap(),
            "new destination"
        );
        std::fs::remove_file(writer.path()).unwrap();
        blocker.restore().unwrap();
        assert_eq!(std::fs::read(writer.path()).unwrap(), bytes);
        assert_eq!(entries(directory.path()), before);
    }

    #[test]
    fn failed_drop_preserves_the_backup_and_an_unexpected_destination() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&original_items()).unwrap();
        let bytes = std::fs::read(writer.path()).unwrap();
        let mut blocker = TranscriptRewriteBlocker::new(writer.path()).unwrap();
        let backup = blocker.backup_path().to_path_buf();
        std::fs::remove_dir(writer.path()).unwrap();
        std::fs::write(writer.path(), "unexpected replacement").unwrap();
        assert!(blocker.restore().is_err());
        drop(blocker);
        assert_eq!(std::fs::read(backup).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(writer.path()).unwrap(),
            "unexpected replacement"
        );
    }

    #[test]
    fn invalid_setup_leaves_the_parent_and_other_files_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let writer = TranscriptWriter::create(directory.path()).unwrap();
        let before = entries(directory.path());
        assert!(TranscriptRewriteBlocker::new(directory.path()).is_err());
        assert!(TranscriptRewriteBlocker::new(directory.path().join("missing.jsonl")).is_err());
        assert_eq!(entries(directory.path()), before);
        assert!(writer.path().is_file());
    }
}
