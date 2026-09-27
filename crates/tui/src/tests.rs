//! Shared fixtures and per-area TUI tests.
#[path = "tests/hints.rs"]
mod hints_tests;
#[path = "tests/keys.rs"]
mod keys_tests;
#[path = "tests/layout.rs"]
mod layout_tests;
#[path = "tests/overlays.rs"]
mod overlays_tests;
#[path = "tests/rationalization.rs"]
mod rationalization_tests;
#[path = "tests/transcript.rs"]
mod transcript_tests;

#[path = "acp_input_tests.rs"]
mod acp_input_tests;
#[path = "build_child_tests.rs"]
mod build_child_tests;
#[path = "file_reference_tests.rs"]
mod file_reference_tests;
#[path = "fold_tests.rs"]
mod fold_tests;
#[path = "inline_web_tests.rs"]
mod inline_web_tests;
#[path = "input_contract_tests.rs"]
mod input_contract_tests;
#[path = "mode_tests.rs"]
mod mode_tests;
#[path = "selection_tests.rs"]
mod selection_tests;
#[path = "skill_tests.rs"]
mod skill_tests;
#[path = "status_presentation_tests.rs"]
mod status_presentation_tests;
#[path = "subtask_batch_tests.rs"]
mod subtask_batch_tests;
#[path = "timed_tail_tests.rs"]
mod timed_tail_tests;
#[path = "tool_adapter_tests.rs"]
mod tool_adapter_tests;
#[path = "turn_progress_tests.rs"]
mod turn_progress_tests;

use super::*;
use crate::agent_transcript::AgentTranscriptReducer;
use crate::app::{ActiveSelection, SelectionScope};
use crate::command::COMMANDS;
use crate::presentation::{PresentationBlockKind, PresentationRole};
use ratatui::crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Position,
    style::{Color, Modifier},
};
use rig_core::message::{
    Document, DocumentSourceKind, Reasoning, ReasoningContent, Text, ToolCall, ToolCallId,
    ToolFunction, ToolResult, ToolResultContent,
};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOperation;
use zevria_foundation::FileChangeOutput;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::ModelRole;
use zevria_foundation::QUESTION_TOOL_NAME;
use zevria_foundation::QuestionAnswer;
use zevria_foundation::QuestionAnswerValue;
use zevria_foundation::QuestionOption;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequest;
use zevria_foundation::QuestionRequestId;
use zevria_foundation::QuestionResponse;
use zevria_foundation::RECONCILE_REPORTS_TOOL_NAME;
use zevria_foundation::SUBMIT_PLAN_TOOL_NAME;
use zevria_foundation::TASK_TOOL_NAME;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::ToolResultDetail;
use zevria_foundation::ToolResultMetadata;
use zevria_foundation::TurnId;
use zevria_foundation::subtask::SubtaskDescriptor;
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskLaunchMetadata;
use zevria_foundation::subtask::SubtaskStatus;
use zevria_instructions::SkillApplication;
use zevria_instructions::SkillInvocation;
use zevria_instructions::SkillMeta;
use zevria_instructions::SkillName;
use zevria_instructions::SkillSnapshot;
use zevria_model::CompactionBackend;
use zevria_model::CompactionCheckpoint;
use zevria_model::CompactionTrigger;
use zevria_model::ContextTokenSnapshot;
use zevria_model::ContextTokenSource;
use zevria_model::OwnedModelRequestItem;
use zevria_model::ProviderReplay;
use zevria_model::SUMMARY_PREFIX;
use zevria_model::TokenUsage;
use zevria_session_api::AgentRunStreamState;
use zevria_session_api::SessionStreamBatch;
use zevria_session_api::SessionUpdate;
use zevria_session_api::TranscriptEdit;
use zevria_session_api::TranscriptEditReplacement;
use zevria_session_api::TranscriptEditTarget;
use zevria_session_api::session_event_channel;
use zevria_transcript::AGENT_RUN_TRANSCRIPT_VERSION;
use zevria_transcript::AgentRunTranscriptHeader;
use zevria_transcript::AgentRunTranscriptRecord;
use zevria_transcript::transcript::SessionSummary;
use zevria_tui_widgets::render_startup_frame;
use zevria_workflow::AgentPlanEntry;
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentRunEvent;
use zevria_workflow::AgentRunId;
use zevria_workflow::AgentRunLocation;
use zevria_workflow::AgentRunOutcome;
use zevria_workflow::AgentRunRepair;
use zevria_workflow::AgentRunStatus;
use zevria_workflow::AgentStructuredPlan;
use zevria_workflow::AgentUnavailableDecisionId;
use zevria_workflow::AgentUsage;
use zevria_workflow::AgentUserDecisionId;
use zevria_workflow::CLAUDE_PLAN_HANDOFF_PLAN_ID;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::PlanArtifact;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanHandoff;
use zevria_workflow::PlanId;
use zevria_workflow::PlanRecord;
use zevria_workflow::PlanResolution;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;
use zevria_workflow::RecordedDecisionAccounting;
use zevria_workflow::RecordedDecisionDisposition;
use zevria_workflow::ReportDisagreement;
use zevria_workflow::ReportDisagreementClassification;
use zevria_workflow::ReportDisagreementResolution;
use zevria_workflow::ReportPosition;
use zevria_workflow::ReportReconciliation;
use zevria_workflow::RepositoryEvidenceResolutionKind;
use zevria_workflow::UnavailableDecisionAccounting;
use zevria_workflow::UnavailableDecisionDisposition;

