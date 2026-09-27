use super::*;

#[test]
fn native_answer_frames_grow_and_reconcile_citations_in_the_same_selected_block() {
    let mut attempt = ordered_attempt();
    attempt.activity.clear();
    attempt.presentation = vec![part(
        0,
        AssistantPartIdentity::Content(0),
        AssistantPresentationContent::Answer {
            text: "A streamed answer".into(),
        },
    )];
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    app.select_for_test(cursor(0, 0));
    let first = rendered_text(&mut app, 100, 20);
    assert!(first.contains("A streamed answer"));
    let block_id = match &app.history()[0] {
        HistoryEntry::Conversation(entry) => entry.blocks[0].id,
        _ => panic!("answer"),
    };
    attempt.presentation[0].content = AssistantPresentationContent::Answer {
        text: "A streamed answer continues before completion".into(),
    };
    attempt.touch();
    publish(&mut app, attempt.clone());
    assert_eq!(attempt.outcome, Outcome::InProgress);
    assert!(
        rendered_text(&mut app, 100, 20).contains("A streamed answer continues before completion")
    );
    assert_eq!(app.selection(), cursor(0, 0));
    let final_text = "A streamed answer [source](https://example.com) continues before completion";
    attempt.presentation[0].content = AssistantPresentationContent::Answer {
        text: final_text.into(),
    };
    attempt.finish(Outcome::Completed);
    publish(&mut app, attempt.clone());
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: Message::assistant(final_text),
            display_attempt_id: Some(attempt.id),
        },
    );
    assert_eq!(visible(app.conversation_projection_mut()), [final_text]);
    let HistoryEntry::Conversation(entry) = &app.history()[0] else {
        panic!("answer")
    };
    assert_eq!(entry.blocks.len(), 1);
    assert_eq!(entry.blocks[0].id, block_id);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(
        rendered_text(&mut app, 100, 20)
            .matches("A streamed answer")
            .count(),
        1
    );
}

#[test]
fn long_answer_frames_rebuild_only_the_growing_source_block() {
    let mut attempt = ordered_attempt();
    let mut text = String::from("A long streaming answer\n\n```rust\n");
    attempt.presentation[2].content = AssistantPresentationContent::Answer { text: text.clone() };
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    rendered_text(&mut app, 100, 30);
    let start = std::time::Instant::now();
    for _ in 0..16 {
        let before = app.view_cache().block_rebuilds;
        text.push_str(&"let readable_unicode = \"🦀\";\n".repeat(128));
        attempt.presentation[2].content =
            AssistantPresentationContent::Answer { text: text.clone() };
        attempt.touch();
        publish(&mut app, attempt.clone());
        rendered_text(&mut app, 100, 30);
        assert_eq!(
            app.view_cache().block_rebuilds - before,
            1,
            "reasoning and hosted actions retain their layouts"
        );
    }
    assert_eq!(
        visible(app.conversation_projection_mut()).last(),
        Some(&text)
    );
    eprintln!(
        "16 growing answer frames through {} bytes: {:?}",
        text.len(),
        start.elapsed()
    );
}

#[test]
fn reports_do_not_promote_or_flatten_verified_incomplete_answers() {
    for outcome in [Outcome::InProgress, Outcome::Failed, Outcome::Interrupted] {
        let mut attempt = ordered_attempt();
        if outcome != Outcome::InProgress {
            attempt.finish(outcome);
        }
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        for event in standard_events(&attempt) {
            reducer.apply_event(&mut conversation, event);
        }
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(metadata(attempt)),
            },
        );
        let before = visible(&conversation);
        reducer.reconcile_report(
            &mut conversation,
            "flattened report is not a native completed answer",
        );
        assert_eq!(visible(&conversation), before);
    }
}

