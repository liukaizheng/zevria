//! Render Markdown into ratatui lines using pulldown-cmark.
//!
//! [`markdown_lines`] walks the pulldown-cmark event stream with a small style
//! stack and a current-line span buffer, producing owned (`'static`) lines that
//! can be dropped straight into a ratatui `Paragraph`.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::{syntax::highlighted_code_lines, theme::theme};

/// Leading indent added per list nesting level, so lists read as offset blocks.
const LIST_INDENT: &str = "  ";

/// Convert a Markdown string into owned ratatui lines layered on top of `base`.
///
/// List-item lines wider than `wrap_width` are pre-wrapped with a hanging
/// indent so continuation lines align under the item's content — ratatui's
/// `Wrap` would restart them at column 0. Everything else is emitted at full
/// length for the caller to wrap. A `wrap_width` of `0` disables wrapping.
pub fn markdown_lines(input: &str, base: Style, wrap_width: usize) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::new(base, wrap_width);
    let parser = Parser::new_ext(
        input,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    for event in parser {
        renderer.handle(event);
    }
    renderer.finish()
}

struct MarkdownRenderer {
    base: Style,
    /// Display width the produced lines must fit in; list-item lines longer
    /// than this are wrapped with a hanging indent. `0` disables wrapping.
    wrap_width: usize,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    quote_depth: usize,
    code_block: Option<CodeBlockState>,
    pending_link_url: Option<String>,
    pending_blank: bool,
    /// Durable layout metadata for every currently open list item. Nested
    /// items push their own layout and reveal the parent's again when closed.
    open_items: Vec<ItemLayout>,
    /// Item-specific metadata for the currently buffered logical line. This
    /// separates its already-materialized prefix from the styled content that
    /// may need wrapping.
    current_item_line: Option<ItemLine>,
    /// The next emitted content should be seeded with the active item's
    /// continuation prefix. Keeping this deferred avoids prefix-only rows when
    /// a break is immediately followed by the end of an item or block.
    pending_item_continuation: bool,
    table: Option<TableState>,
}

struct CodeBlockState {
    language: Option<String>,
    text: String,
}

impl CodeBlockState {
    fn new(kind: CodeBlockKind<'_>) -> Self {
        let language = match kind {
            CodeBlockKind::Fenced(info) => fence_language(&info),
            CodeBlockKind::Indented => None,
        };
        Self {
            language,
            text: String::new(),
        }
    }
}

/// Durable layout of an open list item. The continuation prefix reaches the
/// same display column as the item's first content character and is reused by
/// both explicit and width-wrapped continuation rows.
#[derive(Clone)]
struct ItemLayout {
    hang_width: usize,
    continuation_prefix: Vec<Span<'static>>,
    /// Quote markers already represented in `continuation_prefix`. Quotes
    /// entered after the item opened are appended after the item indentation.
    quote_depth: usize,
}

/// Layout metadata for one buffered logical line within an item. Its prefix
/// may be the item's marker or its continuation prefix, while subsequent
/// width-wrapped rows always use the durable continuation prefix.
struct ItemLine {
    layout: ItemLayout,
    prefix_span_count: usize,
}

/// Buffered contents of a table while it streams in. Column widths depend on the
/// widest cell, so cells are collected here and the grid is emitted only once the
/// table closes.
struct TableState {
    alignments: Vec<Alignment>,
    /// Finished rows, each a list of cells, each cell a list of inline spans.
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Number of leading `rows` that make up the header (0 or 1 for GFM).
    header_rows: usize,
    current_row: Vec<Vec<Span<'static>>>,
    current_cell: Vec<Span<'static>>,
    in_cell: bool,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            rows: Vec::new(),
            header_rows: 0,
            current_row: Vec::new(),
            current_cell: Vec::new(),
            in_cell: false,
        }
    }

    fn start_row(&mut self) {
        self.current_row = Vec::new();
    }

    fn start_cell(&mut self) {
        self.current_cell = Vec::new();
        self.in_cell = true;
    }

    fn end_cell(&mut self) {
        self.current_row
            .push(std::mem::take(&mut self.current_cell));
        self.in_cell = false;
    }

    fn end_row(&mut self, is_header: bool) {
        self.rows.push(std::mem::take(&mut self.current_row));
        if is_header {
            self.header_rows = self.rows.len();
        }
    }
}