impl App {
    fn reduce_without_effects(&mut self, event: SessionEvent) {
        assert!(
            self.reduce(event).is_empty(),
            "fixture must explicitly handle reducer effects"
        );
    }
}

// Full help and short footers are generated from the same contextual catalogue.
fn context_help(app: &App) -> String {
    zevria_tui_input::hints::help_lines(app.surface().context(), &app.hint_eligibility())
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

// Exercise the production native adapter and cached presentation/layout path.
fn layout_native_message(
    message: &Message,
    states: Option<&[Option<crate::app::ToolCallState>]>,
    lines: &mut Vec<ratatui::text::Line<'static>>,
    width: u16,
    selected: Option<usize>,
) -> Option<crate::viewport::RowRange> {
    let mut entry = HistoryEntry::from_message(message.clone(), ToolCallStatus::Finished)?;
    if let HistoryEntry::Conversation(conversation) = &mut entry {
        for (index, block) in conversation.blocks.iter_mut().enumerate() {
            if let PresentationBlockKind::Tool(crate::presentation::PresentedTool::Native {
                state,
                ..
            }) = &mut block.kind
                && let Some(Some(supplied)) = states.and_then(|states| states.get(index))
            {
                let arguments = state.arguments.clone();
                **state = supplied.clone();
                state.arguments = arguments;
            }
        }
    }
    if let HistoryEntry::Conversation(conversation) = &mut entry {
        let mut block_ids = crate::presentation::BlockIdAllocator::default();
        block_ids.identify(conversation);
        let mut blocks = Vec::new();
        for block in std::mem::take(&mut conversation.blocks) {
            let children = block
                .native_tool()
                .map(|(_, state)| state.subtasks.clone())
                .unwrap_or_default();
            let parent = block.id;
            blocks.push(block);
            for (entry_index, descriptor) in children {
                blocks.push(crate::presentation::PresentationBlock {
                    id: block_ids.allocate(),
                    revision: 0,
                    role: Some(crate::presentation::PresentationRole::Assistant),
                    prompt_group: None,
                    prompt: None,
                    visibility: crate::presentation::BlockVisibility::Always,
                    kind: PresentationBlockKind::Subtask {
                        parent,
                        entry_index,
                        descriptor,
                    },
                });
            }
        }
        conversation.blocks = blocks;
    }
    let mut cache = crate::layout::ConversationCache::default();
    cache.refresh(
        &[entry],
        selected.map(|content_index| ActiveSelection {
            selection: Selection {
                history_index: 0,
                content_index,
            },
            scope: SelectionScope::Block,
        }),
        width,
        false,
        &crate::app::FoldState::default(),
    );
    let layout = &cache.entries()[0];
    lines.extend(layout.lines.clone());
    layout.selection
}

const TEST_TURN_ID: TurnId = TurnId::new(1);
const READABLE_REASONING_SUMMARY: &str = "summary before opaque content";
const READABLE_REASONING_TEXT: &str = "text after opaque content";
const MIXED_ENCRYPTED_REASONING_PAYLOAD: &str = "ciphertext-mixed-sentinel";
const MIXED_REDACTED_REASONING_PAYLOAD: &str = "redacted-mixed-sentinel";
const MIXED_REASONING_SIGNATURE: &str = "signature-mixed-sentinel";
const OPAQUE_ENCRYPTED_REASONING_PAYLOAD: &str = "ciphertext-only-sentinel";
const OPAQUE_REDACTED_REASONING_PAYLOAD: &str = "redacted-only-sentinel";

fn configured_profiles() -> Vec<(ModelRole, ModelProfileRef)> {
    vec![
        (
            ModelRole::Build,
            ModelProfileRef::new("provider-build", "build-model"),
        ),
        (
            ModelRole::Plan,
            ModelProfileRef::new("provider-plan", "plan-model"),
        ),
        (
            ModelRole::Review,
            ModelProfileRef::new("provider-review", "review-model"),
        ),
        (
            ModelRole::Explore,
            ModelProfileRef::new("provider-explore", "explore-model"),
        ),
    ]
}

fn configured_app() -> App {
    App::new().with_model_profiles(configured_profiles())
}

fn deterministic_test_workspace() -> PathBuf {
    let current = std::env::current_dir().expect("current test directory");
    current
        .ancestors()
        .last()
        .expect("filesystem root")
        .join("zevria-tui-test-workspace")
}

fn test_session_views(root: App) -> SessionViews {
    test_session_views_in(root, deterministic_test_workspace())
}

fn test_session_views_in(root: App, workspace: impl Into<PathBuf>) -> SessionViews {
    SessionViews::new(root, workspace.into())
}

fn test_plan_artifact() -> PlanArtifact {
    let title = "Durable approval workflow";
    PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: title.to_string(),
        markdown: format!(
            "# {title}\n\n## Goal\nOwn approval in the engine.\n\n## Decisions\nUse typed state.\n\n## Implementation\nBuild it.\n\n## Validation\nTest it.\n\n## Risks\nNone."
        ),
        source_turn_id: TEST_TURN_ID,
    }
}

