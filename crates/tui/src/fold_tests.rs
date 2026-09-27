//! Folding stays in pane-local presentation, never in copy/edit/model content.

use super::selection_tests::{numbered_lines, plain_block};
use super::*;
use crate::app::{FoldKey, FoldState};
use crate::layout::ConversationCache;
use crate::presentation::{
    AcpToolPresentation, ConversationEntry, PresentationBlockId, PresentedTool,
};

fn chord(app: &mut App, suffix: char) {
    chord_with_modifiers(app, suffix, KeyModifiers::NONE);
}

fn chord_with_modifiers(app: &mut App, suffix: char, modifiers: KeyModifiers) {
    assert_eq!(app.handle_event(key(KeyCode::Char('z'))), None);
    assert!(app.interaction().pending_z());
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char(suffix), modifiers)),
        None
    );
    assert!(!app.interaction().pending_z());
}

fn fold_messages(app: &mut App, indices: impl IntoIterator<Item = usize>) {
    for history_index in indices {
        app.select_message_for_test(cursor(history_index, 0));
        chord(app, 'c');
    }
    app.select_for_test(None);
}

fn select_scope(app: &mut App, scope: Option<SelectionScope>, selection: Option<Selection>) {
    match scope {
        None => app.select_for_test(None),
        Some(SelectionScope::Message) => app.select_message_for_test(selection),
        Some(SelectionScope::Block) => app.select_for_test(selection),
    }
}

fn fold_keys(app: &App) -> std::collections::HashSet<FoldKey> {
    let mut keys = std::collections::HashSet::new();
    for (history_index, entry) in app.history().iter().enumerate() {
        if let Some((start, end)) = app.folds().span_containing(history_index) {
            keys.insert(FoldKey::Span { start, end });
        }
        if message_folded(app, history_index) {
            keys.insert(FoldKey::Message { history_index });
        }
        for content_index in 0..entry.selectable_upper_bound() {
            if folded(app, history_index, content_index) {
                keys.insert(match entry {
                    HistoryEntry::Conversation(entry) => FoldKey::Block {
                        history_index,
                        id: entry.blocks[content_index].id,
                    },
                    _ => FoldKey::Item {
                        history_index,
                        content_index,
                    },
                });
            }
        }
    }
    keys
}

fn folded(app: &App, history: usize, content: usize) -> bool {
    match &app.history()[history] {
        HistoryEntry::Conversation(entry) => app
            .folds()
            .entry(history)
            .is_block_folded(entry.blocks[content].id),
        _ => app.folds().entry(history).is_item_folded(content),
    }
}

fn semantic_id(app: &App, history: usize, content: usize) -> PresentationBlockId {
    let HistoryEntry::Conversation(entry) = &app.history()[history] else {
        panic!("semantic entry")
    };
    entry.blocks[content].id
}

fn message_folded(app: &App, history: usize) -> bool {
    app.folds().entry(history).is_message_folded()
}

fn executing_edit() -> App {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            turn_id: TEST_TURN_ID,
            display_attempt_id: None,
            message: assistant_message(vec![tool_call(
                "edit",
                None,
                "edit",
                json!({"file_path":"src/main.rs"}),
            )]),
        },
    );
    app
}

fn finish_edit(app: &mut App, output: &str) {
    app.reduce_without_effects(SessionEvent::ToolResults {
        turn_id: TEST_TURN_ID,
        message: tool_result_message("edit", None, "edit", output),
        metadata: vec![
            file_metadata("edit", None, "edit", ToolCallOutcome::Error, Vec::new())
                .with_diagnostic(output),
        ],
    });
}

fn start_span_turn(app: &mut App, id: u64, prompt: &str) {
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TurnId::new(id),
        message: Message::user(prompt),
        mode: SessionMode::Build,
    });
}

fn span_call(app: &mut App, id: u64, call: usize, message: Message, complete: bool) {
    let turn_id = TurnId::new(id);
    app.reduce_without_effects(SessionEvent::ModelCallStarted { turn_id, call });
    app.reduce_without_effects(if complete {
        SessionEvent::TurnCompleted {
            turn_id,
            message,
            display_attempt_id: None,
        }
    } else {
        SessionEvent::Intermediate {
            turn_id,
            message,
            display_attempt_id: None,
        }
    });
}

fn span_turn(app: &mut App, id: u64, calls: usize) {
    start_span_turn(app, id, &format!("turn {id} prompt\n{}", numbered_lines(6)));
    for call in 1..=calls {
        span_call(
            app,
            id,
            call,
            assistant_message(vec![
                AssistantContent::text(format!("call {id}-{call}\n\n{}", numbered_lines(6))),
                AssistantContent::text("second block"),
            ]),
            call == calls,
        );
    }
}

/// Long intermediate calls make manual scrolling possible, while short final
/// answers let the folded transcript fit in the pane.
fn scrolled_turns_with_short_finals(turns: u64, width: u16, height: u16) -> App {
    let mut app = App::new();
    for turn in 1..=turns {
        start_span_turn(
            &mut app,
            turn,
            &format!("turn {turn} prompt\n{}", numbered_lines(6)),
        );
        for call in 1..=2 {
            span_call(
                &mut app,
                turn,
                call,
                Message::assistant(format!(
                    "intermediate {turn}-{call}\n\n{}",
                    numbered_lines(60)
                )),
                false,
            );
        }
        span_call(
            &mut app,
            turn,
            3,
            Message::assistant(format!("final {turn}\n\nfinished {turn}")),
            true,
        );
    }
    rendered_buffer(&mut app, width, height);
    let intermediate = app.history().len() - 2;
    let top = app.view_cache().entries()[..intermediate]
        .iter()
        .map(|entry| entry.extent())
        .sum::<usize>()
        + app.view_cache().entries()[intermediate].items[0].1.start()
        + 5;
    assert!(app.view_scroll() > top);
    for _ in top..app.view_scroll() {
        app.handle_event(key(KeyCode::Char('k')));
    }
    rendered_buffer(&mut app, width, height);
    assert_eq!(app.view_scroll(), top);
    assert!(!app.view_follow());
    app
}

fn first_fold_frame(
    app: &mut App,
    suffix: char,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    assert_eq!(app.handle_event(key(KeyCode::Char('z'))), None);
    assert!(app.interaction().pending_z());
    rendered_buffer(app, width, height);
    let modifiers = if suffix.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    assert_eq!(
        app.handle_event(modified_key(KeyCode::Char(suffix), modifiers)),
        None
    );
    assert!(!app.interaction().pending_z());
    // This must be the very first render after the fold, not a cache-only check.
    rendered_buffer(app, width, height)
}

