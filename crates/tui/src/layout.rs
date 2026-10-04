//! Incremental layout cache for the conversation pane.
//!
//! Rendering used to rebuild every conversation line on every frame — a full
//! Markdown parse and syntect highlight of the whole transcript per keystroke
//! or streamed delta. History entries are immutable once committed except for
//! their tool-call lifecycle state, so this module caches each entry's
//! rendered lines and revalidates them per frame against a tiny fingerprint
//! instead of re-rendering. The in-flight streamed message is cached per
//! content block: streamed snapshots only ever append, so completed blocks
//! are reused and each delta re-renders just the block that grew.

pub(crate) mod prepare;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use rig_core::message::Message;

use crate::app::{
    ActiveSelection, EntryFolds, FoldState, HeaderTimings, HistoryEntry, Selection, SelectionScope,
    SpanRole, ToolCallStatus, TurnStartTarget,
};
use crate::chrome::style_selected_line;
use crate::layout::prepare::{
    ConversationBlockContext, FoldHeader, card_inset, fold_body_rows, render_compaction_divider,
    render_conversation_block, render_ensemble, render_error, render_plan_artifact,
    render_plan_handoff, truncate_line_to_width, wrapped_height,
};
use crate::presentation::{
    ConversationEntry, NativeHeader, PresentationBlock, PresentationBlockId, PresentationBlockKind,
    PresentationRole, TextFlavor, TranscriptAppearance,
};
use crate::viewport::RowRange;

type ItemRanges = Vec<(usize, RowRange)>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum EntrySelection {
    #[default]
    None,
    Message,
    Block(usize),
}

impl EntrySelection {
    pub(crate) fn includes(self, index: usize) -> bool {
        self == Self::Message || self == Self::Block(index)
    }

    fn single_item(self) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Message => Some(0),
            Self::Block(index) => Some(index),
        }
    }
}

/// Fingerprint of one visible semantic block. Every reducer mutation bumps
/// its revision, regardless of whether the source is native Zevria or ACP.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BlockFingerprint {
    id: PresentationBlockId,
    revision: u64,
    header_role: Option<PresentationRole>,
    header: Option<NativeHeader>,
    separator_before: bool,
    item_gap_before: bool,
    selected: bool,
    folded: bool,
    reasoning_heading: bool,
    appearance: TranscriptAppearance,
    inner_width: u16,
    prompt_group: Option<PresentationBlockId>,
}

/// Cached rendering of one semantic block. Role headers live with the first
/// block after a visible role transition, making the block independently
/// reusable even when diagnostics are inserted or removed around it.
struct PresentationBlockLayout {
    fingerprint: BlockFingerprint,
    width: u16,
    lines: Vec<Line<'static>>,
    assistant_header_line: Option<usize>,
    elapsed_seconds: Option<u64>,
    body_line_start: usize,
    fold_header: Option<FoldHeader>,
    height: usize,
    rows: RowRange,
    decoration: RowRange,
    role: Option<PresentationRole>,
}

impl PresentationBlockLayout {
    fn render(
        block: &crate::presentation::PresentationBlock,
        fingerprint: BlockFingerprint,
        width: u16,
    ) -> Self {
        let mut lines = Vec::new();
        let rows = render_conversation_block(
            block,
            &mut lines,
            ConversationBlockContext {
                width,
                header_role: fingerprint.header_role,
                header: fingerprint.header,
                separator_before: fingerprint.separator_before,
                item_gap_before: fingerprint.item_gap_before,
                selected: fingerprint.selected,
                folded: fingerprint.folded,
                reasoning_heading: fingerprint.reasoning_heading,
                appearance: fingerprint.appearance,
            },
        );
        let height = wrapped_height(&lines, width);
        Self {
            fingerprint,
            width,
            lines,
            assistant_header_line: rows.assistant_header_line,
            elapsed_seconds: None,
            body_line_start: rows.content_line_start,
            fold_header: rows.fold_header,
            height,
            rows: rows.body,
            decoration: rows.decoration,
            role: block.role,
        }
    }

    fn header_is_current(&self, timings: &HeaderTimings) -> bool {
        self.assistant_header_line.is_none()
            || self.elapsed_seconds == header_elapsed_seconds(self.fingerprint.header, timings)
    }

    /// Refresh only decoration, retaining Markdown/highlighting and folded body lines.
    fn refresh_header(&mut self, timings: &HeaderTimings) {
        let (Some(line), Some(header)) = (self.assistant_header_line, self.fingerprint.header)
        else {
            return;
        };
        let seconds = header_elapsed_seconds(Some(header), timings);
        if self.elapsed_seconds == seconds {
            return;
        }
        self.elapsed_seconds = seconds;
        self.lines[line] = prepare::native_assistant_header(header, seconds);
        let start = wrapped_height(&self.lines[..self.body_line_start], self.width);
        self.rows = RowRange::from_start_len(start, self.rows.len());
        self.height = self.rows.end();
        self.decoration = RowRange::new(self.decoration.start(), self.height);
    }
}

fn header_elapsed_seconds(header: Option<NativeHeader>, timings: &HeaderTimings) -> Option<u64> {
    header
        .and_then(|header| timings.elapsed(header))
        .map(|elapsed| elapsed.as_secs())
}

/// Physical decoration rows, measured alongside selection content. Roleless
/// diagnostics and outcome notices never inherit adjacent card membership.
pub(crate) struct DecorationRows {
    pub(crate) rows: RowRange,
    pub(crate) role: Option<PresentationRole>,
    pub(crate) card: bool,
}

/// Cached rendering of one history entry at a given width and selection.
#[derive(Default)]
pub(crate) struct EntryLayout {
    width: u16,
    pub(crate) span: Option<SpanRole>,
    /// Summary-only fingerprint; covered entries retain their real layouts.
    span_counts: Option<(usize, usize)>,
    entry_selection: EntrySelection,
    diagnostics_visible: bool,
    appearance: TranscriptAppearance,
    pub(crate) decorations: Vec<DecorationRows>,
    conversation_blocks: Option<Vec<PresentationBlockLayout>>,
    item_folds: Vec<bool>,
    message_folded: bool,
    /// The entry's rendered lines. Selection geometry is cached separately.
    pub(crate) lines: Vec<Line<'static>>,
    /// Wrapped height of `lines` at `width`. The inter-entry gap row that
    /// follows every entry in the pane is not included.
    pub(crate) height: usize,
    /// Every visible semantic item's original content index and entry-relative
    /// wrapped rows, independent of selection styling.
    pub(crate) items: ItemRanges,
    /// The selected content item's wrapped row range relative to the entry's
    /// first row, when this entry holds the selection.
    pub(crate) selection: Option<RowRange>,
}

impl EntryLayout {
    /// Painting, total height, and hit testing share the same trailing gap.
    pub(crate) fn extent(&self) -> usize {
        self.height.saturating_add(usize::from(self.height > 0))
    }

    #[allow(clippy::too_many_arguments)]
    fn matches(
        &self,
        entry: &HistoryEntry,
        entry_selection: EntrySelection,
        width: u16,
        diagnostics_visible: bool,
        previous_reasoning: bool,
        appearance: TranscriptAppearance,
        entry_folds: EntryFolds<'_>,
        timings: &HeaderTimings,
    ) -> bool {
        self.span.is_none()
            && self.appearance == appearance
            && self.width == width
            && self.entry_selection == entry_selection
            && self.message_folded == entry_folds.is_message_folded()
            && self.diagnostics_visible == diagnostics_visible
            && match entry {
                HistoryEntry::Conversation(entry) => self.conversation_matches(
                    entry,
                    entry_selection,
                    width,
                    diagnostics_visible,
                    previous_reasoning,
                    appearance,
                    entry_folds,
                    timings,
                ),
                // These entries are immutable once committed.
                HistoryEntry::PlanArtifact(_)
                | HistoryEntry::PlanHandoff(..)
                | HistoryEntry::CompactionDivider
                | HistoryEntry::Error(_) => {
                    self.conversation_blocks.is_none()
                        && self
                            .item_folds
                            .iter()
                            .copied()
                            .eq((0..entry.selectable_upper_bound())
                                .map(|index| entry_folds.is_item_folded(index)))
                }
                // Compact worker rows mutate outside the presentation model.
                HistoryEntry::Ensemble(_) => false,
            }
    }

