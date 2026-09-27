//! Caret-local completion, independent of leading-command classification.
//!
//! File results and request/dismissal identity are UI state, never draft content.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;

use crate::{command::MatchEntry, composer::clamp_cursor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Skill,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionQuery {
    pub kind: CompletionKind,
    /// Decoded text before the caret, not the whole replacement token.
    pub prefix: String,
    pub replacement: Range<usize>,
}

impl CompletionQuery {
    pub fn replacement(&self) -> Range<usize> {
        self.replacement.clone()
    }
}

/// Preserve the leading `/` and `$` palette's prefix-only replacement contract.
pub(crate) fn command_query(input: &str, cursor: usize) -> Option<CompletionQuery> {
    let kind = match input.as_bytes().first()? {
        b'/' => CompletionKind::Command,
        b'$' => CompletionKind::Skill,
        _ => return None,
    };
    let cursor = clamp_cursor(input, cursor).max(1);
    let prefix = input.get(1..cursor)?;
    if prefix.chars().any(char::is_whitespace) {
        return None;
    }
    Some(CompletionQuery {
        kind,
        prefix: prefix.into(),
        replacement: 0..cursor,
    })
}

/// Parse whole reference tokens while searching for the one containing the caret.
/// Quoted references may contain whitespace. A backslash escapes the next
/// character; the canonical encoder only needs to escape quotes and backslashes.
/// Registered image markers are hard boundaries, not searchable file text.
pub fn file_query(
    input: &str,
    cursor: usize,
    images: impl IntoIterator<Item = Range<usize>>,
) -> Option<CompletionQuery> {
    let cursor = clamp_cursor(input, cursor);
    let mut chars = input.char_indices().peekable();
    let mut graphemes = input.grapheme_indices(true).peekable();
    let mut boundary = true;
    while let Some((start, ch)) = chars.next() {
        if start >= cursor {
            break;
        }
        while graphemes.peek().is_some_and(|(offset, _)| *offset < start) {
            let (_, grapheme) = graphemes.next().expect("peeked grapheme");
            boundary = grapheme.chars().any(char::is_whitespace);
        }
        if ch != '@' || !boundary {
            continue;
        }
        let mut quoted = chars.peek().is_some_and(|(_, ch)| *ch == '"');
        if quoted {
            chars.next();
        }
        let mut prefix = String::new();
        let mut end = input.len();
        while let Some(&(offset, ch)) = chars.peek() {
            if !quoted && ch.is_whitespace() {
                end = offset;
                break;
            }
            chars.next();
            if ch == '"' && quoted {
                quoted = false;
                continue;
            }
            if ch == '\\' {
                if let Some(&(escaped_offset, escaped)) = chars.peek() {
                    chars.next();
                    if escaped_offset < cursor {
                        prefix.push(escaped);
                    }
                } else if offset < cursor {
                    prefix.push(ch);
                }
            } else if offset < cursor {
                prefix.push(ch);
            }
        }
        if cursor <= end {
            let replacement = start..end;
            // Whitespace with combining marks can make a character boundary
            // differ from an editor boundary. Never split a grapheme to accept.
            if clamp_cursor(input, start) != start
                || clamp_cursor(input, end) != end
                || images
                    .into_iter()
                    .any(|image| replacement.start < image.end && image.start < replacement.end)
            {
                return None;
            }
            return Some(CompletionQuery {
                kind: CompletionKind::File,
                prefix,
                replacement,
            });
        }
    }
    None
}

/// Reversible ordinary text, deliberately not an attachment marker.
pub fn file_reference(path: &str) -> String {
    if path
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'))
    {
        format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("@{path}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionAcceptance {
    /// May execute an exactly classified built-in on Enter.
    Builtin,
    /// Skills and files only insert text, irrespective of resulting classification.
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileQueryIdentity {
    pub generation: u64,
    pub cursor: usize,
    pub query: CompletionQuery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCompletionRequest {
    pub activation: u64,
    pub request: u64,
    pub identity: FileQueryIdentity,
}

/// Bounded diagnostics: no filesystem errors or paths are accumulated here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSearchStatus {
    pub loading: bool,
    pub unavailable: bool,
    pub incomplete: bool,
    pub omitted: usize,
    pub errors: usize,
}

#[derive(Debug, Default)]
pub(crate) struct FileCompletionState {
    pub active: Option<FileCompletionRequest>,
    pub query: Option<FileQueryIdentity>,
    pub dismissed: Option<FileQueryIdentity>,
    pub next_request: u64,
    pub next_activation: u64,
    pub paths: Vec<String>,
    pub status: FileSearchStatus,
}

/// Prepared rows. File matching and discovery never run while painting.
pub struct CompletionView<'a> {
    pub kind: CompletionKind,
    pub entries: Vec<CompletionEntry<'a>>,
    pub status: FileSearchStatus,
}

pub enum CompletionEntry<'a> {
    Command(MatchEntry<'a>),
    File(&'a str),
}

impl CompletionEntry<'_> {
    fn label(&self) -> String {
        match self {
            Self::Command(entry) => format!("{}{}", entry.sigil(), entry.name()),
            Self::File(path) => (*path).to_owned(),
        }
    }

    fn description(&self) -> &str {
        match self {
            Self::Command(entry) => entry.description(),
            Self::File(_) => "",
        }
    }
}

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    chrome::{ScreenRows, paint_selection, selection_style, style_selected_line},
    theme::theme,
    viewport::{RowRange, Viewport, render_scrollbar, rows_to_u16},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MenuGeometry {
    pub prompt: Rect,
    pub modal_body: Rect,
}

/// Draw the completion menu directly above the input box.
pub fn render_menu(
    frame: &mut Frame,
    geometry: MenuGeometry,
    menu: &CompletionMenu,
    data: &CompletionView<'_>,
    viewport: &mut Viewport,
    eligibility: &crate::hints::Eligibility,
) {
    let matches = &data.entries;
    let muted = crate::chrome::dim_style();
    let mut rows: Vec<Line<'static>> = matches
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut row = Line::from(vec![
                Span::styled(
                    entry.label(),
                    Style::default()
                        .fg(theme().roles.tools)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {}", entry.description()), muted),
            ]);
            if index == menu.selected.min(matches.len().saturating_sub(1)) {
                style_selected_line(&mut row);
            }
            row
        })
        .collect();
    if matches.is_empty() && !data.status.loading && !data.status.unavailable {
        rows.push(Line::styled(
            match data.kind {
                CompletionKind::Command => "no matching command",
                CompletionKind::Skill => "no matching skill",
                CompletionKind::File => "no matching file",
            },
            muted,
        ));
    }
    let mut diagnostics = Vec::new();
    if data.status.incomplete || data.status.omitted > 0 || data.status.errors > 0 {
        diagnostics.push(format!(
            "Partial index · {} omitted · {} unreadable{}",
            data.status.omitted,
            data.status.errors,
            if data.status.incomplete {
                " · limit reached"
            } else {
                ""
            },
        ));
    }
    if data.status.loading {
        diagnostics.push(
            if matches.is_empty() {
                "Loading workspace files…"
            } else {
                "Refreshing workspace files…"
            }
            .into(),
        );
    }
    if data.status.unavailable {
        diagnostics.push("Workspace unavailable — reopen to retry".into());
    }
    // Pin diagnostics below the list, so a partial index is visible even when
    // fifty selectable results fill a short popup. Diagnostics are never rows.
    let status = (!diagnostics.is_empty()).then(|| Line::styled(diagnostics.join(" · "), muted));
    let input_area = geometry.prompt;
    let desired_height = rows
        .len()
        .saturating_add(usize::from(status.is_some()))
        .saturating_add(2);
    let available_height = input_area.y.saturating_sub(geometry.modal_body.y);
    let popup_height = rows_to_u16(desired_height.min(usize::from(available_height)));
    if popup_height < 3 {
        // No room above the input box inside the padded body.
        return;
    }
    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(popup_height),
        width: input_area.width,
        height: popup_height,
    };
    let kind = match data.kind {
        CompletionKind::Command => "Commands",
        CompletionKind::Skill => "Skills",
        CompletionKind::File => "Files",
    };
    let footer = crate::hints::hint_line(
        crate::keymap::KeyContext::Completion,
        eligibility,
        usize::from(popup_area.width.saturating_sub(2)),
    );
    let mut inner_area =
        zevria_tui_widgets::overlay::modal(frame, popup_area, format!(" {kind} "), footer);
    let status_area = if status.is_some() && inner_area.height > 0 {
        inner_area.height -= 1;
        Some(Rect {
            x: inner_area.x,
            y: inner_area.bottom(),
            width: inner_area.width,
            height: 1,
        })
    } else {
        None
    };
    let visible_rows = usize::from(inner_area.height);
    viewport.reconcile(rows.len(), visible_rows);
    if !matches.is_empty() {
        let highlighted_index = menu.selected.min(matches.len() - 1);
        viewport.reveal(RowRange::from_start_len(highlighted_index, 1));
    }
    let visible = viewport.visible_range();
    let visible_rows = rows[visible.start()..visible.end()].to_vec();

    frame.render_widget(Paragraph::new(visible_rows), inner_area);
    if let (Some(status), Some(area)) = (status, status_area) {
        frame.render_widget(Paragraph::new(status), area);
    }
    if !matches.is_empty() {
        let highlighted_index = menu.selected.min(matches.len() - 1);
        if highlighted_index >= visible.start() && highlighted_index < visible.end() {
            let y = inner_area
                .y
                .saturating_add(rows_to_u16(highlighted_index - visible.start()));
            paint_selection(
                frame.buffer_mut(),
                inner_area,
                ScreenRows::new(y, y.saturating_add(1)),
                selection_style(),
            );
        }
    }
    render_scrollbar(frame, popup_area, viewport);
}

/// Highlighted-row state for the completion menu.
#[derive(Debug, Default)]
pub struct CompletionMenu {
    pub(crate) selected: usize,
}

impl CompletionMenu {
    /// Selected completion row; mutation is owned by the composer.
    pub const fn selected(&self) -> usize {
        self.selected
    }

    pub const fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, match_count: usize) {
        self.selected = self
            .selected
            .saturating_add(1)
            .min(match_count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests;