#[test]
fn acp_citation_replacements_cover_prior_segments_even_when_previews_are_skipped() {
    for delayed in [false, true] {
        let mut attempt = ordered_attempt();
        attempt.activity.clear();
        attempt.presentation = vec![part(
            0,
            AssistantPartIdentity::Content(0),
            AssistantPresentationContent::Answer {
                text: "Claim.".into(),
            },
        )];
        let bound = |attempt: WebSearchAttemptRecord, id: &str| {
            let AssistantPresentationContent::Answer { text } = &attempt.presentation[0].content
            else {
                panic!("answer")
            };
            ResponseDisplay {
                version: 1,
                bindings: vec![Binding::Text {
                    kind: Kind::Message,
                    message_id: id.into(),
                    start: 0,
                    end: text.len(),
                    sources: vec![attempt.presentation[0].source.clone()],
                }],
                attempt,
            }
        };
        let preview = |text: &str, id: &str| AgentRunEvent::AgentMessage {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        if !delayed {
            reducer.apply_preview(&mut conversation, preview("Claim.", "draft"));
        }
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(bound(attempt.clone(), "draft")),
            },
        );
        if !delayed {
            assert_eq!(visible(&conversation), ["Claim."]);
        }
        let draft = "Claim. More prose.";
        attempt.presentation[0].content =
            AssistantPresentationContent::Answer { text: draft.into() };
        attempt.touch();
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(bound(attempt.clone(), "draft")),
            },
        );
        if !delayed {
            reducer.apply_preview(&mut conversation, preview(draft, "draft"));
        }
        let corrected = "Claim. [source](https://example.com) More prose.";
        attempt.presentation[0].content = AssistantPresentationContent::Answer {
            text: corrected.into(),
        };
        attempt.touch();
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(bound(attempt.clone(), "corrected")),
            },
        );
        if delayed {
            reducer.apply_preview(&mut conversation, preview(draft, "draft"));
        }
        reducer.apply_preview(&mut conversation, preview(corrected, "corrected"));
        assert_eq!(visible(&conversation), [corrected]);
        // Unbound text is not collateral damage of retiring the old segment.
        reducer.apply_event(&mut conversation, preview(" unrelated suffix", "draft"));
        assert_eq!(visible(&conversation), [corrected, " unrelated suffix"]);
        attempt.finish(Outcome::Completed);
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(bound(attempt, "corrected")),
            },
        );
        reducer.reconcile_report(&mut conversation, corrected);
        assert_eq!(visible(&conversation), [corrected, " unrelated suffix"]);
    }
}

#[test]
fn immutable_revisions_reject_conflicts_but_allow_acp_binding_enrichment() {
    let attempt = ordered_attempt();
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    let before = visible(app.conversation_projection_mut());
    let mut conflict = attempt.clone();
    conflict.outcome = Outcome::Failed;
    publish(&mut app, conflict.clone());
    assert_eq!(visible(app.conversation_projection_mut()), before);

    let mut reducer = AgentTranscriptReducer::default();
    let mut conversation = ConversationState::default();
    for event in standard_events(&attempt) {
        reducer.apply_event(&mut conversation, event);
    }
    let display = metadata(attempt);
    let mut actions_only = display.clone();
    actions_only
        .bindings
        .retain(|binding| matches!(binding, Binding::Tool { .. }));
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(actions_only),
        },
    );
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(display.clone()),
        },
    );
    assert_eq!(visible(&conversation), before);
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(ResponseDisplay {
                attempt: conflict,
                ..display
            }),
        },
    );
    assert_eq!(visible(&conversation), before);
}

#[test]
fn completed_acp_displays_do_not_disable_later_prompt_report_repair() {
    let mut attempt = ordered_attempt();
    attempt.finish(Outcome::Completed);
    let mut reducer = AgentTranscriptReducer::default();
    let mut conversation = ConversationState::default();
    for event in standard_events(&attempt) {
        reducer.apply_event(&mut conversation, event);
    }
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::ResponseDisplay {
            display: Box::new(metadata(attempt)),
        },
    );
    let before = visible(&conversation);
    reducer.apply_event(
        &mut conversation,
        AgentRunEvent::Prompt {
            text: "later prompt".into(),
            continuation: true,
            repair: None,
        },
    );
    reducer.apply_preview(
        &mut conversation,
        AgentRunEvent::AgentMessage {
            message_id: Some("later-message".into()),
            text: "incomplete preview".into(),
        },
    );
    reducer.reconcile_report(&mut conversation, "complete later report");
    let rows = visible(&conversation);
    assert_eq!(&rows[..before.len()], before.as_slice());
    assert_eq!(rows.last().unwrap(), "complete later report");
    assert!(!rows.iter().any(|row| row.contains("incomplete preview")));
}

