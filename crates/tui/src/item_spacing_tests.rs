//! Measured spacing between visible top-level items, not their internal rows.
use super::*;
use crate::app::{FoldKey, FoldState};
use crate::layout::prepare::{ConversationBlockContext, render_conversation_block, wrapped_height};
use crate::layout::{ConversationCache, EntryLayout};
use crate::presentation::{
    AcpToolPresentation, BlockVisibility, ChecklistItem, ChecklistStatus, ConversationEntry,
    PresentationBlock, PresentationBlockId, PresentedChecklist, PresentedPlan,
    PresentedPlanContent, PresentedTool, PromptAnnotation, TextFlavor, TranscriptAppearance,
    WebActivityPresentation,
};
use crate::status_icon::StatusIcon;
use crate::viewport::RowRange;
use selection_tests::plain_block;

fn conversation(blocks: Vec<PresentationBlock>) -> HistoryEntry {
    HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks,
    })
}

fn line_at_row(entry: &EntryLayout, width: u16, row: usize) -> &ratatui::text::Line<'static> {
    let mut offset = 0;
    for line in &entry.lines {
        offset += wrapped_height(std::slice::from_ref(line), width);
        if row < offset {
            return line;
        }
    }
    panic!("missing row {row}");
}

fn assert_gap(cache: &ConversationCache, width: u16, previous: RowRange, next: RowRange) {
    let gap = RowRange::new(previous.end(), next.start());
    assert_eq!(gap.len(), 1, "exactly one generated item gap");
    assert_eq!(
        cache.selection_at_bottom(gap),
        None,
        "gap is not a copy target"
    );
    let line = line_at_row(&cache.entries()[0], width, gap.start());
    assert!(line.spans.is_empty(), "gap is an unstyled layout row");
    assert_ne!(line.style.bg, Some(SELECTION_BG));
}

#[test]
fn top_level_items_have_one_measured_gap_in_native_and_acp_layouts() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let messages = [
        assistant_message(vec![
            AssistantContent::text("first"),
            AssistantContent::text("second"),
        ]),
        assistant_message(vec![
            AssistantContent::text("first"),
            AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Summary("thought".into())],
            }),
            tool_call("spacing-tool", None, "command", json!({"command":"pwd"})),
        ]),
        Message::User {
            content: vec![
                UserContent::text("a user text item that wraps at narrow widths"),
                image.to_user_content(),
                UserContent::Document(Document {
                    data: DocumentSourceKind::String("payload".into()),
                    media_type: None,
                    additional_params: None,
                }),
            ],
        },
        assistant_message(vec![AssistantContent::text(""), AssistantContent::text("")]),
    ];
    for appearance in [TranscriptAppearance::Native, TranscriptAppearance::Acp] {
        for width in [8, 40, 80] {
            for message in &messages {
                let mut history = [history_message(message.clone())];
                if appearance == TranscriptAppearance::Acp
                    && let Message::User { .. } = message
                    && let HistoryEntry::Conversation(entry) = &mut history[0]
                {
                    let group = entry.blocks[0].id;
                    for block in &mut entry.blocks {
                        block.prompt_group = Some(group);
                    }
                    entry.blocks[0].prompt = Some(PromptAnnotation::default());
                }
                let mut cache = ConversationCache::default();
                cache.set_appearance(appearance);
                cache.refresh(&history, None, width, false, &FoldState::default());
                let items = cache.entries()[0].items.clone();
                assert_eq!(
                    items.len(),
                    if matches!(message, Message::Assistant { content, .. } if content.len() == 2) {
                        2
                    } else {
                        3
                    }
                );
                assert!(
                    !line_text(&cache.entries()[0].lines[0]).trim().is_empty(),
                    "no leading gap"
                );
                assert_eq!(
                    items.last().unwrap().1.end(),
                    cache.entries()[0].height,
                    "no trailing gap"
                );
                assert_eq!(
                    cache.entries()[0].height,
                    wrapped_height(&cache.entries()[0].lines, width)
                );
                for pair in items.windows(2) {
                    assert_gap(&cache, width, pair[0].1, pair[1].1);
                }
                for &(index, body) in &items {
                    cache.refresh(
                        &history,
                        cursor(0, index).map(|selection| ActiveSelection {
                            selection,
                            scope: SelectionScope::Block,
                        }),
                        width,
                        false,
                        &FoldState::default(),
                    );
                    assert_eq!(cache.entries()[0].selection, Some(body));
                    assert_eq!(cache.entries()[0].items, items);
                    for pair in items.windows(2) {
                        assert_gap(&cache, width, pair[0].1, pair[1].1);
                    }
                }
            }
        }
    }
}

