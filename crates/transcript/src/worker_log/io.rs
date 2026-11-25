//! Streaming worker log validation, append, and same-handle tail repair.

use std::{
    fs::File,
    io::{BufRead as _, BufReader, BufWriter, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use serde::{Deserialize, Deserializer};
use zevria_workflow::{
    AgentRunEvent, AgentUnavailableDecision, CLAUDE_PLAN_HANDOFF_PLAN_ID,
    MAX_AGENT_RUN_RECORD_BYTES, WorkerReviewEvent,
};

use crate::jsonl::with_record_buffer;

use super::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunProjection, AgentRunRecord, AgentRunTranscriptHeader,
    AgentRunTranscriptRecord, AgentRunValidation,
};

pub(super) const WRITER_UNUSABLE: &str =
    "agent-run writer is unusable after an append or sync failure; reopen the log";

/// Append-only JSONL worker transcript. An append or sync error poisons the
/// writer. A write or sync error may leave a partial or committed suffix;
/// reopen the log rather than retry on this writer.
pub struct AgentRunTranscriptWriter {
    path: PathBuf,
    pub(super) file: Option<BufWriter<File>>,
    pub(super) validation: AgentRunValidation,
    line: Vec<u8>,
}

impl AgentRunTranscriptWriter {
    pub fn create(path: PathBuf, header: AgentRunTranscriptHeader) -> anyhow::Result<Self> {
        let record = AgentRunTranscriptRecord::Header { header };
        record.validate_format()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create agent-run directory at {}",
                    parent.display()
                )
            })?;
        }
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to create agent-run log at {}", path.display()))?;
        let mut writer = Self {
            path,
            file: Some(BufWriter::new(file)),
            validation: AgentRunValidation::default(),
            line: Vec::new(),
        };
        writer.append(&record)?;
        Ok(writer)
    }

    pub fn append_to(path: PathBuf) -> anyhow::Result<Self> {
        let mut reader = AgentRunTranscriptReader::open_with(&path, true, false)?;
        for record in reader.by_ref() {
            record?;
        }
        Self::append_validated(reader)
    }

    /// Validate and project from the same open handle before any repair.
    pub fn reopen(
        path: PathBuf,
        expected: &AgentRunTranscriptHeader,
    ) -> anyhow::Result<(Self, AgentRunProjection)> {
        let mut reader = AgentRunTranscriptReader::open_with(&path, true, true)?;
        let projection = reader.project()?;
        anyhow::ensure!(
            projection.header.as_ref() == Some(expected),
            "worker transcript identity does not match the ensemble recovery record"
        );
        reader.validation.review = projection
            .review
            .as_deref()
            .map(|review| Arc::new(review.resume_validator()));
        let writer = Self::append_validated(reader)?;
        Ok((writer, projection))
    }

    fn append_validated(reader: AgentRunTranscriptReader) -> anyhow::Result<Self> {
        let mut file = reader.reader.into_inner();
        if reader.recoverable_tail {
            file.set_len(reader.verified_offset)?;
        }
        file.seek(SeekFrom::Start(reader.verified_offset))?;
        if reader.needs_newline {
            file.write_all(b"\n")?;
        }
        if reader.recoverable_tail || reader.needs_newline {
            file.sync_data().with_context(|| {
                format!(
                    "failed to sync repaired agent-run log at {}",
                    reader.path.display()
                )
            })?;
        }
        Ok(Self {
            path: reader.path,
            file: Some(BufWriter::new(file)),
            validation: reader.validation,
            line: Vec::new(),
        })
    }

    pub fn append(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<()> {
        self.append_buffered(record)?;
        self.sync()
    }

    /// Write one complete JSONL record without forcing it to stable storage.
    /// The ACP supervisor batches these writes on a blocking writer task and
    /// calls [`Self::sync`] before publishing their normalized events.
    pub fn append_buffered(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        let result = self.stage_record(record);
        if result.is_err() {
            self.disarm();
        }
        result
    }

    fn stage_record(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        let file = self.file.as_mut().context(WRITER_UNUSABLE)?;
        let change = self.validation.prepare(record)?;
        let (len, write) = with_record_buffer(&mut self.line, |line| -> anyhow::Result<_> {
            serde_json::to_writer(&mut *line, record)
                .context("failed to serialize agent-run record")?;
            anyhow::ensure!(
                line.len() < MAX_AGENT_RUN_RECORD_BYTES,
                "agent-run record exceeds the 64 MiB limit"
            );
            line.push(b'\n');
            Ok((line.len(), file.write_all(line)))
        })?;
        write.with_context(|| {
            format!("failed to append agent-run log at {}", self.path.display())
        })?;
        self.validation.commit(record, change);
        Ok(len)
    }

    /// Flush every preceding buffered record to stable storage.
    pub fn sync(&mut self) -> anyhow::Result<()> {
        let file = self.file.as_mut().context(WRITER_UNUSABLE)?;
        let result = file.flush().and_then(|()| file.get_ref().sync_data());
        if result.is_err() {
            self.disarm();
        }
        result.with_context(|| format!("failed to sync agent-run log at {}", self.path.display()))
    }

    fn disarm(&mut self) {
        if let Some(file) = self.file.take() {
            // Dropping a BufWriter can flush an unacknowledged batch or retry
            // a failed write. Discard the buffer and let reopen inspect the file.
            let _ = file.into_parts();
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Streaming strict worker reader. Only incomplete current-format final appends
/// are crash debris; unsupported or oversized records fail the whole load.
pub struct AgentRunTranscriptReader {
    path: PathBuf,
    pub(super) reader: BufReader<File>,
    line: usize,
    buffer: Vec<u8>,
    done: bool,
    verified_offset: u64,
    needs_newline: bool,
    recoverable_tail: bool,
    validation: AgentRunValidation,
}

impl AgentRunTranscriptReader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with(path, false, false)
    }

    fn open_with(path: &Path, write: bool, collect_review: bool) -> anyhow::Result<Self> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| counts.worker_loads += 1);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .with_context(|| format!("failed to read agent-run log at {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            line: 0,
            buffer: Vec::new(),
            done: false,
            verified_offset: 0,
            needs_newline: false,
            recoverable_tail: false,
            validation: AgentRunValidation {
                collect_review,
                ..Default::default()
            },
        })
    }

    fn project(&mut self) -> anyhow::Result<AgentRunProjection> {
        let mut projection = AgentRunProjection::default();
        for record in self.by_ref() {
            projection.apply_display(&record?);
        }
        projection.review = self.validation.review.clone();
        Ok(projection)
    }

    fn accept_line(&mut self) {
        self.verified_offset += self.buffer.len() as u64;
        self.needs_newline = !self.buffer.ends_with(b"\n");
    }

    fn read_line_bounded(&mut self) -> std::io::Result<Option<bool>> {
        self.buffer.clear();
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Ok((!self.buffer.is_empty()).then_some(false));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if consumed > MAX_AGENT_RUN_RECORD_BYTES - self.buffer.len() {
                // The iterator stops at an oversized record. There is no
                // later line to recover, so do not drain the rest of this one.
                return Ok(Some(true));
            }
            self.buffer.extend_from_slice(&available[..consumed]);
            self.reader.consume(consumed);
            if newline.is_some() {
                return Ok(Some(false));
            }
        }
    }
}