impl MarkdownRenderer {
    fn new(base: Style, wrap_width: usize) -> Self {
        Self {
            base,
            wrap_width,
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![base],
            list_stack: Vec::new(),
            quote_depth: 0,
            code_block: None,
            pending_link_url: None,
            pending_blank: false,
            open_items: Vec::new(),
            current_item_line: None,
            pending_item_continuation: false,
            table: None,
        }
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&self.base)
    }

    fn push_span(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        let span = Span::styled(text, style);
        // While a table cell is open, inline content is buffered into that cell
        // instead of the current line, so the grid can be laid out once widths
        // are known.
        if let Some(table) = self.table.as_mut()
            && table.in_cell
        {
            table.current_cell.push(span);
            return;
        }

        self.materialize_item_continuation();
        self.current_spans.push(span);
    }

    /// Build the visual prefix for `depth` nested block quotes.
    fn quote_prefix_for_depth(depth: usize) -> Vec<Span<'static>> {
        if depth == 0 {
            Vec::new()
        } else {
            vec![Span::styled(
                "▎ ".repeat(depth),
                Style::default().fg(theme().text.muted),
            )]
        }
    }

    /// Prefix every rendered row with the current block-quote marker. List
    /// layouts keep a copy so their continuation rows retain quote prefixing.
    fn quote_prefix(&self) -> Vec<Span<'static>> {
        Self::quote_prefix_for_depth(self.quote_depth)
    }

    /// Extend an item-line layout with quotes entered after the item opened.
    /// Their markers follow the item's marker or continuation indentation,
    /// preserving the visual nesting order (`  • ▎ text` / `    ▎ text`).
    fn extend_layout_to_quote_depth(
        layout: &mut ItemLayout,
        quote_depth: usize,
    ) -> Vec<Span<'static>> {
        let added_depth = quote_depth.saturating_sub(layout.quote_depth);
        if added_depth == 0 {
            return Vec::new();
        }

        let prefix = Self::quote_prefix_for_depth(added_depth);
        layout.hang_width += prefix.iter().map(|span| span.width()).sum::<usize>();
        layout.continuation_prefix.extend(prefix.iter().cloned());
        layout.quote_depth = quote_depth;
        prefix
    }

    /// Defer the active item's continuation prefix until visible content is
    /// emitted. Prefix-only buffers would otherwise turn trailing hard breaks
    /// into phantom whitespace rows.
    fn prepare_item_continuation(&mut self) {
        self.pending_item_continuation =
            self.current_spans.is_empty() && !self.open_items.is_empty();
    }

    /// Materialize the active item's deferred continuation prefix and attach
    /// per-line metadata so this logical row can hang-wrap like the marker row.
    /// If a block quote began inside the item, its marker follows the active
    /// item prefix instead of being lost or moved in front of the list.
    fn materialize_item_continuation(&mut self) {
        if self.pending_item_continuation {
            self.pending_item_continuation = false;

            let Some(mut layout) = self.open_items.last().cloned() else {
                return;
            };
            Self::extend_layout_to_quote_depth(&mut layout, self.quote_depth);
            let prefix = self.continuation_prefix(&layout);
            let prefix_span_count = prefix.len();
            self.current_spans.extend(prefix);
            self.current_item_line = Some(ItemLine {
                layout,
                prefix_span_count,
            });
            return;
        }

        // The item marker may already be buffered when its first child is a
        // block quote (`- > quoted`). Add only quote levels that were entered
        // after that marker was built, and treat them as part of the line prefix
        // for hanging-wrap calculations.
        let Some(item_line) = self.current_item_line.as_mut() else {
            return;
        };
        let prefix = Self::extend_layout_to_quote_depth(&mut item_line.layout, self.quote_depth);
        item_line.prefix_span_count += prefix.len();
        self.current_spans.extend(prefix);
    }

    /// Fit a continuation prefix into the available hanging-indent width. At
    /// ordinary widths this is the item's exact content column; at extremely
    /// narrow widths it is clamped to preserve the existing one-column content
    /// budget.
    fn continuation_prefix(&self, layout: &ItemLayout) -> Vec<Span<'static>> {
        let width = if self.wrap_width == 0 {
            layout.hang_width
        } else {
            layout.hang_width.min(self.wrap_width.saturating_sub(1))
        };
        fit_prefix(&layout.continuation_prefix, width, self.base)
    }

    /// Push the buffered spans as a finished line and start a fresh buffer.
    /// Always emits, even when empty, so blank lines inside code blocks and
    /// between explicit breaks are preserved. Every logical line within a
    /// list item carries line metadata and wraps with the item's durable
    /// hanging indent when it overflows `wrap_width`.
    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        match self.current_item_line.take() {
            Some(item)
                if self.wrap_width > 0
                    && spans.iter().map(|span| span.width()).sum::<usize>() > self.wrap_width =>
            {
                self.wrap_item(spans, item);
            }
            _ => self.lines.push(Line::from(spans)),
        }
        self.prepare_item_continuation();
    }

    /// Wrap an overflowing logical line within a list item. Its current prefix
    /// (the marker on the first row, otherwise a continuation prefix) stays on
    /// that row, and all additional rows use the item's durable continuation
    /// prefix. Breaks at whitespace where possible and preserves inline span
    /// styles.
    fn wrap_item(&mut self, mut spans: Vec<Span<'static>>, item: ItemLine) {
        let hang_width = item
            .layout
            .hang_width
            .min(self.wrap_width.saturating_sub(1));
        let content_budget = self.wrap_width - hang_width;

        let content_spans = spans.split_off(item.prefix_span_count);
        let prefix_spans = spans;

        let mut rows = pack_rows(tokenize(&content_spans), content_budget).into_iter();

        let mut first_line = prefix_spans;
        first_line.extend(rows.next().unwrap_or_default());
        self.lines.push(Line::from(first_line));

        let continuation_prefix = self.continuation_prefix(&item.layout);
        for row in rows {
            let mut line_spans = continuation_prefix.clone();
            line_spans.extend(row);
            self.lines.push(Line::from(line_spans));
        }
    }

    /// Flush only if the current line has content, so block boundaries don't
    /// leave stray blank lines.
    fn soft_end(&mut self) {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
    }

    /// Seed a fresh line with either the active item's deferred continuation
    /// layout or the current block-quote prefix. A marker already buffered for
    /// an item's first paragraph is left untouched.
    fn start_line(&mut self) {
        if !self.current_spans.is_empty() {
            return;
        }
        if self.open_items.is_empty() {
            self.pending_item_continuation = false;
            self.current_spans.extend(self.quote_prefix());
        } else {
            self.prepare_item_continuation();
        }
    }

    /// End a soft/hard line break: flush and re-seed the block-quote prefix.
    fn break_line(&mut self) {
        self.flush_line();
        self.start_line();
    }

    /// Insert a blank separator line between sibling blocks (never at the top,
    /// never between list items).
    fn separate(&mut self) {
        if self.pending_blank && !self.lines.is_empty() && self.list_stack.is_empty() {
            self.lines.push(Line::from(String::new()));
        }
        self.pending_blank = false;
    }

    /// Finish the current block; the next block gets a blank line before it.
    fn end_block(&mut self) {
        self.soft_end();
        self.pending_blank = true;
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let style = self.current_style().fg(theme().content.code);
                self.push_span(code.to_string(), style);
            }
            Event::SoftBreak | Event::HardBreak => {
                // A line break inside a table cell would corrupt the grid, so
                // collapse it to a space; elsewhere it ends the current line.
                if self.table.as_ref().is_some_and(|table| table.in_cell) {
                    self.push_span(" ", self.current_style());
                } else {
                    self.break_line();
                }
            }
            Event::Rule => {
                self.separate();
                self.start_line();
                self.push_span("────────", Style::default().fg(theme().surfaces.border));
                self.end_block();
            }
            // Inline/block HTML and other events are rendered as their raw text
            // so nothing is silently dropped.
            Event::Html(html) | Event::InlineHtml(html) => {
                let style = self.current_style().fg(theme().text.muted);
                self.push_span(html.to_string(), style);
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.separate();
                self.start_line();
            }
            Tag::Heading { level, .. } => {
                let style = self
                    .current_style()
                    .fg(theme().content.heading)
                    .add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
                self.separate();
                self.start_line();
                self.push_span(format!("{} ", "#".repeat(heading_depth(level))), style);
            }
            Tag::BlockQuote(_) => {
                self.separate();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                let style = self.current_style().fg(theme().content.code);
                self.style_stack.push(style);
                self.code_block = Some(CodeBlockState::new(kind));
                self.separate();
                self.start_line();
            }
            Tag::List(start) => {
                self.separate();
                self.list_stack.push(start);
            }
            Tag::Item => {
                // Flush a parent's current row, but do not materialize its
                // deferred continuation before constructing the child marker.
                self.soft_end();
                self.pending_item_continuation = false;
                self.current_item_line = None;

                let depth = self.list_stack.len().max(1);
                let indent = LIST_INDENT.repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                let style = self.current_style();
                let quote_prefix = self.quote_prefix();
                self.current_spans.extend(quote_prefix.iter().cloned());

                let marker_prefix = format!("{indent}{marker}");
                let marker_width = display_width(&marker_prefix);
                self.current_spans.push(Span::styled(marker_prefix, style));

                let hang_width = self.current_spans.iter().map(|span| span.width()).sum();
                let mut continuation_prefix = quote_prefix;
                continuation_prefix.push(Span::styled(" ".repeat(marker_width), self.base));
                let layout = ItemLayout {
                    hang_width,
                    continuation_prefix,
                    quote_depth: self.quote_depth,
                };
                let prefix_span_count = self.current_spans.len();
                self.open_items.push(layout.clone());
                self.current_item_line = Some(ItemLine {
                    layout,
                    prefix_span_count,
                });
            }
            Tag::Emphasis => self.push_style(Modifier::ITALIC),
            Tag::Strong => {
                let style = self
                    .current_style()
                    .fg(theme().content.strong)
                    .add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
            }
            Tag::Strikethrough => self.push_style(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                self.pending_link_url = Some(dest_url.to_string());
                self.style_stack
                    .push(self.current_style().fg(theme().content.link));
            }
            Tag::Table(alignments) => {
                self.separate();
                self.table = Some(TableState::new(alignments));
            }
            Tag::TableHead => {
                // Header cells render bold; the style stack carries it into every
                // span pushed while the head is open.
                self.push_style(Modifier::BOLD);
                if let Some(table) = self.table.as_mut() {
                    table.start_row();
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.start_row();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.start_cell();
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_block(),
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.end_block();
            }
            TagEnd::CodeBlock => {
                if let Some(code_block) = self.code_block.take() {
                    self.emit_code_block(code_block);
                }
                self.style_stack.pop();
                self.end_block();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.end_block();
            }
            TagEnd::Item => {
                self.soft_end();
                self.pending_item_continuation = false;
                self.current_item_line = None;
                self.open_items.pop();
                self.prepare_item_continuation();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.style_stack.pop();
                if let Some(url) = self.pending_link_url.take() {
                    self.push_span(
                        format!(" ({url})"),
                        Style::default().fg(theme().content.link),
                    );
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.end_cell();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.end_row(false);
                }
            }
            TagEnd::TableHead => {
                self.style_stack.pop();
                if let Some(table) = self.table.as_mut() {
                    table.end_row(true);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.end_block();
            }
            _ => {}
        }
    }

    fn push_style(&mut self, modifier: Modifier) {
        let style = self.current_style().add_modifier(modifier);
        self.style_stack.push(style);
    }

    fn text(&mut self, text: &str) {
        if let Some(code_block) = self.code_block.as_mut() {
            code_block.text.push_str(text);
        } else {
            self.push_span(text.to_string(), self.current_style());
        }
    }

    /// Highlight a buffered fenced block while preserving its exact line layout.
    /// Unknown and omitted language tags retain the original uniform code color.
    fn emit_code_block(&mut self, code_block: CodeBlockState) {
        let fallback = self.current_style();
        let trailing_line_ending = code_block.text.ends_with('\n');
        let highlighted =
            highlighted_code_lines(&code_block.text, code_block.language.as_deref(), fallback);
        let line_count = highlighted.len();
        for (index, line) in highlighted.into_iter().enumerate() {
            for span in line.spans {
                self.push_span(span.content.into_owned(), span.style);
            }
            if index + 1 < line_count || trailing_line_ending {
                self.break_line();
            }
        }
    }

    /// Lay out the buffered table as aligned rows joined by ` │ `, with a `─┼─`
    /// rule under the header. Each column is padded to its widest cell's display
    /// width; when that would overflow `wrap_width`, the widest columns are
    /// narrowed and their cell content wraps onto extra lines within the column,
    /// so the grid stays aligned instead of spilling to column 0. The ` │ `
    /// separator and `─┼─` join are both three columns wide, so the crosses land
    /// exactly under the bars.
    fn emit_table(&mut self, table: TableState) {
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }

        let cell_width =
            |cell: &[Span<'static>]| -> usize { cell.iter().map(|span| span.width()).sum() };
        let mut widths = vec![0usize; cols];
        for row in &table.rows {
            for (col, cell) in row.iter().enumerate() {
                widths[col] = widths[col].max(cell_width(cell));
            }
        }

        let separator_width = 3 * (cols - 1);
        let needs_wrap =
            self.wrap_width > 0 && widths.iter().sum::<usize>() + separator_width > self.wrap_width;
        if needs_wrap {
            // Narrow the widest column one cell at a time until the grid fits,
            // but never below one column of content.
            let available = self.wrap_width.saturating_sub(separator_width).max(cols);
            while widths.iter().sum::<usize>() > available {
                let widest = widths
                    .iter()
                    .enumerate()
                    .max_by_key(|&(_, &width)| width)
                    .map_or(0, |(col, _)| col);
                if widths[widest] <= 1 {
                    break;
                }
                widths[widest] -= 1;
            }
        }

        let muted = Style::default().fg(theme().surfaces.border_strong);
        for (index, row) in table.rows.iter().enumerate() {
            // Each cell becomes one or more lines: its content wrapped to the
            // column width when the table was narrowed, or a single line as-is.
            let cell_lines: Vec<Vec<Vec<Span<'static>>>> = (0..cols)
                .map(|col| match row.get(col) {
                    Some(cell) if needs_wrap => pack_rows(tokenize(cell), widths[col].max(1)),
                    Some(cell) => vec![cell.clone()],
                    None => Vec::new(),
                })
                .collect();
            let row_height = cell_lines.iter().map(Vec::len).max().unwrap_or(0).max(1);

            for line_index in 0..row_height {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (col, &width) in widths.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span::styled(" │ ", muted));
                    }
                    let cell_line = cell_lines[col].get(line_index);
                    let content_width = cell_line.map_or(0, |line| cell_width(line));
                    let pad = width.saturating_sub(content_width);
                    let (left, right) = match table.alignments.get(col) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    if left > 0 {
                        spans.push(Span::styled(" ".repeat(left), self.base));
                    }
                    if let Some(cell_line) = cell_line {
                        spans.extend(cell_line.iter().cloned());
                    }
                    // Skip trailing padding on the final column so lines carry no
                    // dead whitespace off the right edge.
                    if right > 0 && col + 1 < cols {
                        spans.push(Span::styled(" ".repeat(right), self.base));
                    }
                }
                self.lines.push(Line::from(spans));
            }

            // Rule directly beneath the last header row.
            if table.header_rows > 0 && index + 1 == table.header_rows {
                let mut rule: Vec<Span<'static>> = Vec::new();
                for (col, &width) in widths.iter().enumerate() {
                    if col > 0 {
                        rule.push(Span::styled("─┼─", muted));
                    }
                    rule.push(Span::styled("─".repeat(width), muted));
                }
                self.lines.push(Line::from(rule));
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        // Drop a trailing blank line so streamed chunks don't accumulate gaps.
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn fence_language(info: &str) -> Option<String> {
    let language = info.split_whitespace().next()?.split(',').next()?.trim();
    (!language.is_empty()).then(|| language.to_string())
}

/// A run of same-styled text inside a [`Token`].
struct Fragment {
    text: String,
    style: Style,
}

/// A maximal run of whitespace or of non-whitespace ("word") content. A word
/// keeps one fragment per style so a line break never lands inside it.
struct Token {
    fragments: Vec<Fragment>,
    is_whitespace: bool,
    width: usize,
}

/// Display width of `text` as ratatui will measure it.
fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Clone a styled prefix at exactly `target_width` display columns, truncating
/// only at grapheme boundaries and padding if needed. This keeps block-quote
/// markers and their styles when an item continuation prefix fits normally,
/// while still allowing a one-column content budget on exceptionally narrow
/// terminals.
fn fit_prefix(
    spans: &[Span<'static>],
    target_width: usize,
    padding_style: Style,
) -> Vec<Span<'static>> {
    if target_width == 0 {
        return Vec::new();
    }
    if spans.iter().map(|span| span.width()).sum::<usize>() == target_width {
        return spans.to_vec();
    }

    let mut fitted = Vec::new();
    let mut width = 0;
    'spans: for span in spans {
        for grapheme in span.styled_graphemes(Style::default()) {
            let grapheme_width = display_width(grapheme.symbol);
            if width + grapheme_width > target_width {
                break 'spans;
            }
            push_coalesced(&mut fitted, grapheme.symbol, span.style);
            width += grapheme_width;
        }
    }
    if width < target_width {
        push_coalesced(
            &mut fitted,
            &" ".repeat(target_width - width),
            padding_style,
        );
    }
    fitted
}

