//! Session namespaces, workspace resolution, and startup transcript leases.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use zevria_acp::ExecutionProfile;
use zevria_transcript::transcript;

pub(crate) fn canonical_workspace(workspace: &Path) -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let canonical = zevria_foundation::windows_io::checked_directory_path(workspace)
        .context("unsupported protected Windows workspace")?;
    #[cfg(not(windows))]
    let canonical = std::fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace at {}", workspace.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "workspace {} is not a directory",
        canonical.display()
    );
    Ok(canonical)
}

/// Child native transcripts and leases never participate in root discovery.
pub fn sessions_dir(workspace: &Path, profile: ExecutionProfile) -> PathBuf {
    match profile {
        ExecutionProfile::Interactive => transcript::sessions_dir(workspace),
        ExecutionProfile::EnsembleWorker => {
            zevria_foundation::runtime_paths::workspace_state_root(workspace)
                .join("ensemble-sessions")
        }
    }
}

pub(super) fn ensure_worker_ignore_guard(directory: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(directory)?;
    let path = directory.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file
            .write_all(b"*\n")
            .and_then(|()| file.sync_all())
            .with_context(|| {
                format!(
                    "failed to persist worker ignore guard at {}",
                    path.display()
                )
            }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to create worker ignore guard at {}", path.display())),
    }
}

/// Shared root/worker startup boundary. Scan only the selected namespace and
/// acquire ownership before transcript loading/repair or any engine work. Both
/// enumeration and bounded coordinator waits run off the async executor.
pub(super) async fn prepare_session_lease(
    path: &Path,
) -> anyhow::Result<Arc<crate::session_lease::RootSessionLease>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::session_lease::RootSessionLease::acquire_sweeping(&path).map(Arc::new)
    })
    .await
    .context("session lease startup task failed")?
}

pub(crate) fn transcript_recovery_notice(path: &Path, malformed_lines: usize) -> Option<String> {
    (malformed_lines > 0).then(|| {
        format!(
            "Recovered the session transcript at {} by removing {malformed_lines} malformed record(s).",
            path.display()
        )
    })
}
