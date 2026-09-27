//! Source-neutral transcript presentation types used by every TUI pane.
//!
//! Durable engine/provider values are adapted into these blocks before they
//! reach layout. This keeps protocol details out of the renderer and gives
//! native Zevria and ACP transcripts the same selection, copying, styling,
//! and cache invalidation rules.

use rig_core::message::{ProviderCallId, Reasoning, ReasoningContent, ToolCall, ToolResult};
use zevria_content::WebSearchStatus;
use zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::ToolResultMetadata;
use zevria_foundation::subtask::{SubtaskDescriptor, SubtaskStatus};
use zevria_foundation::{
    QUESTION_TOOL_NAME, QuestionResponse, RECONCILE_REPORTS_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME,
    TASK_TOOL_NAME, TaskList, TaskStatus,
};
use zevria_workflow::{
    AgentRunLocation, RecordedDecisionDisposition, ReportDisagreementClassification,
    ReportDisagreementResolution, ReportReconciliation, UnavailableDecisionDisposition,
};

use crate::status_icon::StatusIcon;

/// Yield only plaintext reasoning parts in their original source order.
pub(crate) fn readable_reasoning_parts(reasoning: &Reasoning) -> impl Iterator<Item = &str> {
    reasoning.content.iter().filter_map(|part| match part {
        ReasoningContent::Summary(text) | ReasoningContent::Text { text, .. } => {
            (!text.trim().is_empty()).then_some(text.as_str())
        }
        ReasoningContent::Encrypted(_) | ReasoningContent::Redacted { .. } => None,
    })
}

/// A complete projection replacement retires every ephemeral anchor even if
/// the new source happens to reuse the same block and history positions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectionEpoch(u64);
impl Default for ProjectionEpoch {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationAnchor {
    pub epoch: ProjectionEpoch,
    pub history: usize,
    pub block: PresentationBlockId,
}

/// Stable, pane-local identity of a semantic content block.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PresentationBlockId(pub(crate) u64);

/// One allocation domain per projection, shared by native, hosted and ACP
/// adapters. Provider call IDs and transcript positions never enter this space.
#[derive(Debug, Default)]
pub(crate) struct BlockIdAllocator {
    next: u64,
}

impl BlockIdAllocator {
    pub(crate) fn allocate(&mut self) -> PresentationBlockId {
        let id = PresentationBlockId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("presentation identity exhausted");
        id
    }

    /// An adapter may start with local ordinals (e.g. a streamed message).
    /// Committing the logical message gives all its blocks projection identities.
    pub(crate) fn identify(&mut self, entry: &mut ConversationEntry) {
        let mut identities = entry
            .blocks
            .iter()
            .map(|block| (block.id, self.allocate()))
            .collect::<std::collections::HashMap<_, _>>();
        for block in &mut entry.blocks {
            block.id = identities[&block.id];
            block.prompt_group = block
                .prompt_group
                .map(|id| *identities.entry(id).or_insert_with(|| self.allocate()));
            if let PresentationBlockKind::Subtask { parent, .. } = &mut block.kind {
                // Legacy/sparse projections may omit a parent. Reserve its
                // identity rather than aliasing an unrelated future block.
                *parent = *identities.entry(*parent).or_insert_with(|| self.allocate());
            }
        }
    }
}

/// Explicit pane appearance, independent of composer and diagnostics state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TranscriptAppearance {
    #[default]
    Native,
    Acp,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PromptOrigin {
    #[default]
    Initial,
    Feedback,
    Retry,
    Recovery,
    Continuation,
}

impl PromptOrigin {
    pub(crate) const fn label(self) -> Option<&'static str> {
        match self {
            Self::Initial => None,
            Self::Feedback => Some("feedback"),
            Self::Retry => Some("retry"),
            Self::Recovery => Some("recovery"),
            Self::Continuation => Some("continuation"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptPhase {
    Queued,
    Dispatched,
    Recovering,
    Cancelling,
    Succeeded,
    Failed,
    Interrupted,
}

impl PromptPhase {
    pub(crate) const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

/// Display-only metadata owned by the first content block of an input. Success
/// remains explicit even though its transient badge is no longer painted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PromptAnnotation {
    pub(crate) origin: PromptOrigin,
    pub(crate) generation: Option<u64>,
    pub(crate) request_id: Option<zevria_workflow::WorkerControlId>,
    pub(crate) phase: Option<PromptPhase>,
    pub(crate) latest_attempt: Option<u64>,
    pub(crate) cancel_requested: bool,
}

/// Conversation role whose header precedes a block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresentationRole {
    User,
    Assistant,
    System,
}

/// Whether a block belongs in the polished transcript or only in the raw
/// diagnostic view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockVisibility {
    Always,
    Diagnostics,
    /// Standard projection covered by validated ordered metadata; retained,
    /// but not rendered twice even in the diagnostic view.
    Covered,
    /// Underlying ACP action retained beneath a derived pending group.
    Grouped,
}

/// Plain user text and Markdown-aware assistant text use the same block with
/// an explicit rendering flavor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextFlavor {
    Plain,
    Markdown,
}

/// Display lifecycle for both native and ACP tool calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresentedToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Denied,
    Interrupted,
    Unknown,
}

