//! Viewport-relative selection entry and cached semantic row geometry.

use super::*;
use crate::layout::ConversationCache;
use crate::presentation::{
    BlockVisibility, ConversationEntry, PresentationBlock, PresentationBlockId, TextFlavor,
};

const SELECTION_SHORTCUTS: [fn(&mut App); 2] = [double_escape, press_v];

fn assert_no_target(app: &mut App) {
    let before = (app.view_scroll(), app.view_follow());
    let rebuilds = app.view_cache().rebuilds;
    for enter_selection in SELECTION_SHORTCUTS {
        enter_selection(app);
        assert!(app.interaction().is_normal());
        assert_eq!(app.selection(), None);
        assert_eq!((app.view_scroll(), app.view_follow()), before);
        assert_eq!(app.view_cache().rebuilds, rebuilds);
    }
}

pub(super) fn numbered_lines(count: usize) -> String {
    (0..count).map(|index| format!("row {index}\n")).collect()
}

pub(super) fn plain_block(id: u64, role: PresentationRole, text: &str) -> PresentationBlock {
    PresentationBlock {
        id: PresentationBlockId(id),
        revision: 0,
        role: Some(role),
        prompt_group: None,
        prompt: None,
        visibility: BlockVisibility::Always,
        kind: PresentationBlockKind::Text {
            text: text.into(),
            flavor: TextFlavor::Plain,
            editable: false,
        },
    }
}

fn assert_user_jump(app: &mut App, chord: char, expected: Option<Selection>) {
    assert_eq!(app.handle_event(ctrl(chord)), None);
    assert_eq!(app.selection(), expected);
    assert!(app.interaction().is_selecting());
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
}

#[test]
fn user_jumps_group_native_text_and_images_and_skip_non_user_entries() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::User {
        content: vec![
            UserContent::text("first"),
            image.to_user_content(),
            UserContent::text("more"),
        ],
    }));
    app.seed_history_entry(history_message(assistant_message(vec![
        AssistantContent::text("answer"),
        tool_call("call", None, "command", json!({"command":"pwd"})),
    ])));
    app.seed_history_entry(history_message(Message::system("system")));
    let artifact = test_plan_artifact();
    app.seed_history_entry(HistoryEntry::PlanArtifact(artifact.clone()));
    app.seed_history_entry(HistoryEntry::PlanHandoff(
        PlanHandoff::new(artifact, "source"),
        None,
    ));
    app.seed_history_entry(HistoryEntry::Error("failure".into()));
    app.seed_history_entry(HistoryEntry::CompactionDivider);
    app.seed_history_entry(history_message(Message::User {
        content: vec![
            image.to_user_content(),
            UserContent::text("image-first prompt"),
        ],
    }));
    app.seed_history_entry(history_message(Message::assistant("last answer")));
    assert!(
        HistoryEntry::from_message(
            tool_result_message("call", None, "command", "result"),
            ToolCallStatus::Finished
        )
        .is_none(),
        "tool results must not manufacture user targets"
    );
    app.set_input_for_test("draft", 2);
    app.detach_follow_for_test();

    for source in [
        cursor(1, 0),
        cursor(1, 1),
        cursor(2, 0),
        cursor(3, 0),
        cursor(4, 0),
        cursor(5, 0),
    ] {
        app.select_for_test(source);
        assert_user_jump(&mut app, 'u', cursor(0, 0));
        app.select_for_test(source);
        assert_user_jump(&mut app, 'd', cursor(7, 0));
    }
    for content in 0..3 {
        app.select_for_test(cursor(0, content));
        assert_user_jump(&mut app, 'u', cursor(0, content));
        assert_user_jump(&mut app, 'd', cursor(7, 0));
    }
    for content in 0..2 {
        app.select_for_test(cursor(7, content));
        assert_user_jump(&mut app, 'd', cursor(7, content));
        assert_user_jump(&mut app, 'u', cursor(0, 0));
    }
    app.select_for_test(cursor(8, 0));
    assert_user_jump(&mut app, 'd', cursor(8, 0));
    assert_user_jump(&mut app, 'u', cursor(7, 0));
    assert_eq!(app.input(), "draft");
    assert_eq!(app.input_cursor(), 2);
    assert!(!app.view_follow());
}

