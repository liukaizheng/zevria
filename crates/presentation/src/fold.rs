//! Pane-local, explicit folding intent. No fold state enters the transcript.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::{
    blocks::PresentationBlockId,
    conversation::{ConversationState, FinalOutput, HistoryEntry, Selection, SelectionScope},
    session::StreamSnapshotId,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FoldKey {
    Span {
        start: usize,
        end: usize,
    },
    Message {
        history_index: usize,
    },
    Block {
        history_index: usize,
        id: PresentationBlockId,
    },
    Item {
        history_index: usize,
        content_index: usize,
    },
}

impl FoldKey {
    pub fn for_selection(
        conversation: &ConversationState,
        selection: Selection,
        scope: SelectionScope,
        diagnostics_visible: bool,
    ) -> Option<Self> {
        let Selection {
            history_index,
            content_index,
        } = selection;
        let entry = conversation.entry(history_index)?;
        if scope == SelectionScope::Message {
            return entry
                .message_fold_eligible(diagnostics_visible)
                .then_some(Self::Message { history_index });
        }
        if !entry.index_is_selectable(content_index, diagnostics_visible) {
            return None;
        }
        Some(match conversation.selection_identity(selection) {
            Some(id) => Self::Block { history_index, id },
            None => Self::Item {
                history_index,
                content_index,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanRole {
    Summary { end: usize },
    Hidden { start: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnFold {
    /// `zm`: show each turn's final output.
    AllFinals,
    /// `zM`: show only the last turn's final output, if it has one.
    LastFinal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FoldMode {
    #[default]
    Expanded,
    /// Ordinary local fold: keep a one-line preview of the body.
    Preview,
    /// Bulk fold: show the disclosure summary, never the body.
    Summary,
}

impl FoldMode {
    pub fn is_folded(self) -> bool {
        self != Self::Expanded
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BulkFold {
    Summary,
    /// Remove this intent if a folded preview becomes a committed answer.
    Provisional,
}

/// Membership records user intent, even when a body currently fits one row.
/// New items are always expanded; turn folding never changes a global default.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FoldState {
    folded: HashSet<FoldKey>,
    bulk: HashMap<FoldKey, BulkFold>,
    stream: Option<StreamSnapshotId>,
    // Non-overlapping inclusive ranges, keyed by start. Rendering and turn
    // navigation can find a row's representative without scanning other folds.
    spans: BTreeMap<usize, usize>,
}

impl FoldState {
    pub fn contains(&self, key: FoldKey) -> bool {
        match key {
            FoldKey::Span { start, end } => self.spans.get(&start) == Some(&end),
            _ => self.mode(key).is_folded(),
        }
    }

    pub fn mode(&self, key: FoldKey) -> FoldMode {
        if self.bulk.contains_key(&key)
            || matches!(key, FoldKey::Span { start, end } if self.spans.get(&start) == Some(&end))
        {
            FoldMode::Summary
        } else if self.folded.contains(&key) {
            FoldMode::Preview
        } else {
            FoldMode::Expanded
        }
    }

    pub fn toggle(&mut self, key: FoldKey) {
        if self.contains(key) {
            self.unfold(key);
        } else {
            self.fold(key);
        }
    }

    pub fn fold(&mut self, key: FoldKey) {
        if let FoldKey::Span { start, end } = key {
            debug_assert!(start <= end);
            // Supersede only overlapping spans; no nested folds are affected.
            let overlaps = self
                .spans
                .range(..=end)
                .rev()
                .take_while(|(_, old_end)| **old_end >= start)
                .map(|(&old_start, _)| old_start)
                .collect::<Vec<_>>();
            for old_start in overlaps {
                self.spans.remove(&old_start);
            }
            self.spans.insert(start, end);
        } else if !self.contains(key) {
            self.folded.insert(key);
        }
    }

    pub fn unfold(&mut self, key: FoldKey) {
        match key {
            FoldKey::Span { start, end } => {
                if self.spans.get(&start) == Some(&end) {
                    self.spans.remove(&start);
                }
            }
            _ => {
                self.folded.remove(&key);
                self.bulk.remove(&key);
            }
        }
    }

    /// Replace the bulk result for the current snapshot, without resetting
    /// unrelated manual folds. A leading headerless run is its own display turn.
    pub fn fold_turns(
        &mut self,
        conversation: &ConversationState,
        diagnostics_visible: bool,
        fold: TurnFold,
    ) {
        self.spans.clear();
        self.bulk.clear();
        self.stream = None;
        let history = conversation.history();
        let growing = conversation.growing_entries();
        let starts = std::iter::once(0)
            .chain((1..history.len()).filter(|&index| history[index].has_prompt_header()));
        let mut groups = starts.chain(std::iter::once(history.len())).peekable();
        while let Some(start) = groups.next()
            && let Some(&end) = groups.peek()
            && start < end
        {
            let group = start..end;
            let output = (fold == TurnFold::AllFinals || end == history.len())
                .then(|| conversation.final_output(group.clone(), diagnostics_visible))
                .flatten();
            let mut span = None;
            for index in group {
                let entry = &history[index];
                let protected = output
                    .as_ref()
                    .filter(|(at, _)| *at == index)
                    .map(|(_, output)| output);
                let in_place = growing.contains(&index);
                if protected.is_some()
                    || in_place
                    || matches!(entry, HistoryEntry::CompactionDivider)
                {
                    self.finish_span(&mut span);
                }
                if !entry.message_fold_eligible(diagnostics_visible) {
                    continue;
                }
                let message = FoldKey::Message {
                    history_index: index,
                };
                if protected.is_some() {
                    self.unfold(message);
                }
                match (protected, entry) {
                    (Some(FinalOutput::PlanArtifact), _) => self.unfold(FoldKey::Item {
                        history_index: index,
                        content_index: 0,
                    }),
                    (protected, HistoryEntry::Conversation(conversation_entry))
                        if protected.is_some() || in_place =>
                    {
                        for (content_index, block) in conversation_entry.blocks.iter().enumerate() {
                            let key = FoldKey::Block {
                                history_index: index,
                                id: block.id,
                            };
                            if matches!(protected, Some(FinalOutput::Answer(ids)) if ids.contains(&block.id))
                            {
                                self.unfold(key);
                                self.unfold(FoldKey::Item {
                                    history_index: index,
                                    content_index,
                                });
                            } else if block.visible(diagnostics_visible) {
                                let intent = if in_place
                                    && !conversation.is_committed_answer(index, block.id)
                                {
                                    BulkFold::Provisional
                                } else {
                                    BulkFold::Summary
                                };
                                self.bulk.insert(key, intent);
                            }
                        }
                    }
                    _ if entry.has_prompt_header() => {
                        self.finish_span(&mut span);
                        self.bulk.insert(message, BulkFold::Summary);
                    }
                    _ => {
                        let (_, end) = span.get_or_insert((index, index));
                        *end = index;
                    }
                }
            }
            self.finish_span(&mut span);
        }
    }

    fn finish_span(&mut self, span: &mut Option<(usize, usize)>) {
        if let Some((start, end)) = span.take() {
            self.spans.insert(start, end);
        }
    }

    pub(crate) fn fold_stream(&mut self, snapshot: Option<StreamSnapshotId>) {
        self.stream = snapshot;
    }

    pub fn stream_is_folded(&self, snapshot: Option<StreamSnapshotId>) -> bool {
        snapshot.is_some() && snapshot == self.stream
    }

    pub fn stream_folds(&self, snapshot: Option<StreamSnapshotId>) -> EntryFolds<'_> {
        EntryFolds {
            stream_summary: self.stream_is_folded(snapshot),
            ..EntryFolds::none()
        }
    }

    pub fn span_containing(&self, history_index: usize) -> Option<(usize, usize)> {
        let (&start, &end) = self.spans.range(..=history_index).next_back()?;
        (history_index <= end).then_some((start, end))
    }

    pub fn unfold_all(&mut self) {
        self.clear();
    }

    pub fn entry(&self, history_index: usize) -> EntryFolds<'_> {
        EntryFolds {
            state: Some(self),
            history_index,
            stream_summary: false,
        }
    }

    pub fn reconcile(&mut self, conversation: &ConversationState) {
        let history = conversation.history();
        self.spans.retain(|&start, end| {
            history.get(start..=*end).is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.selectable_upper_bound() > 0)
                    && !entries
                        .iter()
                        .any(|entry| matches!(entry, HistoryEntry::CompactionDivider))
            })
        });
        let resolve = |key| match key {
            FoldKey::Span { .. } => unreachable!("only local folds are stored here"),
            FoldKey::Message { history_index } => conversation
                .entry(history_index)
                .is_some_and(|entry| entry.selectable_upper_bound() > 0)
                .then_some(key),
            FoldKey::Block { history_index, id } => {
                let (history_index, id) = conversation.resolved_identity((history_index, id));
                conversation
                    .selection_for_identity(history_index, id)
                    .is_some()
                    .then_some(FoldKey::Block { history_index, id })
            }
            FoldKey::Item {
                history_index,
                content_index,
            } => conversation
                .entry(history_index)
                .is_some_and(|entry| {
                    !matches!(entry, HistoryEntry::Conversation(_))
                        && content_index < entry.selectable_upper_bound()
                })
                .then_some(key),
        };
        self.folded = self.folded.drain().filter_map(resolve).collect();
        self.bulk = self
            .bulk
            .drain()
            .filter_map(|(key, intent)| {
                let key = resolve(key)?;
                if intent == BulkFold::Provisional
                    && let FoldKey::Block { history_index, id } = key
                    && conversation.is_committed_answer(history_index, id)
                {
                    return None;
                }
                Some((key, intent))
            })
            .collect();
    }

    pub fn clear(&mut self) {
        self.folded.clear();
        self.bulk.clear();
        self.spans.clear();
        self.stream = None;
    }

    #[cfg(test)]
    fn keys(&self) -> HashSet<FoldKey> {
        self.folded
            .iter()
            .chain(self.bulk.keys())
            .copied()
            .chain(
                self.spans
                    .iter()
                    .map(|(&start, &end)| FoldKey::Span { start, end }),
            )
            .collect()
    }
}

/// Read-only fold modes for one history entry or an ephemeral tail snapshot.
#[derive(Clone, Copy)]
pub struct EntryFolds<'a> {
    state: Option<&'a FoldState>,
    history_index: usize,
    stream_summary: bool,
}

impl EntryFolds<'_> {
    pub const fn none() -> Self {
        Self {
            state: None,
            history_index: 0,
            stream_summary: false,
        }
    }

    pub fn span(self) -> Option<SpanRole> {
        let (start, end) = self.state?.span_containing(self.history_index)?;
        Some(if self.history_index == start {
            SpanRole::Summary { end }
        } else {
            SpanRole::Hidden { start }
        })
    }

    pub fn is_message_folded(self) -> bool {
        self.message_mode().is_folded()
    }

    pub fn message_mode(self) -> FoldMode {
        self.mode(FoldKey::Message {
            history_index: self.history_index,
        })
    }

    pub fn is_block_folded(self, id: PresentationBlockId) -> bool {
        self.block_mode(id).is_folded()
    }

    pub fn block_mode(self, id: PresentationBlockId) -> FoldMode {
        if self.stream_summary {
            FoldMode::Summary
        } else {
            self.mode(FoldKey::Block {
                history_index: self.history_index,
                id,
            })
        }
    }

    pub fn is_item_folded(self, content_index: usize) -> bool {
        self.item_mode(content_index).is_folded()
    }

    pub fn item_mode(self, content_index: usize) -> FoldMode {
        self.mode(FoldKey::Item {
            history_index: self.history_index,
            content_index,
        })
    }

    fn mode(self, key: FoldKey) -> FoldMode {
        self.state
            .map_or(FoldMode::Expanded, |state| state.mode(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{BlockVisibility, DisplayTurn, NativeHeader};
    use crate::conversation::{EnsembleHistory, ToolCallStatus};
    use rig_core::message::Message;

    fn message(text: &str) -> HistoryEntry {
        HistoryEntry::from_message(Message::user(text), ToolCallStatus::Finished).unwrap()
    }

    fn selection(history_index: usize, content_index: usize) -> Selection {
        Selection {
            history_index,
            content_index,
        }
    }

    #[test]
    fn selection_keys_use_stable_ids_and_exclude_dividers_and_invalid_items() {
        let mut conversation = ConversationState::default();
        conversation.push_entry(message("body"));
        conversation.push_entry(HistoryEntry::Error("error".into()));
        conversation.push_entry(HistoryEntry::CompactionDivider);
        assert_eq!(
            FoldKey::for_selection(&conversation, selection(0, 0), SelectionScope::Block, false),
            Some(FoldKey::Block {
                history_index: 0,
                id: PresentationBlockId(0),
            })
        );
        assert_eq!(
            FoldKey::for_selection(&conversation, selection(1, 0), SelectionScope::Block, false),
            Some(FoldKey::Item {
                history_index: 1,
                content_index: 0,
            })
        );
        for selected in [
            selection(2, 0),
            selection(0, 1),
            selection(1, 1),
            selection(3, 0),
        ] {
            assert_eq!(
                FoldKey::for_selection(&conversation, selected, SelectionScope::Block, false),
                None
            );
        }
        for history_index in [0, 1] {
            assert_eq!(
                FoldKey::for_selection(
                    &conversation,
                    selection(history_index, 0),
                    SelectionScope::Message,
                    false
                ),
                Some(FoldKey::Message { history_index }),
            );
        }
        assert_eq!(
            FoldKey::for_selection(
                &conversation,
                selection(2, 0),
                SelectionScope::Message,
                false
            ),
            None
        );
        assert!(!EntryFolds::none().is_message_folded());
        assert!(!EntryFolds::none().is_block_folded(PresentationBlockId(0)));
        assert!(!EntryFolds::none().is_item_folded(0));
    }

    #[test]
    fn hidden_conversation_blocks_are_not_fold_targets() {
        let mut conversation = ConversationState::default();
        let HistoryEntry::Conversation(mut entry) = message("diagnostic") else {
            unreachable!()
        };
        entry.blocks[0].visibility = BlockVisibility::Diagnostics;
        conversation.push_entry(HistoryEntry::Conversation(entry));
        for scope in [SelectionScope::Block, SelectionScope::Message] {
            assert!(FoldKey::for_selection(&conversation, selection(0, 0), scope, false).is_none());
            assert!(FoldKey::for_selection(&conversation, selection(0, 0), scope, true).is_some());
        }
    }

    #[test]
    fn local_actions_preserve_nested_intent() {
        let span = FoldKey::Span { start: 1, end: 2 };
        let message = FoldKey::Message { history_index: 1 };
        let block = FoldKey::Block {
            history_index: 1,
            id: PresentationBlockId(3),
        };
        let item = FoldKey::Item {
            history_index: 2,
            content_index: 0,
        };
        let mut folds = FoldState::default();
        for key in [block, item, message, span] {
            folds.fold(key);
            folds.fold(key);
        }
        assert_eq!(folds.entry(1).span(), Some(SpanRole::Summary { end: 2 }));
        folds.unfold(span);
        folds.unfold(span);
        assert_eq!(folds.entry(2).span(), None);
        assert!(folds.entry(1).is_message_folded());
        folds.toggle(message);
        assert!(!folds.entry(1).is_message_folded());
        assert!(folds.entry(1).is_block_folded(PresentationBlockId(3)));
        assert!(folds.entry(2).is_item_folded(0));
        folds.unfold_all();
        assert_eq!(folds, FoldState::default());
    }

    fn prompt(text: &str, turn: usize) -> HistoryEntry {
        let HistoryEntry::Conversation(mut entry) = message(text) else {
            unreachable!()
        };
        entry.header = Some(NativeHeader::Prompt(DisplayTurn(turn)));
        HistoryEntry::Conversation(entry)
    }

    fn state(entries: impl IntoIterator<Item = HistoryEntry>) -> ConversationState {
        let mut conversation = ConversationState::default();
        for entry in entries {
            conversation.push_entry(entry);
        }
        conversation
    }

    fn block_key(conversation: &ConversationState, row: usize, part: usize) -> FoldKey {
        FoldKey::for_selection(
            conversation,
            selection(row, part),
            SelectionScope::Block,
            true,
        )
        .unwrap()
    }

    fn indexed_response() -> zevria_transcript::TranscriptItem {
        use serde_json::json;
        zevria_transcript::TranscriptItem::provider_message(zevria_model::ProviderReplay::openai_responses(
            zevria_foundation::ModelProfileRef::new("fixture", "model"),
            vec![
                json!({"type":"message","role":"assistant","status":"completed","id":"commentary","content":[{"type":"output_text","text":"Interim prose"}]}),
                json!({"type":"web_search_call","id":"search","status":"completed","action":{"type":"search","query":"cats"}}),
                json!({"type":"message","role":"assistant","status":"completed","id":"final","content":[
                    {"type":"output_text","text":"First part"},
                    {"type":"output_text","text":"Second part","annotations":[{"type":"url_citation","url":"https://example.com/reference","title":"Reference","start_index":0,"end_index":6}]}
                ]}),
                json!({"type":"web_search_call","id":"open","status":"completed","action":{"type":"open_page","url":"https://example.net/page"}}),
                json!({"type":"web_search_call","id":"find","status":"completed","action":{"type":"find_in_page","url":"https://example.net/page","pattern":"needle"}}),
            ],
        )).unwrap()
    }

    fn tool_round() -> Message {
        use rig_core::message::{AssistantContent, CallId, ToolCall, ToolFunction, ToolName};
        Message::from(vec![
            AssistantContent::text("Tool preamble"),
            AssistantContent::ToolCall(ToolCall::new(
                CallId::from_wire("call"),
                ToolFunction::new(ToolName::new("tool").unwrap(), serde_json::json!({})),
            )),
        ])
    }

    #[test]
    fn bulk_folds_set_final_answer_visibility_for_live_and_restored_turns() {
        use crate::session_model::SessionModel;
        use zevria_content::ResponseAttemptOutcome;
        use zevria_foundation::{SessionMode, TurnId};
        use zevria_session_api::SessionEvent as E;
        use zevria_transcript::{IncompleteResponse, TranscriptItem, restored_attempt};

        let item = indexed_response();
        let canonical = item.display_message().unwrap();
        let mut live = SessionModel::default();
        let mut saved = Vec::new();
        for i in 1..=2 {
            let turn_id = TurnId::new(i);
            let prompt = Message::user(format!("turn {i}"));
            let mut attempt = restored_attempt(&item, i as usize).unwrap();
            let mut trailing = attempt.clone();
            trailing.id = format!("interrupted-{i}");
            trailing.close_uncommitted(ResponseAttemptOutcome::Interrupted);
            live.reduce(E::TurnStarted {
                turn_id,
                message: prompt.clone(),
                mode: SessionMode::Build,
            });
            live.reduce(E::ModelCallStarted { turn_id, call: 1 });
            live.reduce(E::Intermediate {
                turn_id,
                message: tool_round(),
                attempt_id: None,
            });
            live.reduce(E::ModelCallStarted { turn_id, call: 2 });
            attempt.touch();
            live.reduce(E::ResponseAttemptUpdated {
                turn_id,
                attempt: attempt.clone(),
            });
            live.reduce(E::TurnCompleted {
                turn_id,
                message: canonical.clone(),
                attempt_id: Some(attempt.id),
            });
            live.conversation.update_response_attempt(trailing.clone());
            live.conversation.push_error("trailing notice".into());
            live.conversation.push_compaction_divider();
            saved.extend([
                TranscriptItem::Message(prompt),
                TranscriptItem::Message(tool_round()),
                item.clone(),
                TranscriptItem::IncompleteResponse(
                    IncompleteResponse::from_attempt(&trailing)
                        .unwrap()
                        .unwrap(),
                ),
                TranscriptItem::Error {
                    error: "trailing notice".into(),
                },
            ]);
        }
        let mut restored = ConversationState::default();
        restored.restore(saved);
        for (conversation, final_rows) in [(&live.conversation, [2, 8]), (&restored, [2, 7])] {
            let mut folds = FoldState::default();
            for row in final_rows {
                for key in [
                    FoldKey::Message { history_index: row },
                    block_key(conversation, row, 2),
                    block_key(conversation, row, 3),
                ] {
                    folds.fold(key);
                }
            }
            let manual_tool = block_key(conversation, 1, 1);
            folds.fold(manual_tool);
            for action in [
                TurnFold::AllFinals,
                TurnFold::LastFinal,
                TurnFold::AllFinals,
            ] {
                folds.fold_turns(conversation, false, action);
                for (i, row) in final_rows.into_iter().enumerate() {
                    if i == 0 && action == TurnFold::LastFinal {
                        assert_eq!(folds.span_containing(row), Some((row - 1, row + 2)));
                        continue;
                    }
                    assert_eq!(folds.entry(row).span(), None);
                    assert!(!folds.entry(row).is_message_folded());
                    for part in [2, 3] {
                        assert_eq!(
                            folds.mode(block_key(conversation, row, part)),
                            FoldMode::Expanded
                        );
                    }
                    for part in [0, 1, 4, 5] {
                        assert_eq!(
                            folds.mode(block_key(conversation, row, part)),
                            FoldMode::Summary
                        );
                    }
                    assert_eq!(folds.span_containing(row + 1), Some((row + 1, row + 2)));
                }
                assert_eq!(folds.mode(manual_tool), FoldMode::Preview);
                let before = folds.clone();
                folds.fold_turns(conversation, false, action);
                assert_eq!(folds, before);
            }
            let web = block_key(conversation, final_rows[1], 1);
            folds.toggle(web);
            assert_eq!(folds.mode(web), FoldMode::Expanded);
            folds.fold(web);
            assert_eq!(folds.mode(web), FoldMode::Preview);
            folds.unfold_all();
            assert_eq!(folds, FoldState::default());
        }
    }

    #[test]
    fn display_boundaries_keep_plan_outputs_and_dividers_but_not_prompt_only_turns() {
        use zevria_workflow::{
            EnsembleRunId, EnsembleWorkflow, PlanArtifact, PlanHandoff, PlanId, PlanVersion,
        };
        let artifact = PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "Plan body".into(),
            source_turn_id: zevria_foundation::TurnId::new(1),
        };
        let mut conversation = state([
            message("leading prompt"),
            HistoryEntry::from_message(
                Message::assistant("leading output"),
                ToolCallStatus::Finished,
            )
            .unwrap(),
            prompt("first", 1),
            HistoryEntry::PlanArtifact(artifact.clone()),
            HistoryEntry::CompactionDivider,
        ]);
        conversation.complete_response(Message::assistant("Plan prose"), None, None);
        conversation.push_entry(HistoryEntry::PlanHandoff(
            PlanHandoff::new(artifact, "source"),
            Some(NativeHeader::Prompt(DisplayTurn(2))),
        ));
        conversation.complete_response(Message::assistant("handoff answer"), None, None);
        conversation.push_entry(HistoryEntry::Ensemble(EnsembleHistory {
            header: Some(NativeHeader::Prompt(DisplayTurn(3))),
            run_id: EnsembleRunId::from_string("span-ensemble"),
            workflow: EnsembleWorkflow::Review,
            prompt: "review".into(),
            workers: Vec::new(),
            baseline: None,
        }));
        let mut folds = FoldState::default();
        let manual_plan = FoldKey::Item {
            history_index: 3,
            content_index: 0,
        };
        folds.fold(manual_plan);
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        assert!(!folds.contains(manual_plan));
        for row in [1, 3, 4, 7] {
            assert_eq!(folds.entry(row).span(), None);
            assert!(!folds.entry(row).is_message_folded());
        }
        assert_eq!(folds.span_containing(0), Some((0, 0)));
        assert_eq!(
            folds.span_containing(5),
            Some((5, 5)),
            "Plan, not closing prose"
        );
        for row in [2, 6, 8] {
            assert_eq!(folds.entry(row).message_mode(), FoldMode::Summary);
        }
        folds.fold_turns(&conversation, false, TurnFold::LastFinal);
        for row in [1, 3, 7] {
            assert!(
                folds.span_containing(row).is_some(),
                "no previous answer fallback"
            );
        }
        assert_eq!(folds.span_containing(4), None);
        assert!(folds.entry(8).is_message_folded());
    }

    #[test]
    fn in_place_previews_fold_by_id_and_open_on_commit_not_on_revision() {
        use zevria_transcript::restored_attempt;
        let item = indexed_response();
        let mut attempt = restored_attempt(&item, 0).unwrap();
        let later = attempt.presentation.split_off(2);
        let mut conversation = state([prompt("prompt", 1)]);
        conversation.update_response_attempt(attempt.clone());
        let first = block_key(&conversation, 1, 2);
        let mut folds = FoldState::default();
        let old_web = block_key(&conversation, 1, 1);
        folds.fold(old_web);
        folds.fold_turns(&conversation, false, TurnFold::LastFinal);
        assert_eq!(folds.mode(first), FoldMode::Summary);
        assert!(!folds.entry(1).is_message_folded() && folds.entry(1).span().is_none());
        attempt.presentation.extend(later);
        attempt.touch();
        conversation.update_response_attempt(attempt.clone());
        folds.reconcile(&conversation);
        let second = block_key(&conversation, 1, 3);
        assert_eq!(folds.mode(second), FoldMode::Expanded);
        assert_eq!(folds.mode(first), FoldMode::Summary);
        conversation.complete_response(item.display_message().unwrap(), Some(&attempt.id), None);
        folds.reconcile(&conversation);
        assert_eq!(folds.mode(first), FoldMode::Expanded);
        assert_eq!(folds.mode(old_web), FoldMode::Summary);
        folds.unfold_all();
        assert_eq!(folds, FoldState::default());
    }

    #[test]
    fn latest_failed_or_interim_turn_has_no_final_output() {
        let mut conversation = state([prompt("completed", 1)]);
        conversation.complete_response(Message::assistant("older answer"), None, None);
        conversation.push_entry(prompt("unfinished", 2));
        conversation.commit_response(
            Message::assistant("interim without tool"),
            None,
            ToolCallStatus::Executing,
            None,
        );
        conversation.push_error("failure".into());
        let mut folds = FoldState::default();
        folds.fold_turns(&conversation, false, TurnFold::LastFinal);
        assert_eq!(folds.span_containing(1), Some((1, 1)));
        assert!(folds.entry(2).is_message_folded());
        assert_eq!(folds.span_containing(3), Some((3, 4)));
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        assert_eq!(folds.span_containing(1), None);
        assert_eq!(folds.span_containing(3), Some((3, 4)));

        use zevria_transcript::TranscriptItem;
        conversation.restore(vec![
            TranscriptItem::Message(Message::user("completed")),
            TranscriptItem::Message(Message::assistant("older answer")),
            TranscriptItem::Message(Message::user("cancelled")),
            TranscriptItem::Message(Message::assistant("retained partial body")),
            TranscriptItem::Message(tool_round()),
            TranscriptItem::Error {
                error: crate::blocks::TURN_CANCELLED.into(),
            },
        ]);
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        assert!(folds.span_containing(1).is_none());
        assert_eq!(folds.span_containing(3), Some((3, 5)));
    }

    #[test]
    fn lossy_headerless_preview_is_provisional_until_completed() {
        use crate::agent_transcript::AgentTranscriptReducer;
        use zevria_workflow::{AgentRunEvent, AgentRunStatus};
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::Prompt {
                text: "prompt".into(),
                continuation: false,
                repair: None,
            },
        );
        reducer.apply_preview(
            &mut conversation,
            AgentRunEvent::AgentMessage {
                text: "provisional answer".into(),
                message_id: Some("preview".into()),
            },
        );
        let answer = block_key(&conversation, 0, 1);
        let mut folds = FoldState::default();
        folds.fold_turns(&conversation, false, TurnFold::LastFinal);
        assert_eq!(folds.mode(answer), FoldMode::Summary);
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::Status {
                status: AgentRunStatus::Completed,
                detail: None,
            },
        );
        folds.reconcile(&conversation);
        assert_eq!(folds.mode(answer), FoldMode::Expanded);
        assert_eq!(
            folds.mode(block_key(&conversation, 0, 0)),
            FoldMode::Summary
        );
    }

    #[test]
    fn headerless_fallback_ignores_diagnostics_and_incomplete_attempts() {
        use zevria_content::ResponseAttemptOutcome;
        let mut diagnostic = message("hidden diagnostic");
        let HistoryEntry::Conversation(entry) = &mut diagnostic else {
            unreachable!()
        };
        entry.blocks[0].visibility = BlockVisibility::Diagnostics;
        let mut conversation = state([diagnostic, message("old ACP prompt")]);
        conversation.push_message(Message::assistant("ACP answer"), ToolCallStatus::Finished);
        conversation.push_entry(HistoryEntry::Error("trailing notice".into()));
        let mut incomplete = zevria_transcript::restored_attempt(&indexed_response(), 0).unwrap();
        incomplete.close_uncommitted(ResponseAttemptOutcome::Failed);
        conversation.update_response_attempt(incomplete);
        let mut folds = FoldState::default();
        for (visible, start) in [(false, 1), (true, 0)] {
            folds.fold_turns(&conversation, visible, TurnFold::AllFinals);
            assert_eq!(folds.span_containing(1), Some((start, 1)));
            assert_eq!(folds.entry(2).span(), None);
            assert_eq!(folds.span_containing(4), Some((3, 4)));
        }
        conversation.push_entry(message("new cycle"));
        assert_eq!(folds.span_containing(5), None);
    }

    #[test]
    fn fold_spans_replace_overlaps_but_preserve_adjacent_spans_and_inner_intent() {
        let mut folds = FoldState::default();
        folds.fold(FoldKey::Message { history_index: 1 });
        folds.fold(FoldKey::Span { start: 0, end: 2 });
        folds.fold(FoldKey::Span { start: 4, end: 4 });
        folds.fold(FoldKey::Span { start: 2, end: 3 });
        assert_eq!(folds.span_containing(0), None);
        assert_eq!(folds.span_containing(2), Some((2, 3)));
        assert_eq!(folds.span_containing(4), Some((4, 4)));
        folds.fold(FoldKey::Span { start: 1, end: 5 });
        assert_eq!(folds.keys().len(), 2);
        folds.fold(FoldKey::Span { start: 3, end: 3 });
        assert_eq!(folds.span_containing(2), None);
        assert_eq!(folds.span_containing(3), Some((3, 3)));
        assert!(folds.entry(1).is_message_folded());
        folds.unfold_all();
        assert!(folds.keys().is_empty());
    }

    #[test]
    fn fold_turns_reconcile_prunes_truncation_and_empty_ranges() {
        for truncate in [false, true] {
            let mut conversation = state([prompt("first", 1)]);
            conversation.complete_response(Message::assistant("last"), None, None);
            conversation.push_entry(prompt("next", 2));
            conversation.push_entry(message("middle"));
            conversation.complete_response(Message::assistant("last"), None, None);
            let mut folds = FoldState::default();
            folds.fold_turns(&conversation, false, TurnFold::AllFinals);
            assert_eq!(
                folds.keys(),
                HashSet::from([
                    FoldKey::Message { history_index: 0 },
                    FoldKey::Message { history_index: 2 },
                    FoldKey::Span { start: 3, end: 3 },
                ])
            );
            if truncate {
                conversation.commit_edit(3, false);
            } else {
                let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(3) else {
                    unreachable!()
                };
                entry.blocks.clear();
            }
            folds.reconcile(&conversation);
            assert_eq!(
                folds.keys(),
                HashSet::from([
                    FoldKey::Message { history_index: 0 },
                    FoldKey::Message { history_index: 2 },
                ])
            );
            if truncate {
                conversation.push_entry(message("replacement"));
                assert_eq!(folds.span_containing(3), None);
                assert!(!folds.entry(3).is_message_folded());
            }
            let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(0) else {
                unreachable!()
            };
            entry.blocks[0].visibility = BlockVisibility::Diagnostics;
            folds.reconcile(&conversation);
            assert!(folds.entry(0).is_message_folded(), "hidden intent survives");
            for index in [0, 2] {
                let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(index) else {
                    unreachable!()
                };
                entry.blocks.clear();
                folds.reconcile(&conversation);
                assert!(!folds.entry(index).is_message_folded());
            }
            assert!(folds.keys().is_empty());
        }
    }

    #[test]
    fn reconcile_never_hides_a_new_compaction_divider_at_a_reused_position() {
        let mut conversation = ConversationState::default();
        for entry in [message("first"), message("to edit")] {
            conversation.push_entry(entry);
        }
        conversation.complete_response(Message::assistant("last"), None, None);
        let mut folds = FoldState::default();
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        assert_eq!(folds.span_containing(1), Some((0, 1)));
        conversation.commit_edit(1, true);
        assert!(matches!(
            conversation.entry(1),
            Some(HistoryEntry::CompactionDivider)
        ));
        folds.reconcile(&conversation);
        assert_eq!(folds.span_containing(1), None);
    }

    #[test]
    fn reconcile_prunes_truncated_entries_after_commit_edit() {
        let mut conversation = ConversationState::default();
        for entry in [
            message("retained"),
            HistoryEntry::Error("retained".into()),
            HistoryEntry::Error("removed".into()),
            message("removed"),
        ] {
            conversation.push_entry(entry);
        }
        let mut folds = FoldState::default();
        for history_index in 0..conversation.history().len() {
            folds.fold(FoldKey::Message { history_index });
        }
        assert_eq!(folds.keys().len(), 4);
        assert!(
            folds
                .keys()
                .iter()
                .all(|key| matches!(key, FoldKey::Message { .. }))
        );
        conversation.commit_edit(2, true);
        folds.reconcile(&conversation);
        assert_eq!(folds.keys().len(), 2);
        assert!(folds.entry(0).is_message_folded());
        assert!(folds.entry(1).is_message_folded());
        // Reused positions in the new branch do not inherit removed intent.
        conversation.push_entry(message("new item"));
        assert!(!folds.entry(3).is_message_folded());
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(0) else {
            panic!("conversation")
        };
        entry.blocks.clear();
        folds.reconcile(&conversation);
        assert!(!folds.entry(0).is_message_folded());
        assert_eq!(folds.keys().len(), 1);
    }

    #[test]
    fn reconcile_follows_identity_aliases_without_losing_unrelated_folds() {
        use zevria_content::{
            AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
            AssistantSourceAddress, ResponseAttempt,
        };
        let mut conversation = ConversationState::default();
        conversation.push_entry(message("initial projection"));
        conversation.push_entry(HistoryEntry::Error("unrelated".into()));
        let mut folds = FoldState::default();
        folds.fold(FoldKey::Block {
            history_index: 0,
            id: PresentationBlockId(0),
        });
        folds.fold(FoldKey::Item {
            history_index: 1,
            content_index: 0,
        });
        let source = AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Summary(0),
            item_id: Some("thought".into()),
        };
        let mut attempt =
            ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("provider", "model"));
        attempt.presentation.push(AssistantPresentationPart {
            source: source.clone(),
            content: AssistantPresentationContent::Reasoning {
                text: "ordered projection".into(),
            },
        });
        attempt.touch();
        conversation.update_native_attempt(attempt.clone(), None);
        let old = (0, PresentationBlockId(0));
        conversation.alias_attempt_part(old, &attempt.id, &source);
        let resolved = conversation.resolved_identity(old);
        assert_ne!(resolved, old);
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(0) else {
            panic!("conversation")
        };
        entry.blocks.clear();
        folds.reconcile(&conversation);
        assert_eq!(folds.keys().len(), 2);
        assert!(!folds.entry(0).is_block_folded(old.1));
        assert!(folds.entry(resolved.0).is_block_folded(resolved.1));
        assert!(folds.entry(1).is_item_folded(0));
        folds.reconcile(&conversation);
        assert_eq!(folds.keys().len(), 2, "reconciliation is idempotent");
    }

    #[test]
    fn bulk_reopens_output_but_preserves_unrelated_manual_intent() {
        let mut conversation = state([
            prompt("prompt", 1),
            message("earlier message"),
            HistoryEntry::Error("error".into()),
        ]);
        conversation.complete_response(Message::assistant("final message"), None, None);
        let manual = [
            block_key(&conversation, 1, 0),
            block_key(&conversation, 2, 0),
        ];
        let mut folds = FoldState::default();
        for key in manual.into_iter().chain([
            FoldKey::Message { history_index: 3 },
            block_key(&conversation, 3, 0),
        ]) {
            folds.fold(key);
        }
        for action in [TurnFold::AllFinals, TurnFold::LastFinal] {
            folds.fold_turns(&conversation, false, action);
            assert!(!folds.entry(3).is_message_folded());
            assert_eq!(
                folds.mode(block_key(&conversation, 3, 0)),
                FoldMode::Expanded
            );
            for key in manual {
                assert_eq!(folds.mode(key), FoldMode::Preview);
            }
        }
    }
}