impl PresentedToolStatus {
    pub(crate) const fn icon(self) -> StatusIcon {
        match self {
            Self::Pending => StatusIcon::Pending,
            Self::Running => StatusIcon::Running,
            Self::Completed => StatusIcon::Done,
            Self::Failed => StatusIcon::Failed,
            Self::Denied => StatusIcon::Denied,
            Self::Interrupted => StatusIcon::Interrupted,
            Self::Unknown => StatusIcon::Unknown,
        }
    }

    pub(crate) const fn unsuccessful(self) -> bool {
        matches!(self, Self::Failed | Self::Denied | Self::Interrupted)
    }
}

/// Display lifecycle retained for committed native tool-call occurrences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeToolCallStatus {
    Executing,
    /// The assistant call was committed, but no correlated result arrived
    /// before restore or a terminal turn event.
    Interrupted,
    Finished,
}

/// Native Zevria state that is filled when its correlated result arrives.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeToolState {
    pub(crate) status: NativeToolCallStatus,
    pub(crate) result: Option<ToolResult>,
    pub(crate) metadata: Option<ToolResultMetadata>,
    /// Decoded once at the source boundary. Raw call arguments remain intact
    /// for copying and provider correlation, including malformed JSON bytes.
    pub(crate) arguments: Option<serde_json::Value>,
    pub(crate) subtasks: std::collections::BTreeMap<usize, SubtaskDescriptor>,
}

impl NativeToolState {
    pub(crate) fn new(status: NativeToolCallStatus, arguments: &serde_json::Value) -> Self {
        Self {
            status,
            result: None,
            metadata: None,
            arguments: normalized_arguments(arguments),
            subtasks: std::collections::BTreeMap::new(),
        }
    }
}

/// What a `launch_subtasks` block shows beyond its separately rendered
/// child rows; `None` hides the block.
pub(crate) enum LaunchBatchNotice {
    Rejected { denied: bool },
    Ended(StatusIcon),
    Unlaunched { missing: usize, requested: usize },
}

pub(crate) fn launch_batch_notice(state: &NativeToolState) -> Option<LaunchBatchNotice> {
    if state.status == NativeToolCallStatus::Executing {
        return None;
    }

    let launched = state.subtasks.len();
    if launched > 0 {
        let requested = state
            .metadata
            .as_ref()
            .map(ToolResultMetadata::subtasks)
            .filter(|entries| !entries.is_empty())
            .map_or_else(
                || {
                    state
                        .arguments
                        .as_ref()
                        .and_then(|args| args.get("tasks"))
                        .and_then(serde_json::Value::as_array)
                        .map_or(0, Vec::len)
                },
                <[_]>::len,
            );
        return if launched < requested {
            Some(LaunchBatchNotice::Unlaunched {
                missing: requested - launched,
                requested,
            })
        } else {
            None
        };
    }

    Some(if state.status == NativeToolCallStatus::Interrupted {
        LaunchBatchNotice::Ended(StatusIcon::Interrupted)
    } else if tool_call_denied(state) {
        LaunchBatchNotice::Rejected { denied: true }
    } else if state
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.outcome == ToolCallOutcome::Cancelled)
    {
        LaunchBatchNotice::Ended(StatusIcon::Interrupted)
    } else if !state
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.outcome.is_success())
        && tool_call_failed(state)
    {
        LaunchBatchNotice::Rejected { denied: false }
    } else {
        LaunchBatchNotice::Ended(native_tool_status(state).icon())
    })
}

pub(crate) fn tool_call_denied(state: &NativeToolState) -> bool {
    state
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.outcome == ToolCallOutcome::Denied)
}

