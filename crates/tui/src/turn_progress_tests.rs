//! Rebuildable, pane-local native header numbering, never engine/edit IDs.

use super::*;
use crate::app::ConversationState;
use crate::layout::ConversationCache;
use crate::presentation::{DisplayTurn, NativeHeader, TranscriptAppearance};

fn call(app: &mut App, turn_id: TurnId, call: usize) {
    app.reduce_without_effects(SessionEvent::ModelCallStarted { turn_id, call });
}

fn start(app: &mut App, id: u64, text: &str) {
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: TurnId::new(id),
        message: Message::user(text),
        mode: SessionMode::Build,
    });
}

fn complete(app: &mut App, id: u64, text: &str) {
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        turn_id: TurnId::new(id),
        message: Message::assistant(text),
        display_attempt_id: None,
    });
}

fn active_header(app: &mut App) -> Option<NativeHeader> {
    app.render_parts().assistant_header
}

fn assistant(turn: usize, call: usize) -> NativeHeader {
    NativeHeader::Assistant {
        turn: DisplayTurn(turn),
        call,
    }
}

fn labels(app: &App) -> Vec<String> {
    app.history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => entry.header,
            HistoryEntry::Ensemble(entry) => entry.header,
            HistoryEntry::PlanHandoff(_, header) => *header,
            _ => None,
        })
        .map(|header| header.to_string())
        .collect()
}

#[test]
fn call_start_stream_tools_and_completion_share_headers_not_status_indices() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("prompt", 6);
    let now = Instant::now();
    assert!(app.handle_event_at(ctrl_enter(), now).is_some());
    call(&mut app, TEST_TURN_ID, 9);
    assert_eq!(active_header(&mut app), None);
    assert!(!rendered_text(&mut app, 160, 35).contains("● Assistant"));
    start(&mut app, 1, "prompt");
    assert_eq!(app.status_for_test().primary, "Build · Waiting");
    let text = rendered_text(&mut app, 160, 35);
    assert!(text.contains("● You · #1"));
    assert!(!text.contains("● Assistant"));
    app.observe_clock(now + Duration::from_secs(12));
    call(&mut app, TEST_TURN_ID, 1);
    assert!(rendered_text(&mut app, 160, 35).contains("● Assistant · #(1 - 1)"));
    assert_eq!(app.history().len(), 1, "pending header is render-only");
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("preview")).into(),
    });
    let text = rendered_text(&mut app, 160, 35);
    assert_eq!(text.matches("● Assistant · #(1 - 1)").count(), 1);
    assert!(text.contains("streaming · 12s"));
    assert_eq!(app.status_for_test().primary, "Build · Streaming");
    app.reduce_without_effects(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        display_attempt_id: None,
        message: assistant_message(vec![
            AssistantContent::text("preview"),
            tool_call("a", None, "command", json!({"command":"first"})),
            tool_call("b", None, "command", json!({"command":"second"})),
        ]),
    });
    let text = rendered_text(&mut app, 160, 35);
    assert_eq!(
        text.matches("● Assistant").count(),
        1,
        "no empty tool-phase header"
    );
    assert!(!app.render_parts().pending_header);
    assert_eq!(labels(&app), ["#1", "#(1 - 1)"]);
    for attempt in [1, 2] {
        app.reduce_without_effects(SessionEvent::TurnRetrying {
            turn_id: TEST_TURN_ID,
            attempt,
            max_attempts: 4,
            retry_after: Duration::from_secs(4),
            error: "offline".into(),
        });
        assert_eq!(
            app.status_for_test().primary,
            format!("Build · Reconnecting {attempt}/4")
        );
        let text = rendered_text(&mut app, 160, 35);
        assert!(text.contains(&format!("attempt {attempt}/4")));
        assert!(text.contains("connection lost: offline"));
        assert_eq!(text.matches("● Assistant · #(1 - 1)").count(), 1);
    }
    app.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id: TEST_TURN_ID,
        trigger: CompactionTrigger::AutomaticMidTurn,
    });
    assert_eq!(app.status_for_test().primary, "Build · Compacting");
    assert!(rendered_text(&mut app, 160, 35).contains("Compacting context… · 12s"));
    app.reduce_without_effects(SessionEvent::CompactionCompleted {
        turn_id: TEST_TURN_ID,
        trigger: CompactionTrigger::AutomaticMidTurn,
        backend: CompactionBackend::LocalSummary,
    });
    for id in ["a", "b"] {
        app.reduce_without_effects(SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: Message::tool_result(id, "command", "done"),
            metadata: Vec::new(),
        });
    }
    call(&mut app, TEST_TURN_ID, 2);
    assert_eq!(active_header(&mut app), Some(assistant(1, 2)));
    app.reduce_without_effects(SessionEvent::PersistenceChanged {
        path: PathBuf::from("transcript.jsonl"),
        error: Some("disk full".into()),
    });
    assert_eq!(
        app.status_for_test().detail.as_deref(),
        Some("Build · Waiting")
    );
    let text = rendered_text(&mut app, 160, 35);
    assert!(text.contains("running… · 12s"));
    assert!(!text.contains("turn 1"));
    assert!(!text.contains("call 2"));
    complete(&mut app, 1, "done");
    assert_eq!(active_header(&mut app), None);
    assert_eq!(labels(&app), ["#1", "#(1 - 1)", "#(1 - 2)"]);
    assert_eq!(
        rendered_text(&mut app, 160, 35)
            .matches("● Assistant · #(1 - 2)")
            .count(),
        1
    );
    start(&mut app, 90, "second prompt");
    call(&mut app, TurnId::new(90), 1);
    complete(&mut app, 90, "second answer");
    assert_eq!(
        labels(&app),
        ["#1", "#(1 - 1)", "#(1 - 2)", "#2", "#(2 - 1)"]
    );
}

