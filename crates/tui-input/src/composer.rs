//! Grapheme-aware visual layout for the multiline prompt composer.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;
use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;
use zevria_instructions::SkillMeta;

use crate::command::{ClassificationError, ClassifiedInput, CommandRegistry, MatchEntry};
use crate::completion::{
    CompletionAcceptance, CompletionEntry, CompletionKind, CompletionMenu, CompletionQuery,
    CompletionView, FileCompletionRequest, FileCompletionState, FileQueryIdentity,
    FileSearchStatus, command_query, file_query, file_reference,
};

/// One caret stop within a rendered visual row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaretStop {
    byte: usize,
    column: u16,
    navigable: bool,
}

/// One newline-delimited or soft-wrapped row of composer text.
///
/// `display_range` is the contiguous input slice rendered for this row. Soft
/// wrapping may hide an internal whitespace separator, so visual-row ranges do
/// not necessarily form a complete partition of the input.
#[derive(Debug, Eq, PartialEq)]
pub struct VisualRow {
    pub(crate) display_range: Range<usize>,
    stops: Vec<CaretStop>,
}

/// Screen coordinates of the input caret within the full, unscrolled text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaretPosition {
    pub row: usize,
    pub column: u16,
}

/// A width-specific visual projection of the input string.
#[derive(Debug, Eq, PartialEq)]
pub struct ComposerLayout {
    pub(crate) rows: Vec<VisualRow>,
    pub(crate) caret: CaretPosition,
}

impl VisualRow {
    pub fn display_range(&self) -> Range<usize> {
        self.display_range.clone()
    }
}

impl ComposerLayout {
    pub fn rows(&self) -> &[VisualRow] {
        &self.rows
    }
    pub const fn caret(&self) -> CaretPosition {
        self.caret
    }

    /// Split `input` into display-cell-bounded rows and locate `cursor`.
    pub fn new(input: &str, cursor: usize, width: u16) -> Self {
        let width = width.max(1);
        let cursor = clamp_cursor(input, cursor);
        let mut rows = Vec::new();
        let mut line_start = 0;

        for line in input.split('\n') {
            push_logical_line(&mut rows, line, line_start, width);
            line_start = line_start.saturating_add(line.len());
            if line_start < input.len() {
                // `split('\n')` omits the delimiter; skip it for the next
                // logical line's absolute byte coordinates.
                line_start += 1;
            }
        }

        let caret = rows
            .iter()
            .enumerate()
            .rev()
            .find_map(|(row, visual)| {
                visual
                    .stops
                    .iter()
                    .rev()
                    .find(|stop| stop.byte == cursor)
                    .map(|stop| CaretPosition {
                        row,
                        column: stop.column,
                    })
            })
            .unwrap_or_default();

        Self { rows, caret }
    }

    /// Move to the closest caret stop on the adjacent visual row.
    ///
    /// The returned column is the sticky goal used by repeated vertical
    /// movements, even when an intervening row is shorter.
    pub fn move_vertical(
        &self,
        cursor: usize,
        direction: i8,
        preferred_column: Option<u16>,
    ) -> (usize, u16) {
        let goal = preferred_column.unwrap_or(self.caret.column);
        let target = if direction < 0 {
            self.caret.row.checked_sub(1)
        } else {
            self.caret
                .row
                .checked_add(1)
                .filter(|row| *row < self.rows.len())
        };
        let Some(target) = target else {
            return (cursor, goal);
        };
        let stop = self.rows[target]
            .stops
            .iter()
            .filter(|stop| stop.navigable)
            .min_by_key(|stop| {
                (
                    stop.column.abs_diff(goal),
                    // On an exact tie, prefer the stop to the left of the
                    // requested display column.
                    u8::from(stop.column > goal),
                )
            })
            .expect("every visual row has at least one navigable caret stop");
        (stop.byte, goal)
    }
}

#[derive(Clone, Copy, Debug)]
struct MeasuredGrapheme {
    start: usize,
    end: usize,
    width: u16,
    whitespace: bool,
}

#[derive(Clone, Debug)]
struct GraphemeRun {
    graphemes: Range<usize>,
    width: u16,
    whitespace: bool,
}

#[derive(Debug)]
struct VisualRowBuilder {
    display_range: Range<usize>,
    width: u16,
    stops: Vec<CaretStop>,
}

impl VisualRowBuilder {
    fn new(start: usize) -> Self {
        Self {
            display_range: start..start,
            width: 0,
            stops: vec![CaretStop {
                byte: start,
                column: 0,
                navigable: true,
            }],
        }
    }

    fn has_display_text(&self) -> bool {
        !self.display_range.is_empty()
    }

    fn push_grapheme(&mut self, grapheme: MeasuredGrapheme) {
        debug_assert_eq!(self.display_range.end, grapheme.start);
        self.width = self.width.saturating_add(grapheme.width);
        self.display_range.end = grapheme.end;
        self.stops.push(CaretStop {
            byte: grapheme.end,
            column: self.width,
            navigable: true,
        });
    }

    fn alias_hidden_separator(&mut self, graphemes: &[MeasuredGrapheme]) {
        for grapheme in graphemes {
            self.stops.push(CaretStop {
                byte: grapheme.end,
                column: self.width,
                navigable: false,
            });
        }
    }

    fn finish(self) -> VisualRow {
        debug_assert!(self.stops.iter().any(|stop| stop.navigable));
        VisualRow {
            display_range: self.display_range,
            stops: self.stops,
        }
    }
}

