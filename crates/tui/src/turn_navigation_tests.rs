//! Normal-mode semantic turn jumps, including actual first-row painting.

use super::selection_tests::{mixed_role_jump_events, numbered_lines};
use super::*;
use crate::app::TurnStartTarget;
use ratatui::{buffer::Buffer, layout::Rect};

fn jump(app: &mut App, ch: char) {
    assert_eq!(app.handle_event(key(KeyCode::Char(ch))), None);
}

fn draw(app: &mut App, width: u16, height: u16) -> (Buffer, Rect) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut content = Rect::default();
    terminal
        .draw(|frame| {
            content = app.render_surface(frame, false, false).conversation_content;
        })
        .unwrap();
    (terminal.backend().buffer().clone(), content)
}

fn assert_first_row(app: &mut App, width: u16, height: u16, header: &str) {
    let (buffer, content) = draw(app, width, height);
    assert!(content.height > 0);
    let first = buffer_row_text(&buffer, content.y);
    assert!(
        first.contains(header),
        "expected {header:?} in first conversation row: {first:?}"
    );
}

fn positions(app: &mut App) -> Vec<(TurnStartTarget, usize)> {
    let targets = app.render_parts().turn_starts;
    app.view_cache().turn_start_positions(&targets)
}

fn long_conversation() -> App {
    let mut app = App::new();
    app.restore(
        (1..=4)
            .flat_map(|turn| {
                [
                    TranscriptItem::Message(Message::user(format!(
                        "prompt {turn}\n{}",
                        numbered_lines(12)
                    ))),
                    TranscriptItem::Message(Message::assistant(numbered_lines(20))),
                ]
            })
            .collect(),
    );
    app.set_view_for_test(0, false);
    app
}

#[test]
fn strict_turn_navigation_handles_mid_turn_boundaries_gaps_and_batched_keys() {
    let mut app = long_conversation();
    app.set_input_for_test("unsubmitted draft", 5);
    draw(&mut app, 60, 12);
    let rows = positions(&mut app)
        .into_iter()
        .map(|(_, row)| row)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);

    jump(&mut app, '[');
    assert_eq!(app.view_scroll(), 0, "no wrap before the first turn");
    jump(&mut app, ']');
    assert_eq!(app.view_scroll(), rows[1]);
    jump(&mut app, ']');
    assert_eq!(
        app.view_scroll(),
        rows[2],
        "successive keys work before a redraw"
    );
    jump(&mut app, '[');
    assert_eq!(
        app.view_scroll(),
        rows[1],
        "an exact boundary skips the current start"
    );
    assert_first_row(&mut app, 60, 12, "You · #2");

    app.set_view_for_test(rows[1] + 3, false);
    draw(&mut app, 60, 12);
    jump(&mut app, '[');
    assert_eq!(
        app.view_scroll(),
        rows[1],
        "first return to the current turn's beginning"
    );
    jump(&mut app, '[');
    assert_eq!(app.view_scroll(), rows[0]);
    assert_first_row(&mut app, 60, 12, "You · #1");

    let gap = app.view_cache().entries()[0].height;
    app.set_view_for_test(gap, false);
    draw(&mut app, 60, 12);
    jump(&mut app, '[');
    assert_eq!(app.view_scroll(), 0, "separators are not destinations");
    app.set_view_for_test(gap, false);
    draw(&mut app, 60, 12);
    jump(&mut app, ']');
    assert_eq!(
        app.view_scroll(),
        rows[1],
        "assistant messages are not destinations"
    );
    jump(&mut app, ']');
    jump(&mut app, ']');
    jump(&mut app, ']');
    assert_eq!(app.view_scroll(), rows[3], "no wrap after the last turn");
    assert_first_row(&mut app, 60, 12, "You · #4");
    assert!(!app.view_follow());
    assert!(app.interaction().is_normal());
    assert_eq!(app.selection(), None);
    assert_eq!(app.input(), "unsubmitted draft");
    assert_eq!(app.input_cursor(), 5);
}

