//! Pure semantic block preparation. No Frame or input-policy ownership.

use super::EntrySelection;
use crate::app::{EnsembleHistory, EntryFolds, ToolCallState, ToolCallStatus};
use crate::chrome::style_selected_line;
use crate::presentation::{
    AcpToolPresentation, DiagnosticTone, LaunchBatchNotice, ListMarker, NativeHeader,
    PresentationBlock, PresentationBlockKind, PresentationRole, PresentedChecklist, PresentedPlan,
    PresentedPlanContent, PresentedTool, PromptPhase, TextFlavor, TranscriptAppearance,
    WebActivityPresentation, launch_batch_notice, native_header_status, native_list,
    question_response, subtask_icon, tool_argument, tool_call_denied, tool_call_failed,
    tool_result_plain_text,
};
use crate::status_icon::StatusIcon;
use crate::text::display_width;
use crate::theme::theme;
use crate::viewport::RowRange;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use rig_core::message::ToolCall;
use unicode_segmentation::UnicodeSegmentation as _;
use zevria_foundation::{
    FileChange, LAUNCH_SUBTASKS_TOOL_NAME, QUESTION_TOOL_NAME, QuestionResponse,
    RECONCILE_REPORTS_TOOL_NAME, SKILL_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME, TASK_TOOL_NAME,
};
use zevria_workflow::{AgentRunStatus, EnsembleWorkflow, PlanArtifact, PlanHandoff};

use zevria_foundation::{COMMAND_TOOL_NAME, DELETE_TOOL_NAME, EDIT_TOOL_NAME, WRITE_TOOL_NAME};
const MAX_SUBMIT_PLAN_TITLE_GRAPHEMES: usize = 80;
const MAX_RECONCILIATION_LABEL_GRAPHEMES: usize = 96;
const MAX_LAUNCH_DIAGNOSTIC_GRAPHEMES: usize = 240;

/// How many rows a block of lines occupies once wrapped to `width`.
pub(crate) fn wrapped_height(lines: &[Line<'static>], width: u16) -> usize {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

pub(crate) fn role_header(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("● {label}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

pub(crate) fn message_separator_line(width: u16) -> Line<'static> {
    Line::styled(
        "─".repeat(usize::from(width)),
        Style::default().fg(theme().surfaces.border),
    )
}

pub(crate) fn render_error(
    error: &str,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    folded: bool,
) {
    lines.push(role_header("Error", theme().feedback.error));
    let content_start = lines.len();
    for line in error.lines() {
        lines.push(Line::styled(
            line.to_string(),
            Style::default().fg(theme().feedback.error),
        ));
    }
    if folded {
        fold_body_rows(lines, content_start, wrap_width, None, None, None);
    }
}

pub(crate) fn render_compaction_divider(lines: &mut Vec<Line<'static>>) {
    lines.push(Line::styled(
        "── Context compacted ──".to_string(),
        Style::default().fg(theme().text.muted),
    ));
}

pub(crate) fn render_plan_artifact(
    artifact: &PlanArtifact,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    selected_content: Option<usize>,
    folded: bool,
) -> RowRange {
    render_plan_block(
        &format!("Plan artifact · revision {}", artifact.version.revision),
        None,
        &format!("{} · {}", artifact.title, artifact.version),
        &artifact.markdown,
        lines,
        wrap_width,
        selected_content,
        folded,
    )
}

pub(crate) fn render_plan_handoff(
    handoff: &PlanHandoff,
    header: Option<NativeHeader>,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    selected_content: Option<usize>,
    folded: bool,
) -> RowRange {
    render_plan_block(
        "Approved Plan handoff",
        header,
        &format!(
            "{} · source {}",
            handoff.artifact.version, handoff.source_session_id
        ),
        &handoff.artifact.markdown,
        lines,
        wrap_width,
        selected_content,
        folded,
    )
}

fn append_native_header(header: &mut Line<'static>, identity: Option<NativeHeader>) {
    if let Some(identity) = identity {
        header.spans.push(Span::styled(
            format!(" · {identity}"),
            Style::default().fg(theme().text.muted),
        ));
    }
}

pub(crate) fn pending_assistant_header(identity: NativeHeader) -> Line<'static> {
    let mut header = role_header("Assistant", theme().roles.assistant);
    append_native_header(&mut header, Some(identity));
    header
}

pub(crate) fn render_ensemble(
    ensemble: &EnsembleHistory,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    selection: EntrySelection,
    entry_folds: &EntryFolds<'_>,
    message_folded: bool,
) -> Vec<RowRange> {
    let entry_start = lines.len();
    let workflow_color = match ensemble.workflow {
        EnsembleWorkflow::Plan => theme().workflow.plan,
        EnsembleWorkflow::Review => theme().workflow.review,
    };
    let mut header = role_header(&ensemble.workflow.to_string(), workflow_color);
    append_native_header(&mut header, ensemble.header);
    lines.push(header);
    let mut ranges = Vec::with_capacity(ensemble.workers.len().saturating_add(1));
    let prompt_start = lines.len();
    push_plain_text(
        &ensemble.prompt.display_projection(),
        Style::default(),
        lines,
    );
    ensure_content_line(lines, prompt_start);
    if entry_folds.is_item_folded(0) {
        fold_body_rows(lines, prompt_start, wrap_width, None, None, None);
    }
    ranges.push((prompt_start, lines.len()));
    if ensemble.workflow == EnsembleWorkflow::Plan {
        let confirmed = ensemble
            .workers
            .iter()
            .filter(|worker| {
                matches!(
                    worker.status,
                    AgentRunStatus::Confirmed | AgentRunStatus::Completed
                )
            })
            .count();
        let abandoned = ensemble
            .workers
            .iter()
            .filter(|worker| worker.status == AgentRunStatus::Abandoned)
            .count();
        let participating = ensemble.workers.len() - abandoned;
        let mut confirmation_count = if participating == 0 {
            format!("Ensemble cancelled · no participating workers · {abandoned} abandoned")
        } else if abandoned == 0 {
            format!(
                "{confirmed}/{participating} workers confirmed · synthesis waits for every exact revision"
            )
        } else {
            format!(
                "{confirmed}/{participating} workers confirmed · {abandoned} abandoned · synthesis waits for every participating revision"
            )
        };
        if let Some(worker) = ensemble
            .workers
            .iter()
            .find(|worker| ensemble.baseline.as_ref() == Some(&worker.descriptor.id))
        {
            confirmation_count.push_str(&format!(" · baseline: {}", worker.descriptor.label));
        }
        lines.push(Line::raw(confirmation_count));
    }
    for (index, worker) in ensemble.workers.iter().enumerate() {
        let row_start = lines.len();
        let descriptor = &worker.descriptor;
        let status = worker.status;
        let icon = StatusIcon::agent(status);
        let color = icon.color();
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} {}", icon.glyph(0), descriptor.label),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {} · {}{}",
                    descriptor.safe_mode,
                    status,
                    if ensemble.baseline.as_ref() == Some(&descriptor.id) {
                        " · baseline"
                    } else {
                        ""
                    }
                ),
                Style::default().fg(theme().text.muted),
            ),
        ]));
        if let Some(failure) = &worker.failure {
            lines.push(Line::styled(
                format!("    {failure}"),
                Style::default().fg(color),
            ));
        }
        if entry_folds.is_item_folded(index + 1) {
            fold_body_rows(lines, row_start, wrap_width, None, None, None);
        }
        ranges.push((row_start, lines.len()));
    }
    if message_folded {
        fold_body_rows(
            lines,
            prompt_start,
            wrap_width,
            None,
            Some(ranges.len()),
            None,
        );
        if selection != EntrySelection::None {
            for line in &mut lines[prompt_start..] {
                style_selected_line(line);
            }
        }
        let rows = RowRange::from_start_len(
            wrapped_height(&lines[entry_start..prompt_start], wrap_width),
            wrapped_height(&lines[prompt_start..], wrap_width),
        );
        return vec![rows; ranges.len()];
    }
    for (index, &(start, end)) in ranges.iter().enumerate() {
        if selection.includes(index) {
            for line in &mut lines[start..end] {
                style_selected_line(line);
            }
        }
    }
    // Measure each intervening segment and item once, rather than repeatedly
    // cloning and wrapping a growing prefix for every worker.
    let mut previous_end = entry_start;
    let mut offset = 0_usize;
    ranges
        .into_iter()
        .map(|(start, end)| {
            offset = offset.saturating_add(wrapped_height(&lines[previous_end..start], wrap_width));
            let rows =
                RowRange::from_start_len(offset, wrapped_height(&lines[start..end], wrap_width));
            offset = rows.end();
            previous_end = end;
            rows
        })
        .collect()
}