/// Split spans into alternating whitespace/word tokens, preserving each
/// piece's style.
fn tokenize(spans: &[Span<'static>]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    for span in spans {
        let mut rest = span.content.as_ref();
        while let Some(first_char) = rest.chars().next() {
            let run_is_whitespace = first_char.is_whitespace();
            let run_end = rest
                .char_indices()
                .find(|&(_, ch)| ch.is_whitespace() != run_is_whitespace)
                .map_or(rest.len(), |(index, _)| index);
            let (run_text, remainder) = rest.split_at(run_end);
            let fragment = Fragment {
                text: run_text.to_string(),
                style: span.style,
            };
            let run_width = display_width(run_text);
            match tokens.last_mut() {
                Some(token) if token.is_whitespace == run_is_whitespace => {
                    token.fragments.push(fragment);
                    token.width += run_width;
                }
                _ => tokens.push(Token {
                    fragments: vec![fragment],
                    is_whitespace: run_is_whitespace,
                    width: run_width,
                }),
            }
            rest = remainder;
        }
    }
    tokens
}

/// Append `text` to the row, merging into the last span when styles match.
fn push_coalesced(row: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match row.last_mut() {
        Some(last_span) if last_span.style == style => {
            last_span.content.to_mut().push_str(text);
        }
        _ => row.push(Span::styled(text.to_string(), style)),
    }
}