#[test]
fn standalone_compaction_and_pre_turn_phases_never_create_header_or_consume_turn() {
    for trigger in [
        CompactionTrigger::Manual,
        CompactionTrigger::AutomaticPreTurn,
    ] {
        let mut app = App::new();
        app.reduce_without_effects(SessionEvent::CompactionStarted {
            turn_id: TurnId::new(4),
            trigger,
        });
        call(&mut app, TurnId::new(4), 1);
        assert_eq!(active_header(&mut app), None);
        assert_eq!(app.status_for_test().primary, "Build · Compacting");
        let text = rendered_text(&mut app, 120, 15);
        assert!(text.contains("Compacting context… · 0s"));
        assert!(!text.contains("● Assistant"));
        app.reduce_without_effects(SessionEvent::CompactionCompleted {
            turn_id: TurnId::new(4),
            trigger,
            backend: CompactionBackend::LocalSummary,
        });
        call(&mut app, TurnId::new(4), 1);
        assert_eq!(active_header(&mut app), None);
        start(&mut app, 4, "accepted");
        call(&mut app, TurnId::new(4), 1);
        assert_eq!(active_header(&mut app), Some(assistant(1, 1)));
    }
}

#[test]
fn stale_duplicate_pending_and_terminal_call_markers_are_inert() {
    for terminal in [
        SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: Message::assistant("done"),
            display_attempt_id: None,
        },
        SessionEvent::TurnRecovered {
            turn_id: TEST_TURN_ID,
            display_attempt_id: None,
        },
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "failed".into(),
        },
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "rejected".into(),
        },
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    ] {
        let mut app = App::new();
        call(&mut app, TEST_TURN_ID, 9);
        assert_eq!(active_header(&mut app), None);
        start(&mut app, 1, "kept prompt");
        call(&mut app, TEST_TURN_ID, 1);
        call(&mut app, TEST_TURN_ID, 2);
        for (id, raw) in [
            (TEST_TURN_ID, 2),
            (TEST_TURN_ID, 1),
            (TEST_TURN_ID, 0),
            (TurnId::new(99), 9),
        ] {
            call(&mut app, id, raw);
            assert_eq!(active_header(&mut app), Some(assistant(1, 2)));
        }
        app.reduce_without_effects(terminal);
        call(&mut app, TEST_TURN_ID, 3);
        assert_eq!(active_header(&mut app), None);
        assert!(!app.render_parts().pending_header);
        start(&mut app, 8, "next");
        call(&mut app, TurnId::new(8), 1);
        assert_eq!(active_header(&mut app), Some(assistant(2, 1)));
        app.restore(vec![TranscriptItem::Message(Message::user("restored"))]);
        call(&mut app, TurnId::new(8), 2);
        assert_eq!(active_header(&mut app), None);
        start(&mut app, 1, "engine restarted");
        call(&mut app, TEST_TURN_ID, 1);
        assert_eq!(active_header(&mut app), Some(assistant(2, 1)));
    }
}