#[test]
fn native_result_and_selection_follow_call_identity_after_earlier_parts_arrive() {
    let mut attempt = ordered_attempt();
    attempt.activity.truncate(1);
    attempt.presentation = vec![part(
        3,
        AssistantPartIdentity::Tool,
        AssistantPresentationContent::NativeTool {
            call_id: "native-call".into(),
        },
    )];
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            turn_id: TEST_TURN_ID,
            display_attempt_id: Some(attempt.id.clone()),
            message: assistant_message(vec![tool_call(
                "native-item",
                Some("native-call"),
                "command",
                json!({"command":"echo result"}),
            )]),
        },
    );
    assert_eq!(tool_status(&app, 0, 1), ToolCallStatus::Executing);
    app.select_for_test(cursor(0, 1));
    rendered_text(&mut app, 36, 12);
    attempt.presentation.push(reasoning(0, "before search"));
    attempt
        .presentation
        .push(reasoning(2, "before native call"));
    attempt.touch();
    publish(&mut app, attempt.clone());
    assert_eq!(app.selection(), cursor(0, 3));
    assert_eq!(tool_status(&app, 0, 3), ToolCallStatus::Executing);
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "native-item",
                Some("native-call"),
                "command",
                "native result",
            ),
            metadata: vec![],
        },
    );
    assert_eq!(tool_status(&app, 0, 3), ToolCallStatus::Finished);
    attempt.finish(Outcome::Completed);
    publish(&mut app, attempt);
    assert_eq!(app.selection(), cursor(0, 3));
    assert_eq!(tool_status(&app, 0, 3), ToolCallStatus::Finished);
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: json!({"command":"echo result"}).to_string(),
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "native result".into(),
        })
    );
}

#[test]
fn nonfollowing_viewport_tracks_semantic_block_through_growth_and_group_split() {
    let mut attempt = ordered_attempt();
    attempt.activity = vec![action(1, None), action(2, None)];
    attempt
        .presentation
        .retain(|part| part.source.output_index != 2);
    let AssistantPresentationContent::Answer { text } = &mut attempt.presentation[1].content else {
        panic!("answer fixture");
    };
    *text = (0..40).map(|i| format!("answer line {i}\n")).collect();
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    rendered_text(&mut app, 28, 10);
    let HistoryEntry::Conversation(entry) = &app.history()[0] else {
        panic!("entry")
    };
    let anchor = (0, entry.blocks[2].id, 0);
    let original_top = app.view_cache().anchor_row(anchor).unwrap();
    app.set_view_for_test(original_top, false);
    rendered_text(&mut app, 28, 10);
    assert_eq!(
        app.view_cache().semantic_anchor(app.view_scroll()),
        Some(anchor)
    );
    attempt.presentation[0] = reasoning(0, &"longer preceding thought\n".repeat(12));
    attempt.activity[0].action = Some(json!({"type":"search", "query":"a late readable query"}));
    attempt.touch();
    publish(&mut app, attempt);
    rendered_text(&mut app, 28, 10);
    assert!(!app.view_follow());
    assert!(app.view_scroll() > original_top);
    assert_eq!(
        app.view_cache().semantic_anchor(app.view_scroll()),
        Some(anchor)
    );
}

