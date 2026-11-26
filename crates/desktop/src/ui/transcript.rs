use super::{
    blocks::{BlockView, DetailResized, DisclosureChanged},
    rows::{self, RowChange},
    style,
    theme::palette,
};
use crate::{
    actions::Shortcut,
    controller::SessionController,
    disclosure::DisclosureState,
    timeline::{BlockKind, Projector, TimelineRow},
    turns::{self, TurnDirection},
};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::{prelude::*, *};
use std::collections::{HashMap, HashSet};
use zevria_presentation::blocks::{NativeHeader, ProjectionEpoch};

struct RetainedBlock {
    view: Entity<BlockView>,
    _subscriptions: [Subscription; 2],
}

#[derive(Clone, Copy)]
pub(super) enum LocalFold {
    Toggle,
    Close,
    Open,
}

/// Each pane retains its own renderer identities, disclosure intent and viewport.
struct PaneView {
    scroller: Entity<MessageScrollerState>,
    epoch: Option<ProjectionEpoch>,
    rows: Vec<TimelineRow>,
    indices: HashMap<String, usize>,
    projector: Projector,
    blocks: HashMap<String, RetainedBlock>,
    disclosures: DisclosureState,
    conversation_revision: u64,
    structure_revision: u64,
    stream_snapshot: Option<zevria_presentation::session::StreamSnapshotId>,
    live_attempt: Option<NativeHeader>,
    live_rows: HashSet<String>,
    job_rows: HashSet<String>,
    jobs_generation: u64,
    can_inspect_jobs: bool,
    activity: Option<String>,
    busy: bool,
    call: Option<NativeHeader>,
    pending: Option<u64>,
    cancelling: bool,
}
impl PaneView {
    fn new(cx: &mut Context<TranscriptView>) -> Self {
        Self {
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            epoch: None,
            rows: vec![],
            indices: HashMap::new(),
            projector: Projector::default(),
            blocks: HashMap::new(),
            disclosures: DisclosureState::default(),
            conversation_revision: u64::MAX,
            structure_revision: u64::MAX,
            stream_snapshot: None,
            live_attempt: None,
            live_rows: HashSet::new(),
            job_rows: HashSet::new(),
            jobs_generation: 0,
            can_inspect_jobs: false,
            activity: None,
            busy: false,
            call: None,
            pending: None,
            cancelling: false,
        }
    }
}

