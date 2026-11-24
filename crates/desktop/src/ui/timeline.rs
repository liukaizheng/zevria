//! Desktop's display-only timeline. The projection does not depend on GPUI or modify session state.
use crate::{
    disclosure::{DisclosureState, Phase, TailIdentity, TailKind, can_commit},
    rows,
};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
    time::Instant,
};
use zevria_content::{CitedSource, PromptImage};
use zevria_foundation::SubtaskDescriptor;
use zevria_presentation::{
    ConversationTail, PromptPreview, SessionModel,
    blocks::*,
    conversation::{ConversationState, HistoryEntry, ToolCallStatus},
    jobs::{JobRow, notice_rows},
    status_icon::StatusIcon,
};

#[path = "blocks/command.rs"]
pub mod command;
#[path = "blocks/tools.rs"]
pub mod tools;
#[path = "blocks/web_model.rs"]
pub mod web;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StepPosition {
    Only,
    First,
    Middle,
    Last,
}
impl StepPosition {
    pub fn starts(self) -> bool {
        matches!(self, Self::Only | Self::First)
    }
    pub fn ends(self) -> bool {
        matches!(self, Self::Only | Self::Last)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub label: String,
    pub image: PromptImage,
}

#[derive(Clone, Debug)]
pub struct ThoughtSegment {
    pub text: Arc<str>,
    pub phase: Phase,
    pub word_count: usize,
    pub headline: Arc<str>,
    stamp: u64,
}

#[derive(Clone, Debug)]
pub struct ThoughtGroup {
    pub segments: Vec<ThoughtSegment>,
    pub word_count: usize,
    pub is_live: bool,
}
impl ThoughtGroup {
    fn new(segments: Vec<ThoughtSegment>) -> Self {
        Self {
            word_count: segments.iter().map(|segment| segment.word_count).sum(),
            is_live: segments
                .last()
                .is_some_and(|segment| segment.phase == Phase::Live),
            segments,
        }
    }
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_ref())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    fn phase(&self) -> Phase {
        if self.is_live {
            Phase::Live
        } else if self.segments.iter().any(|s| s.phase == Phase::Unsaved) {
            Phase::Unsaved
        } else if self.segments.iter().any(|s| s.phase == Phase::Interrupted) {
            Phase::Interrupted
        } else if self.segments.iter().all(|s| s.phase == Phase::Restored) {
            Phase::Restored
        } else if self.segments.iter().any(|s| s.phase == Phase::Working) {
            Phase::Working
        } else {
            Phase::Finished
        }
    }
}

#[derive(Clone, Debug)]
pub enum Step {
    Tool(Arc<tools::ToolPresentation>),
    Web(web::WebRow),
    Subtask(SubtaskDescriptor),
}
impl Step {
    pub fn status(&self) -> StatusIcon {
        match self {
            Self::Tool(tool) => tool.status,
            Self::Web(web) => web.status(),
            Self::Subtask(task) => subtask_icon(task.status),
        }
    }
}

#[derive(Clone, Debug)]
pub enum BlockKind {
    Prompt {
        images: Vec<Attachment>,
        preparing: bool,
    },
    ThoughtGroup(ThoughtGroup),
    Step(Step),
    Prose {
        plain: bool,
    },
    Image(Attachment),
    Document {
        title: String,
        version: String,
        handoff: bool,
    },
    Jobs {
        finished: Vec<JobRow>,
        running: Vec<JobRow>,
    },
    Notice(NoticeLevel),
    Ensemble {
        status: Option<StatusIcon>,
    },
    Divider,
    Live,
}

pub type Anchor = (usize, PresentationBlockId);

