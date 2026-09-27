//! Summary-only write/delete results and grammar-aware, bounded edit diffs.
//! Unavailable-content diagnostics remain visible for all file tools.

use diffy::{Hunk, Patch};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOperation;
use zevria_foundation::FileChangeOutput;

use crate::{
    syntax::{CodeGrammar, CodeHighlighter, grammar_for_path, visible_code_line},
    theme::theme,
};

/// Maximum wrapped body rows rendered for bounded `edit` and legacy changes.
pub const MAX_EDIT_RENDERED_DIFF_LINES: usize = 1000;
fn add_style() -> Style {
    Style::new().fg(theme().content.diff_addition)
}
fn delete_style() -> Style {
    Style::new().fg(theme().content.diff_deletion)
}
fn context_style() -> Style {
    Style::new().fg(theme().text.muted)
}
fn hunk_style() -> Style {
    Style::new().fg(theme().content.diff_hunk)
}
fn muted_style() -> Style {
    Style::new().fg(theme().text.muted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRenderPolicy {
    Complete,
    Bounded { max_rows: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowBackgrounds {
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeSummary {
    pub verb: &'static str,
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

pub fn summarize_file_change(change: &FileChangeOutput) -> FileChangeSummary {
    let (added, removed) = line_counts(&change.change);
    let verb = match &change.change {
        FileChange::Add { .. } => "Added",
        FileChange::Delete { .. } => "Deleted",
        FileChange::Update {
            move_path: Some(_), ..
        } => "Moved",
        FileChange::Update { .. } => "Edited",
        FileChange::Omitted { operation, .. } => operation_verb(*operation),
    };
    let path = match &change.change {
        FileChange::Update {
            move_path: Some(move_path),
            ..
        } => format!("{} -> {}", change.path.display(), move_path.display()),
        _ => change.path.display().to_string(),
    };
    FileChangeSummary {
        verb,
        path,
        added,
        removed,
    }
}

pub fn render_file_change(
    change: &FileChangeOutput,
    width: u16,
    policy: DiffRenderPolicy,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    let summary = summarize_file_change(change);
    let mut lines = render_file_change_summary(&summary);
    lines.extend(render_file_change_body(
        change,
        width,
        policy,
        row_backgrounds,
    ));
    lines
}

pub fn render_file_change_summary(summary: &FileChangeSummary) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                format!("• {} 1 file ", summary.verb),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("("),
            Span::styled(format!("+{}", summary.added), add_style()),
            Span::raw(" "),
            Span::styled(format!("-{}", summary.removed), delete_style()),
            Span::raw(")"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::raw(summary.path.clone()),
            Span::raw(" ("),
            Span::styled(format!("+{}", summary.added), add_style()),
            Span::raw(" "),
            Span::styled(format!("-{}", summary.removed), delete_style()),
            Span::raw(")"),
        ]),
    ]
}

pub fn render_file_change_body(
    change: &FileChangeOutput,
    width: u16,
    policy: DiffRenderPolicy,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    let body_width = usize::from(width.saturating_sub(4).max(1));
    let (old_grammar, new_grammar) = resolved_grammars(change);
    let render_limit = match policy {
        DiffRenderPolicy::Complete => RenderLimit::Complete,
        DiffRenderPolicy::Bounded { max_rows } => RenderLimit::Rows(max_rows.saturating_add(1)),
    };
    let mut body = render_change_body(
        &change.change,
        body_width,
        old_grammar,
        new_grammar,
        render_limit,
        row_backgrounds,
    );
    let truncated_after = match policy {
        DiffRenderPolicy::Complete => None,
        DiffRenderPolicy::Bounded { max_rows } if body.len() > max_rows => {
            body.truncate(max_rows);
            Some(max_rows)
        }
        DiffRenderPolicy::Bounded { .. } => None,
    };

    let mut lines = Vec::with_capacity(body.len() + usize::from(truncated_after.is_some()));
    for line in body {
        let mut spans = vec![Span::raw("    ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    if let Some(max_rows) = truncated_after {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!("⋮ diff truncated after {max_rows} rendered lines"),
                muted_style(),
            ),
        ]));
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderLimit {
    Complete,
    Rows(usize),
}

impl RenderLimit {
    const fn exhausted(self, rendered_rows: usize) -> bool {
        match self {
            Self::Complete => false,
            Self::Rows(max_rows) => rendered_rows >= max_rows,
        }
    }

    const fn remaining(self, rendered_rows: usize) -> Self {
        match self {
            Self::Complete => Self::Complete,
            Self::Rows(max_rows) => Self::Rows(max_rows.saturating_sub(rendered_rows)),
        }
    }
}

fn operation_verb(operation: FileChangeOperation) -> &'static str {
    match operation {
        FileChangeOperation::Add => "Added",
        FileChangeOperation::Delete => "Deleted",
        FileChangeOperation::Update => "Edited",
    }
}

fn line_counts(change: &FileChange) -> (usize, usize) {
    match change {
        FileChange::Add { content } => (content.lines().count(), 0),
        FileChange::Delete { content } => (0, content.lines().count()),
        FileChange::Omitted { added, removed, .. } => (*added, *removed),
        FileChange::Update { unified_diff, .. } => Patch::from_str(unified_diff)
            .map(|patch| {
                patch
                    .hunks()
                    .iter()
                    .flat_map(Hunk::lines)
                    .fold((0, 0), |(added, removed), line| match line {
                        diffy::Line::Insert(_) => (added + 1, removed),
                        diffy::Line::Delete(_) => (added, removed + 1),
                        diffy::Line::Context(_) => (added, removed),
                    })
            })
            .unwrap_or((0, 0)),
    }
}

fn resolved_grammars(change: &FileChangeOutput) -> (Option<CodeGrammar>, Option<CodeGrammar>) {
    match &change.change {
        FileChange::Add { .. } => (None, grammar_for_path(&change.path)),
        FileChange::Delete { .. } => (grammar_for_path(&change.path), None),
        FileChange::Update { move_path, .. } => {
            let old_grammar = grammar_for_path(&change.path);
            let new_path = move_path.as_deref().unwrap_or(change.path.as_path());
            (old_grammar, grammar_for_path(new_path))
        }
        FileChange::Omitted { .. } => (None, None),
    }
}

fn render_change_body(
    change: &FileChange,
    width: usize,
    old_grammar: Option<CodeGrammar>,
    new_grammar: Option<CodeGrammar>,
    limit: RenderLimit,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    match change {
        FileChange::Add { content } => render_whole_file(
            content,
            DiffLineKind::Insert,
            new_grammar,
            width,
            limit,
            row_backgrounds,
        ),
        FileChange::Delete { content } => render_whole_file(
            content,
            DiffLineKind::Delete,
            old_grammar,
            width,
            limit,
            row_backgrounds,
        ),
        FileChange::Update { unified_diff, .. } => render_unified_diff(
            unified_diff,
            old_grammar,
            new_grammar,
            width,
            limit,
            row_backgrounds,
        ),
        FileChange::Omitted { reason, bytes, .. } => wrap_styled(
            "⋮ ",
            &format!("diff omitted ({bytes} bytes): {reason}"),
            muted_style(),
            width,
            limit,
        ),
    }
}

fn render_whole_file(
    content: &str,
    kind: DiffLineKind,
    grammar: Option<CodeGrammar>,
    width: usize,
    limit: RenderLimit,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    let number_width = content.lines().count().max(1).to_string().len();
    let mut highlighter = grammar.map(CodeHighlighter::new);
    let mut lines = Vec::new();
    for (index, raw_line) in content.split_inclusive('\n').enumerate() {
        if limit.exhausted(lines.len()) {
            break;
        }
        let body = source_spans(highlighter.as_mut(), raw_line, kind.style(row_backgrounds));
        lines.extend(render_diff_line(
            index + 1,
            kind,
            body,
            width,
            number_width,
            limit.remaining(lines.len()),
            row_backgrounds,
        ));
    }
    lines
}

fn render_unified_diff(
    unified_diff: &str,
    old_grammar: Option<CodeGrammar>,
    new_grammar: Option<CodeGrammar>,
    width: usize,
    limit: RenderLimit,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    let Ok(patch) = Patch::from_str(unified_diff) else {
        return render_plain_diff(unified_diff, width, limit);
    };
    if patch.hunks().is_empty() && !unified_diff.is_empty() {
        return render_plain_diff(unified_diff, width, limit);
    }
    let number_width = patch
        .hunks()
        .iter()
        .flat_map(|hunk| [hunk.old_range().end(), hunk.new_range().end()])
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let mut lines = Vec::new();
    for hunk in patch.hunks() {
        if limit.exhausted(lines.len()) {
            break;
        }
        let context = hunk
            .function_context()
            .map(|context| format!(" {context}"))
            .unwrap_or_default();
        lines.extend(wrap_styled(
            "",
            &format!("@@ -{} +{} @@{context}", hunk.old_range(), hunk.new_range()),
            hunk_style(),
            width,
            limit.remaining(lines.len()),
        ));
        if limit.exhausted(lines.len()) {
            break;
        }

        let mut old_highlighter = old_grammar.map(CodeHighlighter::new);
        let mut new_highlighter = new_grammar.map(CodeHighlighter::new);
        let mut old_line = hunk.old_range().start();
        let mut new_line = hunk.new_range().start();
        for line in hunk.lines() {
            if limit.exhausted(lines.len()) {
                break;
            }
            match line {
                diffy::Line::Insert(raw_line) => {
                    let body = source_spans(
                        new_highlighter.as_mut(),
                        raw_line,
                        DiffLineKind::Insert.style(row_backgrounds),
                    );
                    lines.extend(render_diff_line(
                        new_line,
                        DiffLineKind::Insert,
                        body,
                        width,
                        number_width,
                        limit.remaining(lines.len()),
                        row_backgrounds,
                    ));
                    new_line += 1;
                }
                diffy::Line::Delete(raw_line) => {
                    let body = source_spans(
                        old_highlighter.as_mut(),
                        raw_line,
                        DiffLineKind::Delete.style(row_backgrounds),
                    );
                    lines.extend(render_diff_line(
                        old_line,
                        DiffLineKind::Delete,
                        body,
                        width,
                        number_width,
                        limit.remaining(lines.len()),
                        row_backgrounds,
                    ));
                    old_line += 1;
                }
                diffy::Line::Context(raw_line) => {
                    if let Some(highlighter) = old_highlighter.as_mut() {
                        let _ = highlighter.highlight_line(raw_line, context_style());
                    }
                    let body = source_spans(new_highlighter.as_mut(), raw_line, context_style());
                    lines.extend(render_diff_line(
                        new_line,
                        DiffLineKind::Context,
                        body,
                        width,
                        number_width,
                        limit.remaining(lines.len()),
                        row_backgrounds,
                    ));
                    old_line += 1;
                    new_line += 1;
                }
            }
        }
    }
    lines
}

fn source_spans(
    highlighter: Option<&mut CodeHighlighter>,
    raw_line: &str,
    fallback: Style,
) -> Vec<Span<'static>> {
    highlighter.map_or_else(
        || {
            let text = visible_code_line(raw_line);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(text.to_string(), fallback)]
            }
        },
        |highlighter| highlighter.highlight_line(raw_line, fallback),
    )
}