fn conversation_frame_text(buffer: &ratatui::buffer::Buffer) -> String {
    let content = conversation_content_area(buffer, false);
    (content.y..content.bottom())
        .map(|y| {
            (content.x..content.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn turn_folds_show_prompt_on_first_frame_when_content_fits() {
    for suffix in ['M', 'm'] {
        for (width, height) in [(40, 20), (80, 30), (120, 40)] {
            let mut app = scrolled_turns_with_short_finals(1, width, height);
            let first = conversation_frame_text(&first_fold_frame(&mut app, suffix, width, height));
            let summary = "▸ turn 1 prompt · 6 more rows";
            assert!(laid_out_transcript_text(&app).contains(summary));
            assert!(
                first.contains(summary),
                "z{suffix} at {width}x{height}: {first}"
            );
            let viewport = app.render_parts().view.conversation_viewport().clone();
            assert_eq!(viewport.max_top(), 0, "the folded transcript fits");
            assert!(viewport.top() <= viewport.max_top());
            assert_eq!(viewport.top(), viewport.visible_range().start());
            assert!(!app.view_follow(), "clamping must not enable follow");
            let second = conversation_frame_text(&rendered_buffer(&mut app, width, height));
            assert_eq!(first, second, "another render must not repair missing rows");
        }
    }
}

#[test]
fn turn_folds_keep_overflowing_first_frame_consistent() {
    for suffix in ['M', 'm'] {
        for (width, height) in [(80, 8), (80, 17)] {
            let mut app = scrolled_turns_with_short_finals(2, width, height);
            let anchor = app.view_cache().semantic_anchor(app.view_scroll()).unwrap();
            let first = conversation_frame_text(&first_fold_frame(&mut app, suffix, width, height));
            let row = app.view_cache().anchor_row(anchor).unwrap();
            let viewport = app.render_parts().view.conversation_viewport().clone();
            assert!(
                viewport.max_top() > 0,
                "the folded transcript still overflows"
            );
            assert_eq!(
                row > viewport.max_top(),
                height == 17,
                "z{suffix} at {width}x{height}: row={row}, max_top={}",
                viewport.max_top()
            );
            assert_eq!(viewport.top(), row.min(viewport.max_top()));
            assert_eq!(viewport.top(), viewport.visible_range().start());
            assert!(!app.view_follow());
            assert!(
                !first.contains("turn 1 prompt"),
                "offscreen prompts need not be visible"
            );
            let second = conversation_frame_text(&rendered_buffer(&mut app, width, height));
            assert_eq!(first, second, "another render must not repair missing rows");
        }
    }
}

#[test]
fn z_m_shows_multi_turn_prompts_on_first_frame_and_keeps_latest_final_expanded() {
    for (width, height) in [(60, 32), (100, 40), (140, 50)] {
        let mut app = scrolled_turns_with_short_finals(2, width, height);
        let first = conversation_frame_text(&first_fold_frame(&mut app, 'M', width, height));
        let cached = laid_out_transcript_text(&app);
        for turn in 1..=2 {
            let summary = format!("▸ turn {turn} prompt · 6 more rows");
            assert!(cached.contains(&summary));
            assert!(first.contains(&summary), "{width}x{height}: {first}");
        }
        assert!(message_folded(&app, 3));
        assert!(!message_folded(&app, 7));
        assert!(first.contains("▸ final 1"));
        assert!(!first.contains("finished 1"));
        assert!(first.contains("final 2") && first.contains("finished 2"));
        assert!(!first.contains("▸ final 2"));
        let viewport = app.render_parts().view.conversation_viewport().clone();
        assert_eq!(viewport.max_top(), 0, "the folded transcript fits");
        assert!(viewport.top() <= viewport.max_top());
        assert_eq!(viewport.top(), viewport.visible_range().start());
        assert!(!app.view_follow());
        let second = conversation_frame_text(&rendered_buffer(&mut app, width, height));
        assert_eq!(first, second, "another render must not repair missing rows");
    }
}

#[test]
fn z_m_preserves_follow_and_selection_navigation_at_different_sizes() {
    for (width, height) in [(40, 12), (80, 24), (120, 40)] {
        let mut app = scrolled_turns_with_short_finals(2, width, height);
        app.handle_event(modified_key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        rendered_buffer(&mut app, width, height);
        first_fold_frame(&mut app, 'M', width, height);
        assert!(app.view_follow());
        let viewport = app.render_parts().view.conversation_viewport().clone();
        assert_eq!(viewport.top(), viewport.max_top());

        app.handle_event(key(KeyCode::Char('k')));
        rendered_buffer(&mut app, width, height);
        assert!(!app.view_follow());
        app.handle_event(key(KeyCode::Char('j')));
        assert!(!app.view_follow(), "re-pinning waits for the next render");
        rendered_buffer(&mut app, width, height);
        assert!(app.view_follow());

        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(7, 0));
        assert!(!app.view_follow());
        for (event, selected, visible) in [
            (
                key(KeyCode::Char('k')),
                cursor(5, 0),
                "▸ 2 earlier messages",
            ),
            (key(KeyCode::Char('k')), cursor(4, 0), "▸ turn 2 prompt"),
            (ctrl('u'), cursor(0, 0), "▸ turn 1 prompt"),
        ] {
            app.handle_event(event);
            assert_eq!(app.selection(), selected);
            assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
            let frame = conversation_frame_text(&rendered_buffer(&mut app, width, height));
            assert!(frame.contains(visible), "{width}x{height}: {frame}");
            assert!(!app.view_follow());
        }
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(modified_key(KeyCode::Char('G'), KeyModifiers::SHIFT));
        rendered_buffer(&mut app, width, height);
        assert!(app.view_follow());
        let viewport = app.render_parts().view.conversation_viewport().clone();
        assert_eq!(viewport.top(), viewport.max_top());
    }
}

#[test]
fn zm_keeps_prompt_rows_and_collapses_earlier_calls_leaving_the_last_expanded() {
    for inner_fold in [false, true] {
        let mut app = App::new();
        span_turn(&mut app, 1, 3);
        span_turn(&mut app, 2, 1);
        rendered_text(&mut app, 100, 35);
        let unfolded_rows = app.view_cache().entries()[1..=2]
            .iter()
            .map(|e| e.height)
            .sum::<usize>();
        if inner_fold {
            app.select_for_test(cursor(1, 0));
            chord(&mut app, 'c');
            app.select_for_test(None);
        }
        rendered_text(&mut app, 100, 35);
        let rows = app.view_cache().entries()[1..=2]
            .iter()
            .map(|e| e.height)
            .sum::<usize>();
        assert_eq!(rows < unfolded_rows, inner_fold);
        let rebuilt = app.view_cache().block_rebuilds;
        chord(&mut app, 'm');
        let text = rendered_text(&mut app, 100, 50);
        assert_eq!(
            text.matches(&format!("▸ 2 earlier messages · {rows} more rows"))
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("Assistant · #(1 - 3)"));
        assert!(text.contains("Assistant · #(2 - 1)"));
        for turn in [1, 2] {
            assert!(text.contains(&format!("You · #{turn}")));
            assert!(text.contains(&format!("▸ turn {turn} prompt · 6 more rows")));
        }
        assert!(!text.contains("▸ 1 message"));
        assert!(!text.contains("▸ 1 earlier message"));
        for hidden in ["#(1 - 1)", "#(1 - 2)"] {
            assert!(!text.contains(hidden), "{hidden}");
        }
        for index in [0, 4] {
            assert!(message_folded(&app, index));
            assert_eq!(app.folds().span_containing(index), None);
        }
        let entries = app.view_cache().entries();
        assert!(entries[0].height > 1);
        assert_eq!(entries[1].height, 1);
        assert!(
            entries[1]
                .items
                .iter()
                .all(|(_, rows)| *rows == RowRange::new(0, 1))
        );
        let hidden = &entries[2];
        assert_eq!(hidden.height, 0);
        assert_eq!(hidden.extent(), 0);
        assert!(hidden.lines.is_empty() && hidden.items.is_empty());
        assert!(hidden.selection.is_none());
        assert_eq!(
            app.view_cache().block_rebuilds,
            rebuilt,
            "covered blocks are reused"
        );
        let rebuilds = app.view_cache().rebuilds;
        let text = laid_out_transcript_text(&app);
        chord(&mut app, 'm');
        rendered_text(&mut app, 100, 35);
        assert_eq!(laid_out_transcript_text(&app), text);
        assert_eq!(
            app.view_cache().rebuilds,
            rebuilds,
            "summaries are cached too"
        );
        chord(&mut app, 'R');
        rendered_text(&mut app, 100, 35);
        assert!(laid_out_transcript_text(&app).contains("You · #1"));
        if !inner_fold {
            assert_eq!(
                app.view_cache().block_rebuilds,
                rebuilt,
                "expansion reuses covered layouts"
            );
        }
    }
}

#[test]
fn turn_folds_preserve_final_entry_policy_in_normal_and_both_select_scopes() {
    for suffix in ['m', 'M'] {
        for scope in [
            None,
            Some(SelectionScope::Message),
            Some(SelectionScope::Block),
        ] {
            let mut app = App::new();
            for turn in 1..=3 {
                span_turn(&mut app, turn, 3);
            }
            rendered_text(&mut app, 100, 50);
            let expanded = laid_out_transcript_text(&app);
            select_scope(&mut app, scope, cursor(2, 1));
            let modifiers = if suffix == 'M' {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            chord_with_modifiers(&mut app, suffix, modifiers);
            if scope.is_some() {
                assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
                assert_eq!(app.selection(), cursor(1, 0));
            }
            rendered_text(&mut app, 100, 50);
            let text = laid_out_transcript_text(&app);
            assert_eq!(text.matches("▸ 2 earlier messages ·").count(), 3);
            for (turn, prompt) in [(1, 0), (2, 4), (3, 8)] {
                assert!(text.contains(&format!("You · #{turn}")));
                assert!(text.contains(&format!("▸ turn {turn} prompt · 6 more rows")));
                assert!(message_folded(&app, prompt));
                assert_eq!(app.folds().span_containing(prompt), None);
                assert_eq!(
                    app.folds().span_containing(prompt + 1),
                    Some((prompt + 1, prompt + 2))
                );
                assert_eq!(app.view_cache().entries()[prompt + 1].height, 1);
                assert_eq!(app.view_cache().entries()[prompt + 2].height, 0);
                let final_index = prompt + 3;
                assert_eq!(app.folds().span_containing(final_index), None);
                let fold_final = suffix == 'M' && turn < 3;
                assert_eq!(message_folded(&app, final_index), fold_final);
                let final_entry = &app.view_cache().entries()[final_index];
                let final_text = final_entry
                    .lines
                    .iter()
                    .map(line_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(final_text.contains(&format!("Assistant · #({turn} - 3)")));
                if fold_final {
                    assert!(final_text.contains(&format!("▸ call {turn}-3 · 2 blocks ·")));
                    assert!(final_entry.items.iter().all(|(_, rows)| rows.len() == 1));
                    assert!(!final_text.contains("row 5"));
                } else {
                    assert!(!final_text.contains('▸'));
                    assert!(final_text.contains("row 5"));
                    assert!(final_entry.items[0].1.len() > 1);
                }
            }
            let rebuilds = (app.view_cache().rebuilds, app.view_cache().block_rebuilds);
            let intent = fold_keys(&app);
            assert_eq!(intent.len(), if suffix == 'M' { 8 } else { 6 });
            for repeat in [suffix, 'm'] {
                chord(&mut app, repeat);
                assert_eq!(fold_keys(&app), intent);
                rendered_text(&mut app, 100, 50);
                assert_eq!(laid_out_transcript_text(&app), text);
                assert_eq!(
                    (app.view_cache().rebuilds, app.view_cache().block_rebuilds),
                    rebuilds
                );
                for index in [3, 7] {
                    assert_eq!(message_folded(&app, index), suffix == 'M');
                }
                assert!(!message_folded(&app, 11));
            }
            chord(&mut app, 'R');
            rendered_text(&mut app, 100, 50);
            assert_eq!(laid_out_transcript_text(&app), expanded);
            for index in 0..app.history().len() {
                assert!(!message_folded(&app, index));
                assert_eq!(app.folds().span_containing(index), None);
            }

            app.select_for_test(None);
            chord(&mut app, suffix);
            span_turn(&mut app, 4, 3);
            rendered_text(&mut app, 100, 50);
            assert!(
                !message_folded(&app, 11),
                "the previously latest final stays open"
            );
            for index in 12..16 {
                assert!(!message_folded(&app, index));
                assert_eq!(app.folds().span_containing(index), None);
                assert!(app.view_cache().entries()[index].height > 1);
            }
            chord(&mut app, suffix);
            rendered_text(&mut app, 100, 50);
            assert_eq!(
                message_folded(&app, 11),
                suffix == 'M',
                "only another zM folds the older final"
            );
            assert!(message_folded(&app, 12));
            assert_eq!(app.folds().span_containing(13), Some((13, 14)));
            assert!(!message_folded(&app, 15));
            let text = laid_out_transcript_text(&app);
            assert_eq!(text.contains("▸ call 3-3 · 2 blocks ·"), suffix == 'M');
            assert!(text.contains("Assistant · #(4 - 3)"));
            assert!(!text.contains("▸ call 4-3"));
        }
    }
}

#[test]
fn turn_folds_respect_leading_headerless_run_dividers_and_acp_panes() {
    for suffix in ['m', 'M'] {
        let mut app = App::new();
        for text in ["leading first", "leading middle", "leading last"] {
            app.seed_history_entry(history_message(Message::user(text)));
        }
        start_span_turn(&mut app, 1, "numbered prompt");
        span_call(&mut app, 1, 1, Message::assistant("first call"), false);
        app.seed_history_entry(HistoryEntry::CompactionDivider);
        app.seed_history_entry(HistoryEntry::PlanArtifact(test_plan_artifact()));
        app.seed_history_entry(HistoryEntry::Error("turn error".into()));
        span_call(&mut app, 1, 2, Message::assistant("last call"), true);
        chord(&mut app, suffix);
        rendered_text(&mut app, 100, 35);
        for (start, end) in [(0, 1), (4, 4), (6, 7)] {
            assert_eq!(app.folds().span_containing(start), Some((start, end)));
            assert_eq!(app.view_cache().entries()[start].height, 1);
        }
        assert!(message_folded(&app, 3));
        assert_eq!(app.folds().span_containing(3), None);
        assert_eq!(app.folds().span_containing(5), None);
        assert!(
            app.view_cache().entries()[5].height > 0,
            "divider never folds"
        );
        let text = laid_out_transcript_text(&app);
        assert_eq!(text.matches("▸ 2 earlier messages ·").count(), 2);
        assert_eq!(text.matches("▸ 1 earlier message ·").count(), 1);
        assert!(text.contains("You · #1") && text.contains("numbered prompt"));
        assert!(text.contains("leading last"));
        assert!(text.contains("last call"));
        assert_eq!(message_folded(&app, 2), suffix == 'M');
        assert!(!message_folded(&app, 8));
        let special_offset = app.view_cache().entries()[..6]
            .iter()
            .map(|e| e.extent())
            .sum();
        assert_eq!(app.view_cache().semantic_anchor(special_offset), None);

        for scope in [
            None,
            Some(SelectionScope::Message),
            Some(SelectionScope::Block),
        ] {
            let (mut app, mut reducer) = acp_transcript_app();
            for cycle in 0..3 {
                for event in [
                    AgentRunEvent::Prompt {
                        text: format!("cycle {cycle}"),
                        continuation: cycle > 0,
                        repair: None,
                    },
                    AgentRunEvent::AgentMessage {
                        text: format!("answer {cycle}"),
                        message_id: Some(format!("answer-{cycle}")),
                    },
                ] {
                    apply_agent_event(&mut app, &mut reducer, event);
                }
            }
            assert_eq!(app.history().len(), 3);
            select_scope(&mut app, scope, cursor(1, 1));
            chord(&mut app, suffix);
            if scope.is_some() {
                assert_eq!(app.selection(), cursor(0, 0));
                assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
            }
            rendered_text(&mut app, 100, 25);
            assert_eq!(app.folds().span_containing(0), Some((0, 1)));
            let text = laid_out_transcript_text(&app);
            assert!(text.contains("▸ 2 earlier messages ·"));
            assert!(!text.contains("cycle 0") && !text.contains("cycle 1"));
            assert!(text.contains("cycle 2") && text.contains("answer 2"));
            assert!(app.view_cache().entries()[0].decorations.is_empty());
            assert!(
                app.view_cache().entries()[0]
                    .items
                    .iter()
                    .all(|(_, rows)| *rows == RowRange::new(0, 1))
            );
            let intent = fold_keys(&app);
            assert_eq!(
                intent,
                std::collections::HashSet::from([FoldKey::Span { start: 0, end: 1 }])
            );
            chord(&mut app, suffix);
            assert_eq!(fold_keys(&app), intent);
            app.select_message_for_test(cursor(0, 0));
            for _ in 0..2 {
                app.handle_event(ctrl('d'));
                assert_eq!(app.selection(), cursor(2, 0));
            }
            for _ in 0..2 {
                app.handle_event(ctrl('u'));
                assert_eq!(
                    app.selection(),
                    cursor(0, 0),
                    "hidden prompts are not extra stops"
                );
            }
            chord(&mut app, 'R');
            assert!(fold_keys(&app).is_empty());
        }
    }
}

#[test]
fn span_summary_selection_navigation_enter_and_yank() {
    let mut app = App::new();
    span_turn(&mut app, 1, 3);
    span_turn(&mut app, 2, 1);
    let expected = (1..=2)
        .filter_map(|index| {
            app.conversation_projection_mut()
                .selected_message_text(index, false)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    app.select_message_for_test(cursor(1, 0));
    chord(&mut app, 'c');
    app.select_for_test(cursor(2, 1));
    chord(&mut app, 'm');
    assert_eq!(app.selection(), cursor(1, 0));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    let buffer = rendered_buffer(&mut app, 100, 35);
    let summary_y = (0..buffer.area.height)
        .find(|&y| buffer_row_text(&buffer, y).contains("▸ 2 earlier messages"))
        .unwrap();
    let selection_bg = crate::chrome::selection_style().bg.unwrap();
    for y in 0..buffer.area.height {
        assert_eq!(
            (0..buffer.area.width).any(|x| buffer[(x, y)].bg == selection_bg),
            y == summary_y
        );
    }
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy { text: expected })
    );
    for (code, target) in [
        (KeyCode::Char('j'), 3),
        (KeyCode::Char('k'), 1),
        (KeyCode::Char('k'), 0),
        (KeyCode::Char('j'), 1),
        (KeyCode::Down, 3),
        (KeyCode::Down, 4),
        (KeyCode::Down, 5),
        (KeyCode::Down, 5),
        (KeyCode::Up, 4),
        (KeyCode::Up, 3),
        (KeyCode::Up, 1),
        (KeyCode::Up, 0),
        (KeyCode::Up, 0),
    ] {
        app.handle_event(key(code));
        assert_eq!(app.selection(), cursor(target, 0));
    }
    for (code, target) in [('d', 4), ('d', 4), ('u', 0), ('u', 0)] {
        app.handle_event(ctrl(code));
        assert_eq!(app.selection(), cursor(target, 0));
    }
    // Block-scope user jumps also return to a visible Message representative.
    app.select_for_test(cursor(3, 1));
    app.handle_event(ctrl('u'));
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(1, 0));
    chord(&mut app, 'c');
    assert_eq!(
        app.folds().span_containing(1),
        Some((1, 2)),
        "zc is a no-op on a summary"
    );
    for expand in [None, Some('a'), Some('o')] {
        if let Some(suffix) = expand {
            chord(&mut app, suffix);
        } else {
            app.handle_event(key(KeyCode::Enter));
        }
        assert_eq!(app.folds().span_containing(1), None);
        assert_eq!(app.folds().span_containing(4), None);
        for index in [0, 1, 4] {
            assert!(
                message_folded(&app, index),
                "expanding a span preserves Message keys"
            );
        }
        assert_eq!(app.selection(), cursor(1, 0));
        assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
        chord(&mut app, 'm');
    }
    chord(&mut app, 'R');
    for index in [0, 1, 4] {
        assert!(!message_folded(&app, index));
    }
    assert_eq!(app.folds().span_containing(1), None);
    chord(&mut app, 'm');
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.selection(), cursor(0, 0));
    ctrl_e(&mut app);
    assert_eq!(app.input(), format!("turn 1 prompt\n{}", numbered_lines(6)));
    assert_eq!(
        app.recalled_edit_target(),
        Some(&TranscriptEditTarget::PromptOrdinal(0))
    );
}

#[test]
fn zm_keeps_new_entries_expanded_and_survives_edits() {
    let mut app = App::new();
    start_span_turn(
        &mut app,
        1,
        &"a long prompt with words to wrap at different widths ".repeat(8),
    );
    for call in 1..=2 {
        span_call(
            &mut app,
            1,
            call,
            Message::assistant(format!(
                "call {call}: {}",
                "a long assistant message with words to wrap at different widths ".repeat(8)
            )),
            false,
        );
    }
    chord(&mut app, 'm');
    rendered_text(&mut app, 100, 25);
    assert!(message_folded(&app, 0));
    assert_eq!(app.folds().span_containing(1), Some((1, 1)));
    span_call(&mut app, 1, 3, Message::assistant("new call"), true);
    span_turn(&mut app, 2, 1);
    rendered_text(&mut app, 100, 25);
    let before = line_text(&app.view_cache().entries()[1].lines[0]);
    assert_eq!(app.folds().span_containing(1), Some((1, 1)));
    for index in 2..6 {
        assert!(app.view_cache().entries()[index].height > 1);
        assert_eq!(app.folds().span_containing(index), None);
    }
    rendered_text(&mut app, 60, 25);
    let after = line_text(&app.view_cache().entries()[1].lines[0]);
    assert_ne!(before, after, "width changes recount covered wrapped rows");
    assert_eq!(app.folds().span_containing(1), Some((1, 1)));
    app.select_message_for_test(cursor(1, 0));
    chord(&mut app, 'o');
    rendered_text(&mut app, 60, 25);
    let rows = app.view_cache().entries()[1].height;
    assert_eq!(after, format!("▸ 1 earlier message · {rows} more rows"));
    chord(&mut app, 'm');
    assert_eq!(
        app.folds().span_containing(1),
        Some((1, 2)),
        "only another zm extends the span"
    );
    rendered_text(&mut app, 60, 25);
    let change = app.conversation_projection_mut().commit_edit(2, false);
    assert!(app.apply_conversation_change(change).is_empty());
    rendered_text(&mut app, 60, 25);
    assert_eq!(app.history().len(), 2);
    assert!(message_folded(&app, 0));
    assert!(!message_folded(&app, 1));
    for index in 0..2 {
        assert_eq!(app.folds().span_containing(index), None);
    }
    assert!(app.view_cache().entries().iter().all(|e| e.height > 1));
}

#[test]
fn zm_keeps_the_top_row_anchored() {
    for content in 0..3 {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("leading entry")));
        start_span_turn(&mut app, 1, "prompt");
        for call in 1..=3 {
            span_call(
                &mut app,
                1,
                call,
                assistant_message(
                    (0..3)
                        .map(|_| AssistantContent::text(numbered_lines(10)))
                        .collect(),
                ),
                call == 3,
            );
        }
        rendered_text(&mut app, 80, 10);
        let offset = app.view_cache().entries()[..2]
            .iter()
            .map(|e| e.extent())
            .sum::<usize>();
        let top = app.view_cache().entries()[..3]
            .iter()
            .map(|e| e.extent())
            .sum::<usize>()
            + app.view_cache().entries()[3].items[content].1.start()
            + 4;
        app.set_view_for_test(top, false);
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), top);
        chord(&mut app, 'm');
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), offset);
        assert!(!app.view_follow());
        assert!(message_folded(&app, 1));
        assert_eq!(app.folds().span_containing(2), Some((2, 3)));
        assert_eq!(
            app.view_cache()
                .anchor_row((3, semantic_id(&app, 3, content), 4)),
            Some(offset)
        );
        assert_eq!(
            app.view_cache().semantic_anchor(offset),
            Some((2, semantic_id(&app, 2, 0), 0))
        );
        assert_eq!(
            app.view_cache().semantic_anchor(offset + 1),
            None,
            "summary gap has no anchor"
        );
        chord(&mut app, 'R');
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), offset);
        app.handle_event(key(KeyCode::Char('G')));
        for suffix in ['m', 'R'] {
            chord(&mut app, suffix);
            rendered_text(&mut app, 80, 10);
            assert!(app.view_follow());
            assert_eq!(
                app.view_scroll(),
                app.render_parts().view.conversation_viewport().max_top()
            );
        }
    }
}

