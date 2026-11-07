use super::{
    blocks::{self, BlockData, BlockView},
    rows::{self, RowChange},
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
            row_data: HashMap::new(),
            blocks: HashMap::new(),
            _controller: watch,
        };
        this.sync(cx);
        this
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        let model = self.controller.read(cx).model.visible();
        let epoch = format!("{:?}", model.conversation.epoch());
        let mut fingerprints = model
            .conversation
            .history()
            .iter()
            .map(rows::fingerprint)
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
        if let Some(entry) = model.conversation.history().get(index) {
            return blocks::entry(&self.epoch, index, entry, self.fingerprints[index]);
        }
        let (text, plain) = match model.tail(Instant::now()) {
            ConversationTail::None => return Vec::new(),
            ConversationTail::Waiting { elapsed } => {
                (format!("Waiting… {:.1}s", elapsed.as_secs_f32()), true)
            }
            ConversationTail::Compacting { elapsed } => {
                (format!("Compacting… {:.1}s", elapsed.as_secs_f32()), true)
            }
            ConversationTail::Streaming { message, .. } => {
                if let Some(entry) =
                    HistoryEntry::from_message(message.clone(), ToolCallStatus::Executing)
                {
                    return blocks::entry(&format!("{}-tail", self.epoch), index, &entry, 0);
                }
                return Vec::new();
            }
            ConversationTail::Retrying {
                notice,
                retrying_for,
                ..
            } => (
                format!(
                    "Reconnecting · attempt {} · {:.1}s remaining\n{}",
                    notice.attempt,
                    retrying_for.as_secs_f32(),
                    notice.error
                ),
                true,
            ),
        };
        let mut data = BlockData::text(format!("{}-tail", self.epoch), String::new(), text, plain);
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
                    block.revision = hash.finish();
                }
            } else if let Some(fingerprint) = self.fingerprints.get(index) {
                self.row_data.insert(index, (*fingerprint, data.clone()));
            }
            data
        };
        let mut row = div().w_full().flex().flex_col().gap_2().px_4().py_2();
        for data in data {
            let key = data.key.clone();
            let entity = if let Some((entity, _)) = self.blocks.get(&key) {
                let entity = entity.clone();
                entity.update(cx, |block, cx| block.sync(data, cx));
                entity
            } else {
                let entity = cx.new(|cx| BlockView::new(data, self.controller.clone(), cx));
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
            row = row.child(entity);
        }
        row.into_any_element()
    }
}
impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity();
        div().size_full().bg(palette(cx).surfaces.canvas).child(
            MessageScroller::new(
                "transcript",
                self.scroller.clone(),
                move |index, window, cx| owner.update(cx, |this, cx| this.row(index, window, cx)),
            )
            .size_full(),
        )
    }
}