/// Establish valid reducer state before an intra-turn event. Empty accepted
/// messages create no visible row, so characterization fixtures keep their
/// original history shape while using the real lifecycle transition.
fn start_empty_turn(app: &mut App, turn_id: TurnId, mode: SessionMode) {
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id,
        message: Message::User {
            content: Vec::new(),
        },
        mode,
    });
}

fn toggle_mode_with_ack(app: &mut App) {
    let Some(UiAction::SetMode { request_id, mode }) = app.handle_event(key(KeyCode::BackTab))
    else {
        panic!("idle mode shortcut must request management selection");
    };
    app.reduce_without_effects(SessionEvent::ModeResult {
        request_id,
        result: zevria_session_api::ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    });
}

fn apply_turn_event(app: &mut App, event: SessionEvent) {
    match &event {
        SessionEvent::TurnRejected { .. }
        | SessionEvent::TurnStarted { .. }
        | SessionEvent::EnsembleStarted { .. }
        | SessionEvent::PlanHandoffStarted { .. }
        | SessionEvent::CompactionStarted { .. } => {}
        SessionEvent::CompactionCompleted {
            turn_id, trigger, ..
        } if !app.is_busy() => app.reduce_without_effects(SessionEvent::CompactionStarted {
            turn_id: *turn_id,
            trigger: *trigger,
        }),
        _ if !app.is_busy() => {
            let mode = app.next_mode();
            start_empty_turn(app, TEST_TURN_ID, mode);
        }
        _ => {}
    }
    app.reduce_without_effects(event);
}

fn apply_transport_update(views: &mut SessionViews, update: SessionUpdate) {
    match update {
        SessionUpdate::Lifecycle(event) => views.apply(event),
        SessionUpdate::Streams(batch) => views.apply_streams(&batch),
    }
}

fn apply_agent_event(app: &mut App, transcript: &mut AgentTranscriptReducer, event: AgentRunEvent) {
    let change = transcript.apply_event(app.conversation_projection_mut(), event);
    assert!(app.apply_conversation_change(change).is_empty());
}

fn apply_agent_preview(
    app: &mut App,
    transcript: &mut AgentTranscriptReducer,
    event: AgentRunEvent,
) {
    let change = transcript.apply_preview(app.conversation_projection_mut(), event);
    assert!(app.apply_conversation_change(change).is_empty());
}

fn reconcile_agent_report(app: &mut App, transcript: &mut AgentTranscriptReducer, report: &str) {
    let change = transcript.reconcile_report(app.conversation_projection_mut(), report);
    assert!(app.apply_conversation_change(change).is_empty());
}

fn assistant_history_count(app: &App) -> usize {
    app.history()
        .iter()
        .filter(|entry| conversation_has_role(entry, PresentationRole::Assistant))
        .count()
}

fn conversation_has_role(entry: &HistoryEntry, role: PresentationRole) -> bool {
    matches!(
        entry,
        HistoryEntry::Conversation(conversation)
            if conversation.blocks.iter().any(|block| block.role == Some(role))
    )
}

fn assistant_presentation_texts(app: &App) -> Vec<&str> {
    app.history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(conversation) => Some(&conversation.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match &block.kind {
            PresentationBlockKind::Text { text, .. }
                if block.role == Some(PresentationRole::Assistant) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn assistant_reasoning_texts(app: &App) -> Vec<&str> {
    app.history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(conversation) => Some(&conversation.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match &block.kind {
            PresentationBlockKind::Reasoning { parts } => parts.last().map(String::as_str),
            _ => None,
        })
        .collect()
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn ctrl_enter() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
}

/// Enter Insert mode so subsequent printable keys edit the input box.
fn enter_insert(app: &mut App) {
    if !app.interaction().is_insert() {
        assert_eq!(app.handle_event(key(KeyCode::Char('i'))), None);
    }
    assert!(app.interaction().is_insert());
}

fn press_v(app: &mut App) {
    assert!(app.interaction().is_normal());
    assert_eq!(app.handle_event(key(KeyCode::Char('v'))), None);
}

fn double_escape(app: &mut App) {
    let now = Instant::now();
    assert_eq!(app.handle_event_at(key(KeyCode::Esc), now), None);
    assert!(app.interaction().is_normal());
    assert_eq!(app.selection(), None);
    assert_eq!(
        app.handle_event_at(key(KeyCode::Esc), now + Duration::from_millis(100)),
        None
    );
}

fn cursor(history_index: usize, content_index: usize) -> Option<Selection> {
    Some(Selection {
        history_index,
        content_index,
    })
}

fn history_message(message: Message) -> HistoryEntry {
    HistoryEntry::from_message(message, ToolCallStatus::Finished)
        .expect("test message should contain visible content")
}

fn checkpoint() -> CompactionCheckpoint {
    CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user(format!(
            "{SUMMARY_PREFIX}\nhidden summary"
        )))],
        vec!["visible prompt".to_string()],
    )
    .expect("checkpoint")
}

fn tool_call(
    id: &str,
    call_id: Option<&str>,
    name: &str,
    arguments: serde_json::Value,
) -> AssistantContent {
    let function = ToolFunction::new(name.to_string(), arguments);
    AssistantContent::ToolCall(match call_id {
        Some(call_id) => ToolCall::from_dual_wire(id, call_id, function),
        None => ToolCall::new(ToolCallId::new_or_mint(id), function),
    })
}

fn assistant_message(content: Vec<AssistantContent>) -> Message {
    Message::Assistant { id: None, content }
}

