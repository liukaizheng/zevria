//! Committed conversation projection, correlation indexes, and selection.
mod attempts;
mod ensemble;
mod headers;
mod restore;
mod selection;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    sync::OnceLock,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionScope {
    Message,
    Block,
}

use rig_core::message::{AssistantContent, Message, ToolCall, ToolResult, UserContent};
use zevria_foundation::ToolResultMetadata;
use zevria_foundation::subtask::SubtaskDescriptor;
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::subtask::SubtaskStatus;
use zevria_session_api::TranscriptEditTarget;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentRunId;
use zevria_workflow::AgentRunStatus;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanHandoff;
use zevria_workflow::PlanRecord;

use crate::presentation::{
    BlockVisibility, ConversationEntry, DisplayTurn, NativeHeader, PresentationBlock,
    PresentationBlockId, PresentationBlockKind, PresentationRole, PresentedTool, TextFlavor,
    provider_call_id, readable_reasoning_parts,
};

pub use crate::presentation::{
    NativeToolCallStatus as ToolCallStatus, NativeToolState as ToolCallState,
};

fn call_ids_match(left: Option<&str>, right: Option<&str>, exact_call_id: bool) -> bool {
    let exact = left == right;
    if exact_call_id {
        exact
    } else {
        exact || left.is_none() || right.is_none()
    }
}

/// Stable coordinates of one native tool call. Tail replacement clears the
/// complete index before any new entries are appended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolCallLocation {
    history_index: usize,
    block_id: PresentationBlockId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SubtaskLocation {
    parent: ToolCallLocation,
    entry_index: usize,
    row: PresentationBlockId,
}

/// One coherent worker row in an ensemble entry.
#[derive(Debug, Clone)]
pub struct EnsembleWorkerHistory {
    pub descriptor: AgentRunDescriptor,
    pub status: AgentRunStatus,
    pub failure: Option<String>,
    /// Lower-authority summaries/outcomes cannot overwrite a review snapshot.
    review_authoritative: bool,
}

impl EnsembleWorkerHistory {
    pub fn copy_text(&self) -> String {
        let mut text = format!(
            "{} ({}) · safe mode {} · {}",
            self.descriptor.label, self.descriptor.agent, self.descriptor.safe_mode, self.status
        );
        if let Some(failure) = &self.failure {
            text.push_str(&format!("\n{failure}"));
        }
        text
    }
}

/// Root-pane representation of one ensemble command and its ordered worker
/// rows. Worker descriptor, status, and failure cannot drift by index.
#[derive(Debug, Clone)]
pub struct EnsembleHistory {
    pub header: Option<NativeHeader>,
    pub run_id: EnsembleRunId,
    pub workflow: EnsembleWorkflow,
    pub prompt: zevria_content::UserPrompt,
    pub workers: Vec<EnsembleWorkerHistory>,
    pub baseline: Option<AgentRunId>,
}

impl EnsembleHistory {
    fn new(start: EnsembleStart, status: AgentRunStatus, header: Option<NativeHeader>) -> Self {
        Self {
            header,
            run_id: start.run_id,
            workflow: start.workflow,
            prompt: start.prompt,
            baseline: None,
            workers: start
                .agents
                .into_iter()
                .map(|descriptor| EnsembleWorkerHistory {
                    descriptor,
                    status,
                    failure: None,
                    review_authoritative: false,
                })
                .collect(),
        }
    }

    pub fn command(&self) -> String {
        format!(
            "{} {}",
            self.workflow.slash_command(),
            self.prompt.display_projection()
        )
    }

    fn update_status(
        &mut self,
        id: &AgentRunId,
        status: AgentRunStatus,
        failure: Option<String>,
    ) -> bool {
        let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| &worker.descriptor.id == id)
        else {
            return false;
        };
        if worker.review_authoritative || worker.status == AgentRunStatus::Abandoned {
            return false;
        }
        let changed = worker.status != status || (failure.is_some() && worker.failure != failure);
        worker.status = status;
        if failure.is_some() {
            worker.failure = failure;
        }
        changed
    }
}

/// A rendered item in the committed conversation pane.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum HistoryEntry {
    Conversation(ConversationEntry),
    PlanArtifact(PlanArtifact),
    PlanHandoff(PlanHandoff, Option<NativeHeader>),
    JobNotice(zevria_foundation::CommandJobNotice, Option<NativeHeader>),
    Ensemble(EnsembleHistory),
    CompactionDivider,
    Error(String),
}

impl HistoryEntry {
    /// Adapt one native message into the semantic blocks shared with ACP.
    pub fn from_message(message: Message, status: ToolCallStatus) -> Option<Self> {
        let mut blocks = Vec::new();
        match message {
            Message::User { content } => {
                let mut ordinal = 0;
                for (index, item) in content.into_iter().enumerate() {
                    let kind = match item {
                        UserContent::Text(text) => PresentationBlockKind::Text {
                            text: text.text,
                            flavor: TextFlavor::Plain,
                            editable: true,
                            sources: Vec::new(),
                        },
                        UserContent::ToolResult(_) => continue,
                        UserContent::Image(_) => {
                            ordinal += 1;
                            match zevria_content::PromptImage::from_user_content(&item) {
                                Ok(image) => PresentationBlockKind::Image {
                                    image,
                                    ordinal,
                                    editable: true,
                                },
                                Err(_) => PresentationBlockKind::Placeholder(
                                    "[unsupported image; not editable]".into(),
                                ),
                            }
                        }
                        UserContent::Audio(_) => {
                            PresentationBlockKind::Placeholder("[audio]".to_string())
                        }
                        UserContent::Video(_) => {
                            PresentationBlockKind::Placeholder("[video]".to_string())
                        }
                        UserContent::Document(_) => {
                            PresentationBlockKind::Placeholder("[document]".to_string())
                        }
                    };
                    blocks.push(PresentationBlock {
                        id: PresentationBlockId(index as u64),
                        revision: 0,
                        role: Some(PresentationRole::User),
                        prompt_group: None,
                        prompt: None,
                        visibility: BlockVisibility::Always,
                        kind,
                    });
                }
            }
            Message::Assistant(assistant) => {
                for (index, item) in assistant.content.into_iter().enumerate() {
                    let kind = match item {
                        AssistantContent::Text(text) => PresentationBlockKind::Text {
                            text: zevria_content::citations::render_text(&text),
                            flavor: TextFlavor::Markdown,
                            editable: false,
                            sources: zevria_content::citations::text_sources(&text),
                        },
                        AssistantContent::Reasoning(reasoning) => {
                            let parts = readable_reasoning_parts(&reasoning)
                                .map(str::to_string)
                                .collect::<Vec<_>>();
                            if parts.is_empty() {
                                continue;
                            }
                            PresentationBlockKind::Reasoning { parts }
                        }
                        AssistantContent::ToolCall(call) => {
                            let state = ToolCallState::new(
                                status,
                                &zevria_content::message::tool_arguments(&call.function),
                            );
                            PresentationBlockKind::Tool(PresentedTool::Native {
                                call: Box::new(call),
                                state: Box::new(state),
                            })
                        }
                        AssistantContent::Image(_) => {
                            PresentationBlockKind::Placeholder("[image]".to_string())
                        }
                        _ => continue,
                    };
                    blocks.push(PresentationBlock {
                        id: PresentationBlockId(index as u64),
                        revision: 0,
                        role: Some(PresentationRole::Assistant),
                        prompt_group: None,
                        prompt: None,
                        visibility: BlockVisibility::Always,
                        kind,
                    });
                }
            }
            Message::System { content } => blocks.push(PresentationBlock {
                id: PresentationBlockId(0),
                revision: 0,
                role: Some(PresentationRole::System),
                prompt_group: None,
                prompt: None,
                visibility: BlockVisibility::Always,
                kind: PresentationBlockKind::Text {
                    text: content,
                    flavor: TextFlavor::Plain,
                    editable: false,
                    sources: Vec::new(),
                },
            }),
        }
        (!blocks.is_empty()).then_some(Self::Conversation(ConversationEntry {
            header: None,
            blocks,
        }))
    }

