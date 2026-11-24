//! Transcript conversation responsibilities.
use super::header::is_leading_metadata;
use super::item::TranscriptItem;
use super::load::TranscriptLoadOutcome;
use super::projection::is_compaction_prompt_item;
use super::projection::is_conversation_item;
use super::projection::is_prompt_item;
use super::projection::model_input_at;
use super::projection::retained_user_candidates;
use super::writer::TranscriptWriter;
use crate::ModelRequestItem;
#[cfg(any(test, feature = "test-support"))]
use crate::compaction::CompactionCheckpoint;
use crate::ensemble::EnsembleRecord;
use crate::ensemble::EnsembleRunId;
use crate::models::SessionModels;
use anyhow::Context as _;
use rig_core::message::Message;
use std::path::Path;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// One live conversation and the writer for its complete persisted sequence.
///
/// The item list is the *single* in-memory representation of a session's
/// conversation. Model input projects from it. Pending new sessions keep
/// metadata and directives only here; the first conversational commit publishes
/// the entire ordered sequence. Thereafter disk equals the live items (excluding
/// blank lines), unless completed work is retained under degraded persistence.
/// Required conversational changes install memory only after publication.
pub struct Conversation {
    items: Vec<TranscriptItem>,
    pub(super) writer: TranscriptWriter,
    pub(super) persistence_error: Option<String>,
    replay: Result<crate::ValidatedSessionReplay, crate::SessionReplayError>,
    input_identity: Option<zevria_model::ModelInputIdentity>,
    has_images: bool,
    has_compaction_prompt: bool,
    latest_compaction_index: Option<usize>,
    has_real_user_prompt: bool,
    has_conversation_item: bool,
}

/// Disjoint writer borrow used while a provider holds immutable model input.
/// Display checkpoints are persisted immediately, but only installed into the
/// conversation after that request borrow ends. They are never model input.
pub struct AttemptCheckpointWriter<'a> {
    writer: &'a mut TranscriptWriter,
    persistence_error: &'a mut Option<String>,
    records: Vec<crate::IncompleteResponse>,
}

impl AttemptCheckpointWriter<'_> {
    pub fn append(&mut self, record: crate::IncompleteResponse) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        let result = if let Some(error) = &self.persistence_error {
            Err(anyhow::anyhow!(
                "session transcript persistence is degraded: {error}"
            ))
        } else {
            self.writer
                .append_record(&TranscriptItem::IncompleteResponse(Box::new(
                    record.clone(),
                )))
        };
        self.records.push(record);
        if let Err(error) = &result {
            *self.persistence_error = Some(format!("{error:#}"));
        }
        result
    }

    pub fn into_records(self) -> Vec<crate::IncompleteResponse> {
        self.records
    }
}

/// Created and consumed synchronously inside Conversation. Neither arbitrary
/// records nor detached replay facts can enter the validated persistence path.
struct PreparedChange {
    items: Vec<TranscriptItem>,
    replay: crate::ValidatedSessionReplay,
    suffix: usize,
}

impl Conversation {
    /// Begin an empty conversation logging to `writer`.
    pub fn new(writer: TranscriptWriter) -> Self {
        Self {
            items: Vec::new(),
            writer,
            persistence_error: None,
            replay: Ok(crate::ValidatedSessionReplay::default()),
            input_identity: Some(zevria_model::ModelInputIdentity::default()),
            has_images: false,
            has_compaction_prompt: false,
            latest_compaction_index: None,
            has_real_user_prompt: false,
            has_conversation_item: false,
        }
    }

    /// Install validated startup items without writing: either metadata for a
    /// pending new session, or items loaded from this writer's existing log.
    /// The caller owns replay validation; this is not a mutation/acceptance API.
    pub fn from_loaded(
        writer: TranscriptWriter,
        outcome: TranscriptLoadOutcome,
    ) -> anyhow::Result<Self> {
        outcome.ensure_resumable()?;
        anyhow::ensure!(
            writer.path() == outcome.path,
            "loaded transcript does not belong to this writer"
        );
        let mut conversation = Self::new(writer);
        conversation.items = outcome.items;
        conversation.replay = Ok(outcome
            .replay
            .context("loaded transcript has no validated replay")?);
        conversation.rebuild_input_identity();
        Ok(conversation)
    }