#[test]
fn native_child_headers_and_status_titles_are_independent_of_root() {
    let mut root = App::new();
    start(&mut root, 8, "root prompt");
    call(&mut root, TurnId::new(8), 1);
    call(&mut root, TurnId::new(8), 2);
    let mut views = test_session_views(root);
    let child = SubtaskId::new("indexed-child");
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TurnId::new(8),
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("indexed-child", "indexed child"),
    });
    for event in [
        SessionEvent::TurnStarted {
            turn_id: TurnId::new(42),
            message: Message::user("child prompt"),
            mode: SessionMode::Build,
        },
        SessionEvent::ModelCallStarted {
            turn_id: TurnId::new(42),
            call: 1,
        },
    ] {
        views.apply(SessionEvent::SubtaskSession {
            id: child.clone(),
            event: Box::new(event),
        });
    }
    views.handle_event(ctrl('i'));
    let text = rendered_views_text(&mut views, 180, 20);
    assert!(text.contains("● You · #1"));
    assert!(text.contains("indexed child"));
    assert_eq!(text.matches("#(1 - 1)").count(), 1);
    assert!(!text.contains("#(1 - 2)"));
    assert!(!text.contains("turn 42"));
    views.handle_event(ctrl('o'));
    let text = rendered_views_text(&mut views, 180, 20);
    assert_eq!(text.matches("#(1 - 2)").count(), 1);
}

#[test]
fn prompt_metadata_only_rebuilds_native_header_and_never_changes_copy_recall_or_acp() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let prompt = zevria_content::UserPrompt::new(vec![
        zevria_content::PromptBlock::Text("first".into()),
        zevria_content::PromptBlock::Image(image.clone()),
        zevria_content::PromptBlock::Text("last".into()),
    ])
    .unwrap();
    let mut conversation = ConversationState::default();
    let turn = conversation.allocate_turn();
    conversation.push_user_turn(prompt.to_message(), turn);
    let selected = Selection {
        history_index: 0,
        content_index: 0,
    };
    assert_eq!(
        conversation.recallable_selection(selected).unwrap(),
        (prompt.clone(), TranscriptEditTarget::PromptOrdinal(0))
    );
    let HistoryEntry::Conversation(entry) = &conversation.history()[0] else {
        panic!("prompt")
    };
    assert_eq!(
        entry
            .blocks
            .iter()
            .map(|block| block.primary_copy())
            .collect::<Vec<_>>(),
        ["first".to_string(), image.label(1), "last".to_string()]
    );
    let mut cache = ConversationCache::default();
    cache.refresh(
        conversation.history(),
        Some(ActiveSelection {
            selection: selected,
            scope: SelectionScope::Block,
        }),
        8,
        false,
        &crate::app::FoldState::default(),
    );
    assert_eq!(line_text(&cache.entries()[0].lines[0]), "● You · #1");
    let header_height = crate::layout::prepare::wrapped_height(&cache.entries()[0].lines[..1], 8);
    assert!(header_height > 1);
    assert_eq!(cache.entries()[0].items[0].1.start(), header_height);
    let rebuilt = cache.block_rebuilds;
    cache.refresh(
        conversation.history(),
        Some(ActiveSelection {
            selection: selected,
            scope: SelectionScope::Block,
        }),
        8,
        false,
        &crate::app::FoldState::default(),
    );
    assert_eq!(cache.block_rebuilds, rebuilt);
    let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(0) else {
        panic!("prompt")
    };
    entry.header = Some(NativeHeader::Prompt(DisplayTurn(987654)));
    cache.refresh(
        conversation.history(),
        Some(ActiveSelection {
            selection: selected,
            scope: SelectionScope::Block,
        }),
        8,
        false,
        &crate::app::FoldState::default(),
    );
    assert_eq!(cache.block_rebuilds, rebuilt + 1);
    assert_eq!(line_text(&cache.entries()[0].lines[0]), "● You · #987654");
    assert_eq!(
        conversation.recallable_selection(selected).unwrap().0,
        prompt
    );
    cache.set_appearance(TranscriptAppearance::Acp);
    cache.refresh(
        conversation.history(),
        None,
        100,
        false,
        &crate::app::FoldState::default(),
    );
    assert_eq!(line_text(&cache.entries()[0].lines[0]), "● You");
    let mut acp = App::acp_inspect("ACP worker title");
    start(&mut acp, 1, "ACP prompt");
    call(&mut acp, TEST_TURN_ID, 1);
    assert_eq!(active_header(&mut acp), None);
    assert_eq!(acp.status_for_test().primary, "ACP worker title");
    assert!(!rendered_text(&mut acp, 120, 15).contains('#'));
}

