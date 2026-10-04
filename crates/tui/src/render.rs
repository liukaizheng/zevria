//! Frame composition over independently rendered surfaces.
mod composer;
mod plan;
use composer::render_composer_surface;
use plan::render_plan_surface;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use zevria_foundation::SessionMode;

use crate::app::{App, ComposerChrome, ConversationTail, PlanChoice, RetryCountdown};
use crate::chrome::{
    CHROME_PAD_LEFT, PROMPT_PREFIX, PROMPT_PREFIX_WIDTH, PromptInfo, ScreenRows, paint_accent,
    paint_rounded_prompt, paint_selection, paint_surface, selection_style, style_selected_line,
};
use crate::composer::ComposerLayout;
use crate::frame_layout::{FrameLayout, LowerSurface};
use crate::layout::prepare::{
    message_separator_line, presentation_role, push_plain_text, wrapped_height,
};
use crate::presentation::{PresentationRole, TranscriptAppearance};
use crate::status::render_status_bar;
use crate::status_icon::StatusIcon;
use crate::text::{format_elapsed, truncate_display_width};
use crate::theme::theme;
use crate::viewport::{RowRange, render_scrollbar, rows_to_u16};
use zevria_workflow::EnsembleWorkflow;

const MAX_INPUT_ROWS: usize = 6;

impl App {
    /// Draw the conversation pane and input from derived presentation state.
    pub fn render(&mut self, frame: &mut Frame) {
        let cursor = self.surface().cursor;
        let _ = self.render_surface(frame, false, cursor);
    }