fn push_logical_line(rows: &mut Vec<VisualRow>, line: &str, start: usize, width: u16) {
    let graphemes = line
        .grapheme_indices(true)
        .map(|(relative, grapheme)| MeasuredGrapheme {
            start: start.saturating_add(relative),
            end: start
                .saturating_add(relative)
                .saturating_add(grapheme.len()),
            width: grapheme.width().min(u16::MAX as usize) as u16,
            whitespace: grapheme.chars().any(char::is_whitespace),
        })
        .collect::<Vec<_>>();
    let runs = grapheme_runs(&graphemes);
    let mut row = VisualRowBuilder::new(start);
    let mut run_index = 0;

    while run_index < runs.len() {
        let run = &runs[run_index];
        if !run.whitespace {
            place_word(
                rows,
                &mut row,
                &graphemes[run.graphemes.clone()],
                run.width,
                width,
            );
            run_index += 1;
            continue;
        }

        let internal_separator = run_index > 0 && run_index + 1 < runs.len();
        if !internal_separator {
            push_wrapping_graphemes(rows, &mut row, &graphemes[run.graphemes.clone()], width);
            run_index += 1;
            continue;
        }

        let next_word = &runs[run_index + 1];
        debug_assert!(!next_word.whitespace);
        let separator_and_word_width = run.width.saturating_add(next_word.width);
        if next_word.width <= width && row.width.saturating_add(separator_and_word_width) <= width {
            push_unwrapped_graphemes(&mut row, &graphemes[run.graphemes.clone()]);
            push_unwrapped_graphemes(&mut row, &graphemes[next_word.graphemes.clone()]);
        } else {
            let separator = &graphemes[run.graphemes.clone()];
            row.alias_hidden_separator(separator);
            let word_start = separator
                .last()
                .map_or(row.display_range.end, |item| item.end);
            rows.push(row.finish());
            row = VisualRowBuilder::new(word_start);
            place_word(
                rows,
                &mut row,
                &graphemes[next_word.graphemes.clone()],
                next_word.width,
                width,
            );
        }
        run_index += 2;
    }

    rows.push(row.finish());
}

fn grapheme_runs(graphemes: &[MeasuredGrapheme]) -> Vec<GraphemeRun> {
    let mut runs = Vec::new();
    let mut run_start = 0;

    while run_start < graphemes.len() {
        let whitespace = graphemes[run_start].whitespace;
        let mut run_end = run_start + 1;
        let mut width = graphemes[run_start].width;
        while run_end < graphemes.len() && graphemes[run_end].whitespace == whitespace {
            width = width.saturating_add(graphemes[run_end].width);
            run_end += 1;
        }
        runs.push(GraphemeRun {
            graphemes: run_start..run_end,
            width,
            whitespace,
        });
        run_start = run_end;
    }

    runs
}

fn place_word(
    rows: &mut Vec<VisualRow>,
    row: &mut VisualRowBuilder,
    graphemes: &[MeasuredGrapheme],
    word_width: u16,
    width: u16,
) {
    if word_width <= width {
        if row.has_display_text() && row.width.saturating_add(word_width) > width {
            let word_start = graphemes
                .first()
                .map_or(row.display_range.end, |item| item.start);
            let previous = std::mem::replace(row, VisualRowBuilder::new(word_start));
            rows.push(previous.finish());
        }
        push_unwrapped_graphemes(row, graphemes);
        return;
    }

    if row.has_display_text() {
        let word_start = graphemes
            .first()
            .map_or(row.display_range.end, |item| item.start);
        let previous = std::mem::replace(row, VisualRowBuilder::new(word_start));
        rows.push(previous.finish());
    }
    push_wrapping_graphemes(rows, row, graphemes, width);
}

fn push_unwrapped_graphemes(row: &mut VisualRowBuilder, graphemes: &[MeasuredGrapheme]) {
    for grapheme in graphemes {
        row.push_grapheme(*grapheme);
    }
}

fn push_wrapping_graphemes(
    rows: &mut Vec<VisualRow>,
    row: &mut VisualRowBuilder,
    graphemes: &[MeasuredGrapheme],
    width: u16,
) {
    for grapheme in graphemes {
        if row.width > 0 && row.width.saturating_add(grapheme.width) > width {
            let previous = std::mem::replace(row, VisualRowBuilder::new(grapheme.start));
            rows.push(previous.finish());
        }
        row.push_grapheme(*grapheme);
    }
}

/// Logical composer/editor state. Render-only width and scroll measurements
/// remain in `ViewState`; visual vertical movement receives the last rendered
/// width explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageOccurrence {
    ordinal: u64,
    range: Range<usize>,
    image: PromptImage,
}

/// Content only: replay must never restore a generation or pending clipboard work.
/// PromptImage clones share the encoded image backing across snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ContentSnapshot {
    text: String,
    cursor: usize,
    images: Vec<ImageOccurrence>,
    next_ordinal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Backspace,
    Atomic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Transaction {
    before: ContentSnapshot,
    after: ContentSnapshot,
}

const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EditHistory {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    group: Option<EditKind>,
}

/// Complete memory-only draft, including non-recursive edit history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerDraft {
    content: ContentSnapshot,
    history: EditHistory,
    generation: u64,
}

#[derive(Debug, Default)]
pub struct ComposerState {
    text: String,
    cursor: usize,
    preferred_column: Option<u16>,
    menu: CompletionMenu,
    commands: CommandRegistry,
    files: FileCompletionState,
    images: Vec<ImageOccurrence>,
    next_ordinal: u64,
    generation: u64,
    paste_pending: bool,
    history: EditHistory,
}