#[test]
fn live_and_restored_user_jumps_include_both_ensemble_prompts_but_not_workers() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        for restored in [false, true] {
            let start = editable_ensemble_start("jump-ensemble", workflow, "ensemble prompt");
            let first = Message::User {
                content: vec![
                    UserContent::text("first"),
                    UserContent::text("second block"),
                ],
            };
            let answer = Message::assistant("answer");
            let last = Message::user("last prompt");
            let mut app = App::new();
            if restored {
                app.restore(vec![
                    TranscriptItem::Message(first),
                    TranscriptItem::Ensemble(EnsembleRecord::Started { start }),
                    TranscriptItem::Message(answer),
                    TranscriptItem::Message(last),
                ]);
            } else {
                app.reduce_without_effects(SessionEvent::TurnStarted {
                    turn_id: TEST_TURN_ID,
                    message: first,
                    mode: SessionMode::Build,
                });
                app.reduce_without_effects(SessionEvent::TurnRecovered {
                    turn_id: TEST_TURN_ID,
                    display_attempt_id: None,
                });
                app.reduce_without_effects(SessionEvent::EnsembleStarted {
                    turn_id: TurnId::new(2),
                    start,
                    resumed: false,
                });
                app.reduce_without_effects(SessionEvent::TurnCompleted {
                    turn_id: TurnId::new(2),
                    message: answer,
                    display_attempt_id: None,
                });
                app.reduce_without_effects(SessionEvent::TurnStarted {
                    turn_id: TurnId::new(3),
                    message: last,
                    mode: SessionMode::Build,
                });
            }
            app.detach_follow_for_test();
            app.select_for_test(cursor(0, 1));
            assert_user_jump(&mut app, 'd', cursor(1, 0));
            assert_user_jump(&mut app, 'd', cursor(3, 0));
            assert_user_jump(&mut app, 'u', cursor(1, 0));
            assert_user_jump(&mut app, 'u', cursor(0, 0));
            app.select_for_test(cursor(1, 1));
            assert_user_jump(&mut app, 'u', cursor(1, 0));
            app.select_for_test(cursor(1, 1));
            assert_user_jump(&mut app, 'd', cursor(3, 0));
            app.select_for_test(cursor(2, 0));
            assert_user_jump(&mut app, 'u', cursor(1, 0));
        }
    }
}

fn mixed_role_jump_events() -> Vec<AgentRunEvent> {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    vec![
        AgentRunEvent::Prompt {
            text: "first prompt".into(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Stderr {
            text: "between user blocks".into(),
        },
        AgentRunEvent::UserImage {
            image: image.clone(),
            message_id: None,
        },
        AgentRunEvent::UserMessage {
            text: "more first prompt".into(),
            message_id: None,
        },
        AgentRunEvent::AgentMessage {
            text: "first answer".into(),
            message_id: None,
        },
        AgentRunEvent::Stderr {
            text: "between roles".into(),
        },
        AgentRunEvent::UserMessage {
            text: "second prompt".into(),
            message_id: None,
        },
        AgentRunEvent::Stderr {
            text: "inside second prompt".into(),
        },
        AgentRunEvent::UserImage {
            image,
            message_id: None,
        },
        AgentRunEvent::UserMessage {
            text: "more second prompt".into(),
            message_id: None,
        },
        AgentRunEvent::AgentMessage {
            text: "second answer".into(),
            message_id: None,
        },
        AgentRunEvent::Prompt {
            text: "next entry".into(),
            continuation: true,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "last answer".into(),
            message_id: None,
        },
    ]
}

#[test]
fn mixed_role_acp_jumps_use_read_only_user_groups_and_ignore_roleless_diagnostics() {
    for diagnostics in [false, true] {
        let (mut app, mut transcript) = acp_transcript_app();
        for event in mixed_role_jump_events() {
            apply_agent_event(&mut app, &mut transcript, event);
        }
        if diagnostics {
            app.handle_event(key(KeyCode::Char('d')));
        }
        app.detach_follow_for_test();
        for (source, previous, next) in [
            ((0, 0), (0, 0), (0, 6)),
            ((0, 2), (0, 2), (0, 6)),
            ((0, 3), (0, 3), (0, 6)),
            ((0, 4), (0, 0), (0, 6)),
            ((0, 6), (0, 0), (1, 0)),
            ((0, 8), (0, 0), (1, 0)),
            ((0, 9), (0, 0), (1, 0)),
            ((0, 10), (0, 6), (1, 0)),
            ((1, 0), (0, 6), (1, 0)),
            ((1, 1), (1, 0), (1, 1)),
        ] {
            app.select_for_test(cursor(source.0, source.1));
            assert_user_jump(&mut app, 'u', cursor(previous.0, previous.1));
            assert!(!app.can_recall_selected(), "ACP user prompts are read-only");
            app.select_for_test(cursor(source.0, source.1));
            assert_user_jump(&mut app, 'd', cursor(next.0, next.1));
        }
        if diagnostics {
            for (index, previous, next) in [
                (1, (0, 0), (0, 6)),
                (5, (0, 0), (0, 6)),
                (7, (0, 6), (1, 0)),
            ] {
                app.select_for_test(cursor(0, index));
                assert_user_jump(&mut app, 'u', cursor(previous.0, previous.1));
                app.select_for_test(cursor(0, index));
                assert_user_jump(&mut app, 'd', cursor(next.0, next.1));
            }
        }
    }
}

#[test]
fn message_scope_user_jumps_skip_same_entry_targets() {
    for diagnostics in [false, true] {
        let (mut app, mut transcript) = acp_transcript_app();
        for event in mixed_role_jump_events() {
            apply_agent_event(&mut app, &mut transcript, event);
        }
        if diagnostics {
            app.handle_event(key(KeyCode::Char('d')));
        }
        for index in [0, 2, 3, 4, 6, 8, 9, 10] {
            app.select_message_for_test(cursor(0, index));
            assert_user_jump(&mut app, 'u', cursor(0, index));
            assert_user_jump(&mut app, 'd', cursor(1, 0));
        }
        app.select_message_for_test(cursor(1, 1));
        assert_user_jump(&mut app, 'u', cursor(0, 6));
        assert_user_jump(&mut app, 'd', cursor(1, 0));
        // Block scope can still reach a same-entry prompt once, then user
        // jumps switch back to message scope and skip the rest of that entry.
        app.select_for_test(cursor(0, 10));
        assert_user_jump(&mut app, 'u', cursor(0, 6));
        assert_user_jump(&mut app, 'u', cursor(0, 6));
        assert_user_jump(&mut app, 'd', cursor(1, 0));
    }
}

#[test]
fn message_scope_reconciles_cursor_when_its_block_is_hidden() {
    for scope in [SelectionScope::Message, SelectionScope::Block] {
        let mut app = App::new();
        app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![
                plain_block(0, PresentationRole::Assistant, "first"),
                plain_block(1, PresentationRole::Assistant, "last"),
            ],
        }));
        rendered_text(&mut app, 80, 20);
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(0, 1));
        if scope == SelectionScope::Block {
            app.handle_event(key(KeyCode::Enter));
        }
        let Some(HistoryEntry::Conversation(entry)) =
            app.conversation_projection_mut().entry_mut(0)
        else {
            panic!("conversation")
        };
        entry.blocks[1].visibility = BlockVisibility::Covered;
        assert!(
            app.apply_conversation_change(crate::app::ConversationChange::default())
                .is_empty()
        );
        match scope {
            SelectionScope::Message => {
                assert_eq!(app.selection(), cursor(0, 0));
                assert_eq!(app.selection_scope(), Some(scope));
                assert_eq!(
                    app.handle_event(key(KeyCode::Char('y'))),
                    Some(UiAction::Copy {
                        text: "first".into()
                    })
                );
                let Some(HistoryEntry::Conversation(entry)) =
                    app.conversation_projection_mut().entry_mut(0)
                else {
                    panic!("conversation")
                };
                entry.blocks[0].visibility = BlockVisibility::Diagnostics;
                assert!(
                    app.apply_conversation_change(crate::app::ConversationChange::default())
                        .is_empty()
                );
                assert_eq!(app.selection(), None, "no selectable blocks remain");
            }
            SelectionScope::Block => assert_eq!(
                app.selection(),
                None,
                "block scope does not repair its cursor"
            ),
        }
    }
}