pub struct TranscriptView {
    controller: Entity<SessionController>,
    pane: PaneView,
    selected: Option<zevria_foundation::SubtaskId>,
    parked: HashMap<Option<zevria_foundation::SubtaskId>, PaneView>,
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
        let selected = controller.read(cx).model.controls.selected_child.clone();
        let watch = cx.observe_in(&controller, window, |this, _, window, cx| {
            this.sync(window, cx)
        });
        let mut this = Self {
            controller,
            pane,
            selected,
            parked: HashMap::new(),
            window_focus,
            _controller: watch,
        };
        this.sync(window, cx);
        this
    }

    fn rekey(&mut self, old: &str, new: &str, window: &mut Window, cx: &mut Context<Self>) {
        if old == new {
            return;
        }
        self.pane.disclosures.rekey(old, new);
        if let Some(retained) = self.pane.blocks.remove(old) {
            let focused = retained.view.read(cx).contains_focus(window, cx);
            let target = self.pane.blocks.entry(new.into()).or_insert(retained);
            if focused && !target.view.read(cx).contains_focus(window, cx) {
                window.focus(&target.view.read(cx).disclosure_focus(), cx);
            }
        }
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .controller
            .read(cx)
            .model
            .controls
            .selected_child
            .clone();
        let epoch = self
            .controller
            .read(cx)
            .model
            .visible()
            .conversation
            .epoch();
        let pane_changed = selected != self.selected;
        let reset_focus = (pane_changed || Some(epoch) != self.pane.epoch)
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
            self.pane.scroller.update(cx, |list, cx| list.remeasure(cx));
        }
        let controller = self.controller.read(cx);
        self.parked.retain(|id, _| {
            id.as_ref()
                .is_none_or(|id| controller.model.children.iter().any(|c| &c.id == id))
        });
        let model = controller.model.visible();
        let revision = model.conversation.revision();
        let structure = model.conversation.structure_revision();
        let snapshot = model.session.stream_snapshot();
        let live_attempt = model.live_attempt_header();
        let busy = model.session.has_turn_work();
        let call = model.session.call_header();
        let pending = model.pending_prompt().map(|p| p.id());
        let cancelling = controller.cancelling;
        let replaced = Some(epoch) != self.pane.epoch;
        let activity = busy.then(|| {
            let label = super::status::working_activity(model, cancelling);
            // Reconnecting tips read the clock during paint; their row identity
            // and layout only change when entering or leaving reconnect state.
            if label.starts_with("Reconnecting") {
                "Reconnecting".into()
            } else {
                label
            }
        });
        if !replaced
            && !pane_changed
            && self.pane.conversation_revision == revision
            && self.pane.structure_revision == structure
            && self.pane.stream_snapshot == snapshot
            && self.pane.live_attempt == live_attempt
            && self.pane.activity == activity
            && self.pane.busy == busy
            && self.pane.call == call
            && self.pane.pending == pending
            && self.pane.cancelling == cancelling
        {
            self.sync_live_jobs(window, cx);
            self.notify_live_rows(cx);
            return;
        }
        if replaced {
            self.pane.epoch = Some(epoch);
            self.pane.projector = Projector::default();
            self.pane.disclosures = DisclosureState::default();
            self.pane.blocks.clear();
            self.pane.jobs_generation = 0;
            self.pane.can_inspect_jobs = false;
        }
        let mut projection = self.pane.projector.project_timeline(
            model,
            &format!("{epoch:?}"),
            model.unsaved_blocks(),
            activity.as_deref(),
            live_attempt,
            &self.pane.rows,
        );
        let start = projection.start_row;
        let old_suffix = &self.pane.rows[start..];
        let valid: HashSet<_> = projection
            .rows
            .iter()
            .map(|row| row.data.key.as_str())
            .collect();
        let mut aliases = HashMap::new();
        if !replaced {
            let old_keys: HashSet<_> = old_suffix.iter().map(|row| row.data.key.as_str()).collect();
            aliases.extend(
                projection
                    .transfers
                    .into_iter()
                    .filter(|(_, new)| !old_keys.contains(new.as_str())),
            );
            for old in old_suffix {
                if let Some(anchor) = old.data.source {
                    let resolved = model.conversation.resolved_identity(anchor);
                    if let Some(new) = projection.anchors.get(&resolved) {
                        // A known anchor takes precedence over a live ordinal match.
                        aliases.remove(&old.data.key);
                        if *new != old.data.key
                            && !valid.contains(old.data.key.as_str())
                            && (resolved != anchor || !old_keys.contains(new.as_str()))
                        {
                            aliases.insert(old.data.key.clone(), new.clone());
                        }
                    }
                }
            }
        }
        for (old, new) in &aliases {
            self.rekey(old, new, window, cx);
        }
        let old = self.pane.rows.split_off(start);
        for row in &old {
            self.pane.indices.remove(&row.data.key);
            self.pane.live_rows.remove(&row.data.key);
            self.pane.job_rows.remove(&row.data.key);
            if !valid.contains(row.data.key.as_str()) {
                self.pane.blocks.remove(&row.data.key);
                self.pane.disclosures.remove(&row.data.key);
            }
        }
        let jobs = self
            .selected
            .is_none()
            .then(|| self.controller.read(cx).model.root.jobs_inspector());
        for (i, row) in projection.rows.iter_mut().enumerate() {
            row.apply_disclosure(&self.pane.disclosures);
            if row.data.tracks_live_jobs() {
                self.pane.job_rows.insert(row.data.key.clone());
            }
            if row.needs_clock() || jobs.is_some_and(|jobs| row.needs_job_clock(jobs)) {
                self.pane.live_rows.insert(row.data.key.clone());
            }
            self.pane.indices.insert(row.data.key.clone(), start + i);
        }
        self.pane.rows.append(&mut projection.rows);
        self.pane.conversation_revision = revision;
        self.pane.structure_revision = structure;
        self.pane.stream_snapshot = snapshot;
        self.pane.live_attempt = live_attempt;
        self.pane.activity = activity;
        self.pane.busy = busy;
        self.pane.call = call;
        self.pane.pending = pending;
        self.pane.cancelling = cancelling;
        let mut sync = Vec::new();
        if replaced {
            self.pane
                .scroller
                .update(cx, |list, cx| list.reset(self.pane.rows.len(), cx));
        } else {
            let old_keys: HashSet<_> = old.iter().map(|row| row.data.key.as_str()).collect();
            let mut renamed = HashSet::new();
            let before = old
                .iter()
                .map(|row| {
                    let key = aliases
                        .get(&row.data.key)
                        .filter(|next| {
                            self.pane.indices.contains_key(*next)
                                && !old_keys.contains(next.as_str())
                                && renamed.insert(next.as_str())
                        })
                        .unwrap_or(&row.data.key);
                    (key.as_str(), row.fingerprint)
                })
                .collect::<Vec<_>>();
            let after = self.pane.rows[start..]
                .iter()
                .map(|row| (row.data.key.as_str(), row.fingerprint))
                .collect::<Vec<_>>();
            for change in rows::reconcile(&before, &after) {
                match change {
                    RowChange::None => {}
                    RowChange::Remeasure(indices) => {
                        self.pane.scroller.update(cx, |list, cx| {
                            for &i in &indices {
                                let i = start + i;
                                list.remeasure_items(i..i + 1, cx);
                            }
                        });
                        sync.extend(indices.into_iter().map(|i| start + i));
                    }
                    RowChange::Splice { old, count } => {
                        sync.extend(start + old.start..start + old.start + count);
                        self.pane.scroller.update(cx, |list, cx| {
                            list.splice(start + old.start..start + old.end, count, cx)
                        });
                    }
                }
            }
        }
        if reset_focus {
            window.focus(&self.window_focus, cx);
        }
        for (_, key) in aliases {
            if let Some(&index) = self.pane.indices.get(&key) {
                sync.push(index);
            }
        }
        sync.sort_unstable();
        sync.dedup();
        for index in sync {
            let row = &self.pane.rows[index];
            if let Some(block) = self.pane.blocks.get(&row.data.key) {
                block
                    .view
                    .update(cx, |view, cx| view.sync(row.clone(), window, cx));
            }
        }
        self.sync_live_jobs(window, cx);
        self.notify_live_rows(cx);
        cx.notify();
    }

    fn sync_live_jobs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            return;
        }
        let controller = self.controller.read(cx);
        let jobs = controller.model.root.jobs_inspector();
        let generation = jobs.generation();
        let can_inspect = controller.can_inspect_jobs();
        let jobs_changed = generation != self.pane.jobs_generation;
        if !jobs_changed && can_inspect == self.pane.can_inspect_jobs {
            return;
        }
        self.pane.jobs_generation = generation;
        self.pane.can_inspect_jobs = can_inspect;
        // Job events and panel availability can change without a history
        // revision. Update retained actions, including those on static cards.
        let mut remeasure = Vec::new();
        for key in &self.pane.job_rows {
            let index = self.pane.indices[key];
            if jobs_changed {
                let row = &self.pane.rows[index];
                if row.needs_clock() || row.needs_job_clock(jobs) {
                    self.pane.live_rows.insert(key.clone());
                } else {
                    self.pane.live_rows.remove(key);
                }
            }
            if self.pane.blocks.contains_key(key) {
                remeasure.push(index);
            }
        }
        for &index in &remeasure {
            self.pane.blocks[&self.pane.rows[index].data.key]
                .view
                .update(cx, |view, cx| {
                    view.refresh_jobs(window, cx);
                });
        }
        if !remeasure.is_empty() {
            self.pane.scroller.update(cx, |list, cx| {
                for index in remeasure {
                    list.remeasure_items(index..index + 1, cx);
                }
            });
        }
    }

    fn notify_live_rows(&self, cx: &mut Context<Self>) {
        for key in &self.pane.live_rows {
            if let Some(block) = self.pane.blocks.get(key) {
                block.view.update(cx, |_, cx| cx.notify());
            }
        }
    }

    pub fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.pane
            .scroller
            .update(cx, |list, cx| list.scroll_to_end(cx));
    }
    pub fn jump_to_beginning(&mut self, cx: &mut Context<Self>) {
        self.pane
            .scroller
            .update(cx, |list, cx| list.scroll_to_item(0, cx));
    }
    pub fn jump_turn(&mut self, direction: TurnDirection, cx: &mut Context<Self>) -> bool {
        let starts = turns::turn_rows(&self.pane.rows);
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

    pub fn has_expanded_details(&self) -> bool {
        self.pane.disclosures.has_any_expanded(
            self.pane
                .rows
                .iter()
                .filter(|row| row.data.is_disclosure())
                .map(|row| (row.data.key.as_str(), row.default_expanded())),
        )
    }
    pub fn toggle_all_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_expanded_details() {
            self.collapse_all(window, cx);
        } else {
            self.expand_all(window, cx);
        }
    }
    pub fn expand_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pane.disclosures.expand_keys(
            self.pane
                .rows
                .iter()
                .filter(|r| r.data.is_disclosure())
                .map(|r| r.data.key.clone()),
        );
        self.update_disclosures(0..self.pane.rows.len(), window, cx);
    }
    pub fn collapse_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pane.disclosures.collapse_all(
            self.pane
                .rows
                .iter()
                .filter(|r| r.data.is_disclosure() && r.default_expanded())
                .map(|r| r.data.key.clone()),
        );
        self.update_disclosures(0..self.pane.rows.len(), window, cx);
    }
    pub fn set_all_thinking(
        &mut self,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys = self
            .pane
            .rows
            .iter()
            .filter(|r| r.data.thought().is_some())
            .map(|r| r.data.key.clone());
        if expanded {
            self.pane.disclosures.expand_keys(keys);
        } else {
            self.pane.disclosures.collapse_keys(keys);
        }
        self.update_disclosures(0..self.pane.rows.len(), window, cx);
    }
    pub(super) fn fold_local(
        &mut self,
        action: LocalFold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.pane.blocks.iter().find_map(|(key, block)| {
            block
                .view
                .read(cx)
                .has_disclosure_focus(window)
                .then(|| key.clone())
        });
        let Some(index) = key.as_ref().and_then(|k| self.pane.indices.get(k)).copied() else {
            return;
        };
        let key = key.unwrap();
        match action {
            LocalFold::Toggle => self
                .pane
                .disclosures
                .toggle(key, self.pane.rows[index].expanded),
            LocalFold::Open => self.pane.disclosures.set(key, true),
            LocalFold::Close => self.pane.disclosures.set(key, false),
        }
        self.update_disclosures(index..index + 1, window, cx);
    }
    fn update_disclosures(
        &mut self,
        indices: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for index in indices {
            let row = &mut self.pane.rows[index];
            let before = row.expanded;
            row.apply_disclosure(&self.pane.disclosures);
            if before != row.expanded {
                if let Some(block) = self.pane.blocks.get(&row.data.key) {
                    block
                        .view
                        .update(cx, |view, cx| view.sync(row.clone(), window, cx));
                }
                self.pane
                    .scroller
                    .update(cx, |list, cx| list.remeasure_items(index..index + 1, cx));
            }
        }
        cx.notify();
    }

    fn retained(
        &mut self,
        row: TimelineRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BlockView> {
        let key = row.data.key.clone();
        if let Some(block) = self.pane.blocks.get(&key) {
            return block.view.clone();
        }
        let entity = cx.new(|cx| BlockView::new(row, self.controller.clone(), cx));
        let resize = cx.subscribe(&entity, |this, entity, _: &DetailResized, cx| {
            let key = entity.read(cx).key();
            if this
                .pane
                .blocks
                .get(key)
                .is_some_and(|retained| retained.view == entity)
                && let Some(&index) = this.pane.indices.get(key)
            {
                this.pane
                    .scroller
                    .update(cx, |list, cx| list.remeasure_items(index..index + 1, cx));
            }
        });
        let change = cx.subscribe_in(
            &entity,
            window,
            |this, entity, event: &DisclosureChanged, window, cx| {
                let key = entity.read(cx).key().to_owned();
                if this
                    .pane
                    .blocks
                    .get(&key)
                    .is_some_and(|retained| retained.view == *entity)
                    && let Some(&index) = this.pane.indices.get(&key)
                {
                    this.pane.disclosures.set(key, event.expanded);
                    this.update_disclosures(index..index + 1, window, cx);
                }
            },
        );
        self.pane.blocks.insert(
            key,
            RetainedBlock {
                view: entity.clone(),
                _subscriptions: [resize, change],
            },
        );
        entity
    }

    fn row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.pane.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let model = self.controller.read(cx).model.visible();
        let paint = match row.data.kind {
            BlockKind::Prompt {
                preparing: true, ..
            } => model.pending_prompt().and_then(|p| p.paint_probe()),
            BlockKind::Prompt { .. } => model
                .accepted_prompt_paint()
                .filter(|(i, _)| *i == row.data.history_index)
                .map(|(_, paint)| paint),
            _ => None,
        }
        .cloned();
        div()
            .w_full()
            .px(px(style::COLUMN_INSET))
            .pt(px(f32::from(row.gap)))
            .when(row.last, |r| r.pb(px(16.)))
            .child(style::Measure::column().child(self.retained(row, window, cx)))
            .when_some(paint, |row, paint| {
                // List measurement and overscan are not visible paints.
                row.relative().child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| {
                            if bounds.intersects(&window.content_mask().bounds) {
                                paint.mark_painted();
                            }
                        },
                    )
                    .absolute()
                    .size_full()
                    .top_0()
                    .left_0(),
                )
            })
            .into_any_element()
    }

    #[cfg(test)]
    pub(super) fn scroller(&self) -> Entity<MessageScrollerState> {
        self.pane.scroller.clone()
    }
    #[cfg(test)]
    pub(super) fn test_data(&self, _: &App) -> Vec<TimelineRow> {
        self.pane.rows.clone()
    }
    #[cfg(test)]
    pub(super) fn test_row_ticks(&self, key: &str) -> bool {
        self.pane.live_rows.contains(key)
    }
    #[cfg(test)]
    pub(super) fn test_identity(&self, key: &str, cx: &App) -> Option<(EntityId, EntityId)> {
        self.pane
            .blocks
            .get(key)
            .map(|b| (b.view.entity_id(), b.view.read(cx).text_id()))
    }
    #[cfg(test)]
    pub(super) fn test_focus(&self, key: &str, cx: &App) -> FocusHandle {
        self.pane.blocks[key].view.read(cx).disclosure_focus()
    }
    #[cfg(test)]
    pub(super) fn test_prepared(&self, key: &str, cx: &App) -> bool {
        self.pane
            .blocks
            .get(key)
            .is_some_and(|b| b.view.read(cx).test_prepared())
    }
}

impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity();
        let controller = self.controller.read(cx);
        if controller.phase == crate::controller::Phase::Live
            && self.selected.is_none()
            && self.pane.rows.is_empty()
        {
            let name = controller
                .workspace
                .as_deref()
                .and_then(|p| p.file_name())
                .map_or_else(|| "Zevria".into(), |n| n.to_string_lossy().into_owned());
            return div().id("transcript").size_full().px(px(style::COLUMN_INSET)).pt(px(64.))
                .child(style::Measure::column().flex().flex_col().gap(px(12.))
                    .child(div().text_size(px(30.)).line_height(px(36.)).font_weight(FontWeight::SEMIBOLD).child(name))
                    .child(style::body().text_color(palette(cx).text.secondary).child("Describe a change to build, or switch to Plan to work through an approach."))
                    .child(style::kbd_hint(format!("{}  Open workspace     {}  New session     {}  Switch mode", Shortcut::OPEN_WORKSPACE.label(), Shortcut::NEW_SESSION.label(), Shortcut::TOGGLE_MODE.label()), cx)))
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
                    move |i, window, cx| owner.update(cx, |this, cx| this.row(i, window, cx)),
                )
                .with_row_style(StyleRefinement::default().px_0().pb_0())
                .jump_button(true)
                .with_jump_button_label("Jump to latest")
                .size_full(),
            )
            .into_any_element()
    }
}
