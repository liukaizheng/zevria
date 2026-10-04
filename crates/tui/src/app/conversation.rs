//! Committed conversation projection, correlation indexes, and selection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::interaction::SelectionScope;

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

pub(crate) use crate::presentation::{
    NativeToolCallStatus as ToolCallStatus, NativeToolState as ToolCallState,
};

fn call_ids_match(left: Option<&String>, right: Option<&String>, exact_call_id: bool) -> bool {
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

/// One coherent worker row in an ensemble entry.
#[derive(Debug, Clone)]
pub(crate) struct EnsembleWorkerHistory {
    pub(crate) descriptor: AgentRunDescriptor,
    pub(crate) status: AgentRunStatus,
    pub(crate) failure: Option<String>,
    /// Lower-authority summaries/outcomes cannot overwrite a review snapshot.
    review_authoritative: bool,
}

/// Root-pane representation of one ensemble command and its ordered worker
/// rows. Worker descriptor, status, and failure cannot drift by index.
#[derive(Debug, Clone)]
pub(crate) struct EnsembleHistory {
    pub(crate) header: Option<NativeHeader>,
    pub(crate) run_id: EnsembleRunId,
    pub(crate) workflow: EnsembleWorkflow,
    pub(crate) prompt: zevria_content::UserPrompt,
    pub(crate) workers: Vec<EnsembleWorkerHistory>,
    pub(crate) baseline: Option<AgentRunId>,
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

    pub(crate) fn command(&self) -> String {
        format!(
            "{} {}",
            self.workflow.slash_command(),
            self.prompt.display_projection()
        )
    }

    fn update_status(&mut self, id: &AgentRunId, status: AgentRunStatus, failure: Option<String>) {
        let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| &worker.descriptor.id == id)
        else {
            return;
        };
        if worker.review_authoritative || worker.status == AgentRunStatus::Abandoned {
            return;
        }
        worker.status = status;
        if failure.is_some() {
            worker.failure = failure;
        }
    }
}

/// A rendered item in the committed conversation pane.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum HistoryEntry {
    Conversation(ConversationEntry),
    PlanArtifact(PlanArtifact),
    PlanHandoff(PlanHandoff, Option<NativeHeader>),
    Ensemble(EnsembleHistory),
    CompactionDivider,
    Error(String),
}

