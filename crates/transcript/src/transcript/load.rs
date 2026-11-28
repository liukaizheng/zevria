//! Tagged JSONL loading and recovery.
use super::item::{BATCH_VERSION, TranscriptItem, TranscriptLine, replay_active_skills};
use super::sessions::read_record_line;
use crate::instruction_replay::SkillLifecycleLocation;
use crate::models::SessionModels;
use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// Classification of an inspection diagnostic. Only an incomplete final
/// JSON line is eligible for automatic crash recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptDamage {
    IncompleteTail,
    InvalidRecord,
    IncompatibleLifecycle,
    InvalidSessionModels,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDiagnostic {
    /// Original one-based physical line, or None for a cross-record failure.
    pub line: Option<usize>,
    pub kind: TranscriptDamage,
    pub message: String,
}

/// Validated projection of a current transcript, including recoverable tail
/// diagnostics. Unsupported histories return an error instead of a projection.
#[derive(Debug, Clone)]
pub struct TranscriptLoadOutcome {
    pub(super) path: PathBuf,
    pub(super) items: Vec<TranscriptItem>,
    /// Original physical line for each projected item.
    pub source_lines: Vec<usize>,
    pub diagnostics: Vec<TranscriptDiagnostic>,
    pub omitted_diagnostics: usize,
    pub recoverable_lines: usize,
    pub blocked: bool,
    pub(super) needs_newline: bool,
    pub(super) recoverable_tail_offset: Option<u64>,
    pub(super) replay: Option<crate::ValidatedSessionReplay>,
}

impl TranscriptLoadOutcome {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }
    pub fn into_items(self) -> Vec<TranscriptItem> {
        self.items
    }

    pub fn replay(&self) -> &crate::ValidatedSessionReplay {
        self.replay
            .as_ref()
            .expect("successful transcript loads have validated replay")
    }

    fn diagnose(&mut self, line: Option<usize>, kind: TranscriptDamage, message: impl AsRef<str>) {
        self.blocked |= kind != TranscriptDamage::IncompleteTail;
        if self.diagnostics.len() == 256 {
            self.omitted_diagnostics += 1;
        } else {
            self.diagnostics.push(TranscriptDiagnostic {
                line,
                kind,
                message: message.as_ref().chars().take(1024).collect(),
            });
        }
    }

    /// Read validated selection metadata. Production root and native-worker
    /// resume additionally require Some; ordinary child logs do not.
    pub fn session_models(&self) -> anyhow::Result<Option<&SessionModels>> {
        Ok(self.replay().instructions.session_models())
    }

    pub fn ensure_resumable(&self) -> anyhow::Result<()> {
        if self.blocked {
            let detail = self
                .diagnostics
                .iter()
                .filter(|entry| entry.kind != TranscriptDamage::IncompleteTail)
                .take(8)
                .map(|entry| match entry.line {
                    Some(line) => format!("line {line}: {}", entry.message),
                    None => entry.message.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(UnsupportedHistory::new(&self.path, None, detail).into());
        }
        Ok(())
    }
}

/// Load a current resumable history. Unsupported records reject the entire load;
/// [`load_report`] additionally returns diagnostics for recoverable trailing damage.
pub fn load(path: &Path) -> anyhow::Result<Vec<TranscriptItem>> {
    let outcome = load_report(path)?;
    outcome.ensure_resumable()?;
    Ok(outcome.items)
}

/// Read exactly the committed logical prefix, without decoding any following
/// physical lines (which may still be in flight).
pub fn load_prefix_items(
    path: &Path,
    exact_item_count: usize,
) -> anyhow::Result<Vec<TranscriptItem>> {
    if exact_item_count == 0 {
        return Ok(Vec::new());
    }
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut line = Vec::new();
    let mut items = Vec::new();
    let mut line_number = 0;
    while items.len() < exact_item_count && read_record_line(&mut reader, &mut line)? {
        line_number += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<TranscriptLine>(&line)
            .map_err(|error| UnsupportedHistory::decoding(path, line_number, &error))?
        {
            TranscriptLine::Item(item) => items.push(item),
            TranscriptLine::Batch { records, .. } => {
                items.extend(records.into_iter().take(exact_item_count - items.len()));
            }
        }
    }
    anyhow::ensure!(
        items.len() == exact_item_count,
        "parent transcript prefix changed during fork"
    );
    Ok(items)
}

pub fn load_report(path: &Path) -> anyhow::Result<TranscriptLoadOutcome> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| counts.loads += 1);
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to read the session file at {}", path.display()))?;
    let mut outcome = TranscriptLoadOutcome {
        path: path.to_path_buf(),
        items: Vec::new(),
        source_lines: Vec::new(),
        diagnostics: Vec::new(),
        omitted_diagnostics: 0,
        recoverable_lines: 0,
        blocked: false,
        needs_newline: false,
        recoverable_tail_offset: None,
        replay: None,
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut byte_offset = 0;
    let mut incomplete_attempts = std::collections::HashSet::new();
    while read_record_line(&mut reader, &mut line)? {
        line_number += 1;
        let line_offset = byte_offset;
        byte_offset += line.len() as u64;
        outcome.needs_newline = !line.ends_with(b"\n");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut append = |item| -> anyhow::Result<()> {
            if let TranscriptItem::IncompleteResponse(record) = &item
                && !incomplete_attempts.insert(record.attempt_id.clone())
            {
                return Err(UnsupportedHistory::new(
                    path,
                    Some(line_number),
                    "unique incomplete response attempt identities",
                )
                .with_failure(HistoryFailure::Validation {
                    detail: "duplicate incomplete response attempt_id".into(),
                })
                .into());
            }
            outcome.items.push(item);
            outcome.source_lines.push(line_number);
            Ok(())
        };
        match serde_json::from_slice::<TranscriptLine>(&line) {
            Ok(TranscriptLine::Item(item)) => append(item)?,
            Ok(TranscriptLine::Batch { version, records }) => {
                debug_assert_eq!(version, BATCH_VERSION);
                for item in records {
                    append(item)?;
                }
            }
            // read_record_line reaches EOF whenever a bounded line has no LF.
            // An unknown tag, bad version, or malformed completed field is a
            // data error even if the rest of the line was never written.
            Err(error) if outcome.needs_newline && error.is_eof() => {
                outcome.recoverable_lines += 1;
                outcome.recoverable_tail_offset = Some(line_offset);
                outcome.diagnose(
                    Some(line_number),
                    TranscriptDamage::IncompleteTail,
                    error.to_string(),
                );
            }
            Err(error) => {
                let mut failure = UnsupportedHistory::decoding(path, line_number, &error);
                failure.expected = "a tagged JSONL v2 transcript record starting with type".into();
                return Err(failure.into());
            }
        }
    }
    match crate::replay::validate_replay(&outcome.items, true) {
        Ok(replay) => outcome.replay = Some(replay),
        Err(error) => {
            let line = matches!(
                error,
                crate::SessionReplayError::Skills(_) | crate::SessionReplayError::Both { .. }
            )
            .then(|| replay_active_skills(&outcome.items).err())
            .flatten()
            .and_then(|error| {
                error
                    .downcast_ref::<SkillLifecycleLocation>()
                    .and_then(|location| outcome.source_lines.get(location.0))
                    .copied()
            });
            return Err(anyhow::Error::new(error).context(UnsupportedHistory::new(
                path,
                line,
                "valid current session lifecycle",
            )));
        }
    }
    Ok(outcome)
}