#[test]
fn narrow_native_child_panes_keep_inline_order_through_hidden_updates_and_switching() {
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("launch child"),
        mode: SessionMode::Build,
    });
    views.apply(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        display_attempt_id: None,
        message: launch_assistant("launch", "inspect web order", "child task"),
    });
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("inline-child", "inspect web order"),
    });
    let child_id = SubtaskId::new("inline-child");
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child task"),
            mode: SessionMode::Build,
        }),
    });
    let mut attempt = ordered_attempt();
    views.apply_streams(&zevria_session_api::SessionStreamBatch {
        subtasks: [(
            child_id.clone(),
            zevria_session_api::SessionStreamState {
                revision: 1,
                turn_id: TEST_TURN_ID,
                message: None,
                attempt: Some(attempt.clone()),
            },
        )]
        .into(),
        ..Default::default()
    });
    assert_eq!(views.visible_child_id(), None);
    rendered_views_text(&mut views, 28, 12);
    for code in [KeyCode::Esc, KeyCode::Esc, KeyCode::Enter, KeyCode::Enter] {
        assert_eq!(views.handle_event(key(code)), None);
    }
    assert_eq!(views.visible_child_id(), Some(&child_id));
    rendered_views_text(&mut views, 28, 12);
    let live = laid_out_transcript_text(views.child(&child_id).unwrap());
    let before = live.find("first thought").unwrap();
    let search = live.find("Web search:").unwrap();
    let after = live.find("second thought").unwrap();
    let open = live.find("Open page:").unwrap();
    let answer = live.find("cited answer").unwrap();
    assert!(before < search && search < after && after < open && open < answer);
    assert_eq!(live.matches("reasoning").count(), 2);
    assert!(!live.contains("Provider web activity"));
    assert!(views.root().is_busy());
    assert_eq!(views.handle_event(ctrl('o')), None);
    assert_eq!(views.visible_child_id(), None);
    attempt.finish(Outcome::Completed);
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::WebSearchUpdated {
            turn_id: TEST_TURN_ID,
            attempt: attempt.clone(),
        }),
    });
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            display_attempt_id: Some(attempt.id),
            message: Message::assistant("cited answer [source](https://example.com)"),
        }),
    });
    assert_eq!(views.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(views.visible_child_id(), Some(&child_id));
    rendered_views_text(&mut views, 28, 12);
    let completed = laid_out_transcript_text(views.child(&child_id).unwrap());
    assert_eq!(completed.matches("cited answer").count(), 1);
    assert_eq!(completed.matches("reasoning").count(), 2);
    assert!(views.root().is_busy());
}

#[test]
fn child_rows_keep_disjoint_stable_ids_and_parent_order_when_hosted_parts_arrive_late() {
    let mut attempt = ordered_attempt();
    attempt.activity.clear();
    attempt.presentation = vec![part(
        3,
        AssistantPartIdentity::Tool,
        AssistantPresentationContent::NativeTool {
            call_id: "batch".into(),
        },
    )];
    let mut app = App::new();
    publish(&mut app, attempt.clone());
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            turn_id: TEST_TURN_ID,
            display_attempt_id: Some(attempt.id.clone()),
            message: assistant_message(vec![tool_call(
                "native-item",
                Some("batch"),
                "launch_subtasks",
                json!({"tasks":[{"title":"one","type":"explore","prompt":"inspect"}]}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "batch".into(),
            entry_index: 0,
            descriptor: child_descriptor("ordered-child", "inspect"),
        },
    );
    app.select_for_test(cursor(0, 1));
    let identity = app
        .conversation_projection_mut()
        .selection_identity(Selection {
            history_index: 0,
            content_index: 1,
        })
        .unwrap();
    attempt.presentation.push(reasoning(0, "before batch"));
    attempt.presentation.push(reasoning(4, "after batch"));
    attempt.touch();
    publish(&mut app, attempt);
    assert_eq!(app.selection(), cursor(0, 2));
    assert_eq!(
        app.conversation_projection_mut()
            .selection_identity(Selection {
                history_index: 0,
                content_index: 2
            }),
        Some(identity)
    );
    let HistoryEntry::Conversation(entry) = &app.history()[0] else {
        unreachable!()
    };
    let ids: std::collections::HashSet<_> = entry.blocks.iter().map(|block| block.id).collect();
    assert_eq!(
        ids.len(),
        entry.blocks.len(),
        "child rows cannot consume a future hosted-attempt block ID"
    );
    assert!(entry.blocks[1].native_tool().is_some());
    assert!(matches!(
        entry.blocks[2].kind,
        PresentationBlockKind::Subtask { .. }
    ));
    assert!(matches!(
        entry.blocks[3].kind,
        PresentationBlockKind::Reasoning { .. }
    ));
}
