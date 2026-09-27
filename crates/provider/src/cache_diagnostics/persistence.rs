//! Private, bounded, diagnostic-only snapshots. Never conversation authority.
use super::{
    Baseline,
    fingerprint::{self, Fingerprint},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(super) const MAX_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_ITEMS: usize = 32_768;
const VERSION: u32 = 2;

/// One application runtime, shared by its lazy profile slots. This context is
/// never serialized into the transcript or sent to the provider.
#[derive(Clone)]
pub struct CacheDiagnosticContext {
    pub(super) runtime_id: uuid::Uuid,
    directory: PathBuf,
    session: Fingerprint,
}
impl CacheDiagnosticContext {
    pub fn new(transcript: &Path, session_id: &str) -> Self {
        let parent = transcript.parent().unwrap_or(Path::new("."));
        // Follow the application's actual transcript location, not the default
        // sessions directory. Canonicalizing the parent also handles aliases.
        let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        let identity = parent.join(transcript.file_name().unwrap_or_default());
        let mut input = identity.as_os_str().as_encoded_bytes().to_vec();
        input.push(0);
        input.extend_from_slice(session_id.as_bytes());
        Self {
            runtime_id: uuid::Uuid::new_v4(),
            directory: parent.join(".cache-diagnostics"),
            session: fingerprint::bytes(&input),
        }
    }
    pub(super) fn store(&self, profile: &zevria_foundation::ModelProfileRef) -> Store {
        let profile = profile_identity(profile);
        Store {
            directory: self.directory.clone(),
            name: format!("{}-{profile}.json", self.session),
            session: self.session,
            profile,
        }
    }
}
pub(super) fn profile_identity(profile: &zevria_foundation::ModelProfileRef) -> Fingerprint {
    fingerprint::fingerprint(&serde_json::json!([profile.provider, profile.model]))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    session: Fingerprint,
    profile: Fingerprint,
    // An explicit tombstone makes reset/cap invalidation survive restarts.
    baseline: Option<Baseline>,
    invalidated: Option<Invalidation>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Invalidation {
    Reset,
    ItemLimit,
    ByteLimit,
}
impl Invalidation {
    fn reason(self) -> &'static str {
        match self {
            Self::Reset => "persisted_reset",
            Self::ItemLimit => "snapshot_item_limit",
            Self::ByteLimit => "snapshot_byte_limit",
        }
    }
}
#[derive(Clone)]
pub(super) struct Store {
    directory: PathBuf,
    name: String,
    session: Fingerprint,
    profile: Fingerprint,
}
impl Store {
    pub fn load(&self) -> Result<Baseline, &'static str> {
        let bytes = private::read(&self.directory, &self.name)?;
        // Inspect the envelope before decoding the current schema. An old
        // baseline lacks new fields and must be rejected as a version mismatch,
        // not accidentally accepted or misclassified as corrupt current data.
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let version: Version = serde_json::from_slice(&bytes).map_err(|_| "snapshot_corrupt")?;
        if version.version != VERSION {
            return Err("snapshot_version");
        }
        let doc: Document = serde_json::from_slice(&bytes).map_err(|_| "snapshot_corrupt")?;
        if doc.session != self.session || doc.profile != self.profile {
            return Err("snapshot_identity");
        }
        match (doc.baseline, doc.invalidated) {
            (Some(baseline), None) => {
                if baseline.profile != self.profile {
                    return Err("snapshot_identity");
                }
                if baseline.items.len() > MAX_ITEMS {
                    return Err("snapshot_item_limit");
                }
                if baseline
                    .meta
                    .input_count
                    .checked_add(baseline.meta.output_count)
                    != Some(baseline.items.len())
                    || !baseline.meta.valid()
                {
                    return Err("snapshot_corrupt");
                }
                Ok(baseline)
            }
            (None, Some(reason)) => Err(reason.reason()),
            _ => Err("snapshot_corrupt"),
        }
    }
    pub fn save(&self, baseline: &Baseline) -> Result<(), &'static str> {
        if baseline.items.len() > MAX_ITEMS {
            self.invalidate(Invalidation::ItemLimit)?;
            return Err("snapshot_item_limit");
        }
        // All fields are bounded before serialization. There is no prompt data.
        let bytes = serde_json::to_vec(&Document {
            version: VERSION,
            session: self.session,
            profile: self.profile,
            baseline: Some(baseline.clone()),
            invalidated: None,
        })
        .map_err(|_| "snapshot_encode")?;
        if bytes.len() > MAX_BYTES {
            self.invalidate(Invalidation::ByteLimit)?;
            return Err("snapshot_byte_limit");
        }
        private::replace(&self.directory, &self.name, &bytes)
    }
    pub fn invalidate(&self, reason: Invalidation) -> Result<(), &'static str> {
        let bytes = serde_json::to_vec(&Document {
            version: VERSION,
            session: self.session,
            profile: self.profile,
            baseline: None,
            invalidated: Some(reason),
        })
        .map_err(|_| "snapshot_encode")?;
        private::replace(&self.directory, &self.name, &bytes)
    }
}