fn mixed_reasoning_message() -> Message {
    assistant_message(vec![AssistantContent::Reasoning(Reasoning {
        id: Some("reasoning-mixed".to_string()),
        content: vec![
            ReasoningContent::Encrypted(MIXED_ENCRYPTED_REASONING_PAYLOAD.to_string()),
            ReasoningContent::Summary(READABLE_REASONING_SUMMARY.to_string()),
            ReasoningContent::Redacted {
                data: MIXED_REDACTED_REASONING_PAYLOAD.to_string(),
            },
            ReasoningContent::Text {
                text: READABLE_REASONING_TEXT.to_string(),
                signature: Some(MIXED_REASONING_SIGNATURE.to_string()),
            },
        ],
    })])
}

fn opaque_reasoning_message() -> Message {
    assistant_message(vec![AssistantContent::Reasoning(Reasoning {
        id: Some("reasoning-opaque".to_string()),
        content: vec![
            ReasoningContent::Encrypted(OPAQUE_ENCRYPTED_REASONING_PAYLOAD.to_string()),
            ReasoningContent::Redacted {
                data: OPAQUE_REDACTED_REASONING_PAYLOAD.to_string(),
            },
        ],
    })])
}

fn assert_opaque_reasoning_absent(text: &str, payloads: &[&str]) {
    for payload in payloads {
        assert!(
            !text.contains(payload),
            "opaque reasoning payload leaked: {payload:?}"
        );
    }
    for kind in ["encrypted", "redacted"] {
        let marker = format!("[{kind} reasoning]");
        assert!(
            !text.contains(&marker),
            "opaque reasoning marker leaked: {marker:?}"
        );
    }
}

fn tool_result_message(
    id: &str,
    call_id: Option<&str>,
    name: &str,
    output: impl Into<String>,
) -> Message {
    let content = vec![ToolResultContent::text(output)];
    let result = match call_id {
        Some(call_id) => UserContent::tool_result_with_call_id(id, call_id, name, content),
        None => UserContent::tool_result(id, name, content),
    };
    Message::User {
        content: vec![result],
    }
}

fn file_metadata(
    id: &str,
    call_id: Option<&str>,
    tool_name: &str,
    outcome: ToolCallOutcome,
    file_changes: Vec<FileChangeOutput>,
) -> ToolResultMetadata {
    ToolResultMetadata {
        diagnostic: None,
        id: call_id.unwrap_or(id).to_string(),
        call_id: call_id.map(str::to_string),
        tool_name: tool_name.to_string(),
        outcome,
        detail: (!file_changes.is_empty()).then_some(ToolResultDetail::FileChanges(file_changes)),
    }
}

fn tool_status(app: &App, history_index: usize, content_index: usize) -> ToolCallStatus {
    let HistoryEntry::Conversation(entry) = &app.history()[history_index] else {
        panic!("expected a message entry")
    };
    entry.blocks[content_index]
        .native_tool()
        .expect("expected tool-call state")
        .1
        .status
}

/// Flatten the complete cached transcript after rendering, not just the visible viewport.
fn laid_out_transcript_text(app: &App) -> String {
    app.view_cache()
        .entries()
        .iter()
        .flat_map(|entry| &entry.lines)
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Draw the app to an off-screen buffer and flatten it to text.
fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
    rendered_rows(app, width, height).concat()
}

fn rendered_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn rendered_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn buffer_row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn assert_blank_conversation_row(buffer: &ratatui::buffer::Buffer, y: u16) {
    // Include the gutter and selection band, but not the scrollbar.
    for x in 0..buffer.area.width.saturating_sub(1) {
        let cell = &buffer[(x, y)];
        assert_eq!(cell.symbol(), " ", "gap at ({x}, {y})");
        assert_eq!(cell.bg, ZEVRIA_DARK.surfaces.canvas, "gap at ({x}, {y})");
    }
}

fn conversation_content_area(
    buffer: &ratatui::buffer::Buffer,
    inspect: bool,
) -> ratatui::layout::Rect {
    let lower = if inspect {
        crate::frame_layout::LowerSurface::None
    } else {
        crate::frame_layout::LowerSurface::Composer {
            requested_height: 3,
        }
    };
    crate::frame_layout::FrameLayout::compute(buffer.area, inspect, lower, false)
        .conversation_content
}

fn assert_conversation_row_background(
    buffer: &ratatui::buffer::Buffer,
    content: ratatui::layout::Rect,
    y: u16,
    background: Color,
) {
    assert!((content.y..content.bottom()).contains(&y));
    let mut x = 0;
    while x < buffer.area.width {
        let cell = &buffer[(x, y)];
        let expected = if (content.x..content.right()).contains(&x) {
            background
        } else {
            // Surfaces exclude role accents, gutters, and the scrollbar.
            ZEVRIA_DARK.surfaces.canvas
        };
        assert_eq!(cell.bg, expected, "background at ({x}, {y})");
        // Ratatui does not send cells covered by a wide glyph to TestBackend;
        // the leading cell styles the whole glyph. Still check every space.
        x += crate::text::display_width(cell.symbol()).max(1) as u16;
    }
}

fn full_width_message_separator_rows(buffer: &ratatui::buffer::Buffer, inspect: bool) -> Vec<u16> {
    let content = conversation_content_area(buffer, inspect);
    (content.y..content.bottom())
        .filter(|&y| {
            content.width > 0
                && (content.x..content.right()).all(|x| buffer[(x, y)].symbol() == "─")
        })
        .collect()
}

