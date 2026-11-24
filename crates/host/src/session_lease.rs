//! Session ownership survives atomic transcript replacement. Each namespace has
//! one permanent `.leases.lock` coordinator and transient per-session sidecars.
//! All updated writers coordinate opening and removing sidecars; file presence
//! alone never proves ownership. Stop all older binaries/workers before rollout.
//! Manual lock deletion, concurrent offline purges, and external replacement of
//! locking identities are unsupported, as are filesystems with broken OS locks.

use std::{
    fs::{self, File, Metadata, TryLockError},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use zevria_foundation::open_lock_file;

#[cfg(test)]
#[path = "session_lease_tests.rs"]
pub(crate) mod tests;

const COORDINATOR_NAME: &str = ".leases.lock";
const COORDINATOR_WAIT: Duration = Duration::from_millis(250);
const COORDINATOR_POLL: Duration = Duration::from_millis(5);

/// Keep the separate locking identity for the entire runtime/engine lifetime,
/// not the frontend lifetime. Only the final Arc release runs cleanup. Closing
/// before deletion supports platforms that cannot unlink an open locked file;
/// the directory coordinator protects that otherwise-unlocked interval.
#[derive(Debug)]
pub struct RootSessionLease {
    file: Option<File>,
    path: PathBuf,
    coordinator_path: PathBuf,
}

impl RootSessionLease {
    /// Acquire a lease without sweeping inactive sidecars, for lifecycle tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn acquire(transcript: &Path) -> anyhow::Result<Self> {
        Self::acquire_inner(transcript, false)
    }

    /// Synchronous, bounded-wait acquisition with a sweep of inactive sidecars.
    /// Startup calls this on a blocking worker before opening or repairing the
    /// transcript.
    pub(crate) fn acquire_sweeping(transcript: &Path) -> anyhow::Result<Self> {
        Self::acquire_inner(transcript, true)
    }

    fn acquire_inner(transcript: &Path, sweep: bool) -> anyhow::Result<Self> {
        let parent = transcript
            .parent()
            .context("session transcript has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create session directory {}", parent.display()))?;
        let coordinator_path = parent.join(COORDINATOR_NAME);
        let _coordinator = DirectoryCoordinator::acquire(&coordinator_path, COORDINATOR_WAIT)?;
        if sweep {
            sweep_locked(parent);
        }
        let path = transcript.with_extension("jsonl.lock");
        let file = open_lock_file(&path, true)
            .with_context(|| format!("cannot open session lease {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file: Some(file),
                path,
                coordinator_path,
            }),
            Err(error) => {
                // A contender must not retain the old inode after releasing
                // coordination: the owner may immediately close and remove it.
                drop(file);
                #[cfg(test)]
                tests::checkpoint(tests::Event::ContenderClosed);
                match error {
                    TryLockError::WouldBlock => anyhow::bail!(
                        "session {:?} is held by another runtime or recovery command; stop it before retrying",
                        transcript.file_stem().unwrap_or_default()
                    ),
                    TryLockError::Error(error) => Err(error)
                        .with_context(|| format!("cannot lock session lease {}", path.display())),
                }
            }
        }
    }
}

impl Drop for RootSessionLease {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        // Drop can run on an async executor (including startup-error paths).
        // Do not sleep here: one nonblocking attempt, then defer to startup.
        match DirectoryCoordinator::acquire(&self.coordinator_path, Duration::ZERO) {
            Ok(_coordinator) => {
                if let Err(error) = remove_eligible(&self.path, file) {
                    tracing::warn!(target: "zevria::session_lease", path = %self.path.display(), %error, "session lease cleanup deferred until a later startup");
                }
            }
            Err(error) => {
                drop(file);
                tracing::warn!(target: "zevria::session_lease", path = %self.path.display(), %error, "session lease cleanup deferred until a later startup");
            }
        }
    }
}

/// This locking identity is permanent: never remove it, even if no sidecars
/// remain. Hold it only for lease bookkeeping, including the inactive-sidecar
/// sweep, never for transcript IO, asynchronous work, or the runtime lifetime.
struct DirectoryCoordinator {
    _file: File,
}

impl DirectoryCoordinator {
    fn acquire(path: &Path, wait: Duration) -> anyhow::Result<Self> {
        let file = open_lock_file(path, true)
            .with_context(|| format!("cannot open session lease coordinator {}", path.display()))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    #[cfg(test)]
                    tests::checkpoint(tests::Event::CoordinatorBlocked);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    anyhow::ensure!(
                        !remaining.is_zero(),
                        "session lease coordinator {} is busy; retry after other lease maintenance finishes",
                        path.display()
                    );
                    std::thread::sleep(COORDINATOR_POLL.min(remaining));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!("cannot lock session lease coordinator {}", path.display())
                    });
                }
            }
        }
    }
}

fn eligible_name(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.as_encoded_bytes()
            .strip_suffix(b".jsonl.lock")
            .is_some_and(|basename| !basename.is_empty())
    })
}

fn eligible_metadata(metadata: &Metadata) -> bool {
    metadata.file_type().is_file() && metadata.len() == 0
}

fn eligible_sidecar(path: &Path) -> io::Result<bool> {
    if !eligible_name(path) {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(eligible_metadata(&metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Caller holds the coordinator and this file's exclusive lock. Revalidate both
/// handle and path, close on *every* branch while still coordinated, then unlink
/// only eligible sidecars. Customized files and transcripts are never removed.
fn remove_eligible(path: &Path, file: File) -> io::Result<()> {
    let eligible = eligible_metadata(&file.metadata()?) && eligible_sidecar(path)?;
    drop(file);
    if eligible {
        #[cfg(test)]
        tests::checkpoint(tests::Event::BeforeRemove);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Best-effort maintenance of the selected namespace while coordinated.
#[cfg(test)]
pub(crate) fn sweep_inactive(directory: &Path) {
    if let Ok(_coordinator) =
        DirectoryCoordinator::acquire(&directory.join(COORDINATOR_NAME), COORDINATOR_WAIT)
    {
        sweep_locked(directory);
    }
}

fn sweep_locked(directory: &Path) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(target: "zevria::session_lease", path = %directory.display(), %error, "cannot scan inactive session leases");
            return;
        }
    };
    for entry in entries {
        let result = entry
            .map_err(anyhow::Error::from)
            .and_then(|entry| sweep_candidate_locked(&entry.path()));
        if let Err(error) = result {
            tracing::warn!(target: "zevria::session_lease", path = %directory.display(), %error, "inactive session lease cleanup deferred until a later startup");
        }
    }
}

#[cfg(test)]
fn sweep_candidate(path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("session lease has no parent")?;
    let _coordinator =
        DirectoryCoordinator::acquire(&parent.join(COORDINATOR_NAME), COORDINATOR_WAIT)?;
    sweep_candidate_locked(path)
}

fn sweep_candidate_locked(path: &Path) -> anyhow::Result<()> {
    if !eligible_sidecar(path)? {
        return Ok(());
    }
    let file = match open_lock_file(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot reopen session lease {}", path.display()));
        }
    };
    match file.try_lock() {
        Ok(()) => remove_eligible(path, file)
            .with_context(|| format!("cannot remove inactive session lease {}", path.display())),
        Err(error) => {
            // In particular, close a failed contender before the coordinator.
            drop(file);
            match error {
                TryLockError::WouldBlock => Ok(()),
                TryLockError::Error(error) => Err(error)
                    .with_context(|| format!("cannot probe session lease {}", path.display())),
            }
        }
    }
}