#[test]
fn za_folds_selected_message_to_one_row_with_marker_and_count() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(numbered_lines(6))));
    rendered_text(&mut app, 80, 20);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert!(!app.interaction().selection_reveal());
    chord(&mut app, 'a');
    assert!(app.interaction().selection_reveal());
    assert!(app.rendered_selection_window().is_none());
    let buffer = rendered_buffer(&mut app, 80, 20);
    let text = (0..buffer.area.height)
        .map(|y| buffer_row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("● You"));
    assert!(text.contains("▸ row 0 · 5 more rows"));
    assert!(!text.contains("row 3"));
    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.items[0].1.len(), 1);
    assert_eq!(entry.selection, Some(entry.items[0].1));
    let row = entry
        .lines
        .iter()
        .find(|line| line_text(line).contains('▸'))
        .unwrap();
    assert_eq!(row.style.bg, crate::chrome::selection_style().bg);
    let summary_y = (0..buffer.area.height)
        .find(|&y| buffer_row_text(&buffer, y).contains('▸'))
        .unwrap();
    let selection_bg = crate::chrome::selection_style().bg.unwrap();
    assert!(buffer.content().iter().any(|cell| cell.bg == selection_bg));
    for y in 0..buffer.area.height {
        let selected_cells = (0..buffer.area.width).any(|x| buffer[(x, y)].bg == selection_bg);
        assert_eq!(
            selected_cells,
            y == summary_y,
            "only the folded body is selected"
        );
    }
    chord(&mut app, 'a');
    let text = rendered_text(&mut app, 80, 20);
    assert!(text.contains("row 3"));
    assert!(!text.contains('▸'));
    assert_eq!(app.view_cache().entries()[0].items[0].1.len(), 6);
}