    /// Numbered display-turn boundaries shared by folding and Normal-mode
    /// navigation, including handoffs and job continuations. These are not the
    /// editable transcript prompt ordinals; metadata and worker rows are excluded.
    pub fn has_prompt_header(&self) -> bool {
        let header = match self {
            Self::Conversation(entry) => entry.header,
            Self::Ensemble(entry) => entry.header,
            Self::PlanHandoff(_, header) | Self::JobNotice(_, header) => *header,
            _ => None,
        };
        matches!(header, Some(NativeHeader::Prompt(_)))
    }

    pub fn selectable_upper_bound(&self) -> usize {
        match self {
            Self::Conversation(entry) => entry.blocks.len(),
            Self::PlanArtifact(_) | Self::PlanHandoff(..) | Self::JobNotice(..) => 1,
            Self::Ensemble(ensemble) => ensemble.workers.len().saturating_add(1),
            Self::CompactionDivider => 0,
            Self::Error(_) => 1,
        }
    }

    pub fn message_fold_eligible(&self, diagnostics_visible: bool) -> bool {
        self.first_selectable_index(diagnostics_visible).is_some()
    }

    pub fn index_is_selectable(&self, index: usize, diagnostics_visible: bool) -> bool {
        match self {
            Self::Conversation(entry) => entry
                .blocks
                .get(index)
                .is_some_and(|block| block.visible(diagnostics_visible)),
            Self::CompactionDivider => false,
            _ => index < self.selectable_upper_bound(),
        }
    }

    /// Pair each visible user-content block with its message's first visible
    /// block. Match layout's visible role transitions: roleless diagnostics do
    /// not split a group, and native messages remain grouped within an entry.
    /// Eligibility is semantic, not tied to whether the prompt can be edited.
    fn user_message_blocks(
        &self,
        diagnostics_visible: bool,
    ) -> impl Iterator<Item = (usize, usize)> + '_ {
        let blocks = match self {
            Self::Conversation(entry) => entry.blocks.as_slice(),
            _ => &[],
        };
        let mut group_start = None;
        blocks
            .iter()
            .enumerate()
            .filter(move |(_, block)| block.visible(diagnostics_visible))
            .filter_map(move |(index, block)| {
                if block
                    .role
                    .is_some_and(|role| role != PresentationRole::User)
                {
                    group_start = None;
                }
                if block.role == Some(PresentationRole::User)
                    && matches!(
                        block.kind,
                        PresentationBlockKind::Text { .. }
                            | PresentationBlockKind::Image { .. }
                            | PresentationBlockKind::Placeholder(_)
                    )
                {
                    if block.prompt.is_some() {
                        group_start = Some(index);
                    }
                    Some((index, *group_start.get_or_insert(index)))
                } else {
                    None
                }
            })
            .chain(matches!(self, Self::Ensemble(_)).then_some((0, 0)))
    }

    fn first_selectable_index(&self, diagnostics_visible: bool) -> Option<usize> {
        (0..self.selectable_upper_bound())
            .find(|&index| self.index_is_selectable(index, diagnostics_visible))
    }

    fn next_selectable_index(&self, index: usize, diagnostics_visible: bool) -> Option<usize> {
        (index.saturating_add(1)..self.selectable_upper_bound())
            .find(|&candidate| self.index_is_selectable(candidate, diagnostics_visible))
    }

    fn previous_selectable_index(&self, index: usize, diagnostics_visible: bool) -> Option<usize> {
        (0..index.min(self.selectable_upper_bound()))
            .rev()
            .find(|&candidate| self.index_is_selectable(candidate, diagnostics_visible))
    }
}

/// A turn's result, as opposed to all of the activity sharing its entry.
#[derive(Debug, Eq, PartialEq)]
pub enum FinalOutput {
    Answer(Vec<PresentationBlockId>),
    PlanArtifact,
}

fn assistant_answer(block: &PresentationBlock) -> bool {
    block.role == Some(PresentationRole::Assistant)
        && match &block.kind {
            PresentationBlockKind::Text { text, .. } => !text.trim().is_empty(),
            PresentationBlockKind::Image { .. } => true,
            PresentationBlockKind::Placeholder(text) => text == "[image]",
            _ => false,
        }
}

fn tool_call_round(message: &Message) -> bool {
    matches!(message, Message::Assistant(assistant) if assistant.content.iter().any(|part| matches!(part, AssistantContent::ToolCall(_))))
}

pub fn without_tool_calls(message: Message) -> Option<Message> {
    match message {
        Message::Assistant(mut assistant) => {
            assistant
                .content
                .retain(|item| !matches!(item, AssistantContent::ToolCall(_)));
            (!assistant.content.is_empty()).then_some(Message::Assistant(assistant))
        }
        message => Some(message),
    }
}

fn selected_plain_text(entry: &HistoryEntry, content_index: usize) -> Option<String> {
    match entry {
        HistoryEntry::Conversation(entry) => entry
            .blocks
            .get(content_index)
            .map(PresentationBlock::primary_copy),
        HistoryEntry::PlanArtifact(artifact) if content_index == 0 => {
            Some(artifact.markdown.clone())
        }
        HistoryEntry::PlanHandoff(handoff, _) if content_index == 0 => {
            Some(handoff.artifact.markdown.clone())
        }
        HistoryEntry::Ensemble(ensemble) if content_index == 0 => Some(ensemble.command()),
        HistoryEntry::Ensemble(ensemble) => {
            let worker = ensemble.workers.get(content_index.checked_sub(1)?)?;
            Some(worker.copy_text())
        }
        HistoryEntry::JobNotice(notice, _) if content_index == 0 => Some(notice.text().to_owned()),
        HistoryEntry::Error(error) if content_index == 0 => Some(error.clone()),
        HistoryEntry::PlanArtifact(_)
        | HistoryEntry::PlanHandoff(..)
        | HistoryEntry::JobNotice(..)
        | HistoryEntry::CompactionDivider
        | HistoryEntry::Error(_) => None,
    }
}

fn selected_tool(entry: &HistoryEntry, content_index: usize) -> Option<&PresentedTool> {
    let HistoryEntry::Conversation(entry) = entry else {
        return None;
    };
    let PresentationBlockKind::Tool(tool) = &entry.blocks.get(content_index)?.kind else {
        return None;
    };
    Some(tool)
}

fn selected_tool_output(entry: &HistoryEntry, content_index: usize) -> Option<String> {
    let HistoryEntry::Conversation(entry) = entry else {
        return None;
    };
    entry.blocks.get(content_index)?.secondary_copy()
}

fn selected_subtask_id(entry: &HistoryEntry, content_index: usize) -> Option<SubtaskId> {
    let HistoryEntry::Conversation(entry) = entry else {
        return None;
    };
    match &entry.blocks.get(content_index)?.kind {
        PresentationBlockKind::Subtask { descriptor, .. } => Some(descriptor.id.clone()),
        _ => None,
    }
}

fn selected_agent_run_id(entry: &HistoryEntry, content_index: usize) -> Option<AgentRunId> {
    let HistoryEntry::Ensemble(ensemble) = entry else {
        return None;
    };
    ensemble
        .workers
        .get(content_index.checked_sub(1)?)
        .map(|worker| worker.descriptor.id.clone())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub history_index: usize,
    pub content_index: usize,
}

/// A turn's displayed beginning, not its selectable body. Native prompts
/// (including special entries) use projection-scoped entry identity; grouped
/// ACP prompts retain their presentation block identity across reducer updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnStartTarget {
    pub epoch: crate::presentation::ProjectionEpoch,
    pub history_index: usize,
    pub block: Option<PresentationBlockId>,
}

/// Cross-domain consequences of a committed conversation mutation.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct ConversationChange {
    invalidate_from: Option<usize>,
    clear_selection: bool,
    reset_viewport: bool,
    removed_ensemble_runs: Vec<EnsembleRunId>,
}

impl ConversationChange {
    pub fn replaced_from(index: usize) -> Self {
        Self {
            invalidate_from: Some(index),
            clear_selection: true,
            reset_viewport: false,
            removed_ensemble_runs: Vec::new(),
        }
    }

    pub fn projection_replaced() -> Self {
        Self {
            invalidate_from: Some(0),
            clear_selection: true,
            reset_viewport: true,
            removed_ensemble_runs: Vec::new(),
        }
    }

    pub fn invalidate_from(&self) -> Option<usize> {
        self.invalidate_from
    }

    pub const fn clears_selection(&self) -> bool {
        self.clear_selection
    }

    pub const fn resets_viewport(&self) -> bool {
        self.reset_viewport
    }