fn render_plain_diff(unified_diff: &str, width: usize, limit: RenderLimit) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in unified_diff.lines() {
        if limit.exhausted(lines.len()) {
            break;
        }
        lines.extend(wrap_styled(
            "",
            line,
            Style::default(),
            width,
            limit.remaining(lines.len()),
        ));
    }
    lines
}

#[derive(Clone, Copy)]
enum DiffLineKind {
    Insert,
    Delete,
    Context,
}

impl DiffLineKind {
    const fn sign(self) -> char {
        match self {
            Self::Insert => '+',
            Self::Delete => '-',
            Self::Context => ' ',
        }
    }

    fn style(self, row_backgrounds: DiffRowBackgrounds) -> Style {
        match (self, row_backgrounds) {
            (Self::Insert, DiffRowBackgrounds::Enabled) => {
                add_style().bg(theme().content.diff_addition_background)
            }
            (Self::Delete, DiffRowBackgrounds::Enabled) => {
                delete_style().bg(theme().content.diff_deletion_background)
            }
            (Self::Insert, DiffRowBackgrounds::Disabled) => add_style(),
            (Self::Delete, DiffRowBackgrounds::Disabled) => delete_style(),
            (Self::Context, _) => context_style(),
        }
    }
}

fn render_diff_line(
    line_number: usize,
    kind: DiffLineKind,
    body: Vec<Span<'static>>,
    width: usize,
    number_width: usize,
    limit: RenderLimit,
    row_backgrounds: DiffRowBackgrounds,
) -> Vec<Line<'static>> {
    let gutter = Span::styled(
        format!("{line_number:>number_width$} {} ", kind.sign()),
        kind.style(row_backgrounds),
    );
    wrap_spans(gutter, body, width, limit)
}