impl ComposerState {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn is_paste_pending(&self) -> bool {
        self.paste_pending
    }
    pub fn begin_paste(&mut self) {
        self.close_edit_group();
        self.paste_pending = true;
    }
    pub fn finish_paste(&mut self, generation: u64) -> bool {
        if !self.paste_pending || self.generation != generation {
            return false;
        }
        self.paste_pending = false;
        true
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty() && !self.paste_pending
    }
    pub fn is_blank(&self) -> bool {
        !self.paste_pending && self.prompt().is_blank()
    }
    pub fn image_ranges(&self) -> Vec<Range<usize>> {
        self.images
            .iter()
            .map(|image| image.range.clone())
            .collect()
    }
    pub fn cancel_paste(&mut self) {
        self.close_edit_group();
        if self.paste_pending {
            self.paste_pending = false;
            self.generation = self.generation.wrapping_add(1);
        }
    }
    fn content_snapshot(&self) -> ContentSnapshot {
        ContentSnapshot {
            text: self.text.clone(),
            cursor: clamp_cursor(&self.text, self.cursor),
            images: self.images.clone(),
            next_ordinal: self.next_ordinal,
        }
    }

    pub fn snapshot(&self) -> ComposerDraft {
        ComposerDraft {
            content: self.content_snapshot(),
            history: self.history.clone(),
            generation: self.generation,
        }
    }
    pub fn matches_draft(&self, draft: &ComposerDraft) -> bool {
        // History bookkeeping is not acknowledgement identity. In particular,
        // editing and undoing to the same content still changes the live token.
        !self.paste_pending
            && self.generation == draft.generation
            && self.text == draft.content.text
            && self.images == draft.content.images
            && self.next_ordinal == draft.content.next_ordinal
    }
    pub fn restore(&mut self, draft: ComposerDraft) {
        self.cancel_paste();
        self.restore_content(draft.content);
        self.history = draft.history;
        self.close_edit_group();
    }

    fn restore_content(&mut self, content: ContentSnapshot) {
        self.suspend_file_completion();
        self.files.dismissed = None;
        self.text = content.text;
        self.cursor = clamp_cursor(&self.text, content.cursor);
        self.images = content.images;
        self.next_ordinal = content.next_ordinal;
        self.generation = self.generation.wrapping_add(1);
        self.reset_after_edit();
    }

    /// End a typing/Backspace run without changing content or discarding redo.
    pub fn close_edit_group(&mut self) {
        self.history.group = None;
    }

    pub fn undo(&mut self) -> bool {
        self.close_edit_group();
        let Some(transaction) = self.history.undo.pop() else {
            return false;
        };
        self.restore_content(transaction.before.clone());
        self.history.redo.push(transaction);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.close_edit_group();
        let Some(transaction) = self.history.redo.pop() else {
            return false;
        };
        self.restore_content(transaction.after.clone());
        self.history.undo.push(transaction);
        true
    }

    /// One user action, including all of its text, image and ordinal mutations.
    fn transact<R>(&mut self, kind: EditKind, edit: impl FnOnce(&mut Self) -> R) -> R {
        self.normalize_cursor();
        if kind == EditKind::Atomic || self.history.group != Some(kind) {
            self.close_edit_group();
        }
        let before = self.content_snapshot();
        let result = edit(self);
        self.normalize_cursor();
        let after = self.content_snapshot();
        // Cursor-only changes and failed/no-op edits do not consume redo.
        if before.text != after.text
            || before.images != after.images
            || before.next_ordinal != after.next_ordinal
        {
            self.generation = self.generation.wrapping_add(1);
            self.reset_after_edit();
            self.history.redo.clear();
            if self.history.group == Some(kind)
                && let Some(previous) = self.history.undo.last_mut()
                && previous.after == before
            {
                previous.after = after;
            } else {
                self.history.undo.push(Transaction { before, after });
                if self.history.undo.len() > HISTORY_LIMIT {
                    self.history.undo.remove(0);
                }
            }
            self.history.group = (kind != EditKind::Atomic).then_some(kind);
        }
        result
    }
    pub fn replace_prompt(&mut self, prompt: &UserPrompt) {
        self.clear();
        for block in prompt.blocks() {
            match block {
                PromptBlock::Text(text) => self.insert_text_inner(text),
                PromptBlock::Image(image) => self
                    .attach_image_inner(image.clone())
                    .expect("validated recalled prompt"),
            }
        }
        // A recalled prompt is a fresh baseline, not a series of user edits.
        self.history = EditHistory::default();
    }
    pub fn prompt(&self) -> UserPrompt {
        self.prompt_from(0)
    }
    pub fn prompt_from(&self, start: usize) -> UserPrompt {
        let mut blocks = Vec::new();
        let mut offset = start;
        for occurrence in &self.images {
            if occurrence.range.start < start {
                continue;
            }
            if offset < occurrence.range.start {
                blocks.push(PromptBlock::Text(
                    self.text[offset..occurrence.range.start].into(),
                ));
            }
            blocks.push(PromptBlock::Image(occurrence.image.clone()));
            offset = occurrence.range.end;
        }
        if offset < self.text.len() {
            blocks.push(PromptBlock::Text(self.text[offset..].into()));
        }
        UserPrompt::new(blocks).expect("composer enforces aggregate limits")
    }
    pub fn attach_image(&mut self, image: PromptImage) -> Result<(), zevria_content::PromptError> {
        self.transact(EditKind::Atomic, |state| state.attach_image_inner(image))
    }