    pub fn take_removed_ensemble_runs(&mut self) -> Vec<EnsembleRunId> {
        std::mem::take(&mut self.removed_ensemble_runs)
    }

    pub fn merge(&mut self, mut other: Self) {
        self.invalidate_from = match (self.invalidate_from, other.invalidate_from) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left @ Some(_), None) | (None, left @ Some(_)) => left,
            (None, None) => None,
        };
        self.clear_selection |= other.clear_selection;
        self.reset_viewport |= other.reset_viewport;
        self.removed_ensemble_runs
            .append(&mut other.removed_ensemble_runs);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AttemptBlockKey {
    Part(u64, zevria_content::AssistantPartIdentity),
    Action(u64),
    Incomplete,
}

#[derive(Debug)]
struct HostedAttempt {
    history_index: usize,
    attempt: zevria_content::ResponseAttempt,
    ids: HashMap<AttemptBlockKey, PresentationBlockId>,
    owned_ids: HashSet<PresentationBlockId>,
    canonical: Option<Message>,
    acp_tools: HashMap<String, PresentationBlockKind>,
    tool_status: ToolCallStatus,
    final_answer: bool,
    projected: bool,
}

impl HostedAttempt {
    fn answer_item(&self) -> Option<u64> {
        use zevria_content::AssistantPresentationContent as Content;
        self.attempt
            .presentation
            .iter()
            .filter(|part| matches!(&part.content, Content::Answer { text, .. } if !text.trim().is_empty()))
            .map(|part| part.source.output_index)
            .max()
    }

    fn projected_answer_item(&self) -> Option<u64> {
        use zevria_content::{AssistantPresentationContent as Content, ResponseAttemptOutcome};
        if !self.projected
            || self.attempt.outcome != ResponseAttemptOutcome::Completed
            || self
                .attempt
                .presentation
                .iter()
                .any(|part| matches!(part.content, Content::NativeTool { .. }))
        {
            return None;
        }
        self.answer_item()
    }
}

/// TUI-observed assistant work (generation and its tools), never provider telemetry
/// or transcript data.
#[derive(Debug)]
enum HeaderTiming {
    Running(Instant),
    Finished(Duration),
}

impl HeaderTiming {
    fn elapsed(&self, now: Instant) -> Duration {
        match *self {
            Self::Running(started_at) => now.saturating_duration_since(started_at),
            Self::Finished(elapsed) => elapsed,
        }
    }
}

/// Ordered observations retain runtime-only calls until restore. The history
/// boundary lets accepted edits discard numbering and timing with the removed branch.
#[derive(Debug)]
struct HeaderObservation {
    history_index: usize,
    header: NativeHeader,
    timing: Option<HeaderTiming>,
}

#[derive(Debug, Default)]
pub struct ConversationState {
    epoch: crate::presentation::ProjectionEpoch,
    block_ids: crate::presentation::BlockIdAllocator,
    history: Vec<HistoryEntry>,
    // Parallel to history so immutable variants don't need their own wrapper.
    entry_revisions: Vec<u64>,
    entries_by_revision: BTreeMap<u64, usize>,
    revision: u64,
    structure_revision: u64,
    block_hint: Option<(usize, usize, PresentationBlockId)>,
    block_indices: Vec<OnceLock<HashMap<PresentationBlockId, usize>>>,
    header_indices: HashMap<NativeHeader, usize>,
    running_headers: HashMap<NativeHeader, Instant>,
    timing_revision: u64,
    prompt_count: usize,
    call_counts: HashMap<DisplayTurn, usize>,
    represented_headers: HashMap<NativeHeader, Vec<usize>>,
    ensemble_indices: HashMap<EnsembleRunId, usize>,
    plan_versions: HashSet<zevria_workflow::PlanVersion>,
    command_call_locations: HashMap<String, ToolCallLocation>,
    command_job_locations: HashMap<zevria_foundation::CommandJobId, ToolCallLocation>,
    subtask_call_locations: HashMap<String, ToolCallLocation>,
    subtask_locations: HashMap<SubtaskId, Vec<SubtaskLocation>>,
    headers: Vec<HeaderObservation>,
    executing_tool_calls: HashMap<String, Vec<ToolCallLocation>>,
    hosted_attempts: HashMap<String, HostedAttempt>,
    // An empty set of parts is explicit evidence of a non-final native round.
    // Keep IDs, not content ordinals: an ordered attempt can insert web rows.
    response_answers: HashMap<usize, HashSet<PresentationBlockId>>,
    agent_entry: Option<usize>,
    agent_previews: HashMap<usize, HashSet<PresentationBlockId>>,
    incomplete_agent_entries: HashSet<usize>,
    identity_aliases: HashMap<(usize, PresentationBlockId), (usize, PresentationBlockId)>,
    pending_subtasks: HashMap<String, Vec<(usize, SubtaskDescriptor)>>,
    subtask_statuses: HashMap<SubtaskId, SubtaskStatus>,
}