fn bottom_row_cells(buffer: &ratatui::buffer::Buffer) -> Vec<ratatui::buffer::Cell> {
    let y = buffer.area.height.saturating_sub(1);
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].clone())
        .collect()
}

fn line_text(line: &ratatui::text::Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_reconciliation_lines(
    arguments: serde_json::Value,
    state: crate::app::ToolCallState,
) -> Vec<ratatui::text::Line<'static>> {
    let message = assistant_message(vec![tool_call(
        "reconcile-render-test",
        None,
        RECONCILE_REPORTS_TOOL_NAME,
        arguments,
    )]);
    let states = vec![Some(state)];
    let mut lines = Vec::new();
    layout_native_message(&message, Some(&states), &mut lines, 160, None);
    lines
}

fn reconciliation_text(lines: &[ratatui::text::Line<'static>]) -> String {
    lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
}

fn test_reconciliation() -> ReportReconciliation {
    let applied_id = AgentUserDecisionId::from_question(
        &QuestionRequestId::new("reconcile-render-decisions"),
        "applied",
    );
    let inapplicable_id = AgentUserDecisionId::from_question(
        &QuestionRequestId::new("reconcile-render-decisions"),
        "inapplicable",
    );
    let root_question_id = AgentUnavailableDecisionId::from_request(&QuestionRequestId::new(
        "reconcile-render-unavailable-root",
    ));
    let unavailable_id = AgentUnavailableDecisionId::from_request(&QuestionRequestId::new(
        "reconcile-render-unavailable-inapplicable",
    ));
    let positions = || {
        vec![
            ReportPosition {
                label: "first".to_string(),
                position: "FIRST_POSITION_SENTINEL".to_string(),
            },
            ReportPosition {
                label: "second".to_string(),
                position: "SECOND_POSITION_SENTINEL".to_string(),
            },
        ]
    };

    ReportReconciliation {
        disagreements: vec![
            ReportDisagreement {
                id: "facts".to_string(),
                summary: "SUMMARY_SENTINEL_FACTS".to_string(),
                positions: positions(),
                classification: ReportDisagreementClassification::Factual,
                resolution: ReportDisagreementResolution::RepositoryEvidence {
                    kind: RepositoryEvidenceResolutionKind::FactualClaim,
                    evidence: "EVIDENCE_SENTINEL_FACTS".to_string(),
                },
            },
            ReportDisagreement {
                id: "requirement".to_string(),
                summary: "SUMMARY_SENTINEL_REQUIREMENT".to_string(),
                positions: positions(),
                classification: ReportDisagreementClassification::PreferenceTradeoff,
                resolution: ReportDisagreementResolution::ExplicitUserRequirement {
                    requirement: "REQUIREMENT_SENTINEL".to_string(),
                },
            },
            ReportDisagreement {
                id: "recorded".to_string(),
                summary: "SUMMARY_SENTINEL_RECORDED".to_string(),
                positions: positions(),
                classification: ReportDisagreementClassification::Factual,
                resolution: ReportDisagreementResolution::RecordedUserDecisions {
                    decision_ids: vec![applied_id.clone()],
                    application: "APPLICATION_SENTINEL".to_string(),
                },
            },
            ReportDisagreement {
                id: "question".to_string(),
                summary: "SUMMARY_SENTINEL_QUESTION".to_string(),
                positions: positions(),
                classification: ReportDisagreementClassification::PreferenceTradeoff,
                resolution: ReportDisagreementResolution::RootQuestion {
                    reason: "REASON_SENTINEL".to_string(),
                },
            },
        ],
        decisions: vec![
            RecordedDecisionAccounting {
                decision_id: applied_id,
                disposition: RecordedDecisionDisposition::Applied {
                    explanation: "EXPLANATION_SENTINEL_APPLIED".to_string(),
                },
            },
            RecordedDecisionAccounting {
                decision_id: inapplicable_id,
                disposition: RecordedDecisionDisposition::ObjectivelyInapplicable {
                    evidence: "EVIDENCE_SENTINEL_INAPPLICABLE".to_string(),
                },
            },
        ],
        unavailable_decisions: vec![
            UnavailableDecisionAccounting {
                unavailable_decision_id: root_question_id,
                disposition: UnavailableDecisionDisposition::RootQuestionRequired {
                    reason: "UNAVAILABLE_REASON_SENTINEL".to_string(),
                },
            },
            UnavailableDecisionAccounting {
                unavailable_decision_id: unavailable_id,
                disposition: UnavailableDecisionDisposition::ObjectivelyInapplicable {
                    evidence: "UNAVAILABLE_EVIDENCE_SENTINEL".to_string(),
                },
            },
        ],
    }
}

fn test_tool_result(id: &str, name: &str, output: &str) -> ToolResult {
    match tool_result_message(id, None, name, output) {
        Message::User { content } => match content.into_iter().next() {
            Some(UserContent::ToolResult(result)) => result,
            _ => panic!("expected a tool result"),
        },
        _ => panic!("expected a user tool-result message"),
    }
}

fn cursor_visible_after_render(app: &mut App, width: u16, height: u16) -> bool {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal.backend().cursor_visible()
}

fn cursor_position_after_render(app: &mut App, width: u16, height: u16) -> Position {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal.backend().cursor_position()
}

fn handoff_stream_update(rows: usize, assistant_snapshot: bool) -> SessionEvent {
    let message = Message::assistant("Implementation stream row.\n".repeat(rows));
    if assistant_snapshot {
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: zevria_content::AssistantStreamSnapshot {
                message: Some(message),
                attempt: None,
            },
        }
    } else {
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: message.into(),
        }
    }
}