    fn attach_image_inner(
        &mut self,
        image: PromptImage,
    ) -> Result<(), zevria_content::PromptError> {
        let mut blocks = self.prompt().into_blocks();
        blocks.push(PromptBlock::Image(image.clone()));
        UserPrompt::new(blocks)?;
        loop {
            self.next_ordinal += 1;
            if !self
                .text
                .contains(&format!("[image {}]", self.next_ordinal))
            {
                break;
            }
        }
        let token = format!("[image {}]", self.next_ordinal);
        let start = self.cursor;
        self.insert_text_inner(&token);
        self.images.push(ImageOccurrence {
            ordinal: self.next_ordinal,
            range: start..start + token.len(),
            image,
        });
        self.images.sort_by_key(|image| image.range.start);
        Ok(())
    }
    fn edit_range(&mut self, range: Range<usize>, replacement: &str) {
        let removed = range.end - range.start;
        self.images.retain_mut(|image| {
            let invalid = if range.is_empty() {
                image.range.start < range.start && range.start < image.range.end
            } else {
                range.start < image.range.end && range.end > image.range.start
            };
            if invalid {
                return false;
            }
            if image.range.start >= range.end {
                image.range.start = image.range.start - removed + replacement.len();
                image.range.end = image.range.end - removed + replacement.len();
            }
            true
        });
        self.text.replace_range(range, replacement);
    }

    pub fn worker_commands(&mut self) {
        self.commands = CommandRegistry::worker();
    }
    pub fn with_skills(&mut self, skills: Vec<SkillMeta>) {
        if self.completion_kind() == Some(CompletionKind::File) {
            self.commands = CommandRegistry::new(skills);
            return;
        }
        let cursor = clamp_cursor(&self.text, self.cursor);
        let selected = self
            .commands
            .matches(&self.text, cursor)
            .get(self.menu.selected)
            .map(|entry| (entry.sigil(), entry.name().to_owned()));
        self.commands = CommandRegistry::new(skills);
        let matches = self.commands.matches(&self.text, cursor);
        self.menu.selected = selected
            .and_then(|(sigil, name)| {
                matches
                    .iter()
                    .position(|entry| entry.sigil() == sigil && entry.name() == name)
            })
            .unwrap_or(self.menu.selected.min(matches.len().saturating_sub(1)));
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn preferred_column(&self) -> Option<u16> {
        self.preferred_column
    }

    pub const fn menu(&self) -> &CompletionMenu {
        &self.menu
    }

    pub const fn registry(&self) -> &CommandRegistry {
        &self.commands
    }

    pub fn classify(&self) -> Result<ClassifiedInput, ClassificationError> {
        self.commands.classify_prompt(&self.prompt())
    }

    fn file_identity(&self) -> Option<FileQueryIdentity> {
        Some(FileQueryIdentity {
            generation: self.generation,
            cursor: clamp_cursor(&self.text, self.cursor),
            query: file_query(
                &self.text,
                self.cursor,
                self.images.iter().map(|image| image.range.clone()),
            )?,
        })
    }

    pub fn completion_query(&self) -> Option<CompletionQuery> {
        if let Some(identity) = self.file_identity() {
            return (self.files.dismissed.as_ref() != Some(&identity)).then_some(identity.query);
        }
        command_query(&self.text, self.cursor)
    }

    pub fn completion_kind(&self) -> Option<CompletionKind> {
        self.completion_query().map(|query| query.kind)
    }

    fn current_file_results(&self) -> bool {
        self.files.active.as_ref().is_some_and(|request| {
            self.file_identity().as_ref() == Some(&request.identity)
                && self.files.dismissed.as_ref() != Some(&request.identity)
        })
    }

    pub fn completion_view(&self) -> Option<CompletionView<'_>> {
        let kind = self.completion_kind()?;
        let (entries, status) = if kind == CompletionKind::File {
            if self.current_file_results() {
                (
                    self.files
                        .paths
                        .iter()
                        .map(|path| CompletionEntry::File(path))
                        .collect(),
                    self.files.status.clone(),
                )
            } else {
                (
                    Vec::new(),
                    FileSearchStatus {
                        loading: true,
                        ..Default::default()
                    },
                )
            }
        } else {
            (
                self.commands
                    .matches(&self.text, self.cursor)
                    .into_iter()
                    .map(CompletionEntry::Command)
                    .collect(),
                FileSearchStatus::default(),
            )
        };
        Some(CompletionView {
            kind,
            entries,
            status,
        })
    }

    pub fn matching_completion_count(&self) -> usize {
        self.completion_view().map_or(0, |view| view.entries.len())
    }

    pub fn completion_filter_active(&self) -> bool {
        self.completion_kind().is_some()
    }

    /// Called by the UI owner, never by rendering. A continuing activation can
    /// change its query without starting a new workspace traversal.
    pub fn reconcile_file_completion(&mut self, enabled: bool) -> Option<FileCompletionRequest> {
        let current = self.file_identity();
        if enabled && self.files.dismissed.as_ref() != current.as_ref() {
            self.files.dismissed = None;
        }
        let identity = enabled.then_some(current).flatten();
        let Some(identity) =
            identity.filter(|identity| self.files.dismissed.as_ref() != Some(identity))
        else {
            self.suspend_file_completion();
            return None;
        };
        self.files.dismissed = None;
        if self
            .files
            .active
            .as_ref()
            .is_some_and(|active| active.identity == identity)
        {
            return self.files.active.clone();
        }
        if self.files.active.is_none() {
            self.files.next_activation = self.files.next_activation.wrapping_add(1);
        }
        self.files.next_request = self.files.next_request.wrapping_add(1);
        if self.files.query.as_ref() != Some(&identity) {
            self.files.paths.clear();
            self.menu.selected = 0;
        }
        self.files.query = Some(identity.clone());
        self.files.active = Some(FileCompletionRequest {
            activation: self.files.next_activation,
            request: self.files.next_request,
            identity,
        });
        self.files.status = FileSearchStatus {
            loading: true,
            ..Default::default()
        };
        self.files.active.clone()
    }