/// Safe structured context for a persisted-record failure. Decoder messages can
/// contain arbitrary record values, so only categories and fixed descriptions
/// are retained, never the raw Serde error or JSON payload.
#[derive(Debug)]
pub enum HistoryFailure {
    Decode {
        category: serde_json::error::Category,
        record_line: usize,
        record_column: usize,
        detail: &'static str,
    },
    Validation {
        /// Locally generated constraint explanation, not a protocol payload.
        detail: String,
    },
    Version {
        expected: u32,
        found: u32,
    },
}

/// A persisted format failure, never recoverable crash debris or a model conversion request.
#[derive(Debug)]
pub struct UnsupportedHistory {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub expected: String,
    pub failure: Option<HistoryFailure>,
}

impl UnsupportedHistory {
    pub fn new(path: &Path, line: Option<usize>, expected: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            line,
            expected: expected.into(),
            failure: None,
        }
    }

    pub fn with_failure(mut self, failure: HistoryFailure) -> Self {
        self.failure = Some(failure);
        self
    }

    pub fn decoding(path: &Path, line: usize, error: &serde_json::Error) -> Self {
        let message = error.to_string();
        let detail = if message.starts_with("invalid type:") {
            "invalid value type"
        } else if message.starts_with("unknown variant") {
            "unknown record, event, or enum variant"
        } else if message.starts_with("unknown field") {
            "unknown field"
        } else if message.starts_with("missing field") {
            "missing required field"
        } else if message.starts_with("duplicate field") {
            "duplicate field"
        } else {
            "record does not match the current schema"
        };
        Self::new(path, Some(line), "a decodable current record").with_failure(
            HistoryFailure::Decode {
                category: error.classify(),
                record_line: error.line(),
                record_column: error.column(),
                detail,
            },
        )
    }
}

impl std::fmt::Display for UnsupportedHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported history at {}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        match &self.failure {
            Some(HistoryFailure::Decode {
                category,
                record_line,
                record_column,
                detail,
            }) => {
                write!(f, "; decoding failure ({category:?}: {detail}")?;
                if *record_line != 0 || *record_column != 0 {
                    write!(f, ", record line {record_line}, column {record_column}")?;
                }
                write!(f, ")")?;
            }
            Some(HistoryFailure::Validation { detail }) => {
                // Validation explanations are authored by the local validators.
                // Bound and sanitize them even if a future constraint is longer.
                let detail = zevria_content::readable::sanitize_readable(detail)
                    .chars()
                    .take(256)
                    .collect::<String>();
                write!(f, "; validation failure: {detail}")?;
            }
            Some(HistoryFailure::Version { expected, found }) => {
                write!(
                    f,
                    "; worker version mismatch: found v{found}, expected v{expected}"
                )?;
            }
            None => {}
        }
        write!(
            f,
            "; expected {}. Original bytes were preserved; preserve the logs and resolve the reported history issue before resuming",
            self.expected
        )
    }
}
impl std::error::Error for UnsupportedHistory {}

#[cfg(test)]
#[path = "load_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "history_format_tests.rs"]
mod format_tests;

#[cfg(test)]
#[path = "batch_tests.rs"]
mod batch_tests;