fn streaming_handoff_app(with_compaction: bool) -> App {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::PlanHandoffStarted {
        turn_id: TEST_TURN_ID,
        handoff: PlanHandoff::new(test_plan_artifact(), "source-session"),
    });
    if with_compaction {
        app.reduce_without_effects(SessionEvent::CompactionStarted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::AutomaticMidTurn,
        });
        app.reduce_without_effects(SessionEvent::CompactionCompleted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::AutomaticMidTurn,
            backend: CompactionBackend::LocalSummary,
        });
        assert!(matches!(app.history()[1], HistoryEntry::CompactionDivider));
    }
    app.reduce_without_effects(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        display_attempt_id: None,
        message: assistant_message(vec![
            AssistantContent::text("Committed implementation row.\n".repeat(40)),
            tool_call(
                "handoff-tool",
                None,
                "command",
                json!({"command": "echo implemented"}),
            ),
        ]),
    });
    app.reduce_without_effects(handoff_stream_update(8, true));
    rendered_text(&mut app, 80, 18);
    let conversation_index = 1 + usize::from(with_compaction);
    assert_eq!(app.view_cache().entries().len(), conversation_index + 1);
    assert!(matches!(
        app.history()[conversation_index],
        HistoryEntry::Conversation(_)
    ));
    let conversation_start = app.view_cache().entries()[..conversation_index]
        .iter()
        .map(crate::layout::EntryLayout::extent)
        .sum::<usize>();
    assert!(
        app.view_scroll() > conversation_start,
        "committed content below the handoff must prevent bottom-clamping from hiding a jump"
    );
    assert!(app.view_follow());
    app
}

fn ctrl_e(app: &mut App) -> Option<UiAction> {
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    )))
}

fn editable_ensemble_start(
    run_id: &str,
    workflow: EnsembleWorkflow,
    prompt: &str,
) -> EnsembleStart {
    EnsembleStart {
        run_id: EnsembleRunId::from_string(run_id),
        workflow,
        prompt: prompt.into(),
        agents: vec![AgentRunDescriptor {
            id: AgentRunId::from_string(format!("{run_id}-agent")),
            agent: "fake".to_string(),
            label: "Fake ACP".to_string(),
            safe_mode: "read-only".to_string(),
        }],
    }
}

fn idle_app_with_ensemble(start: EnsembleStart) -> App {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start,
        resumed: false,
    });
    app.reduce_without_effects(SessionEvent::TurnRecovered {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
    });
    app
}

fn start_timed_tail(app: &mut App, event: Option<SessionEvent>) {
    let now = std::time::Instant::now();
    assert!(
        app.reduce_at(
            SessionEvent::TurnStarted {
                turn_id: TEST_TURN_ID,
                message: Message::User {
                    content: Vec::new()
                },
                mode: SessionMode::Build,
            },
            now
        )
        .is_empty()
    );
    if let Some(event) = event {
        assert!(app.reduce_at(event, now).is_empty());
    }
}

// --- subtask rows, results, and subsession navigation ---

fn child_launch(id: &str, title: &str) -> SubtaskLaunchMetadata {
    SubtaskLaunchMetadata {
        id: SubtaskId::new(id),
        title: title.into(),
        kind: SubtaskKind::Explore,
        workspace: None,
    }
}

fn child_descriptor(id: &str, title: &str) -> SubtaskDescriptor {
    SubtaskDescriptor {
        id: SubtaskId::new(id),
        parent_session_id: "root".to_string(),
        title: title.to_string(),
        kind: SubtaskKind::Explore,
        workspace: None,
        status: SubtaskStatus::Starting,
    }
}

fn launch_assistant(call_id: &str, title: &str, prompt: &str) -> Message {
    assistant_message(vec![tool_call(
        &format!("fc-{call_id}"),
        Some(call_id),
        "launch_subtasks",
        json!({"tasks":[{"title": title, "prompt": prompt, "type": "explore"}]}),
    )])
}

fn launch_metadata(
    call_id: &str,
    child_id: &str,
    title: &str,
    outcome: ToolCallOutcome,
) -> ToolResultMetadata {
    ToolResultMetadata {
        diagnostic: None,
        id: call_id.to_string(),
        call_id: Some(call_id.to_string()),
        tool_name: "launch_subtasks".to_string(),
        outcome,
        detail: Some(ToolResultDetail::Subtasks(vec![
            zevria_foundation::SubtaskEntryMetadata {
                index: 0,
                status: match outcome {
                    ToolCallOutcome::Success => SubtaskStatus::Completed,
                    ToolCallOutcome::Cancelled => SubtaskStatus::Cancelled,
                    _ => SubtaskStatus::Failed,
                },
                launch: Some(SubtaskLaunchMetadata {
                    id: SubtaskId::new(child_id),
                    title: title.to_string(),
                    kind: SubtaskKind::Explore,
                    workspace: None,
                }),
            },
        ])),
    }
}