#[test]
fn message_navigation_skips_entries_without_visible_selectable_blocks() {
    let mut app = App::new();
    let mut hidden = plain_block(0, PresentationRole::Assistant, "hidden");
    hidden.visibility = BlockVisibility::Diagnostics;
    app.seed_history_entry(history_message(Message::user("first")));
    app.seed_history_entry(HistoryEntry::CompactionDivider);
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![hidden.clone()],
    }));
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![
            hidden,
            plain_block(1, PresentationRole::Assistant, "visible"),
            plain_block(2, PresentationRole::Assistant, "last"),
        ],
    }));
    app.select_message_for_test(cursor(0, 0));
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(
        app.selection(),
        cursor(3, 1),
        "the cursor is the first visible block"
    );
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(3, 1));
    app.handle_event(key(KeyCode::Enter));
    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(3, 2));
    app.handle_event(key(KeyCode::Char('k')));
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.selection(), cursor(3, 1));
    app.handle_event(key(KeyCode::Esc));
    app.handle_event(key(KeyCode::Char('k')));
    assert_eq!(app.selection(), cursor(0, 0));
}

#[test]
fn live_and_restored_acp_projections_have_identical_user_jump_targets() {
    for restored in [false, true] {
        for diagnostics in [false, true] {
            let start =
                editable_ensemble_start("acp-jumps", EnsembleWorkflow::Review, "first prompt");
            let descriptor = start.agents[0].clone();
            let mut views = test_session_views(App::new());
            if restored {
                let mut records = historical_agent_records(&start, &descriptor);
                records.extend(
                    mixed_role_jump_events()
                        .into_iter()
                        .map(|event| AgentRunTranscriptRecord::Event { event }),
                );
                views.restore_agent_run(records);
            } else {
                views.apply(SessionEvent::EnsembleStarted {
                    turn_id: TEST_TURN_ID,
                    start: start.clone(),
                    resumed: false,
                });
                for event in mixed_role_jump_events() {
                    views.apply(SessionEvent::AgentRunUpdated {
                        turn_id: TEST_TURN_ID,
                        ensemble_run_id: start.run_id.clone(),
                        agent_run_id: descriptor.id.clone(),
                        event,
                    });
                }
            }
            views.handle_event(ctrl('i'));
            if diagnostics {
                views.handle_event(key(KeyCode::Char('d')));
            }
            rendered_views_text(&mut views, 180, 60);
            views.handle_event(key(KeyCode::Esc));
            views.handle_event(key(KeyCode::Esc));
            assert_eq!(
                views.agent(&descriptor.id).unwrap().selection(),
                cursor(1, 1)
            );
            for (chord, target) in [('u', (0, 6)), ('u', (0, 6)), ('d', (1, 0)), ('d', (1, 0))] {
                assert_eq!(views.handle_event(ctrl(chord)), None);
                let app = views.agent(&descriptor.id).unwrap();
                assert_eq!(app.selection(), cursor(target.0, target.1));
                assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
                assert!(!app.view_follow());
            }
        }
    }
}