#[test]
fn zc_and_zo_are_idempotent_and_single_row_items_never_fold() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(numbered_lines(6))));
    app.seed_history_entry(history_message(Message::user("short")));
    app.select_for_test(cursor(0, 0));
    for _ in 0..2 {
        chord(&mut app, 'c');
        rendered_text(&mut app, 80, 20);
        assert_eq!(app.view_cache().entries()[0].items[0].1.len(), 1);
    }
    for _ in 0..2 {
        chord(&mut app, 'o');
        rendered_text(&mut app, 80, 20);
        assert_eq!(app.view_cache().entries()[0].items[0].1.len(), 6);
    }
    app.select_for_test(cursor(1, 0));
    for suffix in ['a', 'a', 'c', 'c', 'o', 'o'] {
        chord(&mut app, suffix);
        rendered_text(&mut app, 80, 20);
        assert_eq!(app.view_cache().entries()[1].items[0].1.len(), 1);
        assert!(!laid_out_transcript_text(&app).contains('▸'));
    }
}

#[test]
fn fold_intent_survives_width_changes_without_collapsing_single_row_bodies() {
    let text = "a long line of words that is one row wide but wraps on a narrow terminal";
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(text)));
    app.select_for_test(cursor(0, 0));
    chord(&mut app, 'c');
    rendered_text(&mut app, 120, 20);
    assert!(folded(&app, 0, 0));
    assert!(!laid_out_transcript_text(&app).contains('▸'));
    rendered_text(&mut app, 40, 20);
    assert!(laid_out_transcript_text(&app).contains('▸'));
    assert_eq!(app.view_cache().entries()[0].items[0].1.len(), 1);
    rendered_text(&mut app, 120, 20);
    assert!(!laid_out_transcript_text(&app).contains('▸'));
    assert!(folded(&app, 0, 0));
}