impl Iterator for AgentRunTranscriptReader {
    type Item = anyhow::Result<AgentRunTranscriptRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let oversized = match self.read_line_bounded() {
                Ok(Some(oversized)) => oversized,
                Ok(None) => {
                    self.done = true;
                    return self.validation.header.is_none().then(|| {
                        Err(crate::transcript::UnsupportedHistory::new(
                            &self.path,
                            Some(1),
                            "worker header v1",
                        )
                        .into())
                    });
                }
                Err(error) => {
                    self.done = true;
                    return Some(Err(anyhow::Error::from(error).context(format!(
                        "failed to read agent-run log at {}",
                        self.path.display()
                    ))));
                }
            };
            self.line = self.line.saturating_add(1);
            if oversized {
                self.done = true;
                return Some(Err(crate::transcript::UnsupportedHistory::new(
                    &self.path,
                    Some(self.line),
                    "bounded current worker record",
                )
                .into()));
            }
            if self.buffer.iter().all(u8::is_ascii_whitespace) {
                self.accept_line();
                continue;
            }
            // Decode the wire representation first, then validate separately.
            // The public record's TryFrom would flatten local constraints into
            // a Serde data error and obscure version/order/identity failures.
            let record = match serde_json::from_slice::<AgentRunRecord>(&self.buffer) {
                Ok(record) => record.into_record(),
                Err(error) => {
                    self.done = true;
                    if self.validation.header.is_some()
                        && !self.buffer.ends_with(b"\n")
                        && serde_json::from_slice::<serde_json::Value>(&self.buffer)
                            .is_err_and(|error| error.is_eof())
                        && !unsupported_worker_tail(&self.buffer, &self.validation)
                    {
                        self.recoverable_tail = true;
                        return None;
                    }
                    return Some(Err(crate::transcript::UnsupportedHistory::decoding(
                        &self.path, self.line, &error,
                    )
                    .into()));
                }
            };
            if let Err(error) = self.validation.apply(&record) {
                use crate::transcript::{HistoryFailure, UnsupportedHistory};
                self.done = true;
                let failure = match &record {
                    AgentRunTranscriptRecord::Header { header }
                        if header.version != AGENT_RUN_TRANSCRIPT_VERSION =>
                    {
                        HistoryFailure::Version {
                            expected: AGENT_RUN_TRANSCRIPT_VERSION,
                            found: header.version,
                        }
                    }
                    _ => HistoryFailure::Validation {
                        detail: error.to_string(),
                    },
                };
                return Some(Err(UnsupportedHistory::new(
                    &self.path,
                    Some(self.line),
                    "current worker records with matching identity and valid journal order",
                )
                .with_failure(failure)
                .into()));
            }
            #[cfg(feature = "test-support")]
            crate::replay_probe::record(|counts| counts.worker_records += 1);
            self.accept_line();
            return Some(Ok(record));
        }
    }
}

