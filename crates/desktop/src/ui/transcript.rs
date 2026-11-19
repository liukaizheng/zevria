use super::{
    blocks::{
        self, BlockData, BlockKind, BlockView, DisclosureChanged, Gutter, LiveTail,
        MessageDisclosure,
    },
    rows::{self, RowChange},
    style,
    theme::palette,
};
use crate::{
    actions::Shortcut,
    controller::SessionController,
    disclosure::{self, Phase, TailIdentity},
    turns::{self, TurnDirection},
};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::{prelude::*, *};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};
use zevria_presentation::{
    ConversationTail,
    blocks::PresentationBlockId,
    conversation::{HistoryEntry, ToolCallStatus},
    fold::{FoldKey, FoldState, SpanRole, TurnFold},
};

struct RetainedBlock {
    view: Entity<BlockView>,
    _subscriptions: Vec<Subscription>,
}
#[derive(Clone, Copy)]
pub(super) enum LocalFold {
    Toggle,
    Close,
    Open,
}

fn fold_row(key: FoldKey) -> usize {
    match key {
        FoldKey::Span { start, .. } => start,
        FoldKey::Message { history_index }
        | FoldKey::Block { history_index, .. }
        | FoldKey::Item { history_index, .. } => history_index,
    }
}

/// Each pane keeps its own rendering, fold intent, and scroll position while parked.
struct PaneView {
    scroller: Entity<MessageScrollerState>,
    epoch: String,
    fingerprints: Vec<u64>,
    row_data: HashMap<usize, (u64, Vec<BlockData>)>,
    tail_data: Vec<BlockData>,
    blocks: HashMap<String, RetainedBlock>,
    folds: FoldState,
    // A covered row/target can be restored when its representative reopens.
    scroll_return: Option<(usize, usize)>,
    focus_return: Option<(FoldKey, FoldKey)>,
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
            folds: FoldState::default(),
            scroll_return: None,
            focus_return: None,
            restored: HashSet::new(),
            anchors: HashMap::new(),
            revision: u64::MAX,
            busy: false,
            call: None,
        }
    }
    fn source(&self, index: usize) -> &[BlockData] {
        self.row_data
            .get(&index)
            .map(|(_, data)| data.as_slice())
            .unwrap_or(&self.tail_data)
    }
    fn range_key(&self, start: usize) -> String {
        format!("{}-range-{start}", self.epoch)
    }
    fn representative(&self, target: FoldKey) -> FoldKey {
        let row = fold_row(target);
        if let Some((start, end)) = self.folds.span_containing(row) {
            FoldKey::Span { start, end }
        } else if self.folds.entry(row).is_message_folded()
            || matches!(target, FoldKey::Span { .. })
        {
            FoldKey::Message { history_index: row }
        } else {
            target
        }
    }
    /// Visibility order is range, message, then individual block/item. The
    /// canonical row data and its retained renderers are never replaced by a preview.
    fn data(&self, index: usize) -> Vec<BlockData> {
        let Some((_, blocks)) = self.row_data.get(&index) else {
            return self.tail_data.clone();
        };
        let folds = self.folds.entry(index);
        match folds.span() {
            Some(SpanRole::Hidden { .. }) => return Vec::new(),
            Some(SpanRole::Summary { end }) => {
                let count = (index..=end)
                    .filter(|&row| !self.source(row).is_empty())
                    .count();
                return vec![BlockData::range(&self.epoch, index, end, count)];
            }
            None => {}
        }
        let folded = folds.is_message_folded();
        let mut data = if folded {
            blocks.first().cloned().into_iter().collect()
        } else {
            blocks.clone()
        };
        if let Some(first) = data.first_mut().filter(|first| first.target.is_some()) {
            first.message = Some(MessageDisclosure {
                key: FoldKey::Message {
                    history_index: index,
                },
                folded,
                show_speaker: folded
                    || index
                        .checked_sub(1)
                        .is_some_and(|previous| self.folds.entry(previous).span().is_some()),
                blocks: blocks.len(),
                speaker: first.speaker(),
            });
        }
        for block in &mut data {
            block.expanded = !block
                .target
                .is_some_and(|target| self.folds.contains(target));
        }
        data
    }
}
pub struct TranscriptView {
    controller: Entity<SessionController>,
    pane: PaneView,
    selected: Option<zevria_foundation::SubtaskId>,
    parked: HashMap<Option<zevria_foundation::SubtaskId>, PaneView>,
    compact: bool,
    window_focus: FocusHandle,
    _controller: Subscription,
}
impl TranscriptView {
    pub fn new(
        controller: Entity<SessionController>,
        window_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = PaneView::new(cx);
        let selected = controller.read(cx).model.selected_child.clone();
        let watch = cx.observe_in(&controller, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        let mut this = Self {
            controller,
            pane,
            selected,
            parked: HashMap::new(),
            compact: false,
            window_focus,
            _controller: watch,
        };
        this.sync(window, cx);
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
    fn rekey(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
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
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.controller.read(cx).model.selected_child.clone();
        let pane_changed = selected != self.selected;
        let epoch = format!(
            "{:?}",
            self.controller
                .read(cx)
                .model
                .visible()
                .conversation
                .epoch()
        );
        let reset_focus = (pane_changed || epoch != self.pane.epoch)
            && self
                .pane
                .blocks
                .values()
                .any(|block| block.view.read(cx).contains_focus(window, cx));
        if pane_changed {
            let next = self
                .parked
                .remove(&selected)
                .unwrap_or_else(|| PaneView::new(cx));
            let previous = std::mem::replace(&mut self.pane, next);
            self.parked.insert(self.selected.clone(), previous);
            self.selected = selected;
            self.pane.revision = u64::MAX;
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
        let history = model.conversation.history();
        // Management is busy for admission/status, never a conversational live-tip row.
        let busy = model.session.has_turn_work();
        let call = model.session.call_header();
        let revision = controller.projection_revision;
        // The clock and draft edits must not re-fingerprint history or clone the tail.
        if epoch == self.pane.epoch
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
                block.target = None;
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
            let mut tip = BlockData::new(
                format!("{epoch}-tip"),
                BlockKind::Live(LiveTail::Activity(phrase.clone())),
                Gutter::Live,
                phrase,
                true,
            );
            tip.row = history.len();
            tail.push(tip);
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
        let prior_folds = history_changed.then(|| self.pane.folds.clone());
        if replaced {
            self.pane.blocks.clear();
            self.pane.folds.clear();
            self.pane.scroll_return = None;
            self.pane.focus_return = None;
            self.pane.restored = rows
                .values()
                .flat_map(|(_, data)| data.iter().map(|b| b.key.clone()))
                .collect();
        } else if history_changed {
            self.pane.folds.reconcile(&model.conversation);
        }
        self.pane.epoch = epoch;
        // Never attach a retained parser to an ambiguous repeated tail block.
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
        self.pane.row_data = rows;
        self.pane.tail_data = tail;
        self.pane.anchors = anchors;
        self.prune_views(cx);
        self.pane.revision = revision;
        self.pane.busy = busy;
        self.pane.call = call;
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
        if reset_focus {
            // The previous pane's disclosures aren't in this key dispatch path anymore.
            window.focus(&self.window_focus, cx);
        }
        if let Some(before) = prior_folds.as_ref().filter(|_| !replaced) {
            self.remeasure_folds(before, cx);
        }
        self.refresh_views(cx);
    }
    fn prune_views(&mut self, cx: &App) {
        let pane = &mut self.pane;
        let valid = pane
            .row_data
            .values()
            .flat_map(|(_, data)| data)
            .chain(&pane.tail_data)
            .map(|b| b.key.as_str())
            .collect::<HashSet<_>>();
        let range_prefix = format!("{}-range-", pane.epoch);
        pane.blocks.retain(|key, block| {
            valid.contains(key.as_str())
                || (key.starts_with(&range_prefix)
                    && matches!(
                        pane.folds.entry(block.view.read(cx).row()).span(),
                        Some(SpanRole::Summary { .. })
                    ))
        });
        pane.restored.retain(|key| valid.contains(key.as_str()));
    }
    fn refresh_views(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.pane.fingerprints.len() {
            // Do not clone long, unmaterialized history on every stream delta.
            if !self
                .pane
                .source(index)
                .iter()
                .any(|block| self.pane.blocks.contains_key(&block.key))
                && !matches!(
                    self.pane.folds.entry(index).span(),
                    Some(SpanRole::Summary { .. })
                )
            {
                continue;
            }
            for block in self.pane.data(index) {
                if let Some(retained) = self.pane.blocks.get(&block.key) {
                    retained
                        .view
                        .update(cx, |view, cx| view.sync(block, self.compact, cx));
                }
            }
        }
    }
    fn remeasure_folds(&mut self, before: &FoldState, cx: &mut Context<Self>) {
        if before == &self.pane.folds {
            return;
        }
        let pane = &mut self.pane;
        pane.scroller.update(cx, |list, cx| {
            let top = list.logical_scroll_top();
            // The list preserves a live item anchor when remeasuring. Only
            // redirect the top when its representative changes, never unpin the tail.
            if !list.is_following_tail() {
                let origin = pane
                    .scroll_return
                    .filter(|&(row, _)| row == top.item_ix)
                    .map_or(top.item_ix, |(_, origin)| origin);
                let visible = pane
                    .folds
                    .span_containing(origin)
                    .map_or(origin, |(start, _)| start);
                pane.scroll_return = (visible != origin).then_some((visible, origin));
                if visible != top.item_ix
                    || (top.offset_in_item > px(0.)
                        && (before.entry(visible).span() != pane.folds.entry(visible).span()
                            || (!before.entry(visible).is_message_folded()
                                && pane.folds.entry(visible).is_message_folded())))
                {
                    list.scroll_to_item(visible, cx);
                }
            } else {
                pane.scroll_return = None;
            }
            for (&row, (_, blocks)) in &pane.row_data {
                let old = before.entry(row);
                let new = pane.folds.entry(row);
                if old.span() != new.span()
                    || (new.span().is_none()
                        && (old.is_message_folded() != new.is_message_folded()
                            || (!new.is_message_folded()
                                && blocks.iter().filter_map(|b| b.target).any(|target| {
                                    before.contains(target) != pane.folds.contains(target)
                                }))))
                {
                    list.remeasure_items(row..row + 1, cx);
                }
            }
        });
    }
    pub fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.pane.scroller.update(cx, |list, cx| {
            list.scroll_to_end(cx);
        });
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
            &self.pane.folds,
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
    fn focused_target(&self, window: &Window, cx: &App) -> Option<FoldKey> {
        self.pane
            .blocks
            .values()
            .find_map(|b| b.view.read(cx).focused_target(window, cx))
            .filter(|&target| self.pane.representative(target) == target)
    }
    fn focus_target(&mut self, target: FoldKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(data) = self.pane.data(fold_row(target)).into_iter().find(|b| {
            b.target == Some(target) || b.message.is_some_and(|message| message.key == target)
        }) {
            let view = self.retained(data, window, cx);
            window.focus(&view.read(cx).fold_focus(target), cx);
        }
    }
    pub(super) fn fold_local(
        &mut self,
        action: LocalFold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.focused_target(window, cx) {
            self.apply_folds(window, cx, |folds| match action {
                LocalFold::Toggle => folds.toggle(target),
                LocalFold::Close => folds.fold(target),
                LocalFold::Open => folds.unfold(target),
            });
        }
    }
    pub(super) fn fold_turns(
        &mut self,
        action: TurnFold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let controller = self.controller.clone();
        self.update_folds(window, cx, |folds, cx| {
            folds.fold_turns(
                controller.read(cx).model.visible().conversation.history(),
                false,
                action,
            );
        });
    }
    pub(super) fn unfold_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_folds(window, cx, FoldState::unfold_all);
    }
    pub(super) fn thinking(&mut self, expanded: bool, window: &mut Window, cx: &mut Context<Self>) {
        let keys = self
            .pane
            .row_data
            .values()
            .flat_map(|(_, blocks)| blocks)
            .filter(|b| matches!(b.kind, BlockKind::Reasoning))
            .filter_map(|b| b.target)
            .collect::<Vec<_>>();
        self.apply_folds(window, cx, |folds| {
            for target in keys {
                if expanded {
                    folds.unfold(target);
                } else {
                    folds.fold(target);
                }
            }
        });
    }
    fn apply_folds(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut FoldState),
    ) {
        self.update_folds(window, cx, |folds, _| change(folds));
    }
    fn update_folds(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut FoldState, &App),
    ) {
        let focused = self.focused_target(window, cx);
        let before = self.pane.folds.clone();
        change(&mut self.pane.folds, cx);
        if before == self.pane.folds {
            return;
        }
        self.remeasure_folds(&before, cx);
        if let Some(current) = focused {
            let preferred = self
                .pane
                .focus_return
                .take()
                .filter(|&(representative, _)| representative == current)
                .map_or(current, |(_, original)| original);
            let next = self.pane.representative(preferred);
            if next != preferred && !matches!(preferred, FoldKey::Span { .. }) {
                self.pane.focus_return = Some((next, preferred));
            }
            if next != current || (!before.contains(next) && self.pane.folds.contains(next)) {
                self.focus_target(next, window, cx);
            }
        }
        self.prune_views(cx);
        self.refresh_views(cx);
        cx.notify();
    }
    #[cfg(test)]
    pub(super) fn test_data(&self) -> Vec<BlockData> {
        (0..self.pane.fingerprints.len())
            .flat_map(|index| self.pane.data(index))
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
    pub(super) fn test_focus(&self, key: &str, target: FoldKey, cx: &App) -> FocusHandle {
        self.pane
            .blocks
            .get(key)
            .unwrap()
            .view
            .read(cx)
            .fold_focus(target)
    }
    #[cfg(test)]
    pub(super) fn test_clamped(&self, key: &str, cx: &App) -> bool {
        self.pane
            .blocks
            .get(key)
            .unwrap()
            .view
            .read(cx)
            .text_is_clamped(cx)
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
            let view = entity.read(cx);
            if !this
                .pane
                .blocks
                .get(view.key())
                .is_some_and(|block| block.view == entity)
            {
                return;
            }
            let row = view.row();
            if this.pane.folds.entry(row).span().is_some() && view.key() != this.pane.range_key(row)
            {
                return;
            }
            this.pane
                .scroller
                .update(cx, |list, cx| list.remeasure_items(row..row + 1, cx));
            cx.notify();
        });
        let change = cx.subscribe_in(
            &entity,
            window,
            |this, entity, event: &DisclosureChanged, window, cx| {
                if !this
                    .pane
                    .blocks
                    .get(entity.read(cx).key())
                    .is_some_and(|block| block.view == *entity)
                {
                    return;
                }
                if this.pane.representative(event.target) == event.target {
                    this.apply_folds(window, cx, |folds| {
                        if event.expanded {
                            folds.unfold(event.target);
                        } else {
                            folds.fold(event.target);
                        }
                    });
                }
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
        let data = self.pane.data(index);
        if data.is_empty() {
            return div().w_full().h_0().into_any_element();
        }
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
                        .child(style::kbd_hint(format!("{}  Open workspace     {}  New session     {}  Switch mode",
                            Shortcut::OPEN_WORKSPACE.label(), Shortcut::NEW_SESSION.label(), Shortcut::TOGGLE_MODE.label()), cx))))
                .into_any_element();
        }
        div()
            .key_context("ZevriaTranscript")
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