#[test]
fn copy_while_folded_returns_full_content() {
    let text = numbered_lines(6);
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(text.clone())));
    app.select_for_test(cursor(0, 0));
    chord(&mut app, 'a');
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy { text: text.clone() })
    );
    ctrl_e(&mut app);
    assert_eq!(
        app.input(),
        text,
        "recall still sees the full model content"
    );

    let mut tool = executing_edit();
    finish_edit(&mut tool, &text);
    tool.select_for_test(cursor(0, 0));
    chord(&mut tool, 'a');
    rendered_text(&mut tool, 80, 20);
    assert_eq!(tool.view_cache().entries()[0].items[0].1.len(), 1);
    assert_eq!(
        tool.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: json!({"file_path":"src/main.rs"}).to_string()
        })
    );
    assert_eq!(
        tool.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy { text })
    );
}

#[test]
fn removed_fold_commands_are_inert_in_normal_and_both_select_scopes() {
    // zr, zt, zl, and zL are unsupported, not aliases or kind toggles.
    for scope in [
        None,
        Some(SelectionScope::Message),
        Some(SelectionScope::Block),
    ] {
        for pre_folded in [false, true] {
            let mut app = App::new();
            span_turn(&mut app, 1, 3);
            app.seed_history_entry(history_message(assistant_message(vec![
                AssistantContent::Reasoning(Reasoning::summaries(vec!["first\n\nsecond".into()])),
                AssistantContent::text("answer\n\nmore"),
                tool_call(
                    "native",
                    None,
                    "command",
                    json!({"command":"first\nsecond"}),
                ),
            ])));
            let mut tool = plain_block(40, PresentationRole::Assistant, "");
            tool.kind =
                PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(AcpToolPresentation {
                    metadata: None,
                    id: "acp".into(),
                    title: "ACP tool".into(),
                    kind: "execute".into(),
                    status: "failed".into(),
                    content: vec![numbered_lines(4)],
                    locations: vec![],
                    raw_input: None,
                    raw_output: None,
                })));
            app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: vec![tool],
            }));
            app.seed_history_entry(HistoryEntry::Error(numbered_lines(4)));
            if pre_folded {
                app.select_for_test(cursor(4, 0));
                chord(&mut app, 'c');
                app.select_for_test(cursor(6, 0));
                chord(&mut app, 'c');
                fold_messages(&mut app, [4]);
                chord(&mut app, 'm');
            }
            select_scope(&mut app, scope, cursor(6, 0));
            rendered_text(&mut app, 100, 35);
            let before = fold_keys(&app);
            let selection = app.selection();
            let text = laid_out_transcript_text(&app);
            assert_eq!(before.is_empty(), !pre_folded);
            for (suffix, modifiers) in [
                ('r', KeyModifiers::NONE),
                ('t', KeyModifiers::NONE),
                ('l', KeyModifiers::NONE),
                ('L', KeyModifiers::NONE),
                ('L', KeyModifiers::SHIFT),
            ] {
                chord_with_modifiers(&mut app, suffix, modifiers);
                assert_eq!(fold_keys(&app), before, "z{suffix}, {scope:?}");
                assert_eq!(app.selection(), selection);
                assert_eq!(app.selection_scope(), scope);
                // Consuming the unsupported suffix must also disarm the prefix.
                assert_eq!(app.handle_event(key(KeyCode::Char('M'))), None);
                assert_eq!(fold_keys(&app), before);
                rendered_text(&mut app, 100, 35);
                assert_eq!(laid_out_transcript_text(&app), text);
            }
        }
    }
}

#[test]
fn folds_survive_tool_revision_bumps_and_diagnostics_toggle() {
    let mut app = executing_edit();
    app.select_for_test(cursor(0, 0));
    chord(&mut app, 'c');
    rendered_text(&mut app, 80, 20);
    assert!(folded(&app, 0, 0));
    assert!(
        !laid_out_transcript_text(&app).contains('▸'),
        "executing file tool has one row"
    );
    finish_edit(&mut app, &numbered_lines(6));
    let text = rendered_text(&mut app, 80, 20);
    assert!(folded(&app, 0, 0));
    assert!(text.contains("▸ ◆ edit src/main.rs ✗ · 6 more rows"));
    assert!(!text.contains("row 3"));
    chord(&mut app, 'o');
    assert!(rendered_text(&mut app, 80, 20).contains("row 3"));

    let (mut inspect, mut reducer) = acp_transcript_app();
    apply_agent_event(
        &mut inspect,
        &mut reducer,
        AgentRunEvent::Prompt {
            text: numbered_lines(6),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut inspect,
        &mut reducer,
        AgentRunEvent::Stderr {
            text: "diagnostic\nsecond row".into(),
        },
    );
    fold_messages(&mut inspect, [0]);
    assert!(message_folded(&inspect, 0));
    assert!(
        !folded(&inspect, 0, 1),
        "message folding does not add hidden diagnostic Block keys"
    );
    for _ in 0..2 {
        inspect.handle_event(key(KeyCode::Char('d')));
        rendered_text(&mut inspect, 80, 20);
        assert!(message_folded(&inspect, 0));
        assert_eq!(inspect.view_cache().entries()[0].items[0].1.len(), 1);
    }
}

#[test]
fn message_folds_keep_the_top_item_anchored_and_follow_stays_at_bottom() {
    let mut app = App::new();
    for _ in 0..3 {
        app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
    }
    for within in [0, 17] {
        rendered_text(&mut app, 80, 9);
        let top = app.view_cache().entries()[0].extent() + within;
        app.set_view_for_test(top, false);
        rendered_text(&mut app, 80, 9);
        assert_eq!(app.view_scroll(), top);
        fold_messages(&mut app, 0..3);
        rendered_text(&mut app, 80, 9);
        let folded_start = app.view_cache().entries()[0].extent();
        assert_eq!(app.view_scroll(), folded_start + usize::from(within > 0));
        assert!(!app.view_follow());
        chord(&mut app, 'R');
    }
    app.handle_event(key(KeyCode::Char('G')));
    rendered_text(&mut app, 80, 9);
    for close in [true, false] {
        if close {
            fold_messages(&mut app, 0..3);
        } else {
            chord(&mut app, 'R');
        }
        rendered_text(&mut app, 80, 9);
        assert!(app.view_follow());
        let bottom = app.render_parts().view.conversation_viewport().max_top();
        assert_eq!(app.view_scroll(), bottom);
    }
}

#[test]
fn fold_toggle_rebuilds_only_the_toggled_block() {
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: (0..3)
            .map(|id| plain_block(id, PresentationRole::Assistant, &numbered_lines(6)))
            .collect(),
    }));
    app.select_for_test(cursor(0, 1));
    rendered_text(&mut app, 80, 24);
    let rebuilt = app.view_cache().block_rebuilds;
    let entries = app.view_cache().rebuilds;
    chord(&mut app, 'a');
    rendered_text(&mut app, 80, 24);
    assert_eq!(app.view_cache().block_rebuilds, rebuilt + 1);
    assert_eq!(app.view_cache().rebuilds, entries + 1);
    rendered_text(&mut app, 80, 24);
    assert_eq!(app.view_cache().block_rebuilds, rebuilt + 1);
    assert_eq!(app.view_cache().rebuilds, entries + 1);
}

