//! Read-only root-session discovery shared by in-process frontends.
use std::path::{Path, PathBuf};
pub use zevria_transcript::transcript::SessionSummary;

/// Newest-first resumable roots, excluding the live root by session identity.
/// Run on a background executor: this performs bounded filesystem reads.
pub fn resumable(workspace: &Path, exclude: Option<&str>) -> anyhow::Result<Vec<SessionSummary>> {
    let mut sessions = zevria_transcript::transcript::list_sessions(
        &zevria_transcript::transcript::sessions_dir(workspace),
    )?;
    sessions.retain(|session| exclude != Some(session.id.as_str()));
    Ok(sessions)
}

pub fn latest(workspace: &Path) -> anyhow::Result<Option<PathBuf>> {
    zevria_transcript::transcript::latest_session_file(
        &zevria_transcript::transcript::sessions_dir(workspace),
    )
}