#[test]
fn user_jump_targets_start_at_first_visible_user_content_not_diagnostics() {
    let mut hidden = plain_block(0, PresentationRole::User, "hidden prompt block");
    hidden.visibility = BlockVisibility::Covered;
    let mut hidden_assistant = plain_block(3, PresentationRole::Assistant, "covered answer");
    hidden_assistant.visibility = BlockVisibility::Covered;
    let mut diagnostic = plain_block(2, PresentationRole::User, "");
    diagnostic.kind = PresentationBlockKind::Diagnostic(crate::presentation::PresentedDiagnostic {
        label: "diagnostic".into(),
        text: "not user content".into(),
        tone: crate::presentation::DiagnosticTone::Muted,
    });
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![
            hidden,
            plain_block(1, PresentationRole::User, "visible"),
            diagnostic,
            hidden_assistant,
            plain_block(4, PresentationRole::User, "same group"),
            plain_block(5, PresentationRole::Assistant, "answer"),
        ],
    }));
    app.select_for_test(cursor(0, 5));
    assert_user_jump(&mut app, 'u', cursor(0, 1));
    app.select_for_test(cursor(0, 4));
    assert_user_jump(&mut app, 'u', cursor(0, 4));
    assert_user_jump(&mut app, 'd', cursor(0, 4));
}

#[test]
fn successful_user_jumps_reveal_off_screen_targets_with_oversized_directionality() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
    app.seed_history_entry(history_message(Message::assistant(numbered_lines(30))));
    app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
    app.seed_history_entry(history_message(Message::assistant("last answer")));
    rendered_text(&mut app, 80, 10);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(3, 0));
    for (chord, target, upward) in [
        ('u', (2, 0), true),
        ('u', (0, 0), true),
        ('d', (2, 0), false),
    ] {
        assert_user_jump(&mut app, chord, cursor(target.0, target.1));
        assert!(app.interaction().selection_reveal());
        rendered_text(&mut app, 80, 10);
        let entries = app.view_cache().entries();
        let offset: usize = entries[..target.0].iter().map(|entry| entry.extent()).sum();
        let range = entries[target.0].selection.unwrap().shifted(offset);
        assert_eq!(
            app.view_scroll(),
            if upward {
                range.start()
            } else {
                range.end() - 6
            }
        );
        assert!(!app.view_follow());
    }
    app.handle_event(key(KeyCode::Esc));
    assert!(app.interaction().is_normal());
}

#[test]
fn boundary_user_jumps_and_ignored_page_keys_do_not_request_reveal_or_move_view() {
    for user in [false, true] {
        let mut app = App::new();
        let text = numbered_lines(30);
        app.seed_history_entry(history_message(if user {
            Message::user(text)
        } else {
            Message::assistant(text)
        }));
        app.set_view_for_test(10, false);
        rendered_text(&mut app, 80, 10);
        double_escape(&mut app);
        for event in [ctrl('u'), ctrl('d'), ctrl('b'), ctrl('f')] {
            app.handle_event(event);
            assert_eq!(app.selection(), cursor(0, 0));
            assert!(!app.interaction().selection_reveal());
            rendered_text(&mut app, 80, 10);
            assert_eq!(app.view_scroll(), 10);
            assert!(!app.view_follow());
        }
        app.handle_event(key(KeyCode::PageUp));
        assert_eq!(app.view_scroll(), 4);
        assert_eq!(app.selection(), cursor(0, 0));
        assert!(!app.interaction().selection_reveal());
        app.handle_event(key(KeyCode::PageDown));
        assert_eq!(app.view_scroll(), 10);
    }
}

#[test]
fn fitting_user_jumps_reveal_both_directions_without_following_the_tail() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("first prompt")));
    app.seed_history_entry(history_message(Message::assistant(numbered_lines(30))));
    app.seed_history_entry(history_message(Message::user("last prompt")));
    rendered_text(&mut app, 80, 10);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(2, 0));
    for (chord, target, text) in [('u', (0, 0), "first prompt"), ('d', (2, 0), "last prompt")] {
        assert_user_jump(&mut app, chord, cursor(target.0, target.1));
        assert!(app.interaction().selection_reveal());
        let rendered = rendered_text(&mut app, 80, 10);
        assert!(rendered.contains(text));
        assert!(!app.view_follow());
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy { text: text.into() })
        );
        let before = app.view_scroll();
        // At either boundary a second jump is a no-op even after reveal.
        assert_user_jump(&mut app, chord, cursor(target.0, target.1));
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), before);
    }
}

#[test]
fn boundary_user_jumps_clear_pending_tool_yank_chords() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            turn_id: TEST_TURN_ID,
            display_attempt_id: None,
            message: assistant_message(vec![tool_call(
                "yank-jump",
                None,
                "command",
                json!({"command":"pwd"}),
            )]),
        },
    );
    app.reduce_without_effects(SessionEvent::ToolResults {
        turn_id: TEST_TURN_ID,
        message: tool_result_message("yank-jump", None, "command", "output"),
        metadata: vec![],
    });
    app.select_for_test(cursor(0, 0));
    let now = Instant::now();
    let expected = Some(UiAction::Copy {
        text: "{\"command\":\"pwd\"}".into(),
    });
    assert_eq!(app.handle_event_at(key(KeyCode::Char('y')), now), expected);
    app.handle_event_at(ctrl('u'), now + Duration::from_millis(10));
    assert_eq!(
        app.handle_event_at(key(KeyCode::Char('y')), now + Duration::from_millis(20)),
        expected
    );
}