#[test]
fn acp_card_blocks_fold_inside_their_card() {
    let (mut app, mut reducer) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::Prompt {
            text: numbered_lines(6),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut reducer,
        AgentRunEvent::ToolCall {
            id: "tool".into(),
            title: "inspect".into(),
            kind: "execute".into(),
            status: "failed".into(),
            content: vec![numbered_lines(4)],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        },
    );
    rendered_text(&mut app, 80, 24);
    let full = app.view_cache().entries()[0].items[0].1.len();
    app.select_for_test(cursor(0, 0));
    chord(&mut app, 'a');
    rendered_text(&mut app, 80, 24);
    let entry = &app.view_cache().entries()[0];
    assert_eq!(
        line_text(&entry.lines[1]),
        format!(" ▸ row 0 · {} more rows", full - 1)
    );
    assert_eq!(entry.items[0].1.len(), 1);
    assert!(entry.decorations[0].card);
    assert_eq!(entry.decorations[0].rows.end(), entry.items[0].1.end());
    app.select_for_test(cursor(0, 1));
    chord(&mut app, 'c');
    app.select_for_test(None);
    rendered_text(&mut app, 80, 24);
    let line = app.view_cache().entries()[0]
        .lines
        .iter()
        .find(|line| line_text(line).contains("▸ ◆"))
        .unwrap();
    assert_eq!(line_text(line), "▸ ◆ execute · inspect ✗ · 4 more rows");
    assert!(
        !app.view_cache().entries()[0].decorations[1].card,
        "ACP tools retain their non-card appearance"
    );
    let status = line
        .spans
        .iter()
        .find(|span| span.content.contains('✗'))
        .unwrap();
    assert_eq!(status.style.fg, Some(crate::theme::theme().feedback.error));
    let marker = line
        .spans
        .iter()
        .find(|span| span.content.contains('▸'))
        .unwrap();
    assert_eq!(marker.style.fg, Some(crate::theme::theme().text.muted));

    app.select_message_for_test(cursor(0, 1));
    chord(&mut app, 'c');
    rendered_text(&mut app, 80, 24);
    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.height, 2);
    assert!(line_text(&entry.lines[1]).starts_with(" ▸ "));
    assert!(line_text(&entry.lines[1]).contains("2 blocks"));
    assert_eq!(entry.decorations.len(), 1);
    assert!(entry.decorations[0].card);
    assert_eq!(entry.decorations[0].rows.end(), entry.height);
    assert_eq!(entry.items[0].1, entry.items[1].1);
}

#[test]
fn plan_artifact_handoff_error_and_ensemble_items_fold_and_unfold() {
    let start = editable_ensemble_start(
        "fold-ensemble",
        EnsembleWorkflow::Plan,
        "first\nsecond\nthird",
    );
    let mut app = idle_app_with_ensemble(start);
    let Some(HistoryEntry::Ensemble(ensemble)) = app.conversation_projection_mut().entry_mut(0)
    else {
        panic!("ensemble")
    };
    ensemble.workers[0].failure = Some("worker failure".into());
    let artifact = test_plan_artifact();
    app.seed_history_entry(HistoryEntry::PlanArtifact(artifact.clone()));
    app.seed_history_entry(HistoryEntry::PlanHandoff(
        PlanHandoff::new(artifact, "source"),
        None,
    ));
    app.seed_history_entry(HistoryEntry::Error(numbered_lines(6)));
    app.seed_history_entry(HistoryEntry::CompactionDivider);
    rendered_text(&mut app, 120, 30);
    let heights = app
        .view_cache()
        .entries()
        .iter()
        .map(|entry| entry.height)
        .collect::<Vec<_>>();
    fold_messages(&mut app, 0..4);
    rendered_text(&mut app, 120, 30);
    let entries = app.view_cache().entries();
    assert!(entries[0].items.iter().all(|(_, rows)| rows.len() == 1));
    assert_eq!(entries[0].items[1].1, entries[0].items[0].1);
    assert_eq!(
        entries[0].height, 2,
        "entire ensemble folds below its header"
    );
    for history in 0..4 {
        assert!(message_folded(&app, history));
        assert!(!folded(&app, history, 0));
    }
    for entry in &entries[1..4] {
        assert_eq!(entry.height, 2, "header plus summary");
    }
    assert_eq!(entries[4].height, heights[4]);
    let text = laid_out_transcript_text(&app);
    assert!(!text.contains("workers confirmed"));
    assert!(text.contains("▸ Durable approval workflow"));
    assert!(text.contains("Approved Plan handoff"));
    assert!(text.contains("● Error"));
    assert!(!text.contains("worker failure"));
    let rebuilt = app.view_cache().rebuilds;
    rendered_text(&mut app, 120, 30);
    assert_eq!(
        app.view_cache().rebuilds,
        rebuilt + 1,
        "only ensemble always rebuilds"
    );
    chord(&mut app, 'R');
    rendered_text(&mut app, 120, 30);
    assert_eq!(
        app.view_cache()
            .entries()
            .iter()
            .map(|entry| entry.height)
            .collect::<Vec<_>>(),
        heights
    );
    assert!(laid_out_transcript_text(&app).contains("worker failure"));
    // Also exercise per-item invalidation instead of only the global flag vector.
    for (history, &height) in heights.iter().enumerate().take(4).skip(1) {
        app.select_for_test(cursor(history, 0));
        chord(&mut app, 'a');
        rendered_text(&mut app, 120, 30);
        assert_eq!(app.view_cache().entries()[history].height, 2);
        chord(&mut app, 'a');
        rendered_text(&mut app, 120, 30);
        assert_eq!(app.view_cache().entries()[history].height, height);
    }
}

#[test]
fn z_chord_disarms_on_other_keys_and_is_text_in_insert() {
    let mut app = App::new();
    for _ in 0..2 {
        app.seed_history_entry(history_message(Message::user(numbered_lines(6))));
    }
    rendered_text(&mut app, 80, 12);
    app.handle_event(key(KeyCode::Char('z')));
    app.handle_event(key(KeyCode::Char('j')));
    assert!(!app.interaction().pending_z());
    app.handle_event(key(KeyCode::Char('M')));
    assert!(fold_keys(&app).is_empty());
    app.select_message_for_test(cursor(0, 0));
    app.handle_event(key(KeyCode::Char('z')));
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(1, 0));
    app.handle_event(key(KeyCode::Char('a')));
    assert!(fold_keys(&app).is_empty());
    app.handle_event(key(KeyCode::Esc));
    app.handle_event(key(KeyCode::Char('z')));
    assert_eq!(app.handle_event(ctrl('c')), Some(UiAction::Quit));
    assert!(
        !app.interaction().pending_z(),
        "early returns consume the chord too"
    );
    app.handle_event(ctrl('z'));
    assert!(
        !app.interaction().pending_z(),
        "modified z is not the plain prefix"
    );
    fold_messages(&mut app, [0]);
    let intent = fold_keys(&app);
    enter_insert(&mut app);
    // Even unsupported fold-command names remain literal text in Insert.
    let text = "za zc zo zm zM zR zr zt zl zL";
    for ch in text.chars() {
        let modifiers = if ch.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        assert_eq!(
            app.handle_event(modified_key(KeyCode::Char(ch), modifiers)),
            None
        );
        assert!(!app.interaction().pending_z());
        assert_eq!(fold_keys(&app), intent);
    }
    assert_eq!(app.input(), text);
}

#[test]
fn zr_does_not_trigger_a_pending_fresh_plan_retry() {
    let artifact = test_plan_artifact();
    let expected = artifact.version;
    let mut app = App::new();
    app.restore_plan_state(PlanWorkflowState::Resolved {
        artifact,
        resolution: PlanResolution::ImplementedFresh,
    });
    app.seed_history_entry(history_message(assistant_message(vec![
        AssistantContent::Reasoning(Reasoning::summaries(vec!["thought".into()])),
    ])));
    chord(&mut app, 'r');
    assert!(fold_keys(&app).is_empty(), "removed zr must be inert");
    assert_eq!(
        app.handle_event(key(KeyCode::Char('r'))),
        Some(UiAction::ResolvePlan {
            expected,
            decision: PlanDecision::ImplementFresh
        })
    );
}

#[test]
fn streamed_tail_is_never_folded() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(numbered_lines(6))));
    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("first\n\nsecond\n\nthird")).into(),
    });
    fold_messages(&mut app, [0]);
    rendered_text(&mut app, 80, 24);
    assert!(message_folded(&app, 0));
    let (lines, height) = app.view_cache().streaming().unwrap();
    let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(height > 2);
    assert!(text.contains("third"));
    assert!(!text.contains('▸'));
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        turn_id: TEST_TURN_ID,
        message: Message::assistant("first\n\nsecond\n\nthird"),
        display_attempt_id: None,
    });
    rendered_text(&mut app, 80, 24);
    assert!(
        !message_folded(&app, 1),
        "committed tail is a new, expanded item"
    );
}