impl ConversationState {
    pub fn epoch(&self) -> crate::presentation::ProjectionEpoch {
        self.epoch
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn entry(&self, index: usize) -> Option<&HistoryEntry> {
        self.history.get(index)
    }

    pub fn final_output(
        &self,
        group: Range<usize>,
        diagnostics_visible: bool,
    ) -> Option<(usize, FinalOutput)> {
        let visible = |block: &PresentationBlock| block.visible(diagnostics_visible);
        // A submitted/published Plan is the result, even if the terminal prose
        // and publication arrived in the opposite order.
        if let Some(index) = group
            .clone()
            .rev()
            .find(|&index| matches!(self.history[index], HistoryEntry::PlanArtifact(_)))
        {
            return Some((index, FinalOutput::PlanArtifact));
        }
        let headerless = !self.history.get(group.start)?.has_prompt_header();
        // Uncommitted attempts and earlier parts of indexed ACP responses are
        // not eligible for the headerless fallback.
        let mut excluded = HashSet::new();
        if headerless {
            for state in self
                .hosted_attempts
                .values()
                .filter(|state| group.contains(&state.history_index))
            {
                let answer_item = state.projected_answer_item();
                excluded.extend(state.ids.iter().filter_map(|(key, id)| match key {
                    AttemptBlockKey::Part(output, _) if Some(*output) != answer_item => Some(*id),
                    _ => None,
                }));
            }
        }
        group.rev().find_map(|index| {
            let HistoryEntry::Conversation(entry) = &self.history[index] else {
                return None;
            };
            if let Some(ids) = self.response_answers.get(&index) {
                let parts: Vec<_> = entry
                    .blocks
                    .iter()
                    .filter(|block| visible(block) && ids.contains(&block.id))
                    .map(|block| block.id)
                    .collect();
                return (!parts.is_empty()).then_some((index, FinalOutput::Answer(parts)));
            }
            if !headerless
                || entry.header.is_some()
                || self.incomplete_agent_entries.contains(&index)
            {
                return None;
            }
            let previews = self.agent_previews.get(&index);
            let mut parts = Vec::new();
            let mut next_group = false;
            for block in entry.blocks.iter().filter(|block| visible(block)) {
                if assistant_answer(block)
                    && !excluded.contains(&block.id)
                    && !previews.is_some_and(|ids| ids.contains(&block.id))
                {
                    if next_group {
                        parts.clear();
                        next_group = false;
                    }
                    parts.push(block.id);
                } else if matches!(block.kind, PresentationBlockKind::Tool(_))
                    || block.role == Some(PresentationRole::User)
                {
                    parts.clear();
                } else if matches!(block.kind, PresentationBlockKind::Reasoning { .. }) {
                    next_group = true;
                }
            }
            (!parts.is_empty()).then_some((index, FinalOutput::Answer(parts)))
        })
    }

    pub fn is_committed_answer(&self, index: usize, id: PresentationBlockId) -> bool {
        if let Some(parts) = self.response_answers.get(&index) {
            return parts.contains(&id);
        }
        self.agent_entry == Some(index) && !self.incomplete_agent_entries.contains(&index)
            && self.hosted_attempts.values().any(|state| {
                state.history_index == index && state.projected && state.ids.iter().any(|(key, &part)| {
                    part == id && matches!(key, AttemptBlockKey::Part(output, _) if Some(*output) == state.projected_answer_item())
                })
            })
    }

    pub fn growing_entries(&self) -> HashSet<usize> {
        use zevria_content::ResponseAttemptOutcome::*;
        self.agent_entry
            .into_iter()
            .chain(self.hosted_attempts.values().filter_map(|state| {
                (state.canonical.is_none()
                    && matches!(state.attempt.outcome, InProgress | Completed))
                .then_some(state.history_index)
            }))
            .collect()
    }

    pub(crate) fn set_agent_entry(&mut self, index: usize) {
        self.agent_entry = Some(index);
    }

    pub(crate) fn mark_agent_preview(&mut self, index: usize, id: PresentationBlockId) {
        self.agent_previews.entry(index).or_default().insert(id);
    }

    pub(crate) fn complete_agent_entry(&mut self, index: usize) {
        let changed = self.agent_previews.remove(&index).is_some()
            | self.incomplete_agent_entries.remove(&index);
        let mut answer_changed = false;
        if let Some((_, FinalOutput::Answer(parts))) = self.final_output(index..index + 1, true) {
            let parts = parts.into_iter().collect();
            if self.response_answers.get(&index) != Some(&parts) {
                self.response_answers.insert(index, parts);
                answer_changed = true;
            }
        }
        if changed || answer_changed {
            self.touch_entry(index);
            self.change_structure();
        }
    }

    pub(crate) fn mark_agent_entry_incomplete(&mut self, index: usize) {
        if self.response_answers.remove(&index).is_some()
            | self.incomplete_agent_entries.insert(index)
        {
            self.touch_entry(index);
            self.change_structure();
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn structure_revision(&self) -> u64 {
        self.structure_revision
    }

    pub fn entry_revision(&self, index: usize) -> u64 {
        self.entry_revisions[index]
    }

    /// Each extant entry is returned at most once, even if it was updated
    /// repeatedly while the view was hidden. Projection replacement uses epoch.
    pub fn changed_entries_since(&self, revision: u64) -> impl Iterator<Item = usize> + '_ {
        self.entries_by_revision
            .range((
                std::ops::Bound::Excluded(revision),
                std::ops::Bound::Unbounded,
            ))
            .map(|(_, &index)| index)
    }

    pub(crate) fn touch_entry(&mut self, index: usize) {
        self.revision += 1;
        let previous = std::mem::replace(&mut self.entry_revisions[index], self.revision);
        self.entries_by_revision.remove(&previous);
        self.entries_by_revision.insert(self.revision, index);
    }

    pub(crate) fn change_structure(&mut self) {
        self.structure_revision += 1;
    }

    pub(crate) fn invalidate_block_indices(&mut self, index: usize) {
        if let Some(indices) = self.block_indices.get_mut(index) {
            indices.take();
        }
    }

    pub(crate) fn block_index(&self, index: usize, id: PresentationBlockId) -> Option<usize> {
        let HistoryEntry::Conversation(entry) = self.entry(index)? else {
            return None;
        };
        self.block_indices[index]
            .get_or_init(|| {
                entry
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| (block.id, index))
                    .collect()
            })
            .get(&id)
            .copied()
    }

    pub(crate) fn append_entry_block(&mut self, index: usize, block: PresentationBlock) {
        let HistoryEntry::Conversation(entry) = &mut self.history[index] else {
            unreachable!()
        };
        if let Some(indices) = self.block_indices[index].get_mut() {
            indices.insert(block.id, entry.blocks.len());
        }
        entry.blocks.push(block);
        self.touch_entry(index);
    }

    pub(crate) fn hinted_block_mut(
        &mut self,
        history_index: usize,
        block_id: PresentationBlockId,
    ) -> Option<&mut PresentationBlock> {
        let HistoryEntry::Conversation(entry) = self.history.get_mut(history_index)? else {
            return None;
        };
        let block_index = if let Some((history, index, id)) = self.block_hint
            && history == history_index
            && id == block_id
            && entry.blocks.get(index).is_some_and(|block| block.id == id)
        {
            index
        } else {
            match self.block_indices[history_index].get() {
                Some(indices) => indices.get(&block_id).copied()?,
                None => entry
                    .blocks
                    .iter()
                    .rposition(|block| block.id == block_id)?,
            }
        };
        self.block_hint = Some((history_index, block_index, block_id));
        entry.blocks.get_mut(block_index)
    }

    pub(crate) fn remove_block(&mut self, index: usize, id: PresentationBlockId) -> bool {
        let Some(position) = self.block_index(index, id) else {
            return false;
        };
        let HistoryEntry::Conversation(entry) = &mut self.history[index] else {
            unreachable!("only conversation entries contain blocks");
        };
        entry.blocks.remove(position);
        self.invalidate_block_indices(index);
        self.touch_entry(index);
        self.change_structure();
        true
    }

    pub(crate) fn entry_mut_untracked(&mut self, index: usize) -> Option<&mut HistoryEntry> {
        self.history.get_mut(index)
    }

    pub fn entry_mut(&mut self, index: usize) -> Option<&mut HistoryEntry> {
        if index >= self.history.len() {
            return None;
        }
        self.touch_entry(index);
        self.invalidate_block_indices(index);
        self.history.get_mut(index)
    }

    pub fn allocate_block_id(&mut self) -> PresentationBlockId {
        self.block_ids.allocate()
    }

    pub fn push_entry(&mut self, mut entry: HistoryEntry) -> usize {
        let index = self.history.len();
        match &mut entry {
            HistoryEntry::Conversation(conversation) => {
                self.block_ids.identify(conversation);
                if let Some(header) = conversation.header {
                    self.represented_headers
                        .entry(header)
                        .or_default()
                        .push(index);
                }
            }
            HistoryEntry::Ensemble(ensemble) => {
                self.ensemble_indices.insert(ensemble.run_id.clone(), index);
            }
            HistoryEntry::PlanArtifact(artifact) => {
                self.plan_versions.insert(artifact.version);
            }
            _ => {}
        }
        self.history.push(entry);
        self.entry_revisions.push(0);
        self.block_indices.push(OnceLock::new());
        self.touch_entry(index);
        index
    }

    pub fn replace_projection(&mut self, projection: ConversationState) -> ConversationChange {
        *self = projection;
        self.epoch = Default::default();
        ConversationChange::projection_replaced()
    }

    pub fn clear_projection(&mut self) -> ConversationChange {
        *self = Self::default();
        ConversationChange::projection_replaced()
    }

    pub fn contains_presented_error(&self, error: &str) -> bool {
        self.history.iter().any(|entry| {
            let HistoryEntry::Conversation(entry) = entry else {
                return false;
            };
            entry.blocks.iter().any(|block| {
                matches!(&block.kind, PresentationBlockKind::Error(existing) if existing == error)
            })
        })
    }

    pub fn push_error(&mut self, error: String) {
        self.push_entry(HistoryEntry::Error(error));
    }

    pub fn push_plan_artifact(&mut self, artifact: PlanArtifact) {
        if !self.plan_versions.contains(&artifact.version) {
            self.push_entry(HistoryEntry::PlanArtifact(artifact));
        }
    }

    pub fn push_plan_handoff(&mut self, handoff: PlanHandoff, turn: DisplayTurn) {
        self.push_entry(HistoryEntry::PlanHandoff(
            handoff,
            Some(NativeHeader::Prompt(turn)),
        ));
    }

    pub fn push_job_notice(
        &mut self,
        notice: zevria_foundation::CommandJobNotice,
        header: Option<NativeHeader>,
    ) {
        self.push_entry(HistoryEntry::JobNotice(notice, header));
    }

    pub fn push_compaction_divider(&mut self) {
        self.push_entry(HistoryEntry::CompactionDivider);
    }

    pub fn push_user_turn(&mut self, message: Message, turn: DisplayTurn) {
        if let Some(HistoryEntry::Conversation(mut entry)) =
            HistoryEntry::from_message(message, ToolCallStatus::Finished)
        {
            entry.header = Some(NativeHeader::Prompt(turn));
            self.push_entry(HistoryEntry::Conversation(entry));
        }
    }

    pub fn push_message(&mut self, message: Message, status: ToolCallStatus) {
        self.push_numbered_message(message, status, None);
    }

    fn push_numbered_message(
        &mut self,
        message: Message,
        status: ToolCallStatus,
        header: Option<NativeHeader>,
    ) -> Option<usize> {
        let mut entry = HistoryEntry::from_message(message, status)?;
        if let HistoryEntry::Conversation(conversation) = &mut entry {
            conversation.header = header;
        }
        let history_index = self.push_entry(entry);
        if status == ToolCallStatus::Executing
            && let HistoryEntry::Conversation(conversation) = &self.history[history_index]
        {
            for block in &conversation.blocks {
                if let Some((call, _)) = block.native_tool() {
                    self.executing_tool_calls
                        .entry(call.id.to_string())
                        .or_default()
                        .push(ToolCallLocation {
                            history_index,
                            block_id: block.id,
                        });
                }
            }
        }
        self.index_native_locations(history_index);
        Some(history_index)
    }

    pub fn has_executing_tool_calls(&self) -> bool {
        !self.executing_tool_calls.is_empty()
    }

    pub fn header_has_executing_tool_calls(&self, header: NativeHeader) -> bool {
        self.executing_tool_calls
            .values()
            .flatten()
            .any(|location| {
                matches!(self.history.get(location.history_index),
                Some(HistoryEntry::Conversation(entry)) if entry.header == Some(header))
            })
    }

    fn tool_call_at(&self, location: ToolCallLocation) -> Option<&ToolCall> {
        let HistoryEntry::Conversation(entry) = self.history.get(location.history_index)? else {
            return None;
        };
        entry
            .blocks
            .get(self.block_index(location.history_index, location.block_id)?)?
            .native_tool()
            .map(|(call, _)| call)
    }

    fn select_executing_call(&self, result: &ToolResult) -> Option<ToolCallLocation> {
        let candidates = self.executing_tool_calls.get(result.call.wire().as_ref())?;
        [true, false].into_iter().find_map(|exact_call_id| {
            candidates
                .iter()
                .filter(|location| {
                    self.tool_call_at(**location).is_some_and(|call| {
                        call_ids_match(
                            provider_call_id(call.id.provider()),
                            provider_call_id(result.call.provider()),
                            exact_call_id,
                        )
                    })
                })
                .max_by_key(|location| {
                    (
                        location.history_index,
                        std::cmp::Reverse(location.block_id.0),
                    )
                })
                .copied()
        })
    }

    fn finish_tool_call(
        &mut self,
        location: ToolCallLocation,
        result: &ToolResult,
        metadata: Option<&ToolResultMetadata>,
    ) {
        self.remove_executing(result.call.wire().as_ref(), location);
        let index = location.history_index;
        let Some(block_index) = self.block_index(index, location.block_id) else {
            return;
        };
        let HistoryEntry::Conversation(entry) = &mut self.history[index] else {
            return;
        };
        let block = &mut entry.blocks[block_index];
        let Some((call, state)) = block.native_tool_mut() else {
            return;
        };
        state.status = ToolCallStatus::Finished;
        state.web_search_activity.clear();
        state.result = Some(result.clone());
        state.metadata = metadata
            .filter(|metadata| metadata.tool_name == call.function.name)
            .cloned();
        if call.function.name == zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME {
            for entry in state
                .metadata
                .as_ref()
                .map_or(&[][..], ToolResultMetadata::subtasks)
            {
                let Some(launch) = &entry.launch else {
                    continue;
                };
                let descriptor =
                    state
                        .subtasks
                        .entry(entry.index)
                        .or_insert_with(|| SubtaskDescriptor {
                            id: launch.id.clone(),
                            parent_session_id: String::new(),
                            title: launch.title.clone(),
                            kind: launch.kind,
                            workspace: launch.workspace.clone(),
                            status: entry.status,
                        });
                if !descriptor.status.is_terminal() {
                    descriptor.status = entry.status;
                }
            }
        }
        let has_children = !state.subtasks.is_empty();
        let job_id = state
            .metadata
            .as_ref()
            .filter(|_| call.function.name == zevria_foundation::COMMAND_TOOL_NAME)
            .and_then(ToolResultMetadata::command_job)
            .map(|job| job.id.clone());
        block.touch();
        if let Some(id) = job_id {
            insert_newest(&mut self.command_job_locations, id, location);
        }
        if has_children {
            sync_subtask_rows(entry, location.block_id, &mut self.block_ids);
            self.invalidate_block_indices(index);
            self.index_subtask_descriptors(location);
        }
        self.touch_entry(index);
    }

    fn remove_executing(&mut self, call_id: &str, location: ToolCallLocation) {
        if let Some(candidates) = self.executing_tool_calls.get_mut(call_id) {
            candidates.retain(|candidate| *candidate != location);
            if candidates.is_empty() {
                self.executing_tool_calls.remove(call_id);
            }
        }
    }

    /// Return only headers whose last executing tool was settled by these results.
    /// A late or duplicate result must not stop a newer model call's clock.
    pub fn finish_tool_results(
        &mut self,
        message: &Message,
        metadata: &[ToolResultMetadata],
    ) -> Vec<NativeHeader> {
        let Message::User { content } = message else {
            return Vec::new();
        };
        let mut headers = Vec::new();
        let mut metadata_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, metadata) in metadata.iter().enumerate() {
            metadata_by_id
                .entry(metadata.id.as_str())
                .or_default()
                .push(index);
        }
        for item in content {
            let UserContent::ToolResult(result) = item else {
                continue;
            };
            let matched_metadata = [true, false].into_iter().find_map(|exact_call_id| {
                let candidates = metadata_by_id.get_mut(result.call.wire().as_ref())?;
                let position = candidates.iter().position(|&index| {
                    call_ids_match(
                        metadata[index].call_id.as_deref(),
                        provider_call_id(result.call.provider()),
                        exact_call_id,
                    )
                })?;
                Some(&metadata[candidates.remove(position)])
            });
            if let Some(location) = self.select_executing_call(result) {
                if let Some(HistoryEntry::Conversation(entry)) =
                    self.history.get(location.history_index)
                    && let Some(header) = entry.header
                    && !headers.contains(&header)
                {
                    headers.push(header);
                }
                self.finish_tool_call(location, result, matched_metadata);
            }
        }
        headers.retain(|&header| !self.header_has_executing_tool_calls(header));
        headers
    }