    pub(crate) fn render_surface(
        &mut self,
        frame: &mut Frame,
        workspace_header_required: bool,
        cursor_owned: bool,
    ) -> FrameLayout {
        let surface = self.surface();
        let eligibility = self.hint_eligibility();
        let mut parts = self.render_parts();
        let frame_area = frame.area();
        paint_surface(
            frame.buffer_mut(),
            frame_area,
            Style::new()
                .fg(theme().text.primary)
                .bg(theme().surfaces.canvas),
        );
        let inspect = matches!(parts.chrome, ComposerChrome::Inspect);
        let plan_approval = parts.plan_dialog.is_some();
        let composer_layout = if inspect || plan_approval {
            None
        } else {
            let prompt_width = FrameLayout::padded_width(frame_area);
            let composer_width = FrameLayout::composer_text_width(prompt_width).max(1);
            parts.view.set_composer_width(composer_width);
            Some(ComposerLayout::new(
                parts.input,
                parts.input_cursor,
                composer_width,
            ))
        };
        let lower = if inspect {
            LowerSurface::None
        } else if plan_approval {
            LowerSurface::Plan {
                requested_height: 5,
            }
        } else {
            let requested_height = composer_layout
                .as_ref()
                .map_or(1, |layout| layout.rows().len().min(MAX_INPUT_ROWS))
                .saturating_add(2);
            LowerSurface::Composer {
                requested_height: rows_to_u16(requested_height),
            }
        };
        let layout = FrameLayout::compute(frame_area, inspect, lower, workspace_header_required);

        if layout.status.height > 0 {
            render_status_bar(frame, layout.status, &parts.status);
        }

        let wrap_width = layout.conversation_content.width.max(1);
        {
            let cache = parts.view.conversation_cache_mut();
            cache.set_appearance(parts.appearance);
            cache.set_header_timings(std::mem::take(&mut parts.header_timings));
            cache.refresh(
                parts.history,
                parts.selected,
                wrap_width,
                parts.diagnostics_visible,
                parts.folds,
            );
            let streaming = match parts.tail {
                ConversationTail::Streaming { message, .. } => Some(message),
                ConversationTail::None
                | ConversationTail::Compacting { .. }
                | ConversationTail::Waiting { .. }
                | ConversationTail::Retrying { .. } => None,
            };
            cache.refresh_streaming(
                streaming,
                parts
                    .assistant_header
                    .filter(|_| streaming.is_some() || parts.pending_header),
                wrap_width,
            );
        }

        // Message content stays cached; timing and its boundary are layout-owned.
        let streaming_height = parts
            .view
            .conversation_cache()
            .streaming()
            .map_or(0, |(_, height)| height);
        let status_lines = operation_status_lines(&parts.tail);
        let status_height = wrapped_height(&status_lines, wrap_width);
        // Committed entries already own a trailing gap. A streamed message or
        // header-only tail needs a blank row before the roleless status block.
        let status_gap = usize::from(streaming_height > 0 && status_height > 0);
        let tail_height = streaming_height
            .saturating_add(status_gap)
            .saturating_add(status_height);
        let tail_is_chat = streaming_height > 0;
        let viewport_height = usize::from(layout.conversation_content.height);

        let separator_after = {
            let entries = parts.view.conversation_cache().entries();
            let mut separators = vec![false; entries.len()];
            let mut next_visible_is_chat = (tail_height > 0).then_some(tail_is_chat);
            for (index, entry) in entries.iter().enumerate().rev() {
                if entry.height == 0 {
                    continue;
                }
                let current_is_chat = parts.history.get(index).is_some_and(|history| {
                    history_has_visible_chat_group(history, parts.diagnostics_visible)
                });
                separators[index] = current_is_chat && next_visible_is_chat == Some(true);
                next_visible_is_chat = Some(current_is_chat);
            }
            separators
        };

        let (selection_range, entries_total) = {
            let mut selection_range = None;
            let mut entries_total = 0_usize;
            for entry in parts.view.conversation_cache().entries() {
                if let Some(range) = entry.selection {
                    selection_range = Some(range.shifted(entries_total));
                }
                entries_total = entries_total.saturating_add(entry.extent());
            }
            (selection_range, entries_total)
        };
        let streaming_range = RowRange::from_start_len(entries_total, streaming_height);
        let status_range = RowRange::from_start_len(
            streaming_range.end().saturating_add(status_gap),
            status_height,
        );
        let total = status_range.end();
        parts
            .view
            .refresh_turn_positions(&parts.turn_starts, layout.conversation_content);
        parts.view.reconcile_conversation_viewport(
            total,
            viewport_height,
            selection_range.filter(|_| parts.selection_reveal),
            parts.selected.is_some(),
        );

        parts
            .view
            .record_rendered_selection_window(layout.conversation_content);
        let viewport = parts.view.conversation_viewport().clone();
        let window = viewport.visible_range();
        let scroll = viewport.top();
        let mut visible = Vec::new();
        let mut accent_ranges = Vec::new();
        let mut surface_ranges = Vec::new();
        let mut window_start = None;
        let mut offset = 0_usize;
        for (index, entry) in parts.view.conversation_cache().entries().iter().enumerate() {
            if offset >= window.end() {
                break;
            }
            let entry_range = RowRange::from_start_len(offset, entry.height);
            let separator = usize::from(entry.height > 0);
            let end = offset.saturating_add(entry.extent());
            if RowRange::new(offset, end).intersects(window) {
                window_start.get_or_insert(offset);
                visible.extend(entry.lines.iter().cloned());
                if separator > 0 {
                    visible.push(
                        if separator_after[index]
                            && parts.appearance == TranscriptAppearance::Native
                        {
                            message_separator_line(wrap_width)
                        } else {
                            Line::default()
                        },
                    );
                }
                for decoration in &entry.decorations {
                    if decoration.role == Some(PresentationRole::User) || decoration.card {
                        surface_ranges.push(decoration.rows.shifted(offset));
                    }
                }
                if parts.appearance == TranscriptAppearance::Acp && !entry.decorations.is_empty() {
                    for decoration in &entry.decorations {
                        let range = decoration.rows.shifted(offset);
                        if let Some(role) = decoration.role {
                            accent_ranges.push((range, presentation_role(role).1));
                        }
                    }
                } else if entry.height > 0 {
                    let color = parts
                        .history
                        .get(index)
                        .map_or(theme().text.muted, |history| {
                            history_accent(history, parts.diagnostics_visible)
                        });
                    accent_ranges.push((entry_range, color));
                }
            }
            offset = end;
        }
        if RowRange::from_start_len(entries_total, tail_height).intersects(window) {
            window_start.get_or_insert(entries_total);
            if let Some((lines, _)) = parts.view.conversation_cache().streaming() {
                visible.extend(lines.iter().cloned());
            }
            if status_gap > 0 {
                visible.push(Line::default());
            }
            visible.extend(status_lines);
            accent_ranges.push((streaming_range, theme().roles.assistant));
        }
        let window_scroll = scroll.saturating_sub(window_start.unwrap_or(scroll));
        for range in surface_ranges {
            if let Some(rows) = project_rows(range, window, layout.conversation_content) {
                paint_surface(
                    frame.buffer_mut(),
                    Rect::new(
                        layout.conversation_content.x,
                        rows.start,
                        layout.conversation_content.width,
                        rows.end.saturating_sub(rows.start),
                    ),
                    Style::default().bg(theme().surfaces.panel),
                );
            }
        }
        frame.render_widget(
            Paragraph::new(visible)
                .wrap(Wrap { trim: false })
                .scroll((rows_to_u16(window_scroll), 0)),
            layout.conversation_content,
        );
        for (range, color) in accent_ranges {
            if let Some(rows) = project_rows(range, window, layout.conversation) {
                paint_accent(
                    frame.buffer_mut(),
                    layout.conversation.x,
                    rows,
                    Style::default().fg(color),
                );
            }
        }
        if let Some(selection) = selection_range
            && let Some(rows) = project_rows(selection, window, layout.conversation)
        {
            paint_selection(
                frame.buffer_mut(),
                layout.selection_band,
                rows,
                selection_style(),
            );
        }
        render_scrollbar(frame, layout.conversation_scrollbar, &viewport);

        if let Some(dialog) = parts.plan_dialog {
            render_plan_surface(frame, &layout, &mut parts, dialog);
        } else if !inspect {
            let composer_layout = composer_layout
                .as_ref()
                .expect("ordinary interactive panes have a composer layout");
            render_composer_surface(frame, &layout, &mut parts, composer_layout, cursor_owned);
        }

        if layout.hints.height > 0 {
            let hints = crate::hints::hint_line(
                surface.context(),
                &eligibility,
                usize::from(layout.hints.width),
            );
            frame.render_widget(Paragraph::new(hints), layout.hints);
        }

        if parts.command_menu_active
            && let Some(completion) = &parts.completion
        {
            crate::completion::render_menu(
                frame,
                crate::completion::MenuGeometry {
                    prompt: layout.prompt,
                    modal_body: layout.modal_body,
                },
                parts.menu,
                completion,
                parts.view.completion_viewport_mut(),
                &eligibility,
            );
        }

        self.render_help(frame, layout.modal_body);
        layout
    }
}

