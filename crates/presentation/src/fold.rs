//! Pane-local, explicit folding intent. No fold state enters the transcript.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::{AddAssign, RangeInclusive},
};

use crate::{
    blocks::{
        ConversationEntry, DiagnosticTone, PresentationBlock, PresentationBlockId,
        PresentationBlockKind, PresentationRole, PresentedTool, TURN_CANCELLED, WebActionKind,
        WebActivityPresentation, aggregate_web_status, incomplete_response_outcome,
        native_header_status, subtask_icon,
    },
    conversation::{ConversationState, FinalOutput, HistoryEntry, Selection, SelectionScope},
    session::StreamSnapshotId,
    status_icon::StatusIcon,
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
    pub fn history_index(self) -> usize {
        match self {
            Self::Span { start, .. } => start,
            Self::Message { history_index }
            | Self::Block { history_index, .. }
            | Self::Item { history_index, .. } => history_index,
        }
    }

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
    /// Bulk intent: derive a preview or a grouped summary from the content.
    Summary,
}

impl FoldMode {
    pub fn is_folded(self) -> bool {
        self != Self::Expanded
    }
}

/// Counts of semantic work, not rendered rows, provider calls, or history entries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActivityDigest {
    pub thoughts: usize,
    pub tool_calls: usize,
    pub searches: usize,
    pub pages: usize,
    pub web_other: usize,
    pub subtasks: usize,
    pub notes: usize,
    pub notices: usize,
    pub images: usize,
    pub status: Option<StatusIcon>,
}

impl ActivityDigest {
    fn add_status(&mut self, status: StatusIcon) {
        self.status = Some(aggregate_web_status(
            self.status.into_iter().chain([status]),
        ));
    }

    fn add_web(&mut self, activity: &WebActivityPresentation) {
        for action in &activity.actions {
            match action.kind {
                WebActionKind::Search => self.searches += 1,
                WebActionKind::OpenPage => self.pages += 1,
                WebActionKind::FindInPage | WebActionKind::Other => self.web_other += 1,
            }
        }
        if activity.actions.is_empty() {
            self.web_other += activity.members.len().max(1);
        }
        self.add_status(activity.status());
    }

    pub fn add_block(&mut self, block: &PresentationBlock) {
        use PresentationBlockKind as Kind;
        match &block.kind {
            Kind::Reasoning { .. } => self.thoughts += 1,
            Kind::WebActivity(activity) => self.add_web(activity),
            Kind::Tool(PresentedTool::Acp(tool)) => {
                if let Some(activity) = tool.hosted_activity() {
                    self.add_web(&activity);
                } else {
                    self.tool_calls += 1;
                    self.add_status(tool.presented_status().icon());
                }
            }
            Kind::Tool(PresentedTool::Native { call, state }) => {
                self.tool_calls += 1;
                self.add_status(native_header_status(call, state));
            }
            Kind::Subtask { descriptor, .. } => {
                self.subtasks += 1;
                self.add_status(subtask_icon(descriptor.status));
            }
            Kind::Image { .. } => self.images += 1,
            Kind::Placeholder(label) if label == "[image]" => self.images += 1,
            Kind::Placeholder(_) => {
                if let Some(outcome) = incomplete_response_outcome(&block.kind) {
                    self.notices += 1;
                    self.add_status(
                        if outcome == zevria_content::ResponseAttemptOutcome::Interrupted {
                            StatusIcon::Interrupted
                        } else {
                            StatusIcon::Failed
                        },
                    );
                } else {
                    self.notes += 1;
                }
            }
            Kind::Diagnostic(diagnostic) => {
                self.notices += 1;
                self.add_status(if diagnostic.tone == DiagnosticTone::Error {
                    StatusIcon::Failed
                } else {
                    StatusIcon::Denied
                });
            }
            Kind::Error(_) => {
                self.notices += 1;
                self.add_status(StatusIcon::Failed);
            }
            Kind::Text { .. } | Kind::Plan(_) => self.notes += 1,
        }
    }