/// Trim trailing whitespace off the row and move it into `rows`, dropping it
/// when nothing remains, and reset the running width.
fn finish_row(
    rows: &mut Vec<Vec<Span<'static>>>,
    current_row: &mut Vec<Span<'static>>,
    current_row_width: &mut usize,
) {
    while let Some(last_span) = current_row.last_mut() {
        let trimmed_length = last_span.content.trim_end().len();
        if trimmed_length == 0 {
            current_row.pop();
        } else {
            if trimmed_length < last_span.content.len() {
                last_span.content.to_mut().truncate(trimmed_length);
            }
            break;
        }
    }
    if !current_row.is_empty() {
        rows.push(std::mem::take(current_row));
    }
    *current_row_width = 0;
}

/// Greedily pack tokens into rows no wider than `content_budget`, breaking at
/// whitespace and hard-splitting words that are wider than the budget.
fn pack_rows(tokens: Vec<Token>, content_budget: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut current_row_width = 0usize;

    for token in tokens {
        if token.is_whitespace {
            // Whitespace never starts a wrapped row: keep it when it fits,
            // otherwise wrap here and swallow the separator.
            if current_row_width + token.width <= content_budget {
                for fragment in token.fragments {
                    push_coalesced(&mut current_row, &fragment.text, fragment.style);
                }
                current_row_width += token.width;
            } else {
                finish_row(&mut rows, &mut current_row, &mut current_row_width);
            }
            continue;
        }

        if current_row_width + token.width > content_budget {
            finish_row(&mut rows, &mut current_row, &mut current_row_width);
        }
        if token.width <= content_budget {
            for fragment in token.fragments {
                push_coalesced(&mut current_row, &fragment.text, fragment.style);
            }
            current_row_width += token.width;
        } else {
            // A single word wider than the budget: split it at grapheme
            // boundaries so it still makes progress.
            for fragment in token.fragments {
                let probe = Span::styled(fragment.text.as_str(), fragment.style);
                for grapheme in probe.styled_graphemes(Style::default()) {
                    let grapheme_width = display_width(grapheme.symbol);
                    if current_row_width + grapheme_width > content_budget {
                        finish_row(&mut rows, &mut current_row, &mut current_row_width);
                    }
                    push_coalesced(&mut current_row, grapheme.symbol, fragment.style);
                    current_row_width += grapheme_width;
                }
            }
        }
    }
    finish_row(&mut rows, &mut current_row, &mut current_row_width);
    rows
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    /// Wide enough that nothing wraps in tests that aren't about wrapping.
    const NO_WRAP: usize = 200;

    /// Collapse a rendered line back to plain text for assertions.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Display column where `needle` starts in a rendered line.
    fn content_column(line: &Line<'_>, needle: &str) -> usize {
        let text = line_text(line);
        let byte_index = text.find(needle).expect("content should be present");
        display_width(&text[..byte_index])
    }

    #[test]
    fn renders_heading_paragraph_and_inline_styles() {
        let lines = markdown_lines(
            "# Title\n\nHello **bold** and `code`.",
            Style::default(),
            NO_WRAP,
        );
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["# Title", "", "Hello bold and code."]);

        // The heading marker span carries the bold modifier.
        let heading = &lines[0];
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(heading.spans[0].style.fg, Some(theme().content.heading));

        let paragraph = &lines[2];
        let plain_span = paragraph
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "Hello ")
            .expect("plain text span should be present");
        assert_eq!(plain_span.style.fg, None);
        assert!(!plain_span.style.add_modifier.contains(Modifier::BOLD));

        let bold_span = paragraph
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "bold")
            .expect("bold text span should be present");
        assert_eq!(bold_span.style.fg, Some(theme().content.strong));
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn renders_bullets_and_code_blocks() {
        let markdown = "- one\n- two\n\n```\nfn main() {}\n```";
        let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • one", "  • two", "", "fn main() {}"]);
    }

    #[test]
    fn highlights_fenced_code_from_the_language_info_string() {
        let markdown = "```rust,ignore\nfn main() {\n    let message = \"hello\";\n}\n```";
        let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            text,
            vec!["fn main() {", "    let message = \"hello\";", "}"]
        );

        let mut foregrounds = Vec::new();
        for span in lines.iter().flat_map(|line| &line.spans) {
            if span
                .content
                .chars()
                .any(|character| !character.is_whitespace())
                && let Some(foreground) = span.style.fg
                && !foregrounds.contains(&foreground)
            {
                foregrounds.push(foreground);
            }
        }
        assert!(
            foregrounds.len() > 1,
            "expected multiple syntax colors, got {foregrounds:?}"
        );
        assert!(
            foregrounds
                .iter()
                .all(|color| matches!(color, Color::Rgb(_, _, _))),
            "expected syntect true-color output, got {foregrounds:?}"
        );
    }

    #[test]
    fn untagged_and_unknown_code_blocks_keep_the_plain_code_style() {
        for markdown in [
            "```\nfn plain() {}\n```",
            "```not-a-language\nfn unknown() {}\n```",
        ] {
            let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
            for span in lines.iter().flat_map(|line| &line.spans) {
                assert_eq!(span.style.fg, Some(theme().content.code));
            }
        }
    }

    #[test]
    fn highlighted_code_preserves_blank_lines() {
        let markdown = "```rust\nfn main() {\n\n}\n```";
        let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["fn main() {", "", "}"]);
    }

    #[test]
    fn ordered_lists_number_sequentially() {
        let lines = markdown_lines("1. first\n2. second", Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  1. first", "  2. second"]);
    }

    #[test]
    fn nested_lists_step_in_by_level() {
        let lines = markdown_lines("- a\n  - b", Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • a", "    • b"]);
    }

    #[test]
    fn unordered_item_soft_and_hard_breaks_stay_indented() {
        for markdown in [
            "- first line\n  second line",
            "- first line  \n  second line",
        ] {
            let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
            let text: Vec<String> = lines.iter().map(line_text).collect();
            assert_eq!(text, vec!["  • first line", "    second line"]);
        }
    }

    #[test]
    fn later_item_paragraphs_use_the_continuation_prefix() {
        let lines = markdown_lines(
            "- first paragraph\n\n  second paragraph",
            Style::default(),
            NO_WRAP,
        );
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • first paragraph", "    second paragraph"]);
    }

    #[test]
    fn ordered_and_nested_multiline_items_use_their_content_columns() {
        let ordered = markdown_lines("10. first\n    second", Style::default(), NO_WRAP);
        let ordered_text: Vec<String> = ordered.iter().map(line_text).collect();
        assert_eq!(ordered_text, vec!["  10. first", "      second"]);
        assert_eq!(content_column(&ordered[0], "first"), 6);
        assert_eq!(content_column(&ordered[1], "second"), 6);

        let nested = markdown_lines(
            "- parent first\n  - child first\n    child second\n\n  parent second",
            Style::default(),
            NO_WRAP,
        );
        let nested_text: Vec<String> = nested.iter().map(line_text).collect();
        assert_eq!(
            nested_text,
            vec![
                "  • parent first",
                "    • child first",
                "      child second",
                "    parent second",
            ]
        );
        for (line, needle, expected_column) in [
            (&nested[0], "parent first", 4),
            (&nested[1], "child first", 6),
            (&nested[2], "child second", 6),
            (&nested[3], "parent second", 4),
        ] {
            assert_eq!(content_column(line, needle), expected_column);
        }
    }

    #[test]
    fn trailing_hard_break_does_not_emit_a_prefix_only_row() {
        // Pulldown normally elides a break at a completed block boundary, but
        // streamed input can leave the renderer at this exact event boundary.
        // Drive it directly to ensure the deferred prefix is discarded when
        // the item closes before more content arrives.
        let mut renderer = MarkdownRenderer::new(Style::default(), NO_WRAP);
        renderer.start_tag(Tag::List(None));
        renderer.start_tag(Tag::Item);
        renderer.text("first");
        renderer.handle(Event::HardBreak);
        renderer.end_tag(TagEnd::Item);
        renderer.end_tag(TagEnd::List(false));

        let lines = renderer.finish();
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • first"]);
    }

    #[test]
    fn quoted_multiline_items_keep_the_quote_prefix() {
        let lines = markdown_lines("> - first\n>   second", Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["▎   • first", "▎     second"]);
    }

    #[test]
    fn blockquotes_nested_in_items_keep_the_quote_prefix() {
        for (markdown, expected) in [
            ("- item\n\n  > quoted", vec!["  • item", "    ▎ quoted"]),
            ("- > quoted", vec!["  • ▎ quoted"]),
        ] {
            let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
            let text: Vec<String> = lines.iter().map(line_text).collect();
            assert_eq!(text, expected, "markdown: {markdown:?}");
        }
    }

    #[test]
    fn blockquotes_are_prefixed() {
        let lines = markdown_lines("> quoted", Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["▎ quoted"]);
    }

    #[test]
    fn renders_table_as_aligned_grid() {
        let markdown = "| Name | Score |\n|------|-------|\n| alpha | 1 |\n| beta | 22 |";
        let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            text,
            vec!["Name  │ Score", "──────┼──────", "alpha │ 1", "beta  │ 22"]
        );
    }

    #[test]
    fn right_aligned_column_pads_on_the_left() {
        let markdown = "| Item | Qty |\n|:-----|----:|\n| a | 5 |\n| bb | 100 |";
        let lines = markdown_lines(markdown, Style::default(), NO_WRAP);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // The `Qty` column is right-aligned: content sits at the right edge, so
        // shorter values gain leading spaces.
        assert_eq!(
            text,
            vec!["Item │ Qty", "─────┼────", "a    │   5", "bb   │ 100"]
        );
    }

    #[test]
    fn table_header_cells_are_bold() {
        let lines = markdown_lines("| H |\n|---|\n| v |", Style::default(), NO_WRAP);
        // First emitted line is the header row; its content span is bold.
        let header = &lines[0];
        let header_span = header
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "H")
            .expect("header content span should be present");
        assert!(header_span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(header_span.style.fg, None);
    }

    #[test]
    fn long_list_items_wrap_with_hanging_indent() {
        let lines = markdown_lines("- alpha beta gamma delta", Style::default(), 14);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // Budget is 14 - 4 ("  • ") = 10 columns; continuations align under
        // "alpha", not at column 0.
        assert_eq!(text, vec!["  • alpha beta", "    gamma", "    delta"]);
    }

    #[test]
    fn ordered_items_hang_under_their_number() {
        let lines = markdown_lines("1. one two three four", Style::default(), 16);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  1. one two", "     three four"]);
    }

    #[test]
    fn nested_items_hang_at_their_own_depth() {
        let lines = markdown_lines("- a\n  - deep item that wraps", Style::default(), 16);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • a", "    • deep item", "      that wraps"]);
    }

    #[test]
    fn explicit_item_continuations_wrap_with_the_same_prefix() {
        let lines = markdown_lines("- first\n  alpha beta gamma", Style::default(), 14);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • first", "    alpha beta", "    gamma"]);
    }

    #[test]
    fn multiline_wrapped_items_keep_inline_styles() {
        let lines = markdown_lines("- **first\n  alpha beta gamma**", Style::default(), 14);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • first", "    alpha beta", "    gamma"]);

        let bold_text: String = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .inspect(|span| {
                assert_eq!(span.style.fg, Some(theme().content.strong));
            })
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(bold_text, "firstalpha betagamma");
    }

    #[test]
    fn wrapped_items_keep_inline_styles() {
        let lines = markdown_lines("- alpha **bold** tail", Style::default(), 10);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • alpha", "    bold", "    tail"]);

        // The wrapped continuation keeps the strong span's bold styling.
        let bold_span = &lines[1].spans[1];
        assert_eq!(bold_span.content.as_ref(), "bold");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold_span.style.fg, Some(theme().content.strong));
    }

    #[test]
    fn overlong_words_hard_split_and_stay_indented() {
        let lines = markdown_lines("- abcdefghijklmnop", Style::default(), 10);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text, vec!["  • abcdef", "    ghijkl", "    mnop"]);
    }

    #[test]
    fn wide_tables_wrap_cell_content_within_columns() {
        let markdown = "| Name | Description |\n|------|-------------|\n| a | one two three four |";
        let lines = markdown_lines(markdown, Style::default(), 20);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        // The Description column is narrowed from 18 to 13 so the grid fits in
        // 20 columns; its cell wraps onto a second line that stays inside the
        // column instead of spilling to column 0.
        assert_eq!(
            text,
            vec![
                "Name │ Description",
                "─────┼──────────────",
                "a    │ one two three",
                "     │ four",
            ]
        );
    }
}