    /// Refresh a real layout regardless of whether it is rendered or covered.
    /// Moving between the two stores preserves reusable semantic block layouts.
    #[allow(clippy::too_many_arguments)]
    fn ensure_current(
        slot: &mut Option<Self>,
        entry: &HistoryEntry,
        entry_selection: EntrySelection,
        width: u16,
        diagnostics_visible: bool,
        previous_reasoning: bool,
        appearance: TranscriptAppearance,
        entry_folds: EntryFolds<'_>,
        timings: &HeaderTimings,
    ) -> Option<usize> {
        if slot.as_ref().is_some_and(|cached| {
            cached.matches(
                entry,
                entry_selection,
                width,
                diagnostics_visible,
                previous_reasoning,
                appearance,
                entry_folds,
                timings,
            )
        }) {
            return None;
        }
        let reusable = slot
            .as_mut()
            .and_then(|cached| cached.conversation_blocks.take())
            .unwrap_or_default();
        let (built, rebuilt_blocks) = Self::build(
            entry,
            entry_selection,
            width,
            diagnostics_visible,
            reusable,
            previous_reasoning,
            appearance,
            entry_folds,
            timings,
        );
        *slot = Some(built);
        Some(rebuilt_blocks)
    }

    fn span_placeholder(role: SpanRole, width: u16) -> Self {
        Self {
            span: Some(role),
            width,
            ..Self::default()
        }
    }

