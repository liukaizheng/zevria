//! Transcript writer responsibilities.
use super::header::session_mode;
use super::header::validate_header;
use super::item::MAX_ROOT_RECORD_BYTES;
use super::item::TranscriptItem;
use super::load::TranscriptLoadOutcome;
use super::load::load;
use super::load::load_report;
use super::projection::is_conversation_item;
use super::sessions::pick_session_id;
use anyhow::Context as _;
use serde::Serialize;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// Storage for a session's intended `{id}.jsonl` path. Newly allocated logs
/// remain pending until Conversation commits substantive conversational input.
pub struct TranscriptWriter {
    session_id: String,
    pub(super) file: TranscriptFile,
    path: PathBuf,
    recovered_malformed_lines: usize,
    pub(super) needs_newline: bool,
}

pub(super) enum TranscriptFile {
    Pending,
    Writable(std::fs::File),
    ReadOnly(std::fs::File),
}

impl TranscriptFile {
    fn handle(&self) -> Option<&std::fs::File> {
        match self {
            Self::Pending => None,
            Self::Writable(file) | Self::ReadOnly(file) => Some(file),
        }
    }
}

/// A failed append is rolled back before returning. If the storage device
/// also rejects rollback, no code can promise which bytes survived: expose
/// that ambiguity rather than claiming the required operation was uncommitted.
pub(super) fn append_or_rollback(
    append: impl FnOnce() -> std::io::Result<()>,
    rollback: impl FnOnce() -> std::io::Result<()>,
) -> anyhow::Result<()> {
    if let Err(error) = append() {
        if let Err(rollback) = rollback() {
            tracing::warn!(%error, %rollback, "transcript append durability is indeterminate");
            return Err(anyhow::Error::new(error).context(format!(
                "transcript append durability is indeterminate: rollback failed ({rollback}); inspect the session before resuming"
            )));
        }
        return Err(error.into());
    }
    Ok(())
}

impl TranscriptWriter {
    /// Allocate a fresh ID and intended path without creating a transcript.
    /// Directory and lease activity is independent of transcript persistence.
    pub fn allocate(sessions_dir: &Path) -> anyhow::Result<Self> {
        for _ in 0..8 {
            if let Some(writer) = Self::try_allocate_with_id(sessions_dir, &pick_session_id())? {
                return Ok(writer);
            }
        }
        anyhow::bail!(
            "failed to pick an unused session id in {}",
            sessions_dir.display()
        )
    }