#[test]
fn ensemble_resume_reuses_turn_and_rebases_restarted_raw_calls() {
    for restored in [false, true] {
        let ensemble = editable_ensemble_start(
            "indexed-ensemble",
            EnsembleWorkflow::Review,
            "review prompt",
        );
        let mut app = App::new();
        if restored {
            app.restore(vec![
                TranscriptItem::Ensemble(EnsembleRecord::Started {
                    start: ensemble.clone(),
                }),
                TranscriptItem::Message(Message::assistant("saved response")),
                TranscriptItem::Compaction(checkpoint()),
                TranscriptItem::Error {
                    error: "interrupted".into(),
                },
            ]);
        } else {
            app.reduce_without_effects(SessionEvent::EnsembleStarted {
                turn_id: TurnId::new(4),
                start: ensemble.clone(),
                resumed: false,
            });
            assert_eq!(
                active_header(&mut app),
                None,
                "workers are not model-loop calls"
            );
            call(&mut app, TurnId::new(4), 1);
            complete(&mut app, 4, "saved response");
        }
        let before = app.history().len();
        app.reduce_without_effects(SessionEvent::EnsembleStarted {
            turn_id: TurnId::new(7),
            start: ensemble,
            resumed: true,
        });
        assert_eq!(app.history().len(), before);
        call(&mut app, TurnId::new(7), 1);
        assert_eq!(active_header(&mut app), Some(assistant(1, 2)));
        complete(&mut app, 7, "resumed response");
        let text = rendered_text(&mut app, 160, 40);
        assert!(text.contains("● Ensemble Review · #1"));
        assert_eq!(text.matches("● Assistant · #(1 - 2)").count(), 1);
        start(&mut app, 20, "next prompt");
        assert_eq!(labels(&app).last().unwrap(), "#2");
    }
}