    /// Ownership/lifecycle invalidation is independent of draft identity.
    pub fn suspend_file_completion(&mut self) {
        self.files.active = None;
        // Keep the last highlighted path across pane/overlay ownership changes.
        // It is only selectable after the same query is explicitly reconciled.
        self.files.status = FileSearchStatus::default();
    }

    pub fn install_file_results(
        &mut self,
        request: &FileCompletionRequest,
        paths: Vec<String>,
        status: FileSearchStatus,
    ) -> bool {
        if self.files.active.as_ref() != Some(request) || !self.current_file_results() {
            return false;
        }
        let selected = self.files.paths.get(self.menu.selected);
        self.menu.selected = selected
            .and_then(|path| paths.iter().position(|entry| entry == path))
            .unwrap_or(self.menu.selected.min(paths.len().saturating_sub(1)));
        self.files.paths = paths;
        self.files.status = status;
        true
    }

    pub fn move_menu_up(&mut self) {
        self.close_edit_group();
        self.menu.move_up();
    }

    pub fn move_menu_down(&mut self) {
        self.close_edit_group();
        let count = self.matching_completion_count();
        self.menu.move_down(count);
    }

    pub fn move_menu_by(&mut self, amount: isize) {
        self.close_edit_group();
        self.menu.selected = self
            .menu
            .selected
            .saturating_add_signed(amount)
            .min(self.matching_completion_count().saturating_sub(1));
    }

    pub fn accept_highlighted_completion(&mut self) -> Option<CompletionAcceptance> {
        self.transact(EditKind::Atomic, Self::accept_highlighted_completion_inner)
    }

    fn accept_highlighted_completion_inner(&mut self) -> Option<CompletionAcceptance> {
        let query = self.completion_query()?;
        let replacement = query.replacement;
        let (invocation, accepted) = if query.kind == CompletionKind::File {
            if !self.current_file_results() {
                return None;
            }
            let index = self
                .menu
                .selected
                .min(self.files.paths.len().checked_sub(1)?);
            (
                file_reference(&self.files.paths[index]),
                CompletionAcceptance::Text,
            )
        } else {
            let matches = self.commands.matches(&self.text, self.cursor);
            let entry = &matches[self.menu.selected.min(matches.len().checked_sub(1)?)];
            (
                format!("{}{}", entry.sigil(), entry.name()),
                if matches!(entry, MatchEntry::Builtin(_)) {
                    CompletionAcceptance::Builtin
                } else {
                    CompletionAcceptance::Text
                },
            )
        };
        let suffix = self.text[replacement.end..].to_string();
        let mut text = String::with_capacity(
            self.text.len() - (replacement.end - replacement.start) + invocation.len() + 1,
        );
        text.push_str(&self.text[..replacement.start]);
        text.push_str(&invocation);
        let cursor = match suffix.graphemes(true).next() {
            Some(grapheme) if grapheme.chars().next().is_some_and(char::is_whitespace) => {
                let cursor = text.len() + grapheme.len();
                text.push_str(&suffix);
                cursor
            }
            _ => {
                text.push(' ');
                let cursor = text.len();
                text.push_str(&suffix);
                cursor
            }
        };
        // Completion is a text edit, not whole-draft replacement.
        let inserted_end = text.len() - suffix.len();
        self.edit_range(replacement.clone(), &text[replacement.start..inserted_end]);
        self.cursor = cursor;
        self.suspend_file_completion();
        Some(accepted)
    }

    pub fn cancel_completion(&mut self) -> bool {
        if self.completion_kind() == Some(CompletionKind::File) {
            self.close_edit_group();
            self.files.dismissed = self.file_identity();
            self.suspend_file_completion();
            return true;
        }
        self.transact(EditKind::Atomic, Self::cancel_completion_inner)
    }

    fn cancel_completion_inner(&mut self) -> bool {
        let Some(replacement) =
            command_query(&self.text, self.cursor).map(|query| query.replacement())
        else {
            return false;
        };
        self.edit_range(replacement.clone(), "");
        self.cursor = replacement.start;
        self.reset_navigation();
        self.menu.selected = 0;
        true
    }

    /// Lifecycle reset: accepted/discarded input must never be resurrected.
    pub fn clear(&mut self) {
        self.suspend_file_completion();
        self.files.paths.clear();
        self.files.query = None;
        self.files.dismissed = None;
        self.history = EditHistory::default();
        self.next_ordinal = 0;
        self.images.clear();
        self.paste_pending = false;
        self.generation = self.generation.wrapping_add(1);
        self.text.clear();
        self.cursor = 0;
        self.reset_navigation();
        self.menu.selected = 0;
    }