#[test]
fn final_and_short_conversations_align_headers_with_view_only_blank_space() {
    for long_prefix in [false, true] {
        let mut app = App::new();
        app.restore(vec![
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant(if long_prefix {
                numbered_lines(40)
            } else {
                "answer".into()
            })),
            TranscriptItem::Message(Message::user("last")),
        ]);
        app.set_view_for_test(0, false);
        draw(&mut app, 80, 24);
        let last = positions(&mut app)[1].1;
        jump(&mut app, ']');
        let (buffer, content) = draw(&mut app, 80, 24);
        assert!(buffer_row_text(&buffer, content.y).contains("You · #2"));
        assert!(buffer_row_text(&buffer, content.y + 1).contains("last"));
        for y in content.y + 2..content.bottom() {
            let text = (content.x..content.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            assert!(
                text.trim().is_empty(),
                "expected trailing blank conversation space: {text:?}"
            );
        }
        assert_eq!(app.view_scroll(), last);
        assert!(!app.view_follow());
        jump(&mut app, ']');
        assert_eq!((app.view_scroll(), app.view_follow()), (last, false));
        jump(&mut app, 'G');
        draw(&mut app, 80, 24);
        let actual = app
            .view_cache()
            .entries()
            .iter()
            .map(|entry| entry.extent())
            .sum::<usize>();
        assert_eq!(
            app.view_scroll(),
            actual.saturating_sub(usize::from(content.height))
        );
        assert!(
            app.view_follow(),
            "G must discard virtual padding and resume ordinary follow"
        );
    }
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(Message::user("only"))]);
    draw(&mut app, 80, 24);
    for ch in ['[', ']'] {
        jump(&mut app, ch);
        assert_eq!(
            (app.view_scroll(), app.view_follow()),
            (0, true),
            "no-op preserves follow"
        );
    }
}

#[test]
fn native_prompt_kinds_are_destinations_live_and_restored_but_metadata_is_not() {
    let skill = SkillInvocation::new(
        SkillName::parse("commit").unwrap(),
        "ship it",
        SkillApplication::Activate(
            SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Instructions").unwrap(),
        ),
    );
    let request =
        zevria_foundation::RequestMetadata::new(zevria_foundation::RequestBehavior::Orchestrate);
    let handoff = PlanHandoff::new(test_plan_artifact(), "source");
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let start = editable_ensemble_start("navigation-ensemble", workflow, "ensemble prompt");
        for restored in [false, true] {
            let prompts = [
                Message::user("ordinary"),
                Message::user("/orchestrate work"),
                skill.display_message(),
            ];
            let image_prompt = Message::User {
                content: vec![image.to_user_content()],
            };
            let mut app = App::new();
            if restored {
                app.restore(vec![
                    TranscriptItem::Message(prompts[0].clone()),
                    TranscriptItem::Message(Message::assistant("answer")),
                    TranscriptItem::RequestPrompt {
                        message: Message::user("work"),
                        request: request.clone(),
                    },
                    TranscriptItem::RequestDirective(
                        zevria_instructions::RequestDirective::boundary(request.clone()),
                    ),
                    TranscriptItem::SkillInvocation(skill.clone()),
                    TranscriptItem::Message(assistant_message(vec![tool_call(
                        "call",
                        None,
                        "command",
                        json!({}),
                    )])),
                    TranscriptItem::Message(tool_result_message("call", None, "command", "result")),
                    TranscriptItem::Error {
                        error: "diagnostic".into(),
                    },
                    TranscriptItem::Compaction(checkpoint()),
                    TranscriptItem::SessionMode(SessionMode::Plan),
                    TranscriptItem::Plan(PlanRecord::Ready {
                        artifact: test_plan_artifact(),
                    }),
                    TranscriptItem::Plan(PlanRecord::Handoff {
                        handoff: handoff.clone(),
                    }),
                    TranscriptItem::Ensemble(EnsembleRecord::Started {
                        start: start.clone(),
                    }),
                    TranscriptItem::Message(image_prompt),
                ]);
            } else {
                for (index, message) in prompts.into_iter().enumerate() {
                    let turn_id = TurnId::new(index as u64 + 1);
                    app.reduce_without_effects(SessionEvent::TurnStarted {
                        turn_id,
                        message,
                        mode: SessionMode::Build,
                    });
                    app.reduce_without_effects(SessionEvent::TurnCompleted {
                        turn_id,
                        message: Message::assistant("answer"),
                        display_attempt_id: None,
                    });
                }
                app.seed_history_entry(HistoryEntry::PlanArtifact(test_plan_artifact()));
                app.seed_history_entry(HistoryEntry::Error("diagnostic".into()));
                app.seed_history_entry(HistoryEntry::CompactionDivider);
                app.reduce_without_effects(SessionEvent::PlanHandoffStarted {
                    turn_id: TurnId::new(4),
                    handoff: handoff.clone(),
                });
                app.reduce_without_effects(SessionEvent::TurnCompleted {
                    turn_id: TurnId::new(4),
                    message: Message::assistant("implemented"),
                    display_attempt_id: None,
                });
                app.reduce_without_effects(SessionEvent::EnsembleStarted {
                    turn_id: TurnId::new(5),
                    start: start.clone(),
                    resumed: false,
                });
                app.reduce_without_effects(SessionEvent::TurnRecovered {
                    turn_id: TurnId::new(5),
                    display_attempt_id: None,
                });
                app.reduce_without_effects(SessionEvent::TurnStarted {
                    turn_id: TurnId::new(6),
                    message: image_prompt,
                    mode: SessionMode::Build,
                });
            }
            app.set_view_for_test(0, false);
            draw(&mut app, 100, 20);
            assert_eq!(
                positions(&mut app).len(),
                6,
                "{workflow:?}, restored={restored}"
            );
            for (index, label) in [
                "You",
                "You",
                "You",
                "Approved Plan handoff",
                &workflow.to_string(),
                "You",
            ]
            .into_iter()
            .enumerate()
            {
                if index > 0 {
                    jump(&mut app, ']');
                }
                assert_first_row(&mut app, 100, 20, &format!("{label} · #{}", index + 1));
            }
            jump(&mut app, '[');
            jump(&mut app, '[');
            assert_first_row(&mut app, 100, 20, "Approved Plan handoff · #4");
        }
    }
}