pub(crate) fn tool_call_failed(state: &NativeToolState) -> bool {
    state.metadata.as_ref().is_some_and(|metadata| {
        matches!(
            metadata.outcome,
            ToolCallOutcome::Error | ToolCallOutcome::Skipped | ToolCallOutcome::Partial
        )
    })
}

pub(crate) fn native_tool_status(state: &NativeToolState) -> PresentedToolStatus {
    match state.status {
        NativeToolCallStatus::Executing => PresentedToolStatus::Running,
        NativeToolCallStatus::Interrupted => PresentedToolStatus::Interrupted,
        NativeToolCallStatus::Finished if tool_call_denied(state) => PresentedToolStatus::Denied,
        NativeToolCallStatus::Finished
            if state
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.outcome == ToolCallOutcome::Cancelled) =>
        {
            PresentedToolStatus::Interrupted
        }
        NativeToolCallStatus::Finished if tool_call_failed(state) => PresentedToolStatus::Failed,
        NativeToolCallStatus::Finished
            if state
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.outcome.is_success()) =>
        {
            PresentedToolStatus::Completed
        }
        NativeToolCallStatus::Finished => PresentedToolStatus::Unknown,
    }
}

pub(crate) fn question_response(state: &NativeToolState) -> Option<QuestionResponse> {
    state
        .result
        .as_ref()
        .map(tool_result_plain_text)
        .and_then(|output| serde_json::from_str(&output).ok())
}

/// Resolved operation outcome shared by expanded headers, readable lists and
/// typed folds. Item completion is not evidence that its update succeeded.
pub(crate) fn native_header_status(call: &ToolCall, state: &NativeToolState) -> StatusIcon {
    let status = native_tool_status(state);
    if status == PresentedToolStatus::Completed {
        match call.function.name.as_str() {
            TASK_TOOL_NAME | RECONCILE_REPORTS_TOOL_NAME => {
                let confirmed = state.result.is_some()
                    && state
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.outcome.is_success());
                if !confirmed {
                    return StatusIcon::Unknown;
                }
                if call.function.name == TASK_TOOL_NAME {
                    return StatusIcon::Updated;
                }
            }
            QUESTION_TOOL_NAME
                if matches!(question_response(state), Some(QuestionResponse::Dismissed)) =>
            {
                return StatusIcon::Dismissed;
            }
            _ => {}
        }
    }
    status.icon()
}

/// Provider-neutral materialized ACP tool call. Sparse updates mutate this
/// value in place; collection fields replace their previous contents.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AcpToolPresentation {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) content: Vec<String>,
    pub(crate) locations: Vec<AgentRunLocation>,
    pub(crate) raw_input: Option<serde_json::Value>,
    pub(crate) raw_output: Option<serde_json::Value>,
    pub(crate) metadata: Option<ToolResultMetadata>,
}

impl AcpToolPresentation {
    pub(crate) fn presented_status(&self) -> PresentedToolStatus {
        if let Some(metadata) = &self.metadata {
            return match metadata.outcome {
                ToolCallOutcome::Success => PresentedToolStatus::Completed,
                ToolCallOutcome::Error | ToolCallOutcome::Skipped | ToolCallOutcome::Partial => {
                    PresentedToolStatus::Failed
                }
                ToolCallOutcome::Denied => PresentedToolStatus::Denied,
                ToolCallOutcome::Cancelled => PresentedToolStatus::Interrupted,
            };
        }
        match self.status.as_str() {
            "pending" => PresentedToolStatus::Pending,
            "in_progress" | "running" => PresentedToolStatus::Running,
            "completed" | "finished" => PresentedToolStatus::Completed,
            "failed" | "error" => PresentedToolStatus::Failed,
            "denied" | "rejected" => PresentedToolStatus::Denied,
            "interrupted" | "cancelled" => PresentedToolStatus::Interrupted,
            _ => PresentedToolStatus::Unknown,
        }
    }

    pub(crate) fn hosted_activity(&self) -> Option<WebActivityPresentation> {
        if !zevria_content::web_search::is_hosted_search_input(&self.id, self.raw_input.as_ref()) {
            return None;
        }
        let activity = zevria_content::WebSearchActivity {
            output_index: zevria_content::web_search::HostedSearchAddress::parse(&self.id)?
                .output_index,
            item_id: None,
            status: WebSearchStatus::InProgress,
            action: self
                .raw_input
                .as_ref()
                .and_then(|input| input.get("action"))
                .filter(|action| !action.is_null())
                .cloned(),
        };
        Some(WebActivityPresentation {
            members: vec![activity.output_index],
            detail: (!activity.details().is_empty()).then(|| activity.label()),
            outcomes: vec![(self.presented_status().icon(), 1)],
        })
    }