#[test]
fn restore_counts_typed_anchors_and_canonical_responses_not_metadata_or_projections() {
    let mut app = App::new();
    let skill = SkillInvocation::new(
        SkillName::parse("commit").unwrap(),
        "ship it",
        SkillApplication::Activate(
            SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Instructions").unwrap(),
        ),
    );
    let handoff = PlanHandoff::new(test_plan_artifact(), "source");
    app.restore(vec![
        TranscriptItem::Message(Message::assistant("orphan stays unnumbered")),
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Message(opaque_reasoning_message()),
        TranscriptItem::Message(assistant_message(vec![tool_call(
            "a",
            None,
            "command",
            json!({}),
        )])),
        TranscriptItem::ToolResults {
            message: Message::tool_result("a", "command", "result"),
            metadata: Vec::new(),
            skill_applications: Vec::new(),
        },
        TranscriptItem::Message(Message::tool_result("b", "command", "legacy result")),
        TranscriptItem::Compaction(checkpoint()),
        TranscriptItem::SessionMode(SessionMode::Plan),
        TranscriptItem::Error {
            error: "diagnostic".into(),
        },
        TranscriptItem::Plan(PlanRecord::Published {
            artifact: test_plan_artifact(),
            provenance: zevria_workflow::PlanPublicationProvenance::Synthesized,
        }),
        TranscriptItem::Message(Message::assistant("third response")),
        TranscriptItem::SkillInvocation(skill),
        TranscriptItem::Message(Message::assistant("skill response")),
        TranscriptItem::Plan(PlanRecord::Handoff {
            handoff: handoff.clone(),
        }),
        TranscriptItem::Message(Message::assistant("implemented")),
    ]);
    assert_eq!(
        labels(&app),
        [
            "#1", "#(1 - 2)", "#(1 - 3)", "#2", "#(2 - 1)", "#3", "#(3 - 1)"
        ]
    );
    let HistoryEntry::Conversation(orphan) = &app.history()[0] else {
        panic!("orphan")
    };
    assert_eq!(orphan.header, None);
    start(&mut app, 1, "after restore");
    call(&mut app, TEST_TURN_ID, 1);
    assert_eq!(active_header(&mut app), Some(assistant(4, 1)));
    let mut live = App::new();
    live.reduce_without_effects(SessionEvent::PlanHandoffStarted {
        turn_id: TurnId::new(55),
        handoff,
    });
    call(&mut live, TurnId::new(55), 1);
    live.set_view_for_test(0, false);
    let text = rendered_text(&mut live, 180, 100);
    assert!(text.contains("● Approved Plan handoff · #1"));
    assert!(text.contains("● Assistant · #(1 - 1)"));
    assert!(!text.contains("● You"));
}

#[test]
fn accepted_edit_truncates_numbering_but_pending_and_rejected_edits_do_not() {
    for accepted in [false, true] {
        let mut app = App::new();
        app.restore(vec![
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant("first answer")),
            TranscriptItem::Plan(PlanRecord::Handoff {
                handoff: PlanHandoff::new(test_plan_artifact(), "source"),
            }),
            TranscriptItem::Message(Message::user("second editable prompt")),
            TranscriptItem::Message(Message::assistant("old branch")),
            TranscriptItem::Message(Message::user("removed tail")),
        ]);
        let before = labels(&app);
        app.select_for_test(cursor(3, 0));
        ctrl_e(&mut app);
        app.set_input_for_test("replacement", 11);
        assert!(matches!(
            app.handle_event(ctrl_enter()),
            Some(UiAction::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(1),
                ..
            }))
        ));
        assert_eq!(labels(&app), before);
        call(&mut app, TurnId::new(80), 5);
        assert_eq!(labels(&app), before);
        if accepted {
            start(&mut app, 80, "replacement");
            assert_eq!(labels(&app), ["#1", "#(1 - 1)", "#2", "#3"]);
            call(&mut app, TurnId::new(80), 1);
            complete(&mut app, 80, "new branch");
            assert_eq!(labels(&app).last().unwrap(), "#(3 - 1)");
            start(&mut app, 91, "new tail");
            assert_eq!(labels(&app).last().unwrap(), "#4");
        } else {
            app.reduce_without_effects(SessionEvent::TurnRejected {
                turn_id: TurnId::new(80),
                error: "rejected".into(),
            });
            assert_eq!(labels(&app), before);
        }
    }
}

#[test]
fn runtime_only_calls_survive_live_resume_but_are_not_invented_on_restore() {
    let ensemble = editable_ensemble_start("failed-call", EnsembleWorkflow::Review, "review");
    let mut live = App::new();
    live.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: ensemble.clone(),
        resumed: false,
    });
    call(&mut live, TEST_TURN_ID, 1);
    live.reduce_without_effects(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        message: Message::assistant("saved response"),
        display_attempt_id: None,
    });
    call(&mut live, TEST_TURN_ID, 2);
    live.reduce_without_effects(SessionEvent::TurnFailed {
        turn_id: TEST_TURN_ID,
        error: "unsaved dispatch".into(),
    });
    let mut restored = App::new();
    restored.restore(vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: ensemble.clone(),
        }),
        TranscriptItem::Message(Message::assistant("saved response")),
        TranscriptItem::Error {
            error: "unsaved dispatch".into(),
        },
    ]);
    for (app, expected) in [(&mut live, 3), (&mut restored, 2)] {
        app.reduce_without_effects(SessionEvent::EnsembleStarted {
            turn_id: TurnId::new(9),
            start: ensemble.clone(),
            resumed: true,
        });
        call(app, TurnId::new(9), 1);
        assert_eq!(active_header(app), Some(assistant(1, expected)));
    }
}