#[test]
fn item_spacing_preserves_authored_whitespace_and_grouped_internals() {
    let mut blocks = vec![plain_block(
        0,
        PresentationRole::Assistant,
        "first\n\nlast\n\n",
    )];
    let kinds = [
        PresentationBlockKind::Text {
            text: "\nsecond".into(),
            flavor: TextFlavor::Plain,
            editable: false,
        },
        PresentationBlockKind::Text {
            text: "- first bullet\n- second bullet\n\n```text\ncode\n```".into(),
            flavor: TextFlavor::Markdown,
            editable: false,
        },
        PresentationBlockKind::Reasoning {
            parts: vec!["first thought\n\nlast thought".into(), "next part".into()],
        },
        PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(AcpToolPresentation {
            id: "tool".into(),
            title: "output".into(),
            kind: "execute".into(),
            status: "completed".into(),
            content: vec!["first output\n\nlast output".into()],
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
            metadata: None,
        }))),
        PresentationBlockKind::Plan(PresentedPlan {
            plan_id: None,
            content: PresentedPlanContent::Checklist(PresentedChecklist {
                label: "checklist".into(),
                items: vec![
                    ChecklistItem {
                        text: "first task".into(),
                        priority: None,
                        status: ChecklistStatus::Completed,
                    },
                    ChecklistItem {
                        text: "second task".into(),
                        priority: None,
                        status: ChecklistStatus::Pending,
                    },
                ],
            }),
        }),
        PresentationBlockKind::WebActivity(WebActivityPresentation {
            detail: Some("first web action\nsecond web action".into()),
            members: vec![0, 1],
            outcomes: vec![(StatusIcon::Done, 2)],
        }),
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        let mut block = plain_block((index + 1) as u64, PresentationRole::Assistant, "");
        block.kind = kind;
        blocks.push(block);
    }
    let history = [conversation(blocks.clone())];
    let mut cache = ConversationCache::default();
    cache.refresh(&history, None, 80, false, &FoldState::default());
    for (block, &(_, body)) in blocks.iter().zip(&cache.entries()[0].items) {
        let mut standalone = Vec::new();
        render_conversation_block(
            block,
            &mut standalone,
            ConversationBlockContext {
                width: 80,
                header_role: None,
                header: None,
                separator_before: false,
                item_gap_before: false,
                selected: false,
                folded: false,
                reasoning_heading: matches!(block.kind, PresentationBlockKind::Reasoning { .. }),
                appearance: TranscriptAppearance::Native,
            },
        );
        assert_eq!(
            &cache.entries()[0].lines[body.start()..body.end()],
            standalone.as_slice(),
            "only the boundary changes"
        );
    }
    let HistoryEntry::Conversation(entry) = &history[0] else {
        unreachable!()
    };
    for (before, after) in blocks.iter().zip(&entry.blocks) {
        assert_eq!(before.primary_copy(), after.primary_copy());
        assert_eq!(before.secondary_copy(), after.secondary_copy());
    }
    for pair in cache.entries()[0].items.windows(2) {
        assert_gap(&cache, 80, pair[0].1, pair[1].1);
    }
}

#[test]
fn invisible_items_never_create_leading_trailing_or_phantom_gaps() {
    let mut diagnostic = plain_block(0, PresentationRole::Assistant, "diagnostic");
    diagnostic.visibility = BlockVisibility::Diagnostics;
    let mut reasoning = plain_block(1, PresentationRole::Assistant, "");
    reasoning.kind = PresentationBlockKind::Reasoning {
        parts: vec![" \n".into()],
    };
    let HistoryEntry::Conversation(launch) = HistoryEntry::from_message(
        launch_assistant("hidden-launch", "child", "private"),
        ToolCallStatus::Executing,
    )
    .unwrap() else {
        unreachable!()
    };
    let mut hidden_tool = launch.blocks[0].clone();
    hidden_tool.id = PresentationBlockId(2);
    let mut trailing = vec![reasoning.clone(), diagnostic.clone(), hidden_tool.clone()];
    for (index, block) in trailing.iter_mut().enumerate() {
        block.id = PresentationBlockId((index + 4) as u64);
    }
    for visible in [
        Vec::new(),
        vec![plain_block(3, PresentationRole::Assistant, "body")],
    ] {
        let history = [conversation(
            [
                vec![diagnostic.clone(), reasoning.clone(), hidden_tool.clone()],
                visible.clone(),
                trailing.clone(),
            ]
            .concat(),
        )];
        let mut cache = ConversationCache::default();
        for diagnostics in [false, true, false] {
            cache.refresh(&history, None, 80, diagnostics, &FoldState::default());
            let count = visible.len() + 2 * usize::from(diagnostics);
            let entry = &cache.entries()[0];
            assert_eq!(entry.height, count * 2);
            assert_eq!(entry.items.len(), count);
            if count == 0 {
                assert!(entry.lines.is_empty());
            } else {
                assert_eq!(entry.items[0].1, RowRange::new(1, 2));
                assert_eq!(entry.items.last().unwrap().1.end(), entry.height);
                for pair in entry.items.windows(2) {
                    assert_gap(&cache, 80, pair[0].1, pair[1].1);
                }
            }
        }
    }
}