    pub(crate) fn input_text(&self) -> String {
        if let Some(activity) = self.hosted_activity() {
            return activity.copy_text();
        }
        self.raw_input.as_ref().map_or_else(
            || {
                let mut summary = format!("{} · {}", self.title, self.kind);
                for location in &self.locations {
                    summary.push('\n');
                    summary.push_str(&location.path.display().to_string());
                    if let Some(line) = location.line {
                        summary.push_str(&format!(":{line}"));
                    }
                }
                summary
            },
            json_copy_text,
        )
    }

    pub(crate) fn output_text(&self) -> Option<String> {
        if !self.content.is_empty() {
            return Some(self.content.join("\n"));
        }
        self.raw_output.as_ref().map(json_copy_text)
    }
}

fn json_copy_text(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string)
}

/// Display-only decoding. Never use this normalized value for dispatch.
pub(crate) fn normalized_arguments(raw: &serde_json::Value) -> Option<serde_json::Value> {
    match raw {
        serde_json::Value::String(raw) => serde_json::from_str(raw).ok(),
        value => Some(value.clone()),
    }
}

pub(crate) fn tool_argument(state: &NativeToolState, key: &str) -> Option<String> {
    state
        .arguments
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// One shared tool block. Native calls retain the Rig value needed for result
/// correlation and specialized bodies; ACP calls use their normalized state.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PresentedTool {
    Native {
        call: Box<ToolCall>,
        state: Box<NativeToolState>,
    },
    Acp(Box<AcpToolPresentation>),
}

impl PresentedTool {
    pub(crate) fn primary_copy(&self) -> String {
        match self {
            Self::Native { call, state } => {
                if call.function.name == SUBMIT_PLAN_TOOL_NAME
                    && let Some(markdown) = tool_argument(state, "markdown")
                {
                    markdown
                } else {
                    call.function
                        .arguments
                        .as_str()
                        .map_or_else(|| call.function.arguments.to_string(), str::to_string)
                }
            }
            Self::Acp(tool) => tool.input_text(),
        }
    }

    pub(crate) fn secondary_copy(&self) -> Option<String> {
        match self {
            Self::Native { state, .. } => state.result.as_ref().map(tool_result_plain_text),
            Self::Acp(tool) => tool.output_text(),
        }
    }

    pub(crate) fn native_call(&self) -> Option<(&ToolCall, &NativeToolState)> {
        match self {
            Self::Native { call, state } => Some((&**call, &**state)),
            Self::Acp(_) => None,
        }
    }

    pub(crate) fn native_call_mut(&mut self) -> Option<(&ToolCall, &mut NativeToolState)> {
        match self {
            Self::Native { call, state } => Some((&**call, &mut **state)),
            Self::Acp(_) => None,
        }
    }
}

/// One row of a source-neutral checklist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChecklistItem {
    pub(crate) text: String,
    pub(crate) priority: Option<String>,
    pub(crate) status: ChecklistStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChecklistStatus {
    Pending,
    InProgress,
    Completed,
    Unknown,
}

impl ChecklistStatus {
    pub(crate) const fn icon(self) -> StatusIcon {
        match self {
            Self::Pending => StatusIcon::Pending,
            Self::InProgress => StatusIcon::Running,
            Self::Completed => StatusIcon::Done,
            Self::Unknown => StatusIcon::Unknown,
        }
    }
}

pub(crate) const fn subtask_icon(status: SubtaskStatus) -> StatusIcon {
    match status {
        SubtaskStatus::Starting | SubtaskStatus::Running => StatusIcon::Running,
        SubtaskStatus::Completed => StatusIcon::Done,
        SubtaskStatus::Failed => StatusIcon::Failed,
        SubtaskStatus::Cancelled => StatusIcon::Interrupted,
    }
}

pub(crate) const fn task_icon(status: TaskStatus) -> StatusIcon {
    match status {
        TaskStatus::Pending => StatusIcon::Pending,
        TaskStatus::InProgress => StatusIcon::Running,
        TaskStatus::Completed => StatusIcon::Done,
    }
}

pub(crate) const fn recorded_decision_icon(
    disposition: &RecordedDecisionDisposition,
) -> StatusIcon {
    match disposition {
        RecordedDecisionDisposition::Applied { .. } => StatusIcon::Done,
        RecordedDecisionDisposition::ObjectivelyInapplicable { .. } => StatusIcon::Dismissed,
    }
}