// Anchor all leaf operations to a no-follow directory descriptor. In particular,
// no read/open/rename can follow a substituted snapshot or staging symlink.
// A private directory prevents other users from replacing checked entries.
#[cfg(unix)]
mod private {
    use super::*;
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    };

    fn directory(path: &Path, create: bool) -> Result<File, &'static str> {
        if create {
            let result = std::fs::DirBuilder::new().mode(0o700).create(path);
            if let Err(error) = result
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err("storage_unwritable");
            }
        }
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "snapshot_missing"
            } else {
                "storage_unavailable"
            }
        })?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("storage_not_directory_or_symlink");
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "storage_unavailable")?;
        let meta = file.metadata().map_err(|_| "storage_unavailable")?;
        // SAFETY: geteuid has no preconditions.
        if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
            return Err("storage_permissions");
        }
        Ok(file)
    }
    fn c_name(name: &str) -> CString {
        CString::new(name).expect("digest/UUID filenames have no NUL")
    }
    fn open(dir: &File, name: &str, flags: i32) -> Result<File, &'static str> {
        // SAFETY: dir is live, name is NUL-terminated, and mode is supplied for O_CREAT.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c_name(name).as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == std::io::ErrorKind::NotFound {
                "snapshot_missing"
            } else if error.raw_os_error() == Some(libc::ELOOP) {
                "snapshot_symlink"
            } else {
                "snapshot_io"
            });
        }
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn regular(file: &File) -> Result<std::fs::Metadata, &'static str> {
        let meta = file.metadata().map_err(|_| "snapshot_io")?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("snapshot_not_regular");
        }
        // SAFETY: geteuid has no preconditions.
        if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
            return Err("snapshot_permissions");
        }
        Ok(meta)
    }
    pub fn read(path: &Path, name: &str) -> Result<Vec<u8>, &'static str> {
        let dir = directory(path, false)?;
        let mut file = open(&dir, name, libc::O_RDONLY)?;
        let before = regular(&file)?;
        if before.len() > MAX_BYTES as u64 {
            return Err("snapshot_byte_limit");
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "snapshot_io")?;
        if bytes.len() > MAX_BYTES {
            return Err("snapshot_byte_limit");
        }
        let after = regular(&file)?;
        if before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
            || bytes.len() as u64 != after.len()
        {
            return Err("snapshot_changed_during_read");
        }
        Ok(bytes)
    }
    pub fn replace(path: &Path, name: &str, bytes: &[u8]) -> Result<(), &'static str> {
        let dir = directory(path, true)?;
        match open(&dir, name, libc::O_RDONLY) {
            Ok(file) => {
                regular(&file)?;
            }
            Err("snapshot_missing") => {}
            Err(reason) => return Err(reason),
        }
        let staging = format!(".{}.tmp", uuid::Uuid::new_v4());
        let mut staged = false;
        let result = (|| {
            let mut file = open(
                &dir,
                &staging,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )?;
            staged = true;
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "storage_unwritable")?;
            // Recheck a destination possibly replaced while staging. Rename
            // replaces an entry, never writes through it (even under a race).
            match open(&dir, name, libc::O_RDONLY) {
                Ok(file) => {
                    regular(&file)?;
                }
                Err("snapshot_missing") => {}
                Err(reason) => return Err(reason),
            }
            // SAFETY: live directory descriptor and NUL-terminated leaf names.
            if unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    c_name(&staging).as_ptr(),
                    dir.as_raw_fd(),
                    c_name(name).as_ptr(),
                )
            } != 0
            {
                return Err("snapshot_replace");
            }
            staged = false;
            dir.sync_all().map_err(|_| "snapshot_directory_sync")
        })();
        // Best effort cleanup, also on failed publication. Never a shared name.
        // SAFETY: same live descriptor and private leaf name as creation.
        if staged {
            unsafe {
                libc::unlinkat(dir.as_raw_fd(), c_name(&staging).as_ptr(), 0);
            }
        }
        result
    }
}
#[cfg(not(unix))]
mod private {
    use super::*;
    // Do not silently weaken the no-follow/private-file contract on platforms
    // without the descriptor-relative implementation. Requests still proceed.
    pub fn read(_: &Path, _: &str) -> Result<Vec<u8>, &'static str> {
        Err("storage_platform_unsupported")
    }
    pub fn replace(_: &Path, _: &str, _: &[u8]) -> Result<(), &'static str> {
        Err("storage_platform_unsupported")
    }
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