#[test]
fn selection_shortcuts_select_the_bottom_visible_item_not_the_transcript_tail() {
    for enter_selection in SELECTION_SHORTCUTS {
        let mut app = App::new();
        for index in 0..8 {
            app.seed_history_entry(history_message(Message::user(format!("item {index}"))));
        }
        app.set_input_for_test("draft", 2);
        app.set_view_for_test(3, false);
        let before = rendered_buffer(&mut app, 80, 10);
        assert_eq!(app.rendered_selection_window(), Some(RowRange::new(3, 9)));
        let rebuilds = app.view_cache().rebuilds;
        enter_selection(&mut app);
        assert_eq!(
            app.view_cache().rebuilds,
            rebuilds,
            "input queries cached geometry without rebuilding"
        );
        assert_eq!(app.selection(), cursor(2, 0));
        assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
        assert_eq!(app.input(), "draft");
        assert_eq!(app.input_cursor(), 2);
        assert!(!app.view_follow());
        assert!(!app.interaction().selection_reveal());
        let after = rendered_buffer(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), 3);
        let content = conversation_content_area(&after, false);
        for y in content.y..content.bottom() {
            assert_eq!(buffer_row_text(&before, y), buffer_row_text(&after, y));
        }
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: "item 2".into()
            })
        );
    }
}

#[test]
fn bottom_clipped_block_wins_over_a_later_block_in_the_same_message() {
    for enter_selection in SELECTION_SHORTCUTS {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::User {
            content: (0..3)
                .map(|_| UserContent::Text(Text::new(numbered_lines(4))))
                .collect(),
        }));
        app.set_view_for_test(0, false);
        rendered_text(&mut app, 80, 10);
        assert_eq!(
            app.view_cache().entries()[0].items,
            vec![
                (0, RowRange::new(1, 5)),
                (1, RowRange::new(5, 9)),
                (2, RowRange::new(9, 13)),
            ]
        );
        enter_selection(&mut app);
        assert_eq!(app.selection(), cursor(0, 1));
        assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
        let message_buffer = rendered_buffer(&mut app, 80, 10);
        let content = conversation_content_area(&message_buffer, false);
        assert_eq!(
            message_buffer[(content.x, content.bottom() - 2)].bg,
            SELECTION_BG
        );
        app.handle_event(key(KeyCode::Enter));
        let buffer = rendered_buffer(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), 0);
        let content = conversation_content_area(&buffer, false);
        assert_eq!(buffer[(content.x, content.bottom() - 1)].bg, SELECTION_BG);
        assert_ne!(buffer[(content.x, content.bottom() - 2)].bg, SELECTION_BG);

        app.handle_event(key(KeyCode::Down));
        assert_eq!(app.selection(), cursor(0, 2));
        assert!(app.interaction().selection_reveal());
        rendered_text(&mut app, 80, 10);
        assert_eq!(
            app.view_scroll(),
            7,
            "explicit navigation reveals the fitting item"
        );
    }
}

#[test]
fn successful_entry_detaches_follow_without_moving_the_bottom_view() {
    for enter_selection in SELECTION_SHORTCUTS {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
        rendered_text(&mut app, 80, 10);
        let scroll = app.view_scroll();
        assert!(app.view_follow());
        assert!(scroll > 0);
        enter_selection(&mut app);
        assert_eq!(app.selection(), cursor(0, 0));
        assert!(!app.view_follow());
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), scroll);
    }
}

#[test]
fn oversized_entry_stays_put_across_redraws_copy_ignored_keys_and_native_stream_updates() {
    for enter_selection in SELECTION_SHORTCUTS {
        let mut app = App::new();
        let text = numbered_lines(30);
        app.seed_history_entry(history_message(Message::user(text.clone())));
        start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
        app.set_view_for_test(10, false);
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.rendered_selection_window(), Some(RowRange::new(10, 16)));
        assert!(app.is_busy());
        enter_selection(&mut app);
        assert_eq!(app.selection(), cursor(0, 0));
        for _ in 0..3 {
            rendered_text(&mut app, 80, 10);
            assert_eq!(app.view_scroll(), 10);
        }
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy { text })
        );
        app.handle_event(key(KeyCode::Char('x')));
        app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::assistant("streamed background text")).into(),
        });
        assert!(
            app.apply_conversation_change(crate::app::ConversationChange::default())
                .is_empty()
        );
        for _ in 0..3 {
            rendered_text(&mut app, 80, 10);
            assert_eq!(app.view_scroll(), 10);
            assert!(!app.interaction().selection_reveal());
        }
    }
}

#[test]
fn v_while_selecting_does_not_reset_the_cursor_scope_or_reveal_intent() {
    for block_scope in [false, true] {
        for navigate in [false, true] {
            let mut app = App::new();
            app.seed_history_entry(history_message(Message::user("first")));
            app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
            app.seed_history_entry(history_message(Message::user("off-screen tail")));
            app.set_view_for_test(10, false);
            rendered_text(&mut app, 80, 10);
            press_v(&mut app);
            assert_eq!(app.selection(), cursor(1, 0));
            assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
            if block_scope {
                app.handle_event(key(KeyCode::Enter));
            }
            if navigate {
                app.handle_event(key(KeyCode::Up));
            }
            let before = (
                app.interaction().active_selection(),
                app.interaction().selection_reveal(),
                app.view_scroll(),
                app.view_follow(),
                app.view_cache().rebuilds,
            );
            assert_eq!(app.handle_event(key(KeyCode::Char('v'))), None);
            assert_eq!(
                (
                    app.interaction().active_selection(),
                    app.interaction().selection_reveal(),
                    app.view_scroll(),
                    app.view_follow(),
                    app.view_cache().rebuilds,
                ),
                before
            );
        }
    }
}