    pub fn update_web_search_activity(
        &mut self,
        call_id: &str,
        activity: Vec<zevria_content::HostedActivity>,
    ) {
        let Some(location) = self
            .executing_tool_calls
            .get(call_id)
            .and_then(|locations| locations.last())
            .copied()
        else {
            return;
        };
        let index = location.history_index;
        let Some(block_index) = self.block_index(index, location.block_id) else {
            return;
        };
        let HistoryEntry::Conversation(entry) = &mut self.history[index] else {
            return;
        };
        let block = &mut entry.blocks[block_index];
        if let Some((call, state)) = block.native_tool_mut()
            && call.function.name == zevria_foundation::WEB_SEARCH_TOOL_NAME
            && state.status == ToolCallStatus::Executing
            && state.web_search_activity != activity
        {
            state.web_search_activity = activity;
            block.touch();
            self.touch_entry(index);
        }
    }

    pub fn interrupt_executing_tool_calls(&mut self) {
        for locations in std::mem::take(&mut self.executing_tool_calls).into_values() {
            for location in locations {
                let index = location.history_index;
                let Some(block_index) = self.block_index(index, location.block_id) else {
                    continue;
                };
                let HistoryEntry::Conversation(entry) = &mut self.history[index] else {
                    continue;
                };
                let block = &mut entry.blocks[block_index];
                if let Some((_, state)) = block.native_tool_mut() {
                    state.status = ToolCallStatus::Interrupted;
                    state.web_search_activity.clear();
                    block.touch();
                    self.touch_entry(index);
                }
            }
        }
    }