impl HistoryEntry {
    /// Adapt one native message into the semantic blocks shared with ACP.
    pub(crate) fn from_message(message: Message, status: ToolCallStatus) -> Option<Self> {
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
            Message::Assistant { content, .. } => {
                for (index, item) in content.into_iter().enumerate() {
                    let kind = match item {
                        AssistantContent::Text(text) => PresentationBlockKind::Text {
                            text: zevria_content::citations::render_text(&text),
                            flavor: TextFlavor::Markdown,
                            editable: false,
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
                            let state = ToolCallState::new(status, &call.function.arguments);
                            PresentationBlockKind::Tool(PresentedTool::Native {
                                call: Box::new(call),
                                state: Box::new(state),
                            })
                        }
                        AssistantContent::Image(_) => {
                            PresentationBlockKind::Placeholder("[image]".to_string())
                        }
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
                },
            }),
        }
        (!blocks.is_empty()).then_some(Self::Conversation(ConversationEntry {
            header: None,
            blocks,
        }))
    }

    /// Numbered, user-originated prompt boundaries shared by folding and
    /// Normal-mode turn navigation. Metadata and worker rows are not prompts.
    pub(crate) fn has_prompt_header(&self) -> bool {
        let header = match self {
            Self::Conversation(entry) => entry.header,
            Self::Ensemble(entry) => entry.header,
            Self::PlanHandoff(_, header) => *header,
            _ => None,
        };
        matches!(header, Some(NativeHeader::Prompt(_)))
    }

    pub(crate) fn selectable_upper_bound(&self) -> usize {
        match self {
            Self::Conversation(entry) => entry.blocks.len(),
            Self::PlanArtifact(_) | Self::PlanHandoff(..) => 1,
            Self::Ensemble(ensemble) => ensemble.workers.len().saturating_add(1),
            Self::CompactionDivider => 0,
            Self::Error(_) => 1,
        }
    }

    pub(crate) fn message_fold_eligible(&self, diagnostics_visible: bool) -> bool {
        self.first_selectable_index(diagnostics_visible).is_some()
    }

    fn index_is_selectable(&self, index: usize, diagnostics_visible: bool) -> bool {
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

pub(crate) fn without_tool_calls(message: Message) -> Option<Message> {
    match message {
        Message::Assistant { id, content } => {
            let content = content
                .into_iter()
                .filter(|item| !matches!(item, AssistantContent::ToolCall(_)))
                .collect::<Vec<_>>();
            (!content.is_empty()).then_some(Message::Assistant { id, content })
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
            let mut text = format!(
                "{} ({}) · safe mode {} · {}",
                worker.descriptor.label,
                worker.descriptor.agent,
                worker.descriptor.safe_mode,
                worker.status
            );
            if let Some(failure) = &worker.failure {
                text.push_str(&format!("\n{failure}"));
            }
            Some(text)
        }
        HistoryEntry::Error(error) if content_index == 0 => Some(error.clone()),
        HistoryEntry::PlanArtifact(_)
        | HistoryEntry::PlanHandoff(..)
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
pub(crate) struct Selection {
    pub(crate) history_index: usize,
    pub(crate) content_index: usize,
}

/// A turn's displayed beginning, not its selectable body. Native prompts
/// (including special entries) use projection-scoped entry identity; grouped
/// ACP prompts retain their presentation block identity across reducer updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnStartTarget {
    pub(crate) epoch: crate::presentation::ProjectionEpoch,
    pub(crate) history_index: usize,
    pub(crate) block: Option<PresentationBlockId>,
}

/// Cross-domain consequences of a committed conversation mutation.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ConversationChange {
    invalidate_from: Option<usize>,
    clear_selection: bool,
    reset_viewport: bool,
    removed_ensemble_runs: Vec<EnsembleRunId>,
}

impl ConversationChange {
    pub(crate) fn replaced_from(index: usize) -> Self {
        Self {
            invalidate_from: Some(index),
            clear_selection: true,
            reset_viewport: false,
            removed_ensemble_runs: Vec::new(),
        }
    }

    pub(crate) fn projection_replaced() -> Self {
        Self {
            invalidate_from: Some(0),
            clear_selection: true,
            reset_viewport: true,
            removed_ensemble_runs: Vec::new(),
        }
    }

    pub(crate) fn invalidate_from(&self) -> Option<usize> {
        self.invalidate_from
    }

    pub(crate) const fn clears_selection(&self) -> bool {
        self.clear_selection
    }

    pub(crate) const fn resets_viewport(&self) -> bool {
        self.reset_viewport
    }

    pub(crate) fn take_removed_ensemble_runs(&mut self) -> Vec<EnsembleRunId> {
        std::mem::take(&mut self.removed_ensemble_runs)
    }

    pub(crate) fn merge(&mut self, mut other: Self) {
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
    attempt: zevria_content::WebSearchAttemptRecord,
    ids: HashMap<AttemptBlockKey, PresentationBlockId>,
    canonical: Option<Message>,
    acp_tools: HashMap<String, PresentationBlockKind>,
    tool_status: ToolCallStatus,
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
pub(crate) struct ConversationState {
    epoch: crate::presentation::ProjectionEpoch,
    block_ids: crate::presentation::BlockIdAllocator,
    history: Vec<HistoryEntry>,
    headers: Vec<HeaderObservation>,
    executing_tool_calls: HashMap<String, Vec<ToolCallLocation>>,
    hosted_attempts: HashMap<String, HostedAttempt>,
    identity_aliases: HashMap<(usize, PresentationBlockId), (usize, PresentationBlockId)>,
    pending_subtasks: HashMap<String, Vec<(usize, SubtaskDescriptor)>>,
    subtask_statuses: HashMap<SubtaskId, SubtaskStatus>,
}

impl ConversationState {
    pub(crate) fn epoch(&self) -> crate::presentation::ProjectionEpoch {
        self.epoch
    }

    pub(crate) fn allocate_turn(&mut self) -> DisplayTurn {
        let turn = DisplayTurn(
            self.headers
                .iter()
                .filter(|entry| matches!(entry.header, NativeHeader::Prompt(_)))
                .count()
                + 1,
        );
        self.observe_header(NativeHeader::Prompt(turn));
        turn
    }

    pub(crate) fn allocate_call(&mut self, turn: DisplayTurn) -> NativeHeader {
        let call = self.headers.iter().filter(|entry| {
            matches!(entry.header, NativeHeader::Assistant { turn: existing, .. } if existing == turn)
        }).count() + 1;
        let header = NativeHeader::Assistant { turn, call };
        self.observe_header(header);
        header
    }

    fn observe_header(&mut self, header: NativeHeader) {
        self.headers.push(HeaderObservation {
            history_index: self.history.len(),
            header,
            timing: None,
        });
    }

    pub(crate) fn start_header(&mut self, header: NativeHeader, now: Instant) {
        if matches!(header, NativeHeader::Assistant { .. })
            && let Some(observation) = self
                .headers
                .iter_mut()
                .rev()
                .find(|entry| entry.header == header)
        {
            // A duplicate observation must never reset a running or finished call.
            observation.timing.get_or_insert(HeaderTiming::Running(now));
        }
    }

    pub(crate) fn finish_header(&mut self, header: Option<NativeHeader>, now: Instant) {
        if let Some(header) = header
            && let Some(observation) = self
                .headers
                .iter_mut()
                .rev()
                .find(|entry| entry.header == header)
            && let Some(timing @ HeaderTiming::Running(_)) = &mut observation.timing
        {
            *timing = HeaderTiming::Finished(timing.elapsed(now));
        }
    }

    /// Unknown starts (including restored headers) do not acquire a zero duration.
    pub(crate) fn header_timings(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (NativeHeader, Duration)> + '_ {
        self.headers.iter().filter_map(move |observation| {
            observation
                .timing
                .as_ref()
                .map(|timing| (observation.header, timing.elapsed(now)))
        })
    }

    pub(crate) fn represents_header(&self, header: NativeHeader) -> bool {
        self.history.iter().any(|entry| {
            let HistoryEntry::Conversation(entry) = entry else {
                return false;
            };
            entry.header == Some(header)
                && entry.blocks.iter().any(|block| {
                    block.role == Some(PresentationRole::Assistant) && block.visible(false)
                })
        })
    }

    pub(crate) fn ensemble_turn(&self, run_id: &EnsembleRunId) -> Option<DisplayTurn> {
        self.history.iter().rev().find_map(|entry| match entry {
            HistoryEntry::Ensemble(ensemble) if &ensemble.run_id == run_id => {
                ensemble.header.map(NativeHeader::turn)
            }
            _ => None,
        })
    }

    pub(crate) fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub(crate) fn entry(&self, index: usize) -> Option<&HistoryEntry> {
        self.history.get(index)
    }

    pub(crate) fn turn_starts(&self, diagnostics_visible: bool) -> Vec<TurnStartTarget> {
        let mut targets = Vec::new();
        for (history_index, entry) in self.history.iter().enumerate() {
            let target = TurnStartTarget {
                epoch: self.epoch,
                history_index,
                block: None,
            };
            if entry.has_prompt_header() {
                targets.push(target);
            } else if let HistoryEntry::Conversation(conversation) = entry
                && conversation.header.is_none()
            {
                targets.extend(
                    entry
                        .user_message_blocks(diagnostics_visible)
                        .filter(|(index, start)| index == start)
                        .map(|(index, _)| TurnStartTarget {
                            block: Some(conversation.blocks[index].id),
                            ..target
                        }),
                );
            }
        }
        targets
    }

    pub(crate) fn reconcile_turn_start(
        &self,
        mut target: TurnStartTarget,
    ) -> Option<TurnStartTarget> {
        if target.epoch != self.epoch {
            return None;
        }
        if let Some(id) = target.block {
            let (history_index, id) = self.resolved_identity((target.history_index, id));
            target.history_index = history_index;
            target.block = Some(id);
            let entry = self.entry(history_index)?;
            let HistoryEntry::Conversation(conversation) = entry else {
                return None;
            };
            (conversation.header.is_none()
                && entry
                    .user_message_blocks(true)
                    .any(|(index, start)| index == start && conversation.blocks[index].id == id))
            .then_some(target)
        } else {
            self.entry(target.history_index)?
                .has_prompt_header()
                .then_some(target)
        }
    }

    pub(crate) fn entry_mut(&mut self, index: usize) -> Option<&mut HistoryEntry> {
        self.history.get_mut(index)
    }

    pub(crate) fn allocate_block_id(&mut self) -> PresentationBlockId {
        self.block_ids.allocate()
    }

    pub(crate) fn push_entry(&mut self, mut entry: HistoryEntry) -> usize {
        if let HistoryEntry::Conversation(conversation) = &mut entry {
            self.block_ids.identify(conversation);
        }
        let index = self.history.len();
        self.history.push(entry);
        index
    }

    pub(crate) fn replace_projection(
        &mut self,
        projection: ConversationState,
    ) -> ConversationChange {
        *self = projection;
        self.epoch = Default::default();
        ConversationChange::projection_replaced()
    }

    pub(crate) fn clear_projection(&mut self) -> ConversationChange {
        *self = Self::default();
        ConversationChange::projection_replaced()
    }

    pub(crate) fn contains_presented_error(&self, error: &str) -> bool {
        self.history.iter().any(|entry| {
            let HistoryEntry::Conversation(entry) = entry else {
                return false;
            };
            entry.blocks.iter().any(|block| {
                matches!(&block.kind, PresentationBlockKind::Error(existing) if existing == error)
            })
        })
    }

    pub(crate) fn update_web_search(&mut self, attempt: zevria_content::WebSearchAttemptRecord) {
        self.update_attempt(attempt, false);
    }

    pub(crate) fn update_native_web_search(
        &mut self,
        attempt: zevria_content::WebSearchAttemptRecord,
        header: Option<NativeHeader>,
    ) {
        let id = attempt.id.clone();
        let existing = self.hosted_attempts.contains_key(&id);
        self.update_web_search(attempt);
        if !existing {
            self.bind_attempt_header(&id, header);
        }
    }

    fn bind_attempt_header(&mut self, id: &str, header: Option<NativeHeader>) {
        if let Some(attempt) = self.hosted_attempts.get(id)
            && let HistoryEntry::Conversation(entry) = &mut self.history[attempt.history_index]
        {
            // Later dispatches and activity revisions cannot relabel earlier output.
            entry.header = entry.header.or(header);
        }
    }

    /// The ACP reducer validates the immutable source revision before filtering
    /// its text to verified ordinary projections. Later binding coverage may
    /// enrich that filtered presentation without changing the source revision.
    pub(crate) fn update_projected_web_search(
        &mut self,
        attempt: zevria_content::WebSearchAttemptRecord,
    ) {
        self.update_attempt(attempt, true);
    }

    fn update_attempt(&mut self, attempt: zevria_content::WebSearchAttemptRecord, projected: bool) {
        if attempt.validate().is_err() || !attempt.has_display() {
            return;
        }
        let id = attempt.id.clone();
        if let Some(existing) = self.hosted_attempts.get_mut(&id) {
            if attempt.revision < existing.attempt.revision
                || (!projected && attempt.revision == existing.attempt.revision)
                || attempt == existing.attempt
            {
                return;
            }
            existing.attempt = attempt;
        } else {
            let history_index = self.push_entry(HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: Vec::new(),
            }));
            self.hosted_attempts.insert(
                id.clone(),
                HostedAttempt {
                    history_index,
                    attempt,
                    ids: HashMap::new(),
                    canonical: None,
                    acp_tools: HashMap::new(),
                    tool_status: ToolCallStatus::Finished,
                },
            );
        }
        self.reconcile_attempt(&id);
    }

    pub(crate) fn commit_response(
        &mut self,
        message: Message,
        id: Option<&str>,
        status: ToolCallStatus,
        header: Option<NativeHeader>,
    ) {
        if let Some(id) = id {
            self.bind_attempt_header(id, header);
        }
        if let Some(id) = id
            && let Some(attempt) = self.hosted_attempts.get_mut(id)
        {
            attempt.canonical = Some(message);
            attempt.tool_status = status;
            self.reconcile_attempt(id);
        } else {
            self.push_numbered_message(message, status, header);
        }
        self.attach_pending_subtasks();
    }

    fn reconcile_attempt(&mut self, id: &str) {
        use crate::presentation::WebActivityPresentation;
        use zevria_content::AssistantPresentationContent as Content;
        let state = self.hosted_attempts.get_mut(id).expect("known attempt");
        let attempt = &state.attempt;
        let mut desired = Vec::new();
        let mut ordered = std::collections::BTreeMap::new();
        for part in &attempt.presentation {
            ordered.insert(
                (part.source.output_index, Some(part.source.part)),
                Some(part),
            );
        }
        for action in &attempt.activity {
            ordered.insert((action.output_index, None), None);
        }
        let mut pending = Vec::new();
        let flush = |pending: &mut Vec<&zevria_content::WebSearchActivity>,
                     desired: &mut Vec<_>| {
            if let Some(first) = pending.first() {
                desired.push((
                    AttemptBlockKey::Action(first.output_index),
                    PresentationBlockKind::WebActivity(WebActivityPresentation::from_actions(
                        attempt, pending,
                    )),
                ));
                pending.clear();
            }
        };
        for ((output, _), part) in ordered {
            let Some(part) = part else {
                let action = attempt
                    .activity
                    .iter()
                    .find(|action| action.output_index == output)
                    .expect("indexed action");
                if action.details().is_empty() {
                    pending.push(action);
                } else {
                    flush(&mut pending, &mut desired);
                    desired.push((
                        AttemptBlockKey::Action(output),
                        PresentationBlockKind::WebActivity(WebActivityPresentation::from_actions(
                            attempt,
                            &[action],
                        )),
                    ));
                }
                continue;
            };
            let kind = match &part.content {
                Content::Reasoning { text } if !text.trim().is_empty() => {
                    PresentationBlockKind::Reasoning {
                        parts: vec![zevria_content::web_search::sanitize_readable(text)],
                    }
                }
                Content::Answer { text } if !text.trim().is_empty() => {
                    PresentationBlockKind::Text {
                        text: zevria_content::web_search::sanitize_readable(text),
                        flavor: TextFlavor::Markdown,
                        editable: false,
                    }
                }
                Content::NativeTool { call_id } => {
                    let call = state.canonical.as_ref().and_then(|message| match message {
                        Message::Assistant { content, .. } => {
                            content.iter().find_map(|content| match content {
                                AssistantContent::ToolCall(call) if call.id == call_id.as_str() => {
                                    Some(call)
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    });
                    if let Some(call) = call {
                        PresentationBlockKind::Tool(PresentedTool::Native {
                            call: Box::new(call.clone()),
                            state: Box::new(ToolCallState::new(
                                state.tool_status,
                                &call.function.arguments,
                            )),
                        })
                    } else if let Some(tool) = state.acp_tools.get(call_id) {
                        tool.clone()
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            flush(&mut pending, &mut desired);
            desired.push((AttemptBlockKey::Part(output, part.source.part), kind));
        }
        flush(&mut pending, &mut desired);
        let incomplete = match attempt.outcome {
            zevria_content::WebSearchAttemptOutcome::Failed => Some("failed"),
            zevria_content::WebSearchAttemptOutcome::Interrupted => Some("interrupted"),
            _ => None,
        };
        if let Some(outcome) = incomplete {
            desired.push((
                AttemptBlockKey::Incomplete,
                PresentationBlockKind::Placeholder(format!("Incomplete response · {outcome}")),
            ));
        }
        let HistoryEntry::Conversation(entry) = &mut self.history[state.history_index] else {
            return;
        };
        let old_blocks = std::mem::take(&mut entry.blocks);
        let insertion = old_blocks
            .iter()
            .position(|block| state.ids.values().any(|id| *id == block.id))
            .unwrap_or(old_blocks.len());
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut previous = HashMap::new();
        for (index, block) in old_blocks.into_iter().enumerate() {
            if state.ids.values().any(|id| *id == block.id) {
                previous.insert(block.id, block);
            } else if index < insertion {
                before.push(block);
            } else {
                after.push(block);
            }
        }
        entry.blocks = before;
        for (key, kind) in desired {
            let block_id = *state
                .ids
                .entry(key)
                .or_insert_with(|| self.block_ids.allocate());
            let mut block = PresentationBlock {
                id: block_id,
                revision: 0,
                role: Some(PresentationRole::Assistant),
                prompt_group: None,
                prompt: None,
                visibility: BlockVisibility::Always,
                kind,
            };
            if let Some(old) = previous.remove(&block_id) {
                if old.native_tool().is_some() && block.native_tool().is_some() {
                    // A result's state belongs to the semantic call, never its old index.
                    block = old;
                } else {
                    block.revision = old.revision;
                    if old.kind != block.kind {
                        block.touch();
                    }
                }
            }
            entry.blocks.push(block);
        }
        entry.blocks.extend(after);
        let launch_blocks: Vec<_> = entry
            .blocks
            .iter()
            .filter_map(|block| {
                block
                    .native_tool()
                    .filter(|(_, tool)| !tool.subtasks.is_empty())
                    .map(|_| block.id)
            })
            .collect();
        for parent in launch_blocks {
            sync_subtask_rows(entry, parent, &mut self.block_ids);
        }
        self.executing_tool_calls
            .values_mut()
            .for_each(|locations| {
                locations.retain(|location| location.history_index != state.history_index)
            });
        self.executing_tool_calls
            .retain(|_, locations| !locations.is_empty());
        for block in &entry.blocks {
            if let Some((call, tool)) = block.native_tool()
                && tool.status == ToolCallStatus::Executing
            {
                self.executing_tool_calls
                    .entry(call.id.to_string())
                    .or_default()
                    .push(ToolCallLocation {
                        history_index: state.history_index,
                        block_id: block.id,
                    });
            }
        }
    }

    pub(crate) fn bind_attempt_tools(
        &mut self,
        id: &str,
        tools: HashMap<String, PresentationBlockKind>,
    ) {
        if let Some(attempt) = self.hosted_attempts.get_mut(id) {
            attempt.acp_tools = tools;
            self.reconcile_attempt(id);
        }
    }

    /// Place an ACP attempt at the first explicitly covered projection, keeping
    /// unrelated blocks and all block IDs stable. Subsequent updates reconcile
    /// only this attempt's owned blocks inside the shared entry.
    pub(crate) fn place_attempt(
        &mut self,
        id: &str,
        history_index: usize,
        before: PresentationBlockId,
    ) {
        let Some(state) = self.hosted_attempts.get_mut(id) else {
            return;
        };
        if state.history_index == history_index {
            return;
        }
        let Some(HistoryEntry::Conversation(source)) = self.history.get_mut(state.history_index)
        else {
            return;
        };
        let mut blocks = Vec::new();
        source.blocks.retain(|block| {
            if state.ids.values().any(|id| *id == block.id) {
                blocks.push(block.clone());
                false
            } else {
                true
            }
        });
        let Some(HistoryEntry::Conversation(target)) = self.history.get_mut(history_index) else {
            return;
        };
        let position = target
            .blocks
            .iter()
            .position(|block| block.id == before)
            .unwrap_or(target.blocks.len());
        target.blocks.splice(position..position, blocks);
        state.history_index = history_index;
    }

    pub(crate) fn selection_identity(&self, selection: Selection) -> Option<PresentationBlockId> {
        let HistoryEntry::Conversation(entry) = self.entry(selection.history_index)? else {
            return None;
        };
        Some(entry.blocks.get(selection.content_index)?.id)
    }

    pub(crate) fn resolved_identity(
        &self,
        mut identity: (usize, PresentationBlockId),
    ) -> (usize, PresentationBlockId) {
        for _ in 0..8 {
            if let Some(next) = self.identity_aliases.get(&identity) {
                identity = *next;
            } else {
                break;
            }
        }
        identity
    }

    pub(crate) fn alias_attempt_action(
        &mut self,
        old: (usize, PresentationBlockId),
        attempt_id: &str,
        output: u64,
    ) {
        if let Some(attempt) = self.hosted_attempts.get(attempt_id)
            && let Some(id) = attempt.ids.get(&AttemptBlockKey::Action(output))
        {
            self.identity_aliases
                .insert(old, (attempt.history_index, *id));
        }
    }

    pub(crate) fn alias_attempt_part(
        &mut self,
        old: (usize, PresentationBlockId),
        attempt_id: &str,
        source: &zevria_content::AssistantSourceAddress,
    ) {
        if let Some(attempt) = self.hosted_attempts.get(attempt_id)
            && let Some(id) = attempt
                .ids
                .get(&AttemptBlockKey::Part(source.output_index, source.part))
        {
            self.identity_aliases
                .insert(old, (attempt.history_index, *id));
        }
    }

    pub(crate) fn selection_for_identity(
        &self,
        history_index: usize,
        id: PresentationBlockId,
    ) -> Option<Selection> {
        let (history_index, id) = self.resolved_identity((history_index, id));
        let HistoryEntry::Conversation(entry) = self.entry(history_index)? else {
            return None;
        };
        entry
            .blocks
            .iter()
            .position(|block| block.id == id)
            .map(|content_index| Selection {
                history_index,
                content_index,
            })
    }

    pub(crate) fn push_error(&mut self, error: String) {
        self.history.push(HistoryEntry::Error(error));
    }

    pub(crate) fn push_plan_artifact(&mut self, artifact: PlanArtifact) {
        let visible = self.history.iter().any(|entry| {
            matches!(entry, HistoryEntry::PlanArtifact(existing) if existing.version == artifact.version)
        });
        if !visible {
            self.history.push(HistoryEntry::PlanArtifact(artifact));
        }
    }

    pub(crate) fn push_plan_handoff(&mut self, handoff: PlanHandoff, turn: DisplayTurn) {
        self.history.push(HistoryEntry::PlanHandoff(
            handoff,
            Some(NativeHeader::Prompt(turn)),
        ));
    }

    pub(crate) fn push_compaction_divider(&mut self) {
        self.history.push(HistoryEntry::CompactionDivider);
    }

    pub(crate) fn push_user_turn(&mut self, message: Message, turn: DisplayTurn) {
        if let Some(HistoryEntry::Conversation(mut entry)) =
            HistoryEntry::from_message(message, ToolCallStatus::Finished)
        {
            entry.header = Some(NativeHeader::Prompt(turn));
            self.push_entry(HistoryEntry::Conversation(entry));
        }
    }

    pub(crate) fn push_message(&mut self, message: Message, status: ToolCallStatus) {
        self.push_numbered_message(message, status, None);
    }

    fn push_numbered_message(
        &mut self,
        message: Message,
        status: ToolCallStatus,
        header: Option<NativeHeader>,
    ) {
        let Some(mut entry) = HistoryEntry::from_message(message, status) else {
            return;
        };
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
    }

    pub(crate) fn has_executing_tool_calls(&self) -> bool {
        !self.executing_tool_calls.is_empty()
    }

    pub(crate) fn header_has_executing_tool_calls(&self, header: NativeHeader) -> bool {
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
            .iter()
            .find(|block| block.id == location.block_id)?
            .native_tool()
            .map(|(call, _)| call)
    }

    fn select_executing_call(&self, result: &ToolResult) -> Option<ToolCallLocation> {
        let candidates = self.executing_tool_calls.get(result.call.as_str())?;
        [true, false].into_iter().find_map(|exact_call_id| {
            candidates
                .iter()
                .filter(|location| {
                    self.tool_call_at(**location).is_some_and(|call| {
                        call_ids_match(
                            provider_call_id(call.provider.as_ref()),
                            provider_call_id(result.provider.as_ref()),
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
        if let Some(candidates) = self.executing_tool_calls.get_mut(result.call.as_str()) {
            candidates.retain(|candidate| *candidate != location);
            if candidates.is_empty() {
                self.executing_tool_calls.remove(result.call.as_str());
            }
        }
        let Some(HistoryEntry::Conversation(entry)) = self.history.get_mut(location.history_index)
        else {
            return;
        };
        let Some(block) = entry
            .blocks
            .iter_mut()
            .find(|block| block.id == location.block_id)
        else {
            return;
        };
        let Some((call, state)) = block.native_tool_mut() else {
            return;
        };
        state.status = ToolCallStatus::Finished;
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
        block.touch();
        sync_subtask_rows(entry, location.block_id, &mut self.block_ids);
    }

    /// Return only headers whose last executing tool was settled by these results.
    /// A late or duplicate result must not stop a newer model call's clock.
    pub(crate) fn finish_tool_results(
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
                let candidates = metadata_by_id.get_mut(result.call.as_str())?;
                let position = candidates.iter().position(|&index| {
                    call_ids_match(
                        metadata[index].call_id.as_ref(),
                        provider_call_id(result.provider.as_ref()),
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

    pub(crate) fn interrupt_executing_tool_calls(&mut self) {
        for locations in std::mem::take(&mut self.executing_tool_calls).into_values() {
            for location in locations {
                let Some(HistoryEntry::Conversation(entry)) =
                    self.history.get_mut(location.history_index)
                else {
                    continue;
                };
                if let Some(block) = entry
                    .blocks
                    .iter_mut()
                    .find(|block| block.id == location.block_id)
                    && let Some((_, state)) = block.native_tool_mut()
                {
                    state.status = ToolCallStatus::Interrupted;
                    block.touch();
                }
            }
        }
    }

    pub(crate) fn attach_subtask_launch(
        &mut self,
        call_id: &str,
        entry_index: usize,
        mut descriptor: SubtaskDescriptor,
    ) {
        if let Some(status) = self.subtask_statuses.get(&descriptor.id) {
            descriptor.status = *status;
        }
        for entry in self.history.iter_mut().rev() {
            let HistoryEntry::Conversation(entry) = entry else {
                continue;
            };
            for block in &mut entry.blocks {
                let Some((call, state)) = block.native_tool_mut() else {
                    continue;
                };
                if call.id != call_id
                    || call.function.name != zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME
                {
                    continue;
                }
                if let Some(existing) = state.subtasks.get(&entry_index) {
                    if existing.id != descriptor.id {
                        return;
                    }
                    if existing.status.is_terminal() || descriptor.status == SubtaskStatus::Starting
                    {
                        descriptor.status = existing.status;
                    }
                }
                state.subtasks.insert(entry_index, descriptor);
                let parent = block.id;
                block.touch();
                sync_subtask_rows(entry, parent, &mut self.block_ids);
                return;
            }
        }
        let pending = self.pending_subtasks.entry(call_id.into()).or_default();
        pending.retain(|(index, _)| *index != entry_index);
        pending.push((entry_index, descriptor));
    }

    /// Keep status-before-launch observations; never regress terminal children
    /// when delayed Starting/Running announcements arrive.
    pub(crate) fn update_subtask_status(&mut self, id: &SubtaskId, status: SubtaskStatus) {
        let current = self.subtask_statuses.entry(id.clone()).or_insert(status);
        if !current.is_terminal() && status != SubtaskStatus::Starting {
            *current = status;
        }
        let status = *current;
        for entry in self.history.iter_mut().rev() {
            let HistoryEntry::Conversation(entry) = entry else {
                continue;
            };
            let mut changed = Vec::new();
            for block in &mut entry.blocks {
                let Some((_, state)) = block.native_tool_mut() else {
                    continue;
                };
                if let Some(descriptor) = state
                    .subtasks
                    .values_mut()
                    .find(|descriptor| &descriptor.id == id)
                {
                    if !descriptor.status.is_terminal() {
                        descriptor.status = status;
                    }
                    changed.push(block.id);
                    block.touch();
                }
            }
            for parent in changed {
                sync_subtask_rows(entry, parent, &mut self.block_ids);
            }
        }
    }

    fn attach_pending_subtasks(&mut self) {
        for (call, entries) in std::mem::take(&mut self.pending_subtasks) {
            for (index, descriptor) in entries {
                self.attach_subtask_launch(&call, index, descriptor);
            }
        }
    }

    pub(crate) fn add_ensemble(
        &mut self,
        start: EnsembleStart,
        status: AgentRunStatus,
        header: Option<NativeHeader>,
    ) {
        self.history
            .push(HistoryEntry::Ensemble(EnsembleHistory::new(
                start, status, header,
            )));
    }

    pub(crate) fn has_ensemble(&self, run_id: &EnsembleRunId) -> bool {
        self.history.iter().rev().any(
            |entry| matches!(entry, HistoryEntry::Ensemble(ensemble) if &ensemble.run_id == run_id),
        )
    }

    fn ensemble_mut(&mut self, run_id: &EnsembleRunId) -> Option<&mut EnsembleHistory> {
        self.history.iter_mut().rev().find_map(|entry| match entry {
            HistoryEntry::Ensemble(ensemble) if &ensemble.run_id == run_id => Some(ensemble),
            _ => None,
        })
    }

    pub(crate) fn ensemble_baseline(&self, run_id: &EnsembleRunId) -> Option<&AgentRunId> {
        self.history.iter().rev().find_map(|entry| match entry {
            HistoryEntry::Ensemble(ensemble) if &ensemble.run_id == run_id => {
                ensemble.baseline.as_ref()
            }
            _ => None,
        })
    }

    pub(crate) fn update_ensemble_baseline(
        &mut self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
        selected: bool,
    ) {
        if let Some(ensemble) = self.ensemble_mut(run_id) {
            if ensemble.workers.iter().any(|worker| {
                worker.review_authoritative
                    && (&worker.descriptor.id == worker_id
                        || ensemble.baseline.as_ref() == Some(&worker.descriptor.id))
            }) {
                return;
            }
            if selected
                && ensemble.workflow == EnsembleWorkflow::Plan
                && ensemble.workers.iter().any(|worker| {
                    &worker.descriptor.id == worker_id && worker.status != AgentRunStatus::Abandoned
                })
            {
                ensemble.baseline = Some(worker_id.clone());
            } else if ensemble.baseline.as_ref() == Some(worker_id) {
                ensemble.baseline = None;
            }
        }
    }

    pub(crate) fn update_ensemble_worker(
        &mut self,
        run_id: &EnsembleRunId,
        agent_run_id: &AgentRunId,
        status: AgentRunStatus,
        failure: Option<String>,
    ) {
        if let Some(ensemble) = self.ensemble_mut(run_id) {
            ensemble.update_status(agent_run_id, status, failure);
            if status == AgentRunStatus::Abandoned
                && ensemble.baseline.as_ref() == Some(agent_run_id)
            {
                ensemble.baseline = None;
            }
        }
    }

    pub(crate) fn project_worker_completion(
        &mut self,
        run_id: &EnsembleRunId,
        completion: crate::projection::WorkerCompletion,
    ) {
        let crate::projection::WorkerCompletion {
            worker_id,
            mut status,
            partial,
            has_plan_proof,
            baseline,
            mut failure,
        } = completion;
        self.update_ensemble_baseline(run_id, &worker_id, baseline);
        let Some(ensemble) = self.ensemble_mut(run_id) else {
            return;
        };
        if status == AgentRunStatus::Completed
            && (partial || (ensemble.workflow == EnsembleWorkflow::Plan && !has_plan_proof))
        {
            status = AgentRunStatus::Failed;
            failure.get_or_insert_with(|| {
                if partial {
                    "worker reported an invalid partial Completed outcome".to_string()
                } else {
                    "Plan worker reported Completed without final Markdown proof".to_string()
                }
            });
        }
        ensemble.update_status(&worker_id, status, failure);
    }

    pub(crate) fn project_worker_review(
        &mut self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
        state: &zevria_workflow::WorkerReviewState,
        mode: crate::projection::ProjectionMode,
    ) {
        if &state.descriptor.id != worker_id {
            return;
        }
        let Some(ensemble) = self.ensemble_mut(run_id) else {
            return;
        };
        if ensemble.workflow != EnsembleWorkflow::Plan {
            return;
        }
        let Some(worker) = ensemble
            .workers
            .iter_mut()
            .find(|worker| &worker.descriptor.id == worker_id)
        else {
            return;
        };
        if worker.descriptor != state.descriptor {
            return;
        }
        let confirmed = state.confirmed_plan();
        if state.confirmation.is_some()
            && confirmed
                .as_ref()
                .is_none_or(|plan| &plan.receipt.target.run_id != run_id)
        {
            return;
        }
        worker.review_authoritative = true;
        worker.status = state.status();
        if mode == crate::projection::ProjectionMode::Restored
            && matches!(
                worker.status,
                AgentRunStatus::Running
                    | AgentRunStatus::Queued
                    | AgentRunStatus::Starting
                    | AgentRunStatus::Resuming
            )
        {
            worker.status = AgentRunStatus::Interrupted;
        }
        worker.failure.clone_from(&state.diagnostic);
        if confirmed.is_some_and(|plan| plan.baseline.is_some()) && !state.abandoned {
            ensemble.baseline = Some(worker_id.clone());
        } else if ensemble.baseline.as_ref() == Some(worker_id) {
            ensemble.baseline = None;
        }
    }

    pub(crate) fn reviewed_worker_status(
        &self,
        run_id: &EnsembleRunId,
        worker_id: &AgentRunId,
    ) -> Option<AgentRunStatus> {
        self.history.iter().find_map(|entry| {
            let HistoryEntry::Ensemble(ensemble) = entry else {
                return None;
            };
            if &ensemble.run_id != run_id {
                return None;
            }
            ensemble
                .workers
                .iter()
                .find(|worker| &worker.descriptor.id == worker_id && worker.review_authoritative)
                .map(|worker| worker.status)
        })
    }

    pub(crate) fn update_unfinished_ensemble_workers(
        &mut self,
        run_id: &EnsembleRunId,
        status: AgentRunStatus,
    ) {
        let Some(ensemble) = self.ensemble_mut(run_id) else {
            return;
        };
        for worker in &mut ensemble.workers {
            if !worker.review_authoritative && !worker.status.is_terminal() {
                worker.status = status;
            }
        }
    }

    pub(crate) fn commit_edit(
        &mut self,
        target_index: usize,
        compacted: bool,
    ) -> ConversationChange {
        let anchor = self
            .history
            .get(target_index)
            .and_then(|entry| match entry {
                HistoryEntry::Conversation(entry) => entry.header,
                HistoryEntry::Ensemble(entry) => entry.header,
                HistoryEntry::PlanHandoff(_, header) => *header,
                _ => None,
            })
            .filter(|header| matches!(header, NativeHeader::Prompt(_)));
        let cutoff = anchor
            .and_then(|header| self.headers.iter().position(|entry| entry.header == header))
            .or_else(|| {
                self.headers
                    .iter()
                    .position(|entry| entry.history_index >= target_index)
            });
        if let Some(cutoff) = cutoff {
            self.headers.truncate(cutoff);
        }
        let removed = self.history.split_off(target_index.min(self.history.len()));
        let mut change = ConversationChange::replaced_from(target_index);
        change.removed_ensemble_runs = removed
            .into_iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Ensemble(ensemble) => Some(ensemble.run_id),
                HistoryEntry::Conversation(_)
                | HistoryEntry::PlanArtifact(_)
                | HistoryEntry::PlanHandoff(..)
                | HistoryEntry::CompactionDivider
                | HistoryEntry::Error(_) => None,
            })
            .collect();
        if compacted {
            self.history.push(HistoryEntry::CompactionDivider);
        }
        self.executing_tool_calls.clear();
        self.hosted_attempts.clear();
        self.identity_aliases.clear();
        self.pending_subtasks.clear();
        self.subtask_statuses.clear();
        change
    }

    pub(crate) fn message_recall_selection(&self, history_index: usize) -> Option<Selection> {
        let content_index = match self.history.get(history_index)? {
            HistoryEntry::Conversation(entry) => entry
                .blocks
                .iter()
                .position(PresentationBlock::is_editable)?,
            HistoryEntry::Ensemble(_) => 0,
            _ => return None,
        };
        Some(Selection {
            history_index,
            content_index,
        })
    }

    pub(crate) fn recallable_selection(
        &self,
        selection: Selection,
    ) -> Option<(zevria_content::UserPrompt, TranscriptEditTarget)> {
        match self.history.get(selection.history_index)? {
            HistoryEntry::Conversation(entry) => {
                if !entry.blocks.get(selection.content_index)?.is_editable() {
                    return None;
                }
                let blocks = entry
                    .blocks
                    .iter()
                    .map(|block| match &block.kind {
                        PresentationBlockKind::Text {
                            text,
                            editable: true,
                            ..
                        } => Some(zevria_content::PromptBlock::Text(text.clone())),
                        PresentationBlockKind::Image {
                            image,
                            editable: true,
                            ..
                        } => Some(zevria_content::PromptBlock::Image(image.clone())),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                let text = zevria_content::UserPrompt::new(blocks).ok()?;
                Some((
                    text,
                    TranscriptEditTarget::PromptOrdinal(
                        self.prompt_ordinal(selection.history_index),
                    ),
                ))
            }
            HistoryEntry::Ensemble(ensemble) if selection.content_index == 0 => Some((
                ensemble
                    .prompt
                    .with_prefix(format!("{} ", ensemble.workflow.slash_command())),
                TranscriptEditTarget::EnsembleRun(ensemble.run_id.clone()),
            )),
            HistoryEntry::PlanArtifact(_)
            | HistoryEntry::PlanHandoff(..)
            | HistoryEntry::Ensemble(_)
            | HistoryEntry::CompactionDivider
            | HistoryEntry::Error(_) => None,
        }
    }

    fn prompt_ordinal(&self, history_index: usize) -> usize {
        self.history[..history_index.min(self.history.len())]
            .iter()
            .filter(|entry| match entry {
                HistoryEntry::Conversation(entry) => {
                    entry.blocks.iter().any(PresentationBlock::is_editable)
                }
                _ => false,
            })
            .count()
    }

    pub(crate) fn selected_plain_text(&self, selection: Selection) -> Option<String> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_plain_text(entry, selection.content_index))
    }

    pub(crate) fn selected_message_text(
        &self,
        history_index: usize,
        diagnostics_visible: bool,
    ) -> Option<String> {
        let entry = self.history.get(history_index)?;
        let items = (0..entry.selectable_upper_bound())
            .filter(|&index| entry.index_is_selectable(index, diagnostics_visible))
            .filter_map(|index| selected_plain_text(entry, index))
            .collect::<Vec<_>>();
        (!items.is_empty()).then(|| items.join("\n\n"))
    }

    pub(crate) fn selected_readable_list(&self, selection: Selection) -> Option<String> {
        let HistoryEntry::Conversation(entry) = self.history.get(selection.history_index)? else {
            return None;
        };
        entry
            .blocks
            .get(selection.content_index)?
            .readable_list_copy()
    }

    pub(crate) fn selected_tool_output(&self, selection: Selection) -> Option<String> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_tool_output(entry, selection.content_index))
    }

    pub(crate) fn selection_is_tool(&self, selection: Selection) -> bool {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_tool(entry, selection.content_index))
            .is_some()
    }

    pub(crate) fn selected_subtask_id(&self, selection: Selection) -> Option<SubtaskId> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_subtask_id(entry, selection.content_index))
    }

    pub(crate) fn selected_agent_run_id(&self, selection: Selection) -> Option<AgentRunId> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_agent_run_id(entry, selection.content_index))
    }

    pub(crate) fn next_entry_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        if let Some((history_index, content_index)) = self
            .history
            .iter()
            .enumerate()
            .skip(selection.history_index.saturating_add(1))
            .find_map(|(history_index, entry)| {
                entry
                    .first_selectable_index(diagnostics_visible)
                    .map(|content_index| (history_index, content_index))
            })
        {
            return Selection {
                history_index,
                content_index,
            };
        }
        selection
    }

    pub(crate) fn previous_entry_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        let Some((history_index, content_index)) = self
            .history
            .iter()
            .enumerate()
            .take(selection.history_index)
            .rev()
            .find_map(|(history_index, entry)| {
                entry
                    .first_selectable_index(diagnostics_visible)
                    .map(|content_index| (history_index, content_index))
            })
        else {
            return selection;
        };
        Selection {
            history_index,
            content_index,
        }
    }

    pub(crate) fn next_block_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        self.history
            .get(selection.history_index)
            .and_then(|entry| {
                entry.next_selectable_index(selection.content_index, diagnostics_visible)
            })
            .map_or(selection, |content_index| Selection {
                content_index,
                ..selection
            })
    }

    pub(crate) fn previous_block_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        self.history
            .get(selection.history_index)
            .and_then(|entry| {
                entry.previous_selectable_index(selection.content_index, diagnostics_visible)
            })
            .map_or(selection, |content_index| Selection {
                content_index,
                ..selection
            })
    }

    pub(crate) fn next_user_entry(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let mut target = selection;
        loop {
            target = self.next_user_message(target, diagnostics_visible)?;
            if target.history_index != selection.history_index {
                return Some(target);
            }
        }
    }

    pub(crate) fn previous_user_entry(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let mut target = selection;
        loop {
            target = self.previous_user_message(target, diagnostics_visible)?;
            if target.history_index != selection.history_index {
                return Some(target);
            }
        }
    }

    pub(crate) fn next_user_message(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        self.history
            .iter()
            .enumerate()
            .skip(selection.history_index)
            .find_map(|(history_index, entry)| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .find(|&(_, start)| {
                        history_index > selection.history_index || start > selection.content_index
                    })
                    .map(|(_, content_index)| Selection {
                        history_index,
                        content_index,
                    })
            })
    }

    pub(crate) fn previous_user_message(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        // A selection on any user block excludes that whole message. Other
        // items (including diagnostics) search from their own coordinates.
        let before = self
            .history
            .get(selection.history_index)
            .and_then(|entry| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .find(|&(index, _)| index == selection.content_index)
            })
            .map_or(selection.content_index, |(_, start)| start);
        self.history
            .iter()
            .enumerate()
            .take(selection.history_index.saturating_add(1))
            .rev()
            .find_map(|(history_index, entry)| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .filter(|&(_, start)| history_index < selection.history_index || start < before)
                    .last()
                    .map(|(_, content_index)| Selection {
                        history_index,
                        content_index,
                    })
            })
    }

    pub(crate) fn reconcile_selection(
        &self,
        selection: Selection,
        scope: SelectionScope,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let entry = self.history.get(selection.history_index)?;
        if entry.index_is_selectable(selection.content_index, diagnostics_visible) {
            Some(selection)
        } else if scope == SelectionScope::Message {
            entry
                .first_selectable_index(diagnostics_visible)
                .map(|content_index| Selection {
                    content_index,
                    ..selection
                })
        } else {
            None
        }
    }

    /// Rebuild the pane from persisted transcript items.
    pub(crate) fn restore(&mut self, items: Vec<TranscriptItem>) {
        let review_projection = zevria_transcript::project_worker_reviews(&items);
        // Started seeds projection state but is not review evidence. Only actual
        // review transitions/seals may supersede outcome-only/ReportsReady data.
        let reviewed: std::collections::HashSet<_> = items
            .iter()
            .flat_map(|item| match item {
                TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
                    run_id,
                    worker_id,
                    ..
                }) => vec![(run_id.clone(), worker_id.clone())],
                TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
                    run_id,
                    outcomes,
                    ..
                }) => outcomes
                    .iter()
                    .map(|outcome| (run_id.clone(), outcome.descriptor.id.clone()))
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        *self = Self::default();
        let reconstructed = zevria_transcript::reconstruct_transcript(&items);
        // A binding is authoritative even if its activity was flushed after
        // the canonical response. Seed that entry at the response boundary,
        // rather than guessing an association from neighboring records.
        let mut saved_attempts = HashMap::new();
        for item in &reconstructed {
            if let TranscriptItem::WebSearchAttempt(attempt) = item {
                let saved = saved_attempts.entry(attempt.id.clone()).or_insert(attempt);
                if attempt.revision > saved.revision {
                    *saved = attempt;
                }
            }
        }
        let mut current_turn = None;
        for item in reconstructed.iter().cloned() {
            match item {
                TranscriptItem::WebSearchAttempt(attempt) => self.update_web_search(attempt),
                provider @ (TranscriptItem::Message(_)
                | TranscriptItem::RequestPrompt { .. }
                | TranscriptItem::ProviderMessage(_)
                | TranscriptItem::AssistantMessage { .. }) => {
                    let message = provider
                        .display_message()
                        .expect("a provider transcript item must contain a message");
                    let header = match &message {
                        Message::User { content }
                            if matches!(
                                provider,
                                TranscriptItem::Message(_) | TranscriptItem::RequestPrompt { .. }
                            ) && content
                                .iter()
                                .all(|part| !matches!(part, UserContent::ToolResult(_))) =>
                        {
                            let turn = self.allocate_turn();
                            current_turn = Some(turn);
                            Some(NativeHeader::Prompt(turn))
                        }
                        Message::Assistant { .. } => {
                            current_turn.map(|turn| self.allocate_call(turn))
                        }
                        _ => None,
                    };
                    if let Some(id) = provider.display_attempt_id()
                        && !self.hosted_attempts.contains_key(id)
                        && let Some(attempt) = saved_attempts.get(id)
                    {
                        self.update_web_search((**attempt).clone());
                    }
                    let _ = self.finish_tool_results(&message, &[]);
                    self.commit_response(
                        message,
                        provider.display_attempt_id(),
                        ToolCallStatus::Executing,
                        header,
                    );
                }
                TranscriptItem::ToolResults {
                    message, metadata, ..
                } => {
                    let _ = self.finish_tool_results(&message, &metadata);
                    self.push_message(message, ToolCallStatus::Executing);
                }
                TranscriptItem::SessionModels(_)
                | TranscriptItem::SessionMode(_)
                | TranscriptItem::Directive(_)
                | TranscriptItem::RequestDirective(_) => {}
                TranscriptItem::SkillInvocation(invocation) => {
                    let turn = self.allocate_turn();
                    current_turn = Some(turn);
                    self.push_user_turn(invocation.display_message(), turn);
                }
                TranscriptItem::Plan(record) => match record {
                    PlanRecord::Ready { artifact } | PlanRecord::Published { artifact, .. } => {
                        self.push_plan_artifact(artifact)
                    }
                    PlanRecord::Handoff { handoff } => {
                        let turn = self.allocate_turn();
                        current_turn = Some(turn);
                        self.push_plan_handoff(handoff, turn);
                    }
                    PlanRecord::Started { .. }
                    | PlanRecord::RevisionRequested { .. }
                    | PlanRecord::Resolved { .. } => {}
                },
                TranscriptItem::Ensemble(record) => match record {
                    EnsembleRecord::Started { start } => {
                        let turn = self
                            .ensemble_turn(&start.run_id)
                            .unwrap_or_else(|| self.allocate_turn());
                        current_turn = Some(turn);
                        if !self.has_ensemble(&start.run_id) {
                            self.add_ensemble(
                                start,
                                AgentRunStatus::Interrupted,
                                Some(NativeHeader::Prompt(turn)),
                            );
                        }
                    }
                    EnsembleRecord::ReportsReady { run_id, agents, .. } => {
                        current_turn = self.ensemble_turn(&run_id).or(current_turn);
                        for summary in agents {
                            self.project_worker_completion(
                                &run_id,
                                crate::projection::WorkerCompletion::summary(&run_id, summary),
                            );
                        }
                    }
                    EnsembleRecord::Cancelled { run_id } => {
                        self.update_unfinished_ensemble_workers(&run_id, AgentRunStatus::Cancelled);
                    }
                    EnsembleRecord::Failed { run_id, .. } => {
                        self.update_unfinished_ensemble_workers(
                            &run_id,
                            AgentRunStatus::Interrupted,
                        );
                    }
                    EnsembleRecord::WorkerReview {
                        run_id,
                        worker_id,
                        event,
                        ..
                    } if matches!(
                        event.as_ref(),
                        zevria_workflow::WorkerReviewEvent::Abandoned { .. }
                    ) =>
                    {
                        self.update_ensemble_worker(
                            &run_id,
                            &worker_id,
                            AgentRunStatus::Abandoned,
                            Some(zevria_workflow::ensemble::WORKER_ABANDONMENT_REASON.into()),
                        );
                    }
                    EnsembleRecord::WorkersConfirmed {
                        run_id, outcomes, ..
                    } => {
                        for outcome in outcomes {
                            self.project_worker_completion(
                                &run_id,
                                crate::projection::WorkerCompletion::outcome(&run_id, outcome),
                            );
                        }
                    }
                    EnsembleRecord::ReviewStarted { .. }
                    | EnsembleRecord::WorkerReview { .. }
                    | EnsembleRecord::ControlResult { .. }
                    | EnsembleRecord::Completed { .. } => {}
                },
                TranscriptItem::Compaction(_) => {
                    self.history.push(HistoryEntry::CompactionDivider);
                }
                TranscriptItem::Error { error } => self.history.push(HistoryEntry::Error(error)),
            }
        }
        if let Ok(runs) = review_projection {
            for (run_id, states) in runs {
                for state in states {
                    if reviewed.contains(&(run_id.clone(), state.descriptor.id.clone())) {
                        self.project_worker_review(
                            &run_id,
                            &state.descriptor.id,
                            &state,
                            crate::projection::ProjectionMode::Restored,
                        );
                    }
                }
            }
        }
        self.interrupt_executing_tool_calls();
    }

    #[cfg(test)]
    pub(crate) fn seed_entry(&mut self, entry: HistoryEntry) {
        self.push_entry(entry);
    }
}

/// Materialize one selectable row per accepted child in input order, retaining
/// existing block IDs so selection and fold state survive insertion/reordering.
fn sync_subtask_rows(
    entry: &mut ConversationEntry,
    parent: PresentationBlockId,
    ids: &mut crate::presentation::BlockIdAllocator,
) {
    let children = entry
        .blocks
        .iter()
        .find(|block| block.id == parent)
        .and_then(PresentationBlock::native_tool)
        .map(|(_, state)| state.subtasks.clone())
        .unwrap_or_default();
    let mut rows = Vec::new();
    for (entry_index, descriptor) in children {
        let existing = entry.blocks.iter().position(|block| matches!(&block.kind, PresentationBlockKind::Subtask { parent: owner, entry_index: index, .. } if *owner == parent && *index == entry_index));
        let mut row = if let Some(position) = existing {
            entry.blocks.remove(position)
        } else {
            let id = ids.allocate();
            PresentationBlock {
                id,
                revision: 0,
                role: Some(PresentationRole::Assistant),
                prompt_group: None,
                prompt: None,
                visibility: BlockVisibility::Always,
                kind: PresentationBlockKind::Subtask {
                    parent,
                    entry_index,
                    descriptor: descriptor.clone(),
                },
            }
        };
        let kind = PresentationBlockKind::Subtask {
            parent,
            entry_index,
            descriptor,
        };
        if row.kind != kind {
            row.kind = kind;
            row.touch();
        }
        rows.push(row);
    }
    if let Some(position) = entry.blocks.iter().position(|block| block.id == parent) {
        entry.blocks.splice(position + 1..position + 1, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_workflow::AgentRunDescriptor;
    use zevria_workflow::EnsembleRunId;

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
            Message::tool_result("call", "command", "tool output"),
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
            WebSearchActivity, WebSearchAttemptOutcome, WebSearchAttemptRecord, WebSearchStatus,
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
        let mut attempt =
            WebSearchAttemptRecord::new(zevria_foundation::ModelProfileRef::new("p", "m"));
        attempt.activity.push(WebSearchActivity {
            item_id: Some("action".into()),
            output_index: 0,
            status: WebSearchStatus::Searching,
            action: Some(serde_json::json!({"type":"search", "query":"Rust"})),
        });
        conversation.update_web_search(attempt.clone());
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
        attempt.finish(WebSearchAttemptOutcome::Interrupted);
        conversation.update_web_search(attempt);
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
    fn citation_entities_cannot_reintroduce_terminal_controls_through_markdown() {
        let text = zevria_content::citations::render(
            "Claim",
            Some(&serde_json::json!([
                {"type":"url_citation","start_index":0,"end_index":5,"title":"&Tab; and &#27;","url":"https://example.com/?q=&Tab;&x=1"}
            ])),
        );
        let displayed = crate::markdown::markdown_lines(&text, ratatui::style::Style::default(), 0)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(!displayed.chars().any(char::is_control));
        assert!(displayed.contains("https://example.com/?q=&Tab;&x=1"));
    }

    #[test]
    fn hosted_rows_update_restore_and_citations_are_copyable() {
        use zevria_content::WebSearchActivity;
        use zevria_content::WebSearchAttemptOutcome;
        use zevria_content::WebSearchAttemptRecord;
        use zevria_content::WebSearchStatus;
        use zevria_foundation::ModelProfileRef;
        let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
        attempt.activity.push(WebSearchActivity {
            item_id: Some("ws".into()),
            output_index: 0,
            status: WebSearchStatus::Searching,
            action: Some(serde_json::json!({"type":"search","query":"Rust"})),
        });
        let mut conversation = ConversationState::default();
        conversation.update_web_search(attempt.clone());
        attempt.finish(WebSearchAttemptOutcome::Interrupted);
        conversation.update_web_search(attempt.clone());
        assert_eq!(conversation.history().len(), 1);
        assert!(
            selected_plain_text(&conversation.history()[0], 1)
                .unwrap()
                .contains("interrupted")
        );
        conversation.restore(vec![TranscriptItem::WebSearchAttempt(attempt)]);
        assert_eq!(conversation.history().len(), 1);
        let replay = zevria_model::ProviderReplay::openai_responses(
            ModelProfileRef::new("p", "m"),
            vec![
                serde_json::json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Claim","annotations":[{"type":"url_citation","start_index":0,"end_index":5,"title":"Source","url":"https://example.com/a"}]}]}),
            ],
        );
        conversation.push_message(replay.to_message().unwrap(), ToolCallStatus::Finished);
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
            TranscriptItem::SessionModels(models),
            TranscriptItem::SessionMode(zevria_foundation::SessionMode::Build),
            TranscriptItem::Directive(zevria_instructions::DirectiveContent::skill(
                &zevria_instructions::SkillSnapshot::new(
                    "hidden".parse().unwrap(),
                    "Hidden",
                    "HIDDEN_SKILL_BODY",
                )
                .unwrap(),
            )),
            TranscriptItem::Directive(
                zevria_instructions::DirectiveContent::new(
                    zevria_instructions::DirectivePayload::SkillRevocation {
                        name: "hidden".parse().unwrap(),
                        reason: "disabled".into(),
                    },
                )
                .unwrap(),
            ),
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