/// Render transient status from already-observed time, never from a live clock.
fn operation_status_lines(tail: &ConversationTail<'_>) -> Vec<Line<'static>> {
    let (elapsed, headline, color) = match tail {
        ConversationTail::None => return Vec::new(),
        ConversationTail::Waiting { elapsed } => {
            (elapsed, "running…".to_string(), theme().text.muted)
        }
        ConversationTail::Streaming { elapsed, .. } => {
            (elapsed, "streaming".to_string(), theme().text.muted)
        }
        ConversationTail::Compacting { elapsed } => (
            elapsed,
            "Compacting context…".to_string(),
            theme().text.muted,
        ),
        ConversationTail::Retrying {
            notice,
            countdown,
            elapsed,
        } => {
            let delay = match countdown {
                RetryCountdown::Immediate => "reconnecting now".to_string(),
                RetryCountdown::Elapsed => "reconnecting…".to_string(),
                RetryCountdown::Pending(remaining) => {
                    let seconds = remaining
                        .as_secs()
                        .saturating_add(u64::from(remaining.subsec_nanos() > 0));
                    format!("next attempt in {seconds}s")
                }
            };
            (
                elapsed,
                format!(
                    "⚠ reconnecting (attempt {}/{}) · {delay}",
                    notice.attempt, notice.max_attempts
                ),
                theme().feedback.warning,
            )
        }
    };
    let spinner = StatusIcon::Running.glyph(((elapsed.as_millis() / 250) % 4) as usize);
    let mut lines = vec![Line::from(vec![
        Span::styled(spinner, Style::default().fg(StatusIcon::Running.color())),
        Span::styled(
            format!(" {headline} · {}", format_elapsed(*elapsed)),
            Style::default().fg(color),
        ),
    ])];
    if let ConversationTail::Retrying { notice, .. } = tail {
        push_plain_text(
            &format!("connection lost: {}", notice.error),
            crate::chrome::dim_style(),
            &mut lines,
        );
    }
    lines
}

fn project_rows(range: RowRange, window: RowRange, area: Rect) -> Option<ScreenRows> {
    let start = range.start().max(window.start());
    let end = range.end().min(window.end());
    if start >= end || area.height == 0 {
        return None;
    }
    let local_start = rows_to_u16(start.saturating_sub(window.start())).min(area.height);
    let local_end = rows_to_u16(end.saturating_sub(window.start())).min(area.height);
    (local_start < local_end).then(|| {
        ScreenRows::new(
            area.y.saturating_add(local_start),
            area.y.saturating_add(local_end),
        )
    })
}

fn history_accent(entry: &crate::app::HistoryEntry, diagnostics_visible: bool) -> Color {
    match entry {
        crate::app::HistoryEntry::Conversation(entry) => entry
            .blocks
            .iter()
            .filter(|block| block.visible(diagnostics_visible))
            .find_map(|block| block.role)
            .map_or(theme().text.muted, |role| match role {
                PresentationRole::User => theme().roles.you,
                PresentationRole::Assistant => theme().roles.assistant,
                PresentationRole::System => theme().roles.system,
            }),
        crate::app::HistoryEntry::PlanArtifact(_) | crate::app::HistoryEntry::PlanHandoff(..) => {
            theme().workflow.plan
        }
        crate::app::HistoryEntry::Ensemble(ensemble) => match ensemble.workflow {
            EnsembleWorkflow::Plan => theme().workflow.plan,
            EnsembleWorkflow::Review => theme().workflow.review,
        },
        crate::app::HistoryEntry::Error(_) => theme().feedback.error,
        crate::app::HistoryEntry::CompactionDivider => theme().text.muted,
    }
}

fn history_has_visible_chat_group(
    entry: &crate::app::HistoryEntry,
    diagnostics_visible: bool,
) -> bool {
    matches!(
        entry,
        crate::app::HistoryEntry::Conversation(entry)
            if entry
                .blocks
                .iter()
                .any(|block| block.visible(diagnostics_visible) && block.role.is_some())
    )
}