    pub fn attach_subtask_launch(
        &mut self,
        call_id: &str,
        entry_index: usize,
        mut descriptor: SubtaskDescriptor,
    ) {
        if let Some(status) = self.subtask_statuses.get(&descriptor.id) {
            descriptor.status = *status;
        }
        if let Some(&location) = self.subtask_call_locations.get(call_id)
            && let Some(block_index) = self.block_index(location.history_index, location.block_id)
            && let HistoryEntry::Conversation(entry) = &mut self.history[location.history_index]
            && let Some((_, state)) = entry.blocks[block_index].native_tool_mut()
        {
            if let Some(existing) = state.subtasks.get(&entry_index) {
                if existing.id != descriptor.id {
                    return;
                }
                if existing.status.is_terminal() || descriptor.status == SubtaskStatus::Starting {
                    descriptor.status = existing.status;
                }
                if existing == &descriptor {
                    return;
                }
            }
            state.subtasks.insert(entry_index, descriptor);
            entry.blocks[block_index].touch();
            sync_subtask_rows(entry, location.block_id, &mut self.block_ids);
            self.invalidate_block_indices(location.history_index);
            self.index_subtask_descriptors(location);
            self.touch_entry(location.history_index);
        } else {
            let pending = self.pending_subtasks.entry(call_id.into()).or_default();
            pending.retain(|(index, _)| *index != entry_index);
            pending.push((entry_index, descriptor));
        }
    }

    pub fn update_command_job(&mut self, job: &zevria_foundation::CommandJobSnapshot) {
        let by_call = job
            .call_id
            .as_ref()
            .and_then(|id| self.command_call_locations.get(id));
        let by_job = self.command_job_locations.get(&job.id);
        let Some((location, block_index)) = by_call
            .into_iter()
            .chain(by_job)
            .filter_map(|&loc| Some((loc, self.block_index(loc.history_index, loc.block_id)?)))
            .max_by_key(|(loc, position)| (loc.history_index, std::cmp::Reverse(*position)))
        else {
            return;
        };
        let index = location.history_index;
        if let HistoryEntry::Conversation(entry) = &mut self.history[index]
            && let Some((call, state)) = entry.blocks[block_index].native_tool_mut()
            && call.function.name.as_str() == zevria_foundation::COMMAND_TOOL_NAME
            && state.command_job.as_ref() != Some(job)
        {
            state.command_job = Some(job.clone());
            entry.blocks[block_index].touch();
            insert_newest(&mut self.command_job_locations, job.id.clone(), location);
            self.touch_entry(index);
        }
    }

    /// Keep status-before-launch observations; never regress terminal children
    /// when delayed Starting/Running announcements arrive.
    pub fn update_subtask_status(&mut self, id: &SubtaskId, status: SubtaskStatus) {
        let current = self.subtask_statuses.entry(id.clone()).or_insert(status);
        if !current.is_terminal() && status != SubtaskStatus::Starting {
            *current = status;
        }
        let status = *current;
        let Some(locations) = self.subtask_locations.get(id).cloned() else {
            return;
        };
        for SubtaskLocation {
            parent,
            entry_index,
            row,
        } in locations
        {
            let index = parent.history_index;
            let row = self.block_index(index, row);
            if let Some(block_index) = self.block_index(index, parent.block_id)
                && let HistoryEntry::Conversation(entry) = &mut self.history[index]
                && let Some((_, state)) = entry.blocks[block_index].native_tool_mut()
                && let Some(descriptor) = state.subtasks.get_mut(&entry_index)
                && &descriptor.id == id
                && !descriptor.status.is_terminal()
                && descriptor.status != status
            {
                descriptor.status = status;
                entry.blocks[block_index].touch();
                if let Some(row) = row.map(|row| &mut entry.blocks[row])
                    && let PresentationBlockKind::Subtask { descriptor, .. } = &mut row.kind
                {
                    descriptor.status = status;
                    row.touch();
                }
                self.touch_entry(index);
            }
        }
    }

    fn index_subtask_descriptors(&mut self, location: ToolCallLocation) {
        let index = location.history_index;
        let Some(block_index) = self.block_index(index, location.block_id) else {
            return;
        };
        if let HistoryEntry::Conversation(entry) = &self.history[index] {
            for block in &entry.blocks[block_index + 1..] {
                if !matches!(&block.kind, PresentationBlockKind::Subtask { parent, .. } if *parent == location.block_id)
                {
                    break;
                }
                index_subtask_row(&mut self.subtask_locations, index, block);
            }
        }
    }

