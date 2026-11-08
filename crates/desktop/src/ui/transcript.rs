use super::{
    blocks::{self, BlockData, BlockKind, BlockView, Gutter, LiveTail},
    rows::{self, RowChange},
    style,
    theme::palette,
};
use crate::controller::SessionController;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::{prelude::*, *};
use std::{collections::HashMap, time::Instant};
use zevria_presentation::{
    ConversationTail,
    conversation::{HistoryEntry, ToolCallStatus},
};
pub struct TranscriptView {
    controller: Entity<SessionController>,
    scroller: Entity<MessageScrollerState>,
    epoch: String,
    fingerprints: Vec<u64>,
    compact: bool,
    row_data: HashMap<usize, (u64, Vec<BlockData>)>,
    blocks: HashMap<String, (Entity<BlockView>, Subscription)>,
    _controller: Subscription,
}
impl TranscriptView {
    pub fn new(controller: Entity<SessionController>, cx: &mut Context<Self>) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let watch = cx.observe(&controller, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        let mut this = Self {
            controller,
            scroller,
            epoch: String::new(),
            fingerprints: Vec::new(),
            compact: false,
            row_data: HashMap::new(),
            blocks: HashMap::new(),
            _controller: watch,
        };
        this.sync(cx);
        this
    }
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        self.row_data.clear();
        self.scroller.update(cx, |list, cx| {
            list.remeasure_items(0..self.fingerprints.len(), cx)
        });
        for (block, _) in self.blocks.values() {
            block.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        let model = self.controller.read(cx).model.visible();
        let epoch = format!("{:?}", model.conversation.epoch());
        let history = model.conversation.history();
        let mut fingerprints = history
            .iter()
            .enumerate()
            .map(|(i, e)| {
                rows::fingerprint(
                    e,
                    rows::continues_turn(i.checked_sub(1).and_then(|p| history.get(p)), e),
                )
            })
            .collect::<Vec<_>>();
        // The live row is stable, including when it becomes empty.
        fingerprints.push(0);
        if epoch != self.epoch {
            self.epoch = epoch;
            self.row_data.clear();
            self.blocks.clear();
            self.scroller
                .update(cx, |list, cx| list.reset(fingerprints.len(), cx));
        } else {
            match rows::compare(&self.fingerprints, &fingerprints) {
                RowChange::None => {}
                RowChange::Remeasure(indices) => {
                    for index in indices {
                        self.row_data.remove(&index);
                        self.scroller
                            .update(cx, |list, cx| list.remeasure_items(index..index + 1, cx));
                    }
                }
                RowChange::Splice { old, count } => {
                    self.row_data.retain(|index, _| *index < old.start);
                    self.scroller
                        .update(cx, |list, cx| list.splice(old, count, cx));
                }
            }
            let last = fingerprints.len() - 1;
            self.row_data.remove(&last);
            self.scroller
                .update(cx, |list, cx| list.remeasure_items(last..last + 1, cx));
        }
        self.fingerprints = fingerprints;
    }
    fn data(&self, index: usize, cx: &App) -> Vec<BlockData> {
        let model = self.controller.read(cx).model.visible();
        let history = model.conversation.history();
        let child = self.controller.read(cx).model.selected_child.is_some();
        if let Some(entry) = history.get(index) {
            let continues =
                rows::continues_turn(index.checked_sub(1).and_then(|p| history.get(p)), entry);
            return blocks::entry(
                &self.epoch,
                index,
                entry,
                self.fingerprints[index],
                continues,
                child,
            );
        }
        let (tail, gutter) = match model.tail(Instant::now()) {
            ConversationTail::None => return Vec::new(),
            ConversationTail::Waiting { elapsed } => (
                LiveTail::Waiting {
                    elapsed: format!("{:.1} s", elapsed.as_secs_f32()),
                },
                Gutter::Live,
            ),
            ConversationTail::Compacting { elapsed } => (
                LiveTail::Compacting {
                    elapsed: format!("{:.1} s", elapsed.as_secs_f32()),
                },
                Gutter::Live,
            ),
            ConversationTail::Streaming { message, .. } => {
                if let Some(entry) =
                    HistoryEntry::from_message(message.clone(), ToolCallStatus::Executing)
                {
                    return blocks::entry(
                        &format!("{}-tail", self.epoch),
                        index,
                        &entry,
                        0,
                        history.last().is_some_and(rows::is_assistant),
                        child,
                    );
                }
                return Vec::new();
            }
            ConversationTail::Retrying {
                notice,
                retrying_for,
                ..
            } => (
                LiveTail::Retrying {
                    attempt: notice.attempt,
                    remaining: format!("{:.1} s", retrying_for.as_secs_f32()),
                    error: notice.error.clone(),
                },
                Gutter::Glyph(zevria_presentation::status_icon::StatusIcon::Denied),
            ),
        };
        let mut data = BlockData::new(
            format!("{}-tail", self.epoch),
            BlockKind::Live(tail.clone()),
            gutter,
            format!("{tail:?}"),
            true,
        );
        // Tail equality cannot rely on durable block revisions.
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        data.body.hash(&mut hash);
        data.revision = hash.finish();
        vec![data]
    }
    fn row(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let is_tail = index + 1 == self.fingerprints.len();
        let data = if !is_tail
            && let Some((fingerprint, data)) = self.row_data.get(&index)
            && Some(fingerprint) == self.fingerprints.get(index)
        {
            data.clone()
        } else {
            let mut data = self.data(index, cx);
            if is_tail {
                for block in &mut data {
                    use std::hash::{Hash, Hasher};
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    block.body.hash(&mut hash);
                    format!("{:?}", block.gutter).hash(&mut hash);
                    block.revision = hash.finish();
                }
            } else if let Some(fingerprint) = self.fingerprints.get(index) {
                self.row_data.insert(index, (*fingerprint, data.clone()));
            }
            data
        };
        let tools_only = !data.is_empty()
            && data
                .iter()
                .all(|d| matches!(d.kind, BlockKind::Tool { .. } | BlockKind::Subtask { .. }));
        let history = self
            .controller
            .read(cx)
            .model
            .visible()
            .conversation
            .history();
        let mut previous_tool =
            index
                .checked_sub(1)
                .and_then(|i| history.get(i))
                .map(|e| match e {
                    HistoryEntry::Conversation(e) => {
                        e.blocks
                            .iter()
                            .rfind(|b| b.visible(false))
                            .is_some_and(|b| {
                                matches!(
                            b.kind,
                            zevria_presentation::blocks::PresentationBlockKind::Tool(_)
                                | zevria_presentation::blocks::PresentationBlockKind::Subtask { .. }
                        )
                            })
                    }
                    _ => false,
                });
        let mut row = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(16.))
            .py(px(if tools_only { 1. } else { 4. }));
        for data in data {
            let tool = matches!(
                data.kind,
                BlockKind::Tool { .. } | BlockKind::Subtask { .. }
            );
            let extra_gap = if previous_tool.is_some_and(|previous| previous != tool) {
                8.
            } else {
                0.
            };
            previous_tool = Some(tool);
            let key = data.key.clone();
            let entity = if let Some((entity, _)) = self.blocks.get(&key) {
                let entity = entity.clone();
                entity.update(cx, |block, cx| block.sync(data, self.compact, cx));
                entity
            } else {
                let entity =
                    cx.new(|cx| BlockView::new(data, self.controller.clone(), self.compact, cx));
                let stable_key = key.clone();
                let observe = cx.observe(&entity, move |this, _, cx| {
                    let index = this
                        .row_data
                        .iter()
                        .find_map(|(index, (_, data))| {
                            data.iter().any(|b| b.key == stable_key).then_some(*index)
                        })
                        .unwrap_or(this.fingerprints.len().saturating_sub(1));
                    this.scroller
                        .update(cx, |list, cx| list.remeasure_items(index..index + 1, cx));
                    cx.notify();
                });
                self.blocks.insert(key, (entity.clone(), observe));
                entity
            };
            row = row.child(div().pt(px(extra_gap)).child(entity));
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
            let path = controller
                .workspace
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
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
                        .child(style::mono().text_size(px(12.)).text_color(palette(cx).text.muted).child(path))
                        .child(style::body().text_color(palette(cx).text.secondary).child("Describe a change for Zevria to build, or switch to Plan to review an approach before any file changes."))))
                .into_any_element();
        }
        div()
            .size_full()
            .bg(palette(cx).surfaces.canvas)
            .child(
                MessageScroller::new(
                    "transcript",
                    self.scroller.clone(),
                    move |index, window, cx| {
                        owner.update(cx, |this, cx| this.row(index, window, cx))
                    },
                )
                .jump_button(true)
                .with_jump_button_label("Jump to latest")
                .size_full(),
            )
            .into_any_element()
    }
}