#[test]
fn acp_in_place_report_rewrite_retains_survivors_and_prunes_removed_blocks() {
    let (mut app, mut reducer) = acp_transcript_app();
    for event in [
        AgentRunEvent::Prompt {
            text: numbered_lines(4),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "first\n\nparagraph".into(),
            message_id: Some("first".into()),
        },
        AgentRunEvent::Thought {
            text: "thought\n\nmore".into(),
            message_id: Some("thought".into()),
        },
        AgentRunEvent::AgentMessage {
            text: "second\n\nparagraph".into(),
            message_id: Some("second".into()),
        },
    ] {
        apply_agent_event(&mut app, &mut reducer, event);
    }
    for content in 0..4 {
        app.select_for_test(cursor(0, content));
        chord(&mut app, 'c');
    }
    fold_messages(&mut app, [0]);
    let HistoryEntry::Conversation(entry) = &app.history()[0] else {
        panic!("conversation")
    };
    let ids = entry
        .blocks
        .iter()
        .map(|block| block.id)
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 4);
    reconcile_agent_report(&mut app, &mut reducer, "new report\n\nnew paragraph");
    rendered_text(&mut app, 80, 24);
    for &id in &ids[..3] {
        assert!(app.folds().entry(0).is_block_folded(id));
    }
    assert!(!app.folds().entry(0).is_block_folded(ids[3]));
    assert!(message_folded(&app, 0), "Message key survives the rewrite");
    // A structural replacement still clears selection; reselect the message
    // to verify the surviving inner folds are revealed by zo.
    app.select_message_for_test(cursor(0, 0));
    chord(&mut app, 'o');
    rendered_text(&mut app, 80, 24);
    assert!(laid_out_transcript_text(&app).contains("▸ new report"));
}

#[test]
fn root_and_child_panes_keep_independent_folds_across_navigation() {
    let mut root = App::new();
    root.seed_history_entry(history_message(Message::user(numbered_lines(6))));
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    fold_messages(&mut root, [0]);
    let mut views = test_session_views(root);
    let id = SubtaskId::new("fold-child");
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("fold-child", "fold child"),
    });
    views.apply(SessionEvent::SubtaskSession {
        id: id.clone(),
        event: Box::new(SessionEvent::TurnStarted {
            turn_id: TurnId::new(42),
            message: Message::user(numbered_lines(6)),
            mode: SessionMode::Build,
        }),
    });
    views.handle_event(ctrl('i'));
    rendered_views_text(&mut views, 100, 20);
    assert_eq!(views.visible_child_id(), Some(&id));
    assert!(!message_folded(views.child(&id).unwrap(), 0));
    for code in [
        KeyCode::Esc,
        KeyCode::Esc,
        KeyCode::Char('z'),
        KeyCode::Char('c'),
        KeyCode::Esc,
    ] {
        views.handle_event(key(code));
    }
    assert!(message_folded(views.child(&id).unwrap(), 0));
    views.handle_event(ctrl('o'));
    assert_eq!(views.visible_child_id(), None);
    assert!(message_folded(views.root(), 0));
    for ch in ['z', 'R'] {
        views.handle_event(key(KeyCode::Char(ch)));
    }
    assert!(!message_folded(views.root(), 0));
    views.handle_event(ctrl('i'));
    assert_eq!(views.visible_child_id(), Some(&id));
    assert!(message_folded(views.child(&id).unwrap(), 0));
}

#[test]
fn message_fold_summary_counts_blocks_and_honours_inner_block_folds() {
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: (0..3)
            .map(|id| plain_block(id, PresentationRole::Assistant, &numbered_lines(4)))
            .collect(),
    }));
    app.select_for_test(cursor(0, 1));
    chord(&mut app, 'c');
    rendered_text(&mut app, 100, 24);
    assert_eq!(app.view_cache().entries()[0].height, 10);
    assert!(folded(&app, 0, 1));
    app.handle_event(key(KeyCode::Esc));
    chord(&mut app, 'c');
    rendered_text(&mut app, 100, 24);
    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.height, 2);
    assert_eq!(
        line_text(&entry.lines[1]),
        "▸ row 0 · 3 blocks · 8 more rows"
    );
    assert!(
        entry
            .items
            .iter()
            .all(|(_, rows)| *rows == RowRange::new(1, 2))
    );
    assert_eq!(entry.selection, Some(RowRange::new(1, 2)));
    assert!(message_folded(&app, 0));
    assert!(folded(&app, 0, 1));
    chord(&mut app, 'a');
    rendered_text(&mut app, 100, 24);
    assert!(!message_folded(&app, 0));
    assert!(folded(&app, 0, 1), "za removes only the outer fold");
    assert_eq!(app.view_cache().entries()[0].height, 10);
    chord(&mut app, 'R');
    rendered_text(&mut app, 100, 24);
    assert!(!folded(&app, 0, 1));
    assert_eq!(app.view_cache().entries()[0].height, 13);
}

#[test]
fn enter_on_a_folded_message_expands_it_and_selects_blocks() {
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![
            plain_block(0, PresentationRole::Assistant, &numbered_lines(4)),
            plain_block(1, PresentationRole::Assistant, &numbered_lines(4)),
        ],
    }));
    rendered_text(&mut app, 80, 24);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(0, 1));
    chord(&mut app, 'c');
    rendered_text(&mut app, 80, 24);
    assert!(message_folded(&app, 0));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert!(!message_folded(&app, 0));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Block));
    assert_eq!(app.selection(), cursor(0, 1));
    assert!(!app.interaction().selection_reveal());
    chord(&mut app, 'a');
    rendered_text(&mut app, 80, 24);
    assert!(!message_folded(&app, 0));
    assert!(folded(&app, 0, 1));
    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.items[0].1.len(), 4);
    assert_eq!(entry.items[1].1.len(), 1);
    assert_eq!(entry.selection, Some(entry.items[1].1));
}

#[test]
fn z_r_clears_all_four_fold_key_types_including_hidden_intent() {
    for scope in [
        None,
        Some(SelectionScope::Message),
        Some(SelectionScope::Block),
    ] {
        let mut app = App::new();
        span_turn(&mut app, 1, 3);
        app.seed_history_entry(HistoryEntry::Error(numbered_lines(4)));
        app.select_for_test(cursor(1, 0));
        chord(&mut app, 'c');
        app.select_for_test(cursor(4, 0));
        chord(&mut app, 'c');
        fold_messages(&mut app, [1, 4]);
        let expected = std::collections::HashSet::from([
            FoldKey::Span { start: 1, end: 3 },
            FoldKey::Message { history_index: 0 },
            FoldKey::Message { history_index: 1 },
            FoldKey::Message { history_index: 4 },
            FoldKey::Block {
                history_index: 1,
                id: semantic_id(&app, 1, 0),
            },
            FoldKey::Item {
                history_index: 4,
                content_index: 0,
            },
        ]);
        for suffix in ['m', 'M', 'm'] {
            chord(&mut app, suffix);
            assert_eq!(
                fold_keys(&app),
                expected,
                "turn folds preserve manual intent"
            );
        }
        let Some(HistoryEntry::Conversation(entry)) =
            app.conversation_projection_mut().entry_mut(1)
        else {
            panic!("conversation")
        };
        entry.blocks[0].visibility = crate::presentation::BlockVisibility::Diagnostics;
        entry.blocks[1].visibility = crate::presentation::BlockVisibility::Covered;
        select_scope(&mut app, scope, cursor(4, 0));
        rendered_text(&mut app, 100, 35);
        assert_eq!(
            fold_keys(&app),
            expected,
            "hidden intent survives rendering"
        );
        for _ in 0..2 {
            chord_with_modifiers(&mut app, 'R', KeyModifiers::SHIFT);
            assert!(fold_keys(&app).is_empty());
        }
    }
}

#[test]
fn turn_folds_drop_block_scope_only_when_the_selected_entry_is_folded() {
    for suffix in ['M', 'm'] {
        let mut app = App::new();
        span_turn(&mut app, 1, 1);
        span_turn(&mut app, 2, 1);
        app.select_for_test(cursor(0, 0));
        chord(&mut app, suffix);
        assert!(message_folded(&app, 0));
        assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
        assert!(!app.interaction().selection_reveal());

        app.select_for_test(cursor(1, 1));
        chord(&mut app, suffix);
        assert_eq!(message_folded(&app, 1), suffix == 'M');
        assert_eq!(
            app.selection_scope(),
            Some(if suffix == 'M' {
                SelectionScope::Message
            } else {
                SelectionScope::Block
            })
        );

        app.select_for_test(cursor(3, 1));
        chord(&mut app, suffix);
        assert_eq!(app.selection_scope(), Some(SelectionScope::Block));
        assert!(!message_folded(&app, 3));
    }
}