fn child(id: u64, parent: u64, index: usize) -> PresentationBlock {
    let mut block = plain_block(id, PresentationRole::Assistant, "");
    block.kind = PresentationBlockKind::Subtask {
        parent: PresentationBlockId(parent),
        entry_index: index,
        descriptor: child_descriptor(&format!("child-{index}"), &format!("child {index}")),
    };
    block
}

#[test]
fn tool_notices_and_children_are_one_item_group_even_when_the_header_is_hidden() {
    for width in [12, 80] {
        for surrounding in [false, true] {
            let HistoryEntry::Conversation(mut launch) = HistoryEntry::from_message(
                assistant_message(vec![tool_call(
                    "launch",
                    None,
                    "launch_subtasks",
                    json!({"tasks":[{}, {}, {}]}),
                )]),
                ToolCallStatus::Executing,
            )
            .unwrap() else {
                unreachable!()
            };
            let mut parent = launch.blocks.remove(0);
            parent.id = PresentationBlockId(1);
            let (_, state) = parent.native_tool_mut().unwrap();
            state
                .subtasks
                .insert(0, child_descriptor("child-0", "child 0"));
            state
                .subtasks
                .insert(1, child_descriptor("child-1", "child 1"));
            let mut blocks = vec![parent, child(2, 1, 0), child(3, 1, 1)];
            if surrounding {
                blocks.insert(0, plain_block(0, PresentationRole::Assistant, "before"));
                blocks.push(plain_block(4, PresentationRole::Assistant, "after"));
            }
            let mut history = [conversation(blocks)];
            let mut cache = ConversationCache::default();
            for notice in [false, true, false] {
                let HistoryEntry::Conversation(entry) = &mut history[0] else {
                    unreachable!()
                };
                let parent = entry
                    .blocks
                    .iter_mut()
                    .find(|block| block.id == PresentationBlockId(1))
                    .unwrap();
                parent.native_tool_mut().unwrap().1.status = if notice {
                    ToolCallStatus::Finished
                } else {
                    ToolCallStatus::Executing
                };
                parent.touch();
                let rebuilt = cache.block_rebuilds;
                cache.refresh(&history, None, width, false, &FoldState::default());
                if rebuilt > 0 {
                    assert_eq!(
                        cache.block_rebuilds - rebuilt,
                        if notice { 2 } else { 1 },
                        "only the notice and first child boundary change"
                    );
                }
                let items = &cache.entries()[0].items;
                assert_eq!(
                    items.len(),
                    2 + usize::from(notice) + 2 * usize::from(surrounding)
                );
                let group_start = usize::from(surrounding);
                let group_end = items.len() - usize::from(surrounding);
                for pair in items[group_start..group_end].windows(2) {
                    assert_eq!(
                        pair[0].1.end(),
                        pair[1].1.start(),
                        "no gaps inside a tool group"
                    );
                }
                if surrounding {
                    assert_gap(&cache, width, items[0].1, items[1].1);
                    assert_gap(&cache, width, items[group_end - 1].1, items[group_end].1);
                }
                assert!(!line_text(&cache.entries()[0].lines[0]).is_empty());
                assert_eq!(items.last().unwrap().1.end(), cache.entries()[0].height);
            }
        }
    }
}