fn attached_subtask_status(app: &App, id: &SubtaskId) -> Option<SubtaskStatus> {
    app.history().iter().rev().find_map(|entry| {
        let HistoryEntry::Conversation(entry) = entry else {
            return None;
        };
        entry.blocks.iter().find_map(|block| {
            let (call, state) = block.native_tool()?;
            (call.function.name == "launch_subtasks")
                .then(|| {
                    state
                        .subtasks
                        .values()
                        .find(|descriptor| &descriptor.id == id)
                        .map(|descriptor| descriptor.status)
                })
                .flatten()
        })
    })
}

fn ctrl(ch: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
}

fn two_child_launch_arguments() -> serde_json::Value {
    json!({"tasks": [
        {"title": "first child", "prompt": "SECRET first prompt", "type": "explore"},
        {"title": "second child", "prompt": "SECRET second prompt", "type": "explore"}
    ]})
}

fn unlaunched_metadata(call_id: &str, outcome: ToolCallOutcome) -> ToolResultMetadata {
    file_metadata(
        call_id,
        Some(call_id),
        "launch_subtasks",
        outcome,
        Vec::new(),
    )
}

fn launch_state_with_result(outcome: ToolCallOutcome, output: &str) -> crate::app::ToolCallState {
    crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Finished,
        result: Some(ToolResult {
            call: ToolCallId::new_or_mint("call-diagnostic"),
            provider: None,
            name: "launch_subtasks".to_string(),
            content: vec![ToolResultContent::text(output)],
        }),
        metadata: Some(unlaunched_metadata("call-diagnostic", outcome)),
        subtasks: Default::default(),
    }
}

fn navigation_agent(id: &str) -> AgentRunDescriptor {
    AgentRunDescriptor {
        id: AgentRunId::from_string(id),
        agent: "fake".to_string(),
        label: "Fake ACP".to_string(),
        safe_mode: "read-only".to_string(),
    }
}

fn navigation_ensemble_start(
    run_id: &EnsembleRunId,
    descriptor: &AgentRunDescriptor,
) -> EnsembleStart {
    EnsembleStart {
        run_id: run_id.clone(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review".into(),
        agents: vec![descriptor.clone()],
    }
}

fn historical_subtask_launch(id: &str) -> TranscriptItem {
    TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::user("subtask launched"),
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: format!("call-{id}"),
            call_id: Some(format!("call-{id}")),
            tool_name: "launch_subtasks".to_string(),
            outcome: ToolCallOutcome::Success,
            detail: Some(ToolResultDetail::Subtasks(vec![
                zevria_foundation::SubtaskEntryMetadata {
                    index: 0,
                    status: SubtaskStatus::Completed,
                    launch: Some(SubtaskLaunchMetadata {
                        id: SubtaskId::new(id),
                        title: format!("Subtask {id}"),
                        kind: SubtaskKind::Explore,
                        workspace: None,
                    }),
                },
            ])),
        }],
    }
}

fn historical_agent_records(
    start: &EnsembleStart,
    descriptor: &AgentRunDescriptor,
) -> Vec<AgentRunTranscriptRecord> {
    vec![AgentRunTranscriptRecord::Header {
        header: AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            descriptor: descriptor.clone(),
            prompt: start.prompt.clone(),
        },
    }]
}

fn session_summary(id: &str, preview: Option<&str>) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        path: PathBuf::from(format!("/tmp/sessions/{id}.jsonl")),
        modified: SystemTime::now(),
        preview: preview.map(str::to_string),
    }
}

fn question_request(id: &str) -> QuestionRequest {
    QuestionRequest {
        id: QuestionRequestId::new(id),
        questions: vec![
            QuestionPrompt {
                id: "scope".to_string(),
                header: "Scope".to_string(),
                question: "How broad should this change be?".to_string(),
                options: vec![
                    QuestionOption {
                        label: "Focused".to_string(),
                        description: "Change only the requested workflow.".to_string(),
                    },
                    QuestionOption {
                        label: "Broad".to_string(),
                        description: "Include adjacent cleanup.".to_string(),
                    },
                ],
                kind: QuestionPromptKind::SingleSelect { allow_other: true },
                required: true,
                default: None,
            },
            QuestionPrompt {
                id: "tests".to_string(),
                header: "Tests".to_string(),
                question: "Which validation should run?".to_string(),
                options: vec![
                    QuestionOption {
                        label: "Focused".to_string(),
                        description: "Run only feature tests.".to_string(),
                    },
                    QuestionOption {
                        label: "Full".to_string(),
                        description: "Run the full workspace.".to_string(),
                    },
                ],
                kind: QuestionPromptKind::SingleSelect { allow_other: true },
                required: true,
                default: None,
            },
        ],
        source_label: None,
        dismissible: true,
    }
}

/// Draw the view manager to an off-screen buffer and flatten it to text.
fn rendered_views_text(views: &mut SessionViews, width: u16, height: u16) -> String {
    rendered_views_rows(views, width, height).concat()
}

fn rendered_views_rows(views: &mut SessionViews, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| views.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn rendered_views_status_cells(
    views: &mut SessionViews,
    width: u16,
    height: u16,
) -> Vec<ratatui::buffer::Cell> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| views.render(frame)).unwrap();
    bottom_row_cells(terminal.backend().buffer())
}

