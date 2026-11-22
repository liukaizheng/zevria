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
    LiveThought(ThoughtGroup),
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
            BlockKind::ThoughtGroup(thought) | BlockKind::LiveThought(thought) => Some(thought),
            _ => None,
        }
    }
    pub fn primary_copy(&self) -> String {
        match &self.kind {
            BlockKind::ThoughtGroup(thought) | BlockKind::LiveThought(thought) => thought.text(),
            BlockKind::Step(Step::Web(web)) => web.copy_text(),
            _ => self.primary.to_string(),
        }
    }
    pub fn is_disclosure(&self) -> bool {
        self.thought().is_some() || matches!(self.kind, BlockKind::Step(_))
    }
    pub fn default_expanded(&self) -> bool {
        match &self.kind {
            BlockKind::ThoughtGroup(thought) | BlockKind::LiveThought(thought) => thought.is_live,
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
    pub fn apply_disclosure(&mut self, disclosures: &DisclosureState) {
        self.expanded = if self.data.is_disclosure() {
            disclosures.is_expanded(&self.data.key, self.data.default_expanded())
        } else {
            true
        };
        self.fingerprint = hash((
            &self.data.key,
            self.data.revision,
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

#[derive(Default)]
pub struct Projector {
    entries: Vec<Entry>,
    restored: HashSet<PresentationBlockId>,
    live: Vec<Binding>,
    pending: Option<(u64, Arc<BlockData>)>,
}

pub struct Projection {
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
    ) -> Projection {
        let streaming = match model.tail(Instant::now()) {
            ConversationTail::Streaming { message, .. } => {
                HistoryEntry::from_message(message.clone(), ToolCallStatus::Executing)
            }
            _ => None,
        };
        self.project(
            &model.conversation,
            epoch,
            model.session.call_header(),
            model.session.has_turn_work(),
            streaming.as_ref(),
            model.pending_prompt(),
            unsaved,
            activity,
            live_attempt,
        )
    }

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
        let mut next_live = Vec::new();
        let mut committed = Vec::new();
        let previous_call = self.live.first().map(|binding| binding.identity.header);
        let live_entry = live_attempt.filter(|_| stream.is_none()).and_then(|header| {
            history.iter().rposition(|entry| matches!(entry, HistoryEntry::Conversation(entry) if entry.header == Some(header)))
        });
        for (index, entry) in history.iter().enumerate() {
            let stamp = rows::fingerprint(entry);
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
        if let Some(activity) = activity {
            // The active thought/step already signals work. Reserve the tip for
            // waiting, switching calls, reconnecting, or otherwise out-of-band work.
            let active = timeline
                .turn
                .thoughts
                .last()
                .is_some_and(|thought| thought.phase == Phase::Live)
                || timeline.turn.rows.last().is_some_and(|row| {
                    matches!(&row.data.kind,
                    BlockKind::LiveThought(_) | BlockKind::Step(_) if row.data.default_expanded())
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
        let mut projection = timeline.finish();
        // Only rekey a disappearing row. One reasoning group can already own
        // several bindings; a specific live-tail row owns exactly one.
        let keys: HashSet<_> = projection
            .rows
            .iter()
            .map(|row| row.data.key.as_str())
            .collect();
        for before in &self.live {
            if keys.contains(before.key.as_str()) {
                continue;
            }
            let compatible = |after: &&Binding| {
                can_commit(before.identity, after.identity, &before.text, &after.text)
            };
            let mut candidates = next_live.iter().chain(&committed).filter(compatible);
            if let Some(after) = candidates.next()
                && candidates.all(|other| other.key == after.key)
            {
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

#[derive(Default)]
struct Turn {
    rows: Vec<TimelineRow>,
    thought_at: Option<usize>,
    thoughts: Vec<ThoughtSegment>,
    navigable: bool,
    has_steps_or_prose: bool,
}

struct Timeline<'a> {
    epoch: &'a str,
    ordinal: usize,
    turn: Turn,
    rows: Vec<TimelineRow>,
    anchors: HashMap<Anchor, String>,
}
impl<'a> Timeline<'a> {
    fn new(epoch: &'a str) -> Self {
        Self {
            epoch,
            ordinal: 0,
            turn: Turn::default(),
            rows: vec![],
            anchors: HashMap::new(),
        }
    }

    fn thought_key(&self) -> String {
        format!("{}-turn-{}-thoughts", self.epoch, self.ordinal)
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
                    source,
                } => {
                    let mut segment = segment.clone();
                    segment.phase = if live && index == last {
                        Phase::Live
                    } else {
                        phase(*source)
                    };
                    let separate = segment.phase == Phase::Live && self.turn.has_steps_or_prose;
                    let key = if separate {
                        identity.key(self.epoch)
                    } else {
                        self.thought_key()
                    };
                    if let Some(bindings) = bindings.as_deref_mut() {
                        bindings.push(Binding {
                            identity: *identity,
                            key: key.clone(),
                            text: segment.text.clone(),
                        });
                    }
                    if separate {
                        self.turn.rows.push(thought_row(key, vec![segment], true));
                    } else {
                        self.turn.thought_at.get_or_insert(self.turn.rows.len());
                        self.turn.thoughts.push(segment);
                    }
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
        if matches!(data.kind, BlockKind::Prose { .. } | BlockKind::Step(_)) {
            self.turn.thought_at.get_or_insert(self.turn.rows.len());
            self.turn.has_steps_or_prose = true;
        }
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
        if self.turn.rows.is_empty() && self.turn.thoughts.is_empty() {
            return;
        }
        let key = self.thought_key();
        let turn = std::mem::take(&mut self.turn);
        let mut rows = turn.rows;
        if !turn.thoughts.is_empty() {
            rows.insert(
                turn.thought_at.unwrap_or(0),
                thought_row(key, turn.thoughts, false),
            );
        }
        rows[0].turn_start = turn.navigable;
        self.rows.append(&mut rows);
        self.ordinal += 1;
    }

    fn finish(mut self) -> Projection {
        self.finish_turn();
        let last = self.rows.len().saturating_sub(1);
        for i in 0..self.rows.len() {
            let row = &self.rows[i];
            let step = |row: &TimelineRow| matches!(row.data.kind, BlockKind::Step(_));
            let previous = i.checked_sub(1).and_then(|p| self.rows.get(p));
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
            let gap = if i == 0 {
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
            rows: self.rows,
            transfers: vec![],
            anchors: self.anchors,
        }
    }
}

fn thought_row(key: String, segments: Vec<ThoughtSegment>, tail: bool) -> TimelineRow {
    let revision = hash(
        segments
            .iter()
            .map(|s| (s.stamp, s.phase))
            .collect::<Vec<_>>(),
    );
    let thought = ThoughtGroup::new(segments);
    let phase = thought.phase();
    let mut data = BlockData::new(
        key,
        0,
        if tail {
            BlockKind::LiveThought(thought)
        } else {
            BlockKind::ThoughtGroup(thought)
        },
        "",
    );
    data.revision = revision;
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
        HostedActivity, HostedActivityKind, HostedActivityStatus, ResponseAttempt,
        ResponseAttemptOutcome,
    };
    use zevria_foundation::{ModelProfileRef, TurnId};
    use zevria_workflow::{PlanArtifact, PlanId, PlanVersion};

    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry::from_message(message, ToolCallStatus::Finished).unwrap()
    }
    fn thought(text: &str) -> Message {
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
            Reasoning::new(text),
        )]))
    }
    fn tool(id: &str) -> Message {
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
            ToolCall::new(
                CallId::from_wire(id),
                ToolFunction::new(
                    ToolName::new("command").unwrap(),
                    json!({"command":"cargo fmt"}),
                ),
            ),
        )]))
    }
    fn call(call: usize) -> NativeHeader {
        NativeHeader::Assistant {
            turn: DisplayTurn(1),
            call,
        }
    }
    fn presented(
        p: &mut Projector,
        conv: &ConversationState,
        stream: Option<(NativeHeader, &HistoryEntry)>,
        disclosures: &mut DisclosureState,
    ) -> Vec<TimelineRow> {
        let mut view = p.project(
            conv,
            "epoch",
            stream.map(|(h, _)| h),
            stream.is_some(),
            stream.map(|(_, e)| e),
            None,
            &HashSet::new(),
            None,
            None,
        );
        for (old, new) in view.transfers {
            disclosures.rekey(&old, &new);
        }
        for row in &mut view.rows {
            row.apply_disclosure(disclosures);
        }
        view.rows
    }

    #[test]
    fn thoughts_aggregate_across_calls_and_at_restore() {
        let mut conv = ConversationState::default();
        conv.push_user_turn(Message::user("Check the changes"), DisplayTurn(1));
        let mut projector = Projector::default();
        let mut disclosures = DisclosureState::default();
        let streamed = entry(thought("  First pass "));
        let rows = presented(
            &mut projector,
            &conv,
            Some((call(1), &streamed)),
            &mut disclosures,
        );
        assert_eq!(rows[1].data.key, "epoch-turn-0-thoughts");
        assert!(rows[1].expanded);
        let anchor = rows[1].data.key.clone();
        conv.commit_response(
            thought("  First pass "),
            None,
            ToolCallStatus::Finished,
            Some(call(1)),
        );
        conv.commit_response(tool("one"), None, ToolCallStatus::Finished, Some(call(1)));
        let second = entry(thought("Second pass\n\nDetails"));
        let rows = presented(
            &mut projector,
            &conv,
            Some((call(2), &second)),
            &mut disclosures,
        );
        assert_eq!(rows[1].data.key, anchor);
        assert!(!rows[1].expanded);
        let tail = rows.last().unwrap();
        assert!(matches!(tail.data.kind, BlockKind::LiveThought(_)));
        assert!(tail.expanded);
        disclosures.set(tail.data.key.clone(), false);
        let grown = entry(thought("Second pass\n\nDetails and notes"));
        let rows = presented(
            &mut projector,
            &conv,
            Some((call(2), &grown)),
            &mut disclosures,
        );
        assert!(!rows.last().unwrap().expanded);
        conv.complete_response(
            thought("Second pass\n\nDetails and notes"),
            None,
            Some(call(2)),
        );
        let completed = presented(&mut projector, &conv, None, &mut disclosures);
        let restored = presented(
            &mut Projector::default(),
            &conv,
            None,
            &mut DisclosureState::default(),
        );
        for rows in [&completed, &restored] {
            assert_eq!(
                rows.iter().filter(|r| r.data.thought().is_some()).count(),
                1
            );
            assert!(!rows[1].expanded);
            let thought = rows[1].data.thought().unwrap();
            assert_eq!(thought.segments.len(), 2);
            assert_eq!(thought.word_count, 7);
            assert_eq!(&*thought.segments[1].headline, "Second pass");
            assert_eq!(
                rows[1].data.primary_copy(),
                "  First pass \n\nSecond pass\n\nDetails and notes"
            );
        }
        assert_eq!(restored[1].phase, Phase::Restored);
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