/// Render one independently cacheable semantic block. `header_role` is the
/// role transition computed from the surrounding visible blocks.
pub(crate) struct ConversationBlockContext {
    pub(crate) width: u16,
    pub(crate) header_role: Option<PresentationRole>,
    pub(crate) header: Option<NativeHeader>,
    pub(crate) separator_before: bool,
    pub(crate) selected: bool,
    pub(crate) folded: bool,
    pub(crate) reasoning_heading: bool,
    pub(crate) appearance: TranscriptAppearance,
}

fn status_span(icon: StatusIcon) -> Span<'static> {
    Span::styled(icon.glyph(0), Style::default().fg(icon.color()))
}

fn tool_prefix() -> Span<'static> {
    Span::styled("◆ ", Style::default().fg(theme().roles.tools))
}

/// Logical header, not a parsed screen line. Kept in the block cache so a
/// single-block message fold retains outcomes even for multiline commands.
#[derive(Clone)]
pub(crate) struct FoldHeader {
    label: Line<'static>,
    outcomes: Vec<(StatusIcon, Option<usize>)>,
    counted: bool,
}

impl FoldHeader {
    fn single(label: String, status: StatusIcon) -> Self {
        Self {
            label: Line::from(Span::styled(
                label,
                Style::default().fg(theme().roles.tools),
            )),
            outcomes: vec![(status, None)],
            counted: false,
        }
    }