    /// Undoable local clear. Pending work is canceled, never saved in history.
    pub fn clear_if_nonempty(&mut self) -> bool {
        self.close_edit_group();
        if self.is_empty() {
            return false;
        }
        self.cancel_paste();
        self.transact(EditKind::Atomic, |state| {
            state.edit_range(0..state.text.len(), "");
            state.cursor = 0;
        });
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace(&mut self, text: String, cursor: usize) {
        self.clear();
        self.text = text;
        self.cursor = clamp_cursor(&self.text, cursor);
        self.reset_navigation();
        self.menu.selected = 0;
    }

    pub fn insert_character(&mut self, character: char) {
        let kind = if character == '\n' || character == '\r' {
            EditKind::Atomic
        } else {
            EditKind::Typing
        };
        self.transact(kind, |state| {
            let mut encoded = [0; 4];
            state.insert_text_inner(character.encode_utf8(&mut encoded));
        });
    }

    pub fn insert_text(&mut self, text: &str) {
        self.transact(EditKind::Atomic, |state| state.insert_text_inner(text));
    }

    fn insert_text_inner(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.edit_range(self.cursor..self.cursor, text);
        self.cursor += text.len();
        // Insertion can join graphemes on either side (e.g. an emoji ZWJ).
        // Keep the caret after the complete joined grapheme, not inside it.
        let boundary = clamp_cursor(&self.text, self.cursor);
        if boundary != self.cursor {
            self.cursor = next_boundary(&self.text, boundary);
        }
    }

    pub fn backspace(&mut self) {
        self.transact(EditKind::Backspace, Self::backspace_inner);
    }

    fn backspace_inner(&mut self) {
        let previous = previous_boundary(&self.text, self.cursor);
        if previous == self.cursor {
            return;
        }
        let mut removal = previous..self.cursor;
        for image in &self.images {
            if removal.start < image.range.end && removal.end > image.range.start {
                // Interior Backspace must also remove the marker suffix after the caret.
                removal.start = removal.start.min(image.range.start);
                removal.end = removal.end.max(image.range.end);
            }
        }
        let cursor = removal.start;
        self.edit_range(removal, "");
        self.cursor = clamp_cursor(&self.text, cursor);
    }

    pub fn delete_to_line_end(&mut self) {
        self.transact(EditKind::Atomic, |state| {
            let line = logical_line_bounds(&state.text, state.cursor);
            if state.cursor < line.end {
                state.edit_range(state.cursor..line.end, "");
            }
        });
    }

    pub fn delete_current_line(&mut self) {
        self.transact(EditKind::Atomic, Self::delete_current_line_inner);
    }

    fn delete_current_line_inner(&mut self) {
        let line = logical_line_bounds(&self.text, self.cursor);
        let removal = if line.start == 0 && line.end == self.text.len() {
            line
        } else if line.end < self.text.len() {
            line.start..line.end.saturating_add(1)
        } else {
            line.start.saturating_sub(1)..line.end
        };
        self.edit_range(removal.clone(), "");
        self.cursor = removal.start;
        self.reset_after_edit();
    }

    pub fn move_left(&mut self) {
        self.close_edit_group();
        self.normalize_cursor();
        self.cursor = previous_boundary(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        self.close_edit_group();
        self.normalize_cursor();
        self.cursor = next_boundary(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn move_word_left(&mut self) {
        self.close_edit_group();
        self.normalize_cursor();
        self.cursor = previous_word_start(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn move_word_right(&mut self) {
        self.close_edit_group();
        self.normalize_cursor();
        self.cursor = next_word_start(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn move_vertical(&mut self, direction: i8, rendered_width: u16) {
        self.close_edit_group();
        self.normalize_cursor();
        let width = if rendered_width == 0 {
            u16::MAX
        } else {
            rendered_width
        };
        let layout = ComposerLayout::new(&self.text, self.cursor, width);
        let (cursor, goal) = layout.move_vertical(self.cursor, direction, self.preferred_column);
        self.cursor = cursor;
        self.preferred_column = Some(goal);
    }

    /// Page within the measured editing viewport, retaining preferred column.
    pub fn page_vertical(&mut self, direction: i8, rows: usize, rendered_width: u16) {
        for _ in 0..rows.max(1) {
            let previous = self.cursor;
            self.move_vertical(direction, rendered_width);
            if self.cursor == previous {
                break;
            }
        }
    }

    pub fn move_home(&mut self) {
        self.close_edit_group();
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        self.preferred_column = None;
    }

    pub fn move_end(&mut self) {
        self.close_edit_group();
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
        self.preferred_column = None;
    }

    pub fn reset_navigation(&mut self) {
        self.close_edit_group();
        self.preferred_column = None;
    }

    fn reset_after_edit(&mut self) {
        self.preferred_column = None;
        self.menu.selected = 0;
    }

    fn normalize_cursor(&mut self) {
        self.cursor = clamp_cursor(&self.text, self.cursor);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_for_test(&mut self, text: impl Into<String>, cursor: usize) {
        self.replace(text.into(), cursor);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_menu_selection_for_test(&mut self, selected: usize) {
        self.menu.selected = selected;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_raw_for_test(
        &mut self,
        text: impl Into<String>,
        cursor: usize,
        preferred_column: Option<u16>,
        menu_selection: usize,
    ) {
        self.text = text.into();
        self.cursor = cursor;
        self.preferred_column = preferred_column;
        self.menu.selected = menu_selection;
    }
}

fn logical_line_bounds(input: &str, cursor: usize) -> Range<usize> {
    let cursor = clamp_cursor(input, cursor);
    let start = input[..cursor]
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1));
    let end = input[cursor..]
        .find('\n')
        .map_or(input.len(), |newline| cursor.saturating_add(newline));
    start..end
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme
        .chars()
        .any(|character| character.is_alphanumeric() || character == '_')
}

pub(crate) fn previous_word_start(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor(input, cursor);
    let graphemes = input[..cursor].grapheme_indices(true).collect::<Vec<_>>();
    let mut index = graphemes.len();

    while index > 0 && !is_word_grapheme(graphemes[index - 1].1) {
        index -= 1;
    }
    while index > 0 && is_word_grapheme(graphemes[index - 1].1) {
        index -= 1;
    }

    graphemes.get(index).map_or(0, |(byte, _)| *byte)
}

pub(crate) fn next_word_start(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor(input, cursor);
    let graphemes = input[cursor..].grapheme_indices(true).collect::<Vec<_>>();
    let mut index = 0;

    if graphemes
        .first()
        .is_some_and(|(_, grapheme)| is_word_grapheme(grapheme))
    {
        while index < graphemes.len() && is_word_grapheme(graphemes[index].1) {
            index += 1;
        }
    }
    while index < graphemes.len() && !is_word_grapheme(graphemes[index].1) {
        index += 1;
    }

    graphemes
        .get(index)
        .map_or(input.len(), |(byte, _)| cursor.saturating_add(*byte))
}

/// Clamp an arbitrary byte offset to the preceding grapheme boundary.
pub(crate) fn clamp_cursor(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    if cursor == input.len() {
        return cursor;
    }
    input
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .take_while(|byte| *byte <= cursor)
        .last()
        .unwrap_or(0)
}

/// The grapheme boundary immediately before `cursor`.
pub(crate) fn previous_boundary(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor(input, cursor);
    input
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .take_while(|byte| *byte < cursor)
        .last()
        .unwrap_or(0)
}

/// The grapheme boundary immediately after `cursor`.
pub(crate) fn next_boundary(input: &str, cursor: usize) -> usize {
    let cursor = clamp_cursor(input, cursor);
    input[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |grapheme| cursor + grapheme.len())
}

#[cfg(test)]
mod history_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn display_ranges(layout: &ComposerLayout) -> Vec<Range<usize>> {
        layout
            .rows
            .iter()
            .map(|row| row.display_range.clone())
            .collect()
    }

    fn displayed_rows<'a>(input: &'a str, layout: &ComposerLayout) -> Vec<&'a str> {
        layout
            .rows
            .iter()
            .map(|row| &input[row.display_range.clone()])
            .collect()
    }

    #[test]
    fn fitting_words_wrap_whole_and_hide_only_the_break_separator() {
        let input = "hello world";
        let layout = ComposerLayout::new(input, input.len(), 8);

        assert_eq!(display_ranges(&layout), vec![0..5, 6..11]);
        assert_eq!(displayed_rows(input, &layout), vec!["hello", "world"]);
        assert_eq!(input.as_bytes()[5], b' ');
    }

    #[test]
    fn all_spaces_in_a_selected_soft_break_are_preserved_but_not_displayed() {
        let input = "hello   world";
        let layout = ComposerLayout::new(input, input.len(), 8);

        assert_eq!(display_ranges(&layout), vec![0..5, 8..13]);
        assert_eq!(displayed_rows(input, &layout), vec!["hello", "world"]);
        assert_eq!(&input[5..8], "   ");
    }

    #[test]
    fn hidden_separator_carets_alias_the_visible_row_end() {
        let input = "hello   world";
        for cursor in 5..8 {
            assert_eq!(
                ComposerLayout::new(input, cursor, 8).caret,
                CaretPosition { row: 0, column: 5 },
                "separator boundary {cursor} should alias the preceding row"
            );
        }
        assert_eq!(
            ComposerLayout::new(input, 8, 8).caret,
            CaretPosition { row: 1, column: 0 }
        );

        let layout = ComposerLayout::new(input, 8, 8);
        assert!(
            layout.rows[0]
                .stops
                .iter()
                .any(|stop| stop.byte == 8 && !stop.navigable)
        );
        assert!(
            layout.rows[1]
                .stops
                .iter()
                .any(|stop| stop.byte == 8 && stop.navigable)
        );
    }

    #[test]
    fn vertical_movement_never_targets_hidden_separator_offsets() {
        let input = "hello   world";
        let lower = ComposerLayout::new(input, input.len(), 8);
        assert_eq!(lower.move_vertical(input.len(), -1, None), (5, 5));

        let upper = ComposerLayout::new(input, 0, 8);
        assert_eq!(upper.move_vertical(0, 1, None), (8, 0));
    }

    #[test]
    fn punctuation_remains_attached_to_its_wrapping_word() {
        let input = "hi hello,";
        let layout = ComposerLayout::new(input, input.len(), 8);

        assert_eq!(displayed_rows(input, &layout), vec!["hi", "hello,"]);
        assert_eq!(display_ranges(&layout), vec![0..2, 3..9]);
    }

    #[test]
    fn explicit_whitespace_and_empty_logical_lines_remain_visible() {
        let input = "  hi  \n   \n";
        let layout = ComposerLayout::new(input, input.len(), 2);

        assert_eq!(
            displayed_rows(input, &layout),
            vec!["  ", "hi", "  ", "  ", " ", ""]
        );
        assert_eq!(
            display_ranges(&layout),
            vec![0..2, 2..4, 4..6, 7..9, 9..10, 11..11]
        );
    }

    #[test]
    fn overwide_words_wrap_only_at_grapheme_boundaries() {
        let ascii = "abcdef";
        let ascii_layout = ComposerLayout::new(ascii, ascii.len(), 4);
        assert_eq!(displayed_rows(ascii, &ascii_layout), vec!["abcd", "ef"]);

        let combining = "a\u{301}bc";
        let combining_layout = ComposerLayout::new(combining, combining.len(), 2);
        assert_eq!(
            displayed_rows(combining, &combining_layout),
            vec!["a\u{301}b", "c"]
        );

        let wide = "界界";
        let wide_layout = ComposerLayout::new(wide, wide.len(), 1);
        assert_eq!(displayed_rows(wide, &wide_layout), vec!["界", "界"]);
    }

    #[test]
    fn layout_preserves_empty_lines_and_soft_wrap_boundaries() {
        let layout = ComposerLayout::new("ab\n\nwide", 8, 3);
        assert_eq!(display_ranges(&layout), vec![0..2, 3..3, 4..7, 7..8]);
        assert_eq!(layout.caret, CaretPosition { row: 3, column: 1 });
    }

    #[test]
    fn a_shared_hard_wrap_boundary_resolves_to_the_later_row() {
        let layout = ComposerLayout::new("abcdef", 3, 3);
        assert_eq!(layout.caret, CaretPosition { row: 1, column: 0 });
    }

    #[test]
    fn vertical_movement_keeps_a_sticky_display_column() {
        let input = "abcd\nx\nwxyz";
        let down = ComposerLayout::new(input, 3, 20);
        let (cursor, goal) = down.move_vertical(3, 1, None);
        assert_eq!((cursor, goal), (6, 3));

        let down_again = ComposerLayout::new(input, cursor, 20);
        assert_eq!(down_again.move_vertical(cursor, 1, Some(goal)), (10, 3));
    }

    #[test]
    fn word_boundaries_cover_editor_tokens_unicode_and_newlines() {
        let input = "  alpha...beta_gamma  e\u{301}lan\n世界!";
        let alpha = input.find("alpha").unwrap();
        let beta = input.find("beta_gamma").unwrap();
        let elan = input.find("e\u{301}lan").unwrap();
        let world = input.find("世界").unwrap();

        assert_eq!(previous_word_start(input, 0), 0);
        assert_eq!(previous_word_start(input, alpha + 3), alpha);
        assert_eq!(previous_word_start(input, alpha + "alpha".len()), alpha);
        assert_eq!(previous_word_start(input, beta), alpha);
        assert_eq!(previous_word_start(input, beta + "beta_".len()), beta);
        assert_eq!(previous_word_start(input, elan), beta);
        assert_eq!(previous_word_start(input, world), elan);
        assert_eq!(previous_word_start(input, input.len()), world);

        assert_eq!(next_word_start(input, 0), alpha);
        assert_eq!(next_word_start(input, alpha), beta);
        assert_eq!(next_word_start(input, alpha + 2), beta);
        assert_eq!(next_word_start(input, alpha + "alpha".len()), beta);
        assert_eq!(next_word_start(input, beta), elan);
        assert_eq!(next_word_start(input, beta + "beta_".len()), elan);
        assert_eq!(next_word_start(input, elan), world);
        assert_eq!(next_word_start(input, world), input.len());
        assert_eq!(next_word_start(input, input.len()), input.len());

        assert!(is_word_grapheme("_"));
        assert!(is_word_grapheme("e\u{301}"));
        assert!(is_word_grapheme("世"));
        assert!(!is_word_grapheme("."));
        assert!(!is_word_grapheme("\n"));
    }

    #[test]
    fn word_movement_resets_only_the_preferred_column() {
        let input = "alpha...beta";
        let beta = input.find("beta").unwrap();
        let mut state = ComposerState::default();
        state.set_raw_for_test(input, beta + 2, Some(7), 4);

        state.move_word_left();
        assert_eq!(state.text(), input);
        assert_eq!(state.cursor(), beta);
        assert_eq!(state.preferred_column(), None);
        assert_eq!(state.menu().selected, 4);

        state.set_raw_for_test(input, 0, Some(7), 4);
        state.move_word_right();
        assert_eq!(state.cursor(), beta);
        assert_eq!(state.preferred_column(), None);
        assert_eq!(state.menu().selected, 4);
    }

    #[test]
    fn delete_to_line_end_is_unicode_safe_and_preserves_the_newline() {
        let input = "alpha βeta\nnext";
        let cursor = "alpha ".len();
        let mut state = ComposerState::default();
        state.set_raw_for_test(input, cursor, Some(7), 4);

        state.delete_to_line_end();

        assert_eq!(state.text(), "alpha \nnext");
        assert_eq!(state.cursor(), cursor);
        assert_eq!(state.preferred_column(), None);
        assert_eq!(state.menu().selected, 0);
    }

    #[test]
    fn delete_to_line_end_no_ops_at_logical_and_buffer_end() {
        for (input, cursor) in [("one\ntwo", 3), ("one", 3), ("one\n\ntwo", 4)] {
            let mut state = ComposerState::default();
            state.set_raw_for_test(input, cursor, Some(7), 4);

            state.delete_to_line_end();

            assert_eq!(state.text(), input);
            assert_eq!(state.cursor(), cursor);
            assert_eq!(state.preferred_column(), Some(7));
            assert_eq!(state.menu().selected, 4);
        }
    }

    #[test]
    fn delete_current_line_removes_the_expected_adjacent_delimiter() {
        let cases = [
            ("one\ntwo\nthree", 1, "two\nthree", 0),
            ("one\ntwo\nthree", 5, "one\nthree", 4),
            ("one\ntwo\nthree", 9, "one\ntwo", 7),
            ("one", 1, "", 0),
            ("\none", 0, "one", 0),
            ("one\n\nthree", 4, "one\nthree", 4),
            ("one\n", 4, "one", 3),
            ("one\ntwo", 3, "two", 0),
            ("", 0, "", 0),
        ];

        for (input, cursor, expected, expected_cursor) in cases {
            let mut state = ComposerState::default();
            state.set_raw_for_test(input, cursor, Some(7), 4);

            state.delete_current_line();

            assert_eq!(state.text(), expected, "input {input:?} at {cursor}");
            assert_eq!(
                state.cursor(),
                expected_cursor,
                "input {input:?} at {cursor}"
            );
            assert_eq!(state.preferred_column(), None);
            assert_eq!(state.menu().selected, 0);
        }
    }

    #[test]
    fn logical_line_bounds_assign_a_newline_caret_to_the_preceding_line() {
        let input = "one\ntwo\n";
        assert_eq!(logical_line_bounds(input, 3), 0..3);
        assert_eq!(logical_line_bounds(input, 4), 4..7);
        assert_eq!(logical_line_bounds(input, input.len()), 8..8);
    }

    #[test]
    fn boundaries_treat_combining_sequences_as_one_edit_unit() {
        let input = "a\u{301}界";
        assert_eq!(next_boundary(input, 0), "a\u{301}".len());
        assert_eq!(previous_boundary(input, input.len()), "a\u{301}".len());
        assert_eq!(clamp_cursor(input, 1), 0);
    }
}
