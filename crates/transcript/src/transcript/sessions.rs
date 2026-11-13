//! Transcript sessions responsibilities.
use super::item::ENSEMBLE_RECORD_KEY;
use super::item::MAX_ROOT_RECORD_BYTES;
use super::item::SKILL_INVOCATION_KEY;
use super::item::SUBTASK_RESULTS_KEY;
use super::item::SessionModelsEnvelope;
use super::item::TOOL_RESULT_METADATA_KEY;
use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// The sessions directory of a workspace: `{workspace}/.zevria/sessions`.
pub fn sessions_dir(workspace: &Path) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace).join("sessions")
}

/// The subsession directory holding one root session's child transcripts:
/// `{workspace}/.zevria/subsessions/{root_session_id}`. Living outside the
/// sessions directory keeps [`latest_session_file`] and `--continue` untouched
/// by child files.
pub fn subsessions_dir(workspace: &Path, root_session_id: &str) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace)
        .join("subsessions")
        .join(root_session_id)
}

/// The plans directory of a workspace: `{workspace}/.zevria/plans`. Submitted
/// artifacts are projected here by the session engine; transcripts remain
/// canonical.
pub fn plans_dir(workspace: &Path) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace).join("plans")
}

/// Pick a fresh session id without touching the filesystem, so composition
/// can name a session before its transcript file exists.
pub fn pick_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Discover the child transcripts recorded under a root's subsession
/// directory as `(child_id, path)` pairs, ordered by modification time. A
/// missing directory simply means the root never launched a subtask.
pub fn subsession_files(subsessions_dir: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    Ok(session_files_by_mtime(subsessions_dir)?
        .into_iter()
        .map(|(_, session_id, path)| (session_id, path))
        .collect())
}

/// The `.jsonl` transcript files in one directory as
/// `(modified, session_id, path)` triples, ordered oldest-first. A missing
/// directory simply means no sessions were recorded there.
fn session_files_by_mtime(
    directory: &Path,
) -> anyhow::Result<Vec<(std::time::SystemTime, String, PathBuf)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::Error::from(error).context(format!(
                "failed to list the session files in {}",
                directory.display()
            )));
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to list the session files in {}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let is_session_file = entry.file_type().is_ok_and(|kind| kind.is_file())
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl");
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !is_session_file {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("failed to read the metadata of {}", path.display()))?;
        files.push((modified, stem.to_string(), path));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    Ok(files)
}

/// One resumable session discovered on disk, with the metadata a picker
/// displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    /// The session's first user prompt: one line, truncated for display.
    pub preview: Option<String>,
}

/// How many leading transcript lines [`list_sessions`] scans for a preview.
const PREVIEW_SCAN_LINES: usize = 16;
/// Character cap for a session preview line.
const PREVIEW_MAX_CHARS: usize = 80;

/// Every resumable session in the directory, newest-first. Empty files and
/// legacy models-plus-prefix-only roots are skipped. A saved mode is meaningful
/// session state even without a message and must remain reopenable.
pub fn list_sessions(sessions_dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
    let mut summaries = Vec::new();
    for (modified, session_id, path) in session_files_by_mtime(sessions_dir)?.into_iter().rev() {
        if is_abandoned_root(&path) {
            continue;
        }
        let preview = session_preview(&path);
        summaries.push(SessionSummary {
            id: session_id,
            path,
            modified,
            preview,
        });
    }
    Ok(summaries)
}

pub(super) fn bounded_record_lines<R: std::io::BufRead>(
    mut reader: R,
) -> impl Iterator<Item = std::io::Result<Vec<u8>>> {
    use std::io::{BufRead as _, Read as _};
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut line = Vec::new();
        match (&mut reader)
            .take(MAX_ROOT_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => {
                done = true;
                None
            }
            Ok(_) if line.len() > MAX_ROOT_RECORD_BYTES => {
                done = true;
                Some(Err(std::io::Error::other(
                    "root record exceeds the 64 MiB limit",
                )))
            }
            Ok(_) => Some(Ok(line)),
            Err(error) => {
                done = true;
                Some(Err(error))
            }
        }
    })
}