    fn status_spans(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (index, &(icon, count)) in self.outcomes.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(theme().text.muted)));
            }
            if let Some(count) = count {
                spans.push(Span::styled(
                    format!("{count} "),
                    Style::default().fg(theme().text.muted),
                ));
            }
            spans.push(status_span(icon));
        }
        spans
    }

    /// Prefer complete counts/order; when impossible retain the most important
    /// non-success outcomes first. Never turn a mixed group into a lone tick.
    fn limited_status_spans(&self, width: usize) -> Vec<Span<'static>> {
        let full = self.status_spans();
        if spans_width(&full) <= width {
            return full;
        }
        let mut outcomes = self.outcomes.clone();
        outcomes.sort_by_key(|(icon, _)| match icon {
            StatusIcon::Failed => 0,
            StatusIcon::Denied => 1,
            StatusIcon::Interrupted => 2,
            StatusIcon::Running => 3,
            StatusIcon::Pending => 4,
            StatusIcon::Updated | StatusIcon::Unknown => 5,
            StatusIcon::Dismissed => 6,
            StatusIcon::Done => 7,
        });
        let budget = width.saturating_sub(usize::from(width > 1));
        let mut spans = Vec::new();
        for (icon, count) in outcomes {
            let mut next = Vec::new();
            if !spans.is_empty() {
                next.push(Span::raw(" · "));
            }
            if let Some(count) = count {
                next.push(Span::styled(
                    format!("{count} "),
                    Style::default().fg(theme().text.muted),
                ));
            }
            next.push(status_span(icon));
            if spans_width(&spans) + spans_width(&next) > budget {
                if spans.is_empty() && budget > 0 {
                    spans.push(status_span(icon));
                }
                break;
            }
            spans.extend(next);
        }
        if width > 1 {
            if spans_width(&spans) + 1 < width {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled("…", Style::default().fg(theme().text.muted)));
        }
        spans
    }

    fn folded(&self, width: usize, hidden_rows: usize) -> Line<'static> {
        if width == 0 {
            return Line::default();
        }
        let full_status = self.status_spans();
        let status_width = spans_width(&full_status);
        if !self.outcomes.is_empty() && width < 4 + status_width && status_width <= width {
            return Line::from(full_status);
        }
        if width < 5 {
            return if self.outcomes.is_empty() {
                Line::from(truncate_line_to_width(
                    &single_line_label(&self.label),
                    width,
                ))
            } else {
                Line::from(self.limited_status_spans(width))
            };
        }
        let status = self.limited_status_spans(width - 4);
        let separator = usize::from(!status.is_empty());
        let available = width.saturating_sub(4 + spans_width(&status) + separator);
        let label = single_line_label(&self.label);
        let suffix = format!(" · {hidden_rows} more rows");
        let show_count = label.width() + display_width(&suffix) <= available;
        let label_budget = available.saturating_sub(if show_count {
            display_width(&suffix)
        } else {
            0
        });
        let mut spans = vec![
            Span::styled("▸ ", Style::default().fg(theme().text.muted)),
            tool_prefix(),
        ];
        spans.extend(truncate_line_to_width(&label, label_budget));
        if !status.is_empty() {
            if label_budget > 0 {
                spans.push(Span::raw(" "));
            }
            spans.extend(status);
        }
        if show_count {
            spans.push(Span::styled(
                suffix,
                Style::default().fg(theme().text.muted),
            ));
        }
        Line::from(spans)
    }

    fn expanded(&self) -> Line<'static> {
        let mut spans = vec![tool_prefix()];
        spans.extend(self.label.spans.iter().cloned().map(|mut span| {
            span.style = self.label.style.patch(span.style);
            span
        }));
        if !self.outcomes.is_empty() {
            spans.push(Span::styled(
                if self.counted { " · " } else { " " },
                Style::default().fg(theme().text.muted),
            ));
            spans.extend(self.status_spans());
        }
        Line::from(spans)
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn single_line_label(label: &Line<'static>) -> Line<'static> {
    let mut label = label.clone();
    for span in &mut label.spans {
        if span.content.chars().any(char::is_control) {
            span.content = span
                .content
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect::<String>()
                .into();
        }
    }
    label
}

fn render_web_activity(
    activity: &WebActivityPresentation,
    lines: &mut Vec<Line<'static>>,
) -> FoldHeader {
    let counted = activity.detail.is_none();
    let header = FoldHeader {
        label: Line::from(Span::styled(
            activity
                .detail
                .clone()
                .unwrap_or_else(|| format!("Web actions · {}", activity.members.len())),
            Style::default().fg(theme().roles.tools),
        )),
        outcomes: activity
            .outcomes
            .iter()
            .map(|&(icon, count)| (icon, counted.then_some(count)))
            .collect(),
        counted,
    };
    // Hosted detail preserves readable newlines. Split structured spans, not
    // copy text: one logical prefix, with outcomes on the final detail line.
    let mut row = Line::default();
    for span in header.expanded().spans {
        for (index, text) in span.content.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::take(&mut row));
            }
            row.spans.push(Span::styled(text.to_string(), span.style));
        }
    }
    lines.push(row);
    header
}

pub(crate) struct RenderedBlockRows {
    pub(crate) fold_header: Option<FoldHeader>,
    pub(crate) content_line_start: usize,
    pub(crate) body: RowRange,
    pub(crate) decoration: RowRange,
}

pub(crate) fn card_inset(width: u16) -> u16 {
    u16::from(width >= 6)
}