#[test]
fn clamped_navigation_requests_revelation_in_both_directions() {
    for navigation in [
        KeyCode::Char('j'),
        KeyCode::Down,
        KeyCode::Char('k'),
        KeyCode::Up,
    ] {
        for top in [0, 10] {
            let mut app = App::new();
            app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
            app.set_view_for_test(top, false);
            rendered_text(&mut app, 80, 10);
            double_escape(&mut app);
            assert!(!app.interaction().selection_reveal());
            app.handle_event(key(navigation));
            assert_eq!(app.selection(), cursor(0, 0), "navigation clamps");
            assert!(app.interaction().selection_reveal());
            rendered_text(&mut app, 80, 10);
            assert_eq!(
                app.view_scroll(),
                if top == 0 { 25 } else { 1 },
                "retain the oversized-item reveal algorithm for {navigation:?}"
            );
            assert!(!app.view_follow());
        }
    }
    // A fitting last item clipped by the bottom also reveals on clamped j.
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::User {
        content: vec![
            UserContent::text(numbered_lines(4)),
            UserContent::text(numbered_lines(4)),
        ],
    }));
    app.set_view_for_test(0, false);
    rendered_text(&mut app, 80, 10);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(0, 1));
    app.handle_event(key(KeyCode::Enter));
    app.handle_event(key(KeyCode::Char('j')));
    rendered_text(&mut app, 80, 10);
    assert_eq!(app.view_scroll(), 3);
}

#[test]
fn cached_ranges_exclude_headers_separators_gaps_dividers_and_hidden_blocks() {
    let mut diagnostic = plain_block(1, PresentationRole::User, "hidden diagnostic");
    diagnostic.visibility = BlockVisibility::Diagnostics;
    let history = vec![
        HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![diagnostic.clone()],
        }),
        HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![
                plain_block(0, PresentationRole::User, "first"),
                diagnostic,
                plain_block(2, PresentationRole::Assistant, "second"),
                plain_block(3, PresentationRole::Assistant, ""),
            ],
        }),
        HistoryEntry::CompactionDivider,
        HistoryEntry::Error("failure".into()),
    ];
    let mut cache = ConversationCache::default();
    cache.refresh(&history, None, 80, false, &crate::app::FoldState::default());
    assert_eq!(cache.entries()[0].extent(), 0);
    assert!(cache.entries()[0].items.is_empty());
    assert_eq!(
        cache.entries()[1].items,
        vec![
            (0, RowRange::new(1, 2)),
            (2, RowRange::new(4, 5)),
            (3, RowRange::new(5, 6)),
        ]
    );
    assert!(
        cache
            .entries()
            .iter()
            .all(|entry| entry.selection.is_none())
    );
    let expected = [
        None,
        cursor(1, 0),
        None,
        None,
        cursor(1, 2),
        cursor(1, 3),
        None,
        None,
        None,
        cursor(3, 0),
        cursor(3, 0),
        None,
    ];
    for (row, selection) in expected.into_iter().enumerate() {
        assert_eq!(
            cache.selection_at_bottom(RowRange::from_start_len(row, 1)),
            selection,
            "row {row}"
        );
        assert_eq!(
            cache.selection_at_bottom(RowRange::new(row, row)),
            None,
            "empty window at {row}"
        );
    }
    assert_eq!(cache.selection_at_bottom(RowRange::new(1, 5)), cursor(1, 2));
    assert_eq!(
        cache.selection_at_bottom(RowRange::new(2, 4)),
        None,
        "exact half-open boundaries"
    );
    let items = cache.entries()[1].items.clone();
    cache.refresh(
        &history,
        cursor(1, 2).map(|selection| ActiveSelection {
            selection,
            scope: SelectionScope::Block,
        }),
        80,
        false,
        &crate::app::FoldState::default(),
    );
    assert_eq!(cache.entries()[1].items, items);
    assert_eq!(cache.entries()[1].selection, Some(items[1].1));
}

#[test]
fn selection_shortcuts_without_a_usable_prior_render_stay_unselected() {
    let mut app = App::new();
    assert_no_target(&mut app);
    app.seed_history_entry(history_message(Message::user("not rendered")));
    assert_no_target(&mut app);
    for (width, height) in [(0, 10), (1, 10), (2, 10), (80, 0)] {
        rendered_text(&mut app, width, height);
        assert_eq!(app.rendered_selection_window(), None, "{width}x{height}");
        assert_no_target(&mut app);
    }
    let mut empty = App::new();
    rendered_text(&mut empty, 80, 10);
    assert_no_target(&mut empty);
}

#[test]
fn single_row_headers_separators_and_dividers_do_not_substitute_nearby_items() {
    for row in [0, 2, 3, 5, 6, 7, 8] {
        let mut app = App::subtask_inspect("one-row pane");
        app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: vec![
                plain_block(0, PresentationRole::User, "first"),
                plain_block(1, PresentationRole::Assistant, "second"),
            ],
        }));
        app.seed_history_entry(HistoryEntry::CompactionDivider);
        app.seed_history_entry(history_message(Message::user("last")));
        app.set_view_for_test(row, false);
        rendered_text(&mut app, 80, 1);
        assert_eq!(
            app.rendered_selection_window(),
            Some(RowRange::from_start_len(row, 1))
        );
        assert_no_target(&mut app);
    }
}