/// The first plain user prompt within the file's leading lines, flattened to
/// one bounded display line. Unreadable, malformed, or absent content simply
/// yields no preview — a session must remain listable regardless.
pub(super) fn session_preview(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in bounded_record_lines(reader).take(PREVIEW_SCAN_LINES) {
        let line = String::from_utf8(line.ok()?).ok()?;
        if line.trim().is_empty() {
            continue;
        }
        // Only user prompts qualify: compound tool-result and subtask-result
        // records are engine traffic, not the user's prompt. A direct skill
        // invocation previews as its compact `$name` form.
        let value: serde_json::Value = serde_json::from_str(&line).ok()?;
        // Picker previews never instantiate a validated image or decode pixels.
        if let Some(start) = value.get(ENSEMBLE_RECORD_KEY).and_then(|e| e.get("start")) {
            let workflow: crate::EnsembleWorkflow =
                serde_json::from_value(start.get("workflow")?.clone()).ok()?;
            let prefix = workflow.slash_command();
            return Some(preview_line(&format!(
                "{prefix} {}",
                preview_blocks(start.get("prompt")?, true)
            )));
        }
        if let Some(invocation) = value.get(SKILL_INVOCATION_KEY) {
            return Some(preview_line(&format!(
                "${} {}",
                invocation.get("name")?.as_str()?,
                preview_blocks(invocation.get("arguments")?, true)
            )));
        }
        if value.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && !value.as_object()?.contains_key(TOOL_RESULT_METADATA_KEY)
            && !value.as_object()?.contains_key(SUBTASK_RESULTS_KEY)
        {
            let preview = preview_blocks(value.get("content")?, false);
            if !preview.is_empty() {
                return Some(preview_line(&preview));
            }
        }
    }
    None
}

fn preview_blocks(value: &serde_json::Value, typed: bool) -> String {
    let mut preview = String::new();
    if let Some(blocks) = value.as_array() {
        for block in blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    if let Some(text) = block
                        .get(if typed { "value" } else { "text" })
                        .and_then(serde_json::Value::as_str)
                    {
                        preview.extend(text.chars().take(PREVIEW_MAX_CHARS + 1));
                    }
                }
                Some("image") => preview.push_str("[image]"),
                _ => {}
            }
            if preview.chars().count() > PREVIEW_MAX_CHARS {
                break;
            }
        }
    }
    preview
}

/// Flatten prompt text to its first line, char-truncated with an ellipsis.
pub(super) fn preview_line(prompt_text: &str) -> String {
    let first_line = prompt_text.lines().next().unwrap_or_default();
    let mut preview: String = first_line.chars().take(PREVIEW_MAX_CHARS).collect();
    if first_line.chars().count() > PREVIEW_MAX_CHARS {
        preview.push('…');
    }
    preview
}

/// The most recently modified `.jsonl` session file in the directory, if any.
/// A missing directory simply means no sessions exist yet.
pub fn latest_session_file(sessions_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    Ok(session_files_by_mtime(sessions_dir)?
        .into_iter()
        .rev()
        .map(|(_, _, path)| path)
        .find(|path| !is_abandoned_root(path)))
}

/// Only a truly empty file or valid models-only root is safe
/// to hide/delete. A mode-bearing root is retained, including canonical Build:
/// it may be the result of an explicit Plan → Build selection. Unreadable,
/// malformed, and substantive histories also stay visible.
pub fn is_abandoned_root(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut lines = bounded_record_lines(std::io::BufReader::new(file));
    match lines.next() {
        None => return true,
        Some(Ok(line)) if serde_json::from_slice::<SessionModelsEnvelope>(&line).is_ok() => {}
        _ => return false,
    }
    // Stop as soon as history is substantive/damaged; listing should not load
    // and validate entire conversations just to exclude abandoned roots.
    lines.all(|line| line.is_ok_and(|line| line.iter().all(u8::is_ascii_whitespace)))
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