#[test]
fn mixed_role_acp_groups_include_images_and_noneditable_prompts_not_diagnostics() {
    for diagnostics in [false, true] {
        let (mut app, mut transcript) = acp_transcript_app();
        for event in mixed_role_jump_events() {
            apply_agent_event(&mut app, &mut transcript, event);
        }
        if diagnostics {
            jump(&mut app, 'd');
        }
        app.set_view_for_test(0, false);
        draw(&mut app, 35, 15);
        let targets = positions(&mut app);
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets
                .iter()
                .map(|(target, _)| target.history_index)
                .collect::<Vec<_>>(),
            [0, 0, 1]
        );
        assert!(targets.iter().all(|(target, _)| target.block.is_some()));
        jump(&mut app, ']');
        let (buffer, content) = draw(&mut app, 35, 15);
        assert!(
            buffer_row_text(&buffer, content.y).contains("You"),
            "exclude the group's leading separator"
        );
        assert!(buffer_row_text(&buffer, content.y + 1).contains("second prompt"));
        jump(&mut app, ']');
        assert_first_row(&mut app, 35, 15, "You");
        assert_eq!(app.view_scroll(), targets[2].1);
        jump(&mut app, '[');
        assert_eq!(app.view_scroll(), targets[1].1);
        jump(&mut app, 'd');
        assert_first_row(&mut app, 19, 15, "You");
        let after = positions(&mut app);
        assert_eq!(after.len(), 3);
        assert_eq!(
            app.view_scroll(),
            after[1].1,
            "diagnostic and width changes preserve semantic alignment"
        );
    }
}