pub(crate) fn render_conversation_block(
    block: &PresentationBlock,
    lines: &mut Vec<Line<'static>>,
    context: ConversationBlockContext,
) -> RenderedBlockRows {
    let ConversationBlockContext {
        width: wrap_width,
        header_role,
        header: identity,
        separator_before,
        selected,
        folded,
        reasoning_heading,
        appearance,
    } = context;
    let acp = appearance == TranscriptAppearance::Acp;
    let card = acp && block.prompt_group.is_some();
    let inset = if card { card_inset(wrap_width) } else { 0 };
    let inner_width = wrap_width.saturating_sub(inset * 2).max(1);
    let block_start = lines.len();
    if separator_before {
        lines.push(if acp {
            Line::default()
        } else {
            message_separator_line(wrap_width)
        });
    }
    let decoration_start = wrapped_height(&lines[block_start..], wrap_width);
    if let Some(role) = header_role {
        let (label, color) = presentation_role(role);
        let mut header = role_header(label, color);
        if !acp {
            append_native_header(&mut header, identity);
        }
        if acp && let Some(prompt) = &block.prompt {
            if let Some(origin) = prompt.origin.label() {
                header.spans.push(Span::styled(
                    format!(" · {origin}"),
                    Style::default().fg(theme().text.muted),
                ));
            }
            let badge = match prompt.phase {
                Some(PromptPhase::Queued) => Some((StatusIcon::Pending, None)),
                Some(PromptPhase::Dispatched) => Some((StatusIcon::Running, None)),
                Some(PromptPhase::Recovering) => Some((
                    StatusIcon::Running,
                    Some(format!(
                        " recovering {}",
                        prompt.latest_attempt.unwrap_or(0)
                    )),
                )),
                Some(PromptPhase::Cancelling) => {
                    Some((StatusIcon::Running, Some(" cancelling".to_string())))
                }
                Some(PromptPhase::Failed) => Some((StatusIcon::Failed, None)),
                Some(PromptPhase::Interrupted) => Some((StatusIcon::Interrupted, None)),
                Some(PromptPhase::Succeeded) | None => None,
            };
            if let Some((icon, phase)) = badge {
                header.spans.push(Span::raw(" "));
                header.spans.push(status_span(icon));
                if let Some(phase) = phase {
                    header.spans.push(Span::styled(
                        phase,
                        Style::default().fg(theme().feedback.warning),
                    ));
                }
            }
        }
        if card {
            push_card_line(header, inner_width, inset, lines);
        } else {
            lines.push(header);
        }
    }
    let content_start = lines.len();
    let fold_header;
    if card {
        let mut body = Vec::new();
        if let PresentationBlockKind::Text {
            text,
            flavor: TextFlavor::Plain,
            ..
        } = &block.kind
        {
            body.extend(text.split('\n').map(|line| Line::raw(line.to_owned())));
            fold_header = None;
        } else {
            fold_header =
                render_presentation_block(block, &mut body, inner_width, reasoning_heading);
        }
        for line in body {
            push_card_line(line, inner_width, inset, lines);
        }
    } else {
        fold_header = render_presentation_block(block, lines, wrap_width, reasoning_heading);
    }
    ensure_content_line(lines, content_start);
    if folded {
        fold_body_rows(
            lines,
            content_start,
            wrap_width,
            card.then_some((inner_width, inset)),
            None,
            fold_header.as_ref(),
        );
    }
    if selected {
        for line in &mut lines[content_start..] {
            style_selected_line(line);
        }
    }
    let start = wrapped_height(&lines[block_start..content_start], wrap_width);
    let height = wrapped_height(&lines[content_start..], wrap_width);
    RenderedBlockRows {
        fold_header,
        content_line_start: content_start - block_start,
        body: RowRange::from_start_len(start, height),
        decoration: RowRange::new(decoration_start, start.saturating_add(height)),
    }
}

/// Literal cards wrap once against their measured inner width. The outer
/// Paragraph sees physical rows, not a second wrapping decision.
fn push_card_line(line: Line<'static>, width: u16, inset: u16, lines: &mut Vec<Line<'static>>) {
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let prefix = " ".repeat(usize::from(inset));
    for span in line.spans {
        for grapheme in span.content.graphemes(true) {
            let cells = display_width(grapheme);
            if used > 0 && used + cells > usize::from(width) {
                let mut spans = vec![Span::raw(prefix.clone())];
                spans.append(&mut row);
                lines.push(Line::from(spans).style(line.style));
                used = 0;
            }
            // Two-cell glyphs cannot paint in a one-cell viewport. Retain
            // the physical row and exact copy payload rather than splitting.
            let rendered = if cells > usize::from(width) {
                " "
            } else {
                grapheme
            };
            if let Some(last) = row.last_mut().filter(|last| last.style == span.style) {
                last.content.to_mut().push_str(rendered);
            } else {
                row.push(Span::styled(rendered.to_owned(), span.style));
            }
            used += cells.min(usize::from(width));
        }
    }
    let mut spans = vec![Span::raw(prefix)];
    spans.append(&mut row);
    lines.push(Line::from(spans).style(line.style));
}

/// Replace a multi-row body with one summary row. No-op for one-row bodies.
pub(crate) fn fold_body_rows(
    lines: &mut Vec<Line<'static>>,
    content_start: usize,
    wrap_width: u16,
    card: Option<(u16, u16)>,
    blocks: Option<usize>,
    header: Option<&FoldHeader>,
) {
    if wrap_width == 0 {
        lines.truncate(content_start);
        return;
    }
    let full = wrapped_height(&lines[content_start..], wrap_width);
    if full <= 1 {
        return;
    }
    let text_width = usize::from(card.map_or(wrap_width, |(width, _)| width));
    let summary = if let Some(header) = header {
        header.folded(text_width, full - 1)
    } else {
        let block_count = blocks
            .filter(|&count| count > 1)
            .map_or_else(String::new, |count| format!(" · {count} blocks"));
        let suffix = format!("{block_count} · {} more rows", full - 1);
        let muted = Style::default().fg(theme().text.muted);
        // Aggregate/text folds have no lifecycle outcome. Keep their existing
        // wording but omit optional counts when a narrow row cannot fit them.
        let show_count = text_width > 2 + display_width(&suffix);
        let prefix = if text_width == 1 { "▸" } else { "▸ " };
        let budget = text_width.saturating_sub(
            display_width(prefix)
                + if show_count {
                    display_width(&suffix)
                } else {
                    0
                },
        );
        let mut spans = vec![Span::styled(prefix, muted)];
        spans.extend(truncate_line_to_width(
            &single_line_label(&lines[content_start]),
            budget,
        ));
        if show_count {
            spans.push(Span::styled(suffix, muted));
        }
        Line::from(spans)
    };
    lines.truncate(content_start);
    if let Some((width, inset)) = card {
        push_card_line(summary, width, inset, lines);
    } else {
        lines.push(summary);
    }
}

