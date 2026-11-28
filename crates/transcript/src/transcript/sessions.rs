//! Transcript session discovery.
use super::item::{MAX_ROOT_RECORD_BYTES, RecordType, read_batch_version, read_record_type};
use anyhow::Context as _;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};
use std::path::Path;
use std::path::PathBuf;
use std::{
    borrow::Cow,
    io::{self, BufRead, BufReader, Read as _},
};
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

/// Side Chats have a private namespace and do not participate in --continue.
pub fn side_chats_dir(workspace: &Path, root_id: &str) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace)
        .join("side-chats")
        .join(root_id)
}

pub fn side_chat_files(directory: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    subsession_files(directory)
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
/// models-only roots are skipped. A saved mode is meaningful
/// session state even without a message and must remain reopenable.
pub fn list_sessions(sessions_dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
    let mut summaries = Vec::new();
    for (modified, session_id, path) in session_files_by_mtime(sessions_dir)?.into_iter().rev() {
        let (abandoned, preview) = scan_root(&path, true);
        if abandoned {
            continue;
        }
        summaries.push(SessionSummary {
            id: session_id,
            path,
            modified,
            preview,
        });
    }
    Ok(summaries)
}

/// Read one physical line with a fixed bound, reusing the caller's buffer.
pub(super) fn read_record_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    let bytes = reader
        .take(MAX_ROOT_RECORD_BYTES as u64 + 1)
        .read_until(b'\n', line)?;
    if bytes > MAX_ROOT_RECORD_BYTES {
        return Err(io::Error::other("root record exceeds the 64 MiB limit"));
    }
    Ok(bytes != 0)
}

/// Peek at a record without constructing a transcript item, decoding raster
/// data or building a generic JSON object. Unused fields use Serde's skip path.
#[derive(Default)]
struct Discovery {
    substantive: bool,
    preview: Option<String>,
}

impl Discovery {
    fn merge(&mut self, other: Self) {
        self.substantive |= other.substantive;
        if self.preview.is_none() {
            self.preview = other.preview;
        }
    }
}

struct PickerLine<const ALLOW_BATCH: bool>(Discovery);

impl<'de, const ALLOW_BATCH: bool> Deserialize<'de> for PickerLine<ALLOW_BATCH> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(Self(Discovery::default()))
    }
}

impl<'de, const ALLOW_BATCH: bool> Visitor<'de> for PickerLine<ALLOW_BATCH> {
    type Value = Self;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a tagged transcript record")
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<Self, A::Error> {
        let record_type = read_record_type(&mut map)?;
        let fields = MapAccessDeserializer::new(map);
        self.0.substantive = true;
        self.0.preview = match record_type {
            RecordType::Batch if ALLOW_BATCH => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Batch {
                    #[serde(rename = "version", deserialize_with = "read_batch_version")]
                    _version: u32,
                    records: PickerBatch,
                }
                self.0 = Batch::deserialize(fields)?.records.0;
                return Ok(self);
            }
            RecordType::Batch => return Err(serde::de::Error::custom("nested transcript batch")),
            RecordType::SessionModels => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Models {
                    #[serde(rename = "models")]
                    _models: crate::models::SessionModels,
                }
                Models::deserialize(fields)?;
                self.0.substantive = false;
                None
            }
            RecordType::Message | RecordType::RequestPrompt => {
                #[derive(Deserialize)]
                struct Prompt<'a> {
                    #[serde(borrow)]
                    message: PromptMessage<'a>,
                }
                #[derive(Deserialize)]
                struct PromptMessage<'a> {
                    #[serde(borrow)]
                    role: Cow<'a, str>,
                    #[serde(default)]
                    content: PreviewBlocks,
                }
                let prompt = Prompt::deserialize(fields)?.message;
                (prompt.role == "user" && !prompt.content.0.is_empty())
                    .then(|| preview_line(&prompt.content.0))
            }
            RecordType::SkillInvocation => {
                #[derive(Deserialize)]
                struct Skill<'a> {
                    #[serde(borrow)]
                    invocation: Invocation<'a>,
                }
                #[derive(Deserialize)]
                struct Invocation<'a> {
                    #[serde(borrow)]
                    name: Cow<'a, str>,
                    arguments: PreviewBlocks,
                }
                let invocation = Skill::deserialize(fields)?.invocation;
                Some(preview_line(&format!(
                    "${} {}",
                    invocation.name, invocation.arguments.0
                )))
            }
            RecordType::Ensemble => {
                #[derive(Deserialize)]
                struct Ensemble {
                    ensemble: StartRecord,
                }
                #[derive(Deserialize)]
                struct StartRecord {
                    start: Option<Start>,
                }
                #[derive(Deserialize)]
                struct Start {
                    workflow: crate::EnsembleWorkflow,
                    prompt: PreviewBlocks,
                }
                Ensemble::deserialize(fields)?.ensemble.start.map(|start| {
                    preview_line(&format!(
                        "{} {}",
                        start.workflow.slash_command(),
                        start.prompt.0
                    ))
                })
            }
            _ => {
                IgnoredAny::deserialize(fields)?;
                None
            }
        };
        Ok(self)
    }
}