#[test]
fn folded_acp_groups_share_one_stop_but_keep_the_original_semantic_alignment() {
    let (mut app, mut transcript) = acp_transcript_app();
    for event in mixed_role_jump_events() {
        apply_agent_event(&mut app, &mut transcript, event);
    }
    app.set_view_for_test(0, false);
    draw(&mut app, 60, 15);
    jump(&mut app, ']');
    let second_group = positions(&mut app)[1].0;
    assert_first_row(&mut app, 60, 15, "You");
    jump(&mut app, 'z');
    jump(&mut app, 'm');
    assert_first_row(&mut app, 60, 15, "earlier message");
    assert_eq!(
        positions(&mut app).len(),
        2,
        "folded groups do not produce repeated stops"
    );
    jump(&mut app, 'z');
    jump(&mut app, 'R');
    let (buffer, content) = draw(&mut app, 60, 15);
    assert!(buffer_row_text(&buffer, content.y).contains("You"));
    assert!(buffer_row_text(&buffer, content.y + 1).contains("second prompt"));
    assert_eq!(
        app.view_scroll(),
        app.view_cache().turn_start_row(second_group).unwrap()
    );
}

#[test]
fn reflow_folding_and_streaming_preserve_a_turn_header_not_an_old_scroll_anchor() {
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(Message::user("wide words ".repeat(100))),
        TranscriptItem::Message(Message::assistant(numbered_lines(40))),
        TranscriptItem::Plan(PlanRecord::Handoff {
            handoff: PlanHandoff::new(test_plan_artifact(), "source"),
        }),
        TranscriptItem::Message(Message::assistant("answer")),
    ]);
    app.set_view_for_test(3, false);
    draw(&mut app, 80, 20);
    // Mutation captures an older generic body anchor before geometry is invalidated.
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TurnId::new(10),
        message: Message::user("latest"),
        mode: SessionMode::Build,
    });
    jump(&mut app, ']');
    assert_first_row(&mut app, 80, 20, "Approved Plan handoff · #2");
    let wide = app.view_scroll();
    app.handle_event(Event::Resize(22, 18));
    assert_first_row(&mut app, 22, 18, "Approved");
    assert!(
        app.view_scroll() > wide,
        "reflow changed earlier prompt height"
    );
    app.handle_event(Event::Resize(80, 20));
    assert_first_row(&mut app, 80, 20, "Approved Plan handoff · #2");
    jump(&mut app, 'z');
    jump(&mut app, 'm');
    assert_first_row(&mut app, 80, 20, "Approved Plan handoff · #2");
    let folded_top = app.view_scroll();
    assert!(folded_top < wide);
    assert!(app.folds().entry(2).is_message_folded());
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TurnId::new(10),
        snapshot: Message::assistant(numbered_lines(100)).into(),
    });
    assert_first_row(&mut app, 80, 20, "Approved Plan handoff · #2");
    assert_eq!(app.view_scroll(), folded_top);
    assert!(!app.view_follow());
    jump(&mut app, '[');
    assert_first_row(&mut app, 80, 20, "You · #1");
    assert!(
        app.folds().entry(0).is_message_folded(),
        "jumping must not expand folds"
    );
    jump(&mut app, 'z');
    jump(&mut app, 'R');
    assert_first_row(&mut app, 80, 20, "You · #1");
}

#[test]
fn unrendered_and_zero_sized_panes_defer_ordered_jumps_and_explicit_navigation_cancels_them() {
    for zero_render in [false, true] {
        let mut app = long_conversation();
        if zero_render {
            draw(&mut app, 0, 0);
        }
        jump(&mut app, ']');
        jump(&mut app, ']');
        jump(&mut app, '[');
        assert_first_row(&mut app, 40, 15, "You · #2");
        assert!(!app.view_follow());
    }
    for navigation in [
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
    ] {
        let mut app = long_conversation();
        draw(&mut app, 80, 20);
        app.handle_event(Event::Resize(20, 12));
        jump(&mut app, ']');
        jump(&mut app, ']');
        app.handle_event(key(navigation));
        draw(&mut app, 20, 12);
        assert_ne!(
            app.view_scroll(),
            positions(&mut app)[2].1,
            "stale jumps replayed after {navigation:?}"
        );
    }
    let mut app = long_conversation();
    draw(&mut app, 80, 20);
    jump(&mut app, ']');
    app.handle_event(Event::Resize(32, 15));
    jump(&mut app, ']');
    jump(&mut app, ']');
    jump(&mut app, '[');
    assert_first_row(&mut app, 32, 15, "You · #3");
}