    fn index_native_locations(&mut self, index: usize) {
        let HistoryEntry::Conversation(entry) = &self.history[index] else {
            return;
        };
        let mut accepted = Vec::new();
        // Reverse insertion preserves the original first-block tie breaker.
        for block in entry.blocks.iter().rev() {
            index_subtask_row(&mut self.subtask_locations, index, block);
            let Some((call, state)) = block.native_tool() else {
                continue;
            };
            let location = ToolCallLocation {
                history_index: index,
                block_id: block.id,
            };
            match call.function.name.as_str() {
                zevria_foundation::COMMAND_TOOL_NAME => {
                    insert_newest(
                        &mut self.command_call_locations,
                        call.id.to_string(),
                        location,
                    );
                    if let Some(job) = state
                        .metadata
                        .as_ref()
                        .and_then(|m| m.detail.as_ref())
                        .and_then(|d| d.command_job())
                    {
                        insert_newest(&mut self.command_job_locations, job.id.clone(), location);
                    }
                    if let Some(job) = &state.command_job {
                        insert_newest(&mut self.command_job_locations, job.id.clone(), location);
                    }
                }
                zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME => {
                    let call_id = call.id.to_string();
                    insert_newest(&mut self.subtask_call_locations, call_id.clone(), location);
                    if let Some(pending) = self.pending_subtasks.remove(&call_id) {
                        accepted.push((call_id, pending));
                    }
                }
                _ => {}
            }
        }
        for (id, pending) in accepted {
            for (index, descriptor) in pending {
                self.attach_subtask_launch(&id, index, descriptor);
            }
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_entry(&mut self, entry: HistoryEntry) {
        self.push_entry(entry);
    }
}

fn index_subtask_row(
    locations: &mut HashMap<SubtaskId, Vec<SubtaskLocation>>,
    history_index: usize,
    block: &PresentationBlock,
) {
    if let PresentationBlockKind::Subtask {
        parent,
        entry_index,
        descriptor,
    } = &block.kind
    {
        let location = SubtaskLocation {
            parent: ToolCallLocation {
                history_index,
                block_id: *parent,
            },
            entry_index: *entry_index,
            row: block.id,
        };
        let rows = locations.entry(descriptor.id.clone()).or_default();
        if !rows.contains(&location) {
            rows.push(location);
        }
    }
}

fn insert_newest<K: std::hash::Hash + Eq>(
    index: &mut HashMap<K, ToolCallLocation>,
    key: K,
    location: ToolCallLocation,
) {
    let existing = index.entry(key).or_insert(location);
    if location.history_index >= existing.history_index {
        *existing = location;
    }
}

fn native_tool_keys(
    blocks: &[PresentationBlock],
    owned: &HashSet<PresentationBlockId>,
) -> Vec<(String, PresentationBlockId, ToolCallStatus)> {
    blocks
        .iter()
        .filter_map(|block| {
            let (call, state) = block.native_tool()?;
            owned
                .contains(&block.id)
                .then(|| (call.id.to_string(), block.id, state.status))
        })
        .collect()
}

/// Materialize one selectable row per accepted child in input order, retaining
/// existing block IDs so selection and fold state survive insertion/reordering.
fn sync_subtask_rows(
    entry: &mut ConversationEntry,
    parent: PresentationBlockId,
    ids: &mut crate::presentation::BlockIdAllocator,
) {
    let Some(position) = entry.blocks.iter().position(|block| block.id == parent) else {
        return;
    };
    let Some((_, state)) = entry.blocks[position].native_tool() else {
        return;
    };
    let children = state.subtasks.clone();
    // Reconciliation can move the tool while leaving its previous child rows in
    // place. Collect them in a single pass before splicing the desired order.
    let mut previous = HashMap::new();
    let original = std::mem::take(&mut entry.blocks);
    for block in original {
        if let PresentationBlockKind::Subtask {
            parent: owner,
            entry_index,
            ..
        } = &block.kind
            && *owner == parent
        {
            previous.insert(*entry_index, block);
        } else {
            entry.blocks.push(block);
        }
    }
    let position = entry
        .blocks
        .iter()
        .position(|block| block.id == parent)
        .expect("parent retained");
    entry.blocks.splice(
        position + 1..position + 1,
        children.into_iter().map(|(entry_index, descriptor)| {
            let kind = PresentationBlockKind::Subtask {
                parent,
                entry_index,
                descriptor,
            };
            if let Some(mut row) = previous.remove(&entry_index) {
                if row.kind != kind {
                    row.kind = kind;
                    row.touch();
                }
                row
            } else {
                PresentationBlock {
                    id: ids.allocate(),
                    revision: 0,
                    role: Some(PresentationRole::Assistant),
                    prompt_group: None,
                    prompt: None,
                    visibility: BlockVisibility::Always,
                    kind,
                }
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_workflow::AgentRunDescriptor;
    use zevria_workflow::EnsembleRunId;

    #[test]
    fn local_search_activity_only_lives_on_its_executing_call() {
        use rig_core::message::{CallId, ToolFunction, ToolName};
        use zevria_content::{HostedActivity, HostedActivityKind, HostedActivityStatus};
        let mut conversation = ConversationState::default();
        let name = ToolName::new(zevria_foundation::WEB_SEARCH_TOOL_NAME).unwrap();
        for id in ["search-one", "search-two"] {
            let call = ToolCall::from_wire(
                id,
                ToolFunction::new(name.clone(), serde_json::json!({"query":"version"})),
            );
            conversation.push_message(
                Message::from(vec![AssistantContent::ToolCall(call)]),
                ToolCallStatus::Executing,
            );
        }
        let activity = vec![HostedActivity {
            kind: HostedActivityKind::WebSearch,
            item_id: None,
            output_index: 0,
            status: HostedActivityStatus::InProgress,
            action: Some(serde_json::json!({"type":"open_page","url":"https://example.com/docs"})),
        }];
        let read = |state: &ConversationState, index| {
            let Some(HistoryEntry::Conversation(entry)) = state.entry(index) else {
                panic!("tool row absent")
            };
            let (_, state) = entry.blocks.iter().find_map(|b| b.native_tool()).unwrap();
            (state.status, state.web_search_activity.clone())
        };
        conversation.update_web_search_activity("search-one", activity.clone());
        assert_eq!(
            read(&conversation, 0),
            (ToolCallStatus::Executing, activity.clone())
        );
        assert_eq!(read(&conversation, 1), (ToolCallStatus::Executing, vec![]));
        conversation.finish_tool_results(
            &Message::tool_result(CallId::from_wire("search-one"), name, "version 2"),
            &[],
        );
        conversation.update_web_search_activity("search-one", activity.clone());
        assert_eq!(read(&conversation, 0), (ToolCallStatus::Finished, vec![]));
        conversation.update_web_search_activity("search-two", activity);
        conversation.interrupt_executing_tool_calls();
        assert_eq!(
            read(&conversation, 1),
            (ToolCallStatus::Interrupted, vec![])
        );
    }

    #[test]
    fn late_command_result_does_not_replace_the_newer_job_location() {
        use rig_core::message::{CallId, ToolFunction, ToolName};
        use zevria_foundation::{
            COMMAND_TOOL_NAME, CommandJobDetail, CommandJobDisposition, CommandJobId,
            ToolCallOutcome, ToolResultDetail,
        };

        let mut conversation = ConversationState::default();
        let command = ToolName::new(COMMAND_TOOL_NAME).unwrap();
        let job_id = CommandJobId::new("job-shared");
        for id in ["older", "newer"] {
            let call = ToolCall::from_wire(
                id,
                ToolFunction::new(command.clone(), serde_json::json!({})),
            );
            conversation.push_message(
                Message::from(vec![AssistantContent::ToolCall(call)]),
                ToolCallStatus::Executing,
            );
        }
        for id in ["newer", "older"] {
            conversation.finish_tool_results(
                &Message::tool_result(CallId::from_wire(id), command.clone(), "job started"),
                &[ToolResultMetadata {
                    id: id.into(),
                    call_id: None,
                    tool_name: COMMAND_TOOL_NAME.into(),
                    outcome: ToolCallOutcome::Success,
                    diagnostic: None,
                    detail: Some(ToolResultDetail::CommandJob(CommandJobDetail {
                        id: job_id.clone(),
                        disposition: CommandJobDisposition::StartedInBackground,
                    })),
                }],
            );
        }

        assert!(!conversation.has_executing_tool_calls());
        assert_eq!(conversation.command_job_locations[&job_id].history_index, 1);
    }

    #[test]
    fn shared_header_representation_survives_empty_entry_and_edit() {
        let mut conversation = ConversationState::default();
        let turn = conversation.allocate_turn();
        let header = conversation.allocate_call(turn);
        let visible = conversation
            .push_numbered_message(
                Message::assistant("earlier response"),
                ToolCallStatus::Finished,
                Some(header),
            )
            .unwrap();
        let empty = conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
            header: Some(header),
            blocks: Vec::new(),
        }));
        assert_eq!(
            conversation.represented_header_indices(header),
            &[visible, empty]
        );
        assert!(conversation.represents_header(header));
        conversation.commit_edit(empty, false);
        assert_eq!(conversation.represented_header_indices(header), &[visible]);
        assert!(conversation.represents_header(header));
    }

    #[test]
    fn turn_targets_preserve_block_identity_and_reject_replacement_epochs() {
        let mut conversation = ConversationState::default();
        conversation.push_message(
            Message::user("headerless ACP prompt"),
            ToolCallStatus::Finished,
        );
        let block_target = conversation.turn_starts(false)[0];
        assert!(block_target.block.is_some());
        let turn = conversation.allocate_turn();
        conversation.push_user_turn(Message::user("numbered prompt"), turn);
        let entry_target = conversation.turn_starts(false)[1];
        assert_eq!(entry_target.block, None);
        assert_eq!(
            conversation.reconcile_turn_start(entry_target),
            Some(entry_target)
        );

        // Aliased blocks can move within the same projection; targets follow
        // presentation identity, not an old content index or entry offset.
        let HistoryEntry::Conversation(entry) = conversation.entry_mut(0).unwrap() else {
            panic!()
        };
        let old = entry.blocks[0].id;
        entry.blocks[0].id = PresentationBlockId(100);
        conversation
            .identity_aliases
            .insert((0, old), (0, PresentationBlockId(100)));
        let reconciled = conversation.reconcile_turn_start(block_target).unwrap();
        assert_eq!(reconciled.block, Some(PresentationBlockId(100)));
        assert_eq!(reconciled.epoch, block_target.epoch);
        conversation.restore(vec![TranscriptItem::Message(Message::user("replacement"))]);
        assert_eq!(conversation.reconcile_turn_start(block_target), None);
        assert_eq!(conversation.reconcile_turn_start(entry_target), None);
    }

    #[test]
    fn turn_targets_ignore_zero_content_and_nonprompt_roles_without_changing_user_selection() {
        let mut conversation = ConversationState::default();
        conversation.push_message(Message::user("user"), ToolCallStatus::Finished);
        conversation.push_message(
            Message::assistant("You · #2 is only body text"),
            ToolCallStatus::Finished,
        );
        conversation.push_message(Message::system("system"), ToolCallStatus::Finished);
        conversation.push_message(
            Message::tool_result(
                rig_core::message::CallId::from_wire("call"),
                rig_core::message::ToolName::new("command").expect("nonempty tool name"),
                "tool output",
            ),
            ToolCallStatus::Finished,
        );
        conversation.push_entry(HistoryEntry::Error("error".into()));
        conversation.push_entry(HistoryEntry::CompactionDivider);
        let first = conversation.turn_starts(false);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].history_index, 0);
        let turn = conversation.allocate_turn();
        conversation.push_user_turn(Message::user("second"), turn);
        let second = conversation.turn_starts(false)[1];
        assert_eq!(
            conversation.next_user_message(
                Selection {
                    history_index: 0,
                    content_index: 0
                },
                false
            ),
            Some(Selection {
                history_index: second.history_index,
                content_index: 0
            })
        );
        conversation.commit_edit(second.history_index, false);
        assert_eq!(conversation.reconcile_turn_start(second), None);
    }

    #[test]
    fn native_acp_and_hosted_blocks_share_one_projection_allocator() {
        use zevria_content::{
            HostedActivity, HostedActivityStatus, ResponseAttempt, ResponseAttemptOutcome,
        };
        let mut conversation = ConversationState::default();
        conversation.push_message(Message::user("native prompt"), ToolCallStatus::Finished);
        let mut acp = crate::agent_transcript::AgentTranscriptReducer::default();
        acp.apply_event(
            &mut conversation,
            zevria_workflow::AgentRunEvent::AgentMessage {
                text: "external message".into(),
                message_id: Some("message".into()),
            },
        );
        let mut attempt = ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("p", "m"));
        attempt.hosted_activity.push(HostedActivity {
            kind: zevria_content::HostedActivityKind::WebSearch,
            item_id: Some("action".into()),
            output_index: 0,
            status: HostedActivityStatus::Searching,
            action: Some(serde_json::json!({"type":"search", "query":"Rust"})),
        });
        conversation.update_response_attempt(attempt.clone());
        let before = (0..3)
            .map(|history_index| {
                conversation
                    .selection_identity(Selection {
                        history_index,
                        content_index: 0,
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        conversation.push_message(
            Message::assistant("native answer"),
            ToolCallStatus::Finished,
        );
        attempt.close(ResponseAttemptOutcome::Interrupted);
        conversation.update_response_attempt(attempt);
        acp.apply_event(
            &mut conversation,
            zevria_workflow::AgentRunEvent::AgentMessage {
                text: " continued".into(),
                message_id: Some("message".into()),
            },
        );
        let identities = conversation
            .history()
            .iter()
            .flat_map(|entry| match entry {
                HistoryEntry::Conversation(entry) => entry
                    .blocks
                    .iter()
                    .map(|block| block.id)
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identities.len(),
            identities
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
        for (history_index, id) in before.into_iter().enumerate() {
            assert_eq!(
                conversation.selection_identity(Selection {
                    history_index,
                    content_index: 0
                }),
                Some(id)
            );
        }
        assert!(
            identities.iter().all(|id| id.0 < 16),
            "no reserved namespaces: {identities:?}"
        );
    }

    #[test]
    fn hosted_rows_update_restore_and_citations_are_copyable() {
        use zevria_content::HostedActivity;
        use zevria_content::HostedActivityStatus;
        use zevria_content::ResponseAttempt;
        use zevria_content::ResponseAttemptOutcome;
        use zevria_foundation::ModelProfileRef;
        let mut attempt = ResponseAttempt::new(ModelProfileRef::new("p", "m"));
        attempt.hosted_activity.push(HostedActivity {
            kind: zevria_content::HostedActivityKind::WebSearch,
            item_id: Some("ws".into()),
            output_index: 0,
            status: HostedActivityStatus::Searching,
            action: Some(serde_json::json!({"type":"search","query":"Rust"})),
        });
        let mut conversation = ConversationState::default();
        let items = [
            serde_json::json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Claim","annotations":[{"type":"url_citation","start_index":0,"end_index":5,"title":"Source","url":"https://example.com/a"}]}]}),
        ];
        attempt.presentation = zevria_content::response::responses_native_display(&items).0;
        attempt.presentation[0].source.output_index = 1;
        let sources_for = |history: &HistoryEntry| {
            let HistoryEntry::Conversation(entry) = history else {
                panic!("conversation")
            };
            entry
                .blocks
                .iter()
                .find_map(|block| match &block.kind {
                    PresentationBlockKind::Text { sources, .. } => Some(sources.clone()),
                    _ => None,
                })
                .unwrap()
        };
        conversation.update_response_attempt(attempt.clone());
        let HistoryEntry::Conversation(entry) = &conversation.history()[0] else {
            panic!("conversation")
        };
        let PresentationBlockKind::WebActivity(web) = &entry.blocks[0].kind else {
            panic!("web activity")
        };
        assert_eq!(web.status(), crate::status_icon::StatusIcon::Running);
        assert_eq!(web.actions[0].kind, crate::blocks::WebActionKind::Search);
        assert_eq!(web.actions[0].queries, ["Rust"]);
        assert_eq!(web.copy_text(), "Web search: Rust ◐");
        let live_sources = sources_for(&conversation.history()[0]);
        assert_eq!(live_sources[0].url, "https://example.com/a");
        attempt.close(ResponseAttemptOutcome::Interrupted);
        conversation.update_response_attempt(attempt.clone());
        assert_eq!(conversation.history().len(), 1);
        assert!(
            selected_plain_text(&conversation.history()[0], 2)
                .unwrap()
                .contains("interrupted")
        );
        conversation.restore(vec![TranscriptItem::IncompleteResponse(Box::new(
            zevria_transcript::IncompleteResponse::from_attempt(&attempt)
                .unwrap()
                .unwrap(),
        ))]);
        assert_eq!(conversation.history().len(), 1);
        assert_eq!(sources_for(&conversation.history()[0]), live_sources);
        let replay = zevria_model::ProviderReplay::openai_responses(
            ModelProfileRef::new("p", "m"),
            items.to_vec(),
        );
        conversation.push_message(replay.to_message().unwrap(), ToolCallStatus::Finished);
        assert_eq!(sources_for(&conversation.history()[1]), live_sources);
        assert_eq!(
            selected_plain_text(&conversation.history()[1], 0).unwrap(),
            "Claim [Source](https://example.com/a)"
        );
    }

    #[test]
    fn session_metadata_and_directives_create_no_history_or_prompt_row() {
        let models = zevria_model::models::SessionModels::new(
            zevria_model::models::ModelSelection::new(
                zevria_foundation::ModelProfileRef::new("hidden-provider", "hidden-build"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            zevria_model::models::ModelSelection::new(
                zevria_foundation::ModelProfileRef::new("hidden-provider", "hidden-plan"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
        let mut hidden = vec![
            TranscriptItem::SessionModels(Box::new(models)),
            TranscriptItem::SessionMode(zevria_foundation::SessionMode::Build),
            TranscriptItem::Directive(Box::new(zevria_instructions::DirectiveContent::skill(
                &zevria_instructions::SkillSnapshot::new(
                    "hidden".parse().unwrap(),
                    "Hidden",
                    "HIDDEN_SKILL_BODY",
                )
                .unwrap(),
            ))),
            TranscriptItem::Directive(Box::new(
                zevria_instructions::DirectiveContent::new(
                    zevria_instructions::DirectivePayload::SkillRevocation {
                        name: "hidden".parse().unwrap(),
                        reason: "disabled".into(),
                    },
                )
                .unwrap(),
            )),
        ];
        let mut conversation = ConversationState::default();
        conversation.restore(hidden.clone());
        assert!(conversation.history().is_empty());
        hidden.push(TranscriptItem::Message(Message::user("visible")));
        conversation.restore(hidden);
        assert_eq!(conversation.history().len(), 1);
    }

    #[test]
    fn ensemble_worker_shape_cannot_drift() {
        let descriptor = AgentRunDescriptor {
            id: AgentRunId::from_string("agent"),
            agent: "codex".to_string(),
            label: "worker".to_string(),
            safe_mode: "read-only".to_string(),
        };
        let run_id = EnsembleRunId::from_string("run");
        let mut conversation = ConversationState::default();
        conversation.add_ensemble(
            EnsembleStart {
                run_id: run_id.clone(),
                workflow: EnsembleWorkflow::Review,
                prompt: "review".into(),
                agents: vec![descriptor.clone()],
            },
            AgentRunStatus::Queued,
            None,
        );
        conversation.update_ensemble_worker(
            &run_id,
            &descriptor.id,
            AgentRunStatus::Failed,
            Some("failed".to_string()),
        );
        let HistoryEntry::Ensemble(history) = conversation.history().last().unwrap() else {
            panic!("ensemble entry");
        };
        assert_eq!(history.workers.len(), 1);
        assert_eq!(history.workers[0].descriptor, descriptor);
        assert_eq!(history.workers[0].status, AgentRunStatus::Failed);
        assert_eq!(history.workers[0].failure.as_deref(), Some("failed"));
    }

    #[test]
    fn accepted_tail_replacement_reports_removed_runs() {
        let run_id = EnsembleRunId::from_string("discarded");
        let mut conversation = ConversationState::default();
        conversation.push_message(Message::user("before"), ToolCallStatus::Finished);
        conversation.add_ensemble(
            EnsembleStart {
                run_id: run_id.clone(),
                workflow: EnsembleWorkflow::Review,
                prompt: "review".into(),
                agents: Vec::new(),
            },
            AgentRunStatus::Queued,
            None,
        );
        let mut change = conversation.commit_edit(1, true);
        assert_eq!(change.invalidate_from(), Some(1));
        assert_eq!(change.take_removed_ensemble_runs(), vec![run_id]);
        assert!(matches!(
            conversation.history().last(),
            Some(HistoryEntry::CompactionDivider)
        ));
    }
}