/// Clip on grapheme boundaries, preserving inherited and per-span styling.
/// Leading whitespace (including an ACP card's inset) is not summary content.
pub(crate) fn truncate_line_to_width(line: &Line<'_>, max_width: usize) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let graphemes = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .graphemes(true)
                .map(move |grapheme| (grapheme, line.style.patch(span.style)))
        })
        .skip_while(|(grapheme, _)| grapheme.chars().all(char::is_whitespace));
    let truncated = graphemes
        .clone()
        .map(|(text, _)| display_width(text))
        .sum::<usize>()
        > max_width;
    let budget = max_width - usize::from(truncated);
    let mut used = 0;
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (grapheme, style) in graphemes {
        let cells = display_width(grapheme);
        if used + cells > budget {
            break;
        }
        if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
            last.content.to_mut().push_str(grapheme);
        } else {
            spans.push(Span::styled(grapheme.to_owned(), style));
        }
        used += cells;
    }
    if truncated {
        let style = spans.last().map_or(line.style, |span| span.style);
        spans.push(Span::styled("…", style));
    }
    spans
}

pub(crate) fn presentation_role(role: PresentationRole) -> (&'static str, Color) {
    match role {
        PresentationRole::User => ("You", theme().roles.you),
        PresentationRole::Assistant => ("Assistant", theme().roles.assistant),
        PresentationRole::System => ("System", theme().roles.system),
    }
}

fn render_presentation_block(
    block: &PresentationBlock,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    reasoning_heading: bool,
) -> Option<FoldHeader> {
    match &block.kind {
        PresentationBlockKind::Text { text, flavor, .. } => match flavor {
            TextFlavor::Plain => push_plain_text(text, Style::default(), lines),
            TextFlavor::Markdown => lines.extend(crate::markdown::markdown_lines(
                text,
                Style::default(),
                usize::from(wrap_width),
            )),
        },
        PresentationBlockKind::Reasoning { parts } => {
            let muted = Style::default()
                .fg(theme().text.muted)
                .add_modifier(Modifier::ITALIC);
            if reasoning_heading {
                lines.push(Line::styled("reasoning", muted));
            }
            for part in parts {
                lines.extend(crate::markdown::markdown_lines(
                    part,
                    muted,
                    usize::from(wrap_width),
                ));
            }
        }
        PresentationBlockKind::WebActivity(activity) => {
            return Some(render_web_activity(activity, lines));
        }
        PresentationBlockKind::Tool(PresentedTool::Native { call, state }) => {
            return render_tool_call(call, state, lines, wrap_width);
        }
        PresentationBlockKind::Tool(PresentedTool::Acp(tool)) => {
            return Some(render_acp_tool(tool, lines, wrap_width));
        }
        PresentationBlockKind::Subtask { descriptor, .. } => {
            let workspace = descriptor
                .workspace
                .as_ref()
                .map(|path| format!(" · {path}"))
                .unwrap_or_default();
            let header = FoldHeader::single(
                format!("{} · {}{workspace}", descriptor.kind, descriptor.title),
                subtask_icon(descriptor.status),
            );
            lines.push(header.expanded());
            return Some(header);
        }
        PresentationBlockKind::Plan(plan) => render_presented_plan(plan, lines, wrap_width),
        PresentationBlockKind::Image { image, ordinal, .. } => {
            push_placeholder(&image.label(*ordinal), lines)
        }
        PresentationBlockKind::Placeholder(placeholder) => push_placeholder(placeholder, lines),
        PresentationBlockKind::Diagnostic(diagnostic) => {
            let color = match diagnostic.tone {
                DiagnosticTone::Muted => theme().text.muted,
                DiagnosticTone::Info => theme().feedback.info,
                DiagnosticTone::Success => theme().feedback.success,
                DiagnosticTone::Warning => theme().feedback.warning,
                DiagnosticTone::Error => theme().feedback.error,
            };
            let mut text = diagnostic.text.lines();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} · ", diagnostic.label),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    text.next().unwrap_or_default().to_string(),
                    Style::default().fg(color),
                ),
            ]));
            for line in text {
                lines.push(Line::styled(
                    format!("  {line}"),
                    Style::default().fg(color),
                ));
            }
        }
        PresentationBlockKind::Error(error) => {
            lines.push(Line::styled(
                format!("error · {error}"),
                Style::default().fg(theme().feedback.error),
            ));
        }
    }
    None
}

fn render_acp_tool(
    tool: &AcpToolPresentation,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
) -> FoldHeader {
    if let Some(activity) = tool.hosted_activity() {
        return render_web_activity(&activity, lines);
    }
    let status = tool.presented_status();
    let title = if tool.kind.is_empty() || tool.kind == tool.title {
        tool.title.clone()
    } else {
        format!("{} · {}", tool.kind, tool.title)
    };
    let header = FoldHeader::single(title, status.icon());
    lines.push(header.expanded());
    for location in &tool.locations {
        let mut rendered = location.path.display().to_string();
        if let Some(line) = location.line {
            rendered.push_str(&format!(":{line}"));
        }
        lines.push(Line::styled(
            format!("  {rendered}"),
            Style::default().fg(theme().text.muted),
        ));
    }
    if status.unsuccessful() {
        let output = tool.metadata.as_ref().map_or_else(
            || tool.output_text(),
            |metadata| metadata.diagnostic.clone(),
        );
        if let Some(output) = output {
            let readable = zevria_content::web_search::sanitize_readable(&output);
            push_plain_text(&readable, Style::default().fg(status.icon().color()), lines);
        }
    }
    if let Some(metadata) = &tool.metadata
        && matches!(
            metadata.outcome,
            zevria_foundation::ToolCallOutcome::Success
                | zevria_foundation::ToolCallOutcome::Partial
        )
    {
        for change in metadata.file_changes() {
            lines.extend(crate::diff_render::render_file_change_body(
                change,
                wrap_width,
                crate::diff_render::DiffRenderPolicy::Complete,
                crate::diff_render::DiffRowBackgrounds::Disabled,
            ));
        }
    }
    header
}