pub(crate) const fn unavailable_decision_icon(
    disposition: &UnavailableDecisionDisposition,
) -> StatusIcon {
    match disposition {
        UnavailableDecisionDisposition::RootQuestionRequired { .. } => StatusIcon::Pending,
        UnavailableDecisionDisposition::ObjectivelyInapplicable { .. } => StatusIcon::Dismissed,
    }
}

/// A complete checklist snapshot. ACP plan updates replace `items` in place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresentedChecklist {
    pub(crate) label: String,
    pub(crate) items: Vec<ChecklistItem>,
}

impl PresentedChecklist {
    pub(crate) fn completed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == ChecklistStatus::Completed)
            .count()
    }

    pub(crate) fn copy_text(&self) -> String {
        self.items
            .iter()
            .map(|item| {
                let marker = item.status.icon().glyph(0);
                item.priority.as_ref().map_or_else(
                    || format!("{marker} {}", item.text),
                    |priority| format!("{marker} {} ({priority})", item.text),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One replaceable worker-plan block. ACP plan operations can switch between
/// checklist and inline Markdown representations without leaving stale blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresentedPlan {
    pub(crate) plan_id: Option<String>,
    pub(crate) content: PresentedPlanContent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PresentedPlanContent {
    Checklist(PresentedChecklist),
    Markdown(String),
}

impl PresentedPlan {
    pub(crate) fn copy_text(&self) -> String {
        match &self.content {
            PresentedPlanContent::Checklist(checklist) => checklist.copy_text(),
            PresentedPlanContent::Markdown(markdown) => markdown.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticTone {
    Muted,
    Info,
    Success,
    Warning,
    Error,
}

/// A raw/lifecycle ACP item retained in chronological order but hidden from
/// the polished view by default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresentedDiagnostic {
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) tone: DiagnosticTone,
}

/// Inline hosted actions, shared by native and ACP attempt projections. A
/// group's identity is its first member, so selection follows it on a split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WebActivityPresentation {
    pub(crate) members: Vec<u64>,
    pub(crate) detail: Option<String>,
    pub(crate) outcomes: Vec<(StatusIcon, usize)>,
}
impl WebActivityPresentation {
    pub(crate) fn from_actions(
        attempt: &zevria_content::WebSearchAttemptRecord,
        actions: &[&zevria_content::WebSearchActivity],
    ) -> Self {
        let mut outcomes: Vec<(StatusIcon, usize)> = Vec::new();
        for action in actions {
            let status = match attempt.confirmed_status(action) {
                Some(status) => Some(status),
                None if attempt.outcome.is_terminal() => None,
                None => Some(action.status),
            };
            let icon = match status {
                Some(WebSearchStatus::InProgress | WebSearchStatus::Searching) => {
                    StatusIcon::Running
                }
                Some(WebSearchStatus::Completed) => StatusIcon::Done,
                Some(WebSearchStatus::Failed) => StatusIcon::Failed,
                Some(WebSearchStatus::Interrupted) => StatusIcon::Interrupted,
                None => StatusIcon::Updated,
            };
            if let Some((_, count)) = outcomes.iter_mut().find(|(status, _)| *status == icon) {
                *count += 1;
            } else {
                outcomes.push((icon, 1));
            }
        }
        Self {
            members: actions.iter().map(|action| action.output_index).collect(),
            detail: (actions.len() == 1 && !actions[0].details().is_empty())
                .then(|| actions[0].label()),
            outcomes,
        }
    }
    pub(crate) fn copy_text(&self) -> String {
        let counted = self.detail.is_none();
        let label = self
            .detail
            .clone()
            .unwrap_or_else(|| format!("Web actions · {}", self.members.len()));
        let statuses = self
            .outcomes
            .iter()
            .map(|(status, count)| {
                let glyph = status.glyph(0);
                if counted {
                    format!("{count} {glyph}")
                } else {
                    glyph.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        if counted {
            format!("{label} · {statuses}")
        } else {
            format!("{label} {statuses}")
        }
    }
}

/// One independently selectable semantic unit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PresentationBlockKind {
    Text {
        text: String,
        flavor: TextFlavor,
        editable: bool,
    },
    Reasoning {
        parts: Vec<String>,
    },
    WebActivity(WebActivityPresentation),
    Tool(PresentedTool),
    /// Derived compact child row. Its block identity survives event reordering.
    Subtask {
        parent: PresentationBlockId,
        entry_index: usize,
        descriptor: SubtaskDescriptor,
    },
    Plan(PresentedPlan),
    Image {
        image: zevria_content::PromptImage,
        ordinal: usize,
        editable: bool,
    },
    Placeholder(String),
    Diagnostic(PresentedDiagnostic),
    Error(String),
}

/// One block inside a conversation/timeline entry.
#[derive(Clone, Debug)]
pub(crate) struct PresentationBlock {
    pub(crate) id: PresentationBlockId,
    pub(crate) revision: u64,
    pub(crate) role: Option<PresentationRole>,
    /// First content block's stable identity; decoration is never a copy target.
    pub(crate) prompt_group: Option<PresentationBlockId>,
    pub(crate) prompt: Option<PromptAnnotation>,
    pub(crate) visibility: BlockVisibility,
    pub(crate) kind: PresentationBlockKind,
}

impl PresentationBlock {
    pub(crate) fn visible(&self, diagnostics_visible: bool) -> bool {
        (self.visibility == BlockVisibility::Always
            || (self.visibility == BlockVisibility::Diagnostics && diagnostics_visible))
            && !matches!(&self.kind, PresentationBlockKind::Reasoning { parts } if parts.iter().all(|part| part.trim().is_empty()))
            && !self.native_tool().is_some_and(|(call, state)| {
                call.function.name == LAUNCH_SUBTASKS_TOOL_NAME
                    && launch_batch_notice(state).is_none()
            })
    }

    pub(crate) fn primary_copy(&self) -> String {
        match &self.kind {
            PresentationBlockKind::Text { text, .. } => text.clone(),
            PresentationBlockKind::Reasoning { parts } => parts.join("\n"),
            PresentationBlockKind::Tool(tool) => tool.primary_copy(),
            PresentationBlockKind::Subtask { descriptor, .. } => format!(
                "{} · {} {}",
                descriptor.kind,
                descriptor.title,
                subtask_icon(descriptor.status).glyph(0)
            ),
            PresentationBlockKind::WebActivity(activity) => activity.copy_text(),
            PresentationBlockKind::Plan(plan) => plan.copy_text(),
            PresentationBlockKind::Image { image, ordinal, .. } => image.label(*ordinal),
            PresentationBlockKind::Placeholder(placeholder) => placeholder.clone(),
            PresentationBlockKind::Diagnostic(diagnostic) => diagnostic.text.clone(),
            PresentationBlockKind::Error(error) => error.clone(),
        }
    }

    /// Explicit readable-list copy; never substitutes for raw native y/yy.
    pub(crate) fn readable_list_copy(&self) -> Option<String> {
        match &self.kind {
            PresentationBlockKind::Tool(PresentedTool::Native { call, state })
                if matches!(
                    call.function.name.as_str(),
                    TASK_TOOL_NAME | RECONCILE_REPORTS_TOOL_NAME
                ) =>
            {
                Some(
                    native_list(call, state)
                        .map_or_else(|| self.primary_copy(), |list| list.copy_text()),
                )
            }
            PresentationBlockKind::Plan(PresentedPlan {
                content: PresentedPlanContent::Checklist(checklist),
                ..
            }) => Some(checklist.copy_text()),
            _ => None,
        }
    }

    pub(crate) fn secondary_copy(&self) -> Option<String> {
        match &self.kind {
            PresentationBlockKind::Tool(tool) => tool.secondary_copy(),
            PresentationBlockKind::Text { .. }
            | PresentationBlockKind::Reasoning { .. }
            | PresentationBlockKind::WebActivity(_)
            | PresentationBlockKind::Subtask { .. }
            | PresentationBlockKind::Plan(_)
            | PresentationBlockKind::Image { .. }
            | PresentationBlockKind::Placeholder(_)
            | PresentationBlockKind::Diagnostic(_)
            | PresentationBlockKind::Error(_) => None,
        }
    }

    pub(crate) fn is_editable(&self) -> bool {
        self.editable_text().is_some()
            || matches!(
                self.kind,
                PresentationBlockKind::Image { editable: true, .. }
            )
    }

    pub(crate) fn editable_text(&self) -> Option<&str> {
        match &self.kind {
            PresentationBlockKind::Text {
                text,
                editable: true,
                ..
            } => Some(text),
            _ => None,
        }
    }

    pub(crate) fn native_tool(&self) -> Option<(&ToolCall, &NativeToolState)> {
        match &self.kind {
            PresentationBlockKind::Tool(tool) => tool.native_call(),
            _ => None,
        }
    }

    pub(crate) fn native_tool_mut(&mut self) -> Option<(&ToolCall, &mut NativeToolState)> {
        match &mut self.kind {
            PresentationBlockKind::Tool(tool) => tool.native_call_mut(),
            _ => None,
        }
    }

    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// A pane-local, rebuildable ordinal, never an engine TurnId or an edit target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct DisplayTurn(pub(crate) usize);

/// Native header decoration only. Never enters messages, persistence, or copy payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum NativeHeader {
    Prompt(DisplayTurn),
    Assistant { turn: DisplayTurn, call: usize },
}

impl NativeHeader {
    pub(crate) const fn turn(self) -> DisplayTurn {
        match self {
            Self::Prompt(turn) | Self::Assistant { turn, .. } => turn,
        }
    }
}

impl std::fmt::Display for NativeHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt(turn) => write!(formatter, "#{}", turn.0),
            Self::Assistant { turn, call } => write!(formatter, "#({} - {call})", turn.0),
        }
    }
}

/// A chronological group of semantic blocks. Native messages generally map
/// one-to-one; one ACP prompt cycle can contain many roles and diagnostics so
/// hidden diagnostics do not force repeated Assistant headers.
#[derive(Clone, Debug)]
pub(crate) struct ConversationEntry {
    pub(crate) header: Option<NativeHeader>,
    pub(crate) blocks: Vec<PresentationBlock>,
}

/// Provider-call identity retained only for native tool-result correlation.
pub(crate) fn provider_call_id(provider: Option<&ProviderCallId>) -> Option<&String> {
    provider.map(|provider| &provider.call_id)
}

pub(crate) fn tool_result_plain_text(result: &ToolResult) -> String {
    use rig_core::message::ToolResultContent;

    result
        .content
        .iter()
        .map(|content| match content {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Image(_) => "[image]".to_string(),
            ToolResultContent::Json { value } => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Row-type markers are deliberately separate from lifecycle/status icons.
pub(crate) enum ListMarker {
    Status(StatusIcon),
    Disagreement,
}

impl ListMarker {
    pub(crate) fn glyph(&self) -> &'static str {
        match self {
            Self::Status(icon) => icon.glyph(0),
            Self::Disagreement => "⚖",
        }
    }
}

pub(crate) struct PresentedListRow {
    pub(crate) marker: ListMarker,
    pub(crate) text: String,
    pub(crate) annotation: String,
    pub(crate) muted: bool,
    /// Complete source text for readable copying, never viewport-truncated.
    copy_details: Vec<String>,
}

pub(crate) struct PresentedNativeList {
    pub(crate) label: String,
    pub(crate) status: StatusIcon,
    pub(crate) explanation: Option<String>,
    pub(crate) rows: Vec<PresentedListRow>,
    pub(crate) reconciliation: bool,
}

impl PresentedNativeList {
    fn copy_text(&self) -> String {
        let mut lines = vec![format!("{} {}", self.label, self.status.glyph(0))];
        if let Some(explanation) = &self.explanation {
            lines.push(explanation.clone());
        }
        for row in &self.rows {
            lines.push(format!(
                "{} {}{}",
                row.marker.glyph(),
                row.text,
                row.annotation
            ));
            lines.extend(row.copy_details.iter().cloned());
        }
        lines.join("\n")
    }
}

pub(crate) fn native_list(call: &ToolCall, state: &NativeToolState) -> Option<PresentedNativeList> {
    let status = native_header_status(call, state);
    match call.function.name.as_str() {
        TASK_TOOL_NAME => {
            let tasks = TaskList::from_tool_arguments(state.arguments.as_ref()?).ok()?;
            Some(PresentedNativeList {
                label: format!(
                    "task · {}/{} completed",
                    tasks.completed_count(),
                    tasks.tasks.len()
                ),
                status,
                explanation: tasks.explanation,
                rows: tasks
                    .tasks
                    .into_iter()
                    .map(|task| PresentedListRow {
                        marker: ListMarker::Status(task_icon(task.status)),
                        text: task.step,
                        annotation: String::new(),
                        muted: task.status == TaskStatus::Completed,
                        copy_details: Vec::new(),
                    })
                    .collect(),
                reconciliation: false,
            })
        }
        RECONCILE_REPORTS_TOOL_NAME => {
            let reconciliation: ReportReconciliation =
                serde_json::from_value(state.arguments.as_ref()?.clone()).ok()?;
            let label = format!(
                "reconcile_reports · {} · {}",
                count_label(
                    reconciliation.disagreements.len(),
                    "disagreement",
                    "disagreements"
                ),
                count_label(
                    reconciliation.decisions.len() + reconciliation.unavailable_decisions.len(),
                    "decision",
                    "decisions"
                )
            );
            let mut rows = Vec::new();
            for disagreement in reconciliation.disagreements {
                let mut copy_details = vec![disagreement.summary];
                copy_details.extend(
                    disagreement
                        .positions
                        .into_iter()
                        .map(|position| format!("{}: {}", position.label, position.position)),
                );
                copy_details.push(match &disagreement.resolution {
                    ReportDisagreementResolution::RepositoryEvidence { evidence, .. } => {
                        evidence.clone()
                    }
                    ReportDisagreementResolution::ExplicitUserRequirement { requirement } => {
                        requirement.clone()
                    }
                    ReportDisagreementResolution::RecordedUserDecisions {
                        decision_ids,
                        application,
                    } => format!(
                        "{}: {application}",
                        decision_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    ReportDisagreementResolution::BaselinePrecedence {
                        worker_id,
                        application,
                    } => format!("{worker_id}: {application}"),
                    ReportDisagreementResolution::RootQuestion { reason } => reason.clone(),
                });
                rows.push(PresentedListRow {
                    marker: ListMarker::Disagreement,
                    text: disagreement.id,
                    annotation: format!(
                        " · {} · {}",
                        disagreement_classification_label(disagreement.classification),
                        disagreement_resolution_label(&disagreement.resolution)
                    ),
                    muted: false,
                    copy_details,
                });
            }
            for decision in reconciliation.decisions {
                rows.push(PresentedListRow {
                    marker: ListMarker::Status(recorded_decision_icon(&decision.disposition)),
                    text: decision.decision_id.to_string(),
                    annotation: format!(
                        " · recorded · {}",
                        recorded_decision_disposition_label(&decision.disposition)
                    ),
                    muted: false,
                    copy_details: vec![match decision.disposition {
                        RecordedDecisionDisposition::Applied { explanation } => explanation,
                        RecordedDecisionDisposition::ObjectivelyInapplicable { evidence } => {
                            evidence
                        }
                    }],
                });
            }
            for decision in reconciliation.unavailable_decisions {
                rows.push(PresentedListRow {
                    marker: ListMarker::Status(unavailable_decision_icon(&decision.disposition)),
                    text: decision.unavailable_decision_id.to_string(),
                    annotation: format!(
                        " · unavailable · {}",
                        unavailable_decision_disposition_label(&decision.disposition)
                    ),
                    muted: false,
                    copy_details: vec![match decision.disposition {
                        UnavailableDecisionDisposition::RootQuestionRequired { reason } => reason,
                        UnavailableDecisionDisposition::ObjectivelyInapplicable { evidence } => {
                            evidence
                        }
                    }],
                });
            }
            Some(PresentedNativeList {
                label,
                status,
                explanation: None,
                rows,
                reconciliation: true,
            })
        }
        _ => None,
    }
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn disagreement_classification_label(
    classification: ReportDisagreementClassification,
) -> &'static str {
    match classification {
        ReportDisagreementClassification::Factual => "factual",
        ReportDisagreementClassification::PreferenceTradeoff => "preference tradeoff",
    }
}

fn disagreement_resolution_label(resolution: &ReportDisagreementResolution) -> &'static str {
    match resolution {
        ReportDisagreementResolution::RepositoryEvidence { .. } => "repository evidence",
        ReportDisagreementResolution::ExplicitUserRequirement { .. } => "user requirement",
        ReportDisagreementResolution::RecordedUserDecisions { .. } => "recorded decisions",
        ReportDisagreementResolution::BaselinePrecedence { .. } => "baseline precedence",
        ReportDisagreementResolution::RootQuestion { .. } => "root question",
    }
}

fn recorded_decision_disposition_label(disposition: &RecordedDecisionDisposition) -> &'static str {
    match disposition {
        RecordedDecisionDisposition::Applied { .. } => "applied",
        RecordedDecisionDisposition::ObjectivelyInapplicable { .. } => "objectively inapplicable",
    }
}

fn unavailable_decision_disposition_label(
    disposition: &UnavailableDecisionDisposition,
) -> &'static str {
    match disposition {
        UnavailableDecisionDisposition::RootQuestionRequired { .. } => "root question required",
        UnavailableDecisionDisposition::ObjectivelyInapplicable { .. } => {
            "objectively inapplicable"
        }
    }
}