    fn refresh_summary(&mut self, hidden_rows: usize, count: usize, selected: bool) -> bool {
        let selection = if selected {
            EntrySelection::Message
        } else {
            EntrySelection::None
        };
        if self.span_counts == Some((hidden_rows, count)) && self.entry_selection == selection {
            return false;
        }
        let muted = Style::default().fg(crate::theme::theme().text.muted);
        let plural = if count == 1 { "" } else { "s" };
        let line = Line::from(vec![
            Span::styled("▸ ", muted),
            Span::raw(format!("{count} earlier message{plural}")),
            Span::styled(format!(" · {hidden_rows} more rows"), muted),
        ]);
        // Unlike a body fold, a span always occupies exactly one row, even in
        // a very narrow pane or when it covers just one one-row entry.
        let mut line = Line::from(truncate_line_to_width(&line, usize::from(self.width)));
        if selected {
            style_selected_line(&mut line);
        }
        self.lines = vec![line];
        self.height = 1;
        self.entry_selection = selection;
        self.selection = selected.then_some(RowRange::new(0, 1));
        self.span_counts = Some((hidden_rows, count));
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn conversation_matches(
        &self,
        entry: &ConversationEntry,
        entry_selection: EntrySelection,
        width: u16,
        diagnostics_visible: bool,
        previous_reasoning: bool,
        appearance: TranscriptAppearance,
        entry_folds: EntryFolds<'_>,
        timings: &HeaderTimings,
    ) -> bool {
        let Some(cached) = &self.conversation_blocks else {
            return false;
        };
        let mut cached = cached.iter();
        for (_, _, expected) in conversation_fingerprints(
            entry,
            entry_selection,
            width,
            diagnostics_visible,
            previous_reasoning,
            appearance,
            entry_folds,
        ) {
            if !cached.next().is_some_and(|layout| {
                layout.width == width
                    && layout.fingerprint == expected
                    && layout.header_is_current(timings)
            }) {
                return false;
            }
        }
        cached.next().is_none()
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        entry: &HistoryEntry,
        entry_selection: EntrySelection,
        width: u16,
        diagnostics_visible: bool,
        reusable_blocks: Vec<PresentationBlockLayout>,
        previous_reasoning: bool,
        appearance: TranscriptAppearance,
        entry_folds: EntryFolds<'_>,
        timings: &HeaderTimings,
    ) -> (Self, usize) {
        let mut lines = Vec::new();
        let message_folded = entry_folds.is_message_folded();
        let mut rebuilt_blocks = 0;
        let (items, conversation_blocks) = match entry {
            HistoryEntry::Conversation(entry) => {
                let (blocks, items, rebuilt) = build_conversation_blocks(
                    entry,
                    width,
                    entry_selection,
                    diagnostics_visible,
                    reusable_blocks,
                    &mut lines,
                    previous_reasoning,
                    appearance,
                    entry_folds,
                    timings,
                );
                rebuilt_blocks = rebuilt;
                let items = if message_folded && let Some(first) = blocks.first() {
                    let prefix_len = first.body_line_start;
                    let card = appearance == TranscriptAppearance::Acp
                        && first.fingerprint.prompt_group.is_some();
                    fold_body_rows(
                        &mut lines,
                        prefix_len,
                        width,
                        card.then_some((first.fingerprint.inner_width, card_inset(width))),
                        Some(blocks.len()),
                        if blocks.len() == 1 {
                            first.fold_header.as_ref()
                        } else {
                            None
                        },
                    );
                    let rows = RowRange::from_start_len(
                        wrapped_height(&lines[..prefix_len], width),
                        wrapped_height(&lines[prefix_len..], width),
                    );
                    if entry_selection != EntrySelection::None {
                        for line in &mut lines[prefix_len..] {
                            style_selected_line(line);
                        }
                    }
                    items.into_iter().map(|(index, _)| (index, rows)).collect()
                } else {
                    items
                };
                (items, Some(blocks))
            }
            HistoryEntry::PlanArtifact(artifact) => (
                vec![(
                    0,
                    render_plan_artifact(
                        artifact,
                        &mut lines,
                        width,
                        entry_selection.single_item(),
                        message_folded || entry_folds.is_item_folded(0),
                    ),
                )],
                None,
            ),
            HistoryEntry::PlanHandoff(handoff, header) => (
                vec![(
                    0,
                    render_plan_handoff(
                        handoff,
                        *header,
                        &mut lines,
                        width,
                        entry_selection.single_item(),
                        message_folded || entry_folds.is_item_folded(0),
                    ),
                )],
                None,
            ),
            HistoryEntry::Ensemble(ensemble) => (
                render_ensemble(
                    ensemble,
                    &mut lines,
                    width,
                    entry_selection,
                    &entry_folds,
                    message_folded,
                )
                .into_iter()
                .enumerate()
                .collect(),
                None,
            ),
            HistoryEntry::CompactionDivider => {
                render_compaction_divider(&mut lines);
                (Vec::new(), None)
            }
            HistoryEntry::Error(error) => {
                render_error(
                    error,
                    &mut lines,
                    width,
                    message_folded || entry_folds.is_item_folded(0),
                );
                let start = if message_folded {
                    wrapped_height(&lines[..1], width)
                } else {
                    0
                };
                let rows = RowRange::new(start, wrapped_height(&lines, width));
                if entry_selection.includes(0) {
                    for line in &mut lines[usize::from(message_folded)..] {
                        style_selected_line(line);
                    }
                }
                (vec![(0, rows)], None)
            }
        };
        let height = wrapped_height(&lines, width);
        let mut offset = 0;
        let decorations = conversation_blocks
            .as_ref()
            .map_or_else(Vec::new, |blocks| {
                if message_folded {
                    return blocks
                        .first()
                        .map(|block| DecorationRows {
                            rows: RowRange::from_start_len(0, height),
                            role: block.role,
                            card: appearance == TranscriptAppearance::Acp
                                && block.fingerprint.prompt_group.is_some(),
                        })
                        .into_iter()
                        .collect();
                }
                blocks
                    .iter()
                    .map(|block| {
                        let decoration = DecorationRows {
                            rows: block.decoration.shifted(offset),
                            role: block.role,
                            card: appearance == TranscriptAppearance::Acp
                                && block.fingerprint.prompt_group.is_some(),
                        };
                        offset += block.height;
                        decoration
                    })
                    .collect()
            });
        let selection = match entry_selection {
            EntrySelection::None => None,
            EntrySelection::Message => items
                .first()
                .zip(items.last())
                .map(|((_, first), (_, last))| RowRange::new(first.start(), last.end())),
            EntrySelection::Block(selected) => items
                .iter()
                .find_map(|&(index, rows)| (index == selected).then_some(rows)),
        };
        (
            Self {
                width,
                span: None,
                span_counts: None,
                entry_selection,
                diagnostics_visible,
                appearance,
                decorations,
                message_folded,
                item_folds: if conversation_blocks.is_some() {
                    Vec::new()
                } else {
                    (0..entry.selectable_upper_bound())
                        .map(|index| entry_folds.is_item_folded(index))
                        .collect()
                },
                conversation_blocks,
                lines,
                height,
                items,
                selection,
            },
            rebuilt_blocks,
        )
    }
}

fn block_header(
    block: &PresentationBlock,
    current_role: Option<PresentationRole>,
    appearance: TranscriptAppearance,
) -> (Option<PresentationRole>, bool) {
    if appearance == TranscriptAppearance::Acp && block.prompt_group.is_some() {
        return (
            block.prompt.as_ref().and(block.role),
            block.prompt.is_some() && current_role.is_some(),
        );
    }
    (
        block.role.filter(|role| current_role != Some(*role)),
        block
            .role
            .is_some_and(|role| current_role.is_some() && current_role != Some(role)),
    )
}

fn native_header(
    entry: &ConversationEntry,
    header_role: Option<PresentationRole>,
    appearance: TranscriptAppearance,
) -> Option<NativeHeader> {
    entry.header.filter(|header| {
        appearance == TranscriptAppearance::Native
            && matches!(
                (header, header_role),
                (NativeHeader::Prompt(_), Some(PresentationRole::User))
                    | (
                        NativeHeader::Assistant { .. },
                        Some(PresentationRole::Assistant)
                    )
            )
    })
}

fn effective_width(block: &PresentationBlock, appearance: TranscriptAppearance, width: u16) -> u16 {
    if appearance == TranscriptAppearance::Acp && block.prompt_group.is_some() {
        width.saturating_sub(card_inset(width) * 2).max(1)
    } else {
        width
    }
}

fn conversation_fingerprints<'a>(
    entry: &'a ConversationEntry,
    selection: EntrySelection,
    width: u16,
    diagnostics_visible: bool,
    mut previous_reasoning: bool,
    appearance: TranscriptAppearance,
    entry_folds: EntryFolds<'a>,
) -> impl Iterator<Item = (usize, &'a PresentationBlock, BlockFingerprint)> + 'a {
    let mut current_role = None;
    let mut previous_item = None;
    entry
        .blocks
        .iter()
        .enumerate()
        .filter(move |(_, block)| block.visible(diagnostics_visible))
        .map(move |(index, block)| {
            // Derived child rows share their top-level tool's item identity,
            // including when the tool header itself is hidden.
            let item = match block.kind {
                PresentationBlockKind::Subtask { parent, .. } => parent,
                _ => block.id,
            };
            let item_gap_before = previous_item.is_some_and(|previous| previous != item);
            previous_item = Some(item);
            let (header_role, separator_before) = block_header(block, current_role, appearance);
            if let Some(role) = block.role {
                current_role = Some(role);
            }
            let reasoning = matches!(block.kind, PresentationBlockKind::Reasoning { .. });
            let fingerprint = BlockFingerprint {
                id: block.id,
                revision: block.revision,
                header_role,
                header: native_header(entry, header_role, appearance),
                separator_before,
                item_gap_before,
                selected: !entry_folds.is_message_folded() && selection.includes(index),
                folded: entry_folds.is_block_folded(block.id),
                reasoning_heading: reasoning && !previous_reasoning,
                appearance,
                inner_width: effective_width(block, appearance, width),
                prompt_group: block.prompt_group,
            };
            previous_reasoning = reasoning;
            (index, block, fingerprint)
        })
}

#[allow(clippy::too_many_arguments)]
fn build_conversation_blocks(
    entry: &ConversationEntry,
    width: u16,
    entry_selection: EntrySelection,
    diagnostics_visible: bool,
    reusable_blocks: Vec<PresentationBlockLayout>,
    lines: &mut Vec<Line<'static>>,
    previous_reasoning: bool,
    appearance: TranscriptAppearance,
    entry_folds: EntryFolds<'_>,
    timings: &HeaderTimings,
) -> (Vec<PresentationBlockLayout>, ItemRanges, usize) {
    let mut reusable = reusable_blocks
        .into_iter()
        .map(|layout| (layout.fingerprint.id, layout))
        .collect::<HashMap<_, _>>();
    let mut blocks = Vec::new();
    let mut prefix_height = 0_usize;
    let mut items = Vec::new();
    let mut rebuilt = 0;
    for (index, block, fingerprint) in conversation_fingerprints(
        entry,
        entry_selection,
        width,
        diagnostics_visible,
        previous_reasoning,
        appearance,
        entry_folds,
    ) {
        let mut layout = match reusable.remove(&block.id) {
            Some(layout) if layout.width == width && layout.fingerprint == fingerprint => layout,
            _ => {
                rebuilt += 1;
                PresentationBlockLayout::render(block, fingerprint, width)
            }
        };
        layout.refresh_header(timings);
        items.push((index, layout.rows.shifted(prefix_height)));
        prefix_height = prefix_height.saturating_add(layout.height);
        lines.extend(layout.lines.iter().cloned());
        blocks.push(layout);
    }
    (blocks, items, rebuilt)
}

/// Cached rendering of the in-flight message or render-only pending header.
struct StreamingLayout {
    width: u16,
    header: Option<NativeHeader>,
    elapsed_seconds: Option<u64>,
    blocks: Vec<PresentationBlockLayout>,
    lines: Vec<Line<'static>>,
    height: usize,
}

/// Per-frame validated cache of the conversation pane's wrapped lines.
#[derive(Default)]
pub(crate) struct ConversationCache {
    entries: Vec<EntryLayout>,
    /// Real layouts under summaries/placeholders, including the representative.
    covered: HashMap<usize, EntryLayout>,
    streaming: Option<StreamingLayout>,
    tail_reasoning: bool,
    appearance: TranscriptAppearance,
    header_timings: HeaderTimings,
    /// Entry rebuilds since startup, so tests can assert cache hits.
    #[cfg(test)]
    pub(crate) rebuilds: usize,
    /// Semantic block rebuilds since startup. ACP streaming should change
    /// only the active block, not reparse the whole prompt cycle.
    #[cfg(test)]
    pub(crate) block_rebuilds: usize,
}

impl ConversationCache {
    pub(crate) fn set_appearance(&mut self, appearance: TranscriptAppearance) {
        self.appearance = appearance;
    }

    /// Install decoration state before refreshing committed and streaming layouts.
    pub(crate) fn set_header_timings(&mut self, timings: HeaderTimings) {
        self.header_timings = timings;
    }

    /// Drop cached committed entries at and after a semantic replacement
    /// boundary. Reducers call this immediately rather than storing a hidden
    /// one-frame invalidation marker on `App`.
    pub(crate) fn invalidate_from(&mut self, index: usize) {
        self.entries.truncate(index);
        self.covered.retain(|&history, _| history < index);
    }

    /// Bring the per-entry cache in line with `history`, re-rendering only
    /// entries whose width, selection, or tool-call state changed.
    pub(crate) fn refresh(
        &mut self,
        history: &[HistoryEntry],
        selected: Option<ActiveSelection>,
        width: u16,
        diagnostics_visible: bool,
        folds: &FoldState,
    ) {
        self.entries.truncate(history.len());
        self.covered.retain(|&index, _| index < history.len());
        let mut previous_reasoning = false;
        for (index, entry) in history.iter().enumerate() {
            let entry_folds = folds.entry(index);
            let role = entry_folds.span();
            let entry_selection = selected
                .filter(|active| role.is_none() && active.selection.history_index == index)
                .map_or(EntrySelection::None, |active| match active.scope {
                    SelectionScope::Message => EntrySelection::Message,
                    SelectionScope::Block => EntrySelection::Block(active.selection.content_index),
                });
            let entry_previous_reasoning = previous_reasoning;
            previous_reasoning = match entry {
                HistoryEntry::Conversation(entry) => entry
                    .blocks
                    .iter()
                    .rev()
                    .find(|block| block.visible(diagnostics_visible))
                    .map_or(previous_reasoning, |block| {
                        matches!(block.kind, PresentationBlockKind::Reasoning { .. })
                    }),
                _ => false,
            };
            let mut rendered = self.entries.get_mut(index).map(std::mem::take);
            let mut real = self.covered.remove(&index).or_else(|| {
                if rendered
                    .as_ref()
                    .is_some_and(|cached| cached.span.is_none())
                {
                    rendered.take()
                } else {
                    None
                }
            });
            let rebuilt = EntryLayout::ensure_current(
                &mut real,
                entry,
                entry_selection,
                width,
                diagnostics_visible,
                entry_previous_reasoning,
                self.appearance,
                entry_folds,
                &self.header_timings,
            );
            #[cfg(not(test))]
            let _ = rebuilt;
            #[cfg(test)]
            if let Some(blocks) = rebuilt {
                self.rebuilds += 1;
                self.block_rebuilds += blocks;
            }
            let real = real.expect("ensured layout");
            let layout = if let Some(role) = role {
                self.covered.insert(index, real);
                rendered
                    .filter(|cached| cached.span == Some(role) && cached.width == width)
                    .unwrap_or_else(|| EntryLayout::span_placeholder(role, width))
            } else {
                real
            };
            match self.entries.get_mut(index) {
                Some(slot) => *slot = layout,
                None => self.entries.push(layout),
            }
        }
        for (start, layout) in self.entries.iter_mut().enumerate() {
            let Some(SpanRole::Summary { end }) = layout.span else {
                continue;
            };
            let hidden_rows = (start..=end).map(|index| self.covered[&index].height).sum();
            let count = history[start..=end]
                .iter()
                .filter(|entry| entry.message_fold_eligible(diagnostics_visible))
                .count();
            let selected = selected
                .is_some_and(|active| (start..=end).contains(&active.selection.history_index));
            let rebuilt = layout.refresh_summary(hidden_rows, count, selected);
            #[cfg(not(test))]
            let _ = rebuilt;
            #[cfg(test)]
            if rebuilt {
                self.rebuilds += 1;
            }
            // Content indices can change even when the summary fingerprint
            // does not (for example, diagnostics replacing another block).
            layout.items.clear();
            layout.items.extend(
                self.covered[&start]
                    .items
                    .iter()
                    .map(|&(index, _)| (index, RowRange::new(0, 1))),
            );
        }
        self.tail_reasoning = previous_reasoning;
    }

    /// Ordered destinations validated against the same extents and decoration
    /// rows used for painting. Multiple prompts under one fold are one stop.
    pub(crate) fn turn_start_positions(
        &self,
        targets: &[TurnStartTarget],
    ) -> Vec<(TurnStartTarget, usize)> {
        let mut offset = 0usize;
        let offsets = self
            .entries
            .iter()
            .map(|entry| {
                let start = offset;
                offset = offset.saturating_add(entry.extent());
                start
            })
            .collect::<Vec<_>>();
        let mut positions = targets
            .iter()
            .filter_map(|&target| {
                let (history, within) = self.turn_start_location(target)?;
                Some((target, offsets[history].saturating_add(within)))
            })
            .collect::<Vec<_>>();
        positions.dedup_by_key(|(_, row)| *row);
        positions
    }

    pub(crate) fn turn_start_row(&self, target: TurnStartTarget) -> Option<usize> {
        let (history, within) = self.turn_start_location(target)?;
        let offset = self.entries[..history]
            .iter()
            .map(EntryLayout::extent)
            .sum::<usize>();
        Some(offset.saturating_add(within))
    }

    fn turn_start_location(&self, target: TurnStartTarget) -> Option<(usize, usize)> {
        let history = target.history_index;
        let entry = self.entries.get(history)?;
        let representative = match entry.span {
            Some(SpanRole::Summary { .. }) => history,
            Some(SpanRole::Hidden { start }) => start,
            None => history,
        };
        let real = self.covered.get(&history).unwrap_or(entry);
        if real.height == 0 {
            return None;
        }
        let mut within = 0usize;
        if let Some(id) = target.block {
            let blocks = real.conversation_blocks.as_ref()?;
            let block = blocks.iter().find(|block| block.fingerprint.id == id)?;
            if real.message_folded {
                within = blocks.first()?.decoration.start();
            } else {
                within = blocks
                    .iter()
                    .take_while(|block| block.fingerprint.id != id)
                    .map(|block| block.height)
                    .sum::<usize>()
                    .saturating_add(block.decoration.start());
            }
        }
        let visible = self.entries.get(representative)?;
        if visible.height == 0 {
            return None;
        }
        Some((
            representative,
            if entry.span.is_some() { 0 } else { within },
        ))
    }

    /// Containment lookup, not a search for the next available block: special
    /// entries and gaps must not acquire an anchor to later conversation content.
    pub(crate) fn semantic_anchor(
        &self,
        top: usize,
    ) -> Option<(usize, PresentationBlockId, usize)> {
        let mut offset = 0usize;
        for (history, entry) in self.entries.iter().enumerate() {
            if matches!(entry.span, Some(SpanRole::Summary { .. })) && top == offset {
                return self
                    .covered
                    .get(&history)?
                    .conversation_blocks
                    .as_ref()?
                    .first()
                    .map(|block| (history, block.fingerprint.id, 0));
            }
            let mut block_offset = offset;
            if let Some(blocks) = &entry.conversation_blocks {
                if entry.message_folded {
                    if offset <= top && top < offset.saturating_add(entry.height) {
                        return blocks
                            .first()
                            .map(|block| (history, block.fingerprint.id, top - offset));
                    }
                } else {
                    for block in blocks {
                        let block_end = block_offset.saturating_add(block.height);
                        if block_offset <= top && top < block_end {
                            return Some((
                                history,
                                block.fingerprint.id,
                                top.saturating_sub(block_offset),
                            ));
                        }
                        block_offset = block_end;
                    }
                }
            }
            offset = offset.saturating_add(entry.extent());
        }
        None
    }

    pub(crate) fn anchor_row(&self, anchor: (usize, PresentationBlockId, usize)) -> Option<usize> {
        let (history, id, within) = anchor;
        let entry = self.entries.get(history)?;
        let representative = match entry.span {
            Some(SpanRole::Summary { .. }) => Some(history),
            Some(SpanRole::Hidden { start }) => Some(start),
            None => None,
        };
        if let Some(start) = representative {
            return Some(
                self.entries
                    .iter()
                    .take(start)
                    .map(EntryLayout::extent)
                    .sum(),
            );
        }
        let mut offset = self
            .entries
            .iter()
            .take(history)
            .map(EntryLayout::extent)
            .sum::<usize>();
        let blocks = entry.conversation_blocks.as_ref()?;
        if entry.message_folded {
            return blocks
                .iter()
                .any(|block| block.fingerprint.id == id)
                .then(|| offset.saturating_add(within.min(entry.height.saturating_sub(1))));
        }
        for block in blocks {
            if block.fingerprint.id == id {
                return Some(offset.saturating_add(within.min(block.height.saturating_sub(1))));
            }
            offset = offset.saturating_add(block.height);
        }
        None
    }

    pub(crate) fn entries(&self) -> &[EntryLayout] {
        &self.entries
    }

    /// The bottommost committed semantic item with any content row in the
    /// rendered window. Gaps, hidden blocks, and the ephemeral tail are not
    /// targets; there is deliberately no off-screen fallback.
    pub(crate) fn selection_at_bottom(&self, window: RowRange) -> Option<Selection> {
        if window.is_empty() {
            return None;
        }
        let mut offset = 0_usize;
        let mut selected = None;
        for (history_index, entry) in self.entries.iter().enumerate() {
            if offset >= window.end() {
                break;
            }
            for &(content_index, rows) in &entry.items {
                let rows = rows.shifted(offset);
                if rows.start() >= window.end() {
                    break;
                }
                if !rows.is_empty() && rows.intersects(window) {
                    selected = Some(Selection {
                        history_index,
                        content_index,
                    });
                }
            }
            offset = offset.saturating_add(entry.extent());
        }
        selected
    }

    /// The in-flight message or header-only tail's lines and wrapped height.
    /// Neither tail contributes items to committed selection geometry.
    pub(crate) fn streaming(&self) -> Option<(&[Line<'static>], usize)> {
        self.streaming
            .as_ref()
            .map(|streaming| (streaming.lines.as_slice(), streaming.height))
    }

    /// Cache streamed content, or only its native header before readable content
    /// arrives. The caller omits the header when committed content already owns it.
    pub(crate) fn refresh_streaming(
        &mut self,
        streaming: Option<&Message>,
        header: Option<NativeHeader>,
        width: u16,
    ) {
        let elapsed_seconds = header_elapsed_seconds(header, &self.header_timings);
        let entry = streaming.and_then(|message| {
            HistoryEntry::from_message(message.clone(), ToolCallStatus::Finished)
        });
        let Some(HistoryEntry::Conversation(mut entry)) = entry else {
            let Some(header) = header else {
                self.streaming = None;
                return;
            };
            if self.streaming.as_ref().is_some_and(|cached| {
                cached.width == width
                    && cached.header == Some(header)
                    && cached.blocks.is_empty()
                    && cached.elapsed_seconds == elapsed_seconds
            }) {
                return;
            }
            let lines = vec![prepare::native_assistant_header(header, elapsed_seconds)];
            let height = wrapped_height(&lines, width);
            self.streaming = Some(StreamingLayout {
                width,
                header: Some(header),
                elapsed_seconds,
                blocks: Vec::new(),
                lines,
                height,
            });
            return;
        };
        entry.header = header;
        for block in &mut entry.blocks {
            block.revision = streaming_block_revision(block);
        }

        let cached = self.streaming.take().filter(|cached| cached.width == width);
        let reusable = cached.map_or_else(Vec::new, |cached| cached.blocks);
        let mut lines = Vec::new();
        let (blocks, _, rebuilt) = build_conversation_blocks(
            &entry,
            width,
            EntrySelection::None,
            false,
            reusable,
            &mut lines,
            self.tail_reasoning,
            TranscriptAppearance::Native,
            EntryFolds::none(),
            &self.header_timings,
        );
        #[cfg(test)]
        {
            self.block_rebuilds += rebuilt;
        }
        #[cfg(not(test))]
        let _ = rebuilt;
        let height = wrapped_height(&lines, width);
        self.streaming = Some(StreamingLayout {
            width,
            header,
            elapsed_seconds,
            blocks,
            lines,
            height,
        });
    }
}

/// Content hash used as the revision for ephemeral Rig snapshots. Committed
/// blocks get reducer-owned monotonic revisions; streams have no such state,
/// so hashing preserves the same cache contract without leaking Rig types
/// into rendering.
fn streaming_block_revision(block: &PresentationBlock) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let kind = match &block.kind {
        PresentationBlockKind::Text {
            flavor: TextFlavor::Plain,
            ..
        } => 0_u8,
        PresentationBlockKind::Text {
            flavor: TextFlavor::Markdown,
            ..
        } => 1,
        PresentationBlockKind::Reasoning { .. } => 2,
        PresentationBlockKind::Tool(_) => 3,
        PresentationBlockKind::Plan(_) => 4,
        PresentationBlockKind::Placeholder(_) => 5,
        PresentationBlockKind::Diagnostic(_) => 6,
        PresentationBlockKind::Error(_) => 7,
        PresentationBlockKind::Image { .. } => 8,
        PresentationBlockKind::WebActivity(_) => 9,
        PresentationBlockKind::Subtask { .. } => 10,
    };
    kind.hash(&mut hash);
    block.primary_copy().hash(&mut hash);
    block.secondary_copy().hash(&mut hash);
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{EnsembleHistory, TurnFold};
    use rig_core::message::AssistantContent;
    use zevria_foundation::TurnId;
    use zevria_workflow::EnsembleRunId;
    use zevria_workflow::EnsembleWorkflow;
    use zevria_workflow::PlanArtifact;
    use zevria_workflow::PlanHandoff;
    use zevria_workflow::PlanId;
    use zevria_workflow::PlanVersion;

    fn conversation_entry() -> HistoryEntry {
        HistoryEntry::from_message(
            Message::Assistant {
                id: None,
                content: vec![
                    AssistantContent::text("first block\nwith a wrapped line of text\nlast row"),
                    AssistantContent::text("second block\ninterior row\nlast row"),
                ],
            },
            ToolCallStatus::Finished,
        )
        .expect("visible conversation blocks")
    }

    fn non_conversation_entries() -> [HistoryEntry; 5] {
        let artifact = PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Approved plan".into(),
            markdown: "# Approved plan\n\nKeep the viewport stable.".into(),
            source_turn_id: TurnId::new(1),
        };
        [
            HistoryEntry::PlanArtifact(artifact.clone()),
            HistoryEntry::PlanHandoff(PlanHandoff::new(artifact, "source-session"), None),
            HistoryEntry::Ensemble(EnsembleHistory {
                header: None,
                run_id: EnsembleRunId::from_string("anchor-ensemble"),
                workflow: EnsembleWorkflow::Review,
                prompt: "Review the implementation.".into(),
                workers: Vec::new(),
                baseline: None,
            }),
            HistoryEntry::Error("A visible error.".into()),
            HistoryEntry::CompactionDivider,
        ]
    }

    #[test]
    fn elapsed_decoration_refreshes_wrapping_folds_and_covered_geometry_without_body_rebuilds() {
        use crate::app::{ConversationState, FoldKey};
        use std::time::{Duration, Instant};

        fn assert_geometry(actual: &EntryLayout, expected: &EntryLayout) {
            assert_eq!(actual.lines, expected.lines);
            assert_eq!(actual.height, expected.height);
            assert_eq!(actual.items, expected.items);
            assert_eq!(actual.selection, expected.selection);
            assert_eq!(actual.extent(), expected.extent());
            assert_eq!(actual.height, wrapped_height(&actual.lines, actual.width));
            let decorations = |entry: &EntryLayout| {
                entry
                    .decorations
                    .iter()
                    .map(|decoration| (decoration.rows, decoration.role, decoration.card))
                    .collect::<Vec<_>>()
            };
            assert_eq!(decorations(actual), decorations(expected));
        }

        let now = Instant::now();
        let mut conversation = ConversationState::default();
        let turn = conversation.allocate_turn();
        conversation.push_user_turn(Message::user("before"), turn);
        let header = conversation.allocate_call(turn);
        conversation.start_header(header, now);
        conversation.commit_response(
            Message::Assistant {
                id: None,
                content: vec![
                    AssistantContent::text("**cached Markdown**\n\n```rust\nfn main() {}\n```"),
                    AssistantContent::text("second block\n\nlast row"),
                ],
            },
            None,
            ToolCallStatus::Finished,
            Some(header),
        );
        let turn = conversation.allocate_turn();
        conversation.push_user_turn(Message::user("after"), turn);
        let targets = conversation.turn_starts(false);
        let HistoryEntry::Conversation(entry) = &conversation.history()[1] else {
            panic!()
        };
        let block_id = entry.blocks[0].id;
        let mut saw_growth = false;
        for width in [1, 8, 27, 30, 100] {
            for folding in ["none", "block", "message", "span", "span+message"] {
                for scope in [
                    None,
                    Some(SelectionScope::Block),
                    Some(SelectionScope::Message),
                ] {
                    let selected = scope.map(|scope| ActiveSelection {
                        selection: Selection {
                            history_index: 1,
                            content_index: 0,
                        },
                        scope,
                    });
                    let mut folds = FoldState::default();
                    if folding == "block" {
                        folds.fold(FoldKey::Block {
                            history_index: 1,
                            id: block_id,
                        });
                    }
                    if folding.contains("message") {
                        folds.fold(FoldKey::Message { history_index: 1 });
                    }
                    if folding.contains("span") {
                        folds.fold(FoldKey::Span { start: 0, end: 1 });
                    }
                    let mut cache = ConversationCache::default();
                    cache.set_header_timings(HeaderTimings::observe(
                        &conversation,
                        now + Duration::from_secs(9),
                    ));
                    cache.refresh(conversation.history(), selected, width, false, &folds);
                    let rebuilt = cache.block_rebuilds;
                    let real = cache.covered.get(&1).unwrap_or(&cache.entries[1]);
                    let bodies = real
                        .conversation_blocks
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|block| block.lines[block.body_line_start..].to_vec())
                        .collect::<Vec<_>>();
                    let first_header_height = real.items[0].1.start();
                    for seconds in [10, 59, 60, 3723] {
                        let at = now + Duration::from_secs(seconds);
                        cache.set_header_timings(HeaderTimings::observe(&conversation, at));
                        cache.refresh(conversation.history(), selected, width, false, &folds);
                        assert_eq!(cache.block_rebuilds, rebuilt);
                        let real = cache.covered.get(&1).unwrap_or(&cache.entries[1]);
                        assert_eq!(
                            real.conversation_blocks
                                .as_ref()
                                .unwrap()
                                .iter()
                                .map(|block| block.lines[block.body_line_start..].to_vec())
                                .collect::<Vec<_>>(),
                            bodies
                        );
                        let header_height = wrapped_height(&real.lines[..1], width);
                        assert_eq!(real.items[0].1.start(), header_height);
                        saw_growth |= header_height > first_header_height;
                        if !folding.contains("span") {
                            let offset = cache.entries[0].extent();
                            assert_eq!(
                                cache.selection_at_bottom(RowRange::from_start_len(
                                    offset,
                                    header_height
                                )),
                                None,
                                "header decoration is not selectable"
                            );
                            assert_eq!(
                                cache
                                    .selection_at_bottom(RowRange::from_start_len(
                                        offset + header_height,
                                        1
                                    ))
                                    .unwrap()
                                    .history_index,
                                1
                            );
                        }
                        // A cold layout is the geometry oracle, including message folds
                        // and retained layouts hidden under span summaries.
                        let mut fresh = ConversationCache::default();
                        fresh.set_header_timings(HeaderTimings::observe(&conversation, at));
                        fresh.refresh(conversation.history(), selected, width, false, &folds);
                        for (actual, expected) in cache.entries.iter().zip(&fresh.entries) {
                            assert_geometry(actual, expected);
                        }
                        for (index, actual) in &cache.covered {
                            assert_geometry(actual, &fresh.covered[index]);
                        }
                        assert_eq!(
                            cache.turn_start_positions(&targets),
                            fresh.turn_start_positions(&targets)
                        );
                        let rows = cache.entries.iter().map(EntryLayout::extent).sum::<usize>();
                        for row in 0..rows {
                            assert_eq!(cache.semantic_anchor(row), fresh.semantic_anchor(row));
                            assert_eq!(
                                cache.selection_at_bottom(RowRange::from_start_len(row, 1)),
                                fresh.selection_at_bottom(RowRange::from_start_len(row, 1))
                            );
                        }
                        let entry_rebuilds = cache.rebuilds;
                        cache.set_header_timings(HeaderTimings::observe(
                            &conversation,
                            at + Duration::from_millis(250),
                        ));
                        cache.refresh(conversation.history(), selected, width, false, &folds);
                        assert_eq!(
                            cache.rebuilds, entry_rebuilds,
                            "subsecond ticks leave the header cache intact"
                        );
                    }
                    if folding.contains("span") {
                        folds.unfold(FoldKey::Span { start: 0, end: 1 });
                        cache.refresh(conversation.history(), selected, width, false, &folds);
                        assert!(cache.covered.is_empty());
                        assert_eq!(
                            cache.entries[1].lines[0].to_string(),
                            "● Assistant · #(1 - 1) · 1h 02m 03s"
                        );
                        if selected.is_none() {
                            assert_eq!(
                                cache.block_rebuilds, rebuilt,
                                "uncovering reuses updated bodies"
                            );
                        }
                    }
                }
            }
        }
        assert!(saw_growth, "exercise wrapping across duration boundaries");
    }

    #[test]
    fn turn_positions_use_wrapped_header_geometry_and_deduplicate_folded_starts() {
        use crate::app::{ConversationState, FoldKey};
        use crate::presentation::{BlockVisibility, DisplayTurn};

        let mut conversation = ConversationState::default();
        let HistoryEntry::Conversation(mut mixed) = HistoryEntry::from_message(
            Message::Assistant {
                id: None,
                content: [
                    "prefix",
                    "diagnostic",
                    "prompt",
                    "more prompt",
                    "answer",
                    "diagnostic",
                    "image",
                ]
                .into_iter()
                .map(AssistantContent::text)
                .collect(),
            },
            ToolCallStatus::Finished,
        )
        .unwrap() else {
            panic!()
        };
        for index in [1, 5] {
            mixed.blocks[index].role = None;
            mixed.blocks[index].visibility = BlockVisibility::Diagnostics;
            mixed.blocks[index].kind =
                PresentationBlockKind::Diagnostic(crate::presentation::PresentedDiagnostic {
                    label: "diagnostic".into(),
                    text: "diagnostic\nwrapped rows".into(),
                    tone: crate::presentation::DiagnosticTone::Muted,
                });
        }
        for index in [2, 3, 6] {
            mixed.blocks[index].role = Some(PresentationRole::User);
        }
        mixed.blocks[6].kind = PresentationBlockKind::Image {
            image: zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
            ordinal: 1,
            editable: false,
        };
        conversation.push_entry(HistoryEntry::Conversation(mixed));
        let mut special = non_conversation_entries();
        let HistoryEntry::PlanHandoff(_, header) = &mut special[1] else {
            panic!()
        };
        *header = Some(NativeHeader::Prompt(DisplayTurn(2)));
        for entry in special {
            conversation.push_entry(entry);
        }
        conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
            header: Some(NativeHeader::Prompt(DisplayTurn(3))),
            blocks: Vec::new(),
        }));
        for width in [1, 6, 17, 80] {
            for diagnostics in [false, true] {
                let mut cache = ConversationCache::default();
                cache.set_appearance(TranscriptAppearance::Acp);
                let targets = conversation.turn_starts(diagnostics);
                assert_eq!(
                    targets.len(),
                    4,
                    "two grouped blocks, handoff, and zero-height prompt"
                );
                cache.refresh(
                    conversation.history(),
                    None,
                    width,
                    diagnostics,
                    &FoldState::default(),
                );
                let positions = cache.turn_start_positions(&targets);
                assert_eq!(
                    positions.len(),
                    3,
                    "hidden zero-height destination is omitted"
                );
                assert!(positions.windows(2).all(|pair| pair[0].1 < pair[1].1));
                let entry = &cache.entries()[0];
                for (target, row) in &positions[..2] {
                    let id = target.block.unwrap();
                    let mut prefix = 0;
                    for block in entry.conversation_blocks.as_ref().unwrap() {
                        if block.fingerprint.id == id {
                            assert_eq!(*row, prefix + block.decoration.start());
                            assert!(
                                *row < prefix + block.rows.start(),
                                "the header is before the selectable body"
                            );
                            assert!(block.decoration.start() > 0, "do not land on the separator");
                            break;
                        }
                        prefix += block.height;
                    }
                }
                assert_eq!(
                    positions[2].1,
                    cache.entries()[..2]
                        .iter()
                        .map(EntryLayout::extent)
                        .sum::<usize>()
                );
                let mut folds = FoldState::default();
                folds.fold(FoldKey::Message { history_index: 0 });
                cache.refresh(conversation.history(), None, width, diagnostics, &folds);
                assert_eq!(cache.turn_start_row(targets[0]), Some(0));
                assert_eq!(cache.turn_start_row(targets[1]), Some(0));
                assert_eq!(cache.turn_start_positions(&targets).len(), 2);
                folds.fold(FoldKey::Span { start: 0, end: 2 });
                cache.refresh(conversation.history(), None, width, diagnostics, &folds);
                assert_eq!(cache.turn_start_positions(&targets), vec![(targets[0], 0)]);
            }
        }
    }

    #[test]
    fn span_fold_cache_reuses_real_layouts_and_refreshes_covered_changes() {
        use crate::app::FoldKey;

        let mut history = vec![
            conversation_entry(),
            conversation_entry(),
            conversation_entry(),
        ];
        let mut cache = ConversationCache::default();
        let mut folds = FoldState::default();
        cache.refresh(&history, None, 80, false, &folds);
        let rows = cache.entries[..2].iter().map(|e| e.height).sum::<usize>();
        let rebuilt = cache.block_rebuilds;
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        cache.refresh(&history, None, 80, false, &folds);
        assert_eq!(cache.block_rebuilds, rebuilt);
        assert_eq!(cache.covered.len(), 2);
        assert_eq!(cache.entries[0].span_counts, Some((rows, 2)));
        assert_eq!(cache.entries[1].span, Some(SpanRole::Hidden { start: 0 }));
        assert_eq!(
            cache.selection_at_bottom(RowRange::new(0, 1)),
            Some(Selection {
                history_index: 0,
                content_index: 1
            })
        );
        assert_eq!(cache.selection_at_bottom(RowRange::new(1, 2)), None);
        let selected = Some(ActiveSelection {
            selection: Selection {
                history_index: 1,
                content_index: 1,
            },
            scope: SelectionScope::Block,
        });
        cache.refresh(&history, selected, 80, false, &folds);
        assert_eq!(cache.entries[0].selection, Some(RowRange::new(0, 1)));
        assert_eq!(cache.entries[1].selection, None);
        assert_eq!(
            cache.block_rebuilds, rebuilt,
            "selection styles only the summary"
        );
        let entries = cache.rebuilds;
        cache.refresh(&history, selected, 80, false, &folds);
        assert_eq!(cache.rebuilds, entries);
        folds.fold(FoldKey::Block {
            history_index: 1,
            id: PresentationBlockId(0),
        });
        cache.refresh(&history, selected, 80, false, &folds);
        assert_eq!(cache.block_rebuilds, rebuilt + 1);
        let inner_fold_rows = cache.entries[0].span_counts.unwrap().0;
        assert!(inner_fold_rows < rows);
        folds.fold(FoldKey::Message { history_index: 0 });
        cache.refresh(&history, selected, 80, false, &folds);
        assert!(cache.entries[0].span_counts.unwrap().0 < inner_fold_rows);
        let before = cache.entries[0].span_counts;
        let HistoryEntry::Conversation(entry) = &mut history[1] else {
            unreachable!()
        };
        let PresentationBlockKind::Text { text, .. } = &mut entry.blocks[1].kind else {
            unreachable!()
        };
        *text = "new wrapped content ".repeat(100);
        entry.blocks[1].touch();
        cache.refresh(&history, selected, 80, false, &folds);
        assert_ne!(cache.entries[0].span_counts, before);
        assert_eq!(
            cache.block_rebuilds,
            rebuilt + 2,
            "only the mutated covered block rebuilds"
        );

        // Invalidating a covered entry must discard its real layout too, not
        // restore the old immutable entry when the span is later expanded.
        history[1] = HistoryEntry::Error("replacement".into());
        cache.invalidate_from(1);
        assert!(!cache.covered.contains_key(&1));
        cache.refresh(&history, None, 80, false, &folds);
        assert!(cache.covered[&1].conversation_blocks.is_none());
        folds.unfold(FoldKey::Span { start: 0, end: 1 });
        cache.refresh(&history, None, 80, false, &folds);
        assert!(cache.covered.is_empty());
        assert!(
            cache.entries[1]
                .lines
                .iter()
                .any(|line| line.to_string().contains("replacement"))
        );
        assert!(
            cache.entries[0].message_folded,
            "inner Message intent survives"
        );
        cache.invalidate_from(0);
        assert!(cache.entries.is_empty() && cache.covered.is_empty());
    }

    #[test]
    fn span_fold_counts_only_eligible_messages_and_recounts_diagnostics() {
        let mut diagnostic = conversation_entry();
        let HistoryEntry::Conversation(entry) = &mut diagnostic else {
            unreachable!()
        };
        for block in &mut entry.blocks {
            block.visibility = crate::presentation::BlockVisibility::Diagnostics;
        }
        let history = [
            conversation_entry(),
            diagnostic,
            conversation_entry(),
            conversation_entry(),
        ];
        let mut cache = ConversationCache::default();
        let mut folds = FoldState::default();
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        for diagnostics in [false, true, false] {
            cache.refresh(&history, None, 80, diagnostics, &folds);
            let rows = (0..3).map(|i| cache.covered[&i].height).sum::<usize>();
            assert_eq!(
                cache.entries[0].span_counts,
                Some((rows, if diagnostics { 3 } else { 2 }))
            );
            assert_eq!(cache.covered[&1].height > 0, diagnostics);
            assert_eq!(cache.entries[1].extent(), 0);
            assert_eq!(folds.span_containing(0), Some((0, 2)));
        }
    }

    #[test]
    fn span_fold_singleton_stays_one_row_even_in_narrow_panes() {
        let mut entry = conversation_entry();
        let HistoryEntry::Conversation(conversation) = &mut entry else {
            unreachable!()
        };
        conversation.blocks.truncate(1);
        conversation.blocks[0].role = None;
        let PresentationBlockKind::Text { text, .. } = &mut conversation.blocks[0].kind else {
            unreachable!()
        };
        *text = "x".into();
        let history = [entry, conversation_entry()];
        let mut cache = ConversationCache::default();
        let mut folds = FoldState::default();
        folds.fold_turns(&history, false, TurnFold::FinalExpanded);
        for width in [80, 40, 12, 4, 2, 1, 80] {
            cache.refresh(&history, None, width, false, &folds);
            let entry = &cache.entries[0];
            assert_eq!(entry.height, 1);
            assert_eq!(wrapped_height(&entry.lines, width), 1);
            assert_eq!(entry.span, Some(SpanRole::Summary { end: 0 }));
            if width >= 40 {
                assert_eq!(
                    entry.lines[0].to_string(),
                    "▸ 1 earlier message · 1 more rows"
                );
            }
        }
    }

    #[test]
    fn fold_flag_invalidates_exactly_one_block_fingerprint() {
        let history = [conversation_entry()];
        let mut cache = ConversationCache::default();
        let mut folds = FoldState::default();
        cache.refresh(&history, None, 80, false, &folds);
        let fingerprints = cache.entries[0]
            .conversation_blocks
            .as_ref()
            .unwrap()
            .iter()
            .map(|block| block.fingerprint)
            .collect::<Vec<_>>();
        let rebuilt = cache.block_rebuilds;
        folds.fold(crate::app::FoldKey::Block {
            history_index: 0,
            id: fingerprints[1].id,
        });
        cache.refresh(&history, None, 80, false, &folds);
        assert_eq!(cache.block_rebuilds, rebuilt + 1);
        let blocks = cache.entries[0].conversation_blocks.as_ref().unwrap();
        assert!(blocks[0].fingerprint == fingerprints[0]);
        assert!(
            blocks[1].fingerprint
                == BlockFingerprint {
                    folded: true,
                    ..fingerprints[1]
                }
        );
        assert_eq!(blocks[1].rows.len(), 1);
        cache.refresh(&history, None, 80, false, &folds);
        assert_eq!(cache.block_rebuilds, rebuilt + 1);
        folds.unfold_all();
        cache.refresh(&history, None, 80, false, &folds);
        assert_eq!(cache.block_rebuilds, rebuilt + 2);
    }

    #[test]
    fn semantic_anchors_do_not_fall_forward_from_non_conversation_entries() {
        for special in non_conversation_entries() {
            let history = [special, conversation_entry()];
            let mut cache = ConversationCache::default();
            cache.refresh(&history, None, 24, false, &FoldState::default());
            let first = &cache.entries[0];
            assert!(first.conversation_blocks.is_none());
            assert!(first.height > 0);
            for top in 0..first.extent() {
                assert_eq!(
                    cache.semantic_anchor(top),
                    None,
                    "row {top} in {:?} must not anchor to the later conversation",
                    history[0]
                );
            }
            assert_eq!(
                cache.semantic_anchor(first.extent()).map(|anchor| anchor.0),
                Some(1)
            );
        }
    }

    #[test]
    fn semantic_anchors_exclude_gaps_and_uncommitted_rows() {
        let mut cache = ConversationCache::default();
        assert_eq!(cache.semantic_anchor(0), None);
        cache.refresh(
            &[conversation_entry(), conversation_entry()],
            None,
            24,
            false,
            &FoldState::default(),
        );
        let mut offset = 0;
        for entry in &cache.entries {
            assert_eq!(cache.semantic_anchor(offset + entry.height), None);
            offset += entry.extent();
        }

        cache.refresh_streaming(
            Some(&Message::assistant("ephemeral tail\n".repeat(8))),
            None,
            24,
        );
        let (_, tail_height) = cache.streaming().expect("streamed tail");
        assert!(tail_height > 0);
        for top in offset..=offset + tail_height {
            assert_eq!(cache.semantic_anchor(top), None);
        }
        assert_eq!(cache.semantic_anchor(usize::MAX), None);
    }

    #[test]
    fn semantic_anchors_preserve_block_boundaries_and_round_trip_mixed_history() {
        let mut history = vec![conversation_entry()];
        for special in non_conversation_entries() {
            history.push(special);
            history.push(conversation_entry());
        }
        history.insert(
            1,
            HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: vec![],
            }),
        );
        let mut hidden = conversation_entry();
        let HistoryEntry::Conversation(entry) = &mut hidden else {
            unreachable!();
        };
        for block in &mut entry.blocks {
            block.visibility = crate::presentation::BlockVisibility::Diagnostics;
        }
        history.insert(2, hidden);

        for width in [12, 80] {
            for diagnostics_visible in [false, true] {
                let mut cache = ConversationCache::default();
                cache.refresh(
                    &history,
                    None,
                    width,
                    diagnostics_visible,
                    &FoldState::default(),
                );
                let mut expected = Vec::new();
                for (index, layout) in cache.entries.iter().enumerate() {
                    if let HistoryEntry::Conversation(entry) = &history[index] {
                        let visible = entry
                            .blocks
                            .iter()
                            .filter(|block| block.visible(diagnostics_visible));
                        let blocks = layout.conversation_blocks.as_ref().expect("conversation");
                        assert_eq!(blocks.len(), visible.clone().count());
                        for (block, rendered) in visible.zip(blocks) {
                            assert!(rendered.height >= 3, "fixture includes interior rows");
                            let start = expected.len();
                            for within in [0, rendered.height / 2, rendered.height - 1] {
                                assert_eq!(
                                    cache.semantic_anchor(start + within),
                                    Some((index, block.id, within)),
                                    "block start, interior, and last row retain identity and offset"
                                );
                            }
                            expected.extend(
                                (0..rendered.height).map(|within| Some((index, block.id, within))),
                            );
                        }
                    } else {
                        expected.extend(std::iter::repeat_n(None, layout.height));
                    }
                    expected.extend(std::iter::repeat_n(None, layout.extent() - layout.height));
                }
                assert_eq!(
                    expected.len(),
                    cache.entries.iter().map(EntryLayout::extent).sum::<usize>()
                );
                for (top, anchor) in expected.into_iter().enumerate() {
                    assert_eq!(
                        cache.semantic_anchor(top),
                        anchor,
                        "row {top}, width {width}"
                    );
                    if let Some(anchor) = anchor {
                        assert_eq!(
                            cache.anchor_row(anchor),
                            Some(top),
                            "row {top}, width {width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn semantic_anchors_exclude_empty_and_saturated_block_ranges() {
        let mut cache = ConversationCache::default();
        cache.refresh(
            &[HistoryEntry::Error("prefix".into()), conversation_entry()],
            None,
            80,
            false,
            &FoldState::default(),
        );
        let start = cache.entries[0].extent();
        let entry = &mut cache.entries[1];
        let blocks = entry.conversation_blocks.as_mut().expect("conversation");
        // Exercise a zero-height block before a nonempty block at the same row.
        entry.height -= blocks[0].height;
        blocks[0].height = 0;
        let second_id = blocks[1].fingerprint.id;
        for top in 0..start {
            assert_eq!(cache.semantic_anchor(top), None);
        }
        assert_eq!(cache.semantic_anchor(start), Some((1, second_id, 0)));

        let entry = &mut cache.entries[1];
        entry.conversation_blocks.as_mut().unwrap()[1].height = 0;
        entry.height = 0;
        for top in 0..=start {
            assert_eq!(cache.semantic_anchor(top), None);
        }

        // Saturation makes the second block's [MAX, MAX) range empty even
        // though its stored height is nonzero.
        cache.invalidate_from(0);
        cache.refresh(
            &[HistoryEntry::Error("prefix".into()), conversation_entry()],
            None,
            80,
            false,
            &FoldState::default(),
        );
        cache.entries[0].height = usize::MAX - 2;
        let first_id = cache.entries[1].conversation_blocks.as_ref().unwrap()[0]
            .fingerprint
            .id;
        assert_eq!(cache.semantic_anchor(usize::MAX - 2), None);
        let anchor = cache
            .semantic_anchor(usize::MAX - 1)
            .expect("last representable row");
        assert_eq!(anchor, (1, first_id, 0));
        assert_eq!(cache.anchor_row(anchor), Some(usize::MAX - 1));
        assert_eq!(cache.semantic_anchor(usize::MAX), None);
    }

    #[test]
    fn empty_item_ranges_and_saturated_extents_are_safe() {
        let mut cache = ConversationCache::default();
        cache.refresh(
            &[HistoryEntry::Error("error".into())],
            None,
            80,
            false,
            &FoldState::default(),
        );
        cache.entries[0].items = vec![(0, RowRange::new(1, 1))];
        assert_eq!(cache.selection_at_bottom(RowRange::new(0, 2)), None);
        cache.entries[0].height = usize::MAX;
        assert_eq!(cache.entries[0].extent(), usize::MAX);
        cache.entries[0].items = vec![(0, RowRange::from_start_len(usize::MAX, 1))];
        assert_eq!(
            cache.selection_at_bottom(RowRange::new(0, usize::MAX)),
            None
        );
    }
}