fn render_presented_plan(plan: &PresentedPlan, lines: &mut Vec<Line<'static>>, wrap_width: u16) {
    match &plan.content {
        PresentedPlanContent::Checklist(checklist) => render_checklist(checklist, lines),
        PresentedPlanContent::Markdown(markdown) => {
            lines.push(Line::styled(
                "plan",
                Style::default()
                    .fg(theme().text.muted)
                    .add_modifier(Modifier::BOLD),
            ));
            lines.extend(crate::markdown::markdown_lines(
                markdown,
                Style::default(),
                usize::from(wrap_width),
            ));
        }
    }
}

fn render_checklist(checklist: &PresentedChecklist, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(vec![
        Span::styled(
            checklist.label.clone(),
            Style::default().fg(theme().roles.tools),
        ),
        Span::styled(
            format!(
                " · {}/{} completed",
                checklist.completed_count(),
                checklist.items.len()
            ),
            Style::default().fg(theme().text.muted),
        ),
    ]));
    for item in &checklist.items {
        let icon = item.status.icon();
        let priority = item
            .priority
            .as_ref()
            .map(|priority| format!(" · {priority}"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", icon.glyph(0)),
                Style::default().fg(icon.color()),
            ),
            Span::raw(item.text.clone()),
            Span::styled(priority, Style::default().fg(theme().text.muted)),
        ]));
    }
}

#[allow(clippy::too_many_arguments)]
fn render_plan_block(
    label: &str,
    identity: Option<NativeHeader>,
    metadata: &str,
    markdown: &str,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
    selected_content: Option<usize>,
    folded: bool,
) -> RowRange {
    let start = lines.len();
    let mut header = role_header(label, theme().workflow.plan);
    append_native_header(&mut header, identity);
    lines.push(header);
    let content_start = lines.len();
    lines.push(Line::styled(
        metadata.to_string(),
        Style::default().fg(theme().text.muted),
    ));
    lines.extend(crate::markdown::markdown_lines(
        markdown,
        Style::default(),
        usize::from(wrap_width),
    ));
    if folded {
        fold_body_rows(lines, content_start, wrap_width, None, None, None);
    }
    if selected_content == Some(0) {
        for line in &mut lines[start..] {
            style_selected_line(line);
        }
    }
    RowRange::from_start_len(0, wrapped_height(&lines[start..], wrap_width))
}

pub(crate) fn ensure_content_line(lines: &mut Vec<Line<'static>>, content_line_start: usize) {
    if lines.len() == content_line_start {
        lines.push(Line::from(" "));
    }
}

