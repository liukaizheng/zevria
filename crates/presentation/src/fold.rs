//! Pane-local, explicit folding intent. No fold state enters the transcript.

use std::collections::{BTreeMap, HashSet};

use crate::{
    blocks::{PresentationBlockId, PresentationBlockKind},
    conversation::{ConversationState, HistoryEntry, Selection, SelectionScope},
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
    /// `zm`: fold prompts and earlier-message spans without adding final-entry folds.
    FinalExpanded,
    /// `zM`: older turns also message-fold their final eligible entry.
    OlderFinalFolded,
}

/// Membership records user intent, even when a body currently fits one row.
/// New items are always expanded; turn folding never changes a global default.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FoldState {
    folded: HashSet<FoldKey>,
    // Non-overlapping inclusive ranges, keyed by start. Rendering and turn
    // navigation can find a row's representative without scanning other folds.
    spans: BTreeMap<usize, usize>,
}

impl FoldState {
    pub fn contains(&self, key: FoldKey) -> bool {
        match key {
            FoldKey::Span { start, end } => self.spans.get(&start) == Some(&end),
            _ => self.folded.contains(&key),
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
        } else {
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
            }
        }
    }

    /// Fold each display turn's prompt separately from its earlier-message spans.
    /// Headerless entries join the current turn; a leading headerless run is its
    /// own turn and has no prompt row. Existing Message folds are never removed.
    /// Both actions also fold visible thoughts in the newest turn's final eligible entry.
    pub fn fold_turns(
        &mut self,
        history: &[HistoryEntry],
        diagnostics_visible: bool,
        fold: TurnFold,
    ) {
        let mut groups = Vec::new();
        let mut start = 0;
        for (index, entry) in history.iter().enumerate() {
            if entry.has_prompt_header() {
                if start < index {
                    groups.push(start..index);
                }
                start = index;
            }
        }
        if start < history.len() {
            groups.push(start..history.len());
        }
        let latest = groups.len().saturating_sub(1);
        for (index, group) in groups.into_iter().enumerate() {
            let Some(last) = group
                .clone()
                .rev()
                .find(|&index| history[index].message_fold_eligible(diagnostics_visible))
            else {
                continue;
            };
            self.fold_turn_group(
                history,
                group.start,
                last,
                diagnostics_visible,
                fold == TurnFold::OlderFinalFolded && index < latest,
            );
            if index == latest
                && let HistoryEntry::Conversation(entry) = &history[last]
            {
                for block in &entry.blocks {
                    if block.visible(diagnostics_visible)
                        && matches!(block.kind, PresentationBlockKind::Reasoning { .. })
                    {
                        self.fold(FoldKey::Block {
                            history_index: last,
                            id: block.id,
                        });
                    }
                }
            }
        }
    }

    fn fold_turn_group(
        &mut self,
        history: &[HistoryEntry],
        start: usize,
        last: usize,
        diagnostics_visible: bool,
        fold_final: bool,
    ) {
        let prompt = history[start].has_prompt_header().then_some(start);
        if let Some(index) = prompt
            && index != last
            && history[index].message_fold_eligible(diagnostics_visible)
        {
            self.fold(FoldKey::Message {
                history_index: index,
            });
        }
        if fold_final {
            self.fold(FoldKey::Message {
                history_index: last,
            });
        }
        let earlier_start = prompt.map_or(start, |index| index + 1);
        let mut span = None;
        for (index, entry) in history.iter().enumerate().take(last).skip(earlier_start) {
            if matches!(entry, HistoryEntry::CompactionDivider) {
                if let Some((start, end)) = span.take() {
                    self.fold(FoldKey::Span { start, end });
                }
            } else if entry.message_fold_eligible(diagnostics_visible) {
                let (_, end) = span.get_or_insert((index, index));
                *end = index;
            }
        }
        if let Some((start, end)) = span {
            self.fold(FoldKey::Span { start, end });
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
        self.folded = self
            .folded
            .drain()
            .filter_map(|key| match key {
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
            })
            .collect();
    }

    pub fn clear(&mut self) {
        self.folded.clear();
        self.spans.clear();
    }

    #[cfg(test)]
    fn keys(&self) -> HashSet<FoldKey> {
        self.folded
            .iter()
            .copied()
            .chain(
                self.spans
                    .iter()
                    .map(|(&start, &end)| FoldKey::Span { start, end }),
            )
            .collect()
    }
}