#[test]
fn nonselectable_only_windows_do_not_fall_back_or_detach_follow() {
    let mut divider = App::new();
    divider.seed_history_entry(HistoryEntry::CompactionDivider);
    rendered_text(&mut divider, 80, 10);
    assert_no_target(&mut divider);

    for streaming in [false, true] {
        let mut app = App::new();
        if streaming {
            app.seed_history_entry(history_message(Message::user("committed but off screen")));
        }
        start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
        if streaming {
            app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
                turn_id: TEST_TURN_ID,
                snapshot: (Message::assistant(numbered_lines(30))).into(),
            });
        }
        rendered_text(&mut app, 80, 10);
        if streaming {
            assert!(
                app.rendered_selection_window().unwrap().start()
                    >= app.view_cache().entries()[0].extent()
            );
        }
        assert_no_target(&mut app);
        assert!(app.view_follow());
    }
}

#[test]
fn invalidated_geometry_is_unavailable_until_redraw_without_losing_cached_blocks() {
    for mutation in 0..6 {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user(numbered_lines(30))));
        enter_insert(&mut app);
        rendered_text(&mut app, 80, 10);
        let rebuilt = app.view_cache().block_rebuilds;
        match mutation {
            0 => {
                app.handle_event(Event::Resize(60, 12));
            }
            1 => {
                app.handle_event(Event::Paste("a\nb\nc".into()));
            }
            2 => {
                app.handle_event(key(KeyCode::Esc));
                app.handle_event(key(KeyCode::PageUp));
            }
            3 => app.push_error("new notice".into()),
            4 => {
                assert!(
                    app.apply_conversation_change(crate::app::ConversationChange::default())
                        .is_empty()
                );
            }
            5 => app.restore(vec![TranscriptItem::Message(Message::user("replacement"))]),
            _ => unreachable!(),
        }
        assert_eq!(app.rendered_selection_window(), None, "mutation {mutation}");
        assert_no_target(&mut app);
        rendered_text(&mut app, 80, 10);
        assert!(app.rendered_selection_window().is_some());
        if mutation < 5 {
            assert_eq!(
                app.view_cache().block_rebuilds,
                rebuilt,
                "measurement invalidation preserves blocks"
            );
        }
        for enter_selection in SELECTION_SHORTCUTS {
            enter_selection(&mut app);
            assert!(app.selection().is_some());
            app.handle_event(key(KeyCode::Esc));
        }
    }
}

#[test]
fn native_and_presentation_families_keep_their_copy_actions_on_visible_entry() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let mut placeholder = plain_block(0, PresentationRole::User, "");
    placeholder.kind = PresentationBlockKind::Placeholder("[attachment]".into());
    let mut image_block = placeholder.clone();
    image_block.kind = PresentationBlockKind::Image {
        image: image.clone(),
        ordinal: 1,
        editable: true,
    };
    let artifact = test_plan_artifact();
    let entries = vec![
        (
            history_message(Message::user("plain")),
            "plain".into(),
            false,
        ),
        (
            history_message(Message::assistant("**markdown**")),
            "**markdown**".into(),
            false,
        ),
        (
            history_message(Message::assistant("")),
            String::new(),
            false,
        ),
        (
            history_message(mixed_reasoning_message()),
            format!("{READABLE_REASONING_SUMMARY}\n{READABLE_REASONING_TEXT}"),
            false,
        ),
        (
            history_message(assistant_message(vec![tool_call(
                "call",
                None,
                "command",
                json!({"command":"pwd"}),
            )])),
            "{\"command\":\"pwd\"}".into(),
            false,
        ),
        (
            HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: vec![placeholder],
            }),
            "[attachment]".into(),
            false,
        ),
        (
            HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: vec![image_block],
            }),
            image.label(1),
            false,
        ),
        (
            HistoryEntry::PlanArtifact(artifact.clone()),
            artifact.markdown.clone(),
            true,
        ),
        (
            HistoryEntry::PlanHandoff(PlanHandoff::new(artifact.clone(), "source"), None),
            artifact.markdown,
            true,
        ),
        (
            HistoryEntry::Error("failure".into()),
            "failure".into(),
            true,
        ),
    ];
    for (entry, copy, whole_entry) in entries {
        let mut app = App::new();
        app.seed_history_entry(entry);
        app.set_view_for_test(0, false);
        rendered_text(&mut app, 80, 10);
        let layout = &app.view_cache().entries()[0];
        assert_eq!(layout.items.len(), 1);
        assert!(!layout.items[0].1.is_empty());
        if whole_entry {
            assert_eq!(
                layout.items[0].1,
                RowRange::from_start_len(0, layout.height)
            );
            assert_eq!(
                app.view_cache().selection_at_bottom(RowRange::new(0, 1)),
                cursor(0, 0)
            );
        }
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(0, 0));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy { text: copy })
        );
        rendered_text(&mut app, 80, 10);
        assert_eq!(app.view_scroll(), 0);
    }
}