#[derive(Clone, Debug)]
pub struct BlockData {
    pub key: String,
    pub revision: u64,
    pub kind: BlockKind,
    pub body: Arc<str>,
    primary: Arc<str>,
    pub secondary: Option<Arc<str>>,
    pub sources: Vec<CitedSource>,
    pub history_index: usize,
    pub source: Option<Anchor>,
}
impl BlockData {
    fn new(key: String, history_index: usize, kind: BlockKind, body: impl Into<Arc<str>>) -> Self {
        let body = body.into();
        Self {
            key,
            history_index,
            kind,
            primary: body.clone(),
            body,
            revision: 0,
            sources: vec![],
            secondary: None,
            source: None,
        }
    }
    pub fn thought(&self) -> Option<&ThoughtGroup> {
        match &self.kind {
            BlockKind::ThoughtGroup(thought) => Some(thought),
            _ => None,
        }
    }
    pub fn primary_copy(&self) -> String {
        match &self.kind {
            BlockKind::ThoughtGroup(thought) => thought.text(),
            BlockKind::Step(Step::Web(web)) => web.copy_text(),
            _ => self.primary.to_string(),
        }
    }
    pub fn is_disclosure(&self) -> bool {
        self.thought().is_some() || matches!(self.kind, BlockKind::Step(_))
    }
    pub fn default_expanded(&self) -> bool {
        match &self.kind {
            BlockKind::ThoughtGroup(thought) => thought.is_live,
            BlockKind::Step(step) => step.status() == StatusIcon::Running,
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TimelineRow {
    pub data: Arc<BlockData>,
    pub turn_start: bool,
    pub position: Option<StepPosition>,
    pub phase: Phase,
    pub expanded: bool,
    pub gap: u8,
    pub last: bool,
    pub fingerprint: u64,
}
impl TimelineRow {
    fn new(data: Arc<BlockData>, phase: Phase) -> Self {
        Self {
            data,
            phase,
            turn_start: false,
            position: None,
            expanded: false,
            gap: 0,
            last: false,
            fingerprint: 0,
        }
    }
    pub fn needs_clock(&self) -> bool {
        self.phase == Phase::Live
            || matches!(&self.data.kind, BlockKind::Step(step) if step.status() == StatusIcon::Running)
    }

    pub fn apply_disclosure(&mut self, disclosures: &DisclosureState) {
        self.expanded = if self.data.is_disclosure() {
            disclosures.is_expanded(&self.data.key, self.data.default_expanded())
        } else {
            true
        };
        self.fingerprint = hash((
            &self.data.key,
            self.data.revision,
            self.data.history_index,
            self.data.source,
            self.turn_start,
            self.position,
            self.phase,
            self.expanded,
            self.gap,
            self.last,
        ));
    }
}

fn hash(value: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn headline(text: &str) -> Arc<str> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = line
        .trim_start_matches(['#', '*', '>'])
        .trim()
        .trim_end_matches('*');
    let headline = line
        .split_once(". ")
        .map_or(line, |(first, _)| &line[..first.len() + 1]);
    let mut chars = headline.chars();
    let mut text: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text.into()
}

#[derive(Clone)]
struct Binding {
    identity: TailIdentity,
    key: String,
    text: Arc<str>,
}

#[derive(Clone)]
enum Fragment {
    Prompt(Arc<BlockData>),
    Reasoning {
        segment: ThoughtSegment,
        identity: TailIdentity,
        history_index: usize,
        source: Option<PresentationBlockId>,
    },
    Row {
        data: Arc<BlockData>,
        identity: Option<TailIdentity>,
    },
}

struct Entry {
    stamp: u64,
    fragments: Vec<Fragment>,
    header: Option<NativeHeader>,
    starts_turn: bool,
    interrupted: bool,
    divider: bool,
}

#[derive(Clone, Copy)]
struct Checkpoint {
    history: usize,
    row: usize,
    revision: u64,
    structure: u64,
    call: Option<NativeHeader>,
    busy: bool,
    live_attempt: Option<NativeHeader>,
}

#[derive(Default)]
pub struct Projector {
    entries: Vec<Entry>,
    restored: HashSet<PresentationBlockId>,
    live: Vec<Binding>,
    pending: Option<(u64, Arc<BlockData>)>,
    checkpoint: Option<Checkpoint>,
}

pub struct Projection {
    pub start_row: usize,
    pub rows: Vec<TimelineRow>,
    /// Unambiguous live-to-canonical renderer/disclosure identity transfers.
    pub transfers: Vec<(String, String)>,
    pub anchors: HashMap<Anchor, String>,
}

impl Projector {
    pub fn project_timeline(
        &mut self,
        model: &SessionModel,
        epoch: &str,
        unsaved: &HashSet<(ProjectionEpoch, PresentationBlockId)>,
        activity: Option<&str>,
        live_attempt: Option<NativeHeader>,
        old_rows: &[TimelineRow],
    ) -> Projection {
        let streaming = match model.tail(Instant::now()) {
            ConversationTail::Streaming { message, .. } => {
                HistoryEntry::from_message(message.clone(), ToolCallStatus::Executing)
            }
            _ => None,
        };
        let conversation = &model.conversation;
        let call = model.session.call_header();
        let busy = model.session.has_turn_work();
        let checkpoint = self.checkpoint.filter(|checkpoint| {
            checkpoint.history < conversation.history().len()
                && checkpoint.row < old_rows.len()
                && checkpoint.structure == conversation.structure_revision()
                && checkpoint.call == call
                && checkpoint.busy == busy
                && checkpoint.live_attempt == live_attempt
                && conversation
                    .changed_entries_since(checkpoint.revision)
                    .all(|index| index > checkpoint.history)
        });
        self.project_from(
            conversation,
            epoch,
            call,
            busy,
            streaming.as_ref(),
            model.pending_prompt(),
            unsaved,
            activity,
            live_attempt,
            checkpoint,
            old_rows,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn project(
        &mut self,
        conversation: &ConversationState,
        epoch: &str,
        call: Option<NativeHeader>,
        busy: bool,
        stream: Option<&HistoryEntry>,
        pending: Option<&PromptPreview>,
        unsaved: &HashSet<(ProjectionEpoch, PresentationBlockId)>,
        activity: Option<&str>,
        live_attempt: Option<NativeHeader>,
    ) -> Projection {
        self.project_from(
            conversation,
            epoch,
            call,
            busy,
            stream,
            pending,
            unsaved,
            activity,
            live_attempt,
            None,
            &[],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn project_from(
        &mut self,
        conversation: &ConversationState,
        epoch: &str,
        call: Option<NativeHeader>,
        busy: bool,
        stream: Option<&HistoryEntry>,
        pending: Option<&PromptPreview>,
        unsaved: &HashSet<(ProjectionEpoch, PresentationBlockId)>,
        activity: Option<&str>,
        live_attempt: Option<NativeHeader>,
        checkpoint: Option<Checkpoint>,
        old_rows: &[TimelineRow],
    ) -> Projection {
        let history = conversation.history();
        let previous_len = self.entries.len();
        if self.entries.is_empty() && !busy && self.live.is_empty() {
            self.restored.extend(
                history
                    .iter()
                    .flat_map(|entry| match entry {
                        HistoryEntry::Conversation(entry) => entry.blocks.as_slice(),
                        _ => &[],
                    })
                    .map(|block| block.id),
            );
        }
        self.entries.truncate(history.len());
        let mut timeline = Timeline::new(epoch);
        let from = checkpoint.map_or(0, |checkpoint| checkpoint.history);
        let start_row = checkpoint.map_or(0, |checkpoint| checkpoint.row);
        let mut next_checkpoint = None;
        let mut next_live = Vec::new();
        let mut committed = Vec::new();
        let previous_call = self.live.first().map(|binding| binding.identity.header);
        let live_entry = live_attempt
            .filter(|_| stream.is_none())
            .and_then(|header| conversation.represented_header_index(header));
        for (index, entry) in history.iter().enumerate().skip(from) {
            let stamp = rows::fingerprint(conversation, index);
            if self
                .entries
                .get(index)
                .is_none_or(|entry| entry.stamp != stamp)
            {
                let entry = project_entry(epoch, index, entry, stamp, None);
                if index == self.entries.len() {
                    self.entries.push(entry);
                } else {
                    self.entries[index] = entry;
                }
            }
            let entry = &self.entries[index];
            if entry.starts_turn && !entry.fragments.is_empty() {
                timeline.finish_turn();
                next_checkpoint = Some(Checkpoint {
                    history: index,
                    row: start_row + timeline.rows.len(),
                    revision: conversation.revision(),
                    structure: conversation.structure_revision(),
                    call,
                    busy,
                    live_attempt,
                });
            }
            let track_commit = previous_call == Some(entry.header)
                && (entry.header.is_some() || index >= previous_len.saturating_sub(1));
            let working = busy
                && entry.header.is_some()
                && entry
                    .header
                    .zip(call)
                    .is_some_and(|(a, b)| a.turn() == b.turn());
            let live = live_entry == Some(index) && !entry.interrupted;
            let bindings = if live {
                Some(&mut next_live)
            } else {
                track_commit.then_some(&mut committed)
            };
            timeline.push_entry(entry, live, bindings, |source| {
                if source.is_some_and(|id| unsaved.contains(&(conversation.epoch(), id))) {
                    Phase::Unsaved
                } else if entry.interrupted {
                    Phase::Interrupted
                } else if source.is_some_and(|id| self.restored.contains(&id)) {
                    Phase::Restored
                } else if working {
                    Phase::Working
                } else {
                    Phase::Finished
                }
            });
        }
        if let Some(preview) = pending {
            if self
                .pending
                .as_ref()
                .is_none_or(|(id, _)| *id != preview.id())
            {
                let blocks = preview
                    .blocks()
                    .iter()
                    .filter(|b| b.visible(false))
                    .collect::<Vec<_>>();
                self.pending = Some((
                    preview.id(),
                    prompt(
                        format!("{epoch}-preparing-{}", preview.id()),
                        history.len(),
                        &blocks,
                        true,
                        preview.id(),
                    ),
                ));
            }
            if let Some((_, data)) = &self.pending {
                timeline.start_turn();
                timeline
                    .turn
                    .rows
                    .push(TimelineRow::new(data.clone(), Phase::Working));
            }
        } else {
            self.pending = None;
        }
        if let Some(stream) = stream {
            let entry = project_entry(epoch, history.len(), stream, 0, Some(call));
            timeline.push_entry(&entry, true, Some(&mut next_live), |_| Phase::Working);
        }
        timeline.flush_thoughts();
        if let Some(activity) = activity {
            // The active thought/step already signals work. Reserve the tip for
            // waiting, switching calls, reconnecting, or otherwise out-of-band work.
            let active = timeline.turn.rows.last().is_some_and(|row| {
                matches!(&row.data.kind,
                    BlockKind::ThoughtGroup(_) | BlockKind::Step(_) if row.data.default_expanded())
            });
            if !active || activity.starts_with("Stopping") || activity.starts_with("Reconnecting") {
                let mut data = BlockData::new(
                    format!("{epoch}-tip"),
                    history.len(),
                    BlockKind::Live,
                    activity,
                );
                data.revision = hash(activity);
                timeline
                    .turn
                    .rows
                    .push(TimelineRow::new(Arc::new(data), Phase::Live));
            }
        }
        self.checkpoint = next_checkpoint;
        let previous = start_row.checked_sub(1).and_then(|i| old_rows.get(i));
        let mut projection = timeline.finish(previous);
        projection.start_row = start_row;
        // Only rekey disappearing rows. Segments are bound in order, so the
        // first unambiguous match owns the transfer for the entire thought group.
        let keys: HashSet<_> = projection
            .rows
            .iter()
            .map(|row| row.data.key.as_str())
            .collect();
        let mut transferred = HashSet::new();
        for before in &self.live {
            let key = before.key.as_str();
            if keys.contains(key) || transferred.contains(key) {
                continue;
            }
            let compatible = |after: &&Binding| {
                can_commit(before.identity, after.identity, &before.text, &after.text)
            };
            let mut candidates = next_live.iter().chain(&committed).filter(compatible);
            if let Some(after) = candidates.next()
                && candidates.all(|other| other.key == after.key)
            {
                transferred.insert(key);
                projection
                    .transfers
                    .push((before.key.clone(), after.key.clone()));
            }
        }
        self.live = next_live;
        projection
    }
}

fn project_entry(
    epoch: &str,
    index: usize,
    entry: &HistoryEntry,
    stamp: u64,
    streaming: Option<Option<NativeHeader>>,
) -> Entry {
    let mut fragments = Vec::new();
    let mut header = None;
    let mut interrupted = false;
    let key = format!("{epoch}-entry-{index}");
    let structural = match entry {
        HistoryEntry::Conversation(entry) => {
            header = streaming.unwrap_or(entry.header);
            interrupted = streaming.is_none()
                && entry.blocks.iter().any(|block| {
                    zevria_presentation::incomplete_response_outcome(&block.kind).is_some()
                });
            let mut blocks = entry.blocks.iter().filter(|b| b.visible(false)).peekable();
            let mut ordinal = 0usize;
            while let Some(block) = blocks.next() {
                if is_prompt_content(block) {
                    let mut group = vec![block];
                    while blocks.peek().is_some_and(|next| {
                        is_prompt_content(next)
                            && (entry.header.is_some()
                                || (next.prompt.is_none()
                                    && next.prompt_group == block.prompt_group))
                    }) {
                        group.push(blocks.next().unwrap());
                    }
                    let revision =
                        hash(group.iter().map(|b| (b.id, b.revision)).collect::<Vec<_>>());
                    fragments.push(Fragment::Prompt(prompt(
                        format!("{epoch}-{}", block.id.0),
                        index,
                        &group,
                        false,
                        revision,
                    )));
                    continue;
                }
                let tail_kind = match block.kind {
                    PresentationBlockKind::Reasoning { .. } => Some(TailKind::Thinking),
                    PresentationBlockKind::Text { .. }
                        if block.role == Some(PresentationRole::Assistant) =>
                    {
                        Some(TailKind::Answer)
                    }
                    PresentationBlockKind::Tool(_)
                    | PresentationBlockKind::WebActivity(_)
                    | PresentationBlockKind::Subtask { .. } => None,
                    _ => Some(TailKind::Other),
                };
                let identity = tail_kind.map(|kind| {
                    let identity = TailIdentity {
                        header,
                        kind,
                        ordinal,
                    };
                    ordinal += 1;
                    identity
                });
                if let Some(identity) = identity.filter(|id| id.kind == TailKind::Thinking) {
                    let text: Arc<str> = block.primary_copy().into();
                    let segment = ThoughtSegment {
                        word_count: text.split_whitespace().count(),
                        headline: headline(&text),
                        stamp: hash((&text, block.revision)),
                        text,
                        phase: Phase::Finished,
                    };
                    fragments.push(Fragment::Reasoning {
                        segment,
                        identity,
                        history_index: index,
                        source: streaming.is_none().then_some(block.id),
                    });
                    continue;
                }
                let key = if streaming.is_some() {
                    identity.map_or_else(
                        || format!("{epoch}-stream-{header:?}-{}", block.id.0),
                        |id| id.key(epoch),
                    )
                } else {
                    format!("{epoch}-{}", block.id.0)
                };
                let mut data = project_block(key, index, block);
                if streaming.is_some() {
                    data.source = None;
                }
                fragments.push(Fragment::Row {
                    data: Arc::new(data),
                    identity,
                });
            }
            vec![]
        }
        HistoryEntry::PlanArtifact(plan) => vec![BlockData::new(
            key,
            index,
            BlockKind::Document {
                title: plan.title.clone(),
                version: format!("Revision {}", plan.version.revision),
                handoff: false,
            },
            plan.markdown.as_str(),
        )],
        HistoryEntry::PlanHandoff(handoff, _) => vec![BlockData::new(
            key,
            index,
            BlockKind::Document {
                title: handoff.artifact.title.clone(),
                version: "Approved".into(),
                handoff: true,
            },
            handoff.artifact.markdown.as_str(),
        )],
        HistoryEntry::JobNotice(notice, _) => {
            let (finished, running) = notice_rows(notice);
            let mut data = BlockData::new(
                key,
                index,
                BlockKind::Jobs { finished, running },
                "Background jobs finished",
            );
            data.primary = notice.text().into();
            vec![data]
        }
        HistoryEntry::Ensemble(ensemble) => {
            let prompt = BlockData::new(
                key.clone(),
                index,
                BlockKind::Prompt {
                    images: vec![],
                    preparing: false,
                },
                ensemble.command(),
            );
            std::iter::once(prompt)
                .chain(ensemble.workers.iter().enumerate().map(|(i, worker)| {
                    BlockData::new(
                        format!("{key}-worker-{i}"),
                        index,
                        BlockKind::Ensemble {
                            status: Some(StatusIcon::agent(worker.status)),
                        },
                        worker.copy_text(),
                    )
                }))
                .collect()
        }
        HistoryEntry::Error(error) => {
            let cancelled = error == zevria_presentation::TURN_CANCELLED;
            let mut data = BlockData::new(
                key,
                index,
                BlockKind::Notice(if cancelled {
                    NoticeLevel::Warning
                } else {
                    NoticeLevel::Error
                }),
                if cancelled { "Stopped" } else { error },
            );
            data.primary = error.as_str().into();
            vec![data]
        }
        HistoryEntry::CompactionDivider => vec![BlockData::new(key, index, BlockKind::Divider, "")],
    };
    fragments.extend(structural.into_iter().map(|mut data| {
        data.revision = stamp;
        Fragment::Row {
            data: Arc::new(data),
            identity: None,
        }
    }));
    Entry {
        stamp,
        fragments,
        header,
        starts_turn: entry.has_prompt_header(),
        interrupted,
        divider: matches!(entry, HistoryEntry::CompactionDivider),
    }
}

fn is_prompt_content(block: &PresentationBlock) -> bool {
    block.role == Some(PresentationRole::User)
        && matches!(
            block.kind,
            PresentationBlockKind::Text { .. }
                | PresentationBlockKind::Image { .. }
                | PresentationBlockKind::Placeholder(_)
        )
}

fn prompt(
    key: String,
    index: usize,
    blocks: &[&PresentationBlock],
    preparing: bool,
    stamp: u64,
) -> Arc<BlockData> {
    let mut images = Vec::new();
    let mut body = Vec::new();
    let mut copy = Vec::new();
    for block in blocks {
        let text = block.primary_copy();
        if let PresentationBlockKind::Image { image, .. } = &block.kind {
            images.push(Attachment {
                label: text.clone(),
                image: image.clone(),
            });
        } else {
            body.push(text.clone());
        }
        copy.push(text);
    }
    let mut data = BlockData::new(
        key,
        index,
        BlockKind::Prompt { images, preparing },
        body.join("\n\n"),
    );
    data.primary = copy.join("\n\n").into();
    data.revision = stamp;
    if !preparing {
        data.source = blocks.first().map(|block| (index, block.id));
    }
    Arc::new(data)
}

fn project_block(key: String, index: usize, block: &PresentationBlock) -> BlockData {
    let primary = block.primary_copy();
    let secondary = block.secondary_copy();
    let (kind, body) = match &block.kind {
        PresentationBlockKind::Text { flavor, .. } => (
            BlockKind::Prose {
                plain: *flavor == TextFlavor::Plain,
            },
            primary.as_str(),
        ),
        PresentationBlockKind::Tool(tool) => (
            BlockKind::Step(Step::Tool(Arc::new(tools::ToolPresentation::new(
                tool,
                secondary.as_deref(),
            )))),
            "",
        ),
        PresentationBlockKind::Subtask { descriptor, .. } => {
            (BlockKind::Step(Step::Subtask(descriptor.clone())), "")
        }
        PresentationBlockKind::WebActivity(activity) => (
            BlockKind::Step(Step::Web(web::WebRow::new(activity, primary.clone()))),
            "",
        ),
        PresentationBlockKind::Diagnostic(d) => (
            BlockKind::Notice(if d.tone == DiagnosticTone::Error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Warning
            }),
            &primary[..],
        ),
        PresentationBlockKind::Error(_) => (BlockKind::Notice(NoticeLevel::Error), &primary[..]),
        PresentationBlockKind::Plan(_) => (BlockKind::Ensemble { status: None }, &primary[..]),
        PresentationBlockKind::Image { image, .. } => (
            BlockKind::Image(Attachment {
                image: image.clone(),
                label: primary.clone(),
            }),
            "",
        ),
        PresentationBlockKind::Placeholder(_) => {
            match zevria_presentation::incomplete_response_outcome(&block.kind) {
                Some(zevria_content::ResponseAttemptOutcome::Interrupted) => (
                    BlockKind::Notice(NoticeLevel::Warning),
                    "Stopped before the response finished",
                ),
                Some(_) => (
                    BlockKind::Notice(NoticeLevel::Warning),
                    "The response failed before it finished",
                ),
                None => (BlockKind::Prose { plain: true }, &primary[..]),
            }
        }
        PresentationBlockKind::Reasoning { .. } => {
            unreachable!("reasoning projects as a turn fragment")
        }
    };
    // Hash payloads once per changed entry. List refreshes and disclosure
    // toggles never hash command output, markdown, or file changes again.
    let revision = hash((block.revision, body, &primary, &secondary));
    let mut data = BlockData::new(key, index, kind, body);
    data.revision = revision;
    data.primary = primary.into();
    data.secondary = secondary.map(Arc::from);
    data.source = Some((index, block.id));
    if let PresentationBlockKind::Text { sources, .. } = &block.kind {
        data.sources.clone_from(sources);
    }
    data
}

struct PendingThoughtGroup {
    key: String,
    history_index: usize,
    source: Option<Anchor>,
    segments: Vec<ThoughtSegment>,
}

#[derive(Default)]
struct Turn {
    rows: Vec<TimelineRow>,
    pending_thoughts: Option<PendingThoughtGroup>,
    navigable: bool,
}

struct Timeline<'a> {
    epoch: &'a str,
    turn: Turn,
    rows: Vec<TimelineRow>,
    anchors: HashMap<Anchor, String>,
}
impl<'a> Timeline<'a> {
    fn new(epoch: &'a str) -> Self {
        Self {
            epoch,
            turn: Turn::default(),
            rows: vec![],
            anchors: HashMap::new(),
        }
    }

    fn flush_thoughts(&mut self) {
        if let Some(group) = self.turn.pending_thoughts.take() {
            self.turn.rows.push(thought_row(
                group.key,
                group.history_index,
                group.source,
                group.segments,
            ));
        }
    }

    fn start_turn(&mut self) {
        self.finish_turn();
        self.turn.navigable = true;
    }

    fn push_entry(
        &mut self,
        entry: &Entry,
        live: bool,
        mut bindings: Option<&mut Vec<Binding>>,
        phase: impl Fn(Option<PresentationBlockId>) -> Phase,
    ) {
        if entry.divider {
            self.finish_turn();
        }
        if entry.starts_turn {
            self.start_turn();
        }
        let last = entry.fragments.len().saturating_sub(1);
        let mut new_prompt = !entry.starts_turn;
        for (index, fragment) in entry.fragments.iter().enumerate() {
            match fragment {
                Fragment::Prompt(data) => {
                    if new_prompt {
                        self.start_turn();
                    }
                    new_prompt = true;
                    self.push_row(data.clone(), phase(data.source.map(|(_, id)| id)));
                }
                Fragment::Reasoning {
                    segment,
                    identity,
                    history_index,
                    source,
                } => {
                    let mut segment = segment.clone();
                    segment.phase = if live && index == last {
                        Phase::Live
                    } else {
                        phase(*source)
                    };
                    let anchor = source.map(|id| (*history_index, id));
                    let group =
                        self.turn
                            .pending_thoughts
                            .get_or_insert_with(|| PendingThoughtGroup {
                                key: source.map_or_else(
                                    || identity.key(self.epoch),
                                    |id| format!("{}-{}", self.epoch, id.0),
                                ),
                                history_index: *history_index,
                                source: anchor,
                                segments: vec![],
                            });
                    if let Some(anchor) = anchor {
                        self.anchors.insert(anchor, group.key.clone());
                    }
                    if let Some(bindings) = bindings.as_deref_mut() {
                        bindings.push(Binding {
                            identity: *identity,
                            key: group.key.clone(),
                            text: segment.text.clone(),
                        });
                    }
                    group.segments.push(segment);
                }
                Fragment::Row { data, identity } => {
                    if let Some((bindings, identity)) =
                        bindings.as_deref_mut().zip(identity.as_ref())
                    {
                        bindings.push(Binding {
                            identity: *identity,
                            key: data.key.clone(),
                            text: data.body.clone(),
                        });
                    }
                    let phase = if live && index == last {
                        Phase::Live
                    } else {
                        phase(data.source.map(|(_, id)| id))
                    };
                    self.push_row(data.clone(), phase);
                }
            }
        }
        if entry.divider {
            self.start_turn();
        }
    }

    fn push_row(&mut self, data: Arc<BlockData>, phase: Phase) {
        self.flush_thoughts();
        if let BlockKind::Step(Step::Web(next)) = &data.kind
            && let Some(previous) = self.turn.rows.last_mut()
            && let BlockKind::Step(Step::Web(web)) = &mut Arc::make_mut(&mut previous.data).kind
        {
            web.merge(next);
            let previous = Arc::make_mut(&mut previous.data);
            previous.revision = hash((previous.revision, &data.key, data.revision));
            if let Some(source) = data.source {
                self.anchors.insert(source, previous.key.clone());
            }
            return;
        }
        if let Some(source) = data.source {
            self.anchors.insert(source, data.key.clone());
        }
        self.turn.rows.push(TimelineRow::new(data, phase));
    }

    fn finish_turn(&mut self) {
        self.flush_thoughts();
        if self.turn.rows.is_empty() {
            return;
        }
        let mut turn = std::mem::take(&mut self.turn);
        turn.rows[0].turn_start = turn.navigable;
        self.rows.append(&mut turn.rows);
    }

    fn finish(mut self, previous_row: Option<&TimelineRow>) -> Projection {
        self.finish_turn();
        let last = self.rows.len().saturating_sub(1);
        for i in 0..self.rows.len() {
            let row = &self.rows[i];
            let step = |row: &TimelineRow| matches!(row.data.kind, BlockKind::Step(_));
            let previous = i
                .checked_sub(1)
                .and_then(|p| self.rows.get(p))
                .or(previous_row);
            let position = step(row).then(|| {
                match (
                    !row.turn_start && previous.is_some_and(step),
                    self.rows
                        .get(i + 1)
                        .is_some_and(|next| !next.turn_start && step(next)),
                ) {
                    (false, false) => StepPosition::Only,
                    (false, true) => StepPosition::First,
                    (true, true) => StepPosition::Middle,
                    (true, false) => StepPosition::Last,
                }
            });
            let gap = if previous.is_none() {
                16
            } else if row.turn_start {
                24
            } else if position.is_some_and(|p| !p.starts()) {
                0
            } else if previous.is_some_and(step) || matches!(row.data.kind, BlockKind::Divider) {
                14
            } else {
                10
            };
            let row = &mut self.rows[i];
            row.position = position;
            row.gap = gap;
            row.last = i == last;
        }
        Projection {
            start_row: 0,
            rows: self.rows,
            transfers: vec![],
            anchors: self.anchors,
        }
    }
}

fn thought_row(
    key: String,
    history_index: usize,
    source: Option<Anchor>,
    segments: Vec<ThoughtSegment>,
) -> TimelineRow {
    let revision = hash(
        segments
            .iter()
            .map(|s| (s.stamp, s.phase))
            .collect::<Vec<_>>(),
    );
    let thought = ThoughtGroup::new(segments);
    let phase = thought.phase();
    let mut data = BlockData::new(key, history_index, BlockKind::ThoughtGroup(thought), "");
    data.revision = revision;
    data.source = source;
    TimelineRow::new(Arc::new(data), phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::{
        AssistantContent, AssistantMessage, CallId, Message, Reasoning, ToolCall, ToolFunction,
        ToolName,
    };
    use serde_json::json;
    use zevria_content::{
        AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
        AssistantSourceAddress, HostedActivity, HostedActivityKind, HostedActivityStatus,
        ResponseAttempt, ResponseAttemptOutcome,
    };
    use zevria_foundation::{ModelProfileRef, TurnId};
    use zevria_transcript::transcript::TranscriptItem;
    use zevria_workflow::{PlanArtifact, PlanId, PlanVersion};

    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry::from_message(message, ToolCallStatus::Finished).unwrap()
    }
    fn reasoning(text: &str) -> AssistantContent {
        AssistantContent::Reasoning(Reasoning::new(text))
    }
    fn assistant(content: impl IntoIterator<Item = AssistantContent>) -> Message {
        Message::Assistant(AssistantMessage::new(content.into_iter().collect()))
    }
    fn thought(text: &str) -> Message {
        assistant([reasoning(text)])
    }
    fn command(id: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire(id),
            ToolFunction::new(
                ToolName::new("command").unwrap(),
                json!({"command":"cargo fmt"}),
            ),
        ))
    }
    fn tool(id: &str) -> Message {
        assistant([command(id)])
    }
    fn call(call: usize) -> NativeHeader {
        NativeHeader::Assistant {
            turn: DisplayTurn(1),
            call,
        }
    }
    fn projected(
        p: &mut Projector,
        conv: &ConversationState,
        stream: Option<(NativeHeader, &HistoryEntry)>,
    ) -> Projection {
        p.project(
            conv,
            "epoch",
            stream.map(|(h, _)| h),
            stream.is_some(),
            stream.map(|(_, e)| e),
            None,
            &HashSet::new(),
            None,
            None,
        )
    }
    fn presented(
        p: &mut Projector,
        conv: &ConversationState,
        stream: Option<(NativeHeader, &HistoryEntry)>,
        disclosures: &mut DisclosureState,
    ) -> Vec<TimelineRow> {
        let Projection {
            mut rows,
            transfers,
            ..
        } = projected(p, conv, stream);
        for (old, new) in transfers {
            disclosures.rekey(&old, &new);
        }
        for row in &mut rows {
            row.apply_disclosure(disclosures);
        }
        rows
    }
    fn assert_thought(row: &TimelineRow, texts: &[&str], words: usize, phase: Phase) {
        let thought = row.data.thought().expect("thought group");
        assert_eq!(thought.segments.len(), texts.len());
        assert_eq!(thought.word_count, words);
        assert_eq!(row.data.primary_copy(), texts.join("\n\n"));
        assert_eq!(row.phase, phase);
        assert_eq!(thought.is_live, phase == Phase::Live);
    }

    #[test]
    fn thoughts_separated_by_tools_keep_order_across_calls_and_at_restore() {
        let mut conv = ConversationState::default();
        conv.push_user_turn(Message::user("Check the changes"), DisplayTurn(1));
        let mut projector = Projector::default();
        let mut disclosures = DisclosureState::default();
        let first_text = "  First pass ";
        let first = presented(
            &mut projector,
            &conv,
            Some((call(1), &entry(thought(first_text)))),
            &mut disclosures,
        );
        assert!(first[1].expanded);
        let first_live_key = first[1].data.key.clone();
        disclosures.set(first_live_key.clone(), true);
        conv.commit_response(
            thought(first_text),
            None,
            ToolCallStatus::Finished,
            Some(call(1)),
        );
        conv.commit_response(tool("one"), None, ToolCallStatus::Finished, Some(call(1)));
        let rows = presented(
            &mut projector,
            &conv,
            Some((call(2), &entry(thought("Second pass\n\nDetails")))),
            &mut disclosures,
        );
        assert_ne!(rows[1].data.key, first_live_key);
        assert!(
            rows[1].expanded,
            "the first pin survives only on its own group"
        );
        assert_thought(&rows[1], &[first_text], 2, Phase::Working);
        assert!(matches!(rows[2].data.kind, BlockKind::Step(Step::Tool(_))));
        let second_live_key = rows[3].data.key.clone();
        assert_eq!(rows[3].phase, Phase::Live);
        assert!(rows[3].expanded);
        disclosures.set(second_live_key.clone(), false);
        let second_text = "Second pass\n\nDetails and notes";
        let grown = presented(
            &mut projector,
            &conv,
            Some((call(2), &entry(thought(second_text)))),
            &mut disclosures,
        );
        assert_eq!(grown[3].data.key, second_live_key);
        assert!(!grown[3].expanded);
        conv.complete_response(thought(second_text), None, Some(call(2)));
        let completed = presented(&mut projector, &conv, None, &mut disclosures);
        assert_eq!(completed[1].data.key, rows[1].data.key);
        assert_ne!(completed[3].data.key, second_live_key);
        assert!(completed[1].expanded);
        assert!(!disclosures.is_expanded(&completed[3].data.key, true));

        let restored = presented(
            &mut Projector::default(),
            &conv,
            None,
            &mut DisclosureState::default(),
        );
        for (rows, phase) in [(&completed, Phase::Finished), (&restored, Phase::Restored)] {
            assert_eq!(rows.len(), 4);
            assert!(matches!(rows[0].data.kind, BlockKind::Prompt { .. }));
            assert_thought(&rows[1], &[first_text], 2, phase);
            assert!(matches!(rows[2].data.kind, BlockKind::Step(Step::Tool(_))));
            assert_thought(&rows[3], &[second_text], 5, phase);
            assert!(!rows[3].expanded);
            assert_eq!(
                &*rows[3].data.thought().unwrap().segments[0].headline,
                "Second pass"
            );
        }
        assert!(!restored[1].expanded);
    }

    #[test]
    fn thoughts_aggregate_in_contiguous_runs_around_commands() {
        let before = ["Inspect inputs", "Read build settings", "Check tests"];
        let after = [
            "Review command output",
            "Compare both paths",
            "Keep existing behavior",
            "Check the edge cases",
            "Summarize findings",
        ];
        let prompt = Message::user("Check the workspace");
        let mut conv = ConversationState::default();
        conv.push_user_turn(prompt.clone(), DisplayTurn(1));
        let mut projector = Projector::default();
        let first = projected(
            &mut projector,
            &conv,
            Some((call(1), &entry(assistant(before.map(reasoning))))),
        );
        assert_eq!(first.rows.len(), 2);
        assert_thought(&first.rows[1], &before, 7, Phase::Live);
        assert_eq!(first.rows[1].data.history_index, 1);
        assert_eq!(first.rows[1].data.source, None);
        assert_eq!(
            first.rows[1].data.key,
            TailIdentity {
                header: Some(call(1)),
                kind: TailKind::Thinking,
                ordinal: 0,
            }
            .key("epoch")
        );

        let first_call = assistant(
            before
                .map(reasoning)
                .into_iter()
                .chain(["one", "two", "three", "four"].map(command)),
        );
        conv.commit_response(
            first_call.clone(),
            None,
            ToolCallStatus::Finished,
            Some(call(1)),
        );
        let second = projected(
            &mut projector,
            &conv,
            Some((call(2), &entry(assistant(after.map(reasoning))))),
        );
        assert_eq!(second.rows.len(), 7);
        assert_thought(&second.rows[1], &before, 7, Phase::Working);
        assert_thought(&second.rows[6], &after, 15, Phase::Live);
        assert_eq!(
            second.transfers,
            [(
                first.rows[1].data.key.clone(),
                second.rows[1].data.key.clone()
            )]
        );
        let first_anchor = second.rows[1].data.source.unwrap();
        let HistoryEntry::Conversation(first_entry) = conv.entry(1).unwrap() else {
            panic!("first call should be in history")
        };
        assert_eq!(first_anchor, (1, first_entry.blocks[0].id));
        assert_eq!(
            second.rows[1].data.key,
            format!("epoch-{}", first_anchor.1.0)
        );
        for block in &first_entry.blocks[..before.len()] {
            assert_eq!(second.anchors[&(1, block.id)], second.rows[1].data.key);
        }

        let answer = "Workspace checked.";
        let second_call = assistant(
            after
                .map(reasoning)
                .into_iter()
                .chain([AssistantContent::text(answer)]),
        );
        conv.complete_response(second_call.clone(), None, Some(call(2)));
        let completed = projected(&mut projector, &conv, None);
        assert_eq!(
            completed.transfers,
            [(
                second.rows[6].data.key.clone(),
                completed.rows[6].data.key.clone()
            )]
        );
        assert_eq!(completed.rows[1].data.key, second.rows[1].data.key);
        assert_eq!(completed.rows[6].data.history_index, 2);
        assert_eq!(completed.rows[6].data.source.unwrap().0, 2);

        let mut restored_conv = ConversationState::default();
        restored_conv.restore(vec![
            TranscriptItem::Message(prompt),
            TranscriptItem::Message(first_call),
            TranscriptItem::Message(second_call),
        ]);
        let restored = projected(&mut Projector::default(), &restored_conv, None);
        use StepPosition::*;
        for (projection, phase) in [(&completed, Phase::Finished), (&restored, Phase::Restored)] {
            let [prompt, first, a, b, c, d, later, prose] = projection.rows.as_slice() else {
                panic!("expected prompt, three thoughts, four commands, five thoughts, and prose")
            };
            assert!(matches!(prompt.data.kind, BlockKind::Prompt { .. }));
            assert_thought(first, &before, 7, phase);
            for (row, position) in [(a, First), (b, Middle), (c, Middle), (d, Last)] {
                assert!(matches!(row.data.kind, BlockKind::Step(Step::Tool(_))));
                assert_eq!(row.position, Some(position));
            }
            assert_thought(later, &after, 15, phase);
            assert!(matches!(prose.data.kind, BlockKind::Prose { .. }));
            assert_eq!(prose.data.primary_copy(), answer);
            assert_eq!(
                projection.rows.iter().map(|r| r.gap).collect::<Vec<_>>(),
                [16, 10, 10, 0, 0, 0, 14, 10]
            );
        }
    }

    #[test]
    fn consecutive_thoughts_across_calls_aggregate_without_intervening_rows() {
        let mut conv = ConversationState::default();
        conv.push_user_turn(Message::user("Review the plan"), DisplayTurn(1));
        conv.commit_response(
            thought("First pass"),
            None,
            ToolCallStatus::Finished,
            Some(call(1)),
        );
        let HistoryEntry::Conversation(mut hidden) = entry(Message::assistant("Not displayed"))
        else {
            unreachable!()
        };
        hidden.blocks[0].visibility = BlockVisibility::Diagnostics;
        conv.push_entry(HistoryEntry::Conversation(hidden));
        let mut projector = Projector::default();
        let streamed = projected(
            &mut projector,
            &conv,
            Some((call(2), &entry(thought("Second pass")))),
        );
        assert_eq!(streamed.rows.len(), 2);
        assert_thought(
            &streamed.rows[1],
            &["First pass", "Second pass"],
            4,
            Phase::Live,
        );
        assert_eq!(streamed.rows[1].data.history_index, 1);
        conv.commit_response(
            thought("Second pass"),
            None,
            ToolCallStatus::Finished,
            Some(call(2)),
        );
        conv.push_entry(entry(Message::assistant("Here's the initial analysis.")));
        conv.push_entry(entry(thought("After prose")));
        conv.push_plan_artifact(PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "## Next steps".into(),
            source_turn_id: TurnId::new(1),
        });
        conv.push_entry(entry(thought("After plan")));
        conv.push_error("An error occurred".into());
        conv.push_entry(entry(thought("After notice")));
        let committed = projected(&mut projector, &conv, None);
        assert!(committed.transfers.is_empty());
        let [_, first, prose, second, plan, third, notice, fourth] = committed.rows.as_slice()
        else {
            panic!("expected a separate run after each visible separator")
        };
        assert_eq!(first.data.key, streamed.rows[1].data.key);
        assert_thought(first, &["First pass", "Second pass"], 4, Phase::Finished);
        assert!(matches!(prose.data.kind, BlockKind::Prose { .. }));
        assert_thought(second, &["After prose"], 2, Phase::Finished);
        assert!(matches!(plan.data.kind, BlockKind::Document { .. }));
        assert_thought(third, &["After plan"], 2, Phase::Finished);
        assert!(matches!(
            notice.data.kind,
            BlockKind::Notice(NoticeLevel::Error)
        ));
        assert_thought(fourth, &["After notice"], 2, Phase::Finished);
    }

    #[test]
    fn out_of_order_attempt_thought_preserves_later_group_key_and_override() {
        use AssistantPartIdentity::{Content, Summary};
        use AssistantPresentationContent as Part;
        let part = |output_index, part, content| AssistantPresentationPart {
            source: AssistantSourceAddress {
                output_index,
                part,
                item_id: None,
            },
            content,
        };
        let mut conv = ConversationState::default();
        conv.push_user_turn(Message::user("Check the result"), DisplayTurn(1));
        let mut attempt = ResponseAttempt::new(ModelProfileRef::new("fixture", "model"));
        attempt.presentation = vec![
            part(1, Content(0), Part::answer("Preliminary result")),
            part(
                2,
                Summary(0),
                Part::Reasoning {
                    text: "Double check".into(),
                },
            ),
        ];
        conv.update_native_attempt(attempt.clone(), Some(call(1)));
        let mut projector = Projector::default();
        let mut disclosures = DisclosureState::default();
        let initial = presented(&mut projector, &conv, None, &mut disclosures);
        assert_eq!(initial.len(), 3);
        let later = &initial[2];
        assert_eq!(later.data.primary_copy(), "Double check");
        disclosures.set(later.data.key.clone(), true);

        attempt.presentation.push(part(
            0,
            Summary(0),
            Part::Reasoning {
                text: "Check inputs".into(),
            },
        ));
        attempt.touch();
        conv.update_native_attempt(attempt, Some(call(1)));
        let updated = presented(&mut projector, &conv, None, &mut disclosures);
        let [_, earlier, prose, retained] = updated.as_slice() else {
            panic!("expected the inserted thought before the prose")
        };
        assert_eq!(earlier.data.primary_copy(), "Check inputs");
        assert!(!earlier.expanded);
        assert!(matches!(prose.data.kind, BlockKind::Prose { .. }));
        assert_eq!(retained.data.key, later.data.key);
        assert_eq!(retained.data.source, later.data.source);
        assert_eq!(retained.data.primary_copy(), "Double check");
        assert!(retained.expanded);
    }

    #[test]
    fn step_runs_coalesce_web_and_stitch_but_stop_at_prose_or_compaction() {
        let mut conv = ConversationState::default();
        conv.push_user_turn(Message::user("Inspect"), DisplayTurn(1));
        conv.push_entry(entry(tool("first")));
        let mut attempt = ResponseAttempt::new(ModelProfileRef::new("fixture", "model"));
        attempt
            .hosted_activity
            .extend((0..2).map(|i| HostedActivity {
                kind: HostedActivityKind::WebSearch,
                output_index: i,
                item_id: Some(format!("web_{i}")),
                status: HostedActivityStatus::Completed,
                action: Some(json!({"type":"search", "query":format!("docs {i}")})),
            }));
        attempt.close(ResponseAttemptOutcome::Completed);
        conv.update_native_attempt(attempt, Some(call(1)));
        conv.push_entry(entry(tool("second")));
        conv.push_entry(entry(Message::assistant("Let's check the result.")));
        conv.push_entry(entry(tool("third")));
        conv.push_compaction_divider();
        conv.push_entry(entry(tool("fourth")));
        let rows = presented(
            &mut Projector::default(),
            &conv,
            None,
            &mut DisclosureState::default(),
        );
        use StepPosition::*;
        assert_eq!(
            rows.iter().filter_map(|r| r.position).collect::<Vec<_>>(),
            [First, Middle, Last, Only, Only]
        );
        let BlockKind::Step(Step::Web(web)) = &rows[2].data.kind else {
            panic!("web step in the run")
        };
        assert_eq!(web.actions.len(), 2);
        assert_eq!(rows[2].gap, 0);
        assert_eq!(rows[3].gap, 0);
        assert!(rows.last().unwrap().turn_start);
    }

    #[test]
    fn mixed_acp_entry_has_distinct_prompt_stops_and_plan_stays_in_order() {
        let mut conv = ConversationState::default();
        let mut blocks = vec![];
        for message in [
            Message::user("First ACP prompt"),
            Message::assistant("First answer"),
            Message::user("Second ACP prompt"),
            thought("Second turn only"),
        ] {
            let HistoryEntry::Conversation(entry) = entry(message) else {
                unreachable!()
            };
            blocks.extend(entry.blocks);
        }
        conv.push_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks,
        }));
        conv.push_plan_artifact(PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "## Keep this artifact".into(),
            source_turn_id: TurnId::new(2),
        });
        conv.push_entry(entry(Message::assistant("Plan ready")));
        let rows = presented(
            &mut Projector::default(),
            &conv,
            None,
            &mut DisclosureState::default(),
        );
        assert_eq!(crate::turns::turn_rows(&rows), [0, 2]);
        assert_eq!(rows[2].data.primary_copy(), "Second ACP prompt");
        assert!(matches!(rows[3].data.kind, BlockKind::ThoughtGroup(_)));
        assert!(matches!(
            rows[4].data.kind,
            BlockKind::Document { handoff: false, .. }
        ));
        assert_eq!(&*rows[5].data.body, "Plan ready");
    }

    #[test]
    fn prompt_groups_text_and_attachments_without_extra_turns() {
        let mut conv = ConversationState::default();
        let image = PromptImage::from_rgba(1, 1, &[0, 0, 0, 255]).unwrap();
        let prompt = zevria_content::UserPrompt::new(vec![
            zevria_content::PromptBlock::Text("Inspect the pixel".into()),
            zevria_content::PromptBlock::Image(image),
        ])
        .unwrap();
        conv.push_user_turn(prompt.to_message(), DisplayTurn(1));
        let rows = presented(
            &mut Projector::default(),
            &conv,
            None,
            &mut DisclosureState::default(),
        );
        assert_eq!(rows.len(), 1);
        let BlockKind::Prompt {
            images,
            preparing: false,
        } = &rows[0].data.kind
        else {
            unreachable!()
        };
        assert_eq!(images.len(), 1);
        assert_eq!(&*rows[0].data.body, "Inspect the pixel");
    }
}
