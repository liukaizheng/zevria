//! Transcript load responsibilities.
use super::item::BATCH_KEY;
use super::item::BatchEnvelope;
use super::item::COMMAND_JOB_NOTICE_KEY;
use super::item::COMPACTION_RECORD_KEY;
use super::item::DIRECTIVE_KEY;
use super::item::ENSEMBLE_RECORD_KEY;
use super::item::INCOMPLETE_RESPONSE_KEY;
use super::item::INSTRUCTION_PREFIX_KEY;
use super::item::PLAN_RECORD_KEY;
use super::item::PROVIDER_REPLAY_KEY;
use super::item::REQUEST_DIRECTIVE_KEY;
use super::item::REQUEST_METADATA_KEY;
use super::item::SESSION_MODE_KEY;
use super::item::SESSION_MODELS_KEY;
use super::item::SKILL_APPLICATIONS_KEY;
use super::item::SKILL_DIRECTIVE_KEY;
use super::item::SKILL_INVOCATION_KEY;
use super::item::SessionModeEnvelope;
use super::item::SessionModelsEnvelope;
use super::item::TOOL_DECLARATIONS_KEY;
use super::item::TOOL_RESULT_METADATA_KEY;
use super::item::TranscriptItem;
use super::item::TranscriptLine;
use super::item::current_reserved_key;
use super::item::replay_active_skills;
use super::item::validate_reserved_lifecycle_keys;
use super::sessions::bounded_record_lines;
use crate::instruction_replay::SkillLifecycleLocation;
use crate::models::SessionModels;
use crate::skill::SkillInvocation;
use crate::skill::SkillToolApplication;
use anyhow::Context as _;
use serde::{Deserialize, Deserializer};
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
#[derive(Debug)]
pub struct TranscriptLoadOutcome {
    pub(super) path: PathBuf,
    pub(super) items: Vec<TranscriptItem>,
    /// Original physical line for each projected item.
    pub source_lines: Vec<usize>,
    pub diagnostics: Vec<TranscriptDiagnostic>,
    pub omitted_diagnostics: usize,
    pub recoverable_lines: usize,
    pub blocked: bool,
    // Invalid metadata must never be mistaken for absence.
    session_models_error: Option<String>,
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
        if kind == TranscriptDamage::InvalidSessionModels && self.session_models_error.is_none() {
            let detail = message.as_ref().chars().take(1024).collect::<String>();
            self.session_models_error =
                Some(line.map_or_else(|| detail.clone(), |line| format!("line {line}: {detail}")));
        }
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
        if let Some(error) = &self.session_models_error {
            anyhow::bail!(
                "invalid session model metadata: {error}; restore a valid backup or use an appropriate compatible Zevria version; metadata must not be automatically replaced"
            );
        }
        Ok(match self.items.first() {
            Some(TranscriptItem::SessionModels(models)) => Some(models),
            _ => None,
        })
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
        session_models_error: None,
        needs_newline: false,
        recoverable_tail_offset: None,
        replay: None,
    };
    let mut lines = bounded_record_lines(std::io::BufReader::new(file))
        .enumerate()
        .peekable();
    let mut incomplete_attempts = std::collections::HashSet::new();
    let mut byte_offset = 0_u64;
    while let Some((index, line)) = lines.next() {
        let line = line?;
        let line_offset = byte_offset;
        byte_offset = byte_offset.saturating_add(line.len() as u64);
        let line = line.as_slice();
        outcome.needs_newline = !line.ends_with(b"\n");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Successful current records take one typed decode; expensive key
        // probes and error classification are reserved for damaged records.
        if let Ok(decoded) = serde_json::from_slice::<TranscriptLine>(line) {
            let records = match decoded {
                TranscriptLine::Item(item) => vec![item],
                TranscriptLine::Batch(envelope) => {
                    let batch = envelope.zevria_batch;
                    if batch.version != 1
                        || batch.records.is_empty()
                        || batch.records.iter().any(|item| {
                            matches!(
                                item,
                                TranscriptItem::SessionModels(_) | TranscriptItem::SessionMode(_)
                            )
                        })
                    {
                        outcome.diagnose(
                            Some(index + 1),
                            TranscriptDamage::IncompatibleLifecycle,
                            "invalid batch version, empty batch, or batched session header",
                        );
                        continue;
                    }
                    batch.records
                }
            };
            for item in records {
                if matches!(item, TranscriptItem::SessionModels(_)) && index != 0 {
                    outcome.diagnose(
                        Some(index + 1),
                        TranscriptDamage::InvalidSessionModels,
                        "session model metadata must be unique and on the first physical line",
                    );
                    continue;
                }
                if let TranscriptItem::IncompleteResponse(record) = &item
                    && !incomplete_attempts.insert(record.attempt_id.clone())
                {
                    outcome.diagnose(
                        Some(index + 1),
                        TranscriptDamage::IncompatibleLifecycle,
                        "duplicate incomplete response attempt_id",
                    );
                    continue;
                }
                outcome.items.push(item);
                outcome.source_lines.push(index + 1);
            }
            continue;
        }
        // Detect a reserved top-level key even if its value is truncated. Do
        // not search message text: literal marker strings are ordinary data.
        if partial_json_field(line, &[SESSION_MODE_KEY, "selected"])
            .as_ref()
            .and_then(serde_json::Value::as_str)
            == Some("orchestrate")
        {
            return Err(UnsupportedHistory::new(path, Some(index + 1), "Build or Plan session mode")
                .with_failure(HistoryFailure::Validation { detail: "legacy Orchestrate mode is no longer supported; start a fresh Build session and submit /orchestrate <prompt>. The saved transcript has not been modified".into() }).into());
        }
        let model_metadata = has_session_models_key(line);
        if top_level_keys(line)
            .iter()
            .any(|key| key == SESSION_MODE_KEY)
            && let Err(error) = serde_json::from_slice::<SessionModeEnvelope>(line)
        {
            outcome.diagnose(
                Some(index + 1),
                TranscriptDamage::IncompatibleLifecycle,
                error.to_string(),
            );
            continue;
        }
        if model_metadata {
            let header = serde_json::from_slice::<SessionModelsEnvelope>(line);
            let error = if index != 0 {
                Some(
                    "session model metadata must be unique and on the first physical line"
                        .to_string(),
                )
            } else {
                header.as_ref().err().map(ToString::to_string)
            };
            if let Some(error) = error {
                outcome.diagnose(
                    Some(index + 1),
                    TranscriptDamage::InvalidSessionModels,
                    error,
                );
                continue;
            }
        }
        let value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(error) => {
                let recoverable = lines.peek().is_none()
                    && outcome.needs_newline
                    && error.is_eof()
                    && !has_unsupported_reserved_tail(line);
                let kind = if recoverable {
                    outcome.recoverable_lines += 1;
                    outcome.recoverable_tail_offset = Some(line_offset);
                    TranscriptDamage::IncompleteTail
                } else {
                    TranscriptDamage::InvalidRecord
                };
                outcome.diagnose(Some(index + 1), kind, error.to_string());
                continue;
            }
        };
        // Inspect reserved keys at the record boundary, never marker-like
        // strings inside ordinary messages or a skill's instructions.
        let validation = validate_reserved_lifecycle_keys(&value);
        if let Err(error) = validation {
            outcome.diagnose(
                Some(index + 1),
                TranscriptDamage::IncompatibleLifecycle,
                format!("{error:#}"),
            );
            continue;
        }
        let reserved = value.as_object().is_some_and(|object| {
            object.keys().any(|key| {
                key.starts_with("zevria_skill_")
                    || key == COMPACTION_RECORD_KEY
                    || key == PROVIDER_REPLAY_KEY
                    || key == ENSEMBLE_RECORD_KEY
                    || key == PLAN_RECORD_KEY
                    || key == INSTRUCTION_PREFIX_KEY
                    || key == SESSION_MODE_KEY
                    || key == DIRECTIVE_KEY
                    || key == REQUEST_METADATA_KEY
                    || key == REQUEST_DIRECTIVE_KEY
                    || key == INCOMPLETE_RESPONSE_KEY
                    || key == TOOL_DECLARATIONS_KEY
            })
        });
        match serde_json::from_slice::<TranscriptItem>(line) {
            Ok(item) => {
                if let TranscriptItem::IncompleteResponse(record) = &item
                    && !incomplete_attempts.insert(record.attempt_id.clone())
                {
                    outcome.diagnose(
                        Some(index + 1),
                        TranscriptDamage::IncompatibleLifecycle,
                        "duplicate incomplete response attempt_id",
                    );
                    continue;
                }
                outcome.items.push(item);
                outcome.source_lines.push(index + 1);
            }
            Err(error) => outcome.diagnose(
                Some(index + 1),
                if reserved {
                    TranscriptDamage::IncompatibleLifecycle
                } else {
                    TranscriptDamage::InvalidRecord
                },
                error.to_string(),
            ),
        }
    }
    // One authoritative reduction on successful load. Invalid histories alone
    // pay for a diagnostic location probe; no writable repair precedes this.
    if !outcome.blocked {
        match crate::validate_session_replay(&outcome.items) {
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
    }
    if outcome.blocked {
        let diagnostic = outcome
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.kind != TranscriptDamage::IncompleteTail);
        if diagnostic.is_some_and(|diagnostic| {
            diagnostic.message == "duplicate incomplete response attempt_id"
        }) {
            return Err(UnsupportedHistory::new(
                path,
                diagnostic.and_then(|diagnostic| diagnostic.line),
                "unique incomplete response attempt identities",
            )
            .with_failure(HistoryFailure::Validation {
                detail: "duplicate incomplete response attempt_id".into(),
            })
            .into());
        }
        return Err(UnsupportedHistory::new(path, diagnostic.and_then(|diagnostic| diagnostic.line),
            "current transcript records, dedicated name-based skill invocation and application records, skill directive v1 records with version and payload only, compaction v1 without instruction state, no retired instruction prefix or directive records, optional leading session models followed immediately by optional session mode v1 with an exact ID (mode first without models), provider replay v1 with nonblank source_profile, immutable incomplete response v1 without native tool bindings, ensemble review v1 with explicit confirmations, and valid lifecycle linkage").into());
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

fn has_session_models_key(line: &[u8]) -> bool {
    top_level_keys(line)
        .iter()
        .any(|key| key.starts_with(SESSION_MODELS_KEY))
}

fn has_unsupported_reserved_tail(line: &[u8]) -> bool {
    fn invalid_field<T: serde::de::DeserializeOwned>(line: &[u8], path: &[&str]) -> bool {
        partial_json_field(line, path)
            .is_some_and(|value| serde_json::from_value::<T>(value).is_err())
    }

    let keys = top_level_keys(line);
    if keys.iter().any(|key| key == BATCH_KEY) {
        // Typed streaming decode notices invalid completed children before an
        // interrupted later child; never erase unsupported lifecycle evidence.
        if serde_json::from_slice::<BatchEnvelope>(line).is_err_and(|error| error.is_data()) {
            return true;
        }
        if partial_json_field(line, &[BATCH_KEY, "version"])
            .is_some_and(|value| value.as_u64() != Some(1))
        {
            return true;
        }
    }
    // Probe owned envelope bytes directly: Value would discard duplicate fields
    // in a complete invalid payload whose outer delimiter was interrupted.
    if keys.iter().any(|key| {
        matches!(
            key.as_str(),
            INCOMPLETE_RESPONSE_KEY
                | TOOL_DECLARATIONS_KEY
                | COMPACTION_RECORD_KEY
                | PROVIDER_REPLAY_KEY
                | SKILL_DIRECTIVE_KEY
                | ENSEMBLE_RECORD_KEY
                | SESSION_MODELS_KEY
        )
    }) {
        let mut closed = line.to_vec();
        closed.push(b'}');
        if serde_json::from_slice::<TranscriptItem>(&closed).is_err_and(|error| error.is_data()) {
            return true;
        }
    }
    // A crash may interrupt the reserved key itself before its closing quote.
    // Probe only newly completed top-level keys, never strings in message text.
    let mut completed_key = line.to_vec();
    completed_key.extend_from_slice(b"\":null}");
    if top_level_keys(&completed_key)
        .iter()
        .skip(keys.len())
        .any(|key| key.starts_with("zevria_"))
    {
        return true;
    }
    if keys.iter().any(|key| {
        matches!(
            key.as_str(),
            INSTRUCTION_PREFIX_KEY
                | TOOL_DECLARATIONS_KEY
                | DIRECTIVE_KEY
                | COMPACTION_RECORD_KEY
                | SESSION_MODE_KEY
                | SKILL_INVOCATION_KEY
                | SKILL_APPLICATIONS_KEY
                | TOOL_RESULT_METADATA_KEY
                | REQUEST_METADATA_KEY
                | REQUEST_DIRECTIVE_KEY
        )
    }) {
        return true;
    }
    if keys
        .iter()
        .any(|key| key.starts_with("zevria_") && !current_reserved_key(key))
    {
        return true;
    }
    for (envelope, version) in [
        (COMMAND_JOB_NOTICE_KEY, 1),
        (TOOL_DECLARATIONS_KEY, 1),
        (COMPACTION_RECORD_KEY, crate::compaction::COMPACTION_VERSION),
        (PROVIDER_REPLAY_KEY, crate::PROVIDER_REPLAY_VERSION),
        (
            ENSEMBLE_RECORD_KEY,
            zevria_workflow::ENSEMBLE_REVIEW_VERSION,
        ),
        (INCOMPLETE_RESPONSE_KEY, crate::INCOMPLETE_RESPONSE_VERSION),
        (
            SKILL_DIRECTIVE_KEY,
            zevria_instructions::directive::INSTRUCTION_VERSION,
        ),
    ] {
        if partial_json_field(line, &[envelope, "version"])
            .is_some_and(|value| value.as_u64() != Some(u64::from(version)))
        {
            return true;
        }
    }
    // A completed inner envelope must already be current even if its outer
    // record was interrupted. Validate only actual persisted boundaries, not
    // arbitrary JSON in messages, tool arguments, or provider-native items.
    for key in &keys {
        if matches!(
            key.as_str(),
            COMPACTION_RECORD_KEY
                | PROVIDER_REPLAY_KEY
                | PLAN_RECORD_KEY
                | ENSEMBLE_RECORD_KEY
                | SKILL_DIRECTIVE_KEY
                | COMMAND_JOB_NOTICE_KEY
                | TOOL_DECLARATIONS_KEY
                | INCOMPLETE_RESPONSE_KEY
        ) && let Some(value) = partial_json_field(line, &[key])
            && serde_json::from_value::<TranscriptItem>(serde_json::json!({(key): value})).is_err()
        {
            return true;
        }
    }
    invalid_field::<Vec<SkillToolApplication>>(line, &[SKILL_APPLICATIONS_KEY])
        || invalid_field::<SkillInvocation>(line, &[SKILL_INVOCATION_KEY])
}

/// Read one structural field even when a later value is interrupted. Paths do
/// not traverse arrays or strings, so quoted marker text cannot become metadata.
pub(crate) fn partial_json_field(line: &[u8], path: &[&str]) -> Option<serde_json::Value> {
    probe_json_field(line, path).1
}

fn probe_json_field(line: &[u8], path: &[&str]) -> (bool, Option<serde_json::Value>) {
    struct Probe<'a, 'b> {
        path: &'a [&'a str],
        found: &'b mut Option<serde_json::Value>,
        seen: &'b mut bool,
    }
    impl<'de> serde::de::DeserializeSeed<'de> for Probe<'_, '_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            if self.path.is_empty() {
                *self.seen = true;
                *self.found = Some(serde_json::Value::deserialize(deserializer)?);
                Ok(())
            } else {
                deserializer.deserialize_map(self)
            }
        }
    }
    impl<'de> serde::de::Visitor<'de> for Probe<'_, '_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a record object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == self.path[0] {
                    map.next_value_seed(Probe {
                        path: &self.path[1..],
                        found: self.found,
                        seen: self.seen,
                    })?;
                } else {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(())
        }
    }
    let mut found = None;
    let mut seen = false;
    let _ = serde::de::DeserializeSeed::deserialize(
        Probe {
            path,
            found: &mut found,
            seen: &mut seen,
        },
        &mut serde_json::Deserializer::from_slice(line),
    );
    (seen, found)
}

pub(crate) fn top_level_keys(line: &[u8]) -> Vec<String> {
    struct Probe<'a>(&'a mut Vec<String>);
    impl<'de> serde::de::Visitor<'de> for Probe<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a transcript object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                self.0.push(key);
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut found = Vec::new();
    let _ = serde::Deserializer::deserialize_map(
        &mut serde_json::Deserializer::from_slice(line),
        Probe(&mut found),
    );
    found
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "history_format_tests.rs"]
mod format_tests;

#[cfg(test)]
#[path = "batch_tests.rs"]
mod batch_tests;
