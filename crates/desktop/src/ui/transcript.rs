use super::{
    blocks::{self, BlockData, BlockKind, BlockView, DisclosureChanged, Gutter, LiveTail},
    rows::{self, RowChange},
    style,
    theme::palette,
};
use crate::{
    controller::SessionController,
    disclosure::{self, DisclosureStore, Phase, TailIdentity},
    turns::{self, StepCounts, StepRun, TurnDirection},
};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::{prelude::*, *};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    time::Instant,
};
use zevria_presentation::{
    ConversationTail,
    blocks::PresentationBlockId,
    conversation::{HistoryEntry, ToolCallStatus},
};

struct RetainedBlock {
    view: Entity<BlockView>,
    _subscriptions: Vec<Subscription>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RowRole {
    #[default]
    Plain,
    Summary {
        start: usize,
        expanded: bool,
    },
    Hidden,
}
/// Each pane keeps its own anchors, parsing, choices and scroll position while inspected.
struct PaneView {
    scroller: Entity<MessageScrollerState>,
    epoch: String,
    fingerprints: Vec<u64>,
    row_data: HashMap<usize, (u64, Vec<BlockData>)>,
    tail_data: Vec<BlockData>,
    blocks: HashMap<String, RetainedBlock>,
    disclosures: DisclosureStore,
    steps: DisclosureStore,
    runs: Vec<StepRun>,
    summaries: HashMap<usize, BlockData>,
    roles: Vec<RowRole>,
    final_outputs: bool,
    restored: HashSet<String>,
    anchors: HashMap<String, (usize, PresentationBlockId)>,
    revision: u64,
    busy: bool,
    call: Option<zevria_presentation::blocks::NativeHeader>,
}
impl PaneView {
    fn new(cx: &mut Context<TranscriptView>) -> Self {
        Self {
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            epoch: String::new(),
            fingerprints: Vec::new(),
            row_data: HashMap::new(),
            tail_data: Vec::new(),
            blocks: HashMap::new(),
            disclosures: DisclosureStore::default(),
            steps: DisclosureStore::default(),
            runs: Vec::new(),
            summaries: HashMap::new(),
            roles: Vec::new(),
            final_outputs: false,
            restored: HashSet::new(),
            anchors: HashMap::new(),
            revision: u64::MAX,
            busy: false,
            call: None,
        }
    }
    fn roles(&self, final_outputs: bool) -> Vec<RowRole> {
        let mut roles = vec![RowRole::Plain; self.fingerprints.len()];
        if final_outputs {
            for run in &self.runs {
                let Some(summary) = self.summaries.get(&run.start) else {
                    continue;
                };
                let expanded = self.steps.expanded(&summary.key, summary.phase);
                roles[run.start] = RowRole::Summary {
                    start: run.start,
                    expanded,
                };
                if !expanded {
                    roles[run.start + 1..=run.end].fill(RowRole::Hidden);
                }
            }
        }
        roles
    }
    fn is_summary(&self, key: &str) -> bool {
        self.summaries.values().any(|summary| summary.key == key)
    }
}
pub struct TranscriptView {
    controller: Entity<SessionController>,
    pane: PaneView,
    selected: Option<zevria_foundation::SubtaskId>,
    parked: HashMap<Option<zevria_foundation::SubtaskId>, PaneView>,
    compact: bool,
    final_outputs: bool,
    _controller: Subscription,
    _scroll: Subscription,
}
impl TranscriptView {
    pub fn new(controller: Entity<SessionController>, cx: &mut Context<Self>) -> Self {
        let pane = PaneView::new(cx);
        let selected = controller.read(cx).model.selected_child.clone();
        let watch = cx.observe(&controller, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        let scroll = cx.observe(&pane.scroller, |this, _, cx| {
            this.flush_folds(cx);
        });
        let mut this = Self {
            controller,
            pane,
            selected,
            parked: HashMap::new(),
            compact: false,
            final_outputs: false,
            _controller: watch,
            _scroll: scroll,
        };
        this.sync(cx);
        this
    }
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        self.pane.scroller.update(cx, |list, cx| {
            list.remeasure_items(0..self.pane.fingerprints.len(), cx)
        });
        self.refresh_views(cx);
        cx.notify();
    }
    pub fn final_outputs(&self) -> bool {
        self.final_outputs
    }
    /// Returns whether focus belonged to a summary that has now been retired.
    pub fn set_final_outputs(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.final_outputs == on {
            return false;
        }
        let retired_focus = !on
            && self.pane.summaries.values().any(|summary| {
                self.pane
                    .blocks
                    .get(&summary.key)
                    .is_some_and(|block| block.view.read(cx).is_focused(window, cx))
            });
        self.final_outputs = on;
        self.pane.steps = DisclosureStore::default();
        for pane in self.parked.values_mut() {
            pane.steps = DisclosureStore::default();
            pane.roles = pane.roles(on);
        }
        self.sync_with_window(Some(window), cx);
        cx.notify();
        retired_focus
    }
    fn rekey(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        self.pane.disclosures.rekey(old, new);
        if self.pane.restored.remove(old) {
            self.pane.restored.insert(new.into());
        }
        if let Some(block) = self.pane.blocks.remove(old) {
            self.pane.blocks.entry(new.into()).or_insert(block);
        }
    }
    #[cfg(test)]
    pub(super) fn scroller(&self) -> Entity<MessageScrollerState> {
        self.pane.scroller.clone()
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        self.sync_with_window(None, cx);
    }
    fn sync_with_window(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let selected = self.controller.read(cx).model.selected_child.clone();
        if selected != self.selected {
            let next = self
                .parked
                .remove(&selected)
                .unwrap_or_else(|| PaneView::new(cx));
            let previous = std::mem::replace(&mut self.pane, next);
            self.parked.insert(self.selected.clone(), previous);
            self.selected = selected;
            self.pane.revision = u64::MAX;
            self._scroll = cx.observe(&self.pane.scroller, |this, _, cx| this.flush_folds(cx));
            // Background parsing may have completed while this pane was hidden.
            self.pane.scroller.update(cx, |list, cx| list.remeasure(cx));
        }
        self.parked.retain(|id, _| {
            id.as_ref().is_none_or(|id| {
                self.controller
                    .read(cx)
                    .model
                    .children
                    .iter()
                    .any(|child| &child.id == id)
            })
        });
        let controller = self.controller.read(cx);
        let model = controller.model.visible();
        let epoch = format!("{:?}", model.conversation.epoch());
        let history = model.conversation.history();
        let busy = model.is_busy();
        let call = model.session.call_header();
        let revision = controller.projection_revision;
        let mode_changed = self.final_outputs != self.pane.final_outputs;
        // The clock and draft edits must not re-fingerprint history or clone the tail.
        if !mode_changed
            && epoch == self.pane.epoch
            && revision == self.pane.revision
            && busy == self.pane.busy
            && call == self.pane.call
            && history.len() + 1 == self.pane.fingerprints.len()
        {
            return;
        }
        let replaced = epoch != self.pane.epoch;
        let mut fingerprints = history
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                rows::fingerprint(
                    entry,
                    rows::continues_turn(i.checked_sub(1).and_then(|p| history.get(p)), entry),
                )
            })
            .collect::<Vec<_>>();
        fingerprints.push(0); // Stable live-tip row, even when idle.
        let history_changed = replaced || self.pane.fingerprints != fingerprints;
        let next_runs =
            (history_changed || busy != self.pane.busy || call != self.pane.call || mode_changed)
                .then(|| {
                    if self.final_outputs {
                        turns::step_runs(
                            history,
                            &turns::turn_rows(&model.conversation.turn_starts(false)),
                            busy.then_some(call).flatten().map(|h| h.turn()),
                        )
                    } else {
                        Vec::new()
                    }
                });
        let mut previous_rows = std::mem::take(&mut self.pane.row_data);
        let mut rows = HashMap::new();
        let mut anchors = HashMap::new();
        let mut aliases = Vec::new();
        if !replaced && history_changed {
            for (key, anchor) in &self.pane.anchors {
                let resolved = model.conversation.resolved_identity(*anchor);
                if resolved != *anchor {
                    aliases.push((key.clone(), format!("{epoch}-{}", resolved.1.0)));
                }
            }
        }
        for (index, entry) in history.iter().enumerate() {
            let fingerprint = fingerprints[index];
            let mut data = if !replaced
                && let Some((prior, data)) = previous_rows.remove(&index)
                && prior == fingerprint
            {
                data
            } else {
                blocks::entry(
                    &epoch,
                    index,
                    entry,
                    fingerprint,
                    rows::continues_turn(index.checked_sub(1).and_then(|p| history.get(p)), entry),
                    controller.model.selected_child.is_some(),
                )
            };
            let last = data.len().saturating_sub(1);
            for (position, block) in data.iter_mut().enumerate() {
                if let Some(id) = block.source {
                    anchors.insert(block.key.clone(), (index, id));
                }
                block.phase = if block.source.is_some_and(|id| {
                    controller
                        .unsaved_blocks
                        .contains(&(model.conversation.epoch(), id))
                }) {
                    Phase::Unsaved
                } else if replaced
                    || self.pane.restored.contains(&block.key)
                    || aliases
                        .iter()
                        .any(|(old, new)| new == &block.key && self.pane.restored.contains(old))
                {
                    Phase::Restored
                } else if let HistoryEntry::Conversation(entry) = entry
                    && entry.blocks.iter().any(|b| {
                        zevria_presentation::incomplete_response_outcome(&b.kind).is_some()
                    })
                {
                    Phase::Interrupted
                } else if busy
                    && block.header.is_some()
                    && call
                        .is_some_and(|call| block.header.is_some_and(|h| h.turn() == call.turn()))
                {
                    if block.header == call && position == last {
                        Phase::Live
                    } else {
                        Phase::Working
                    }
                } else {
                    Phase::Finished
                };
            }
            rows.insert(index, (fingerprint, data));
        }
        let mut tail = Vec::new();
        if let ConversationTail::Streaming { message, .. } = model.tail(Instant::now())
            && let Some(entry) =
                HistoryEntry::from_message(message.clone(), ToolCallStatus::Executing)
        {
            tail = blocks::entry(
                &epoch,
                history.len(),
                &entry,
                revision,
                history.last().is_some_and(rows::is_assistant),
                controller.model.selected_child.is_some(),
            );
            let last = tail.len().saturating_sub(1);
            let mut ordinal = 0;
            for (position, block) in tail.iter_mut().enumerate() {
                if let Some(kind) = block.tail_kind() {
                    block.key = TailIdentity {
                        header: call,
                        kind,
                        ordinal,
                    }
                    .key(&epoch);
                    ordinal += 1;
                }
                block.source = None;
                block.header = call;
                block.phase = if position == last {
                    Phase::Live
                } else {
                    Phase::Working
                };
            }
        }
        if busy {
            let phrase = super::status::working_activity(model, controller.cancelling);
            tail.push(BlockData::new(
                format!("{epoch}-tip"),
                BlockKind::Live(LiveTail::Activity(phrase.clone())),
                Gutter::Live,
                phrase,
                true,
            ));
        }
        // Match message-only previews to the newly committed call, never a later call's ordinal.
        let mut transfers = Vec::new();
        if !replaced && history_changed {
            for (_, data) in rows.values() {
                let mut ordinal = 0;
                for block in data {
                    if let Some(kind) = block.tail_kind() {
                        let identity = TailIdentity {
                            header: block.header,
                            kind,
                            ordinal,
                        };
                        if !self.pane.anchors.contains_key(&block.key)
                            && let Some(before) = self
                                .pane
                                .tail_data
                                .iter()
                                .find(|before| before.key == identity.key(&epoch))
                            && disclosure::can_commit(identity, identity, &before.body, &block.body)
                        {
                            transfers.push((before.key.clone(), block.key.clone()));
                        }
                        ordinal += 1;
                    }
                }
            }
        }
        if replaced {
            self.pane.blocks.clear();
            self.pane.disclosures = DisclosureStore::default();
            self.pane.steps = DisclosureStore::default();
            self.pane.roles.clear();
            self.pane.restored = rows
                .values()
                .flat_map(|(_, data)| data.iter().map(|b| b.key.clone()))
                .collect();
        }
        self.pane.epoch = epoch;
        // Ambiguous fallback boundaries retire to policy instead of attaching a
        // reader's choice (or retained parser) to the wrong repeated text.
        let mut counts = HashMap::new();
        for (old, _) in &transfers {
            *counts.entry(old.clone()).or_insert(0usize) += 1;
        }
        for (old, new) in aliases
            .into_iter()
            .chain(transfers.into_iter().filter(|(old, _)| counts[old] == 1))
        {
            self.rekey(&old, &new);
        }
        for block in rows.values().flat_map(|(_, data)| data).chain(&tail) {
            if matches!(block.kind, BlockKind::Reasoning) {
                self.pane.disclosures.observe(&block.key, block.phase);
            }
        }
        self.pane.row_data = rows;
        self.pane.tail_data = tail;
        self.pane.anchors = anchors;
        let mut changed_summaries = Vec::new();
        if let Some(runs) = next_runs {
            let mut summaries = HashMap::new();
            for run in &runs {
                let mut counts = StepCounts::default();
                let data = (run.start..=run.end)
                    .filter_map(|row| self.pane.row_data.get(&row))
                    .flat_map(|(_, data)| data);
                for block in data {
                    match &block.kind {
                        BlockKind::Tool { meta, .. } => {
                            counts.tool_calls += 1;
                            counts.added += meta.added;
                            counts.removed += meta.removed;
                        }
                        BlockKind::Subtask { .. } | BlockKind::Web => counts.tool_calls += 1,
                        BlockKind::Reasoning => counts.thoughts += 1,
                        _ => counts.messages += 1,
                    }
                }
                let gutter = self
                    .pane
                    .row_data
                    .get(&run.start)
                    .and_then(|(_, blocks)| blocks.first())
                    .map_or(Gutter::None, |block| match block.gutter {
                        Gutter::Label(_) => block.gutter,
                        _ => Gutter::None,
                    });
                let summary = BlockData::steps(
                    format!("{}-steps-{}", self.pane.epoch, run.start),
                    counts,
                    gutter,
                    if run.live {
                        Phase::Live
                    } else {
                        Phase::Finished
                    },
                );
                self.pane.steps.observe(&summary.key, summary.phase);
                if self.pane.summaries.get(&run.start).is_none_or(|old| {
                    old.revision != summary.revision
                        || old.gutter != summary.gutter
                        || old.phase != summary.phase
                }) {
                    changed_summaries.push(run.start);
                }
                summaries.insert(run.start, summary);
            }
            self.pane.runs = runs;
            self.pane.summaries = summaries;
            self.pane.steps.retain(
                &self
                    .pane
                    .summaries
                    .values()
                    .map(|b| b.key.clone())
                    .collect(),
            );
        }
        let valid = self
            .pane
            .row_data
            .values()
            .flat_map(|(_, data)| data.iter())
            .chain(&self.pane.tail_data)
            .chain(self.pane.summaries.values())
            .map(|b| b.key.clone())
            .collect::<HashSet<_>>();
        self.pane.blocks.retain(|key, _| valid.contains(key));
        self.pane.restored.retain(|key| valid.contains(key));
        self.pane.disclosures.retain(&valid);
        self.pane.revision = revision;
        self.pane.busy = busy;
        self.pane.call = call;
        self.pane.final_outputs = self.final_outputs;
        if replaced {
            self.pane
                .scroller
                .update(cx, |list, cx| list.reset(fingerprints.len(), cx));
        } else {
            match rows::compare(&self.pane.fingerprints, &fingerprints) {
                RowChange::None => {}
                RowChange::Remeasure(indices) => {
                    for i in indices {
                        self.pane
                            .scroller
                            .update(cx, |list, cx| list.remeasure_items(i..i + 1, cx));
                    }
                }
                RowChange::Splice { old, count } => {
                    self.pane
                        .scroller
                        .update(cx, |list, cx| list.splice(old, count, cx));
                }
            }
            let last = fingerprints.len() - 1;
            self.pane
                .scroller
                .update(cx, |list, cx| list.remeasure_items(last..last + 1, cx));
        }
        self.pane.fingerprints = fingerprints;
        for row in changed_summaries {
            self.pane
                .scroller
                .update(cx, |list, cx| list.remeasure_items(row..row + 1, cx));
        }
        self.apply_roles(window, cx);
        self.flush_folds(cx);
        self.refresh_views(cx);
    }
    /// The one presentation boundary: callers never accidentally expose hidden steps.
    fn data(&self, index: usize) -> Vec<BlockData> {
        let role = self.pane.roles.get(index).copied().unwrap_or_default();
        if role == RowRole::Hidden {
            return Vec::new();
        }
        let mut data = self
            .pane
            .row_data
            .get(&index)
            .map(|(_, data)| data.clone())
            .unwrap_or_else(|| self.pane.tail_data.clone());
        if let RowRole::Summary { start, expanded } = role {
            if expanded {
                if let Some(first) = data.first_mut()
                    && matches!(first.gutter, Gutter::Label(_))
                {
                    first.gutter = Gutter::None;
                }
            } else {
                data.clear();
            }
            if let Some(summary) = self.pane.summaries.get(&start) {
                data.insert(0, summary.clone());
            }
        }
        for block in &mut data {
            block.expanded = block.collapsible
                && if matches!(block.kind, BlockKind::Steps(_)) {
                    self.pane.steps.expanded(&block.key, block.phase)
                } else {
                    self.pane.disclosures.expanded(
                        &block.key,
                        if matches!(block.kind, BlockKind::Reasoning) {
                            block.phase
                        } else {
                            Phase::Restored
                        },
                    )
                };
        }
        data
    }
    fn refresh_views(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.pane.fingerprints.len() {
            // Do not clone long, unmaterialized history on every stream delta.
            let source = self
                .pane
                .row_data
                .get(&index)
                .map(|(_, data)| data.as_slice())
                .unwrap_or(&self.pane.tail_data);
            if !source.iter().any(|b| self.pane.blocks.contains_key(&b.key))
                && !self
                    .pane
                    .summaries
                    .get(&index)
                    .is_some_and(|b| self.pane.blocks.contains_key(&b.key))
            {
                continue;
            }
            for block in self.data(index) {
                if let Some(retained) = self.pane.blocks.get(&block.key) {
                    retained
                        .view
                        .update(cx, |view, cx| view.sync(block, self.compact, cx));
                }
            }
        }
    }
    fn row_for(&self, key: &str) -> Range<usize> {
        if let Some(run) = self.pane.runs.iter().find(|run| {
            self.pane
                .summaries
                .get(&run.start)
                .is_some_and(|b| b.key == key)
        }) {
            return run.start..run.end + 1;
        }
        let index = self
            .pane
            .row_data
            .iter()
            .find_map(|(index, (_, data))| data.iter().any(|b| b.key == key).then_some(*index))
            .unwrap_or(self.pane.fingerprints.len().saturating_sub(1));
        index..index + 1
    }
    /// Role changes invalidate only affected rows; the list retains its live item anchor.
    fn apply_roles(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let roles = self.pane.roles(self.final_outputs);
        let top = self.pane.scroller.read(cx).logical_scroll_top();
        let following = self.pane.scroller.read(cx).is_following_tail();
        let rescue_top = if !following {
            self.pane
                .runs
                .iter()
                .find(|run| {
                    roles.get(run.start)
                        == Some(&RowRole::Summary {
                            start: run.start,
                            expanded: false,
                        })
                        && top.item_ix >= run.start
                        && top.item_ix <= run.end
                        && (top.item_ix > run.start || top.offset_in_item > px(0.))
                        && self.pane.roles.get(top.item_ix) != roles.get(top.item_ix)
                })
                .map(|run| run.start)
        } else {
            None
        };
        let rescue_focus = window.as_ref().and_then(|window| {
            self.pane
                .runs
                .iter()
                .find(|run| {
                    roles.get(run.start)
                        == Some(&RowRole::Summary {
                            start: run.start,
                            expanded: false,
                        })
                        && (run.start..=run.end).any(|row| {
                            self.pane.row_data.get(&row).is_some_and(|(_, blocks)| {
                                blocks.iter().any(|block| {
                                    self.pane.blocks.get(&block.key).is_some_and(|retained| {
                                        retained.view.read(cx).is_focused(window, cx)
                                    })
                                })
                            })
                        })
                })
                .map(|run| run.start)
        });
        for (row, role) in roles.iter().enumerate() {
            if self.pane.roles.get(row) != Some(role) {
                self.pane
                    .scroller
                    .update(cx, |list, cx| list.remeasure_items(row..row + 1, cx));
            }
        }
        self.pane.roles = roles;
        if let Some(row) = rescue_top {
            self.pane
                .scroller
                .update(cx, |list, cx| list.scroll_to_item(row, cx));
        }
        if let (Some(row), Some(window)) = (rescue_focus, window)
            && let Some(summary) = self.data(row).into_iter().next()
        {
            let view = self.retained(summary, window, cx);
            window.focus(&view.read(cx).disclosure_focus(), cx);
        }
    }
    fn flush_folds(&mut self, cx: &mut Context<Self>) {
        let following = self.pane.scroller.read(cx).is_following_tail();
        let changed = self.pane.disclosures.follow_tail(following);
        let steps = self.pane.steps.follow_tail(following);
        if changed.is_empty() && steps.is_empty() {
            return;
        }
        self.apply_roles(None, cx);
        self.refresh_views(cx);
        for key in changed.into_iter().chain(steps) {
            let range = self.row_for(&key);
            self.pane
                .scroller
                .update(cx, |list, cx| list.remeasure_items(range, cx));
        }
        cx.notify();
    }
    pub fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.pane.scroller.update(cx, |list, cx| {
            list.scroll_to_end(cx);
        });
        self.flush_folds(cx);
        cx.notify();
    }
    pub fn jump_turn(&mut self, direction: TurnDirection, cx: &mut Context<Self>) -> bool {
        let starts = turns::turn_rows(
            &self
                .controller
                .read(cx)
                .model
                .visible()
                .conversation
                .turn_starts(false),
        );
        let top = self.pane.scroller.read(cx).logical_scroll_top();
        let Some(row) =
            turns::destination(&starts, top.item_ix, top.offset_in_item > px(0.), direction)
        else {
            return false;
        };
        self.pane
            .scroller
            .update(cx, |list, cx| list.scroll_to_item(row, cx))
    }
    pub fn jump_to_beginning(&mut self, cx: &mut Context<Self>) {
        self.pane
            .scroller
            .update(cx, |list, cx| list.scroll_to_item(0, cx));
    }
    pub fn disclose(
        &mut self,
        expanded: bool,
        all: bool,
        from_composer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = self.pane.blocks.iter().find_map(|(key, b)| {
            b.view
                .read(cx)
                .is_focused(window, cx)
                .then_some(key.clone())
        });
        let mut keys = Vec::new();
        for index in 0..self.pane.fingerprints.len() {
            let data = if all {
                self.pane
                    .row_data
                    .get(&index)
                    .map(|(_, data)| data.clone())
                    .unwrap_or_else(|| self.pane.tail_data.clone())
            } else {
                self.data(index)
            };
            keys.extend(
                data.into_iter()
                    .filter(|b| {
                        if all || from_composer {
                            matches!(b.kind, BlockKind::Reasoning)
                        } else {
                            b.collapsible && focused.as_deref() == Some(&b.key)
                        }
                    })
                    .map(|b| b.key),
            );
        }
        if !all && from_composer {
            keys = keys.into_iter().rev().take(1).collect();
        }
        for key in keys {
            self.choose(&key, expanded, window, cx);
        }
        self.refresh_views(cx);
        cx.notify();
    }
    fn choose(&mut self, key: &str, expanded: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane.is_summary(key) {
            self.pane.steps.choose(key, expanded);
            self.apply_roles(Some(window), cx);
        } else {
            self.pane.disclosures.choose(key, expanded);
        }
        let range = self.row_for(key);
        self.pane
            .scroller
            .update(cx, |list, cx| list.remeasure_items(range, cx));
        self.refresh_views(cx);
        cx.notify();
    }
    #[cfg(test)]
    pub(super) fn test_data(&self) -> Vec<BlockData> {
        (0..self.pane.fingerprints.len())
            .flat_map(|index| self.data(index))
            .collect()
    }
    #[cfg(test)]
    pub(super) fn test_identity(&self, key: &str, cx: &App) -> Option<(EntityId, EntityId)> {
        self.pane
            .blocks
            .get(key)
            .map(|block| (block.view.entity_id(), block.view.read(cx).text_id()))
    }
    #[cfg(test)]
    pub(super) fn test_focus(&self, key: &str, cx: &App) -> FocusHandle {
        self.pane
            .blocks
            .get(key)
            .unwrap()
            .view
            .read(cx)
            .disclosure_focus()
    }
    /// Materialize independently of row rendering so a newly-created summary can rescue focus.
    fn retained(
        &mut self,
        data: BlockData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BlockView> {
        let key = data.key.clone();
        if let Some(retained) = self.pane.blocks.get(&key) {
            let entity = retained.view.clone();
            entity.update(cx, |block, cx| block.sync(data, self.compact, cx));
            return entity;
        }
        let entity = cx.new(|cx| BlockView::new(data, self.controller.clone(), self.compact, cx));
        let observe = cx.observe(&entity, |this, entity, cx| {
            if !this.pane.blocks.contains_key(entity.read(cx).key()) {
                return;
            }
            let range = this.row_for(entity.read(cx).key());
            this.pane
                .scroller
                .update(cx, |list, cx| list.remeasure_items(range, cx));
            cx.notify();
        });
        let change = cx.subscribe_in(
            &entity,
            window,
            |this, _, event: &DisclosureChanged, window, cx| {
                if !this.pane.blocks.contains_key(&event.key) {
                    if let Some(pane) = this
                        .parked
                        .values_mut()
                        .find(|pane| pane.blocks.contains_key(&event.key))
                    {
                        if pane.is_summary(&event.key) {
                            pane.steps.choose(&event.key, event.expanded);
                            pane.roles = pane.roles(this.final_outputs);
                        } else {
                            pane.disclosures.choose(&event.key, event.expanded);
                        }
                    }
                    return;
                }
                this.choose(&event.key, event.expanded, window, cx);
            },
        );
        self.pane.blocks.insert(
            key,
            RetainedBlock {
                view: entity.clone(),
                _subscriptions: vec![observe, change],
            },
        );
        entity
    }
    fn row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.pane.roles.get(index) == Some(&RowRole::Hidden) {
            return div().w_full().h_0().into_any_element();
        }
        let data = self.data(index);
        let mut row = div()
            .w_full()
            .flex()
            .flex_col()
            .px(px(16.))
            .when(index + 1 < self.pane.fingerprints.len(), |row| {
                row.pb(rems(2.))
            });
        for data in data {
            row = row.child(self.retained(data, window, cx));
        }
        row.into_any_element()
    }
}
impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity();
        let controller = self.controller.read(cx);
        let model = controller.model.visible();
        if controller.phase == crate::controller::Phase::Live
            && controller.model.selected_child.is_none()
            && model.conversation.history().is_empty()
            && matches!(model.tail(Instant::now()), ConversationTail::None)
        {
            let name = controller
                .workspace
                .as_deref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Zevria".into());
            return div().id("transcript").size_full().px(px(16.)).pt(px(80.))
                .child(style::Measure { compact: self.compact }.row()
                    .when(!self.compact, |r| r.child(div().w(px(style::GUTTER)).flex_shrink_0()))
                    .child(div().flex_1().min_w_0().max_w(px(style::PROSE)).flex().flex_col().gap(px(16.))
                        .child(div().text_size(px(36.)).line_height(px(40.)).font_weight(FontWeight::SEMIBOLD).child(name))
                        .child(style::body().text_color(palette(cx).text.secondary).child("Describe a change to build, or switch to Plan to work through an approach."))
                        .child(style::kbd_hint("⌘O  Open workspace     ⌘N  New session     ⇧⌘M  Switch mode", cx))))
                .into_any_element();
        }
        div()
            .size_full()
            .bg(palette(cx).surfaces.canvas)
            .child(
                MessageScroller::new(
                    "transcript",
                    self.pane.scroller.clone(),
                    move |index, window, cx| {
                        owner.update(cx, |this, cx| this.row(index, window, cx))
                    },
                )
                .with_row_style(StyleRefinement::default().px_0().pb_0())
                .jump_button(true)
                .with_jump_button_label("Jump to latest")
                .size_full(),
            )
            .into_any_element()
    }
}