    /// Allocate an already-chosen ID. Publication checks for collisions again;
    /// allocation does not reserve the filename or overwrite existing history.
    pub fn allocate_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        Self::try_allocate_with_id(sessions_dir, session_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "a session file for {session_id} already exists in {}",
                sessions_dir.display()
            )
        })
    }

    fn try_allocate_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(sessions_dir).with_context(|| {
            format!(
                "failed to create the sessions directory at {}",
                sessions_dir.display()
            )
        })?;
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(Self {
                session_id: session_id.to_string(),
                file: TranscriptFile::Pending,
                path,
                recovered_malformed_lines: 0,
                needs_newline: false,
            })),
            Err(error) => Err(anyhow::Error::from(error).context(format!(
                "failed to inspect the intended session file at {}",
                path.display()
            ))),
        }
    }

    /// Explicitly construct a persisted empty log (e.g. a historical fixture).
    /// Production new sessions must use [`Self::allocate`] instead.
    pub fn create(sessions_dir: &Path) -> anyhow::Result<Self> {
        // A v4 collision is practically impossible, but `create_new` keeps a
        // collision from silently appending to another session's file.
        for _ in 0..8 {
            if let Some(writer) = Self::try_create_with_id(sessions_dir, &pick_session_id())? {
                return Ok(writer);
            }
        }

        Err(anyhow::anyhow!(
            "failed to pick an unused session id in {}",
            sessions_dir.display()
        ))
    }

    /// Create the transcript file for an already-chosen session id, refusing
    /// to touch a file that exists.
    pub fn create_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        Self::try_create_with_id(sessions_dir, session_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "a session file for {session_id} already exists in {}",
                sessions_dir.display()
            )
        })
    }

    /// `Ok(None)` means a file for this id already exists.
    fn try_create_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(sessions_dir).with_context(|| {
            format!(
                "failed to create the sessions directory at {}",
                sessions_dir.display()
            )
        })?;

        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => Ok(Some(Self {
                session_id: session_id.to_string(),
                file: TranscriptFile::Writable(file),
                path,
                recovered_malformed_lines: 0,
                needs_newline: false,
            })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(anyhow::Error::from(error).context(format!(
                "failed to create the session file at {}",
                path.display()
            ))),
        }
    }

    /// Reopen an existing session file to keep appending to it.
    pub fn append_to(path: PathBuf) -> anyhow::Result<Self> {
        // Validate the complete record boundary and lifecycle before acquiring
        // any writable handle. In particular, never erase unsupported records.
        let outcome = load_report(&path)?;
        Self::append_loaded(&outcome)
    }

    /// Reopen the exact report already preflighted by startup; no second load.
    pub fn append_loaded(outcome: &TranscriptLoadOutcome) -> anyhow::Result<Self> {
        outcome.ensure_resumable()?;
        let mut writer = Self::open_existing(outcome.path.clone(), false, outcome.needs_newline)?;
        if outcome.recoverable_lines > 0 {
            let offset = outcome
                .recoverable_tail_offset
                .context("recoverable tail has no verified byte boundary")?;
            let file = writer
                .file
                .handle()
                .context("tail repair requires a materialized transcript")?;
            // Preserve complete physical records verbatim, including committed
            // batch envelopes. Re-encoding flattened items would rewrite the
            // whole prefix and lose its original commit boundaries.
            file.set_len(offset)
                .and_then(|()| file.sync_data())
                .context("failed to durably truncate the incomplete transcript tail")?;
            writer.needs_newline = false;
            writer.recovered_malformed_lines = outcome.recoverable_lines;
        }
        Ok(writer)
    }

    /// Open only for inspection. Every mutation path fails, including repair
    /// and atomic replacement; the original bytes are never normalized.
    pub fn read_only(path: PathBuf) -> anyhow::Result<Self> {
        load_report(&path)?;
        Self::open_existing(path, true, false)
    }

    fn open_existing(path: PathBuf, read_only: bool, needs_newline: bool) -> anyhow::Result<Self> {
        let session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("the session file {} has no readable name", path.display()))?
            .to_string();
        // Append-only handles cannot resize files on Windows. Writable
        // handles must support tail repair and rollback; append_records
        // positions every write at EOF without relying on append mode.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(&path)
            .with_context(|| format!("failed to open the session file at {}", path.display()))?;
        Ok(Self {
            session_id,
            file: if read_only {
                TranscriptFile::ReadOnly(file)
            } else {
                TranscriptFile::Writable(file)
            },
            path,
            recovered_malformed_lines: 0,
            needs_newline,
        })
    }

    pub const fn is_pending(&self) -> bool {
        matches!(self.file, TranscriptFile::Pending)
    }

    pub const fn is_read_only(&self) -> bool {
        matches!(self.file, TranscriptFile::ReadOnly(_))
    }

    pub(super) fn ensure_writable(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.is_read_only(),
            "the session transcript was opened read-only; mutations and repair are disabled"
        );
        Ok(())
    }

    /// Append an item as a JSON line and flush it to disk.
    pub fn append(&mut self, item: &TranscriptItem) -> anyhow::Result<()> {
        self.ensure_writable()?;
        let file = self
            .file
            .handle()
            .context("pending transcripts must be mutated through Conversation")?;
        if matches!(item, TranscriptItem::SessionModels(_)) {
            anyhow::ensure!(
                file.metadata()?.len() == 0,
                "session models must be a unique first record; replace the canonical header instead"
            );
        }
        if matches!(item, TranscriptItem::SessionMode(_)) {
            let mut proposed = load(&self.path)?;
            proposed.push(item.clone());
            session_mode(&proposed)?;
        }
        self.persist_records(std::slice::from_ref(item), true)
    }

    // Only public checked append and the private validated transaction call this.
    pub(super) fn append_record(&mut self, item: &TranscriptItem) -> anyhow::Result<()> {
        self.append_records(std::slice::from_ref(item), false)
    }

    pub(super) fn append_records(
        &mut self,
        items: &[TranscriptItem],
        batch: bool,
    ) -> anyhow::Result<()> {
        self.ensure_writable()?;
        if items.is_empty() {
            return Ok(());
        }
        if batch {
            anyhow::ensure!(
                std::fs::metadata(&self.path).is_ok_and(|metadata| metadata.is_file()),
                "the canonical session transcript is missing or is not a regular file"
            );
        }
        let mut line = if items.len() == 1 {
            serialize_record(&items[0])?
        } else {
            // Serialize only the suffix, with the same bound as any physical
            // record. A torn final line exposes none of this batch on reload.
            #[derive(Serialize)]
            struct BatchRef<'a> {
                version: u32,
                records: &'a [TranscriptItem],
            }
            #[derive(Serialize)]
            struct EnvelopeRef<'a> {
                zevria_batch: BatchRef<'a>,
            }
            let mut bytes = serde_json::to_vec(&EnvelopeRef {
                zevria_batch: BatchRef {
                    version: 1,
                    records: items,
                },
            })?;
            anyhow::ensure!(
                bytes.len() < MAX_ROOT_RECORD_BYTES,
                "root transcript record exceeds the 64 MiB limit"
            );
            bytes.push(b'\n');
            bytes
        };
        if self.needs_newline {
            line.insert(0, b'\n');
        }
        let TranscriptFile::Writable(file) = &mut self.file else {
            anyhow::bail!("appending requires a materialized writable transcript");
        };
        let original_len = file.metadata()?.len();
        // Writable handles retain resize rights rather than using append-only
        // access. Seek also restores the cursor after a failed append rollback.
        use std::io::{Seek as _, SeekFrom};
        file.seek(SeekFrom::End(0))?;
        append_or_rollback(
            || (&*file).write_all(&line).and_then(|()| file.sync_data()),
            || file.set_len(original_len).and_then(|()| file.sync_data()),
        )
        .with_context(|| {
            format!(
                "failed to append to the session file at {}",
                self.path.display()
            )
        })?;
        self.needs_newline = false;
        Ok(())
    }

    /// Replace the file's contents with these items, keeping the session id.
    /// Used when an edited message rewrites the conversation.
    ///
    /// The replacement is staged beside the session file and renamed over it,
    /// so a failure — or a crash — leaves the original log whole rather than
    /// half-rewritten. Callers may therefore treat an error as "nothing
    /// changed on disk".
    pub fn rewrite(&mut self, items: &[TranscriptItem]) -> anyhow::Result<()> {
        self.ensure_writable()?;
        anyhow::ensure!(
            !self.is_pending(),
            "pending transcripts must be mutated through Conversation"
        );
        validate_header(items.iter())?;
        self.persist_records(items, false)
    }

    /// Raw mutation paths preserve their existing header checks. Pending
    /// proposals additionally need lifecycle validation before memory-only
    /// acceptance. Prepared anchored proposals already validated exactly once.
    pub(super) fn persist_unprepared(
        &mut self,
        items: &[TranscriptItem],
        single_append: bool,
    ) -> anyhow::Result<()> {
        if self.is_pending() {
            crate::validate_session_replay(items)?;
        } else {
            validate_header(items.iter())?;
        }
        self.persist_records(items, single_append)
    }

    /// The one storage decision for all Conversation mutations and repair.
    /// Pending metadata stays exclusively in Conversation.items. The first
    /// substantive proposal publishes the complete sequence, never just a tail.
    pub(super) fn persist_records(
        &mut self,
        items: &[TranscriptItem],
        single_append: bool,
    ) -> anyhow::Result<()> {
        self.ensure_writable()?;
        if self.is_pending() {
            if items.iter().any(is_conversation_item) {
                self.publish_records(items)?;
            } else {
                // Memory-only acceptance must not bypass serialization or the
                // same per-record bounds enforced by durable writes.
                for item in items {
                    serialize_record(item)?;
                }
            }
            Ok(())
        } else if single_append {
            self.append_record(items.last().context("missing appended item")?)
        } else {
            self.rewrite_records(items)
        }
    }

    fn publish_records(&mut self, items: &[TranscriptItem]) -> anyhow::Result<()> {
        let parent = self
            .path
            .parent()
            .context("session file has no parent directory")?;
        let mut staged = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to stage the initial session file for {}",
                self.path.display()
            )
        })?;
        write_items(staged.as_file_mut(), &self.path, items)?;
        staged.as_file().sync_all().with_context(|| {
            format!(
                "failed to sync the initial session file for {}",
                self.path.display()
            )
        })?;
        // No-clobber publication returns the already-open staged handle. An
        // allocation/publication race cannot replace a different history, and
        // there is no fallible reopen after the canonical file becomes visible.
        let file = staged
            .persist_noclobber(&self.path)
            .map_err(|error| error.error)
            .with_context(|| {
                format!(
                    "failed to publish the initial session file at {}",
                    self.path.display()
                )
            })?;
        self.file = TranscriptFile::Writable(file);
        self.needs_newline = false;
        if let Err(error) = sync_directory(parent) {
            tracing::warn!(
                target: "zevria_core::transcript", path = %self.path.display(), %error,
                "initial session publication committed but the parent directory could not be synced"
            );
        }
        Ok(())
    }

    // Private persistence primitive: callers either validate raw headers above,
    // or hold the exact immutable PreparedChange validated across all domains.
    fn rewrite_records(&mut self, items: &[TranscriptItem]) -> anyhow::Result<()> {
        let parent = self.path.parent().with_context(|| {
            format!(
                "session file {} has no parent directory",
                self.path.display()
            )
        })?;
        let mut staged = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to create a staged session file beside {}",
                self.path.display()
            )
        })?;
        if let Some(metadata) = self.file.handle().and_then(|file| file.metadata().ok()) {
            staged
                .as_file()
                .set_permissions(metadata.permissions())
                .with_context(|| {
                    format!("failed to preserve permissions for {}", self.path.display())
                })?;
        }
        write_items(staged.as_file_mut(), &self.path, items)?;
        staged.as_file().sync_all().with_context(|| {
            format!(
                "failed to sync the staged session file for {}",
                self.path.display()
            )
        })?;

        // Keep the staged handle open through the rename. Once the rename
        // succeeds this very handle names the replacement file, so there is
        // no fallible reopen step after the on-disk commit point.
        let (staged_file, staged_path) =
            staged
                .keep()
                .map_err(|error| error.error)
                .with_context(|| {
                    format!(
                        "failed to retain the staged session file for {}",
                        self.path.display()
                    )
                })?;
        if let Err(error) = std::fs::rename(&staged_path, &self.path) {
            let _ = std::fs::remove_file(&staged_path);
            return Err(anyhow::Error::from(error).context(format!(
                "failed to replace the session file at {}",
                self.path.display()
            )));
        }
        self.file = TranscriptFile::Writable(staged_file);
        self.needs_newline = false;
        if let Err(error) = sync_directory(parent) {
            // The replacement and writer handle already agree. Directory
            // fsync strengthens crash durability, but its failure cannot be
            // reported as an uncommitted rewrite without lying to callers.
            tracing::warn!(
                target: "zevria_core::transcript", path = %self.path.display(),
                %error,
                "session replacement committed but the parent directory could not be synced"
            );
        }
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of malformed records removed while reopening this transcript.
    pub fn recovered_malformed_lines(&self) -> usize {
        self.recovered_malformed_lines
    }
}

fn serialize_record(item: &TranscriptItem) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(item).context("failed to serialize a transcript item")?;
    anyhow::ensure!(
        line.len() < MAX_ROOT_RECORD_BYTES,
        "root transcript record exceeds the 64 MiB limit"
    );
    line.push(b'\n');
    Ok(line)
}

/// Write every item to an already-created staged file.
fn write_items(
    file: &mut std::fs::File,
    destination: &Path,
    items: &[TranscriptItem],
) -> anyhow::Result<()> {
    for item in items {
        let line = serialize_record(item)?;
        file.write_all(&line).with_context(|| {
            format!(
                "failed to write the staged session file for {}",
                destination.display()
            )
        })?;
    }
    file.flush().with_context(|| {
        format!(
            "failed to flush the session file at {}",
            destination.display()
        )
    })?;
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
#[path = "writer_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pending_persistence_tests.rs"]
mod pending_persistence_tests;