/// An entry-scoped read-only view; the streamed tail always uses `none()`.
#[derive(Clone, Copy)]
pub struct EntryFolds<'a> {
    state: Option<&'a FoldState>,
    history_index: usize,
}

impl EntryFolds<'_> {
    pub const fn none() -> Self {
        Self {
            state: None,
            history_index: 0,
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
        self.state.is_some_and(|state| {
            state.contains(FoldKey::Message {
                history_index: self.history_index,
            })
        })
    }

    pub fn is_block_folded(self, id: PresentationBlockId) -> bool {
        self.state.is_some_and(|state| {
            state.contains(FoldKey::Block {
                history_index: self.history_index,
                id,
            })
        })
    }

    pub fn is_item_folded(self, content_index: usize) -> bool {
        self.state.is_some_and(|state| {
            state.contains(FoldKey::Item {
                history_index: self.history_index,
                content_index,
            })
        })
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

    #[test]
    fn fold_turns_group_prompt_headers_and_split_at_dividers() {
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
        let mut assistant =
            HistoryEntry::from_message(Message::assistant("call"), ToolCallStatus::Finished)
                .unwrap();
        if let HistoryEntry::Conversation(entry) = &mut assistant {
            entry.header = Some(NativeHeader::Assistant {
                turn: DisplayTurn(1),
                call: 1,
            });
        }
        let history = [
            message("leading"),
            HistoryEntry::Error("leading error".into()),
            message("leading last"),
            prompt("first", 1),
            assistant,
            HistoryEntry::Error("error".into()),
            HistoryEntry::PlanArtifact(artifact.clone()),
            HistoryEntry::CompactionDivider,
            message("after divider"),
            message("last call"),
            HistoryEntry::Ensemble(EnsembleHistory {
                header: Some(NativeHeader::Prompt(DisplayTurn(2))),
                run_id: EnsembleRunId::from_string("span-ensemble"),
                workflow: EnsembleWorkflow::Review,
                prompt: "review".into(),
                workers: Vec::new(),
                baseline: None,
            }),
            message("ensemble last"),
            HistoryEntry::PlanHandoff(
                PlanHandoff::new(artifact, "source"),
                Some(NativeHeader::Prompt(DisplayTurn(3))),
            ),
            message("handoff last"),
            prompt("only entry", 4),
        ];
        let mut expected = HashSet::from([
            FoldKey::Span { start: 0, end: 1 },
            FoldKey::Message { history_index: 3 },
            FoldKey::Span { start: 4, end: 6 },
            FoldKey::Span { start: 8, end: 8 },
            FoldKey::Message { history_index: 10 },
            FoldKey::Message { history_index: 12 },
        ]);
        let mut folds = FoldState::default();
        for _ in 0..2 {
            folds.fold_turns(&history, false, TurnFold::FinalExpanded);
            assert_eq!(folds.keys(), expected);
        }
        assert!(folds.entry(3).is_message_folded());
        assert_eq!(folds.entry(4).span(), Some(SpanRole::Summary { end: 6 }));
        assert_eq!(folds.entry(5).span(), Some(SpanRole::Hidden { start: 4 }));
        for index in [2, 3, 7, 9, 10, 11, 12, 13, 14] {
            assert_eq!(folds.span_containing(index), None);
        }
        assert_eq!(EntryFolds::none().span(), None);

        expected.extend([2, 9, 11, 13].map(|history_index| FoldKey::Message { history_index }));
        for _ in 0..2 {
            folds.fold_turns(&history, false, TurnFold::OlderFinalFolded);
            assert_eq!(folds.keys(), expected);
        }
        assert!(!folds.entry(14).is_message_folded());
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        assert_eq!(
            folds.keys(),
            expected,
            "zm never re-expands zM's final entries"
        );
    }

    fn with_thoughts() -> HistoryEntry {
        let mut entry =
            HistoryEntry::from_message(Message::assistant("answer"), ToolCallStatus::Finished)
                .unwrap();
        let HistoryEntry::Conversation(entry) = &mut entry else {
            unreachable!()
        };
        for (id, visibility) in [
            (1, BlockVisibility::Always),
            (2, BlockVisibility::Always),
            (3, BlockVisibility::Diagnostics),
        ] {
            let mut thought = entry.blocks[0].clone();
            thought.id = PresentationBlockId(id);
            thought.visibility = visibility;
            thought.kind = PresentationBlockKind::Reasoning {
                parts: vec![format!("thought {id}")],
            };
            entry.blocks.push(thought);
        }
        HistoryEntry::Conversation(entry.clone())
    }

    #[test]
    fn both_bulk_actions_only_fold_visible_thoughts_in_the_latest_final_entry() {
        for diagnostics in [false, true] {
            for action in [TurnFold::FinalExpanded, TurnFold::OlderFinalFolded] {
                let mut history = vec![
                    prompt("old", 1),
                    with_thoughts(),
                    prompt("new", 2),
                    with_thoughts(),
                    with_thoughts(),
                ];
                let mut folds = FoldState::default();
                folds.fold_turns(&history, diagnostics, action);
                let mut expected = HashSet::from([
                    FoldKey::Message { history_index: 0 },
                    FoldKey::Message { history_index: 2 },
                    FoldKey::Span { start: 3, end: 3 },
                ]);
                if action == TurnFold::OlderFinalFolded {
                    expected.insert(FoldKey::Message { history_index: 1 });
                }
                for id in 1..=if diagnostics { 3 } else { 2 } {
                    expected.insert(FoldKey::Block {
                        history_index: 4,
                        id: PresentationBlockId(id),
                    });
                }
                assert_eq!(folds.keys(), expected);
                folds.fold_turns(&history, diagnostics, action);
                assert_eq!(folds.keys(), expected, "a repeated action is idempotent");

                history.push(message("latest without thoughts"));
                assert!(folds.entry(5).span().is_none());
                assert!(!folds.entry(5).is_message_folded());
                let mut fresh = FoldState::default();
                fresh.fold_turns(&history, diagnostics, action);
                expected.retain(|key| !matches!(key, FoldKey::Block { .. } | FoldKey::Span { .. }));
                expected.insert(FoldKey::Span { start: 3, end: 4 });
                assert_eq!(fresh.keys(), expected, "do not look backward for reasoning");

                history.push(prompt("new prompt alone", 3));
                let mut fresh = FoldState::default();
                fresh.fold_turns(&history, diagnostics, action);
                assert!(
                    !fresh
                        .keys()
                        .iter()
                        .any(|key| matches!(key, FoldKey::Block { .. }))
                );
                assert!(!fresh.entry(6).is_message_folded());
            }
        }
    }

    #[test]
    fn fold_turns_headerless_history_trims_hidden_entries_and_keeps_new_cycles_open() {
        fn diagnostic() -> HistoryEntry {
            let mut entry = message("diagnostic");
            if let HistoryEntry::Conversation(entry) = &mut entry {
                entry.blocks[0].visibility = BlockVisibility::Diagnostics;
            }
            entry
        }
        let mut history = vec![
            diagnostic(),
            message("first cycle"),
            diagnostic(),
            message("last cycle"),
            diagnostic(),
        ];
        let mut folds = FoldState::default();
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        assert_eq!(
            folds.keys(),
            HashSet::from([FoldKey::Span { start: 1, end: 1 }])
        );
        folds.fold_turns(&history, true, TurnFold::FinalExpanded);
        assert_eq!(
            folds.keys(),
            HashSet::from([FoldKey::Span { start: 0, end: 3 }])
        );
        history.push(message("new cycle"));
        assert_eq!(folds.span_containing(4), None);
        assert_eq!(folds.span_containing(5), None);
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        assert_eq!(
            folds.keys(),
            HashSet::from([FoldKey::Span { start: 1, end: 3 }])
        );
    }

    #[test]
    fn fold_turns_skip_hidden_prompts_and_never_span_them() {
        let mut hidden_prompt = prompt("diagnostic prompt", 1);
        let HistoryEntry::Conversation(entry) = &mut hidden_prompt else {
            unreachable!()
        };
        for block in &mut entry.blocks {
            block.visibility = BlockVisibility::Diagnostics;
        }
        let history = [
            hidden_prompt,
            message("first call"),
            message("middle call"),
            message("last call"),
        ];
        for fold in [TurnFold::FinalExpanded, TurnFold::OlderFinalFolded] {
            let mut folds = FoldState::default();
            folds.fold_turns(&history, false, fold);
            assert_eq!(
                folds.keys(),
                HashSet::from([FoldKey::Span { start: 1, end: 2 }])
            );
            folds.fold_turns(&history, true, fold);
            let expected = HashSet::from([
                FoldKey::Message { history_index: 0 },
                FoldKey::Span { start: 1, end: 2 },
            ]);
            assert_eq!(folds.keys(), expected);
            folds.fold_turns(&history, false, fold);
            assert_eq!(folds.keys(), expected, "hidden Message intent survives");
        }
    }

    #[test]
    fn fold_turns_prompt_only_turns_keep_the_latest_open_even_when_ineligible() {
        let mut history = [
            prompt("older prompt only", 1),
            prompt("latest prompt only", 2),
        ];
        let HistoryEntry::Conversation(entry) = &mut history[1] else {
            unreachable!()
        };
        for block in &mut entry.blocks {
            block.visibility = BlockVisibility::Diagnostics;
        }
        for diagnostics in [false, true] {
            let mut folds = FoldState::default();
            for fold in [TurnFold::FinalExpanded, TurnFold::OlderFinalFolded] {
                folds.fold_turns(&[], diagnostics, fold);
                assert!(folds.keys().is_empty());
            }
            folds.fold_turns(&history, diagnostics, TurnFold::FinalExpanded);
            assert!(folds.keys().is_empty());
            folds.fold_turns(&history, diagnostics, TurnFold::OlderFinalFolded);
            let expected = HashSet::from([FoldKey::Message { history_index: 0 }]);
            assert_eq!(folds.keys(), expected);
            folds.fold_turns(&history, diagnostics, TurnFold::FinalExpanded);
            assert_eq!(folds.keys(), expected);
        }
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
            let mut conversation = ConversationState::default();
            for entry in [
                prompt("first", 1),
                message("last"),
                prompt("next", 2),
                message("middle"),
                message("last"),
            ] {
                conversation.push_entry(entry);
            }
            let mut folds = FoldState::default();
            folds.fold_turns(conversation.history(), false, TurnFold::FinalExpanded);
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
        for entry in [message("first"), message("to edit"), message("last")] {
            conversation.push_entry(entry);
        }
        let mut folds = FoldState::default();
        folds.fold_turns(conversation.history(), false, TurnFold::FinalExpanded);
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
    fn fold_turns_preserve_manual_message_block_and_item_intent() {
        let history = [
            prompt("prompt", 1),
            message("earlier message"),
            HistoryEntry::Error("error".into()),
            message("final message"),
        ];
        let manual = [
            FoldKey::Message { history_index: 3 },
            FoldKey::Block {
                history_index: 1,
                id: PresentationBlockId(0),
            },
            FoldKey::Item {
                history_index: 2,
                content_index: 0,
            },
        ];
        let mut folds = FoldState::default();
        for key in manual {
            folds.fold(key);
        }
        let expected = HashSet::from_iter(manual.into_iter().chain([
            FoldKey::Message { history_index: 0 },
            FoldKey::Span { start: 1, end: 2 },
        ]));
        for fold in [
            TurnFold::FinalExpanded,
            TurnFold::OlderFinalFolded,
            TurnFold::FinalExpanded,
        ] {
            folds.fold_turns(&history, false, fold);
            assert_eq!(folds.keys(), expected);
        }
    }
}