#[test]
fn routing_leaves_composer_selection_and_help_owners_in_control() {
    let mut app = long_conversation();
    draw(&mut app, 80, 20);
    enter_insert(&mut app);
    jump(&mut app, '[');
    jump(&mut app, ']');
    assert_eq!(app.input(), "[]");
    assert_eq!(app.input_cursor(), 2);
    assert_eq!(app.view_scroll(), 0);
    app.handle_event(key(KeyCode::Esc));
    draw(&mut app, 80, 20);
    jump(&mut app, ']');
    draw(&mut app, 80, 20);
    let before = app.view_scroll();
    jump(&mut app, '?');
    jump(&mut app, '[');
    jump(&mut app, ']');
    assert_eq!(app.view_scroll(), before);
    jump(&mut app, '?');
    app.select_for_test(cursor(2, 0));
    jump(&mut app, '[');
    jump(&mut app, ']');
    assert_eq!(app.selection(), cursor(2, 0));
    assert_eq!(app.view_scroll(), before);
    app.handle_event(ctrl('d'));
    assert_eq!(
        app.selection(),
        cursor(4, 0),
        "Selection Ctrl+D remains a user-message jump"
    );
    app.handle_event(ctrl('u'));
    assert_eq!(app.selection(), cursor(2, 0));
    assert_eq!(app.input(), "[]");
    assert_eq!(app.input_cursor(), 2);
}

#[test]
fn accepted_tail_edits_and_worker_retirement_discard_alignment_and_queued_jumps() {
    let mut app = long_conversation();
    draw(&mut app, 80, 20);
    jump(&mut app, ']');
    jump(&mut app, ']');
    assert_first_row(&mut app, 80, 20, "You · #3");
    app.handle_event(Event::Resize(70, 22));
    jump(&mut app, ']');
    let change = app.conversation_projection_mut().commit_edit(4, false);
    app.apply_conversation_change(change);
    let conversation = app.conversation_projection_mut();
    let turn = conversation.allocate_turn();
    conversation.push_user_turn(Message::user("replacement at the same entry index"), turn);
    let (_, content) = draw(&mut app, 70, 22);
    let real = app
        .view_cache()
        .entries()
        .iter()
        .map(|entry| entry.extent())
        .sum::<usize>();
    assert!(
        app.view_scroll() <= real.saturating_sub(usize::from(content.height)),
        "old entry identity must not align a replacement prompt"
    );
    assert_ne!(app.view_scroll(), positions(&mut app)[2].1);
    assert!(
        !app.view_follow(),
        "clearing an invalid target does not create downward intent"
    );

    let (mut worker, mut transcript) = acp_transcript_app();
    for event in mixed_role_jump_events() {
        apply_agent_event(&mut worker, &mut transcript, event);
    }
    worker.set_view_for_test(0, false);
    draw(&mut worker, 80, 30);
    jump(&mut worker, ']');
    worker.handle_event(Event::Resize(80, 30));
    jump(&mut worker, ']');
    worker.freeze_worker();
    draw(&mut worker, 80, 30);
    assert_eq!(
        worker.view_scroll(),
        0,
        "retired panes do not replay pending alignment"
    );
    assert!(!worker.view_follow());
}

#[test]
fn restore_projection_replacement_and_panes_do_not_reuse_turn_targets() {
    let mut root = long_conversation();
    let mut other = long_conversation();
    draw(&mut root, 80, 20);
    draw(&mut other, 40, 15);
    jump(&mut root, ']');
    jump(&mut root, ']');
    assert_first_row(&mut root, 80, 20, "You · #3");
    assert_first_row(&mut other, 40, 15, "You · #1");
    root.handle_event(Event::Resize(30, 12));
    jump(&mut root, ']');
    root.restore(vec![TranscriptItem::Message(Message::user("replacement"))]);
    assert_first_row(&mut root, 80, 20, "You · #1");
    assert_eq!((root.view_scroll(), root.view_follow()), (0, true));
    jump(&mut other, ']');
    let change = other.conversation_projection_mut().clear_projection();
    other.apply_conversation_change(change);
    other.seed_history_entry(history_message(Message::user("reused index")));
    assert_first_row(&mut other, 40, 15, "You");
    assert_eq!((other.view_scroll(), other.view_follow()), (0, true));
}