struct PickerBatch(Discovery);
impl<'de> Deserialize<'de> for PickerBatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Batch;
        impl<'de> Visitor<'de> for Batch {
            type Value = PickerBatch;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a nonempty array of transcript items")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<PickerBatch, A::Error> {
                let mut result = seq
                    .next_element::<PickerLine<false>>()?
                    .ok_or_else(|| serde::de::Error::custom("empty transcript batch"))?
                    .0;
                while let Some(line) = seq.next_element::<PickerLine<false>>()? {
                    result.merge(line.0);
                }
                Ok(PickerBatch(result))
            }
        }
        deserializer.deserialize_seq(Batch)
    }
}

#[derive(Default)]
struct PreviewBlocks(String);
impl<'de> Deserialize<'de> for PreviewBlocks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Blocks;
        impl<'de> Visitor<'de> for Blocks {
            type Value = PreviewBlocks;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("prompt content blocks")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<PreviewBlocks, A::Error> {
                // `value` belongs to typed prompt blocks, where it can also be
                // an image object. Read text only and skip objects recursively.
                #[derive(Deserialize)]
                struct Block<'a> {
                    #[serde(rename = "type", borrow)]
                    kind: Cow<'a, str>,
                    #[serde(default, borrow)]
                    text: Option<Cow<'a, str>>,
                    #[serde(default)]
                    value: PreviewValue,
                }
                let mut preview = String::new();
                while preview.chars().count() <= PREVIEW_MAX_CHARS {
                    let Some(block) = seq.next_element::<Block<'_>>()? else {
                        return Ok(PreviewBlocks(preview));
                    };
                    match block.kind.as_ref() {
                        "text" => preview.extend(
                            block
                                .text
                                .as_deref()
                                .unwrap_or(&block.value.0)
                                .chars()
                                .take(PREVIEW_MAX_CHARS + 1),
                        ),
                        "image" => preview.push_str("[image]"),
                        _ => {}
                    }
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(PreviewBlocks(preview))
            }
        }
        deserializer.deserialize_seq(Blocks)
    }
}

#[derive(Default)]
struct PreviewValue(String);
impl<'de> Deserialize<'de> for PreviewValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Value;
        impl<'de> Visitor<'de> for Value {
            type Value = PreviewValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a text or image prompt value")
            }
            fn visit_str<E>(self, text: &str) -> Result<PreviewValue, E> {
                Ok(PreviewValue(
                    text.chars().take(PREVIEW_MAX_CHARS + 1).collect(),
                ))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PreviewValue, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(PreviewValue::default())
            }
        }
        d.deserialize_any(Value)
    }
}

fn scan_root(path: &Path, want_preview: bool) -> (bool, Option<String>) {
    let Ok(file) = std::fs::File::open(path) else {
        return (false, None);
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return (false, None);
    }
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut lines = 0;
    let mut substantive = false;
    let mut models_only = false;
    loop {
        match read_record_line(&mut reader, &mut buffer) {
            Ok(false) => return (!substantive && (lines == 0 || models_only), None),
            Err(_) => return (false, None),
            Ok(true) => {}
        }
        lines += 1;
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            let Ok(PickerLine(found)) = serde_json::from_slice::<PickerLine<true>>(&buffer) else {
                return (false, None);
            };
            substantive |= found.substantive;
            models_only |= !found.substantive;
            if want_preview && let Some(preview) = found.preview {
                return (false, Some(preview));
            }
        }
        if substantive && (!want_preview || lines >= PREVIEW_SCAN_LINES) {
            return (false, None);
        }
    }
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
    scan_root(path, false).0
}

/// Resolve one root by its filename without scanning other sessions. As with
/// the picker, symlinks are not resumable entries. An explicit ID lookup
/// does not read transcript contents; startup validates the selected log.
pub fn find_session_file(directory: &Path, session_id: &str) -> anyhow::Result<Option<PathBuf>> {
    use std::path::Component;
    let mut components = Path::new(session_id).components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name == session_id)
        || components.next().is_some()
        || session_id.contains('\0')
    {
        return Ok(None);
    }
    let path = directory.join(format!("{session_id}.jsonl"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(path)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect session file {}", path.display()))
        }
    }
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