#[test]
fn message_scope_yank_copies_every_visible_block_but_never_tool_output() {
    let args = json!({"command":"pwd"});
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(assistant_message(vec![
            AssistantContent::Reasoning(Reasoning::summaries(vec!["thought".into()])),
            tool_call("tool", None, "command", args.clone()),
            AssistantContent::text("answer"),
        ])),
        TranscriptItem::ToolResults {
            message: tool_result_message("tool", None, "command", "SECRET tool output"),
            metadata: vec![],
            skill_applications: vec![],
        },
    ]);
    let Some(HistoryEntry::Conversation(entry)) = app.conversation_projection_mut().entry_mut(0)
    else {
        panic!("conversation")
    };
    for visibility in [
        crate::presentation::BlockVisibility::Covered,
        crate::presentation::BlockVisibility::Diagnostics,
    ] {
        entry.blocks.push(crate::presentation::PresentationBlock {
            visibility,
            ..plain_block(
                entry.blocks.len() as u64,
                PresentationRole::Assistant,
                "hidden sentinel",
            )
        });
    }
    app.select_message_for_test(cursor(0, 1));
    let expected = Some(UiAction::Copy {
        text: format!("thought\n\n{args}\n\nanswer"),
    });
    for suffix in ['c', 'o'] {
        chord(&mut app, suffix);
        assert_eq!(app.handle_event(key(KeyCode::Char('y'))), expected);
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            expected,
            "yy is also the primary message copy"
        );
    }
    app.handle_event(key(KeyCode::Enter));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: args.to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "SECRET tool output".into()
        })
    );
    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.handle_event(key(KeyCode::Char('y'))), expected);
}

#[test]
fn message_scope_recall_uses_first_editable_block() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::User {
        content: vec![UserContent::text("first"), UserContent::text("second")],
    }));
    app.select_message_for_test(cursor(0, 1));
    assert!(app.can_recall_selected());
    assert!(matches!(
        app.render_parts().chrome,
        crate::app::ComposerChrome::Selecting {
            can_recall: true,
            scope: SelectionScope::Message
        }
    ));
    chord(&mut app, 'c');
    ctrl_e(&mut app);
    assert_eq!(app.input(), "firstsecond");
    assert_eq!(
        app.recalled_edit_target(),
        Some(&TranscriptEditTarget::PromptOrdinal(0))
    );
    let start = editable_ensemble_start("recall-message", EnsembleWorkflow::Review, "inspect me");
    let mut app = idle_app_with_ensemble(start.clone());
    app.select_message_for_test(cursor(0, 1));
    assert!(
        app.can_recall_selected(),
        "message recall on a worker targets its prompt"
    );
    ctrl_e(&mut app);
    assert_eq!(app.input(), start.command());
    assert_eq!(
        app.recalled_edit_target(),
        Some(&TranscriptEditTarget::EnsembleRun(start.run_id))
    );
}

#[test]
fn message_selection_rebuilds_only_the_affected_entries() {
    let mut app = App::new();
    for _ in 0..3 {
        app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: (0..3)
                .map(|id| plain_block(id, PresentationRole::Assistant, "body"))
                .collect(),
        }));
    }
    app.select_message_for_test(cursor(0, 0));
    rendered_text(&mut app, 80, 24);
    let entries = app.view_cache().rebuilds;
    let blocks = app.view_cache().block_rebuilds;
    app.handle_event(key(KeyCode::Char('j')));
    rendered_text(&mut app, 80, 24);
    assert_eq!(app.view_cache().rebuilds, entries + 2);
    assert_eq!(app.view_cache().block_rebuilds, blocks + 6);
    let entry = &app.view_cache().entries()[1];
    assert_eq!(
        entry.selection,
        Some(RowRange::new(
            entry.items[0].1.start(),
            entry.items[2].1.end()
        ))
    );
    assert!(
        entry.lines[1..]
            .iter()
            .all(|line| line.style.bg == crate::chrome::selection_style().bg)
    );
    rendered_text(&mut app, 80, 24);
    assert_eq!(app.view_cache().rebuilds, entries + 2);
    app.handle_event(key(KeyCode::Enter));
    rendered_text(&mut app, 80, 24);
    assert_eq!(app.view_cache().rebuilds, entries + 3);
    assert_eq!(
        app.view_cache().block_rebuilds,
        blocks + 8,
        "only the two newly unselected blocks rebuild"
    );
}

#[test]
fn message_fold_keeps_the_top_row_anchored() {
    for content in [0, 1, 2] {
        let mut app = App::new();
        for _ in 0..4 {
            app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: (0..3)
                    .map(|id| plain_block(id, PresentationRole::Assistant, &numbered_lines(10)))
                    .collect(),
            }));
        }
        rendered_text(&mut app, 80, 10);
        let offset = app.view_cache().entries()[0].extent();
        let top = offset + app.view_cache().entries()[1].items[content].1.start() + 4;
        app.set_view_for_test(top, false);
        rendered_text(&mut app, 80, 10);
        app.select_message_for_test(cursor(1, content));
        chord(&mut app, 'c');
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), offset + 1);
        assert!(!app.view_follow());
        let entry = &app.view_cache().entries()[1];
        assert_eq!(entry.height, 2);
        assert_eq!(
            app.view_cache().semantic_anchor(offset + 1),
            Some((1, semantic_id(&app, 1, 0), 1))
        );
        assert_eq!(
            app.view_cache().semantic_anchor(offset + entry.height),
            None,
            "the gap is not part of a folded entry"
        );
        chord(&mut app, 'o');
        rendered_text(&mut app, 80, 10);
        assert_eq!(
            app.view_cache().anchor_row((1, semantic_id(&app, 1, 0), 1)),
            Some(offset + 1)
        );
    }
}

#[test]
fn summary_truncation_counts_wrapped_rows_and_preserves_unicode_at_narrow_widths() {
    let first = format!("  {}", "e\u{301} 👩‍💻 界 words ".repeat(8));
    let history = [history_message(Message::user(format!("{first}\nlast")))];
    let mut folds = FoldState::default();
    folds.fold(FoldKey::Block {
        history_index: 0,
        id: PresentationBlockId(0),
    });
    for width in [1, 2, 4, 12, 20, 24, 30, 40, 80] {
        let mut cache = ConversationCache::default();
        cache.refresh(&history, None, width, false, &FoldState::default());
        let full = cache.entries()[0].items[0].1.len();
        assert!(full > 1);
        cache.refresh(&history, None, width, false, &folds);
        let suffix = format!(" · {} more rows", full - 1);
        let show_count = usize::from(width) > 2 + crate::text::display_width(&suffix);
        let prefix = if width == 1 { "▸" } else { "▸ " };
        let budget = usize::from(width).saturating_sub(
            crate::text::display_width(prefix)
                + if show_count {
                    crate::text::display_width(&suffix)
                } else {
                    0
                },
        );
        let expected = format!(
            "{prefix}{}{}",
            if budget == 0 {
                String::new()
            } else {
                crate::text::truncate_display_width(first.trim_start(), budget)
            },
            if show_count { &suffix } else { "" }
        );
        let entry = &cache.entries()[0];
        assert_eq!(
            line_text(entry.lines.last().unwrap()),
            expected,
            "width {width}"
        );
        assert_eq!(
            entry.items[0].1.len(),
            crate::layout::prepare::wrapped_height(&entry.lines[1..], width)
        );
        assert_eq!(entry.items[0].1.len(), 1);
        assert!(entry.lines.last().unwrap().width() <= usize::from(width));
    }
}

#[test]
fn folding_does_not_expose_normally_hidden_tool_output() {
    let secret = "output must stay hidden\nsecond secret";
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(assistant_message(vec![tool_call(
            "command",
            None,
            "command",
            json!({"command":"first\nsecond"}),
        )])),
        TranscriptItem::ToolResults {
            message: tool_result_message("command", None, "command", secret),
            metadata: vec![],
            skill_applications: vec![],
        },
    ]);
    let (mut acp, mut reducer) = acp_transcript_app();
    apply_agent_event(
        &mut acp,
        &mut reducer,
        AgentRunEvent::ToolCall {
            id: "success".into(),
            title: "success".into(),
            kind: "execute".into(),
            status: "completed".into(),
            content: vec![secret.into()],
            locations: vec![AgentRunLocation {
                path: "src/main.rs".into(),
                line: Some(1),
            }],
            raw_input: None,
            raw_output: None,
        },
    );
    for pane in [&mut app, &mut acp] {
        pane.select_message_for_test(cursor(0, 0));
        for suffix in ['c', 'R'] {
            chord(pane, suffix);
            rendered_text(pane, 80, 20);
            assert!(!laid_out_transcript_text(pane).contains(secret));
            assert!(!laid_out_transcript_text(pane).contains("second secret"));
        }
    }
}