fn render_tool_call(
    call: &ToolCall,
    state: &ToolCallState,
    lines: &mut Vec<Line<'static>>,
    wrap_width: u16,
) -> Option<FoldHeader> {
    // Children remain separate blocks. Only information absent from those rows
    // warrants a batch notice; missing launches are a count, not an outcome.
    if call.function.name == LAUNCH_SUBTASKS_TOOL_NAME {
        let notice = launch_batch_notice(state)?;
        let rejected = matches!(notice, LaunchBatchNotice::Rejected { .. });
        let mut header = match notice {
            LaunchBatchNotice::Rejected { denied } => FoldHeader::single(
                "subtask launch".to_string(),
                if denied {
                    StatusIcon::Denied
                } else {
                    StatusIcon::Failed
                },
            ),
            LaunchBatchNotice::Ended(status) => {
                FoldHeader::single("subtask launch".to_string(), status)
            }
            LaunchBatchNotice::Unlaunched { missing, requested } => FoldHeader {
                label: Line::styled(
                    format!("{missing} of {requested} subtasks not launched"),
                    Style::default().fg(theme().feedback.error),
                ),
                outcomes: Vec::new(),
                counted: false,
            },
        };
        let mut line = header.expanded();
        if rejected && let Some(reason) = launch_failure_reason(state) {
            let reason = Span::styled(
                format!(" · {reason}"),
                Style::default().fg(theme().feedback.error),
            );
            line.spans.push(reason.clone());
            header.label.spans.push(reason);
        }
        lines.push(line);
        return Some(header);
    }

    if call.function.name == SKILL_TOOL_NAME {
        let name =
            skill_name_from_arguments(state).unwrap_or_else(|| "<invalid arguments>".to_string());
        let unsuccessful = tool_call_denied(state)
            || tool_call_failed(state)
            || state.status == ToolCallStatus::Interrupted;
        let header =
            FoldHeader::single(format!("skill · {name}"), native_header_status(call, state));
        lines.push(header.expanded());
        if unsuccessful {
            render_tool_diagnostic(state, lines);
        }
        return Some(header);
    }
    if call.function.name == QUESTION_TOOL_NAME {
        return Some(render_question_call(call, state, lines));
    }
    if matches!(
        call.function.name.as_str(),
        TASK_TOOL_NAME | RECONCILE_REPORTS_TOOL_NAME
    ) {
        return Some(render_native_list(call, state, lines));
    }
    if call.function.name == SUBMIT_PLAN_TOOL_NAME {
        return Some(render_submit_plan_call(call, state, lines));
    }

    let Some(file_tool_kind) = FileToolKind::from_name(&call.function.name) else {
        let status = native_header_status(call, state);
        if call.function.name == COMMAND_TOOL_NAME
            && let Some(command) = command_from_arguments(state)
        {
            let mut command_lines = crate::syntax::highlighted_code_lines(
                &command,
                Some("bash"),
                Style::default().fg(theme().roles.tools),
            );
            let header = FoldHeader {
                label: command_lines.first().cloned().unwrap_or_default(),
                outcomes: vec![(status, None)],
                counted: false,
            };
            if let Some(first) = command_lines.first_mut() {
                first.spans.insert(0, tool_prefix());
            }
            if let Some(last) = command_lines.last_mut() {
                last.spans.push(Span::raw(" "));
                last.spans.push(status_span(status));
            }
            lines.extend(command_lines);
            return Some(header);
        }
        let header = FoldHeader::single(
            format!(
                "{}({})",
                call.function.name,
                state
                    .arguments
                    .as_ref()
                    .map_or_else(|| "<invalid arguments>".into(), ToString::to_string)
            ),
            status,
        );
        lines.push(header.expanded());
        return Some(header);
    };

    let denied = tool_call_denied(state);
    let failed = !denied && tool_call_failed(state);
    let interrupted = state.status == ToolCallStatus::Interrupted;
    let unsuccessful = denied || failed || interrupted;
    let status = native_header_status(call, state);
    let compact_change = if matches!(file_tool_kind, FileToolKind::Write | FileToolKind::Delete)
        && state.status == ToolCallStatus::Finished
        && !unsuccessful
        && state.result.is_some()
    {
        state
            .metadata
            .as_ref()
            .filter(|metadata| metadata.outcome.is_success())
            .and_then(|metadata| match metadata.file_changes() {
                [change] => Some(change),
                _ => None,
            })
    } else {
        None
    };
    let compact_summary = compact_change.map(crate::diff_render::summarize_file_change);
    let target = file_tool_target(state).unwrap_or_else(|| "<invalid arguments>".to_string());
    let header = FoldHeader::single(format!("{} {target}", call.function.name), status);
    let mut line = header.expanded();
    if let Some(summary) = &compact_summary {
        line.spans.extend([
            Span::raw(" ("),
            Span::styled(
                format!("+{}", summary.added),
                Style::default().fg(theme().content.diff_addition),
            ),
            Span::raw(" "),
            Span::styled(
                format!("-{}", summary.removed),
                Style::default().fg(theme().content.diff_deletion),
            ),
            Span::raw(")"),
        ]);
    }
    lines.push(line);

    if state.status != ToolCallStatus::Finished {
        return Some(header);
    }
    if unsuccessful {
        render_tool_diagnostic(state, lines);
        // A failed edit may have surviving mutations. Keep the diagnostic and
        // the captured changes; failure is not evidence of a successful rollback.
        if state.metadata.as_ref().is_none_or(|metadata| {
            metadata.outcome != zevria_foundation::ToolCallOutcome::Partial
                || metadata.file_changes().is_empty()
        }) {
            return Some(header);
        }
    }
    if let Some(change) = compact_change {
        // Write/delete show only the summary, except for unavailable-content diagnostics.
        if matches!(change.change, FileChange::Omitted { .. }) {
            lines.extend(crate::diff_render::render_file_change_body(
                change,
                wrap_width,
                crate::diff_render::DiffRenderPolicy::Complete,
                crate::diff_render::DiffRowBackgrounds::Disabled,
            ));
        }
        return Some(header);
    }
    if !unsuccessful && let Some(result) = &state.result {
        push_plain_text(
            &tool_result_plain_text(result),
            Style::default().fg(theme().text.muted),
            lines,
        );
    }
    if let Some(metadata) = &state.metadata {
        for change in metadata.file_changes() {
            match file_tool_kind {
                FileToolKind::Edit => lines.extend(crate::diff_render::render_file_change(
                    change,
                    wrap_width,
                    crate::diff_render::DiffRenderPolicy::Bounded {
                        max_rows: crate::diff_render::MAX_EDIT_RENDERED_DIFF_LINES,
                    },
                    crate::diff_render::DiffRowBackgrounds::Enabled,
                )),
                FileToolKind::Write | FileToolKind::Delete => {
                    let summary = crate::diff_render::summarize_file_change(change);
                    lines.extend(crate::diff_render::render_file_change_summary(&summary));
                    if matches!(change.change, FileChange::Omitted { .. }) {
                        lines.extend(crate::diff_render::render_file_change_body(
                            change,
                            wrap_width,
                            crate::diff_render::DiffRenderPolicy::Complete,
                            crate::diff_render::DiffRowBackgrounds::Disabled,
                        ));
                    }
                }
            }
        }
    }
    Some(header)
}

fn render_native_list(
    call: &ToolCall,
    state: &ToolCallState,
    lines: &mut Vec<Line<'static>>,
) -> FoldHeader {
    let list = native_list(call, state);
    let status = list
        .as_ref()
        .map_or_else(|| native_header_status(call, state), |list| list.status);
    let header = FoldHeader::single(
        list.as_ref().map_or_else(
            || {
                if call.function.name == TASK_TOOL_NAME {
                    "task · invalid list"
                } else {
                    "reconcile_reports · <invalid arguments>"
                }
                .to_string()
            },
            |list| list.label.clone(),
        ),
        status,
    );
    lines.push(header.expanded());
    if let Some(list) = list {
        if let Some(explanation) = list.explanation {
            push_plain_text(
                &explanation,
                Style::default()
                    .fg(theme().text.muted)
                    .add_modifier(Modifier::ITALIC),
                lines,
            );
        }
        for row in list.rows {
            let color = match row.marker {
                ListMarker::Status(icon) => icon.color(),
                ListMarker::Disagreement => theme().roles.tools,
            };
            let indent = if list.reconciliation { "  " } else { "" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{indent}{} ", row.marker.glyph()),
                    Style::default().fg(color),
                ),
                Span::styled(
                    if list.reconciliation {
                        reconciliation_label(&row.text)
                    } else {
                        row.text
                    },
                    Style::default().fg(if row.muted {
                        theme().text.muted
                    } else {
                        theme().text.primary
                    }),
                ),
                Span::styled(row.annotation, Style::default().fg(theme().text.muted)),
            ]));
        }
    }
    render_tool_diagnostic(state, lines);
    header
}