#[test]
fn ensemble_prompt_and_wrapped_worker_ranges_exclude_confirmation_summaries() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let start = editable_ensemble_start("visible-ensemble", workflow, "");
        let worker_id = start.agents[0].id.clone();
        let mut app = App::new();
        app.reduce_without_effects(SessionEvent::EnsembleStarted {
            turn_id: TEST_TURN_ID,
            start: start.clone(),
            resumed: false,
        });
        app.reduce_without_effects(SessionEvent::AgentRunUpdated {
            turn_id: TEST_TURN_ID,
            ensemble_run_id: start.run_id,
            agent_run_id: worker_id.clone(),
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Failed,
                detail: Some(
                    "a failure whose details wrap over several rows in a narrow pane".into(),
                ),
            },
        });
        rendered_text(&mut app, 36, 10);
        let entry = &app.view_cache().entries()[0];
        assert_eq!(entry.items.len(), 2);
        let prompt = entry.items[0].1;
        let worker = entry.items[1].1;
        assert_eq!(
            prompt,
            RowRange::new(1, 2),
            "empty prompt has a placeholder"
        );
        assert!(worker.len() > 2, "worker includes wrapped failure details");
        assert_eq!(
            app.view_cache()
                .selection_at_bottom(RowRange::new(worker.end() - 1, worker.end())),
            cursor(0, 1)
        );
        if workflow == EnsembleWorkflow::Plan {
            assert!(worker.start() > prompt.end());
            assert_eq!(
                app.view_cache()
                    .selection_at_bottom(RowRange::new(prompt.end(), worker.start())),
                None
            );
        }
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(0, 1));
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::OpenAgentRun { id: worker_id })
        );
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Esc));
        if workflow == EnsembleWorkflow::Plan {
            app.set_view_for_test(prompt.end(), false);
            rendered_text(&mut app, 36, 1);
            assert_no_target(&mut app);
        }
        app.set_view_for_test(prompt.start(), false);
        rendered_text(&mut app, 36, 1);
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(0, 0));
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: format!("{} ", workflow.slash_command())
            })
        );
    }
}

#[test]
fn acp_updates_preserve_no_reveal_and_diagnostic_toggles_require_a_fresh_render() {
    for enter_selection in SELECTION_SHORTCUTS {
        let (mut app, mut transcript) = acp_transcript_app();
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::Prompt {
                text: "prompt".into(),
                continuation: false,
                repair: None,
            },
        );
        apply_agent_preview(
            &mut app,
            &mut transcript,
            AgentRunEvent::AgentMessage {
                text: numbered_lines(30),
                message_id: Some("answer".into()),
            },
        );
        app.set_view_for_test(10, false);
        rendered_text(&mut app, 80, 10);
        enter_selection(&mut app);
        let selected = app.selection();
        assert!(selected.is_some());
        apply_agent_preview(
            &mut app,
            &mut transcript,
            AgentRunEvent::AgentMessage {
                text: numbered_lines(35),
                message_id: Some("answer".into()),
            },
        );
        for _ in 0..3 {
            rendered_text(&mut app, 80, 10);
            assert_eq!(app.selection(), selected);
            assert!(!app.interaction().selection_reveal());
            assert_eq!(app.view_scroll(), 10);
        }
        app.handle_event(key(KeyCode::Esc));
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::Stderr {
                text: "visible diagnostic".into(),
            },
        );
        app.handle_event(key(KeyCode::End));
        rendered_text(&mut app, 80, 10);
        let hidden = app
            .view_cache()
            .selection_at_bottom(app.rendered_selection_window().unwrap());
        app.handle_event(key(KeyCode::Char('d')));
        assert_no_target(&mut app);
        rendered_text(&mut app, 80, 10);
        enter_selection(&mut app);
        assert_ne!(app.selection(), hidden);
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: "visible diagnostic".into()
            })
        );
    }
}

#[test]
fn resized_and_reallocated_panes_query_their_actual_content_rectangle() {
    for enter_selection in SELECTION_SHORTCUTS {
        for mut app in [
            App::new(),
            App::subtask_inspect("child"),
            App::acp_inspect("agent"),
        ] {
            app.seed_history_entry(history_message(Message::user(numbered_lines(60))));
            for (width, height) in [(80, 10), (24, 16), (100, 24)] {
                app.set_view_for_test(8, false);
                let buffer = rendered_buffer(&mut app, width, height);
                let content = conversation_content_area(&buffer, app.inspect_only());
                assert_eq!(
                    app.rendered_selection_window(),
                    Some(RowRange::from_start_len(8, usize::from(content.height)))
                );
                enter_selection(&mut app);
                assert_eq!(app.selection(), cursor(0, 0));
                assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
                assert!(!app.interaction().selection_reveal());
                assert!(!app.view_follow());
                rendered_text(&mut app, width, height);
                assert_eq!(app.view_scroll(), 8);
                app.handle_event(key(KeyCode::Esc));
            }
        }
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user(numbered_lines(60))));
        app.set_view_for_test(8, false);
        rendered_text(&mut app, 80, 16);
        let short = app.rendered_selection_window().unwrap();
        app.set_input_for_test("one\ntwo\nthree\nfour\nfive", 0);
        rendered_text(&mut app, 80, 16);
        assert!(app.rendered_selection_window().unwrap().len() < short.len());
        enter_selection(&mut app);
        rendered_text(&mut app, 80, 16);
        assert_eq!(app.view_scroll(), 8);
    }
}