fn unsupported_worker_tail(line: &[u8], validation: &AgentRunValidation) -> bool {
    // A complete payload with only the outer delimiter missing is not crash
    // debris if it is invalid. Probe the original bytes so duplicate map keys
    // cannot disappear through the partial-field Value inspection below.
    let mut closed = line.to_vec();
    closed.push(b'}');
    match serde_json::from_slice::<AgentRunRecord>(&closed) {
        Ok(record) => return validation.prepare(&record.into_record()).is_err(),
        Err(error) if error.is_data() => return true,
        Err(_) => {}
    }
    if top_level_keys(line)
        .iter()
        .any(|key| !matches!(key.as_str(), "record" | "event" | "outcome"))
    {
        return true;
    }
    if partial_json_field(line, &["record"])
        .is_some_and(|value| value != "event" && value != "outcome")
    {
        return true;
    }
    if partial_json_field(line, &["event", "message_id"])
        .is_some_and(|value| value == CLAUDE_PLAN_HANDOFF_PLAN_ID)
    {
        return true;
    }
    if let Some(value) = partial_json_field(line, &["event", "event", "transition"])
        && !value.as_str().is_some_and(|transition| {
            WorkerReviewEvent::KNOWN_WIRE_TRANSITIONS.contains(&transition)
                // Synthetic transitions are known to the workflow reducer,
                // but never written as raw worker review records.
                && !matches!(transition, "fatal" | "published" | "removed" | "decisions")
        })
    {
        return true;
    }
    if let Some(value) = partial_json_field(line, &["event", "type"]) {
        let known = value
            .as_str()
            .is_some_and(|kind| AgentRunEvent::KNOWN_WIRE_TYPES.contains(&kind));
        if !known {
            return true;
        }
        if value == "response_display"
            && partial_json_field(line, &["event", "display", "version"]).is_some_and(|version| {
                version != zevria_content::response::RESPONSE_DISPLAY_VERSION
            })
        {
            return true;
        }
    }
    if partial_json_field(line, &["event", "decision_unavailable", "reason"])
        .is_some_and(|value| value != "normalized_payload_too_large")
    {
        return true;
    }
    if let Some(value) = partial_json_field(line, &["event", "decision_unavailable"])
        && !value.is_null()
        && serde_json::from_value::<AgentUnavailableDecision>(value).is_err()
    {
        return true;
    }
    for kind in ["event", "outcome"] {
        if let Some(value) = partial_json_field(line, &[kind]) {
            let record = serde_json::from_value::<AgentRunTranscriptRecord>(
                serde_json::json!({"record":kind, (kind):value}),
            );
            if record.is_err() || record.is_ok_and(|record| validation.prepare(&record).is_err()) {
                return true;
            }
        }
    }
    false
}

/// Read one structural field even when a later value is interrupted. Paths do
/// not traverse arrays or strings, so quoted marker text cannot become metadata.
fn partial_json_field(line: &[u8], path: &[&str]) -> Option<serde_json::Value> {
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

fn top_level_keys(line: &[u8]) -> Vec<String> {
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

/// Load every current worker record. Callers that only need a projection or can
/// reduce records incrementally should prefer the streaming APIs below.
pub fn load_agent_run(path: &Path) -> anyhow::Result<Vec<AgentRunTranscriptRecord>> {
    AgentRunTranscriptReader::open(path)?.collect()
}

/// Reduce a durable worker log without materializing its complete record list.
pub fn load_agent_run_projection(path: &Path) -> anyhow::Result<AgentRunProjection> {
    AgentRunTranscriptReader::open_with(path, false, true)?.project()
}