#[test]
fn editing_away_a_resumed_ensemble_call_reconciles_the_retained_turns_call_ledger() {
    let ensemble = editable_ensemble_start("retained-ensemble", EnsembleWorkflow::Review, "review");
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: ensemble.clone(),
        }),
        TranscriptItem::Message(Message::assistant("retained call")),
        TranscriptItem::Message(Message::user("editable prompt")),
        TranscriptItem::Message(Message::assistant("removed call")),
    ]);
    app.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(9),
        start: ensemble.clone(),
        resumed: true,
    });
    call(&mut app, TurnId::new(9), 1);
    complete(&mut app, 9, "removed ensemble continuation");
    assert_eq!(labels(&app).last().unwrap(), "#(1 - 2)");
    app.select_for_test(cursor(2, 0));
    ctrl_e(&mut app);
    app.set_input_for_test("replacement", 11);
    assert!(matches!(
        app.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(_))
    ));
    start(&mut app, 10, "replacement");
    call(&mut app, TurnId::new(10), 1);
    complete(&mut app, 10, "new call");
    assert_eq!(labels(&app), ["#1", "#(1 - 1)", "#2", "#(2 - 1)"]);
    app.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(11),
        start: ensemble,
        resumed: true,
    });
    call(&mut app, TurnId::new(11), 1);
    assert_eq!(
        active_header(&mut app),
        Some(assistant(1, 2)),
        "removed continuation cannot leak call 3"
    );
}

#[test]
fn missing_authoritative_call_metadata_leaves_assistant_unnumbered() {
    let mut app = App::new();
    start(&mut app, 9, "prompt");
    complete(&mut app, 9, "answer without a marker");
    let text = rendered_text(&mut app, 100, 20);
    assert!(text.contains("● You · #1"));
    assert!(text.contains("● Assistant"));
    assert!(!text.contains("#("));
}

#[test]
fn pending_geometry_and_identical_stream_content_cache_include_header_identity() {
    let mut cache = ConversationCache::default();
    for width in [1, 2, 8, 20, 40] {
        cache.refresh_streaming(None, Some(assistant(123456, 987654)), width);
        let (lines, height) = cache.streaming().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "● Assistant · #(123456 - 987654)");
        assert_eq!(height, crate::layout::prepare::wrapped_height(lines, width));
        for row in 0..height {
            assert_eq!(cache.semantic_anchor(row), None);
        }
    }
    let message = assistant_message(vec![
        AssistantContent::text("same"),
        AssistantContent::text("other block"),
    ]);
    cache.refresh_streaming(Some(&message), Some(assistant(1, 1)), 40);
    let rebuilt = cache.block_rebuilds;
    cache.refresh_streaming(Some(&message), Some(assistant(1, 1)), 40);
    assert_eq!(cache.block_rebuilds, rebuilt);
    cache.refresh_streaming(Some(&message), Some(assistant(1, 2)), 40);
    assert_eq!(
        cache.block_rebuilds,
        rebuilt + 1,
        "only the role-header block changes"
    );
    assert_eq!(
        line_text(&cache.streaming().unwrap().0[0]),
        "● Assistant · #(1 - 2)"
    );
    cache.refresh_streaming(None, None, 40);
    assert!(cache.streaming().is_none());

    let mut app = App::new();
    start(&mut app, 1, "prompt");
    call(&mut app, TEST_TURN_ID, 1);
    for width in [1, 2, 8, 20, 40] {
        let buffer = rendered_buffer(&mut app, width, 15);
        assert_eq!(buffer.area.width, width);
        assert!(app.view_follow());
    }
}