    pub fn add_entry(&mut self, entry: &HistoryEntry, diagnostics_visible: bool) {
        match entry {
            HistoryEntry::Conversation(entry) => {
                for block in &entry.blocks {
                    if block.visible(diagnostics_visible) {
                        self.add_block(block);
                    }
                }
            }
            HistoryEntry::Ensemble(ensemble) => {
                self.notes += 1;
                self.subtasks += ensemble.workers.len();
                for worker in &ensemble.workers {
                    self.add_status(StatusIcon::agent(worker.status));
                }
            }
            HistoryEntry::Error(error) => {
                self.notices += 1;
                self.add_status(if error == TURN_CANCELLED {
                    StatusIcon::Interrupted
                } else {
                    StatusIcon::Failed
                });
            }
            HistoryEntry::JobNotice(..) => self.notices += 1,
            HistoryEntry::PlanArtifact(_) | HistoryEntry::PlanHandoff(..) => self.notes += 1,
            HistoryEntry::CompactionDivider => {}
        }
    }

    pub fn label(self) -> String {
        [
            (self.thoughts, "thought", "thoughts"),
            (self.tool_calls, "tool call", "tool calls"),
            (self.searches, "search", "searches"),
            (self.pages, "page", "pages"),
            (self.web_other, "web action", "web actions"),
            (self.subtasks, "subtask", "subtasks"),
            (self.notes, "note", "notes"),
            (self.notices, "notice", "notices"),
            (self.images, "image", "images"),
        ]
        .into_iter()
        .filter(|(count, ..)| *count > 0)
        .map(|(count, one, many)| format!("{count} {}", if count == 1 { one } else { many }))
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

impl AddAssign for ActivityDigest {
    fn add_assign(&mut self, rhs: Self) {
        self.thoughts += rhs.thoughts;
        self.tool_calls += rhs.tool_calls;
        self.searches += rhs.searches;
        self.pages += rhs.pages;
        self.web_other += rhs.web_other;
        self.subtasks += rhs.subtasks;
        self.notes += rhs.notes;
        self.notices += rhs.notices;
        self.images += rhs.images;
        if let Some(status) = rhs.status {
            self.add_status(status);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BlockFold {
    #[default]
    Expanded,
    Preview,
    Activity(ActivityDigest),
    Merged,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpanSummary {
    pub digest: ActivityDigest,
    pub answer: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SpanAnswer {
    // The first part supplies the preview. Keep the remaining identities so
    // that no part of the final answer is counted as interim prose.
    Answer(Vec<(usize, PresentationBlockId)>),
    Plan(usize),
}

impl SpanAnswer {
    fn reconcile(
        self,
        conversation: &ConversationState,
        range: RangeInclusive<usize>,
    ) -> Option<Self> {
        match self {
            Self::Answer(ids) => {
                let mut resolved = ids.into_iter().map(|identity| {
                    let (row, id) = conversation.resolved_identity(identity);
                    (range.contains(&row) && conversation.selection_for_identity(row, id).is_some())
                        .then_some((row, id))
                });
                let first = resolved.next()??;
                Some(Self::Answer(
                    std::iter::once(first).chain(resolved.flatten()).collect(),
                ))
            }
            Self::Plan(row) => {
                matches!(conversation.entry(row), Some(HistoryEntry::PlanArtifact(_)))
                    .then_some(Self::Plan(row))
            }
        }
    }

    fn preview(&self, history: &[HistoryEntry], diagnostics_visible: bool) -> Option<String> {
        let text = match *self {
            Self::Answer(ref ids) => {
                let &(row, id) = ids.first()?;
                let HistoryEntry::Conversation(entry) = history.get(row)? else {
                    return None;
                };
                let block = entry
                    .blocks
                    .iter()
                    .find(|block| block.id == id && block.visible(diagnostics_visible))?;
                match &block.kind {
                    PresentationBlockKind::Text { text, .. } => preview_line(text),
                    _ => preview_line(&block.primary_copy()),
                }
            }
            Self::Plan(row) => {
                let HistoryEntry::PlanArtifact(plan) = history.get(row)? else {
                    return None;
                };
                let line = preview_line(&plan.markdown);
                if line.is_empty() {
                    preview_line(&plan.title)
                } else {
                    line
                }
            }
        };
        (!text.is_empty()).then_some(text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SpanFold {
    end: usize,
    answer: Option<SpanAnswer>,
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
    spans: BTreeMap<usize, SpanFold>,
}

impl FoldState {
    pub fn contains(&self, key: FoldKey) -> bool {
        match key {
            FoldKey::Span { start, end } => {
                self.spans.get(&start).is_some_and(|span| span.end == end)
            }
            _ => self.mode(key).is_folded(),
        }
    }

    pub fn mode(&self, key: FoldKey) -> FoldMode {
        if self.bulk.contains_key(&key)
            || matches!(key, FoldKey::Span { start, end } if self.spans.get(&start).is_some_and(|span| span.end == end))
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
            if self.contains(key) {
                return;
            }
            // Supersede only overlapping spans; no nested folds are affected.
            let overlaps = self
                .spans
                .range(..=end)
                .rev()
                .take_while(|(_, span)| span.end >= start)
                .map(|(&old_start, _)| old_start)
                .collect::<Vec<_>>();
            for old_start in overlaps {
                self.spans.remove(&old_start);
            }
            self.spans.insert(start, SpanFold { end, answer: None });
        } else if !self.contains(key) {
            self.folded.insert(key);
        }
    }

    pub fn unfold(&mut self, key: FoldKey) {
        match key {
            FoldKey::Span { start, .. } => {
                if self.contains(key) {
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
            let output = conversation.final_output(group.clone(), diagnostics_visible);
            let protect_output = fold == TurnFold::AllFinals || end == history.len();
            let mut span = None;
            for index in group {
                let entry = &history[index];
                let result = output
                    .as_ref()
                    .filter(|(at, _)| *at == index)
                    .map(|(_, output)| output);
                let protected = result.filter(|_| protect_output);
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
                        let (_, span) = span.get_or_insert((
                            index,
                            SpanFold {
                                end: index,
                                answer: None,
                            },
                        ));
                        span.end = index;
                        if let Some(result) = result {
                            span.answer = Some(match result {
                                FinalOutput::Answer(ids) => {
                                    SpanAnswer::Answer(ids.iter().map(|&id| (index, id)).collect())
                                }
                                FinalOutput::PlanArtifact => SpanAnswer::Plan(index),
                            });
                        }
                    }
                }
            }
            self.finish_span(&mut span);
        }
    }

    fn finish_span(&mut self, span: &mut Option<(usize, SpanFold)>) {
        if let Some((start, span)) = span.take() {
            self.spans.insert(start, span);
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
        let (&start, span) = self.spans.range(..=history_index).next_back()?;
        (history_index <= span.end).then_some((start, span.end))
    }

    pub fn span_summary(
        &self,
        history: &[HistoryEntry],
        start: usize,
        diagnostics_visible: bool,
    ) -> SpanSummary {
        let Some(span) = self.spans.get(&start) else {
            return SpanSummary::default();
        };
        let answer = span
            .answer
            .as_ref()
            .and_then(|answer| answer.preview(history, diagnostics_visible));
        let answer_parts = span.answer.as_ref().filter(|_| answer.is_some());
        let mut digest = ActivityDigest::default();
        for (row, entry) in history.iter().enumerate().take(span.end + 1).skip(start) {
            match (answer_parts, entry) {
                (Some(SpanAnswer::Answer(ids)), HistoryEntry::Conversation(entry)) => {
                    for block in &entry.blocks {
                        if block.visible(diagnostics_visible) && !ids.contains(&(row, block.id)) {
                            digest.add_block(block);
                        }
                    }
                }
                (Some(SpanAnswer::Plan(plan)), HistoryEntry::PlanArtifact(_)) if *plan == row => {}
                _ => digest.add_entry(entry, diagnostics_visible),
            }
        }
        for block in self.absorbed_blocks(history, span.end, diagnostics_visible) {
            digest.add_block(block);
        }
        SpanSummary { digest, answer }
    }

    fn absorbed_blocks<'a>(
        &self,
        history: &'a [HistoryEntry],
        end: usize,
        diagnostics_visible: bool,
    ) -> impl Iterator<Item = &'a PresentationBlock> {
        let row = end + 1;
        let folds = self.entry(row);
        history
            .get(row)
            .into_iter()
            .flat_map(move |entry| match entry {
                HistoryEntry::Conversation(entry) if folds.absorbing_span().is_some() => {
                    Some(entry)
                }
                _ => None,
            })
            .flat_map(move |entry| folds.visible_modes(entry, diagnostics_visible))
            .take_while(|&(_, _, mode)| mode == FoldMode::Summary)
            .map(|(_, block, _)| block)
    }

    /// Return the target of the visible disclosure for a semantic key.
    pub fn representative(
        &self,
        history: &[HistoryEntry],
        key: FoldKey,
        diagnostics_visible: bool,
    ) -> FoldKey {
        let history_index = key.history_index();
        if let Some((start, end)) = self.span_containing(history_index) {
            return FoldKey::Span { start, end };
        }
        let folds = self.entry(history_index);
        if folds.is_message_folded() || matches!(key, FoldKey::Span { .. }) {
            return FoldKey::Message { history_index };
        }
        if let Some(HistoryEntry::Conversation(entry)) = history.get(history_index) {
            match key {
                FoldKey::Message { .. } => {
                    if let Some((start, end)) = folds.absorbing_span()
                        && folds
                            .visible_modes(entry, diagnostics_visible)
                            .all(|(_, _, mode)| mode == FoldMode::Summary)
                    {
                        return FoldKey::Span { start, end };
                    }
                }
                FoldKey::Block { id, .. } => {
                    if let Some((lead, absorbed)) = folds.run_lead(entry, id, diagnostics_visible) {
                        return if let Some((start, end)) = absorbed {
                            FoldKey::Span { start, end }
                        } else {
                            FoldKey::Block {
                                history_index,
                                id: entry.blocks[lead].id,
                            }
                        };
                    }
                }
                _ => {}
            }
        }
        key
    }

    /// Members of one visual disclosure, in transcript order. Folds nested
    /// inside a range or message keep their independent intent when it opens.
    pub fn group(
        &self,
        history: &[HistoryEntry],
        key: FoldKey,
        diagnostics_visible: bool,
    ) -> Vec<FoldKey> {
        let key = self.representative(history, key, diagnostics_visible);
        match key {
            FoldKey::Span { end, .. } => std::iter::once(key)
                .chain(
                    self.absorbed_blocks(history, end, diagnostics_visible)
                        .map(|block| FoldKey::Block {
                            history_index: end + 1,
                            id: block.id,
                        }),
                )
                .collect(),
            FoldKey::Block { history_index, id }
                if let Some(HistoryEntry::Conversation(entry)) = history.get(history_index) =>
            {
                let group: Vec<_> = self
                    .entry(history_index)
                    .visible_modes(entry, diagnostics_visible)
                    .skip_while(|(_, block, _)| block.id != id)
                    .take_while(|&(_, _, mode)| mode == FoldMode::Summary)
                    .map(|(_, block, _)| FoldKey::Block {
                        history_index,
                        id: block.id,
                    })
                    .collect();
                if group.is_empty() { vec![key] } else { group }
            }
            _ => vec![key],
        }
    }

    pub fn open(&mut self, history: &[HistoryEntry], key: FoldKey, diagnostics_visible: bool) {
        for member in self.group(history, key, diagnostics_visible) {
            self.unfold(member);
        }
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
        self.spans.retain(|&start, span| {
            if let Some(answer) = span.answer.take() {
                span.answer = answer.reconcile(conversation, start..=span.end);
            }
            history.get(start..=span.end).is_some_and(|entries| {
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
            .chain(self.spans.iter().map(|(&start, span)| FoldKey::Span {
                start,
                end: span.end,
            }))
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

    /// A span can absorb this entry's leading activity run, but cannot reach
    /// through another span or a (bulk or manual) message preview.
    pub fn absorbing_span(self) -> Option<(usize, usize)> {
        let previous = self.history_index.checked_sub(1)?;
        let state = self.state?;
        if self.is_message_folded() || self.span().is_some() {
            return None;
        }
        state
            .span_containing(previous)
            .filter(|&(_, end)| end == previous)
    }

    fn visible_modes(
        self,
        entry: &ConversationEntry,
        diagnostics_visible: bool,
    ) -> impl Iterator<Item = (usize, &PresentationBlock, FoldMode)> {
        entry
            .blocks
            .iter()
            .enumerate()
            .filter_map(move |(index, block)| {
                block.visible(diagnostics_visible).then(|| {
                    let mode = self.block_mode(block.id);
                    // ACP can keep the prompt and response in one entry. A prompt
                    // folded by block still reads like a prompt, not an activity.
                    let mode = if mode == FoldMode::Summary
                        && block.role == Some(PresentationRole::User)
                    {
                        FoldMode::Preview
                    } else {
                        mode
                    };
                    (index, block, mode)
                })
            })
    }

    /// One mode per original block index. Invisible blocks do not break a run
    /// and have an unused `Expanded` slot to preserve those indices.
    pub fn block_folds(
        self,
        entry: &ConversationEntry,
        diagnostics_visible: bool,
    ) -> Vec<BlockFold> {
        let mut modes = vec![BlockFold::Expanded; entry.blocks.len()];
        if !self.stream_summary
            && self
                .state
                .is_none_or(|state| state.bulk.is_empty() && state.folded.is_empty())
        {
            return modes;
        }
        if self.is_message_folded() {
            // Local previews still shape the message body's hidden row count.
            for (index, block) in entry.blocks.iter().enumerate() {
                if self.block_mode(block.id) == FoldMode::Preview {
                    modes[index] = BlockFold::Preview;
                }
            }
            return modes;
        }
        let mut absorbed = self.absorbing_span().is_some();
        let mut lead = None;
        for (index, block, mode) in self.visible_modes(entry, diagnostics_visible) {
            match mode {
                FoldMode::Summary if absorbed => modes[index] = BlockFold::Merged,
                FoldMode::Summary => {
                    let lead = *lead.get_or_insert_with(|| {
                        modes[index] = BlockFold::Activity(ActivityDigest::default());
                        index
                    });
                    if let BlockFold::Activity(digest) = &mut modes[lead] {
                        digest.add_block(block);
                    }
                    if lead != index {
                        modes[index] = BlockFold::Merged;
                    }
                }
                FoldMode::Expanded | FoldMode::Preview => {
                    absorbed = false;
                    lead = None;
                    if mode == FoldMode::Preview {
                        modes[index] = BlockFold::Preview;
                    }
                }
            }
        }
        modes
    }

    fn run_lead(
        self,
        entry: &ConversationEntry,
        id: PresentationBlockId,
        diagnostics_visible: bool,
    ) -> Option<(usize, Option<(usize, usize)>)> {
        if self.block_mode(id) != FoldMode::Summary {
            return None;
        }
        let mut lead = None;
        let mut absorbed = self.absorbing_span();
        for (index, block, mode) in self.visible_modes(entry, diagnostics_visible) {
            if mode == FoldMode::Summary {
                lead.get_or_insert(index);
            } else {
                lead = None;
                absorbed = None;
            }
            if block.id == id {
                return Some((lead?, absorbed));
            }
        }
        None
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

/// First supplied non-empty line, without Markdown presentation markers.
pub fn preview_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let line = line.trim_start_matches(['#', '>', '-', '+', '*', ' ']);
    let mut result = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' | '*' | '_' | '~' | '[' | ']' => {
                if ch == ']' && chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    for ch in chars.by_ref() {
                        if ch == '(' {
                            depth += 1;
                        }
                        if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            _ => result.push(ch),
        }
    }
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{BlockVisibility, DisplayTurn, NativeHeader};
    use crate::conversation::{EnsembleHistory, ToolCallStatus};
    use rig_core::message::Message;

    #[test]
    fn preview_uses_only_provider_text() {
        assert_eq!(
            preview_line("\n\n### **Trace** the [source](file.rs) with `rg`\nnext line"),
            "Trace the source with rg"
        );
        assert_eq!(preview_line(" \n "), "");
    }
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
    fn digests_pluralize_and_aggregate_status_without_a_default_glyph() {
        let mut digest = ActivityDigest::default();
        assert_eq!(digest.label(), "");
        digest += ActivityDigest {
            thoughts: 1,
            searches: 2,
            status: Some(StatusIcon::Done),
            ..ActivityDigest::default()
        };
        digest += ActivityDigest {
            pages: 1,
            web_other: 2,
            notices: 1,
            status: Some(StatusIcon::Failed),
            ..ActivityDigest::default()
        };
        digest += ActivityDigest {
            notes: 2,
            images: 1,
            ..ActivityDigest::default()
        };
        assert_eq!(
            digest.label(),
            "1 thought · 2 searches · 1 page · 2 web actions · 2 notes · 1 notice · 1 image"
        );
        assert_eq!(digest.status, Some(StatusIcon::Failed));
    }

    #[test]
    fn visible_runs_skip_hidden_diagnostics_and_stop_at_answer_parts() {
        use zevria_transcript::TranscriptItem;
        let mut conversation = ConversationState::default();
        conversation.restore(vec![
            TranscriptItem::Message(Message::user("prompt")),
            indexed_response(),
        ]);
        let HistoryEntry::Conversation(entry) = conversation.entry_mut(1).unwrap() else {
            unreachable!()
        };
        let mut hidden = entry.blocks[0].clone();
        hidden.id = PresentationBlockId(u64::MAX);
        hidden.visibility = BlockVisibility::Diagnostics;
        hidden.kind = PresentationBlockKind::Error("hidden diagnostic".into());
        entry.blocks.insert(1, hidden);
        let mut folds = FoldState::default();
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        let HistoryEntry::Conversation(entry) = conversation.entry(1).unwrap() else {
            unreachable!()
        };
        let modes = folds.entry(1).block_folds(entry, false);
        assert!(
            matches!(modes[0], BlockFold::Activity(d) if d.notes == 1 && d.searches == 1 && d.notices == 0)
        );
        assert_eq!(
            modes[1..5],
            [
                BlockFold::Expanded,
                BlockFold::Merged,
                BlockFold::Expanded,
                BlockFold::Expanded
            ]
        );
        assert!(matches!(modes[5], BlockFold::Activity(d) if d.pages == 1 && d.web_other == 1));
        assert_eq!(modes[6], BlockFold::Merged);
        let first = block_key(&conversation, 1, 0);
        let search = block_key(&conversation, 1, 2);
        assert_eq!(
            folds.representative(conversation.history(), search, false),
            first
        );
        assert_eq!(
            folds.group(conversation.history(), search, false),
            [first, search]
        );
        folds.open(conversation.history(), first, false);
        assert!(!folds.contains(first) && !folds.contains(search));
        assert!(folds.contains(block_key(&conversation, 1, 5)));
    }

    #[test]
    fn stream_snapshot_uses_one_activity_run() {
        let HistoryEntry::Conversation(entry) =
            HistoryEntry::from_message(tool_round(), ToolCallStatus::Executing).unwrap()
        else {
            unreachable!()
        };
        let modes = EntryFolds {
            stream_summary: true,
            ..EntryFolds::none()
        }
        .block_folds(&entry, false);
        assert!(matches!(modes[0], BlockFold::Activity(d) if d.notes == 1 && d.tool_calls == 1));
        assert_eq!(modes[1], BlockFold::Merged);
    }

    #[test]
    fn span_absorbs_leading_activity_and_opens_it_without_touching_nested_folds() {
        use zevria_transcript::TranscriptItem;
        let mut conversation = ConversationState::default();
        conversation.restore(vec![
            TranscriptItem::Message(Message::user("prompt")),
            TranscriptItem::Message(tool_round()),
            indexed_response(),
        ]);
        let mut folds = FoldState::default();
        let nested = block_key(&conversation, 1, 0);
        folds.fold(nested);
        folds.fold_turns(&conversation, false, TurnFold::AllFinals);
        let history = conversation.history();
        let span = FoldKey::Span { start: 1, end: 1 };
        let lead = block_key(&conversation, 2, 0);
        let search = block_key(&conversation, 2, 1);
        let HistoryEntry::Conversation(entry) = &history[2] else {
            unreachable!()
        };
        let modes = folds.entry(2).block_folds(entry, false);
        assert_eq!(
            modes[..3],
            [BlockFold::Merged, BlockFold::Merged, BlockFold::Expanded]
        );
        assert_eq!(folds.representative(history, search, false), span);
        assert_eq!(folds.group(history, span, false), [span, lead, search]);
        let summary = folds.span_summary(history, 1, false);
        assert_eq!(summary.answer, None);
        assert_eq!(summary.digest.label(), "1 tool call · 1 search · 2 notes");
        folds.open(history, span, false);
        assert!(
            [span, lead, search]
                .into_iter()
                .all(|key| !folds.contains(key))
        );
        assert_eq!(folds.mode(nested), FoldMode::Preview);
        assert!(folds.contains(block_key(&conversation, 2, 4)));
    }

    #[test]
    fn older_answer_preview_excludes_all_final_parts_and_reconciles_the_first_identity() {
        use zevria_transcript::restored_attempt;
        let item = indexed_response();
        let attempt = restored_attempt(&item, 0).unwrap();
        let mut conversation = state([prompt("first prompt", 1)]);
        conversation.update_response_attempt(attempt.clone());
        conversation.complete_response(item.display_message().unwrap(), Some(&attempt.id), None);
        conversation.push_entry(prompt("second prompt", 2));
        let mut folds = FoldState::default();
        folds.fold_turns(&conversation, false, TurnFold::LastFinal);
        let summary = folds.span_summary(conversation.history(), 1, false);
        assert_eq!(summary.answer.as_deref(), Some("First part"));
        assert_eq!(
            summary.digest.label(),
            "1 search · 1 page · 1 web action · 1 note"
        );
        let source = &attempt.presentation.last().unwrap().source;
        let HistoryEntry::Conversation(entry) = conversation.entry_mut(1).unwrap() else {
            unreachable!()
        };
        let first = entry
            .blocks
            .iter()
            .position(|block| block.primary_copy() == "First part")
            .unwrap();
        let old = entry.blocks.remove(first).id;
        conversation.alias_attempt_part((1, old), &attempt.id, source);
        let (_, new) = conversation.resolved_identity((1, old));
        assert_ne!(old, new);
        folds.reconcile(&conversation);
        assert!(
            folds
                .span_summary(conversation.history(), 1, false)
                .answer
                .as_deref()
                .is_some_and(|line| line.starts_with("Second"))
        );
        let HistoryEntry::Conversation(entry) = conversation.entry_mut(1).unwrap() else {
            unreachable!()
        };
        entry.blocks.retain(|block| block.id != new);
        folds.reconcile(&conversation);
        assert_eq!(
            folds.span_summary(conversation.history(), 1, false).answer,
            None
        );
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
                TranscriptItem::IncompleteResponse(Box::new(
                    IncompleteResponse::from_attempt(&trailing)
                        .unwrap()
                        .unwrap(),
                )),
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
        assert_eq!(
            folds.span_summary(conversation.history(), 3, false),
            SpanSummary {
                digest: ActivityDigest::default(),
                answer: Some("Plan body".into()),
            }
        );
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