fn wrap_styled(
    prefix: &str,
    text: &str,
    style: Style,
    width: usize,
    limit: RenderLimit,
) -> Vec<Line<'static>> {
    wrap_spans(
        Span::styled(prefix.to_string(), muted_style()),
        vec![Span::styled(text.to_string(), style)],
        width,
        limit,
    )
}

fn wrap_spans(
    gutter: Span<'static>,
    body: Vec<Span<'static>>,
    width: usize,
    limit: RenderLimit,
) -> Vec<Line<'static>> {
    if limit.exhausted(0) {
        return Vec::new();
    }

    let width = width.max(1);
    let gutter_width = gutter.content.chars().count();
    let available = width.saturating_sub(gutter_width).max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;
    let mut saw_body = false;

    for span in body {
        let style = span.style;
        for character in span.content.chars() {
            saw_body = true;
            push_styled_character(&mut current, character, style);
            current_width += 1;
            if current_width == available {
                lines.push(wrapped_line(
                    &gutter,
                    gutter_width,
                    lines.is_empty(),
                    current,
                    width,
                ));
                if limit.exhausted(lines.len()) {
                    return lines;
                }
                current = Vec::new();
                current_width = 0;
            }
        }
    }

    if !current.is_empty() {
        lines.push(wrapped_line(
            &gutter,
            gutter_width,
            lines.is_empty(),
            current,
            width,
        ));
    } else if !saw_body {
        lines.push(wrapped_line(&gutter, gutter_width, true, Vec::new(), width));
    }
    lines
}