fn rendered_views_row_cells(
    views: &mut SessionViews,
    width: u16,
    height: u16,
    y: u16,
) -> Vec<ratatui::buffer::Cell> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| views.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..width).map(|x| buffer[(x, y)].clone()).collect()
}

fn cell_row_text(cells: &[ratatui::buffer::Cell]) -> String {
    cells.iter().map(ratatui::buffer::Cell::symbol).collect()
}

fn expected_workspace_display(workspace: &std::path::Path) -> String {
    assert!(
        workspace.is_absolute(),
        "native absolute fixture: {workspace:?}"
    );
    let mut display = workspace.display().to_string();
    if let Ok(home) = zevria_foundation::runtime_paths::home_dir()
        && let Ok(relative) = workspace.strip_prefix(home)
    {
        display = if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", relative.display())
        };
    }
    if cfg!(windows) {
        display = display.replace('\\', "/");
    }
    display
}

fn workspace_header_test_width(workspace: &str, git_label: &str, gutter: u16) -> u16 {
    u16::try_from(
        crate::text::display_width(" ")
            + crate::text::display_width(workspace)
            + 1
            + crate::text::display_width(git_label),
    )
    .expect("workspace and Git label width")
        + 2 * gutter
}

fn assert_workspace_header_cells(
    cells: &[ratatui::buffer::Cell],
    workspace: &str,
    git_label: &str,
    horizontal_gutter: usize,
    accent: Color,
) {
    let content_end = cells
        .len()
        .checked_sub(horizontal_gutter)
        .expect("header width must contain its right gutter");
    assert!(horizontal_gutter <= content_end);

    for cell in cells[..horizontal_gutter]
        .iter()
        .chain(cells[content_end..].iter())
    {
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, ZEVRIA_DARK.text.primary);
        assert_eq!(cell.bg, ZEVRIA_DARK.surfaces.canvas);
        assert_eq!(cell.modifier, Modifier::empty());
    }

    let content = &cells[horizontal_gutter..content_end];
    let content_text = cell_row_text(content);
    assert!(
        content
            .iter()
            .all(|cell| cell.bg == ZEVRIA_DARK.surfaces.canvas),
        "workspace header content must use the canvas surface"
    );
    assert!(
        content_text.starts_with(&format!(" {workspace}")),
        "workspace glyph and path must begin at the inset boundary: {content_text:?}"
    );
    assert!(
        content_text.ends_with(git_label),
        "Git label must end at the inset boundary: {content_text:?}"
    );
    let workspace_start = horizontal_gutter + crate::text::display_width(" ");
    let workspace_end = workspace_start + crate::text::display_width(workspace);
    let parent_end = workspace.rfind('/').map_or(0, |index| index + 1);
    let leaf_start = workspace_start + crate::text::display_width(&workspace[..parent_end]);
    for cell in &cells[workspace_start..leaf_start] {
        assert_eq!(cell.fg, ZEVRIA_DARK.text.muted);
        assert_eq!(cell.modifier, Modifier::empty());
    }

    let git_width = crate::text::display_width(git_label);
    let git_start = content_end
        .checked_sub(git_width)
        .expect("Git label must fit inside the header content area");
    assert_eq!(cell_row_text(&cells[git_start..content_end]), git_label);
    for cell in cells[horizontal_gutter..workspace_start]
        .iter()
        .chain(cells[leaf_start..workspace_end].iter())
        .chain(cells[git_start..content_end].iter())
    {
        assert_eq!(cell.fg, accent);
        assert_eq!(cell.modifier, Modifier::BOLD);
    }
    for cell in &cells[workspace_end..git_start] {
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, ZEVRIA_DARK.text.primary);
        assert_eq!(cell.modifier, Modifier::empty());
    }
}

fn assert_workspace_header_rule(cells: &[ratatui::buffer::Cell]) {
    for (x, cell) in cells.iter().enumerate() {
        if (2..cells.len().saturating_sub(2)).contains(&x) {
            assert_eq!(cell.symbol(), "─");
            assert_eq!(cell.fg, ZEVRIA_DARK.surfaces.border);
        } else {
            assert_eq!(cell.symbol(), " ");
            assert_eq!(cell.fg, ZEVRIA_DARK.text.primary);
        }
        assert_eq!(cell.bg, ZEVRIA_DARK.surfaces.canvas);
        assert_eq!(cell.modifier, Modifier::empty());
    }
}

fn skill_meta(name: &str, description: &str) -> zevria_instructions::SkillMeta {
    zevria_instructions::SkillMeta {
        name: name.parse().unwrap(),
        description: description.to_string(),
    }
}

fn app_with_skills() -> App {
    App::new().with_skills(vec![
        skill_meta("commit", "Commit changes"),
        skill_meta("review", "Review changes"),
    ])
}

fn assert_reconciliation_status(
    arguments: &serde_json::Value,
    state: crate::app::ToolCallState,
    expected_status: &str,
    expected_color: Color,
) {
    let lines = render_reconciliation_lines(arguments.clone(), state);
    assert_eq!(
        line_text(&lines[1]).split_whitespace().last(),
        Some(expected_status)
    );
    assert_eq!(
        lines[1].spans.last().unwrap().style.fg,
        Some(expected_color)
    );
}

fn acp_transcript_app() -> (App, AgentTranscriptReducer) {
    (
        App::acp_inspect("ACP · Test · read-only · running"),
        AgentTranscriptReducer::default(),
    )
}