fn reconciliation_label(text: &str) -> String {
    let text = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    compact_single_line(&text, MAX_RECONCILIATION_LABEL_GRAPHEMES)
        .unwrap_or_else(|| "<invalid id>".to_string())
}

fn render_submit_plan_call(
    call: &ToolCall,
    state: &ToolCallState,
    lines: &mut Vec<Line<'static>>,
) -> FoldHeader {
    let unsuccessful = tool_call_denied(state)
        || tool_call_failed(state)
        || state.status == ToolCallStatus::Interrupted;
    let header = FoldHeader::single(
        format!("submit_plan · {}", submit_plan_title(state)),
        native_header_status(call, state),
    );
    lines.push(header.expanded());

    // Accepted Markdown is rendered once by the dedicated Plan artifact entry.
    // Rejections have no artifact, so keep their short result visible here.
    if unsuccessful {
        render_tool_diagnostic(state, lines);
    }
    header
}

fn submit_plan_title(state: &ToolCallState) -> String {
    tool_argument(state, "title")
        .and_then(|title| compact_single_line(&title, MAX_SUBMIT_PLAN_TITLE_GRAPHEMES))
        .unwrap_or_else(|| "<invalid arguments>".to_string())
}

fn compact_single_line(text: &str, max_graphemes: usize) -> Option<String> {
    let text = if text.trim() == text
        && text.lines().count() == 1
        && !text.chars().any(char::is_control)
    {
        text.to_string()
    } else {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    if text.is_empty() {
        return None;
    }

    let mut graphemes = text.graphemes(true);
    let mut compact = graphemes.by_ref().take(max_graphemes).collect::<String>();
    if graphemes.next().is_some() {
        compact.push('…');
    }
    Some(compact)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileToolKind {
    Edit,
    Write,
    Delete,
}

impl FileToolKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            EDIT_TOOL_NAME => Some(Self::Edit),
            WRITE_TOOL_NAME => Some(Self::Write),
            DELETE_TOOL_NAME => Some(Self::Delete),
            _ => None,
        }
    }
}

fn command_from_arguments(state: &ToolCallState) -> Option<String> {
    tool_argument(state, "command").filter(|command| !command.trim().is_empty())
}

fn skill_name_from_arguments(state: &ToolCallState) -> Option<String> {
    tool_argument(state, "skill")
}

/// Only descriptor-less failures use this exception to launch-result hiding.
/// Use the display sidecar, never parse model-facing envelopes or arguments.
fn render_tool_diagnostic(state: &ToolCallState, lines: &mut Vec<Line<'static>>) {
    if let Some(metadata) = &state.metadata
        && !metadata.outcome.is_success()
        && let Some(diagnostic) = &metadata.diagnostic
    {
        let text = zevria_content::web_search::sanitize_readable(diagnostic);
        push_plain_text(
            &text,
            Style::default().fg(crate::presentation::native_tool_status(state)
                .icon()
                .color()),
            lines,
        );
    }
}

fn launch_failure_reason(state: &ToolCallState) -> Option<String> {
    state
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.diagnostic.as_deref())
        .and_then(|reason| {
            let safe = reason
                .chars()
                .map(|character| {
                    // Remove terminal controls and bidi controls, but retain
                    // joiners and variation selectors used in scripts/emoji.
                    if character.is_control()
                        || matches!(character, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                    {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let single_line = safe.split_whitespace().collect::<Vec<_>>().join(" ");
            compact_single_line(&single_line, MAX_LAUNCH_DIAGNOSTIC_GRAPHEMES)
        })
}

/// Compact persisted presentation for an interactive question call. The
/// assistant arguments and correlated result are both transcript-backed, so
/// the same summary renders after resume without persisting the live modal.
fn render_question_call(
    call: &ToolCall,
    state: &ToolCallState,
    lines: &mut Vec<Line<'static>>,
) -> FoldHeader {
    let prompts = question_prompts_from_arguments(state);
    let response = question_response(state);
    let denied = tool_call_denied(state);
    let failed = !denied && tool_call_failed(state);
    let summary = match prompts.as_slice() {
        [] => "question".to_string(),
        [(_, header)] => format!("question · {header}"),
        _ => format!("question · {} prompts", prompts.len()),
    };
    let header = FoldHeader::single(summary, native_header_status(call, state));
    lines.push(header.expanded());

    if let Some(QuestionResponse::Answered { answers }) = response {
        for (id, header) in prompts {
            if let Some(answer) = answers.iter().find(|answer| answer.id == id) {
                let answer = match &answer.answer {
                    Some(zevria_foundation::QuestionAnswerValue::String(value)) => value.clone(),
                    Some(zevria_foundation::QuestionAnswerValue::Strings(values)) => {
                        values.join(", ")
                    }
                    None => "Skipped".to_string(),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{header}: "),
                        Style::default().fg(theme().text.muted),
                    ),
                    Span::raw(answer),
                ]));
            }
        }
    } else if denied || failed {
        render_tool_diagnostic(state, lines);
    }
    header
}

fn question_prompts_from_arguments(state: &ToolCallState) -> Vec<(String, String)> {
    let Some(arguments) = &state.arguments else {
        return Vec::new();
    };
    arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            Some((
                question.get("id")?.as_str()?.to_string(),
                question.get("header")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn file_tool_target(state: &ToolCallState) -> Option<String> {
    tool_argument(state, "file_path")
}

pub(crate) fn push_plain_text(text: &str, style: Style, lines: &mut Vec<Line<'static>>) {
    if text.is_empty() {
        return;
    }
    for line in text.lines() {
        lines.push(Line::styled(line.to_string(), style));
    }
}

fn push_placeholder(label: &str, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::styled(
        label.to_string(),
        Style::default().fg(theme().text.muted),
    ));
}