#[test]
fn diagnostic_visibility_invalidates_a_changed_neighboring_item_gap() {
    let mut diagnostic = plain_block(1, PresentationRole::Assistant, "diagnostic");
    diagnostic.visibility = BlockVisibility::Diagnostics;
    let history = [conversation(vec![
        child(0, 99, 0),
        diagnostic,
        child(2, 99, 1),
    ])];
    let mut cache = ConversationCache::default();
    for diagnostics in [false, true, false] {
        let rebuilt = cache.block_rebuilds;
        cache.refresh(&history, None, 80, diagnostics, &FoldState::default());
        let items = &cache.entries()[0].items;
        if diagnostics {
            assert_eq!(
                cache.block_rebuilds - rebuilt,
                2,
                "diagnostic plus the changed child gap"
            );
            assert_gap(&cache, 80, items[0].1, items[1].1);
            assert_gap(&cache, 80, items[1].1, items[2].1);
        } else {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].1.end(), items[1].1.start());
            if rebuilt > 0 {
                assert_eq!(
                    cache.block_rebuilds - rebuilt,
                    1,
                    "the second child loses its gap"
                );
            }
        }
        assert_eq!(items.last().unwrap().1.end(), cache.entries()[0].height);
    }
}

#[test]
fn role_and_prompt_separators_replace_rather_than_duplicate_item_gaps() {
    for appearance in [TranscriptAppearance::Native, TranscriptAppearance::Acp] {
        for width in [12, 80] {
            for prompt_boundary in [false, true] {
                if prompt_boundary && appearance == TranscriptAppearance::Native {
                    continue;
                }
                let mut first = plain_block(0, PresentationRole::User, "first");
                let mut second = plain_block(
                    1,
                    if prompt_boundary {
                        PresentationRole::User
                    } else {
                        PresentationRole::Assistant
                    },
                    "second",
                );
                if prompt_boundary {
                    for block in [&mut first, &mut second] {
                        block.prompt_group = Some(block.id);
                        block.prompt = Some(PromptAnnotation::default());
                    }
                }
                let history = [conversation(vec![first, second])];
                let mut cache = ConversationCache::default();
                cache.set_appearance(appearance);
                cache.refresh(&history, None, width, false, &FoldState::default());
                let entry = &cache.entries()[0];
                assert_eq!(entry.height, 5, "header/body, separator, header/body");
                assert_eq!(
                    entry.items,
                    [(0, RowRange::new(1, 2)), (1, RowRange::new(4, 5))]
                );
                assert_eq!(
                    entry.decorations[1].rows.start(),
                    3,
                    "separator stays outside decoration"
                );
                assert_eq!(cache.selection_at_bottom(RowRange::new(2, 4)), None);
                let boundary = line_text(&entry.lines[2]);
                if appearance == TranscriptAppearance::Native {
                    assert!(boundary.contains('─'));
                } else {
                    assert!(boundary.is_empty());
                }
                assert!(line_text(&entry.lines[3]).contains('●'));
            }
        }
    }
}

#[test]
fn item_folds_keep_gaps_outside_bodies_and_message_folds_measure_them() {
    let history = [conversation(vec![
        plain_block(0, PresentationRole::Assistant, "before"),
        plain_block(1, PresentationRole::Assistant, "one\ntwo\nthree"),
        plain_block(2, PresentationRole::Assistant, "after"),
    ])];
    for width in [12, 80] {
        let mut cache = ConversationCache::default();
        let mut folds = FoldState::default();
        cache.refresh(&history, None, width, false, &folds);
        assert_eq!(cache.entries()[0].height, 8);
        folds.fold(FoldKey::Block {
            history_index: 0,
            id: PresentationBlockId(1),
        });
        cache.refresh(&history, None, width, false, &folds);
        let entry = &cache.entries()[0];
        assert_eq!(entry.height, 6);
        assert_eq!(entry.items[1].1.len(), 1);
        for pair in entry.items.windows(2) {
            assert_gap(&cache, width, pair[0].1, pair[1].1);
        }
        if width == 80 {
            assert!(
                line_text(&entry.lines[3]).contains("2 more rows"),
                "item summary excludes its gap"
            );
        }
        folds.unfold_all();
        folds.fold(FoldKey::Message { history_index: 0 });
        cache.refresh(&history, None, width, false, &folds);
        assert_eq!(cache.entries()[0].height, 2);
        if width == 80 {
            assert!(
                line_text(&cache.entries()[0].lines[1]).contains("3 blocks · 6 more rows"),
                "message summary counts measured gaps"
            );
        }
    }
}