    pub fn adopt_validated(
        &mut self,
        items: Vec<TranscriptItem>,
        replay: crate::ValidatedSessionReplay,
    ) {
        self.items = items;
        self.replay = Ok(replay);
        self.rebuild_input_identity();
    }

    /// Synthetic historical/invalid-evidence injection. Production startup uses
    /// the validated load report and required mutations use `commit_anchored`.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn adopt_persisted(&mut self, items: Vec<TranscriptItem>) {
        self.items = items;
        self.replay = crate::validate_session_replay(&self.items);
        self.rebuild_input_identity();
    }

    /// Reuse all replay domains after append. Invalid committed evidence still
    /// performs the independent full checks to preserve error precedence.
    pub fn replay(&self) -> Result<&crate::ValidatedSessionReplay, crate::SessionReplayError> {
        self.replay.as_ref().map_err(Clone::clone)
    }

    pub fn input_identity(&self) -> Option<&zevria_model::ModelInputIdentity> {
        self.input_identity.as_ref()
    }

    fn rebuild_input_identity(&mut self) {
        self.latest_compaction_index = None;
        self.has_compaction_prompt = false;
        self.has_real_user_prompt = false;
        self.has_conversation_item = false;
        self.update_item_facts(0);
        self.rebuild_model_identity();
    }

    fn rebuild_model_identity(&mut self) {
        let input = self.model_input();
        let has_images = input
            .iter()
            .filter_map(|item| item.message_ref())
            .any(zevria_content::prompt::message_contains_images);
        let identity = zevria_model::ModelInputIdentity::from_items(&input).ok();
        self.input_identity = identity;
        self.has_images = has_images;
    }

    pub fn has_images(&self) -> bool {
        self.has_images
    }

    fn update_item_facts(&mut self, start: usize) {
        for (index, item) in self.items.iter().enumerate().skip(start) {
            if matches!(item, TranscriptItem::Compaction(_)) {
                self.latest_compaction_index = Some(index);
            }
            self.has_compaction_prompt |= is_compaction_prompt_item(item);
            self.has_real_user_prompt |= is_prompt_item(item);
            self.has_conversation_item |= is_conversation_item(item);
        }
    }

    fn input_appended(&mut self, start: usize) {
        self.update_item_facts(start);
        if self
            .latest_compaction_index
            .is_some_and(|index| index >= start)
        {
            self.rebuild_model_identity();
            return;
        }
        self.has_images |= self.items[start..]
            .iter()
            .filter_map(TranscriptItem::model_request_item)
            .filter_map(ModelRequestItem::message_ref)
            .any(zevria_content::prompt::message_contains_images);
        if let Some(identity) = &mut self.input_identity
            && identity
                .extend(
                    self.items[start..]
                        .iter()
                        .filter_map(TranscriptItem::model_request_item),
                )
                .is_err()
        {
            self.input_identity = None;
        }
    }

    fn replay_appended(&mut self, start: usize) {
        self.input_appended(start);
        let prefix = std::mem::replace(&mut self.replay, Ok(Default::default()));
        self.replay = prefix.and_then(|prefix| {
            prefix
                .apply_suffix(&self.items[start..])
                .or_else(|_| crate::validate_session_replay(&self.items))
        });
    }

    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn session_models(&self) -> Option<&SessionModels> {
        self.replay.as_ref().ok()?.instructions.session_models()
    }

    /// Change the active route with one durable, model-inert record.
    pub fn replace_session_models(&mut self, models: SessionModels) -> anyhow::Result<()> {
        self.ensure_metadata_writable()?;
        if self.session_models() != Some(&models) {
            self.append_metadata(TranscriptItem::SessionModels(Box::new(models)))?;
        }
        Ok(())
    }

    pub fn replace_session_mode(&mut self, mode: crate::SessionMode) -> anyhow::Result<bool> {
        self.ensure_metadata_writable()?;
        if self.replay()?.instructions.persisted_mode() == Some(mode) {
            return Ok(false);
        }
        self.append_metadata(TranscriptItem::SessionMode(mode))?;
        Ok(true)
    }

    fn ensure_metadata_writable(&self) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        anyhow::ensure!(
            self.persistence_error.is_none(),
            "session transcript persistence is degraded"
        );
        Ok(())
    }

    fn append_metadata(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.replay()?.instructions.ensure_metadata_boundary()?;
        if self.writer.is_pending() {
            self.writer.validate_record(&item)?;
        } else if let Err(error) = self
            .writer
            .append_records(std::slice::from_ref(&item), true)
        {
            // A rollback or pre-write failure leaves the selected value and
            // durable prefix unchanged. Only an ambiguous append requires repair.
            if error.is::<super::writer::IndeterminateAppend>() {
                self.persistence_error = Some(format!("{error:#}"));
            }
            return Err(error);
        }
        self.replay
            .as_mut()
            .expect("validated replay")
            .instructions
            .append_metadata(&item);
        self.items.push(item);
        Ok(())
    }

    /// Assemble, validate, and persist one authoritative anchored proposal.
    /// Replay belongs exclusively to the installed conversation. Conversational
    /// work requires durability; metadata-only new sessions stay in memory.
    /// Failed validation or persistence never installs prospective state.
    pub fn commit_anchored(
        &mut self,
        index: Option<usize>,
        mut records: Vec<TranscriptItem>,
        mode: Option<crate::SessionMode>,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| counts.proposals += 1);
        let end = index
            .unwrap_or(self.items.len())
            .max(self.leading_metadata_len());
        anyhow::ensure!(
            end <= self.items.len(),
            "replacement index is outside the transcript"
        );
        if mode.is_some() {
            self.ensure_metadata_writable()?;
        }
        if index.is_none() {
            let prefix = self.replay()?;
            if let Some(mode) =
                mode.filter(|mode| Some(*mode) != prefix.instructions.persisted_mode())
            {
                if records.is_empty() {
                    return self.append_metadata(TranscriptItem::SessionMode(mode));
                }
                records.insert(0, TranscriptItem::SessionMode(mode));
            }
            if records.is_empty() {
                return Ok(());
            }
            #[cfg(feature = "test-support")]
            crate::replay_probe::record(|counts| counts.prospective_copies += 1);
            let replay = match prefix.clone().apply_suffix(&records) {
                Ok(replay) => replay,
                Err(_) => {
                    // Preserve independent full-replay error classification on rejection.
                    let start = self.items.len();
                    self.items.extend(records.iter().cloned());
                    let result = crate::validate_session_replay(&self.items);
                    self.items.truncate(start);
                    result?
                }
            };
            let batch = records.len() > 1 || mode.is_some();
            self.push_required_batch_impl(records, false, batch)?;
            self.replay = Ok(replay);
            return Ok(());
        }

        let mut items = self.items[..end].to_vec();
        let retained_models = items.iter().rev().find_map(|item| match item {
            TranscriptItem::SessionModels(models) => Some(models.as_ref()),
            _ => None,
        });
        if let Some(models) = self
            .session_models()
            .filter(|models| Some(*models) != retained_models)
        {
            items.push(TranscriptItem::SessionModels(Box::new(models.clone())));
        }
        let retained_mode = items.iter().rev().find_map(|item| match item {
            TranscriptItem::SessionMode(mode) => Some(*mode),
            _ => None,
        });
        if let Some(mode) = mode
            .or(self.replay()?.instructions.persisted_mode())
            .filter(|mode| Some(*mode) != retained_mode)
        {
            items.push(TranscriptItem::SessionMode(mode));
        }
        items.extend(records);
        let replay = crate::validate_session_replay(&items)?;
        self.persist_prepared_change(PreparedChange {
            items,
            replay,
            suffix: end,
        })
    }

    fn persist_prepared_change(&mut self, change: PreparedChange) -> anyhow::Result<()> {
        self.ensure_durable()?;
        self.writer.persist_edit(&change.items, change.suffix)?;
        self.adopt_validated(change.items, change.replay);
        Ok(())
    }

    /// Split the immutable request history from checkpoint-only persistence.
    pub fn borrow_for_dispatch(
        &mut self,
    ) -> (
        Vec<ModelRequestItem<'_>>,
        Option<&zevria_model::ModelInputIdentity>,
        AttemptCheckpointWriter<'_>,
    ) {
        (
            model_input_at(&self.items, self.latest_compaction_index),
            self.input_identity.as_ref(),
            AttemptCheckpointWriter {
                writer: &mut self.writer,
                persistence_error: &mut self.persistence_error,
                records: Vec::new(),
            },
        )
    }

    pub fn install_attempt_checkpoints(&mut self, records: Vec<crate::IncompleteResponse>) {
        let start = self.items.len();
        self.items.extend(
            records
                .into_iter()
                .map(|record| TranscriptItem::IncompleteResponse(Box::new(record))),
        );
        self.replay_appended(start);
    }

    /// The exact ordered active model projection.
    pub fn model_input(&self) -> Vec<ModelRequestItem<'_>> {
        model_input_at(&self.items, self.latest_compaction_index)
    }

    /// Message-bearing compatibility view of the active model projection.
    /// Opaque provider-only checkpoint entries are intentionally absent.
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.model_input()
            .into_iter()
            .filter_map(ModelRequestItem::message_ref)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn latest_compaction(&self) -> Option<(usize, &CompactionCheckpoint)> {
        let index = self.latest_compaction_index?;
        let TranscriptItem::Compaction(checkpoint) = &self.items[index] else {
            unreachable!()
        };
        Some((index, checkpoint))
    }

    /// User/direct-skill prompts and typed Plan handoff instructions eligible
    /// for bounded retention in the next checkpoint.
    pub fn retained_user_candidates(&self) -> Vec<String> {
        retained_user_candidates(&self.items)
    }

    pub fn has_real_user_prompt(&self) -> bool {
        self.has_real_user_prompt
    }

    pub fn has_compaction_prompt(&self) -> bool {
        self.has_compaction_prompt
    }

    /// Append a pre-effect item. A failure keeps memory unchanged and marks
    /// the writer degraded because the failed write may have left a partial
    /// trailing line that must be repaired before another append.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn push_required(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.push_required_batch_impl(vec![item], true, false)
    }

    /// Legacy fixture append for a group of pre-effect records. Production
    /// workflow transitions use `commit_anchored` for validated atomic admission;
    /// this path remains for historical storage and invalid-evidence tests.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn push_required_batch(&mut self, new_items: Vec<TranscriptItem>) -> anyhow::Result<()> {
        self.push_required_batch_impl(new_items, true, true)
    }

    fn push_required_batch_impl(
        &mut self,
        new_items: Vec<TranscriptItem>,
        replay: bool,
        batch: bool,
    ) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        if new_items.is_empty() {
            return Ok(());
        }
        self.ensure_durable()?;
        let start = self.items.len();
        let pending = self.writer.is_pending();
        self.items.extend(new_items);
        let result = if pending {
            if replay {
                self.writer.persist_unprepared(&self.items)
            } else {
                self.writer.persist_records(&self.items)
            }
        } else {
            self.writer.append_records(&self.items[start..], batch)
        };
        if let Err(error) = result {
            self.items.truncate(start);
            if !pending && (replay || error.is::<super::writer::IndeterminateAppend>()) {
                self.persistence_error = Some(format!("{error:#}"));
            }
            return Err(error);
        }
        if replay {
            self.replay_appended(start);
        } else {
            self.input_appended(start);
        }
        Ok(())
    }

    /// Append work that has already completed externally. If persistence
    /// fails, retain the item in memory so the live conversation remains
    /// truthful, mark the writer degraded, and require a full repair before
    /// any further pre-effect work starts.
    pub fn push_completed(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.push_completed_batch_impl(vec![item], false)
    }

    /// Atomically persist several records for work that has already completed.
    /// A failed replacement keeps the complete batch in memory and marks the
    /// conversation degraded, matching [`Self::push_completed`] semantics while
    /// ensuring a successful crash boundary can expose either every record or
    /// none of them.
    pub fn push_completed_batch(&mut self, new_items: Vec<TranscriptItem>) -> anyhow::Result<()> {
        self.push_completed_batch_impl(new_items, true)
    }

    fn push_completed_batch_impl(
        &mut self,
        new_items: Vec<TranscriptItem>,
        batch: bool,
    ) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        if new_items.is_empty() {
            return Ok(());
        }
        let durable = self.ensure_durable();
        let start = self.items.len();
        let pending = self.writer.is_pending();
        self.items.extend(new_items);
        if pending {
            if let Err(error) = crate::validate_session_replay(&self.items) {
                self.items.truncate(start);
                return Err(error.into());
            }
            if !self.items.iter().any(is_conversation_item) {
                if let Err(error) = self.writer.persist_records(&self.items) {
                    self.items.truncate(start);
                    return Err(error);
                }
                self.replay_appended(start);
                return Ok(());
            }
        }
        let result = durable.and_then(|_| {
            if pending {
                self.writer.persist_records(&self.items)
            } else {
                self.writer.append_records(&self.items[start..], batch)
            }
        });
        self.replay_appended(start);
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let error = error.context("the session transcript is degraded");
                self.persistence_error = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }

    /// Repair any partial/failed append by rewriting the persisted projection
    /// of the complete live conversation. Returns `true` after a successful repair.
    pub fn ensure_durable(&mut self) -> anyhow::Result<bool> {
        self.writer.ensure_writable()?;
        if self.writer.is_pending() && !self.has_conversation_item {
            // Shutdown/repair must never flush intentional metadata-only state.
            return Ok(false);
        }
        if self.persistence_error.is_none() && !self.writer.is_pending() {
            return Ok(false);
        }
        if let Err(error) = self.writer.persist_unprepared(&self.items) {
            let error = error.context("failed to repair the degraded session transcript");
            self.persistence_error = Some(format!("{error:#}"));
            return Err(error);
        }
        self.persistence_error = None;
        Ok(true)
    }

    pub fn persistence_error(&self) -> Option<&str> {
        self.persistence_error.as_deref()
    }

    pub fn is_read_only(&self) -> bool {
        self.writer.is_read_only()
    }

    /// The position of the `ordinal`-th [`prompt item`](is_prompt_item), or
    /// `None` when the conversation holds fewer prompts than that.
    pub fn prompt_position(&self, ordinal: usize) -> Option<usize> {
        let (index, _) = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| is_prompt_item(item))
            .nth(ordinal)?;
        Some(index)
    }

    pub fn leading_metadata_len(&self) -> usize {
        self.items
            .iter()
            .take_while(|item| is_leading_metadata(item))
            .count()
    }

    /// The position of the durable start record for `run_id`, if it remains
    /// in the active root transcript. Later records for the same run are not
    /// valid tail-edit anchors.
    pub fn ensemble_start_position(&self, run_id: &EnsembleRunId) -> Option<usize> {
        self.items.iter().position(|item| {
            matches!(
                item.ensemble_record(),
                Some(EnsembleRecord::Started { start })
                    if &start.run_id == run_id
            )
        })
    }

    /// Drop every item from `index` on and rewrite the retained sequence.
    /// The fallible write runs first, so a failure leaves both sides untouched
    /// and the caller keeps a conversation whose items match its log.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn truncate(&mut self, index: usize) -> anyhow::Result<()> {
        self.ensure_durable()?;
        let index = index.max(self.leading_metadata_len());
        self.writer.persist_unprepared(&self.items[..index])?;
        self.items.truncate(index);
        self.replay = crate::validate_session_replay(&self.items);
        self.rebuild_input_identity();
        Ok(())
    }

    /// Replace `index..` with one new transcript item as a single commit.
    /// A frontend receives confirmation only after the old tail and its typed
    /// replacement have moved together on disk and in memory.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn replace_from(&mut self, index: usize, item: TranscriptItem) -> anyhow::Result<()> {
        self.replace_from_items(index, vec![item])
    }

    /// Atomically replace the transcript tail with several typed records.
    /// Message, skill, ensemble, compaction, and Plan-transition records can
    /// therefore share one durable revision boundary.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn replace_from_items(
        &mut self,
        index: usize,
        new_items: Vec<TranscriptItem>,
    ) -> anyhow::Result<()> {
        self.ensure_durable()?;
        let index = index.max(self.leading_metadata_len());
        let mut replacement = self.items[..index].to_vec();
        replacement.extend(new_items);
        self.writer.persist_unprepared(&replacement)?;
        self.items = replacement;
        self.replay = crate::validate_session_replay(&self.items);
        self.rebuild_input_identity();
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        self.writer.session_id()
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prepared_change_tests.rs"]
mod prepared_change_tests;

#[cfg(test)]
#[path = "instruction_tests.rs"]
mod instruction_tests;