fn wrapped_line(
    gutter: &Span<'static>,
    gutter_width: usize,
    first: bool,
    body: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let prefix = if first {
        gutter.clone()
    } else {
        Span::styled(" ".repeat(gutter_width), gutter.style)
    };
    let mut spans = Vec::with_capacity(body.len() + 1);
    spans.push(prefix);
    spans.extend(body);
    let mut line = Line::from(spans);
    if let Some(background) = gutter.style.bg {
        let padding = width.saturating_sub(line.width());
        if padding > 0 {
            line.spans.push(Span::styled(
                " ".repeat(padding),
                Style::new().bg(background),
            ));
        }
    }
    line
}

fn push_styled_character(spans: &mut Vec<Span<'static>>, character: char, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push(character);
        return;
    }
    spans.push(Span::styled(character.to_string(), style));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn render_file_change(change: &FileChangeOutput, width: u16) -> Vec<Line<'static>> {
        super::render_file_change(
            change,
            width,
            DiffRenderPolicy::Bounded {
                max_rows: MAX_EDIT_RENDERED_DIFF_LINES,
            },
            DiffRowBackgrounds::Disabled,
        )
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn text(lines: Vec<Line<'static>>) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    fn find_line<'a>(lines: &'a [Line<'static>], needle: &str) -> &'a Line<'static> {
        lines
            .iter()
            .find(|line| line_text(line).contains(needle))
            .unwrap_or_else(|| panic!("missing rendered line containing {needle:?}"))
    }

    fn find_span<'a>(line: &'a Line<'static>, needle: &str) -> &'a Span<'static> {
        line.spans
            .iter()
            .find(|span| span.content.contains(needle))
            .unwrap_or_else(|| panic!("missing span containing {needle:?} in {line:?}"))
    }

    fn styled_characters(spans: &[Span<'static>]) -> Vec<(char, Style)> {
        spans
            .iter()
            .flat_map(|span| {
                span.content
                    .chars()
                    .map(move |character| (character, span.style))
            })
            .collect()
    }

    #[test]
    fn renders_add_update_move_delete_and_omitted_changes() {
        let changes = [
            FileChangeOutput {
                path: "new.rs".into(),
                change: FileChange::Add {
                    content: "one\ntwo\n".to_string(),
                },
            },
            FileChangeOutput {
                path: "old.rs".into(),
                change: FileChange::Delete {
                    content: "gone\n".to_string(),
                },
            },
            FileChangeOutput {
                path: "before.rs".into(),
                change: FileChange::Update {
                    unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                    move_path: Some("after.rs".into()),
                },
            },
            FileChangeOutput {
                path: "large.rs".into(),
                change: FileChange::Omitted {
                    operation: FileChangeOperation::Update,
                    reason: "too large".to_string(),
                    added: 10,
                    removed: 2,
                    bytes: 600_000,
                },
            },
        ];
        let rendered = changes
            .iter()
            .flat_map(|change| text(render_file_change(change, 80)))
            .collect::<Vec<_>>();

        for expected in [
            "Added 1 file",
            "Deleted 1 file",
            "Moved 1 file",
            "before.rs -> after.rs",
            "1 - old",
            "1 + new",
            "diff omitted (600000 bytes): too large",
        ] {
            assert!(
                rendered.iter().any(|line| line.contains(expected)),
                "missing {expected:?}: {rendered:?}"
            );
        }
    }

    #[test]
    fn rendered_diff_gutters_keep_diff_colors_and_rust_body_uses_syntax_colors() {
        let change = FileChangeOutput {
            path: "file.rs".into(),
            change: FileChange::Update {
                unified_diff: diffy::create_patch(
                    "fn demo() {\n    let old_value = 1;\n}\n",
                    "fn demo() {\n    let new_value = 2;\n}\n",
                )
                .to_string(),
                move_path: None,
            },
        };
        let lines = render_file_change(&change, 80);
        let deleted = find_line(&lines, "old_value");
        let inserted = find_line(&lines, "new_value");
        let context = find_line(&lines, "fn demo");
        let hunk = find_line(&lines, "@@");

        assert_eq!(
            find_span(deleted, " - ").style.fg,
            Some(theme().content.diff_deletion)
        );
        assert_eq!(
            find_span(inserted, " + ").style.fg,
            Some(theme().content.diff_addition)
        );
        assert_eq!(
            find_span(hunk, "@@").style.fg,
            Some(theme().content.diff_hunk)
        );
        for line in [deleted, inserted] {
            assert_eq!(
                find_span(line, "let").style.fg,
                Some(theme().syntax.keyword)
            );
        }
        assert_eq!(
            find_span(context, "fn").style.fg,
            Some(theme().syntax.keyword)
        );
    }

    #[test]
    fn enabled_row_backgrounds_fill_changed_visual_rows_and_preserve_syntax() {
        const WIDTH: u16 = 32;
        let change = FileChangeOutput {
            path: "file.rs".into(),
            change: FileChange::Update {
                unified_diff: diffy::create_patch(
                    "fn demo() {\n    let old_value = 12345678901234567890;\n}\n",
                    "fn demo() {\n    let new_value = 09876543210987654321;\n}\n",
                )
                .to_string(),
                move_path: None,
            },
        };
        let lines = super::render_file_change(
            &change,
            WIDTH,
            DiffRenderPolicy::Complete,
            DiffRowBackgrounds::Enabled,
        );
        let deleted = find_line(&lines, "old_value");
        let inserted = find_line(&lines, "new_value");
        for (line, background) in [
            (deleted, theme().content.diff_deletion_background),
            (inserted, theme().content.diff_addition_background),
        ] {
            assert_eq!(line.width(), usize::from(WIDTH));
            assert_eq!(line.spans[0].content, "    ");
            assert_eq!(line.spans[0].style.bg, None);
            assert!(
                line.spans[1..]
                    .iter()
                    .all(|span| span.style.bg == Some(background))
            );
            assert_eq!(
                find_span(line, "let").style.fg,
                Some(theme().syntax.keyword)
            );
        }

        let changed_rows = lines
            .iter()
            .filter(|line| {
                line.spans.iter().any(|span| {
                    matches!(
                        span.style.bg,
                        Some(background)
                            if background == theme().content.diff_addition_background
                                || background == theme().content.diff_deletion_background
                    )
                })
            })
            .collect::<Vec<_>>();
        assert!(changed_rows.len() >= 4, "long changed lines should wrap");
        for line in changed_rows {
            assert_eq!(line.width(), usize::from(WIDTH));
            assert_eq!(line.spans[0].style.bg, None);
            let background = line.spans[1]
                .style
                .bg
                .expect("changed gutter has a background");
            assert!(
                line.spans[1..]
                    .iter()
                    .all(|span| span.style.bg == Some(background))
            );
        }

        for needle in ["@@", "fn demo"] {
            assert!(
                find_line(&lines, needle)
                    .spans
                    .iter()
                    .all(|span| span.style.bg.is_none()),
                "{needle:?} should remain on the canvas"
            );
        }

        let disabled = super::render_file_change(
            &change,
            WIDTH,
            DiffRenderPolicy::Complete,
            DiffRowBackgrounds::Disabled,
        );
        for needle in ["old_value", "new_value"] {
            assert!(
                find_line(&disabled, needle)
                    .spans
                    .iter()
                    .all(|span| span.style.bg.is_none())
            );
        }
    }

    #[test]
    fn enabled_background_fills_an_empty_changed_row() {
        let lines = render_diff_line(
            2,
            DiffLineKind::Insert,
            Vec::new(),
            12,
            1,
            RenderLimit::Complete,
            DiffRowBackgrounds::Enabled,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 12);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| { span.style.bg == Some(theme().content.diff_addition_background) })
        );
    }

    #[test]
    fn whole_file_add_and_delete_use_path_grammar_with_colored_gutters() {
        for (needle, change, marker, gutter_color) in [
            (
                "added()",
                FileChangeOutput {
                    path: "added.rs".into(),
                    change: FileChange::Add {
                        content: "fn added() { // added comment\n}\n".to_string(),
                    },
                },
                " + ",
                theme().content.diff_addition,
            ),
            (
                "deleted()",
                FileChangeOutput {
                    path: "deleted.rs".into(),
                    change: FileChange::Delete {
                        content: "fn deleted() { // deleted comment\n}\n".to_string(),
                    },
                },
                " - ",
                theme().content.diff_deletion,
            ),
        ] {
            let lines = render_file_change(&change, 80);
            let code = find_line(&lines, needle);
            assert_eq!(find_span(code, marker).style.fg, Some(gutter_color));
            assert_eq!(find_span(code, "fn").style.fg, Some(theme().syntax.keyword));
            let comment = find_span(code, "comment");
            assert_eq!(comment.style.fg, Some(theme().syntax.comment));
            assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn unknown_paths_keep_flat_add_delete_and_context_body_styles() {
        let additions = render_file_change(
            &FileChangeOutput {
                path: "file.zevria-unknown".into(),
                change: FileChange::Add {
                    content: "added body".to_string(),
                },
            },
            80,
        );
        assert_eq!(
            find_span(find_line(&additions, "added body"), "added body")
                .style
                .fg,
            Some(theme().content.diff_addition)
        );

        let deletions = render_file_change(
            &FileChangeOutput {
                path: "file.zevria-unknown".into(),
                change: FileChange::Delete {
                    content: "deleted body".to_string(),
                },
            },
            80,
        );
        assert_eq!(
            find_span(find_line(&deletions, "deleted body"), "deleted body")
                .style
                .fg,
            Some(theme().content.diff_deletion)
        );

        let update = render_file_change(
            &FileChangeOutput {
                path: "file.zevria-unknown".into(),
                change: FileChange::Update {
                    unified_diff: diffy::create_patch(
                        "context body\ndeleted update\n",
                        "context body\nadded update\n",
                    )
                    .to_string(),
                    move_path: None,
                },
            },
            80,
        );
        for (needle, color) in [
            ("context body", theme().text.muted),
            ("deleted update", theme().content.diff_deletion),
            ("added update", theme().content.diff_addition),
        ] {
            assert_eq!(
                find_span(find_line(&update, needle), needle).style.fg,
                Some(color)
            );
        }
    }

    #[test]
    fn unified_diff_maintains_independent_old_and_new_multiline_states() {
        let unified_diff = concat!(
            "--- original\n",
            "+++ modified\n",
            "@@ -1,3 +1,3 @@\n",
            "-/* old comment\n",
            "+let replacement = 7;\n",
            " let shared = 42;\n",
            "-*/\n",
            "+let finished = 9;\n",
        );
        let lines = render_file_change(
            &FileChangeOutput {
                path: "file.rs".into(),
                change: FileChange::Update {
                    unified_diff: unified_diff.to_string(),
                    move_path: None,
                },
            },
            80,
        );
        let shared = find_line(&lines, "let shared");
        assert_eq!(
            find_span(shared, "let").style.fg,
            Some(theme().syntax.keyword),
            "displayed context must use the independent new-side parser state"
        );
    }

    #[test]
    fn extension_changing_move_selects_old_and_new_grammars_independently() {
        let lines = render_file_change(
            &FileChangeOutput {
                path: "before.rs".into(),
                change: FileChange::Update {
                    unified_diff: diffy::create_patch(
                        "// rust comment\nlet old_value = 1;\n",
                        "# python comment\nnew_value = 1\n",
                    )
                    .to_string(),
                    move_path: Some("after.py".into()),
                },
            },
            80,
        );

        for needle in ["rust comment", "python comment"] {
            let comment = find_span(find_line(&lines, needle), needle);
            assert_eq!(comment.style.fg, Some(theme().syntax.comment));
            assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn highlighted_spans_wrap_without_text_or_style_loss() {
        let grammar = grammar_for_path(Path::new("file.rs")).expect("Rust grammar");
        let mut highlighter = CodeHighlighter::new(grammar);
        let highlighted = highlighter.highlight_line("let value = 42;\n", add_style());
        let expected = styled_characters(&highlighted);
        let wrapped = render_diff_line(
            7,
            DiffLineKind::Insert,
            highlighted,
            8,
            1,
            RenderLimit::Complete,
            DiffRowBackgrounds::Disabled,
        );
        assert!(wrapped.len() > 1);
        assert_eq!(wrapped[0].spans[0].content, "7 + ");
        for line in &wrapped[1..] {
            assert_eq!(line.spans[0].content, "    ");
            assert_eq!(line.spans[0].style, add_style());
        }
        let actual = wrapped
            .iter()
            .flat_map(|line| styled_characters(&line.spans[1..]))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn empty_lines_and_crlf_keep_existing_visible_layout() {
        let lines = render_file_change(
            &FileChangeOutput {
                path: "file.rs".into(),
                change: FileChange::Add {
                    content: "let first = 1;\r\n\r\nlet second = 2;".to_string(),
                },
            },
            80,
        );
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(rendered.iter().all(|line| !line.contains('\r')));
        assert!(rendered.iter().any(|line| line == "    2 + "));
        assert!(rendered.iter().any(|line| line.contains("let first = 1;")));
        assert!(rendered.iter().any(|line| line.contains("let second = 2;")));
    }

    #[test]
    fn malformed_diff_falls_back_to_plain_lines_without_syntax() {
        let change = FileChangeOutput {
            path: "file.rs".into(),
            change: FileChange::Update {
                unified_diff: "not a patch\nstill visible".to_string(),
                move_path: None,
            },
        };
        let lines = render_file_change(&change, 80);
        assert_eq!(
            find_span(find_line(&lines, "not a patch"), "not a patch")
                .style
                .fg,
            None
        );
        assert!(
            text(lines)
                .iter()
                .any(|line| line.contains("still visible"))
        );
    }

    #[test]
    fn rename_only_update_keeps_headers_without_fabricating_a_body() {
        let lines = render_file_change(
            &FileChangeOutput {
                path: "before.rs".into(),
                change: FileChange::Update {
                    unified_diff: String::new(),
                    move_path: Some("after.rs".into()),
                },
            },
            80,
        );
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("Moved 1 file"));
        assert!(line_text(&lines[1]).contains("before.rs -> after.rs"));
    }

    #[test]
    fn complete_and_bounded_policies_apply_to_wrapped_rows() {
        let mut content = (0..400)
            .map(|index| format!("row-{index:04}-abcdefghijklmnopqrstuvwxyz0123456789\n"))
            .collect::<String>();
        content.push_str("FINAL\n");
        let change = FileChangeOutput {
            path: "large.unknown".into(),
            change: FileChange::Add { content },
        };

        let complete = text(render_file_change_body(
            &change,
            20,
            DiffRenderPolicy::Complete,
            DiffRowBackgrounds::Disabled,
        ));
        assert!(complete.len() > MAX_EDIT_RENDERED_DIFF_LINES);
        assert!(complete.iter().any(|line| line.contains("FINAL")));
        assert!(complete.iter().all(|line| !line.contains("diff truncated")));

        let bounded = text(render_file_change_body(
            &change,
            20,
            DiffRenderPolicy::Bounded {
                max_rows: MAX_EDIT_RENDERED_DIFF_LINES,
            },
            DiffRowBackgrounds::Disabled,
        ));
        assert_eq!(bounded.len(), MAX_EDIT_RENDERED_DIFF_LINES + 1);
        assert!(!bounded.iter().any(|line| line.contains("FINAL")));
        assert_eq!(
            bounded
                .iter()
                .filter(|line| line.contains("diff truncated after 1000 rendered lines"))
                .count(),
            1
        );
    }
}