#[test]
fn streaming_item_growth_reuses_completed_blocks_and_matches_committed_spacing() {
    for width in [12, 80] {
        let mut cache = ConversationCache::default();
        let mut content = vec![AssistantContent::text("complete")];
        let reasoning = mixed_reasoning_message();
        let Message::Assistant {
            content: thoughts, ..
        } = reasoning
        else {
            unreachable!()
        };
        for next in [
            None,
            Some(thoughts[0].clone()),
            Some(AssistantContent::text("tail")),
        ] {
            if let Some(next) = next {
                content.push(next);
            }
            let message = assistant_message(content.clone());
            let rebuilt = cache.block_rebuilds;
            cache.refresh_streaming(Some(&message), None, width);
            assert_eq!(
                cache.block_rebuilds - rebuilt,
                1,
                "only the appended block renders"
            );
        }
        *content.last_mut().unwrap() =
            AssistantContent::text("tail grows over multiple wrapped rows");
        let message = assistant_message(content);
        let rebuilt = cache.block_rebuilds;
        cache.refresh_streaming(Some(&message), None, width);
        assert_eq!(
            cache.block_rebuilds - rebuilt,
            1,
            "completed items retain their measured gaps"
        );
        cache.refresh_streaming(Some(&message), None, width);
        assert_eq!(
            cache.block_rebuilds - rebuilt,
            1,
            "unchanged snapshot is a cache hit"
        );
        let mut committed = ConversationCache::default();
        committed.refresh(
            &[history_message(message)],
            None,
            width,
            false,
            &FoldState::default(),
        );
        let (lines, height) = cache.streaming().unwrap();
        assert_eq!(lines, committed.entries()[0].lines);
        assert_eq!(height, committed.entries()[0].height);
        for pair in committed.entries()[0].items.windows(2) {
            assert_gap(&committed, width, pair[0].1, pair[1].1);
        }
    }
}

#[test]
fn gap_rows_keep_user_and_acp_card_surfaces_without_item_selection() {
    for appearance in [TranscriptAppearance::Native, TranscriptAppearance::Acp] {
        for width in [24, 80] {
            let mut blocks = vec![
                plain_block(0, PresentationRole::User, "first"),
                plain_block(
                    1,
                    PresentationRole::User,
                    "chosen text that wraps across several rows at narrow widths",
                ),
                plain_block(2, PresentationRole::User, "[attachment]"),
            ];
            if appearance == TranscriptAppearance::Acp {
                for block in &mut blocks {
                    block.prompt_group = Some(PresentationBlockId(0));
                }
                blocks[0].prompt = Some(PromptAnnotation::default());
            }
            let mut app = if appearance == TranscriptAppearance::Native {
                App::subtask_inspect("item gaps")
            } else {
                App::acp_inspect("item gaps")
            };
            app.seed_history_entry(conversation(blocks));
            app.select_for_test(cursor(0, 1));
            let buffer = rendered_buffer(&mut app, width, 40);
            let area = conversation_content_area(&buffer, true);
            let entry = &app.view_cache().entries()[0];
            assert_eq!(app.view_scroll(), 0);
            for pair in entry.items.windows(2) {
                let y = area.y + pair[0].1.end() as u16;
                assert_eq!(pair[1].1.start(), pair[0].1.end() + 1);
                assert_conversation_row_background(&buffer, area, y, ZEVRIA_DARK.surfaces.panel);
                assert!((area.x..area.right()).all(|x| buffer[(x, y)].symbol() == " "));
            }
            let selected = entry.selection.unwrap();
            let band = crate::frame_layout::FrameLayout::compute(
                buffer.area,
                true,
                crate::frame_layout::LowerSurface::None,
                false,
            )
            .selection_band;
            for row in selected.start()..selected.end() {
                let y = area.y + row as u16;
                assert!((band.x..band.right()).all(|x| buffer[(x, y)].bg == SELECTION_BG));
            }
            for row in 0..entry.height {
                let anchor = app
                    .view_cache()
                    .semantic_anchor(row)
                    .expect("measured row anchor");
                assert_eq!(app.view_cache().anchor_row(anchor), Some(row));
            }
            assert_eq!(
                app.view_cache().semantic_anchor(entry.height),
                None,
                "inter-message gap remains separate"
            );

            // A one-row scrolled pane showing only an item gap has no
            // selectable target and retains the message/card surface.
            let gap = entry.items[0].1.end();
            app.select_for_test(None);
            app.set_view_for_test(gap, false);
            let scrolled = rendered_buffer(&mut app, width, 1);
            let area = conversation_content_area(&scrolled, true);
            assert_eq!(area.height, 1);
            assert_eq!(app.view_scroll(), gap);
            assert_conversation_row_background(&scrolled, area, area.y, ZEVRIA_DARK.surfaces.panel);
            assert!((area.x..area.right()).all(|x| scrolled[(x, area.y)].symbol() == " "));
            assert_eq!(
                app.view_cache()
                    .selection_at_bottom(RowRange::from_start_len(gap, 1)),
                None,
            );
            double_escape(&mut app);
            assert_eq!(
                app.selection(),
                None,
                "a gap must not select its neighboring item"
            );
        }
    }
}
