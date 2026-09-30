//! Transcript behavior and presentation tests.
use super::*;

#[test]
fn unread_intermediate_fences_reasoning_tool_preview_without_a_stale_tail() {
    let (sender, mut receiver) = session_event_channel(8);
    let mut views = test_session_views(App::new());
    sender
        .try_send(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("use a tool"),
            mode: SessionMode::Build,
        })
        .expect("turn starts");
    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("TurnStarted is available"),
    );

    let tool_message = assistant_message(vec![
        AssistantContent::Reasoning(Reasoning::summaries(vec!["inspect first".to_string()])),
        tool_call(
            "fc_preview",
            Some("call_preview"),
            "command",
            json!({"command": "cargo test"}),
        ),
    ]);
    sender.stream_updated(TEST_TURN_ID, tool_message.clone());
    sender
        .try_send(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_message,
        })
        .expect("intermediate queues");

    apply_transport_update(
        &mut views,
        receiver
            .try_recv()
            .expect("lifecycle is prioritized over the unread preview"),
    );
    assert!(receiver.try_recv().is_err(), "the stale preview is fenced");
    assert_eq!(assistant_history_count(views.root()), 1);
    assert!(views.root().streaming().is_none());
}

#[test]
fn a_preview_consumed_before_intermediate_is_cleared_by_the_commit() {
    let (sender, mut receiver) = session_event_channel(8);
    let mut views = test_session_views(App::new());
    sender
        .try_send(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("use a tool"),
            mode: SessionMode::Build,
        })
        .expect("turn starts");
    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("TurnStarted is available"),
    );

    let tool_message = assistant_message(vec![
        AssistantContent::Reasoning(Reasoning::summaries(vec!["inspect first".to_string()])),
        tool_call(
            "fc_consumed",
            Some("call_consumed"),
            "command",
            json!({"command": "cargo test"}),
        ),
    ]);
    sender.stream_updated(TEST_TURN_ID, tool_message.clone());
    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("preview is available"),
    );
    assert!(views.root().streaming().is_some());

    sender
        .try_send(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_message,
        })
        .expect("intermediate queues");
    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("intermediate is available"),
    );
    assert_eq!(assistant_history_count(views.root()), 1);
    assert!(views.root().streaming().is_none());
}

#[test]
fn a_new_preview_after_intermediate_remains_visible() {
    let (sender, mut receiver) = session_event_channel(8);
    let mut views = test_session_views(App::new());
    sender
        .try_send(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("continue after the tool"),
            mode: SessionMode::Build,
        })
        .expect("turn starts");
    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("TurnStarted is available"),
    );

    sender.stream_updated(TEST_TURN_ID, Message::assistant("old preview"));
    sender
        .try_send(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("committed tool call"),
        })
        .expect("intermediate queues");
    sender.stream_updated(TEST_TURN_ID, Message::assistant("legitimate continuation"));

    apply_transport_update(
        &mut views,
        receiver.try_recv().expect("intermediate is prioritized"),
    );
    apply_transport_update(
        &mut views,
        receiver
            .try_recv()
            .expect("post-intermediate preview remains"),
    );
    assert_eq!(assistant_history_count(views.root()), 1);
    assert_eq!(
        views.root().streaming(),
        Some(Message::assistant("legitimate continuation"))
    );
}

#[test]
fn stale_turn_events_cannot_mutate_an_idle_or_newer_turn() {
    let old_turn = TurnId::new(1);
    let new_turn = TurnId::new(2);
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: old_turn,
        message: Message::user("old question"),
        mode: SessionMode::Build,
    });
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: old_turn,
        message: Message::assistant("old answer"),
    });
    let idle_history_len = app.history().len();

    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: old_turn,
        snapshot: (Message::assistant("late draft")).into(),
    });
    app.reduce_without_effects(SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id: old_turn,
        message: Message::assistant("late intermediate"),
    });
    app.reduce_without_effects(SessionEvent::TurnCompleted {
        display_attempt_id: None,
        turn_id: old_turn,
        message: Message::assistant("duplicate terminal"),
    });
    assert_eq!(app.history().len(), idle_history_len);
    assert!(app.streaming().is_none());
    assert!(!app.is_busy());

    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id: new_turn,
        message: Message::user("new question"),
        mode: SessionMode::Plan,
    });
    let active_history_len = app.history().len();
    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: old_turn,
        error: "stale rejection".into(),
    });
    app.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: old_turn,
        snapshot: (Message::assistant("stale old stream")).into(),
    });
    app.reduce_without_effects(SessionEvent::TurnFailed {
        turn_id: old_turn,
        error: "stale old failure".to_string(),
    });
    assert_eq!(app.history().len(), active_history_len);
    assert_eq!(app.active_turn_id(), Some(new_turn));
    assert!(app.is_busy());
    assert_eq!(app.in_flight_mode(), Some(SessionMode::Plan));
}

#[test]
fn recovered_turn_unlocks_without_duplicating_restored_assistant_message() {
    let turn_id = TurnId::new(17);
    let final_message = Message::assistant("already restored synthesis");
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(final_message)]);
    let history_len = app.history().len();
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id,
        message: Message::User {
            content: Vec::new(),
        },
        mode: SessionMode::Build,
    });

    app.reduce_without_effects(SessionEvent::TurnRecovered {
        turn_id,
        display_attempt_id: None,
    });

    assert_eq!(app.history().len(), history_len);
    assert!(!app.is_busy());
    assert_eq!(app.active_turn_id(), None);
    assert_eq!(app.in_flight_mode(), None);
}

#[test]
fn streamed_tool_calls_are_hidden_until_finalized_intermediate() {
    let arguments = json!({ "command": "cargo test", "detail": "x".repeat(300) });

    let mut pure_call = App::new();
    apply_turn_event(
        &mut pure_call,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                arguments.clone(),
            )]))
            .into(),
        },
    );
    assert!(pure_call.streaming().is_none());
    let pure_streamed = rendered_text(&mut pure_call, 100, 10);
    assert!(!pure_streamed.contains("command("));
    assert!(!pure_streamed.contains("executing"));

    let message = assistant_message(vec![
        AssistantContent::text("draft explanation"),
        tool_call("fc_1", Some("call_1"), "command", arguments.clone()),
    ]);
    let mut app = App::new();

    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (message.clone()).into(),
        },
    );

    let streamed = rendered_text(&mut app, 500, 12);
    assert!(streamed.contains("draft explanation"));
    assert!(!streamed.contains("command("));
    assert!(!streamed.contains("executing"));

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message,
        },
    );

    assert_eq!(tool_status(&app, 0, 1), ToolCallStatus::Executing);
    let committed = rendered_text(&mut app, 500, 12);
    assert!(
        committed.contains("◆ cargo test ◐"),
        "shell command and executing status should render"
    );
    assert!(!committed.contains("command("));
    assert!(!committed.contains("detail"));
    assert_eq!(
        committed.matches("Assistant").count(),
        1,
        "only the committed message has a role header during tool execution"
    );
    assert!(committed.contains("running… ·"));
}

#[test]
fn command_tool_renders_shell_grammar_without_argument_wrapper() {
    let command = "rtk git log -1 --format=fuller";
    let message = assistant_message(vec![tool_call(
        "fc_1",
        Some("call_1"),
        "command",
        json!({ "command": command }),
    )]);
    let states = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Executing,
        result: None,
        metadata: None,
        subtasks: Default::default(),
    })];
    let mut lines = Vec::new();

    layout_native_message(&message, Some(&states), &mut lines, 120, None);

    assert_eq!(lines.len(), 2);
    let row = &lines[1];
    let rendered: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(rendered, "◆ rtk git log -1 --format=fuller ◐");
    assert_eq!(
        row.spans.first().unwrap().style.fg,
        Some(ZEVRIA_DARK.roles.tools)
    );
    assert_eq!(
        row.spans.last().unwrap().style.fg,
        Some(ZEVRIA_DARK.feedback.info)
    );
    assert!(
        row.spans[1..row.spans.len() - 1]
            .iter()
            .any(|span| matches!(span.style.fg, Some(Color::Rgb(_, _, _)))),
        "command text should use the Syntect shell grammar"
    );
}

#[test]
fn command_tool_render_handles_multiline_encoded_denied_and_invalid_calls() {
    let encoded =
        serde_json::Value::String(json!({ "command": "printf one\nprintf two" }).to_string());
    let encoded_message = assistant_message(vec![tool_call("encoded", None, "command", encoded)]);
    let executing = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Executing,
        result: None,
        metadata: None,
        subtasks: Default::default(),
    })];
    let mut encoded_lines = Vec::new();
    layout_native_message(
        &encoded_message,
        Some(&executing),
        &mut encoded_lines,
        120,
        None,
    );
    let encoded_text: Vec<String> = encoded_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert_eq!(
        encoded_text,
        vec!["● Assistant", "◆ printf one", "printf two ◐"]
    );

    let denied_message = assistant_message(vec![tool_call(
        "denied",
        None,
        "command",
        json!({ "command": "false" }),
    )]);
    let denied = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Finished,
        result: None,
        metadata: Some(file_metadata(
            "denied",
            None,
            "command",
            ToolCallOutcome::Denied,
            Vec::new(),
        )),
        subtasks: Default::default(),
    })];
    let mut denied_lines = Vec::new();
    layout_native_message(&denied_message, Some(&denied), &mut denied_lines, 120, None);
    let denied_row = &denied_lines[1];
    let denied_text: String = denied_row
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(denied_text, "◆ false ⊘");
    assert_eq!(
        denied_row.spans.last().unwrap().style.fg,
        Some(ZEVRIA_DARK.feedback.error)
    );

    let invalid_arguments = json!({ "value": "missing command" });
    let invalid_message = assistant_message(vec![tool_call(
        "invalid",
        None,
        "command",
        invalid_arguments.clone(),
    )]);
    let finished = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Finished,
        result: None,
        metadata: None,
        subtasks: Default::default(),
    })];
    let mut invalid_lines = Vec::new();
    layout_native_message(
        &invalid_message,
        Some(&finished),
        &mut invalid_lines,
        120,
        None,
    );
    let invalid_text: String = invalid_lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(invalid_text, format!("◆ command({invalid_arguments}) ?"));
}

#[test]
fn correlated_result_finishes_call_without_adding_visible_history() {
    let arguments = json!({ "command": "cargo test --workspace" });
    let output = "secret stdout and stderr";
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                arguments.clone(),
            )]),
        },
    );
    app.select_for_test(cursor(0, 0));

    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        None,
        "executing calls do not have output to copy"
    );

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("fc_1", Some("call_1"), "command", output),
        },
    );

    assert_eq!(app.history().len(), 1);
    assert_eq!(app.selection(), cursor(0, 0));
    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Finished);
    let rendered = rendered_text(&mut app, 160, 14);
    assert!(rendered.contains("◆ cargo test --workspace ?"));
    assert!(!rendered.contains("command("));
    assert!(!rendered.contains(output));
    assert!(!rendered.contains("tool result"));
    assert!(!rendered.contains("fc_1"));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: output.to_string()
        })
    );

    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("final answer"),
        },
    );
    app.select_message_for_test(cursor(1, 0));
    app.handle_event(key(KeyCode::Up));
    assert_eq!(
        app.selection(),
        cursor(0, 0),
        "selection navigation should have no result row to visit"
    );
}

#[test]
fn live_file_result_is_compact_rendered_and_copy_is_metadata_free() {
    let arguments = json!({
        "file_path": "src/lib.rs",
        "replacements": [{
            "old_string": "secret argument old",
            "new_string": "secret argument new",
            "replace_all": false
        }],
        "move_to": null
    });
    let output = "applied edits (1 file changed)";
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "file",
                Some("provider"),
                "edit",
                arguments.clone(),
            )]),
        },
    );

    let executing = rendered_text(&mut app, 120, 12);
    assert!(executing.contains("edit src/lib.rs ◐"));
    assert!(!executing.contains("secret argument old"));
    assert!(!executing.contains("old_string"));

    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message("file", Some("provider"), "edit", output),
            metadata: vec![file_metadata(
                "file",
                Some("provider"),
                "edit",
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "src/lib.rs".into(),
                    change: FileChange::Update {
                        unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                        move_path: None,
                    },
                }],
            )],
        },
    );

    let rendered = rendered_text(&mut app, 120, 24);
    for expected in [
        "edit src/lib.rs ✓",
        output,
        "Edited 1 file",
        "1 - old",
        "1 + new",
    ] {
        assert!(rendered.contains(expected), "missing {expected:?}");
    }
    assert!(!rendered.contains("secret argument new"));

    let buffer = rendered_buffer(&mut app, 120, 24);
    let content = conversation_content_area(&buffer, false);
    for (needle, background) in [
        ("1 - old", ZEVRIA_DARK.content.diff_deletion_background),
        ("1 + new", ZEVRIA_DARK.content.diff_addition_background),
    ] {
        let y = (content.y..content.bottom())
            .find(|&y| {
                (content.x..content.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("missing diff row containing {needle:?}"));
        assert!(
            (content.x..content.x + 4)
                .all(|x| { buffer[(x, y)].bg == ZEVRIA_DARK.surfaces.canvas })
        );
        assert!((content.x + 4..content.right()).all(|x| { buffer[(x, y)].bg == background }));
    }

    app.select_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: output.to_string()
        }),
        "yy copies only the plain model-facing result"
    );
}

#[test]
fn successful_write_and_delete_render_only_one_compact_summary_row() {
    for (tool_name, change, expected_header) in [
        (
            "write",
            FileChange::Add {
                content: "alpha\nomega\n".to_string(),
            },
            "◆ write argument.rs ✓ (+2 -0)",
        ),
        (
            "write",
            FileChange::Update {
                unified_diff: diffy::create_patch(
                    "previous contents\nunchanged context\n",
                    "alpha\nomega\nunchanged context\n",
                )
                .to_string(),
                move_path: None,
            },
            "◆ write argument.rs ✓ (+2 -1)",
        ),
        (
            "delete",
            FileChange::Delete {
                content: "alpha\nomega\n".to_string(),
            },
            "◆ delete argument.rs ✓ (+0 -2)",
        ),
        (
            "write",
            FileChange::Add {
                content: String::new(),
            },
            "◆ write argument.rs ✓ (+0 -0)",
        ),
        (
            "delete",
            FileChange::Delete {
                content: String::new(),
            },
            "◆ delete argument.rs ✓ (+0 -0)",
        ),
        (
            "write",
            FileChange::Update {
                unified_diff: diffy::create_patch("previous contents\n", "").to_string(),
                move_path: None,
            },
            "◆ write argument.rs ✓ (+0 -1)",
        ),
    ] {
        let arguments = if tool_name == "write" {
            json!({"file_path": "argument.rs", "content": "hidden argument"})
        } else {
            json!({"file_path": "argument.rs"})
        };
        let output = "plain result for captured.rs";
        let message =
            assistant_message(vec![tool_call("compact-file", None, tool_name, arguments)]);
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result("compact-file", tool_name, output)),
            metadata: Some(file_metadata(
                "compact-file",
                None,
                tool_name,
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "captured.rs".into(),
                    change,
                }],
            )),
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 120, None);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered.len(), 2, "role header plus one summary row");
        assert_eq!(rendered[1], expected_header);
        let text = rendered.join("\n");
        for hidden in [
            output,
            "hidden argument",
            "alpha",
            "omega",
            "previous contents",
            "unchanged context",
            "Added 1 file",
            "Edited 1 file",
            "Deleted 1 file",
            "captured.rs",
            "diff",
            "@@",
        ] {
            assert!(!text.contains(hidden), "unexpected {hidden:?}");
        }
        assert_eq!(
            lines[1]
                .spans
                .iter()
                .find(|span| span.content.starts_with('+'))
                .and_then(|span| span.style.fg),
            Some(ZEVRIA_DARK.content.diff_addition)
        );
        assert_eq!(
            lines[1]
                .spans
                .iter()
                .find(|span| span.content.starts_with('-'))
                .and_then(|span| span.style.fg),
            Some(ZEVRIA_DARK.content.diff_deletion)
        );
        assert!(
            lines[1].spans.iter().all(|span| span.style.bg.is_none()),
            "{tool_name} summary should remain on the canvas"
        );
    }
}

#[test]
fn no_op_write_renders_only_the_zero_count_header() {
    let message = assistant_message(vec![tool_call(
        "no-op-write",
        None,
        "write",
        json!({"file_path": "same.rs", "content": "unchanged"}),
    )]);
    let states = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Finished,
        result: Some(test_tool_result(
            "no-op-write",
            "write",
            "wrote 9 bytes to same.rs",
        )),
        metadata: Some(file_metadata(
            "no-op-write",
            None,
            "write",
            ToolCallOutcome::Success,
            vec![FileChangeOutput {
                path: "same.rs".into(),
                change: FileChange::Update {
                    unified_diff: String::new(),
                    move_path: None,
                },
            }],
        )),
        subtasks: Default::default(),
    })];
    let mut lines = Vec::new();
    layout_native_message(&message, Some(&states), &mut lines, 120, None);

    assert_eq!(lines.len(), 2);
    assert_eq!(line_text(&lines[1]), "◆ write same.rs ✓ (+0 -0)");
}

#[test]
fn compact_write_and_delete_keep_hidden_arguments_and_plain_results_copyable() {
    for tool_name in ["write", "delete"] {
        let arguments = if tool_name == "write" {
            json!({"file_path": "copy.rs", "content": "hidden body\n"})
        } else {
            json!({"file_path": "copy.rs"})
        };
        let change = if tool_name == "write" {
            FileChange::Add {
                content: "hidden body\n".to_string(),
            }
        } else {
            FileChange::Delete {
                content: "hidden body\n".to_string(),
            }
        };
        let output = format!("plain {tool_name} result");
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id: TEST_TURN_ID,
                message: assistant_message(vec![tool_call(
                    "copy-file",
                    None,
                    tool_name,
                    arguments.clone(),
                )]),
            },
        );

        let executing = rendered_text(&mut app, 100, 16);
        assert!(executing.contains(&format!("{tool_name} copy.rs ◐")));
        assert!(!laid_out_transcript_text(&app).contains("hidden body"));
        assert_eq!(app.view_cache().entries()[0].height, 2);

        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message("copy-file", None, tool_name, &output),
                metadata: vec![file_metadata(
                    "copy-file",
                    None,
                    tool_name,
                    ToolCallOutcome::Success,
                    vec![FileChangeOutput {
                        path: "copy.rs".into(),
                        change,
                    }],
                )],
            },
        );

        let _ = rendered_text(&mut app, 100, 16);
        let rendered = laid_out_transcript_text(&app);
        let counts = if tool_name == "write" {
            "+1 -0"
        } else {
            "+0 -1"
        };
        assert!(rendered.contains(&format!("{tool_name} copy.rs ✓ ({counts})")));
        for hidden in [&output, "hidden body", "file_path", "diff"] {
            assert!(!rendered.contains(hidden), "unexpected {hidden:?}");
        }
        assert_eq!(app.view_cache().entries()[0].height, 2);
        app.select_for_test(cursor(0, 0));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: arguments.to_string(),
            })
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy { text: output }),
            "secondary copy must remain the plain result, not file-change metadata"
        );
    }
}

#[test]
fn defensive_multi_change_write_and_delete_render_only_summaries_and_omission_notices() {
    for tool_name in ["write", "delete"] {
        let output = "changed several files";
        let arguments = if tool_name == "write" {
            json!({"file_path": "argument.rs", "content": "hidden argument"})
        } else {
            json!({"file_path": "argument.rs"})
        };
        let message = assistant_message(vec![tool_call("multi-file", None, tool_name, arguments)]);
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result("multi-file", tool_name, output)),
            metadata: Some(file_metadata(
                "multi-file",
                None,
                tool_name,
                ToolCallOutcome::Success,
                vec![
                    FileChangeOutput {
                        path: "one.rs".into(),
                        change: FileChange::Add {
                            content: "first captured body\n".to_string(),
                        },
                    },
                    FileChangeOutput {
                        path: "two.rs".into(),
                        change: FileChange::Delete {
                            content: "second captured body\n".to_string(),
                        },
                    },
                    FileChangeOutput {
                        path: "three.rs".into(),
                        change: FileChange::Update {
                            unified_diff: diffy::create_patch(
                                "old captured body\n",
                                "new captured body\n",
                            )
                            .to_string(),
                            move_path: None,
                        },
                    },
                    FileChangeOutput {
                        path: "binary.dat".into(),
                        change: FileChange::Omitted {
                            operation: FileChangeOperation::Delete,
                            reason: "deleted file content unavailable: invalid UTF-8".to_string(),
                            added: 0,
                            removed: 0,
                            bytes: 2048,
                        },
                    },
                ],
            )),
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 120, None);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(rendered[1], format!("◆ {tool_name} argument.rs ✓"));
        assert_eq!(
            rendered[2..],
            [
                output,
                "• Added 1 file (+1 -0)",
                "  one.rs (+1 -0)",
                "• Deleted 1 file (+0 -1)",
                "  two.rs (+0 -1)",
                "• Edited 1 file (+1 -1)",
                "  three.rs (+1 -1)",
                "• Deleted 1 file (+0 -0)",
                "  binary.dat (+0 -0)",
                "    ⋮ diff omitted (2048 bytes): deleted file content unavailable: invalid UTF-8",
            ]
        );
        let text = rendered.join("\n");
        for hidden in ["hidden argument", "captured body", "@@", "diff truncated"] {
            assert!(!text.contains(hidden), "unexpected {hidden:?}");
        }
    }
}

#[test]
fn compact_write_and_delete_preserve_omission_reason_bytes_and_counts() {
    for (tool_name, operation, added, removed) in [
        ("write", FileChangeOperation::Add, 12, 0),
        ("write", FileChangeOperation::Update, 12, 8),
        ("delete", FileChangeOperation::Delete, 0, 8),
    ] {
        let arguments = if tool_name == "write" {
            json!({"file_path": "argument.dat", "content": "hidden argument"})
        } else {
            json!({"file_path": "argument.dat"})
        };
        let message = assistant_message(vec![tool_call("omitted", None, tool_name, arguments)]);
        let output = "plain result for captured.dat";
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result("omitted", tool_name, output)),
            metadata: Some(file_metadata(
                "omitted",
                None,
                tool_name,
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "captured.dat".into(),
                    change: FileChange::Omitted {
                        operation,
                        reason: "file content unavailable: invalid UTF-8".to_string(),
                        added,
                        removed,
                        bytes: 4096,
                    },
                }],
            )),
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 120, None);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert_eq!(
            rendered.len(),
            3,
            "role, compact summary, and omission notice"
        );
        assert_eq!(
            rendered[1],
            format!("◆ {tool_name} argument.dat ✓ (+{added} -{removed})")
        );
        assert_eq!(
            rendered[2],
            "    ⋮ diff omitted (4096 bytes): file content unavailable: invalid UTF-8"
        );
        let text = rendered.join("\n");
        for hidden in [
            output,
            "hidden argument",
            "captured.dat",
            "1 file",
            "diff truncated",
        ] {
            assert!(!text.contains(hidden), "unexpected {hidden:?}");
        }
    }
}

#[test]
fn denied_file_result_is_labeled_denied_and_keeps_copyable_output() {
    let arguments = json!({"file_path": "src/lib.rs", "content": "not written"});
    let output = "status: denied\nreason: the tool `write` is unavailable in Plan mode";
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "denied-file",
                Some("provider-denied"),
                "write",
                arguments.clone(),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message("denied-file", Some("provider-denied"), "write", output),
            metadata: vec![
                file_metadata(
                    "denied-file",
                    Some("provider-denied"),
                    "write",
                    ToolCallOutcome::Denied,
                    vec![FileChangeOutput {
                        path: "should-not-render.rs".into(),
                        change: FileChange::Add {
                            content: "metadata should be ignored".to_string(),
                        },
                    }],
                )
                .with_diagnostic("the tool `write` is unavailable in Plan mode"),
            ],
        },
    );

    let _ = rendered_text(&mut app, 100, 16);
    let rendered = laid_out_transcript_text(&app);
    assert!(rendered.contains("write src/lib.rs ⊘"));
    assert!(rendered.contains("the tool `write` is unavailable in Plan mode"));
    assert!(!rendered.contains("status: denied"));
    for hidden in [
        "✗",
        "should-not-render.rs",
        "metadata should be ignored",
        "not written",
        "(+",
    ] {
        assert!(!rendered.contains(hidden), "unexpected {hidden:?}");
    }

    app.select_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string(),
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: output.to_string(),
        })
    );
}

#[test]
fn failed_file_result_is_labeled_and_rendered_as_an_error() {
    let output = "status: error\nerror: file missing.txt does not exist";
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "file",
                None,
                "delete",
                json!({"file_path": "missing.txt"}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message("file", None, "delete", output),
            metadata: vec![
                file_metadata("file", None, "delete", ToolCallOutcome::Error, Vec::new())
                    .with_diagnostic("file missing.txt does not exist"),
            ],
        },
    );

    let _ = rendered_text(&mut app, 100, 16);
    let rendered = laid_out_transcript_text(&app);
    assert!(rendered.contains("delete missing.txt ✗"));
    assert!(rendered.contains("file missing.txt does not exist"));
    assert!(!rendered.contains("status: error"));
    assert!(!rendered.contains("(+"));
}

#[test]
fn restored_compound_file_results_keep_summaries_metadata_and_legacy_fallback() {
    let assistant = assistant_message(vec![
        tool_call(
            "compound",
            Some("call"),
            "write",
            json!({"file_path": "new.rs", "content": "hidden argument"}),
        ),
        tool_call(
            "deletion",
            None,
            "delete",
            json!({"file_path": "obsolete.rs"}),
        ),
        tool_call(
            "legacy",
            None,
            "edit",
            json!({"file_path": "legacy.rs", "replacements": [], "move_to": null}),
        ),
    ]);
    let records = vec![
        TranscriptItem::Message(assistant),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: tool_result_message(
                "compound",
                Some("call"),
                "write",
                "wrote 13 bytes to new.rs",
            ),
            metadata: vec![file_metadata(
                "compound",
                Some("call"),
                "write",
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "new.rs".into(),
                    change: FileChange::Add {
                        content: "fn main() {}\n".to_string(),
                    },
                }],
            )],
        },
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: tool_result_message(
                "deletion",
                None,
                "delete",
                "deleted obsolete.rs (28 bytes)",
            ),
            metadata: vec![file_metadata(
                "deletion",
                None,
                "delete",
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "obsolete.rs".into(),
                    change: FileChange::Delete {
                        content: "const OBSOLETE: bool = true;\n".to_string(),
                    },
                }],
            )],
        },
        TranscriptItem::Message(tool_result_message(
            "legacy",
            None,
            "edit",
            "legacy plain output",
        )),
    ];
    let serialized = serde_json::to_string(&records).expect("serialize compound history");
    let restored: Vec<TranscriptItem> =
        serde_json::from_str(&serialized).expect("restore compound history");
    assert_eq!(
        restored, records,
        "all content and metadata must survive serialization"
    );
    let mut app = App::new();
    app.restore(restored);

    let _ = rendered_text(&mut app, 120, 28);
    let rendered = laid_out_transcript_text(&app);
    assert_eq!(app.history().len(), 1);
    assert_eq!(app.view_cache().entries()[0].height, 7);
    for expected in [
        "write new.rs ✓ (+1 -0)",
        "delete obsolete.rs ✓ (+0 -1)",
        "edit legacy.rs ?",
        "legacy plain output",
    ] {
        assert_eq!(
            rendered.matches(expected).count(),
            1,
            "missing or duplicate {expected:?}"
        );
    }
    for hidden in [
        "wrote 13 bytes",
        "deleted obsolete.rs",
        "hidden argument",
        "1 file",
        "fn main()",
        "OBSOLETE",
        "diff",
    ] {
        assert!(!rendered.contains(hidden), "unexpected {hidden:?}");
    }
}

#[test]
fn restored_write_overwrite_and_delete_keep_complete_metadata_but_constant_height() {
    for body_lines in [1, 1005] {
        let final_marker = "FINAL_RESTORED_CONTENT";
        let padding = "x".repeat(520);
        let mut content = (0..body_lines)
            .map(|index| format!("restored line {index:04} {padding}\n"))
            .collect::<String>();
        content.push_str(final_marker);
        content.push('\n');
        if body_lines == 1005 {
            assert!(content.len() > 512 * 1024, "exercise large stored metadata");
        }
        let previous = content.replacen("restored line", "previous line", 1);
        let mut options = diffy::DiffOptions::new();
        options.set_context_len(content.lines().count());
        let overwrite = options.create_patch(&previous, &content).to_string();
        assert!(
            overwrite.contains(final_marker),
            "overwrite patch keeps full context"
        );
        for (tool_name, change, added, removed) in [
            (
                "write",
                FileChange::Add {
                    content: content.clone(),
                },
                body_lines + 1,
                0,
            ),
            (
                "write",
                FileChange::Update {
                    unified_diff: overwrite,
                    move_path: None,
                },
                1,
                1,
            ),
            (
                "delete",
                FileChange::Delete {
                    content: content.clone(),
                },
                0,
                body_lines + 1,
            ),
        ] {
            let arguments = if tool_name == "write" {
                json!({"file_path": "restored.txt", "content": content})
            } else {
                json!({"file_path": "restored.txt"})
            };
            let call = TranscriptItem::Message(assistant_message(vec![tool_call(
                "restored-file",
                None,
                tool_name,
                arguments,
            )]));
            let output = format!("plain {tool_name} result");
            let metadata = file_metadata(
                "restored-file",
                None,
                tool_name,
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "restored.txt".into(),
                    change,
                }],
            );
            let result = TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: tool_result_message("restored-file", None, tool_name, &output),
                metadata: vec![metadata.clone()],
            };
            let serialized = serde_json::to_string(&result).expect("serialize transcript record");
            assert!(serialized.contains(final_marker));
            let restored_result: TranscriptItem =
                serde_json::from_str(&serialized).expect("restore transcript record");
            assert_eq!(
                restored_result, result,
                "stored file changes must remain complete"
            );
            let mut app = App::new();
            app.restore(vec![call, restored_result]);
            let HistoryEntry::Conversation(entry) = &app.history()[0] else {
                panic!("restored assistant history")
            };
            assert_eq!(
                entry.blocks[0].native_tool().unwrap().1.metadata.as_ref(),
                Some(&metadata),
                "restoration must attach the complete metadata"
            );

            let _ = rendered_text(&mut app, 100, 12);
            assert_eq!(app.handle_event(key(KeyCode::End)), None);
            let visible = rendered_text(&mut app, 100, 12);
            let header = format!("{tool_name} restored.txt ✓ (+{added} -{removed})");
            assert!(visible.contains(&header));
            let rendered = laid_out_transcript_text(&app);
            assert_eq!(app.view_cache().entries()[0].height, 2);
            assert_eq!(app.view_cache().entries()[0].lines.len(), 2);
            assert_eq!(rendered.matches(&header).count(), 1);
            for hidden in [
                output.as_str(),
                final_marker,
                "restored line",
                "previous line",
                "1 file",
                "diff",
                "@@",
            ] {
                assert!(!rendered.contains(hidden), "unexpected {hidden:?}");
            }
        }
    }
}

#[test]
fn duplicate_file_metadata_follows_result_order() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call(
                    "same",
                    Some("same"),
                    "write",
                    json!({"file_path": "one.rs", "content": "one"}),
                ),
                tool_call(
                    "same",
                    Some("same"),
                    "write",
                    json!({"file_path": "two.rs", "content": "two"}),
                ),
            ]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: Message::User {
                content: vec![
                    UserContent::tool_result_with_call_id(
                        "same",
                        "same",
                        "write",
                        vec![ToolResultContent::text("one output")],
                    ),
                    UserContent::tool_result_with_call_id(
                        "same",
                        "same",
                        "write",
                        vec![ToolResultContent::text("two output")],
                    ),
                ],
            },
            metadata: vec![
                file_metadata(
                    "same",
                    Some("same"),
                    "write",
                    ToolCallOutcome::Success,
                    vec![FileChangeOutput {
                        path: "one.rs".into(),
                        change: FileChange::Add {
                            content: "one".to_string(),
                        },
                    }],
                ),
                file_metadata(
                    "same",
                    Some("same"),
                    "write",
                    ToolCallOutcome::Success,
                    vec![FileChangeOutput {
                        path: "two.rs".into(),
                        change: FileChange::Add {
                            content: "two".to_string(),
                        },
                    }],
                ),
            ],
        },
    );

    let HistoryEntry::Conversation(entry) = &app.history()[0] else {
        panic!("assistant history")
    };
    for (index, expected) in [(0, "one.rs"), (1, "two.rs")] {
        assert_eq!(
            entry.blocks[index]
                .native_tool()
                .and_then(|(_, state)| state.metadata.as_ref())
                .and_then(|metadata| metadata.file_changes().first())
                .map(|change| change.path.to_string_lossy().to_string())
                .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn missing_empty_or_mismatched_metadata_keeps_plain_write_and_delete_results() {
    for tool_name in ["write", "delete"] {
        for scenario in ["missing", "empty", "wrong-id", "wrong-tool"] {
            let arguments = if tool_name == "write" {
                json!({"file_path": "file.rs", "content": "hidden argument"})
            } else {
                json!({"file_path": "file.rs"})
            };
            let assistant = assistant_message(vec![tool_call("file", None, tool_name, arguments)]);
            let output = if tool_name == "write" {
                "wrote file.rs\nprevious file content unavailable: invalid UTF-8"
            } else {
                "deleted file.rs (16 bytes)"
            };
            let result = tool_result_message("file", None, tool_name, output);
            let changes = if scenario == "empty" {
                Vec::new()
            } else {
                vec![FileChangeOutput {
                    path: "should-not-render.rs".into(),
                    change: if tool_name == "write" {
                        FileChange::Add {
                            content: "metadata only".to_string(),
                        }
                    } else {
                        FileChange::Delete {
                            content: "metadata only".to_string(),
                        }
                    },
                }]
            };
            let metadata = if scenario == "missing" {
                Vec::new()
            } else {
                vec![file_metadata(
                    if scenario == "wrong-id" {
                        "wrong-id"
                    } else {
                        "file"
                    },
                    None,
                    if scenario == "wrong-tool" {
                        "edit"
                    } else {
                        tool_name
                    },
                    ToolCallOutcome::Success,
                    changes,
                )]
            };
            for restored in [false, true] {
                let mut app = App::new();
                if restored {
                    app.restore(vec![
                        TranscriptItem::Message(assistant.clone()),
                        if scenario == "missing" {
                            TranscriptItem::Message(result.clone())
                        } else {
                            TranscriptItem::ToolResults {
                                skill_applications: Vec::new(),
                                message: result.clone(),
                                metadata: metadata.clone(),
                            }
                        },
                    ]);
                } else {
                    apply_turn_event(
                        &mut app,
                        SessionEvent::Intermediate {
                            display_attempt_id: None,
                            turn_id: TEST_TURN_ID,
                            message: assistant.clone(),
                        },
                    );
                    apply_turn_event(
                        &mut app,
                        SessionEvent::ToolResults {
                            turn_id: TEST_TURN_ID,
                            message: result.clone(),
                            metadata: metadata.clone(),
                        },
                    );
                }

                let _ = rendered_text(&mut app, 100, 14);
                let rendered = laid_out_transcript_text(&app);
                let status = if scenario == "empty" { "✓" } else { "?" };
                assert!(rendered.ends_with(&format!("{tool_name} file.rs {status}\n{output}")));
                assert_eq!(
                    app.view_cache().entries()[0].height,
                    2 + output.lines().count()
                );
                for hidden in [
                    "should-not-render.rs",
                    "metadata only",
                    "hidden argument",
                    "1 file",
                    "diff",
                    "(+",
                ] {
                    assert!(
                        !rendered.contains(hidden),
                        "unexpected {hidden:?}: {tool_name}, {scenario}, restored={restored}"
                    );
                }
            }
        }
    }
}

#[test]
fn write_and_delete_selection_styles_only_the_summary_and_scrolling_never_reveals_content() {
    for tool_name in ["write", "delete"] {
        let content = (0..1010)
            .map(|index| format!("let value_{index} = {index}; // line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let arguments = if tool_name == "write" {
            json!({"file_path": "large.rs", "content": content})
        } else {
            json!({"file_path": "large.rs"})
        };
        let change = if tool_name == "write" {
            FileChange::Add { content }
        } else {
            FileChange::Delete { content }
        };
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id: TEST_TURN_ID,
                message: assistant_message(vec![tool_call("file", None, tool_name, arguments)]),
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message("file", None, tool_name, "hidden plain result"),
                metadata: vec![file_metadata(
                    "file",
                    None,
                    tool_name,
                    ToolCallOutcome::Success,
                    vec![FileChangeOutput {
                        path: "large.rs".into(),
                        change,
                    }],
                )],
            },
        );
        let counts = if tool_name == "write" {
            "+1010 -0"
        } else {
            "+0 -1010"
        };
        let header = format!("◆ {tool_name} large.rs ✓ ({counts})");
        for width in [60, 100] {
            app.select_for_test(None);
            let _ = rendered_text(&mut app, width, 12);
            for scroll in [
                KeyCode::PageDown,
                KeyCode::End,
                KeyCode::PageUp,
                KeyCode::Home,
            ] {
                assert_eq!(app.handle_event(key(scroll)), None);
                let visible = rendered_text(&mut app, width, 12);
                assert!(visible.contains(&header));
                assert_eq!(app.view_cache().entries()[0].height, 2);
                let rendered = laid_out_transcript_text(&app);
                for hidden in ["value_", "hidden plain result", "diff", "1 file"] {
                    assert!(!rendered.contains(hidden), "unexpected {hidden:?}");
                }
            }
            app.select_for_test(cursor(0, 0));
            let buffer = rendered_buffer(&mut app, width, 12);
            let entry = &app.view_cache().entries()[0];
            assert_eq!(entry.height, 2);
            assert_eq!(entry.lines.len(), 2);
            assert_eq!(entry.selection, Some(RowRange::new(1, 2)));
            assert_eq!(line_text(&entry.lines[1]), header);
            let rendered = laid_out_transcript_text(&app);
            for hidden in ["value_", "hidden plain result", "diff", "1 file"] {
                assert!(!rendered.contains(hidden), "unexpected selected {hidden:?}");
            }
            let layout = crate::frame_layout::FrameLayout::compute(
                buffer.area,
                false,
                crate::frame_layout::LowerSurface::Composer {
                    requested_height: 3,
                },
                false,
            );
            let content = layout.conversation_content;
            let band = layout.selection_band;
            let y = (content.y..content.bottom())
                .find(|&y| {
                    (content.x..content.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains(&header)
                })
                .expect("selected summary row");
            for x in buffer.area.x..buffer.area.right() {
                let selected = (band.x..band.right()).contains(&x);
                assert_eq!(buffer[(x, y)].bg == SELECTION_BG, selected);
                if selected {
                    assert_eq!(
                        buffer[(x, y)].fg,
                        ZEVRIA_DARK.surfaces.selection_foreground,
                        "selection overrides tool, status, and line-count foregrounds"
                    );
                }
            }
        }
    }
}

#[test]
fn selected_edit_keeps_the_1000_wrapped_row_limit_and_notice() {
    let content = (0..1010)
        .map(|index| format!("let edited_{index} = {index}; // line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "edit-file",
                None,
                "edit",
                json!({"file_path": "large.rs", "replacements": [], "move_to": null}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "edit-file",
                None,
                "edit",
                "applied edits (1 file changed)",
            ),
            metadata: vec![file_metadata(
                "edit-file",
                None,
                "edit",
                ToolCallOutcome::Success,
                vec![FileChangeOutput {
                    path: "large.rs".into(),
                    change: FileChange::Update {
                        unified_diff: diffy::create_patch("", &content).to_string(),
                        move_path: None,
                    },
                }],
            )],
        },
    );
    app.select_for_test(cursor(0, 0));

    let mut terminal = Terminal::new(TestBackend::new(100, 1020)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("render");
    let buffer = terminal.backend().buffer();
    let mut truncation_rows = 0;
    let rendered = (0..buffer.area.height)
        .map(|y| {
            let row = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            if row.contains("diff truncated after 1000 rendered lines") {
                truncation_rows += 1;
                assert!((1..99).all(|x| buffer[(x, y)].bg == SELECTION_BG));
            }
            row
        })
        .collect::<String>();
    assert!(rendered.contains("edit large.rs ✓"));
    assert!(rendered.contains("applied edits (1 file changed)"));
    assert!(rendered.contains("Edited 1 file"));
    assert_eq!(truncation_rows, 1);
    assert!(!rendered.contains("edited_1009"));
}

#[test]
fn selection_overrides_nested_markdown_and_syntax_foregrounds_last() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::assistant(
        "# Zevria heading\n\n```rust\nlet value = 42; // selected comment\n```",
    )));
    app.select_for_test(cursor(0, 0));

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("render");
    let buffer = terminal.backend().buffer();
    for (needle, modifier) in [
        ("# Zevria heading", Modifier::BOLD),
        ("let value = 42", Modifier::empty()),
        ("selected comment", Modifier::ITALIC),
    ] {
        let (y, start) = (0..buffer.area.height)
            .find_map(|y| {
                let row = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                row.find(needle).map(|start| (y, start as u16))
            })
            .unwrap_or_else(|| panic!("missing selected row containing {needle:?}"));
        for x in start..start + u16::try_from(needle.len()).expect("ASCII test width") {
            let cell = &buffer[(x, y)];
            assert_eq!(
                cell.fg, ZEVRIA_DARK.surfaces.selection_foreground,
                "foreground for {needle:?}"
            );
            assert_eq!(
                cell.bg, ZEVRIA_DARK.surfaces.selection_background,
                "background for {needle:?}"
            );
        }
        if !modifier.is_empty() {
            assert!(buffer[(start, y)].modifier.contains(modifier));
        }
    }
}

#[test]
fn call_id_disambiguates_repeated_tool_ids() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call("shared", Some("provider_1"), "first", json!({})),
                tool_call("shared", Some("provider_2"), "second", json!({})),
            ]),
        },
    );

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("shared", Some("provider_1"), "first", "first output"),
        },
    );

    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Finished);
    assert_eq!(tool_status(&app, 0, 1), ToolCallStatus::Executing);
    app.select_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "{}".to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "first output".to_string()
        })
    );

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("shared", Some("provider_2"), "second", "second output"),
        },
    );

    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Finished);
    assert_eq!(tool_status(&app, 0, 1), ToolCallStatus::Finished);
    assert_eq!(app.history().len(), 1, "both result batches stay hidden");
    app.select_for_test(cursor(0, 1));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "{}".to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "second output".to_string()
        })
    );
}

#[test]
fn exact_call_id_match_beats_a_missing_call_id_fallback() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call("shared", None, "legacy", json!({ "slot": "legacy" })),
                tool_call(
                    "shared",
                    Some("provider_2"),
                    "exact",
                    json!({ "slot": "exact" }),
                ),
            ]),
        },
    );

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("shared", Some("provider_2"), "exact", "exact output"),
        },
    );

    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Executing);
    assert_eq!(tool_status(&app, 0, 1), ToolCallStatus::Finished);
    app.select_for_test(cursor(0, 1));
    app.handle_event(key(KeyCode::Char('y')));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "exact output".to_string()
        })
    );

    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("shared", None, "legacy", "legacy output"),
        },
    );
    app.select_for_test(cursor(0, 0));
    app.handle_event(key(KeyCode::Char('y')));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "legacy output".to_string()
        })
    );
}

#[test]
fn duplicate_call_identifiers_keep_result_order() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call(
                    "duplicate",
                    Some("same_call"),
                    "first",
                    json!({ "slot": 1 }),
                ),
                tool_call(
                    "duplicate",
                    Some("same_call"),
                    "second",
                    json!({ "slot": 2 }),
                ),
            ]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::User {
                content: vec![
                    UserContent::tool_result_with_call_id(
                        "duplicate",
                        "same_call",
                        "first",
                        vec![ToolResultContent::text("first output")],
                    ),
                    UserContent::tool_result_with_call_id(
                        "duplicate",
                        "same_call",
                        "second",
                        vec![ToolResultContent::text("second output")],
                    ),
                ],
            },
        },
    );

    for (content_index, expected) in [(0, "first output"), (1, "second output")] {
        app.select_for_test(cursor(0, content_index));
        app.handle_event(key(KeyCode::Char('y')));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: expected.into()
            })
        );
    }
}

#[test]
fn tool_error_result_still_finishes_without_rendering_output() {
    let arguments = json!({ "command": "false" });
    let output = "status: error\nexit code: 1\nstderr: command failed";
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "failed",
                None,
                "command",
                arguments.clone(),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("failed", None, "command", output),
        },
    );

    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Finished);
    let rendered = rendered_text(&mut app, 100, 14);
    assert!(
        rendered.contains("◆ false ?"),
        "metadata-free output cannot prove an outcome"
    );
    assert!(!rendered.contains("status: error"));
    assert!(!rendered.contains("exit code"));
    assert!(!rendered.contains("command failed"));

    app.select_for_test(cursor(0, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: arguments.to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: output.to_string()
        })
    );
}

#[test]
fn mixed_user_intermediate_preserves_only_non_tool_content() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                json!({}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::User {
                content: vec![
                    UserContent::tool_result_with_call_id(
                        "fc_1",
                        "call_1",
                        "command",
                        vec![ToolResultContent::text("hidden output")],
                    ),
                    UserContent::Text(Text::new("visible note")),
                ],
            },
        },
    );

    assert_eq!(app.history().len(), 2);
    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Finished);
    app.select_for_test(cursor(1, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "visible note".to_string()
        })
    );
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("visible note"));
    assert!(!rendered.contains("hidden output"));
}

#[test]
fn terminal_events_interrupt_unmatched_executing_calls() {
    let call = || {
        assistant_message(vec![tool_call(
            "fc_1",
            Some("call_1"),
            "command",
            json!({}),
        )])
    };

    let mut completed = App::new();
    apply_turn_event(
        &mut completed,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: call(),
        },
    );
    apply_turn_event(
        &mut completed,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("done"),
        },
    );
    assert_eq!(tool_status(&completed, 0, 0), ToolCallStatus::Interrupted);

    let mut failed = App::new();
    apply_turn_event(
        &mut failed,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: call(),
        },
    );
    apply_turn_event(
        &mut failed,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "provider failed".to_string(),
        },
    );
    assert_eq!(tool_status(&failed, 0, 0), ToolCallStatus::Interrupted);
    assert!(matches!(
        failed.history().last(),
        Some(HistoryEntry::Error(_))
    ));

    let mut cancelled = App::new();
    apply_turn_event(
        &mut cancelled,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: call(),
        },
    );
    apply_turn_event(
        &mut cancelled,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    );
    assert_eq!(tool_status(&cancelled, 0, 0), ToolCallStatus::Interrupted);
}

#[test]
fn result_finishes_most_recent_executing_call_with_reused_id() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "reused",
                None,
                "command",
                json!({"turn": 1}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "reused",
                None,
                "command",
                json!({"turn": 2}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("reused", None, "command", "latest output"),
        },
    );

    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Executing);
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
}

#[test]
fn orphan_tool_result_leaves_executing_calls_untouched() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "known",
                Some("call_1"),
                "command",
                json!({}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("unknown", None, "unknown", "orphan output"),
        },
    );

    assert_eq!(app.history().len(), 1, "result-only messages add no entry");
    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Executing);
    assert!(app.has_executing_tool_calls());
}

#[test]
fn executing_call_tracking_clears_on_completion_and_restore() {
    let mut app = App::new();
    assert!(!app.has_executing_tool_calls());
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                json!({}),
            )]),
        },
    );
    assert!(app.has_executing_tool_calls());
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("done"),
        },
    );
    assert!(!app.has_executing_tool_calls());

    let mut restored = App::new();
    restored.restore(vec![
        TranscriptItem::provider_message(ProviderReplay::openai_responses(
            zevria_foundation::ModelProfileRef::new("test", "model"),
            vec![json!({
                "type": "function_call",
                "id": "fc_lost",
                "call_id": "call_lost",
                "name": "command",
                "arguments": "{}",
                "status": "completed"
            })],
        ))
        .expect("provider replay should restore its canonical tool call"),
    ]);
    assert!(!restored.has_executing_tool_calls());
    assert_eq!(tool_status(&restored, 0, 0), ToolCallStatus::Interrupted);
}

#[test]
fn mixed_reasoning_filters_opaque_parts_from_presentation_render_and_copy() {
    let entry = HistoryEntry::from_message(mixed_reasoning_message(), ToolCallStatus::Finished)
        .expect("mixed reasoning should remain visible");
    let HistoryEntry::Conversation(conversation) = &entry else {
        panic!("expected a conversation entry")
    };
    assert_eq!(conversation.blocks.len(), 1);
    assert_eq!(
        conversation.blocks[0].role,
        Some(PresentationRole::Assistant)
    );
    let PresentationBlockKind::Reasoning { parts } = &conversation.blocks[0].kind else {
        panic!("expected a reasoning block")
    };
    assert_eq!(
        parts,
        &[READABLE_REASONING_SUMMARY, READABLE_REASONING_TEXT]
    );

    let mut app = App::new();
    app.seed_history_entry(entry);
    app.select_for_test(cursor(0, 0));
    let rendered = rendered_text(&mut app, 100, 12);
    let summary_index = rendered
        .find(READABLE_REASONING_SUMMARY)
        .expect("summary should render");
    let text_index = rendered
        .find(READABLE_REASONING_TEXT)
        .expect("reasoning text should render");
    assert!(
        summary_index < text_index,
        "readable parts must keep source order"
    );
    assert_opaque_reasoning_absent(
        &rendered,
        &[
            MIXED_ENCRYPTED_REASONING_PAYLOAD,
            MIXED_REDACTED_REASONING_PAYLOAD,
            MIXED_REASONING_SIGNATURE,
        ],
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: format!("{READABLE_REASONING_SUMMARY}\n{READABLE_REASONING_TEXT}")
        })
    );
}

#[test]
fn opaque_only_reasoning_has_no_heading_or_selectable_block() {
    assert!(
        HistoryEntry::from_message(opaque_reasoning_message(), ToolCallStatus::Finished).is_none()
    );
}

#[test]
fn live_opaque_only_reasoning_does_not_render_a_heading() {
    let message = opaque_reasoning_message();
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (message.clone()).into(),
        },
    );
    assert_eq!(app.streaming(), Some(message));

    let rendered = rendered_text(&mut app, 80, 10);
    assert!(app.view_cache().streaming().is_none());
    assert!(!rendered.contains("reasoning"));
    assert_opaque_reasoning_absent(
        &rendered,
        &[
            OPAQUE_ENCRYPTED_REASONING_PAYLOAD,
            OPAQUE_REDACTED_REASONING_PAYLOAD,
        ],
    );
}

#[test]
fn restored_opaque_only_reasoning_has_no_empty_selection() {
    let mut app = App::new();
    app.restore(vec![TranscriptItem::Message(opaque_reasoning_message())]);
    assert!(app.history().is_empty());
    let rendered = rendered_text(&mut app, 80, 10);
    assert!(!rendered.contains("reasoning"));
    assert_opaque_reasoning_absent(
        &rendered,
        &[
            OPAQUE_ENCRYPTED_REASONING_PAYLOAD,
            OPAQUE_REDACTED_REASONING_PAYLOAD,
        ],
    );
}

#[test]
fn legacy_message_renderer_uses_the_shared_readable_reasoning_filter() {
    let mut mixed_lines = Vec::new();
    layout_native_message(
        &mixed_reasoning_message(),
        None,
        &mut mixed_lines,
        100,
        None,
    );
    let mixed_text = mixed_lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    let summary_index = mixed_text
        .find(READABLE_REASONING_SUMMARY)
        .expect("summary should render");
    let text_index = mixed_text
        .find(READABLE_REASONING_TEXT)
        .expect("reasoning text should render");
    assert!(summary_index < text_index);
    assert_opaque_reasoning_absent(
        &mixed_text,
        &[
            MIXED_ENCRYPTED_REASONING_PAYLOAD,
            MIXED_REDACTED_REASONING_PAYLOAD,
            MIXED_REASONING_SIGNATURE,
        ],
    );

    let mut opaque_lines = Vec::new();
    let selection = layout_native_message(
        &opaque_reasoning_message(),
        None,
        &mut opaque_lines,
        100,
        Some(0),
    );
    assert_eq!(selection, None);
    assert!(opaque_lines.is_empty());
    let opaque_text = opaque_lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_opaque_reasoning_absent(
        &opaque_text,
        &[
            OPAQUE_ENCRYPTED_REASONING_PAYLOAD,
            OPAQUE_REDACTED_REASONING_PAYLOAD,
        ],
    );
}

#[test]
fn restored_calls_are_finished_and_result_content_is_omitted() {
    let mut app = App::new();
    app.set_mode_for_test(SessionMode::Plan);
    app.restore_plan_state(PlanWorkflowState::Ready {
        artifact: test_plan_artifact(),
    });
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Plan));
    app.restore(vec![
        TranscriptItem::Message(Message::user("earlier question")),
        TranscriptItem::Message(assistant_message(vec![tool_call(
            "fc_1",
            Some("call_1"),
            "command",
            json!({ "command": "cargo test" }),
        )])),
        TranscriptItem::Message(tool_result_message(
            "fc_1",
            Some("call_1"),
            "command",
            "restored output",
        )),
        TranscriptItem::Message(Message::User {
            content: vec![
                UserContent::tool_result(
                    "orphan",
                    "unknown",
                    vec![ToolResultContent::text("orphan output")],
                ),
                UserContent::Text(Text::new("restored note")),
            ],
        }),
        TranscriptItem::Message(Message::assistant("earlier answer")),
        TranscriptItem::Error {
            error: "old failure".to_string(),
        },
    ]);

    assert_eq!(app.history().len(), 5);
    assert_eq!(app.next_mode(), SessionMode::Build);
    assert_eq!(app.in_flight_mode(), None);
    assert_eq!(app.plan_state(), PlanWorkflowState::Idle);
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
    app.select_for_test(cursor(1, 0));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "{\"command\":\"cargo test\"}".to_string()
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "restored output".to_string()
        })
    );
    let text = rendered_text(&mut app, 100, 30);
    for expected in [
        "earlier question",
        "◆ cargo test ?",
        "restored note",
        "earlier answer",
        "old failure",
    ] {
        assert!(
            text.contains(expected),
            "buffer missing {expected:?} in {text:?}"
        );
    }
    for hidden in ["restored output", "orphan output", "tool result"] {
        assert!(
            !text.contains(hidden),
            "buffer unexpectedly contains {hidden:?}"
        );
    }
}

#[test]
fn renders_roles_markdown_reasoning_and_finished_tool_calls() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::user("hello there")));
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::Reasoning(Reasoning::summaries(vec!["pondering".to_string()])),
            AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new_or_mint("call_1"),
                ToolFunction::new("get_weather".to_string(), json!({ "city": "Paris" })),
            )),
            AssistantContent::Text(Text::new("# Heading\n\n- alpha\n- beta")),
        ],
    }));

    let text = rendered_text(&mut app, 70, 30);
    for expected in [
        "You",
        "hello there",
        "Assistant",
        "reasoning",
        "pondering",
        "get_weather",
        "?",
        "Heading",
        "alpha",
        "beta",
    ] {
        assert!(
            text.contains(expected),
            "buffer missing {expected:?} in {text:?}"
        );
    }
}

#[test]
fn question_history_renders_and_restores_compact_answer_summaries() {
    let arguments = json!({
        "questions": [
            {
                "id": "scope",
                "header": "Scope",
                "question": "Which scope?",
                "options": [
                    {"label": "Focused", "description": "Keep it narrow."},
                    {"label": "Broad", "description": "Include adjacent work."}
                ]
            },
            {
                "id": "tests",
                "header": "Tests",
                "question": "Which tests?",
                "options": [
                    {"label": "Focused", "description": "Feature tests."},
                    {"label": "Full", "description": "Workspace tests."}
                ]
            }
        ]
    });
    let assistant = assistant_message(vec![tool_call(
        "question-call",
        None,
        QUESTION_TOOL_NAME,
        arguments,
    )]);
    let response = QuestionResponse::Answered {
        answers: vec![
            QuestionAnswer {
                id: "scope".to_string(),
                answer: Some(QuestionAnswerValue::String("Focused".to_string())),
            },
            QuestionAnswer {
                id: "tests".to_string(),
                answer: Some(QuestionAnswerValue::String("Full".to_string())),
            },
        ],
    };
    let result = tool_result_message(
        "question-call",
        None,
        QUESTION_TOOL_NAME,
        serde_json::to_string(&response).expect("response json"),
    );
    let metadata = file_metadata(
        "question-call",
        None,
        QUESTION_TOOL_NAME,
        ToolCallOutcome::Success,
        Vec::new(),
    );

    let mut live = App::new();
    apply_turn_event(
        &mut live,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant.clone(),
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: result.clone(),
            metadata: vec![metadata.clone()],
        },
    );
    let live_text = rendered_text(&mut live, 80, 20);

    let mut restored = App::new();
    restored.restore(vec![
        TranscriptItem::Message(assistant),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: result,
            metadata: vec![metadata],
        },
    ]);
    let restored_text = rendered_text(&mut restored, 80, 20);
    for expected in ["question · 2 prompts ✓", "Scope: Focused", "Tests: Full"] {
        assert!(
            live_text.contains(expected),
            "live history missing {expected:?}"
        );
        assert!(
            restored_text.contains(expected),
            "restored history missing {expected:?}"
        );
    }
    assert!(!live_text.contains("Which scope?"));
}

#[test]
fn question_status_icons_preserve_specialized_colors_and_dismissal() {
    let message = assistant_message(vec![tool_call(
        "question-icon",
        None,
        QUESTION_TOOL_NAME,
        json!({"questions": [{"id": "scope", "header": "Scope"}]}),
    )]);
    for (status, outcome, dismissed, glyph, color) in [
        (
            ToolCallStatus::Executing,
            None,
            false,
            "◐",
            ZEVRIA_DARK.feedback.info,
        ),
        (
            ToolCallStatus::Interrupted,
            None,
            false,
            "◼",
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Success),
            false,
            "✓",
            ZEVRIA_DARK.feedback.success,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Success),
            true,
            "–",
            ZEVRIA_DARK.text.muted,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Error),
            true,
            "✗",
            ZEVRIA_DARK.feedback.error,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Denied),
            true,
            "⊘",
            ZEVRIA_DARK.feedback.error,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Cancelled),
            true,
            "◼",
            ZEVRIA_DARK.feedback.warning,
        ),
    ] {
        let response = if dismissed {
            QuestionResponse::Dismissed
        } else {
            QuestionResponse::Answered {
                answers: Vec::new(),
            }
        };
        let mut state = crate::app::ToolCallState::new(
            status,
            &json!({"questions": [{"id": "scope", "header": "Scope"}]}),
        );
        if let Some(outcome) = outcome {
            state.result = Some(test_tool_result(
                "question-icon",
                QUESTION_TOOL_NAME,
                &serde_json::to_string(&response).unwrap(),
            ));
            state.metadata = Some(file_metadata(
                "question-icon",
                None,
                QUESTION_TOOL_NAME,
                outcome,
                Vec::new(),
            ));
        }
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&[Some(state)]), &mut lines, 80, None);
        assert_eq!(line_text(&lines[1]), format!("◆ question · Scope {glyph}"));
        assert_eq!(lines[1].spans.last().unwrap().style.fg, Some(color));
    }
}

#[test]
fn task_history_renders_and_restores_a_compact_checklist() {
    let arguments = json!({
        "explanation": "Implementation is underway.",
        "tasks": [
            {"step": "Inspect the workflow", "status": "completed"},
            {"step": "Implement the task tool", "status": "in_progress"},
            {"step": "Run workspace tests", "status": "pending"}
        ]
    });
    let assistant = assistant_message(vec![tool_call(
        "task-call",
        None,
        TASK_TOOL_NAME,
        arguments,
    )]);
    let result = tool_result_message(
        "task-call",
        None,
        TASK_TOOL_NAME,
        "Task list updated: 1/3 completed, 1 in progress, 1 pending.",
    );
    let metadata = file_metadata(
        "task-call",
        None,
        TASK_TOOL_NAME,
        ToolCallOutcome::Success,
        Vec::new(),
    );

    let mut live = App::new();
    apply_turn_event(
        &mut live,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant.clone(),
        },
    );
    let executing = rendered_text(&mut live, 80, 20);
    assert!(executing.contains("task · 1/3 completed ◐"));

    apply_turn_event(
        &mut live,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: result.clone(),
            metadata: vec![metadata.clone()],
        },
    );
    let live_text = rendered_text(&mut live, 80, 20);

    let mut restored = App::new();
    restored.restore(vec![
        TranscriptItem::Message(assistant),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: result,
            metadata: vec![metadata],
        },
    ]);
    let restored_text = rendered_text(&mut restored, 80, 20);
    for expected in [
        "task · 1/3 completed •",
        "Implementation is underway.",
        "✓ Inspect the workflow",
        "◐ Implement the task tool",
        "○ Run workspace tests",
    ] {
        assert!(
            live_text.contains(expected),
            "live history missing {expected:?}"
        );
        assert!(
            restored_text.contains(expected),
            "restored history missing {expected:?}"
        );
    }
    assert!(!live_text.contains("Task list updated:"));
    assert!(!live_text.contains("in_progress"));
}

#[test]
fn uncorrelated_task_updates_render_as_interrupted_after_restore_failure_and_cancellation() {
    let call = || {
        assistant_message(vec![tool_call(
            "task-call",
            None,
            TASK_TOOL_NAME,
            json!({
                "tasks": [
                    {"step": "Finish the implementation", "status": "in_progress"}
                ]
            }),
        )])
    };

    let mut restored = App::new();
    restored.restore(vec![TranscriptItem::Message(call())]);

    let mut failed = App::new();
    apply_turn_event(
        &mut failed,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: call(),
        },
    );
    apply_turn_event(
        &mut failed,
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "persistence failed".to_string(),
        },
    );

    let mut cancelled = App::new();
    apply_turn_event(
        &mut cancelled,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: call(),
        },
    );
    apply_turn_event(
        &mut cancelled,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    );

    for app in [&mut restored, &mut failed, &mut cancelled] {
        assert_eq!(tool_status(app, 0, 0), ToolCallStatus::Interrupted);
        let rendered = rendered_text(app, 80, 12);
        assert!(rendered.contains("task · 0/1 completed ◼"));
        assert!(!rendered.contains(" •"));
    }

    // Rendering also defends the invariant directly: `Finished` without a
    // successful correlated result is never enough evidence for `updated`.
    let mut missing_result = App::new();
    missing_result.seed_history_entry(
        HistoryEntry::from_message(call(), ToolCallStatus::Finished)
            .expect("task call should be visible"),
    );
    let rendered = rendered_text(&mut missing_result, 80, 12);
    assert!(rendered.contains("task · 0/1 completed ?"));
    assert!(!rendered.contains(" •"));
}

#[test]
fn launch_rows_render_compactly_with_lifecycle_without_leaking_private_details() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("investigate the config"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: launch_assistant("call-launch", "fallback args title", "SECRET quick prompt"),
        },
    );

    // No launch row is shown until a child is accepted or the batch ends.
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(!rendered.contains("subtasks ·"));
    assert!(!rendered.contains("subtask launch"));
    assert!(!rendered.contains("fallback args title · starting"));
    assert!(!rendered.contains("fallback args title · running"));
    assert!(!rendered.contains("SECRET"));
    assert!(!rendered.contains("launch_subtasks"));

    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "call-launch".to_string(),
            entry_index: 0,
            descriptor: child_descriptor("child-1", "map config loading"),
        },
    );
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("◆ explore · map config loading ◐"));

    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: SubtaskId::new("child-1"),
            status: SubtaskStatus::Running,
        },
    );
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("◆ explore · map config loading ◐"));
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Executing);
    assert!(app.has_executing_tool_calls());

    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: SubtaskId::new("child-1"),
            status: SubtaskStatus::Completed,
        },
    );
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("◆ explore · map config loading ✓"));
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Executing);
    assert!(app.has_executing_tool_calls());

    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "fc-call-launch",
                Some("call-launch"),
                "launch_subtasks",
                "status: launched\nid: child-1",
            ),
            metadata: vec![launch_metadata(
                "call-launch",
                "child-1",
                "map config loading",
                ToolCallOutcome::Success,
            )],
        },
    );

    // The attached descriptor names and statuses the row; nothing private or
    // generic about the call is rendered.
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("◆ explore · map config loading ✓"));
    assert!(!rendered.contains("fallback args title"));
    assert!(!rendered.contains("SECRET"));
    assert!(!rendered.contains("status: launched"));
    assert!(!rendered.contains("executing"));
    assert!(!rendered.contains("finished"));
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
    assert!(!app.has_executing_tool_calls());
}

#[test]
fn partial_launch_notices_appear_only_after_execution_stops() {
    for cancelled in [false, true] {
        for string_arguments in [false, true] {
            let mut app = App::new();
            let arguments = two_child_launch_arguments();
            let arguments = if string_arguments {
                json!(arguments.to_string())
            } else {
                arguments
            };
            apply_turn_event(
                &mut app,
                SessionEvent::TurnStarted {
                    turn_id: TEST_TURN_ID,
                    message: Message::user("launch two children"),
                    mode: SessionMode::Build,
                },
            );
            apply_turn_event(
                &mut app,
                SessionEvent::Intermediate {
                    display_attempt_id: None,
                    turn_id: TEST_TURN_ID,
                    message: assistant_message(vec![tool_call(
                        "fc-partial",
                        Some("partial"),
                        "launch_subtasks",
                        arguments,
                    )]),
                },
            );
            rendered_text(&mut app, 100, 20);
            assert!(app.view_cache().entries()[1].lines.is_empty());
            apply_turn_event(
                &mut app,
                SessionEvent::SubtaskLaunched {
                    turn_id: TEST_TURN_ID,
                    call_id: "partial".into(),
                    entry_index: 0,
                    descriptor: child_descriptor("child-first", "first child"),
                },
            );

            let rendered = rendered_text(&mut app, 100, 20);
            assert!(rendered.contains("◆ explore · first child ◐"));
            assert!(!rendered.contains("not launched"));
            assert!(!rendered.contains("subtasks ·"));
            assert!(!rendered.contains("subtask launch"));
            assert_eq!(app.view_cache().entries()[1].lines.len(), 2);

            if cancelled {
                apply_turn_event(
                    &mut app,
                    SessionEvent::TurnCancelled {
                        turn_id: TEST_TURN_ID,
                    },
                );
                assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Interrupted);
            } else {
                let mut metadata = launch_metadata(
                    "partial",
                    "child-first",
                    "first child",
                    ToolCallOutcome::Error,
                );
                let mut entries = metadata.subtasks().to_vec();
                entries.push(zevria_foundation::SubtaskEntryMetadata {
                    index: 1,
                    status: SubtaskStatus::Failed,
                    launch: None,
                });
                metadata.detail = Some(ToolResultDetail::Subtasks(entries));
                apply_turn_event(
                    &mut app,
                    SessionEvent::ToolResults {
                        turn_id: TEST_TURN_ID,
                        message: tool_result_message(
                            "fc-partial",
                            Some("partial"),
                            "launch_subtasks",
                            "status: error\nerror: SECRET batch result",
                        ),
                        metadata: vec![metadata],
                    },
                );
                assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
            }

            let rendered = rendered_text(&mut app, 100, 20);
            assert!(rendered.contains("1 of 2 subtasks not launched"));
            assert!(!rendered.contains("SECRET"));
            assert!(!rendered.contains("subtask launch"));
            let lines = &app.view_cache().entries()[1].lines;
            assert_eq!(lines.len(), 3, "header, partial notice, then child row");
            assert_eq!(line_text(&lines[1]), "◆ 1 of 2 subtasks not launched");
            assert_eq!(
                lines[1].spans.last().unwrap().style.fg,
                Some(ZEVRIA_DARK.feedback.error)
            );
            assert!(line_text(&lines[2]).starts_with("◆ explore · first child "));
        }
    }
}

#[test]
fn successful_launch_batches_have_only_child_rows_and_skip_the_batch_in_navigation() {
    let message = assistant_message(vec![tool_call(
        "fc-clean",
        Some("clean"),
        "launch_subtasks",
        two_child_launch_arguments(),
    )]);
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("launch two children"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: message.clone(),
        },
    );
    let mut entries = Vec::new();
    for (index, title) in ["first child", "second child"].into_iter().enumerate() {
        let id = format!("child-{index}");
        apply_turn_event(
            &mut app,
            SessionEvent::SubtaskLaunched {
                turn_id: TEST_TURN_ID,
                call_id: "clean".into(),
                entry_index: index,
                descriptor: child_descriptor(&id, title),
            },
        );
        entries.push(zevria_foundation::SubtaskEntryMetadata {
            index,
            status: SubtaskStatus::Completed,
            launch: Some(child_launch(&id, title)),
        });
    }
    let metadata = ToolResultMetadata {
        detail: Some(ToolResultDetail::Subtasks(entries)),
        ..unlaunched_metadata("clean", ToolCallOutcome::Success)
    };
    // Exercise the same keyboard path both during execution and after completion.
    for finished in [false, true] {
        if finished {
            apply_turn_event(
                &mut app,
                SessionEvent::ToolResults {
                    turn_id: TEST_TURN_ID,
                    message: tool_result_message(
                        "fc-clean",
                        Some("clean"),
                        "launch_subtasks",
                        "SECRET child reports",
                    ),
                    metadata: vec![metadata.clone()],
                },
            );
        }
        let rendered = rendered_text(&mut app, 100, 20);
        assert!(!rendered.contains("subtasks ·"));
        assert!(!rendered.contains("subtask launch"));
        assert!(!rendered.contains("SECRET"));
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(1, 2));
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.selection_scope(), Some(SelectionScope::Block));
        app.handle_event(key(KeyCode::Char('k')));
        assert_eq!(app.selection(), cursor(1, 1));
        app.handle_event(key(KeyCode::Char('k')));
        assert_eq!(
            app.selection(),
            cursor(1, 1),
            "the hidden batch is not selectable"
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            Some(UiAction::OpenSubtask {
                id: SubtaskId::new("child-0")
            })
        );
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Esc));
        assert!(app.interaction().is_normal());
    }

    let HistoryEntry::Conversation(entry) = &app.history()[1] else {
        panic!("expected launch conversation");
    };
    let (_, state) = entry.blocks[0].native_tool().unwrap();
    let mut lines = Vec::new();
    layout_native_message(
        &message,
        Some(&[Some(state.clone())]),
        &mut lines,
        100,
        None,
    );
    assert_eq!(
        lines.iter().map(line_text).collect::<Vec<_>>(),
        [
            "● Assistant",
            "◆ explore · first child ✓",
            "◆ explore · second child ✓",
        ],
        "no summary or blank row may separate the header from the children"
    );
}

#[test]
fn live_prelaunch_failures_show_status_and_reason_without_opening_a_child() {
    for (outcome, output, status, diagnostic) in [
        (
            ToolCallOutcome::Error,
            "status: error\nerror: title must not be empty; provide a nonblank label for the subtask",
            "✗",
            Some("title must not be empty; provide a nonblank label for the subtask"),
        ),
        (
            ToolCallOutcome::Error,
            "status: error\nerror: the subtask supervisor is not running; launch_subtasks is unavailable",
            "✗",
            Some("the subtask supervisor is not running; launch_subtasks is unavailable"),
        ),
        (
            ToolCallOutcome::Denied,
            "status: denied\nreason: the tool `launch_subtasks` is unavailable in Plan mode",
            "⊘",
            Some("the tool `launch_subtasks` is unavailable in Plan mode"),
        ),
        (
            ToolCallOutcome::Cancelled,
            "status: cancelled\nreason: SECRET cancellation detail",
            "◼",
            None,
        ),
        (
            ToolCallOutcome::Cancelled,
            "status: error\nerror: SECRET cancellation detail",
            "◼",
            None,
        ),
    ] {
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::TurnStarted {
                turn_id: TEST_TURN_ID,
                message: Message::user("inspect synthetic fixtures"),
                mode: SessionMode::Build,
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id: TEST_TURN_ID,
                message: launch_assistant(
                    "call-rejected",
                    "inspect fixture boundaries",
                    "SECRET launch prompt",
                ),
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message(
                    "fc-call-rejected",
                    Some("call-rejected"),
                    "launch_subtasks",
                    output,
                ),
                metadata: vec![ToolResultMetadata {
                    diagnostic: diagnostic.map(str::to_string),
                    ..unlaunched_metadata("call-rejected", outcome)
                }],
            },
        );

        let rendered = rendered_text(&mut app, 120, 16);
        assert!(
            rendered.contains(&format!("subtask launch {status}")),
            "{rendered}"
        );
        if let Some(diagnostic) = diagnostic {
            assert!(
                rendered.contains(&format!("subtask launch {status} · {diagnostic}")),
                "{rendered}"
            );
        }
        for hidden in [
            "SECRET",
            "\"prompt\"",
            "\"type\"",
            "status: error",
            "status: denied",
            " · starting",
            " · running",
        ] {
            assert!(!rendered.contains(hidden), "leaked {hidden:?}: {rendered}");
        }
        if outcome == ToolCallOutcome::Cancelled {
            assert!(!rendered.contains(" ✗"));
        }
        assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(1, 0));
        assert_eq!(
            app.handle_event(key(KeyCode::Enter)),
            None,
            "a pre-launch error has no child pane"
        );
    }
}

#[test]
fn descriptorless_interrupted_launches_do_not_invent_a_failure_or_child() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("inspect a fixture"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: launch_assistant(
                "call-interrupted",
                "inspect fixture boundaries",
                "SECRET prompt",
            ),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    );
    let rendered = rendered_text(&mut app, 100, 16);
    assert!(rendered.contains("subtask launch ◼"));
    assert!(!rendered.contains(" ✗"));
    assert!(!rendered.contains("SECRET"));
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Interrupted);
    double_escape(&mut app);
    app.handle_event(key(KeyCode::Char('k'))); // Step back from the turn-cancelled notice.
    assert_eq!(app.selection(), cursor(1, 0));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(app.selection_scope(), Some(SelectionScope::Block));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
}

#[test]
fn restored_diagnostics_require_typed_evidence_and_never_parse_result_envelopes() {
    let title = "Summarize UI and CLI model changes";
    let reason = format!("title must be 3-5 words, got 6: {title:?}");
    for with_metadata in [true, false] {
        let result = tool_result_message(
            "fc-call-historical",
            Some("call-historical"),
            "launch_subtasks",
            format!("status: error\nerror: {reason}"),
        );
        // A minimal synthetic transcript, not a copy of the user's session.
        let items = [
            TranscriptItem::Message(Message::user("inspect synthetic model settings")),
            TranscriptItem::Message(launch_assistant(
                "call-historical",
                title,
                "SECRET synthetic prompt",
            )),
            if with_metadata {
                TranscriptItem::ToolResults {
                    skill_applications: Vec::new(),
                    message: result,
                    metadata: vec![
                        unlaunched_metadata("call-historical", ToolCallOutcome::Error)
                            .with_diagnostic(&reason),
                    ],
                }
            } else {
                TranscriptItem::Message(result)
            },
        ];
        let directory = tempfile::tempdir().expect("temporary transcript");
        let mut writer = zevria_transcript::transcript::TranscriptWriter::create(directory.path())
            .expect("writer");
        for item in &items {
            writer.append(item).expect("persist synthetic item");
        }
        let mut app = App::new();
        app.restore(
            zevria_transcript::transcript::load(writer.path())
                .expect("reload synthetic transcript"),
        );
        let rendered = rendered_text(&mut app, 120, 16);
        assert!(
            rendered.contains(if with_metadata {
                "subtask launch ✗"
            } else {
                "subtask launch ?"
            }),
            "{rendered}"
        );
        assert_eq!(rendered.contains(&reason), with_metadata, "{rendered}");
        assert!(!rendered.contains("SECRET"));
        assert!(!rendered.contains("\"prompt\""));
        assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
        double_escape(&mut app);
        assert_eq!(app.selection(), cursor(1, 0));
        assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    }
}

#[test]
fn launch_batch_notices_prefer_nonempty_metadata_counts_and_fall_back_to_arguments() {
    for string_arguments in [false, true] {
        for metadata_count in [None, Some(0), Some(1), Some(3)] {
            let arguments = two_child_launch_arguments();
            let arguments = if string_arguments {
                json!(arguments.to_string())
            } else {
                arguments
            };
            let message = assistant_message(vec![tool_call(
                "call-counts",
                None,
                "launch_subtasks",
                arguments,
            )]);
            let mut state = launch_state_with_result(
                ToolCallOutcome::Error,
                "status: error\nerror: SECRET batch result",
            );
            state
                .subtasks
                .insert(0, child_descriptor("child-0", "first child"));
            state.metadata = metadata_count.map(|count| ToolResultMetadata {
                detail: Some(ToolResultDetail::Subtasks(
                    (0..count)
                        .map(|index| zevria_foundation::SubtaskEntryMetadata {
                            index,
                            status: SubtaskStatus::Failed,
                            launch: (index == 0).then(|| child_launch("child-0", "first child")),
                        })
                        .collect(),
                )),
                ..unlaunched_metadata("call-counts", ToolCallOutcome::Error)
            });
            let mut lines = Vec::new();
            layout_native_message(&message, Some(&[Some(state)]), &mut lines, 100, None);
            let notice = match metadata_count {
                Some(1) => None,
                Some(3) => Some("2 of 3 subtasks not launched"),
                _ => Some("1 of 2 subtasks not launched"),
            };
            assert_eq!(lines.len(), 2 + usize::from(notice.is_some()));
            if let Some(notice) = notice {
                assert_eq!(line_text(&lines[1]), format!("◆ {notice}"));
                assert_eq!(
                    lines[1].spans.last().unwrap().style.fg,
                    Some(ZEVRIA_DARK.feedback.error)
                );
            }
            assert_eq!(
                line_text(lines.last().unwrap()),
                "◆ explore · first child ◐"
            );
        }
    }
}

#[test]
fn descriptorless_launch_notices_preserve_status_precedence_and_tone() {
    for (status, outcome, label, color) in [
        (
            ToolCallStatus::Executing,
            ToolCallOutcome::Denied,
            None,
            ZEVRIA_DARK.feedback.error,
        ),
        (
            ToolCallStatus::Interrupted,
            ToolCallOutcome::Denied,
            Some("◼"),
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            ToolCallStatus::Interrupted,
            ToolCallOutcome::Cancelled,
            Some("◼"),
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            ToolCallStatus::Finished,
            ToolCallOutcome::Cancelled,
            Some("◼"),
            ZEVRIA_DARK.feedback.warning,
        ),
        (
            ToolCallStatus::Finished,
            ToolCallOutcome::Success,
            Some("✓"),
            ZEVRIA_DARK.feedback.success,
        ),
    ] {
        let mut state = launch_state_with_result(outcome, "status: error\nerror: SECRET result");
        state.status = status;
        let mut lines = Vec::new();
        layout_native_message(
            &launch_assistant("call-diagnostic", "fallback title", "SECRET prompt"),
            Some(&[Some(state)]),
            &mut lines,
            100,
            None,
        );
        if let Some(label) = label {
            assert_eq!(lines.len(), 2);
            assert_eq!(line_text(&lines[1]), format!("◆ subtask launch {label}"));
            assert_eq!(lines[1].spans.last().unwrap().style.fg, Some(color));
        } else {
            assert!(
                lines.is_empty(),
                "an executing batch takes no space or role header"
            );
        }
    }
}

#[test]
fn launch_failure_diagnostics_are_single_line_sanitized_and_grapheme_bounded() {
    use unicode_segmentation::UnicodeSegmentation as _;

    let call = tool_call(
        "call-diagnostic",
        None,
        "launch_subtasks",
        json!({"tasks":[{
            "title": "inspect fixture boundaries", "prompt": "SECRET prompt", "type": "explore",
        }]}),
    );
    let unicode = "e\u{301}👩🏽‍💻界"; // Three graphemes, with combining marks and joiners.
    for (diagnostic, expected) in [
        (
            Some("queue\u{7}\u{202e}\n\tis\u{0}full\u{202c}".to_string()),
            "queue is full".to_string(),
        ),
        (
            Some(format!("\n\t{}\u{2028}more\nlines", unicode.repeat(100))),
            format!("{}…", unicode.repeat(80)),
        ),
        (Some(unicode.repeat(80)), unicode.repeat(80)),
        (Some("\u{0}\u{1b}\u{202e}\n\t".to_string()), String::new()),
        (None, String::new()),
    ] {
        let mut state = launch_state_with_result(
            ToolCallOutcome::Error,
            "status: error\nerror: SECRET raw payload",
        );
        state.metadata.as_mut().unwrap().diagnostic = diagnostic;
        let mut lines = Vec::new();
        layout_native_message(
            &assistant_message(vec![call.clone()]),
            Some(&[Some(state.clone())]),
            &mut lines,
            40,
            None,
        );
        assert_eq!(lines.len(), 2, "role header and one logical notice row");
        let row = line_text(&lines[1]);
        assert_eq!(
            row,
            if expected.is_empty() {
                "◆ subtask launch ✗".to_string()
            } else {
                format!("◆ subtask launch ✗ · {expected}")
            }
        );
        let diagnostic = row.strip_prefix("◆ subtask launch ✗ · ").unwrap_or("");
        assert!(diagnostic.graphemes(true).count() <= 241);
        assert!(!diagnostic.chars().any(char::is_control));
        assert!(!diagnostic.contains("SECRET"));
        assert_eq!(
            lines[1].spans.last().unwrap().style.fg,
            Some(ZEVRIA_DARK.feedback.error)
        );
    }

    let mut denied =
        launch_state_with_result(ToolCallOutcome::Denied, "status: denied\nreason: \t");
    for missing_result in [false, true] {
        if missing_result {
            denied.result = None;
        }
        let mut lines = Vec::new();
        layout_native_message(
            &assistant_message(vec![call.clone()]),
            Some(&[Some(denied.clone())]),
            &mut lines,
            100,
            None,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[1]), "◆ subtask launch ⊘");
        assert_eq!(
            lines[1].spans.last().unwrap().style.fg,
            Some(ZEVRIA_DARK.feedback.error)
        );
    }
}

#[test]
fn launch_successes_and_launched_children_never_expose_result_diagnostics() {
    let call = tool_call(
        "call-diagnostic",
        None,
        "launch_subtasks",
        json!({"tasks":[{
            "title": "inspect fixture boundaries", "prompt": "SECRET prompt", "type": "explore",
        }]}),
    );
    for (outcome, descriptor_status) in [
        (ToolCallOutcome::Success, None),
        (ToolCallOutcome::Success, Some(SubtaskStatus::Completed)),
        (ToolCallOutcome::Error, Some(SubtaskStatus::Failed)),
        (ToolCallOutcome::Cancelled, Some(SubtaskStatus::Cancelled)),
    ] {
        // An error-looking report must not trigger the descriptor-less exception.
        let mut state = launch_state_with_result(
            outcome,
            "status: error\nerror: SECRET child report or failure body",
        );
        state.subtasks = descriptor_status
            .map(|status| {
                (
                    0,
                    SubtaskDescriptor {
                        status,
                        ..child_descriptor("real-child", "inspect fixture boundaries")
                    },
                )
            })
            .into_iter()
            .collect();
        let mut lines = Vec::new();
        layout_native_message(
            &assistant_message(vec![call.clone()]),
            Some(&[Some(state.clone())]),
            &mut lines,
            100,
            None,
        );
        assert_eq!(lines.len(), 2);
        let row = line_text(&lines[1]);
        assert!(!row.contains("SECRET"));
        if let Some(status) = descriptor_status {
            assert!(row.ends_with(&format!(
                " {}",
                crate::presentation::subtask_icon(status).glyph(0)
            )));
        } else {
            assert_eq!(row, "◆ subtask launch ✓");
            assert_eq!(
                lines[1].spans.last().unwrap().style.fg,
                Some(ZEVRIA_DARK.feedback.success)
            );
        }
    }
}

#[test]
fn concurrent_launch_rows_update_independently_before_the_result_batch() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("investigate both paths"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![
                tool_call(
                    "fc-call-first",
                    Some("call-first"),
                    "launch_subtasks",
                    json!({"tasks":[{"title": "trace first path", "prompt": "SECRET first", "type": "explore"}]}),
                ),
                tool_call(
                    "fc-call-second",
                    Some("call-second"),
                    "launch_subtasks",
                    json!({"tasks":[{"title": "trace second path", "prompt": "SECRET second", "type": "explore"}]}),
                ),
            ]),
        },
    );
    for (call_id, child_id, title) in [
        ("call-first", "child-first", "trace first path"),
        ("call-second", "child-second", "trace second path"),
    ] {
        apply_turn_event(
            &mut app,
            SessionEvent::SubtaskLaunched {
                turn_id: TEST_TURN_ID,
                call_id: call_id.to_string(),
                entry_index: 0,
                descriptor: child_descriptor(child_id, title),
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::SubtaskStatus {
                turn_id: TEST_TURN_ID,
                id: SubtaskId::new(child_id),
                status: SubtaskStatus::Running,
            },
        );
    }

    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: SubtaskId::new("child-first"),
            status: SubtaskStatus::Completed,
        },
    );

    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("◆ explore · trace first path ✓"));
    assert!(rendered.contains("◆ explore · trace second path ◐"));
    assert!(!rendered.contains("SECRET"));
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Executing);
    assert_eq!(tool_status(&app, 1, 2), ToolCallStatus::Executing);
    assert!(app.has_executing_tool_calls());

    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: Message::User {
                content: vec![
                    UserContent::tool_result_with_call_id(
                        "fc-call-first",
                        "call-first",
                        "launch_subtasks",
                        vec![ToolResultContent::text("first report body")],
                    ),
                    UserContent::tool_result_with_call_id(
                        "fc-call-second",
                        "call-second",
                        "launch_subtasks",
                        vec![ToolResultContent::text("second report body")],
                    ),
                ],
            },
            metadata: vec![
                launch_metadata(
                    "call-first",
                    "child-first",
                    "trace first path",
                    ToolCallOutcome::Success,
                ),
                launch_metadata(
                    "call-second",
                    "child-second",
                    "trace second path",
                    ToolCallOutcome::Success,
                ),
            ],
        },
    );

    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Finished);
    assert_eq!(tool_status(&app, 1, 2), ToolCallStatus::Finished);
    assert!(!app.has_executing_tool_calls());
    assert_eq!(
        attached_subtask_status(&app, &SubtaskId::new("child-first")),
        Some(SubtaskStatus::Completed)
    );
    assert_eq!(
        attached_subtask_status(&app, &SubtaskId::new("child-second")),
        Some(SubtaskStatus::Completed)
    );
}

#[test]
fn launch_lifecycle_suffixes_use_semantic_styles() {
    let message = assistant_message(vec![
        tool_call(
            "complete",
            None,
            "launch_subtasks",
            json!({"tasks":[{"title": "completed child task"}]}),
        ),
        tool_call(
            "failed",
            None,
            "launch_subtasks",
            json!({"tasks":[{"title": "failed child task"}]}),
        ),
        tool_call(
            "◼",
            None,
            "launch_subtasks",
            json!({"tasks":[{"title": "cancelled child task"}]}),
        ),
    ]);
    let state = |id, title, status| {
        Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: [(
                0,
                SubtaskDescriptor {
                    status,
                    ..child_descriptor(id, title)
                },
            )]
            .into(),
        })
    };
    let states = vec![
        state(
            "completed-child",
            "completed child task",
            SubtaskStatus::Completed,
        ),
        state("failed-child", "failed child task", SubtaskStatus::Failed),
        state(
            "cancelled-child",
            "cancelled child task",
            SubtaskStatus::Cancelled,
        ),
    ];
    let mut lines = Vec::new();
    layout_native_message(&message, Some(&states), &mut lines, 120, None);

    for (title, status, status_color) in [
        ("completed child task", "✓", ZEVRIA_DARK.feedback.success),
        ("failed child task", "✗", ZEVRIA_DARK.feedback.error),
        ("cancelled child task", "◼", ZEVRIA_DARK.feedback.warning),
    ] {
        let expected = format!("◆ explore · {title} {status}");
        let row = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    == expected
            })
            .expect("launch lifecycle row");
        assert_eq!(
            row.spans.first().unwrap().style.fg,
            Some(ZEVRIA_DARK.roles.tools)
        );
        assert_eq!(row.spans.last().unwrap().style.fg, Some(status_color));
    }
}

#[test]
fn historical_subtask_sidecars_never_reach_frontend_restoration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("root.jsonl");
    for metadata in [false, true] {
        let mut value = serde_json::to_value(Message::user("delivery text")).unwrap();
        value["zevria_subtask_results"] = json!([]);
        if metadata {
            value["zevria_tool_result_metadata"] = json!([]);
        }
        let original = format!(
            "{}\n{}",
            serde_json::to_string(&Message::user("before")).unwrap(),
            value
        );
        std::fs::write(&path, &original).unwrap();
        assert!(zevria_transcript::transcript::load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn restored_launch_rows_attach_from_metadata_and_open_their_child() {
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(Message::user("go explore")),
        TranscriptItem::Message(launch_assistant(
            "call-launch",
            "map config loading",
            "prompt text quick",
        )),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: tool_result_message(
                "fc-call-launch",
                Some("call-launch"),
                "launch_subtasks",
                "status: launched\nid: child-1",
            ),
            metadata: vec![launch_metadata(
                "call-launch",
                "child-1",
                "map config loading",
                ToolCallOutcome::Success,
            )],
        },
    ]);

    assert_eq!(
        attached_subtask_status(&app, &SubtaskId::new("child-1")),
        Some(SubtaskStatus::Completed)
    );

    // Select the launch row and open its child.
    rendered_text(&mut app, 100, 20);
    double_escape(&mut app);
    assert_eq!(app.selection(), cursor(1, 1));
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(
        app.handle_event(key(KeyCode::Enter)),
        Some(UiAction::OpenSubtask {
            id: SubtaskId::new("child-1")
        })
    );
}

#[test]
fn launch_result_metadata_reconciles_restore_outcomes_and_preserves_terminal_events() {
    for (case, outcome, expected_status) in [
        (
            "success",
            ToolCallOutcome::Success,
            SubtaskStatus::Completed,
        ),
        ("error", ToolCallOutcome::Error, SubtaskStatus::Failed),
        (
            "cancelled",
            ToolCallOutcome::Cancelled,
            SubtaskStatus::Cancelled,
        ),
    ] {
        let call_id = format!("call-{case}");
        let child_id = format!("child-{case}");
        let title = format!("restore {case} child");
        let mut app = App::new();
        app.restore(vec![
            TranscriptItem::Message(Message::user("restore launch")),
            TranscriptItem::Message(launch_assistant(&call_id, &title, "private prompt")),
            TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: tool_result_message(
                    &format!("fc-{call_id}"),
                    Some(&call_id),
                    "launch_subtasks",
                    "private result body",
                ),
                metadata: vec![launch_metadata(&call_id, &child_id, &title, outcome)],
            },
        ]);

        assert_eq!(
            attached_subtask_status(&app, &SubtaskId::new(&child_id)),
            Some(expected_status),
            "restore outcome {case}"
        );
        let rendered = rendered_text(&mut app, 100, 16);
        assert!(rendered.contains(&format!(
            "◆ explore · {title} {}",
            crate::presentation::subtask_icon(expected_status).glyph(0)
        )));
        assert!(!rendered.contains("private prompt"));
        assert!(!rendered.contains("private result body"));
    }

    // Without launch metadata/result correlation there is no descriptor to
    // synthesize; restore truthfully leaves the generic call interrupted.
    let mut interrupted = App::new();
    interrupted.restore(vec![
        TranscriptItem::Message(Message::user("restore unfinished launch")),
        TranscriptItem::Message(launch_assistant(
            "call-unfinished",
            "unfinished child task",
            "private unfinished prompt",
        )),
    ]);
    assert_eq!(
        attached_subtask_status(&interrupted, &SubtaskId::new("child-unfinished")),
        None
    );
    let rendered = rendered_text(&mut interrupted, 100, 16);
    assert!(rendered.contains("subtask launch ◼"));
    assert!(!rendered.contains("private unfinished prompt"));

    // A lifecycle terminal state is authoritative over generic result
    // inference, even if the result metadata points to success.
    let mut live = App::new();
    apply_turn_event(
        &mut live,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("launch live child"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: launch_assistant("call-live", "live terminal child", "private prompt"),
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "call-live".to_string(),
            entry_index: 0,
            descriptor: child_descriptor("child-live", "live terminal child"),
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: SubtaskId::new("child-live"),
            status: SubtaskStatus::Failed,
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "fc-call-live",
                Some("call-live"),
                "launch_subtasks",
                "generic success result",
            ),
            metadata: vec![launch_metadata(
                "call-live",
                "child-live",
                "live terminal child",
                ToolCallOutcome::Success,
            )],
        },
    );
    assert_eq!(
        attached_subtask_status(&live, &SubtaskId::new("child-live")),
        Some(SubtaskStatus::Failed)
    );
}

#[test]
fn late_terminal_status_updates_an_interrupted_row_after_turn_teardown() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("launch then cancel"),
            mode: SessionMode::Build,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: launch_assistant("call-late", "late cancelled child", "private prompt"),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "call-late".to_string(),
            entry_index: 0,
            descriptor: child_descriptor("child-late", "late cancelled child"),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: SubtaskId::new("child-late"),
            status: SubtaskStatus::Running,
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    );
    assert_eq!(app.active_turn_id(), None);
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Interrupted);
    let rendered = rendered_text(&mut app, 100, 18);
    assert!(!rendered.contains("subtasks ·"));
    assert!(!rendered.contains("subtask launch"));
    assert!(rendered.contains("◆ explore · late cancelled child "));

    // This event deliberately arrives after active-turn state was cleared.
    app.reduce_without_effects(SessionEvent::SubtaskStatus {
        turn_id: TEST_TURN_ID,
        id: SubtaskId::new("child-late"),
        status: SubtaskStatus::Cancelled,
    });
    assert_eq!(app.active_turn_id(), None);
    assert_eq!(tool_status(&app, 1, 0), ToolCallStatus::Interrupted);
    assert_eq!(
        attached_subtask_status(&app, &SubtaskId::new("child-late")),
        Some(SubtaskStatus::Cancelled)
    );
    let rendered = rendered_text(&mut app, 100, 18);
    assert!(rendered.contains("◆ explore · late cancelled child ◼"));
    assert!(!rendered.contains("late cancelled child · interrupted"));
}

#[test]
fn explore_panes_inherit_profiles_and_override_nominal_build_turns() {
    let child_id = SubtaskId::new("profiled-child");
    let mut views = test_session_views(configured_app());
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "profiled-launch".to_string(),
        entry_index: 0,
        descriptor: child_descriptor("profiled-child", "inspect routing"),
    });
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child prompt"),
            mode: SessionMode::Build,
        }),
    });
    assert_eq!(
        views.child(&child_id).expect("child").in_flight_role(),
        Some(ModelRole::Explore)
    );
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::CompactionStarted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::AutomaticMidTurn,
        }),
    });
    assert_eq!(
        views.child(&child_id).expect("child").in_flight_role(),
        Some(ModelRole::Explore)
    );
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::ContextUsageUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: ContextTokenSnapshot {
                profile: ModelProfileRef::new("runtime-explore", "explore-v2"),
                model_role: ModelRole::Explore,
                projected_input_tokens: 7_500,
                source: ContextTokenSource::Exact,
                automatic_trigger: 40_000,
                input_token_limit: 50_000,
                context_window_tokens: 64_000,
            },
        }),
    });
    views.handle_event(ctrl('i'));
    let rendered = rendered_views_text(&mut views, 360, 12);
    assert!(rendered.contains("explore · inspect routing · starting"));
    assert!(rendered.contains("runtime-explore/explore-v2"));
    assert!(rendered.contains("next 7.5k/50.0k exact"));
    assert!(!rendered.contains("provider-build/build-model"));

    let restored_id = SubtaskId::new("restored-profiled-child");
    let mut restored = test_session_views(configured_app());
    restored.restore_child(
        restored_id,
        Some(child_launch("restored-profiled-child", "restored routing")),
        vec![TranscriptItem::Message(Message::assistant("historical"))],
    );
    restored.handle_event(ctrl('i'));
    let rendered = rendered_views_text(&mut restored, 180, 10);
    assert!(rendered.contains("explore · restored routing · historical"));
    assert!(rendered.contains("provider-explore/explore-model"));
}

#[test]
fn session_views_route_child_events_navigate_and_never_cancel_or_submit() {
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("kick off the explore"),
        mode: SessionMode::Build,
    });
    views.apply(SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
        message: launch_assistant("call-launch", "map config loading", "prompt quick"),
    });
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "call-launch".to_string(),
        entry_index: 0,
        descriptor: child_descriptor("child-1", "map config loading"),
    });
    // Live child traffic reduces into the hidden pane without touching the
    // root pane's busy/streaming state.
    let child_id = SubtaskId::new("child-1");
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child prompt"),
            mode: SessionMode::Build,
        }),
    });
    let child_findings = (0..20)
        .map(|index| format!("child finding {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    views.apply(SessionEvent::SubtaskSession {
        id: child_id.clone(),
        event: Box::new(SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant(child_findings),
        }),
    });
    views.apply(SessionEvent::SubtaskStatus {
        turn_id: TEST_TURN_ID,
        id: child_id.clone(),
        status: SubtaskStatus::Running,
    });
    assert_eq!(views.visible_child_id(), None);
    assert!(views.root().is_busy(), "the parent turn is still running");
    let child = views.child(&child_id).expect("hidden child pane");
    assert_eq!(
        child.history().len(),
        2,
        "hidden pane received live updates"
    );
    assert!(child.inspect_only());
    assert!(
        child
            .subsession_title()
            .is_some_and(|title| title.contains("explore · map config loading · running")),
        "the hidden child title receives the lifecycle transition"
    );
    assert_eq!(
        attached_subtask_status(views.root(), &child_id),
        Some(SubtaskStatus::Running),
        "the same event updates the root launch row"
    );
    assert_eq!(tool_status(views.root(), 1, 0), ToolCallStatus::Executing);

    // Enter narrows to the launch block; a second Enter opens the live child, even while busy.
    rendered_views_text(&mut views, 100, 20);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.visible_child_id(), Some(&child_id));

    // Wheel-generated Up/Down events follow the visible child pane, leaving
    // the root pane's transcript position and follow state untouched. A Down
    // that returns the child to its rendered bottom restores child follow.
    let _ = rendered_views_text(&mut views, 40, 8);
    let child_bottom = views.child(&child_id).expect("child pane").view_scroll();
    assert!(child_bottom > 0, "child fixture must exceed the viewport");
    let root_scroll = views.root().view_scroll();
    let root_follow = views.root().view_follow();

    assert_eq!(views.handle_event(key(KeyCode::Up)), None);
    assert_eq!(
        views.child(&child_id).expect("child pane").view_scroll(),
        child_bottom - 1
    );
    assert!(!views.child(&child_id).expect("child pane").view_follow());
    let _ = rendered_views_text(&mut views, 40, 8);
    assert!(!views.child(&child_id).expect("child pane").view_follow());
    assert_eq!(views.root().view_scroll(), root_scroll);
    assert_eq!(views.root().view_follow(), root_follow);

    assert_eq!(views.handle_event(key(KeyCode::Down)), None);
    assert!(!views.child(&child_id).expect("child pane").view_follow());
    let _ = rendered_views_text(&mut views, 40, 8);
    assert_eq!(
        views.child(&child_id).expect("child pane").view_scroll(),
        child_bottom
    );
    assert!(views.child(&child_id).expect("child pane").view_follow());
    assert_eq!(views.root().view_scroll(), root_scroll);
    assert_eq!(views.root().view_follow(), root_follow);

    // Ctrl-O returns to the parent without cancelling anything.
    assert_eq!(views.handle_event(ctrl('o')), None);
    assert_eq!(views.visible_child_id(), None);
    assert_eq!(
        views.child(&child_id).expect("child pane").history().len(),
        2,
        "navigation never cancels the child"
    );

    // Ctrl-I reopens the latest entered child; legacy terminals send Tab.
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_child_id(), Some(&child_id));
    assert_eq!(views.handle_event(ctrl('o')), None);
    assert_eq!(views.handle_event(key(KeyCode::Tab)), None);
    assert_eq!(views.visible_child_id(), Some(&child_id));

    // Child panes are inspect-only: keys can never submit from them.
    assert_eq!(views.handle_event(key(KeyCode::Char('i'))), None);
    for ch in "never submits".chars() {
        assert_eq!(views.handle_event(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
}

#[test]
fn status_before_launch_is_reconciled_into_running_and_terminal_root_rows() {
    for (case, status) in [
        ("running", SubtaskStatus::Running),
        ("terminal", SubtaskStatus::Failed),
    ] {
        let call_id = format!("call-{case}");
        let child_id = SubtaskId::new(format!("child-{case}"));
        let title = format!("race {case} child");
        let mut views = test_session_views(App::new());
        views.apply(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("launch racing child"),
            mode: SessionMode::Build,
        });
        views.apply(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: launch_assistant(&call_id, &title, "private race prompt"),
        });

        views.apply(SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: child_id.clone(),
            status,
        });
        assert_eq!(
            attached_subtask_status(views.root(), &child_id),
            None,
            "the root cannot correlate status before descriptor attachment"
        );

        views.apply(SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id,
            entry_index: 0,
            descriptor: child_descriptor(child_id.as_str(), &title),
        });
        assert_eq!(
            attached_subtask_status(views.root(), &child_id),
            Some(status),
            "launch attachment must not regress cached {case} status"
        );
        assert!(
            views
                .child(&child_id)
                .and_then(App::subsession_title)
                .is_some_and(|child_title| {
                    child_title.contains(&format!("explore · {title} · {status}"))
                })
        );
        assert_eq!(tool_status(views.root(), 1, 0), ToolCallStatus::Executing);
    }
}

#[test]
fn restored_child_panes_are_historical_inspectable_and_reachable() {
    let mut views = test_session_views(App::new());
    views.restore_child(
        SubtaskId::new("child-9"),
        Some(child_launch("child-9", "old explore task")),
        vec![
            TranscriptItem::Message(Message::user("child task")),
            TranscriptItem::Message(Message::assistant("partial progress")),
        ],
    );

    let child_id = SubtaskId::new("child-9");
    let child = views.child(&child_id).expect("restored pane");
    assert_eq!(child.history().len(), 2);
    assert!(child.inspect_only());
    let title = child.subsession_title().expect("subsession title set");
    assert!(title.contains("explore · old explore task"));
    assert!(title.contains("historical"));

    // With no previously entered child, Ctrl-I falls back to the latest one.
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_child_id(), Some(&child_id));
}

#[test]
fn prompt_to_ensemble_acceptance_creates_fresh_panes_and_prunes_discarded_runs() {
    let old = editable_ensemble_start(
        "prompt-tail-old-run",
        EnsembleWorkflow::Review,
        "discarded review",
    );
    let old_agent_id = old.agents[0].id.clone();
    let fresh = editable_ensemble_start(
        "prompt-tail-fresh-run",
        EnsembleWorkflow::Plan,
        "fresh plan",
    );
    let fresh_agent_id = fresh.agents[0].id.clone();
    let mut root = App::new();
    root.seed_history_entry(history_message(Message::user("ordinary prompt")));
    let mut views = test_session_views(root);
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: old.clone(),
        resumed: false,
    });
    views.apply(SessionEvent::TurnRecovered {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
    });

    // Select the old ensemble, move to the preceding ordinary prompt,
    // then revise that prompt into a fresh ensemble.
    rendered_views_text(&mut views, 100, 20);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Char('k'))), None);
    assert_eq!(views.handle_event(ctrl('e')), None);
    assert!(matches!(
        views.root().recalled_edit_target(),
        Some(TranscriptEditTarget::PromptOrdinal(0))
    ));
    assert_eq!(views.handle_event(ctrl('c')), None);
    for character in "/ensemble-plan fresh plan".chars() {
        assert_eq!(views.handle_event(key(KeyCode::Char(character))), None);
    }
    assert!(matches!(
        views.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            target: TranscriptEditTarget::PromptOrdinal(0),
            replacement: TranscriptEditReplacement::Ensemble {
                workflow: EnsembleWorkflow::Plan,
                ..
            },
        }))
    ));

    // Pending acceptance leaves the discarded pane navigable.
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&old_agent_id));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(2),
        start: fresh,
        resumed: false,
    });

    assert_eq!(views.visible_agent_id(), None);
    assert!(views.agent(&old_agent_id).is_none());
    assert!(views.agent(&fresh_agent_id).is_some());
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&fresh_agent_id));
}

#[test]
fn accepted_ensemble_edits_prune_removed_worker_panes_without_retargeting_navigation() {
    let old = editable_ensemble_start("pane-old-run", EnsembleWorkflow::Review, "review old code");
    let old_agent_id = old.agents[0].id.clone();
    let fresh = editable_ensemble_start(
        "pane-fresh-run",
        EnsembleWorkflow::Review,
        "review fresh code",
    );
    let fresh_agent_id = fresh.agents[0].id.clone();
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: old.clone(),
        resumed: false,
    });
    views.apply(SessionEvent::TurnRecovered {
        display_attempt_id: None,
        turn_id: TEST_TURN_ID,
    });

    // Select and enter the old worker, then return to its command row and
    // submit an unchanged exact command as a typed ensemble replacement.
    rendered_views_text(&mut views, 100, 20);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.handle_event(key(KeyCode::Enter)), None);
    assert_eq!(views.visible_agent_id(), Some(&old_agent_id));
    assert_eq!(views.handle_event(ctrl('o')), None);
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);
    assert_eq!(
        views.root().selection_scope(),
        Some(SelectionScope::Message)
    );
    assert_eq!(views.handle_event(ctrl('e')), None);
    assert!(matches!(
        views.handle_event(ctrl_enter()),
        Some(UiAction::EditTranscript(TranscriptEdit {
            target: TranscriptEditTarget::EnsembleRun(run_id),
            replacement: TranscriptEditReplacement::Ensemble {
                workflow: EnsembleWorkflow::Review,
                ..
            },
        })) if run_id == old.run_id
    ));

    // The pane remains reachable while backend acceptance is pending.
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&old_agent_id));
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TurnId::new(2),
        start: fresh,
        resumed: false,
    });

    assert_eq!(
        views.visible_agent_id(),
        None,
        "a removed visible index returns to root instead of retargeting"
    );
    assert!(views.agent(&old_agent_id).is_none());
    assert!(views.agent(&fresh_agent_id).is_some());
    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&fresh_agent_id));
}

#[test]
fn ctrl_i_live_fallback_uses_shared_agent_then_subtask_recency() {
    let mut views = test_session_views(App::new());
    let run_id = EnsembleRunId::from_string("live-agent-first");
    let agent = navigation_agent("live-agent");
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: navigation_ensemble_start(&run_id, &agent),
        resumed: false,
    });
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "live-subtask-call".to_string(),
        entry_index: 0,
        descriptor: child_descriptor("live-subtask", "newer subtask"),
    });

    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(
        views.visible_child_id(),
        Some(&SubtaskId::new("live-subtask"))
    );
}

#[test]
fn ctrl_i_live_fallback_uses_shared_subtask_then_agent_recency() {
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "live-subtask-call".to_string(),
        entry_index: 0,
        descriptor: child_descriptor("live-subtask", "older subtask"),
    });
    let run_id = EnsembleRunId::from_string("live-agent-last");
    let agent = navigation_agent("live-agent");
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: navigation_ensemble_start(&run_id, &agent),
        resumed: false,
    });

    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&agent.id));
}

#[test]
fn ctrl_i_restored_fallback_uses_root_agent_then_subtask_order() {
    let run_id = EnsembleRunId::from_string("restored-agent-first");
    let agent = navigation_agent("restored-agent");
    let start = navigation_ensemble_start(&run_id, &agent);
    let root_items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        historical_subtask_launch("restored-subtask"),
    ];
    let mut views = test_session_views(App::new());
    views.seed_child_creation_order(&root_items);
    // Restore in the opposite order to prove filesystem iteration cannot
    // redefine the root transcript's recency.
    views.restore_child(
        SubtaskId::new("restored-subtask"),
        Some(child_launch("restored-subtask", "newer subtask")),
        Vec::new(),
    );
    views.restore_agent_run(historical_agent_records(&start, &agent));

    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(
        views.visible_child_id(),
        Some(&SubtaskId::new("restored-subtask"))
    );
}

#[test]
fn ctrl_i_restored_fallback_uses_root_subtask_then_agent_order() {
    let run_id = EnsembleRunId::from_string("restored-agent-last");
    let agent = navigation_agent("restored-agent");
    let start = navigation_ensemble_start(&run_id, &agent);
    let root_items = vec![
        historical_subtask_launch("restored-subtask"),
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
    ];
    let mut views = test_session_views(App::new());
    views.seed_child_creation_order(&root_items);
    views.restore_agent_run(historical_agent_records(&start, &agent));
    views.restore_child(
        SubtaskId::new("restored-subtask"),
        Some(child_launch("restored-subtask", "older subtask")),
        Vec::new(),
    );

    assert_eq!(views.handle_event(ctrl('i')), None);
    assert_eq!(views.visible_agent_id(), Some(&agent.id));
}

#[test]
fn skill_tool_calls_render_as_compact_rows_with_failures_surfaced() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "skill-1",
                None,
                "skill",
                serde_json::json!({"skill": "commit"}),
            )]),
        },
    );
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("skill · commit ◐"));
    assert!(
        rendered.matches("◆").count() == 1,
        "skill calls have exactly one logical header prefix"
    );

    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "skill-1",
                None,
                "skill",
                "status: activated\nskill: commit\napplication: the current request",
            ),
            metadata: vec![file_metadata(
                "skill-1",
                None,
                "skill",
                ToolCallOutcome::Success,
                Vec::new(),
            )],
        },
    );
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("skill · commit ✓"));
    assert!(
        !rendered.contains("</skill>"),
        "engine-owned instructions are not conversation prose"
    );

    // An unknown-name failure is labeled and its short error stays visible.
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "skill-2",
                None,
                "skill",
                serde_json::json!({"skill": "missing"}),
            )]),
        },
    );
    apply_turn_event(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message(
                "skill-2",
                None,
                "skill",
                "status: error\nerror: unknown skill",
            ),
            metadata: vec![
                file_metadata("skill-2", None, "skill", ToolCallOutcome::Error, Vec::new())
                    .with_diagnostic("unknown skill"),
            ],
        },
    );
    let rendered = rendered_text(&mut app, 100, 24);
    assert!(rendered.contains("skill · missing ✗"));
    assert!(rendered.contains("unknown skill"));
}

#[test]
fn reconcile_reports_renders_structured_rows_for_both_argument_encodings() {
    let reconciliation = test_reconciliation();
    let object = serde_json::to_value(&reconciliation).unwrap();
    let encoded = serde_json::Value::String(serde_json::to_string(&reconciliation).unwrap());
    let applied_id = reconciliation.decisions[0].decision_id.to_string();
    let inapplicable_id = reconciliation.decisions[1].decision_id.to_string();
    let root_question_id = reconciliation.unavailable_decisions[0]
        .unavailable_decision_id
        .to_string();
    let unavailable_id = reconciliation.unavailable_decisions[1]
        .unavailable_decision_id
        .to_string();

    for arguments in [object, encoded] {
        let lines = render_reconciliation_lines(
            arguments,
            crate::app::ToolCallState {
                arguments: None,
                status: ToolCallStatus::Executing,
                result: None,
                metadata: None,
                subtasks: Default::default(),
            },
        );
        assert_eq!(
            lines.len(),
            10,
            "one role header plus eight structured rows"
        );
        assert_eq!(
            line_text(&lines[1]),
            "◆ reconcile_reports · 4 disagreements · 4 decisions ◐"
        );
        assert_eq!(
            line_text(&lines[2]),
            "  ⚖ facts · factual · repository evidence"
        );
        assert_eq!(
            line_text(&lines[3]),
            "  ⚖ requirement · preference tradeoff · user requirement"
        );
        assert_eq!(
            line_text(&lines[4]),
            "  ⚖ recorded · factual · recorded decisions"
        );
        assert_eq!(
            line_text(&lines[5]),
            "  ⚖ question · preference tradeoff · root question"
        );
        assert_eq!(
            line_text(&lines[6]),
            format!("  ✓ {applied_id} · recorded · applied")
        );
        assert_eq!(
            line_text(&lines[7]),
            format!("  – {inapplicable_id} · recorded · objectively inapplicable")
        );
        assert_eq!(
            line_text(&lines[8]),
            format!("  ○ {root_question_id} · unavailable · root question required")
        );
        assert_eq!(
            line_text(&lines[9]),
            format!("  – {unavailable_id} · unavailable · objectively inapplicable")
        );

        let rendered = reconciliation_text(&lines);
        for sentinel in [
            "SUMMARY_SENTINEL",
            "FIRST_POSITION_SENTINEL",
            "SECOND_POSITION_SENTINEL",
            "EVIDENCE_SENTINEL",
            "REQUIREMENT_SENTINEL",
            "APPLICATION_SENTINEL",
            "EXPLANATION_SENTINEL",
            "REASON_SENTINEL",
            "UNAVAILABLE_REASON_SENTINEL",
            "UNAVAILABLE_EVIDENCE_SENTINEL",
        ] {
            assert!(!rendered.contains(sentinel), "leaked {sentinel}");
        }
        assert!(!rendered.contains("reconcile_reports("));
        assert_eq!(rendered.matches("◆").count(), 1);
        assert!(!rendered.contains("\"summary\""));
    }
}

#[test]
fn reconcile_reports_lifecycle_uses_semantic_labels_and_colors() {
    let arguments = serde_json::to_value(test_reconciliation()).unwrap();
    let tool_name = RECONCILE_REPORTS_TOOL_NAME;

    assert_reconciliation_status(
        &arguments,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
        "◐",
        ZEVRIA_DARK.feedback.info,
    );

    let success_output =
        "Reconciliation declaration accepted. The next valid step is `submit_plan`.";
    let accepted_lines = render_reconciliation_lines(
        arguments.clone(),
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result(
                "reconcile-accepted",
                tool_name,
                success_output,
            )),
            metadata: Some(file_metadata(
                "reconcile-accepted",
                None,
                tool_name,
                ToolCallOutcome::Success,
                Vec::new(),
            )),
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&accepted_lines[1]).split_whitespace().last(),
        Some("✓")
    );
    assert_eq!(
        accepted_lines[1].spans.last().unwrap().style.fg,
        Some(ZEVRIA_DARK.feedback.success)
    );
    assert!(!reconciliation_text(&accepted_lines).contains(success_output));

    let failed_output = "status: error\nerror: malformed reconciliation\nline two";
    let failed_lines = render_reconciliation_lines(
        arguments.clone(),
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result(
                "reconcile-failed",
                tool_name,
                failed_output,
            )),
            metadata: Some(
                file_metadata(
                    "reconcile-failed",
                    None,
                    tool_name,
                    ToolCallOutcome::Error,
                    Vec::new(),
                )
                .with_diagnostic("malformed reconciliation\nline two"),
            ),
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&failed_lines[1]).split_whitespace().last(),
        Some("✗")
    );
    assert_eq!(
        failed_lines[1].spans.last().unwrap().style.fg,
        Some(ZEVRIA_DARK.feedback.error)
    );
    assert_eq!(
        failed_lines[10..].iter().map(line_text).collect::<Vec<_>>(),
        vec![
            "malformed reconciliation".to_string(),
            "line two".to_string(),
        ]
    );
    assert!(
        failed_lines[10..]
            .iter()
            .all(|line| line.style.fg == Some(ZEVRIA_DARK.feedback.error))
    );

    assert_reconciliation_status(
        &arguments,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result(
                "reconcile-denied",
                tool_name,
                "status: error\nerror: denied by workflow",
            )),
            metadata: Some(file_metadata(
                "reconcile-denied",
                None,
                tool_name,
                ToolCallOutcome::Denied,
                Vec::new(),
            )),
            subtasks: Default::default(),
        },
        "⊘",
        ZEVRIA_DARK.feedback.error,
    );

    assert_reconciliation_status(
        &arguments,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Interrupted,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
        "◼",
        ZEVRIA_DARK.feedback.warning,
    );

    assert_reconciliation_status(
        &arguments,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Finished,
            result: Some(test_tool_result(
                "reconcile-unmatched",
                tool_name,
                "unverified terminal output",
            )),
            metadata: None,
            subtasks: Default::default(),
        },
        "?",
        ZEVRIA_DARK.text.muted,
    );
}

#[test]
fn reconcile_reports_count_labels_and_invalid_arguments_are_compact() {
    let empty = json!({
        "disagreements": [],
        "decisions": [],
        "unavailable_decisions": [],
    });
    let empty_lines = render_reconciliation_lines(
        empty,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&empty_lines[1]),
        "◆ reconcile_reports · 0 disagreements · 0 decisions ◐"
    );
    assert_eq!(empty_lines.len(), 2);

    let singular = json!({
        "disagreements": [{
            "id": "one",
            "summary": "summary",
            "positions": [
                {"label": "a", "position": "a"},
                {"label": "b", "position": "b"}
            ],
            "classification": "factual",
            "resolution": {
                "type": "repository_evidence",
                "kind": "factual_claim",
                "evidence": "evidence"
            }
        }],
        "decisions": [{
            "decision_id": "decision_one",
            "disposition": {"status": "applied", "explanation": "explanation"}
        }],
        "unavailable_decisions": []
    });
    let singular_lines = render_reconciliation_lines(
        singular,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&singular_lines[1]),
        "◆ reconcile_reports · 1 disagreement · 1 decision ◐"
    );

    let malformed_raw = "{\"disagreements\":[MALFORMED_SENTINEL";
    let malformed_lines = render_reconciliation_lines(
        serde_json::Value::String(malformed_raw.to_string()),
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&malformed_lines[1]),
        "◆ reconcile_reports · <invalid arguments> ◐"
    );
    assert_eq!(malformed_lines.len(), 2);
    assert!(!reconciliation_text(&malformed_lines).contains(malformed_raw));
    assert!(!reconciliation_text(&malformed_lines).contains("MALFORMED_SENTINEL"));
}

#[test]
fn reconcile_reports_bounds_untrusted_identifiers_without_rendering_controls() {
    let arguments = json!({
        "disagreements": [{
            "id": format!("{}\n", "x".repeat(98)),
            "summary": "summary",
            "positions": [
                {"label": "a", "position": "a"},
                {"label": "b", "position": "b"}
            ],
            "classification": "factual",
            "resolution": {
                "type": "repository_evidence",
                "kind": "factual_claim",
                "evidence": "evidence"
            }
        }],
        "decisions": [],
        "unavailable_decisions": []
    });
    let lines = render_reconciliation_lines(
        arguments,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
    );
    let row = line_text(&lines[2]);
    assert!(row.contains('…'));
    assert!(!row.chars().any(char::is_control));
    assert!(!row.contains('\n'));

    let invalid_id = json!({
        "disagreements": [{
            "id": "\u{0000}\n",
            "summary": "summary",
            "positions": [
                {"label": "a", "position": "a"},
                {"label": "b", "position": "b"}
            ],
            "classification": "factual",
            "resolution": {
                "type": "repository_evidence",
                "kind": "factual_claim",
                "evidence": "evidence"
            }
        }],
        "decisions": [],
        "unavailable_decisions": []
    });
    let invalid_lines = render_reconciliation_lines(
        invalid_id,
        crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        },
    );
    assert_eq!(
        line_text(&invalid_lines[2]),
        "  ⚖ <invalid id> · factual · repository evidence"
    );
}

#[test]
fn reconcile_reports_copy_and_restore_keep_full_artifact_access() {
    let reconciliation = test_reconciliation();
    let object = serde_json::to_value(&reconciliation).unwrap();
    let encoded = serde_json::Value::String(serde_json::to_string(&reconciliation).unwrap());
    let output = "Reconciliation declaration accepted. The next valid step is `submit_plan`.";

    for arguments in [object, encoded] {
        let expected_primary = match &arguments {
            serde_json::Value::String(raw) => raw.clone(),
            arguments => arguments.to_string(),
        };
        let id = "reconcile-copy-live";
        let mut live = App::new();
        apply_turn_event(
            &mut live,
            SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id: TEST_TURN_ID,
                message: assistant_message(vec![tool_call(
                    id,
                    None,
                    RECONCILE_REPORTS_TOOL_NAME,
                    arguments.clone(),
                )]),
            },
        );
        apply_turn_event(
            &mut live,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message(id, None, RECONCILE_REPORTS_TOOL_NAME, output),
                metadata: vec![file_metadata(
                    id,
                    None,
                    RECONCILE_REPORTS_TOOL_NAME,
                    ToolCallOutcome::Success,
                    Vec::new(),
                )],
            },
        );
        live.reduce_without_effects(SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::Assistant {
                id: None,
                content: Vec::new(),
            },
        });
        live.set_view_for_test(0, false);
        let live_rendered = rendered_text(&mut live, 160, 80);
        assert!(live_rendered.contains("reconcile_reports · 4 disagreements · 4 decisions ✓"));
        assert!(!live_rendered.contains(output));
        live.select_for_test(cursor(0, 0));
        assert_eq!(
            live.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: expected_primary.clone(),
            })
        );
        assert_eq!(
            live.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: output.to_string(),
            })
        );

        let restored_id = "reconcile-copy-restored";
        let assistant = assistant_message(vec![tool_call(
            restored_id,
            None,
            RECONCILE_REPORTS_TOOL_NAME,
            arguments.clone(),
        )]);
        let mut restored = App::new();
        restored.restore(vec![
            TranscriptItem::Message(assistant),
            TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: tool_result_message(
                    restored_id,
                    None,
                    RECONCILE_REPORTS_TOOL_NAME,
                    output,
                ),
                metadata: vec![file_metadata(
                    restored_id,
                    None,
                    RECONCILE_REPORTS_TOOL_NAME,
                    ToolCallOutcome::Success,
                    Vec::new(),
                )],
            },
        ]);
        restored.set_view_for_test(0, false);
        let restored_rendered = rendered_text(&mut restored, 160, 80);
        assert_eq!(restored_rendered, live_rendered);
        assert!(!restored_rendered.contains(output));
        restored.select_for_test(cursor(0, 0));
        assert_eq!(
            restored.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: expected_primary,
            })
        );
        assert_eq!(
            restored.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: output.to_string(),
            })
        );
    }
}

#[test]
fn submit_plan_tool_calls_render_compact_rows_for_both_argument_encodings() {
    let artifact = test_plan_artifact();
    let object = json!({
        "title": artifact.title,
        "markdown": artifact.markdown,
    });
    let cases = [
        ("submit-plan-object", object.clone()),
        (
            "submit-plan-encoded",
            serde_json::Value::String(object.to_string()),
        ),
    ];

    for (id, arguments) in cases {
        let message =
            assistant_message(vec![tool_call(id, None, SUBMIT_PLAN_TOOL_NAME, arguments)]);
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 160, None);

        assert_eq!(lines.len(), 2);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("submit_plan · Durable approval workflow ◐"));
        assert!(!rendered.contains("submit_plan("));
        assert_eq!(rendered.matches("◆").count(), 1);
        assert!(!rendered.contains("## Implementation"));
        assert!(!rendered.contains("Build it."));
    }
}

#[test]
fn submit_plan_tool_call_lifecycle_uses_semantic_labels_and_colors() {
    let artifact = test_plan_artifact();
    let cases = [
        (
            ToolCallStatus::Executing,
            None,
            "◐",
            ZEVRIA_DARK.feedback.info,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Success),
            "✓",
            ZEVRIA_DARK.feedback.success,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Error),
            "✗",
            ZEVRIA_DARK.feedback.error,
        ),
        (
            ToolCallStatus::Finished,
            Some(ToolCallOutcome::Denied),
            "⊘",
            ZEVRIA_DARK.feedback.error,
        ),
        (
            ToolCallStatus::Interrupted,
            None,
            "◼",
            ZEVRIA_DARK.feedback.warning,
        ),
    ];

    for (index, (status, outcome, expected_status, expected_color)) in cases.into_iter().enumerate()
    {
        let id = format!("submit-plan-{index}");
        let message = assistant_message(vec![tool_call(
            &id,
            None,
            SUBMIT_PLAN_TOOL_NAME,
            json!({
                "title": artifact.title,
                "markdown": artifact.markdown,
            }),
        )]);
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status,
            result: None,
            metadata: outcome.map(|outcome| {
                file_metadata(&id, None, SUBMIT_PLAN_TOOL_NAME, outcome, Vec::new())
            }),
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 160, None);

        assert_eq!(lines.len(), 2);
        let row = &lines[1];
        let row_text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            row_text,
            format!("◆ submit_plan · Durable approval workflow {expected_status}")
        );
        assert_eq!(row.spans.last().unwrap().style.fg, Some(expected_color));
    }
}

#[test]
fn submit_plan_failure_surfaces_error_without_rendering_markdown() {
    let artifact = test_plan_artifact();
    let error =
        "status: error\nerror: markdown must begin with exactly `# Durable approval workflow`";
    let result = match tool_result_message("submit-plan-error", None, SUBMIT_PLAN_TOOL_NAME, error)
    {
        Message::User { content } => match content.into_iter().next() {
            Some(UserContent::ToolResult(result)) => result,
            _ => panic!("expected a tool result"),
        },
        _ => panic!("expected a user tool-result message"),
    };
    let message = assistant_message(vec![tool_call(
        "submit-plan-error",
        None,
        SUBMIT_PLAN_TOOL_NAME,
        json!({
            "title": artifact.title,
            "markdown": artifact.markdown,
        }),
    )]);
    let states = vec![Some(crate::app::ToolCallState {
        arguments: None,
        status: ToolCallStatus::Finished,
        result: Some(result),
        metadata: Some(
            file_metadata(
                "submit-plan-error",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                ToolCallOutcome::Error,
                Vec::new(),
            )
            .with_diagnostic("markdown must begin with exactly `# Durable approval workflow`"),
        ),
        subtasks: Default::default(),
    })];
    let mut lines = Vec::new();
    layout_native_message(&message, Some(&states), &mut lines, 160, None);

    let rendered = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("submit_plan · Durable approval workflow ✗"));
    assert!(rendered.contains("markdown must begin with exactly"));
    assert!(!rendered.contains("## Implementation"));
    assert!(!rendered.contains("Build it."));
    assert!(
        lines[2..]
            .iter()
            .all(|line| line.style.fg == Some(ZEVRIA_DARK.feedback.error))
    );
}

#[test]
fn submit_plan_titles_are_normalized_bounded_and_safe() {
    let grapheme = "🧑🏽‍💻";
    let oversized = grapheme.repeat(81);
    let cases = vec![
        (
            json!({"markdown": "SECRET_PLAN_BODY"}),
            "<invalid arguments>".to_string(),
        ),
        (
            json!({
                "title": "  Broken\n  title\twith whitespace  ",
                "markdown": "SECRET_PLAN_BODY",
            }),
            "Broken title with whitespace".to_string(),
        ),
        (
            json!({
                "title": oversized,
                "markdown": "SECRET_PLAN_BODY",
            }),
            format!("{}…", grapheme.repeat(80)),
        ),
    ];

    for (index, (arguments, expected_title)) in cases.into_iter().enumerate() {
        let message = assistant_message(vec![tool_call(
            &format!("submit-plan-title-{index}"),
            None,
            SUBMIT_PLAN_TOOL_NAME,
            arguments,
        )]);
        let states = vec![Some(crate::app::ToolCallState {
            arguments: None,
            status: ToolCallStatus::Executing,
            result: None,
            metadata: None,
            subtasks: Default::default(),
        })];
        let mut lines = Vec::new();
        layout_native_message(&message, Some(&states), &mut lines, 320, None);

        assert_eq!(lines.len(), 2, "titles must remain on one semantic row");
        let row_text = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(row_text, format!("◆ submit_plan · {expected_title} ◐"));
        assert!(!row_text.contains("SECRET_PLAN_BODY"));
    }
}

#[test]
fn submit_plan_rows_copy_markdown_and_keep_result_copy_behavior() {
    let artifact = test_plan_artifact();
    let object = json!({
        "title": artifact.title,
        "markdown": artifact.markdown,
    });
    let arguments = [
        object.clone(),
        serde_json::Value::String(object.to_string()),
    ];
    let output = "Plan artifact accepted";

    for (index, arguments) in arguments.into_iter().enumerate() {
        let id = format!("submit-plan-copy-{index}");
        let mut app = App::new();
        apply_turn_event(
            &mut app,
            SessionEvent::Intermediate {
                display_attempt_id: None,
                turn_id: TEST_TURN_ID,
                message: assistant_message(vec![tool_call(
                    &id,
                    None,
                    SUBMIT_PLAN_TOOL_NAME,
                    arguments,
                )]),
            },
        );
        apply_turn_event(
            &mut app,
            SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: tool_result_message(&id, None, SUBMIT_PLAN_TOOL_NAME, output),
                metadata: vec![file_metadata(
                    &id,
                    None,
                    SUBMIT_PLAN_TOOL_NAME,
                    ToolCallOutcome::Success,
                    Vec::new(),
                )],
            },
        );
        app.select_for_test(cursor(0, 0));

        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: artifact.markdown.clone(),
            })
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: output.to_string(),
            })
        );
    }

    let raw_arguments = "{not-json";
    let mut malformed = App::new();
    apply_turn_event(
        &mut malformed,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "submit-plan-copy-invalid",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                serde_json::Value::String(raw_arguments.to_string()),
            )]),
        },
    );
    malformed.select_for_test(cursor(0, 0));
    assert_eq!(
        malformed.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: raw_arguments.to_string(),
        })
    );
}

#[test]
fn successful_submit_plan_body_renders_once_live_and_after_restore() {
    let artifact = test_plan_artifact();
    let arguments = json!({
        "title": artifact.title,
        "markdown": artifact.markdown,
    });
    let output = "Plan artifact accepted. Make no more tool calls.";

    let mut live = App::new();
    apply_turn_event(
        &mut live,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "submit-plan-live",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                arguments.clone(),
            )]),
        },
    );
    apply_turn_event(
        &mut live,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: tool_result_message("submit-plan-live", None, SUBMIT_PLAN_TOOL_NAME, output),
            metadata: vec![file_metadata(
                "submit-plan-live",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                ToolCallOutcome::Success,
                Vec::new(),
            )],
        },
    );
    live.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact.clone(),
        },
    });
    live.set_view_for_test(0, false);
    let live_rendered = rendered_text(&mut live, 160, 80);
    assert!(live_rendered.contains("submit_plan · Durable approval workflow ✓"));
    assert!(live_rendered.contains("Plan artifact · revision 1"));
    assert_eq!(
        live_rendered.matches("Own approval in the engine.").count(),
        1
    );
    assert!(!live_rendered.contains(output));
    live.select_for_test(cursor(1, 0));
    assert_eq!(
        live.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown.clone(),
        })
    );

    let assistant = assistant_message(vec![tool_call(
        "submit-plan-restored",
        None,
        SUBMIT_PLAN_TOOL_NAME,
        arguments,
    )]);
    let mut restored = App::new();
    restored.restore(vec![
        TranscriptItem::Message(assistant),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: tool_result_message(
                "submit-plan-restored",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                output,
            ),
            metadata: vec![file_metadata(
                "submit-plan-restored",
                None,
                SUBMIT_PLAN_TOOL_NAME,
                ToolCallOutcome::Success,
                Vec::new(),
            )],
        },
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
    ]);
    restored.set_view_for_test(0, false);
    let restored_rendered = rendered_text(&mut restored, 160, 80);
    assert!(restored_rendered.contains("submit_plan · Durable approval workflow ✓"));
    assert!(restored_rendered.contains("Plan artifact · revision 1"));
    assert_eq!(
        restored_rendered
            .matches("Own approval in the engine.")
            .count(),
        1
    );
    assert!(!restored_rendered.contains(output));
    restored.select_for_test(cursor(1, 0));
    assert_eq!(
        restored.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: artifact.markdown,
        })
    );
}

#[test]
fn pending_root_operation_cancels_without_an_engine_turn_id() {
    let mut root = App::new();
    enter_insert(&mut root);
    root.set_input_for_test("pending prompt", "pending prompt".len());
    assert!(matches!(
        root.handle_event(ctrl_enter()),
        Some(UiAction::Submit { .. })
    ));
    let mut views = test_session_views(root);
    assert_eq!(
        views.handle_event(ctrl('c')),
        Some(UiAction::CancelTurn { turn_id: None })
    );
}

#[test]
fn retry_notice_is_replaced_by_accepted_progress() {
    let mut app = App::new();
    let turn_id = TurnId::new(89);
    start_empty_turn(&mut app, turn_id, SessionMode::Build);
    app.reduce_without_effects(SessionEvent::TurnRetrying {
        turn_id,
        attempt: 2,
        max_attempts: 4,
        retry_after: std::time::Duration::from_millis(500),
        error: "offline".to_string(),
    });
    assert!(app.retry_notice().is_some());
    assert!(rendered_text(&mut app, 100, 10).contains("reconnecting"));

    app.reduce_without_effects(SessionEvent::Intermediate {
        display_attempt_id: None,
        turn_id,
        message: Message::assistant("progress resumed"),
    });
    assert!(app.retry_notice().is_none());
    assert!(!rendered_text(&mut app, 100, 10).contains("reconnecting"));
}

#[test]
fn manual_compaction_busy_state_ends_on_completion_and_adds_one_marker() {
    let mut app = App::new();
    let turn_id = TurnId::new(11);
    app.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id,
        trigger: CompactionTrigger::Manual,
    });
    assert!(app.is_busy());
    assert!(app.is_compacting());
    assert_eq!(app.active_turn_id(), Some(turn_id));

    app.reduce_without_effects(SessionEvent::CompactionCompleted {
        turn_id,
        trigger: CompactionTrigger::Manual,
        backend: CompactionBackend::LocalSummary,
    });
    assert!(!app.is_busy());
    assert!(!app.is_compacting());
    assert_eq!(app.active_turn_id(), None);
    assert_eq!(
        app.history()
            .iter()
            .filter(|entry| matches!(entry, HistoryEntry::CompactionDivider))
            .count(),
        1
    );
}

#[test]
fn automatic_compaction_completion_keeps_the_surrounding_turn_active() {
    let mut app = App::new();
    let turn_id = TurnId::new(12);
    app.reduce_without_effects(SessionEvent::TurnStarted {
        turn_id,
        message: Message::user("question"),
        mode: SessionMode::Build,
    });
    app.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id,
        trigger: CompactionTrigger::AutomaticMidTurn,
    });
    app.reduce_without_effects(SessionEvent::CompactionCompleted {
        turn_id,
        trigger: CompactionTrigger::AutomaticMidTurn,
        backend: CompactionBackend::OpenaiResponsesCompact,
    });

    assert!(app.is_busy());
    assert!(!app.is_compacting());
    assert_eq!(app.active_turn_id(), Some(turn_id));
    assert!(matches!(
        app.history().last(),
        Some(HistoryEntry::CompactionDivider)
    ));
}

#[test]
fn restored_checkpoint_shows_only_the_marker_and_never_its_summary() {
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(Message::user("visible prompt")),
        TranscriptItem::Message(Message::assistant("visible answer")),
        TranscriptItem::Compaction(checkpoint()),
    ]);

    assert_eq!(app.history().len(), 3);
    assert!(matches!(
        app.history().last(),
        Some(HistoryEntry::CompactionDivider)
    ));
    assert!(!format!("{:?}", app.history()).contains("hidden summary"));
    rendered_text(&mut app, 100, 20);
    double_escape(&mut app);
    assert_ne!(
        app.selection().map(|selection| selection.history_index),
        Some(2),
        "the divider is not selectable"
    );
}

#[test]
fn agent_stream_previews_replace_accumulated_text_and_terminal_report_repairs_loss() {
    let turn_id = TurnId::new(88);
    let ensemble_run_id = EnsembleRunId::new();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "fake".to_string(),
        label: "Fake ACP".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id,
        start: EnsembleStart {
            run_id: ensemble_run_id.clone(),
            workflow: EnsembleWorkflow::Review,
            prompt: "review this".into(),
            agents: vec![descriptor.clone()],
        },
        resumed: false,
    });

    for (revision, text) in [(1, "hello"), (2, "hello world")] {
        let mut batch = SessionStreamBatch::default();
        batch.agent_runs.push(AgentRunStreamState {
            revision,
            turn_id,
            ensemble_run_id: ensemble_run_id.clone(),
            agent_run_id: descriptor.id.clone(),
            event: AgentRunEvent::AgentMessage {
                text: text.to_string(),
                message_id: Some("message-1".to_string()),
            },
        });
        views.apply_streams(&batch);
    }
    let pane = views.agent(&descriptor.id).expect("agent pane");
    assert_eq!(assistant_presentation_texts(pane), ["hello world"]);

    views.apply(SessionEvent::AgentRunUpdated {
        turn_id,
        ensemble_run_id: ensemble_run_id.clone(),
        agent_run_id: descriptor.id.clone(),
        event: AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
    });
    let mut batch = SessionStreamBatch::default();
    batch.agent_runs.push(AgentRunStreamState {
        revision: 3,
        turn_id,
        ensemble_run_id: ensemble_run_id.clone(),
        agent_run_id: descriptor.id.clone(),
        event: AgentRunEvent::AgentMessage {
            text: " after".to_string(),
            message_id: Some("message-1".to_string()),
        },
    });
    views.apply_streams(&batch);

    views.apply(SessionEvent::AgentRunFinished {
        turn_id,
        ensemble_run_id,
        outcome: AgentRunOutcome {
            confirmation: None,
            descriptor: descriptor.clone(),
            status: AgentRunStatus::Completed,
            report: "hello world after complete".to_string(),
            plan: None,
            partial: false,
            failure: None,
            usage: None,
            acp_session_id: Some("session-1".to_string()),
            user_decisions: Vec::new(),
            decision_ids: Vec::new(),
            unavailable_decisions: Vec::new(),
        },
    });
    let pane = views.agent(&descriptor.id).expect("agent pane");
    let messages = assistant_presentation_texts(pane);
    assert_eq!(messages, ["hello world after complete"]);

    assert_eq!(
        views.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::CONTROL,
        ))),
        None
    );
    assert_eq!(views.visible_agent_id(), Some(&descriptor.id));
    assert_eq!(
        views.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        ))),
        None,
        "ACP panes are inspect-only"
    );
}

#[test]
fn agent_stream_batch_keeps_unread_thought_before_following_message() {
    let turn_id = TurnId::new(89);
    let ensemble_run_id = EnsembleRunId::new();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "fake".to_string(),
        label: "Fake ACP".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id,
        start: EnsembleStart {
            run_id: ensemble_run_id.clone(),
            workflow: EnsembleWorkflow::Review,
            prompt: "review this".into(),
            agents: vec![descriptor.clone()],
        },
        resumed: false,
    });
    let batch = SessionStreamBatch {
        agent_runs: vec![
            AgentRunStreamState {
                revision: 1,
                turn_id,
                ensemble_run_id: ensemble_run_id.clone(),
                agent_run_id: descriptor.id.clone(),
                event: AgentRunEvent::Thought {
                    text: "inspect privately".to_string(),
                    message_id: Some("thought".to_string()),
                },
            },
            AgentRunStreamState {
                revision: 3,
                turn_id,
                ensemble_run_id,
                agent_run_id: descriptor.id.clone(),
                event: AgentRunEvent::AgentMessage {
                    text: "actionable finding".to_string(),
                    message_id: Some("message".to_string()),
                },
            },
        ],
        ..SessionStreamBatch::default()
    };

    views.apply_streams(&batch);

    let pane = views.agent(&descriptor.id).expect("agent pane");
    assert_eq!(assistant_reasoning_texts(pane), ["inspect privately"]);
    assert_eq!(assistant_presentation_texts(pane), ["actionable finding"]);
}

#[test]
fn acp_messages_use_native_roles_markdown_reasoning_and_inline_diagnostics() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Status {
            status: AgentRunStatus::Starting,
            detail: Some("booting".to_string()),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "review **this**".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "# Finding\n\nUse `checked_add`.".to_string(),
            message_id: Some("answer".to_string()),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Thought {
            text: "checking **risk**".to_string(),
            message_id: Some("thought".to_string()),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Stderr {
            text: "worker warning".to_string(),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "After the warning.".to_string(),
            message_id: Some("answer".to_string()),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::ModeChanged {
            mode: "read-only".to_string(),
        },
    );

    app.detach_follow_for_test();
    let polished = rendered_text(&mut app, 140, 40);
    assert!(polished.contains("● You"));
    assert!(
        polished.contains("review **this**"),
        "prompts stay plain text"
    );
    assert!(polished.contains("● Assistant"));
    assert!(polished.contains("Finding"));
    assert!(polished.contains("checked_add"));
    assert!(polished.contains("reasoning"));
    assert!(polished.contains("checking risk"));
    assert!(!polished.contains("ACP status"));
    assert!(!polished.contains("worker warning"));
    assert_eq!(polished.matches("● Assistant").count(), 1);

    assert_eq!(app.handle_event(key(KeyCode::Char('d'))), None);
    let diagnostic = rendered_text(&mut app, 140, 40);
    for expected in ["ACP status", "worker warning", "ACP mode"] {
        assert!(
            diagnostic.contains(expected),
            "missing {expected}: {diagnostic}"
        );
    }
    let status = diagnostic.find("ACP status").unwrap();
    let prompt = diagnostic.find("review **this**").unwrap();
    let finding = diagnostic.find("Finding").unwrap();
    let stderr = diagnostic.find("worker warning").unwrap();
    let tail = diagnostic.find("After the warning.").unwrap();
    assert!(status < prompt && prompt < finding && finding < stderr && stderr < tail);

    app.handle_event(key(KeyCode::Char('d')));
    rendered_text(&mut app, 140, 40);
    double_escape(&mut app);
    assert_eq!(
        app.selection(),
        Some(Selection {
            history_index: 1,
            content_index: 4,
        }),
        "hidden diagnostics must not become selectable"
    );
}

#[test]
fn acp_protocol_packets_do_not_turn_live_tokens_into_markdown_rows() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "review".to_string(),
            continuation: false,
            repair: None,
        },
    );
    for (snapshot, packet) in [
        ("I", "packet 1"),
        ("I’ll", "packet 2"),
        ("I’ll inspect", "packet 3"),
        ("I’ll inspect the repository.", "packet 4"),
    ] {
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::Protocol {
                direction: zevria_workflow::AgentProtocolDirection::AgentToClient,
                json: packet.to_string(),
            },
        );
        apply_agent_preview(
            &mut app,
            &mut transcript,
            AgentRunEvent::AgentMessage {
                text: snapshot.to_string(),
                message_id: Some("message-1".to_string()),
            },
        );
    }

    let HistoryEntry::Conversation(conversation) = &app.history()[0] else {
        panic!("ACP prompt cycle should be a conversation")
    };
    assert_eq!(
        conversation
            .blocks
            .iter()
            .filter(|block| matches!(block.kind, PresentationBlockKind::Text { .. }))
            .count(),
        2,
        "the prompt and one growing assistant message are the only text blocks"
    );
    assert_eq!(
        assistant_presentation_texts(&app),
        ["I’ll inspect the repository."]
    );

    app.detach_follow_for_test();
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("I’ll inspect the repository."));
    assert!(!rendered.contains("packet 1"));
}

#[test]
fn acp_tool_updates_mutate_one_native_style_row_and_support_y_and_yy() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "inspect the file".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::ToolCall {
            id: "tool-1".to_string(),
            title: "Read source".to_string(),
            kind: "read".to_string(),
            status: "in_progress".to_string(),
            content: Vec::new(),
            locations: vec![AgentRunLocation {
                path: PathBuf::from("src/lib.rs"),
                line: Some(12),
            }],
            raw_input: Some(json!({"path": "src/lib.rs"})),
            raw_output: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::ToolCallUpdate {
            id: "tool-1".to_string(),
            title: None,
            kind: None,
            status: Some("completed".to_string()),
            content: Some(vec!["successful output stays collapsed".to_string()]),
            locations: None,
            raw_input: None,
            raw_output: Some(json!("raw successful output")),
        },
    );

    let HistoryEntry::Conversation(conversation) = &app.history()[0] else {
        panic!("ACP prompt cycle should be a conversation")
    };
    assert_eq!(
        conversation
            .blocks
            .iter()
            .filter(|block| matches!(block.kind, PresentationBlockKind::Tool(_)))
            .count(),
        1,
        "sparse updates mutate the original row"
    );
    app.detach_follow_for_test();
    let completed = rendered_text(&mut app, 120, 30);
    assert!(completed.contains("◆ read · Read source ✓"));
    assert!(completed.contains("src/lib.rs:12"));
    assert!(!completed.contains("successful output stays collapsed"));

    app.select_for_test(cursor(0, 1));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "{\"path\":\"src/lib.rs\"}".to_string(),
        })
    );
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "successful output stays collapsed".to_string(),
        })
    );
    assert_eq!(app.handle_event(key(KeyCode::Enter)), None);

    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::ToolCallUpdate {
            id: "tool-1".to_string(),
            title: None,
            kind: None,
            status: Some("failed".to_string()),
            content: Some(vec!["permission denied".to_string()]),
            locations: Some(vec![AgentRunLocation {
                path: PathBuf::from("src/main.rs"),
                line: None,
            }]),
            raw_input: None,
            raw_output: Some(json!({"error": "permission denied"})),
        },
    );
    let failed = rendered_text(&mut app, 120, 30);
    assert!(failed.contains("◆ read · Read source ✗"));
    assert!(failed.contains("src/main.rs"));
    assert!(failed.contains("permission denied"));
    assert!(
        !failed.contains("src/lib.rs:12"),
        "locations are replacements"
    );
}

#[test]
fn acp_command_completion_and_failure_keep_one_descriptive_block_and_full_input_copy() {
    let command = format!("rtk cargo test {}", "long_test_name_".repeat(20));
    let title = format!(
        "command: {}…",
        command.chars().take(120).collect::<String>()
    );
    let input = json!({"command": command});
    for (status, indicator) in [("completed", "✓"), ("failed", "✗")] {
        let (mut app, mut transcript) = acp_transcript_app();
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::Prompt {
                text: "run verification".into(),
                continuation: false,
                repair: None,
            },
        );
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::ToolCall {
                id: "command-1".into(),
                title: title.clone(),
                kind: "execute".into(),
                status: "in_progress".into(),
                content: Vec::new(),
                locations: Vec::new(),
                raw_input: Some(input.clone()),
                raw_output: None,
            },
        );
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::ToolCallUpdate {
                id: "command-1".into(),
                title: None,
                kind: Some("execute".into()),
                status: Some(status.into()),
                content: Some(vec!["command output".into()]),
                locations: None,
                raw_input: Some(input.clone()),
                raw_output: Some(json!("command output")),
            },
        );
        let HistoryEntry::Conversation(conversation) = &app.history()[0] else {
            panic!("command conversation")
        };
        let tools = conversation
            .blocks
            .iter()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::Tool(crate::presentation::PresentedTool::Acp(tool)) => {
                    Some(tool)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].title, title);
        assert_eq!(tools[0].raw_input, Some(input.clone()));
        app.detach_follow_for_test();
        let rendered = rendered_text(&mut app, 240, 30);
        assert!(rendered.contains("command: rtk cargo test"));
        assert!(rendered.contains(&format!(" {indicator}")));
        assert!(!rendered.contains("command completed"));
        app.select_for_test(cursor(0, 1));
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: input.to_string()
            })
        );
        assert_eq!(
            app.handle_event(key(KeyCode::Char('y'))),
            Some(UiAction::Copy {
                text: "command output".into()
            })
        );

        // Sparse updates still honor deliberate title changes from other ACP agents.
        apply_agent_event(
            &mut app,
            &mut transcript,
            AgentRunEvent::ToolCallUpdate {
                id: "command-1".into(),
                title: Some("Explicit agent title".into()),
                kind: None,
                status: None,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        );
        assert!(rendered_text(&mut app, 160, 30).contains("Explicit agent title"));
    }
}

#[test]
fn acp_plan_snapshots_replace_one_checklist_block() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "make a plan".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: None,
                entries: vec![
                    AgentPlanEntry {
                        content: "old first".to_string(),
                        priority: "high".to_string(),
                        status: "pending".to_string(),
                    },
                    AgentPlanEntry {
                        content: "old second".to_string(),
                        priority: "low".to_string(),
                        status: "in_progress".to_string(),
                    },
                ],
            },
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: None,
                markdown: None,
                entries: vec![AgentPlanEntry {
                    content: "replacement".to_string(),
                    priority: "high".to_string(),
                    status: "completed".to_string(),
                }],
            },
        },
    );

    let HistoryEntry::Conversation(conversation) = &app.history()[0] else {
        panic!("ACP prompt cycle should be a conversation")
    };
    assert_eq!(conversation.blocks.len(), 2);
    app.detach_follow_for_test();
    let rendered = rendered_text(&mut app, 100, 25);
    assert!(rendered.contains("plan · 1/1 completed"));
    assert!(rendered.contains("replacement · high"));
    assert!(!rendered.contains("old first"));
    app.select_for_test(cursor(0, 1));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: "✓ replacement (high)".to_string(),
        })
    );
}

#[test]
fn acp_markdown_plan_replaces_representations_across_repair_and_matching_removal() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "make a plan".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("plan-1".to_string()),
                markdown: None,
                entries: vec![AgentPlanEntry {
                    content: "stale checklist".to_string(),
                    priority: "high".to_string(),
                    status: "pending".to_string(),
                }],
            },
        },
    );
    let exact = "# Final Markdown plan\n\n- Preserve exact bytes.  \n";
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("plan-1".to_string()),
                markdown: Some(exact.to_string()),
                entries: Vec::new(),
            },
        },
    );
    let rendered = rendered_text(&mut app, 100, 25);
    assert!(rendered.contains("Final Markdown plan"));
    assert!(rendered.contains("Preserve exact bytes"));
    assert!(!rendered.contains("stale checklist"));
    app.select_for_test(cursor(0, 1));
    assert_eq!(
        app.handle_event(key(KeyCode::Char('y'))),
        Some(UiAction::Copy {
            text: exact.to_string(),
        })
    );

    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "repair now".to_string(),
            continuation: true,
            repair: Some(AgentRunRepair::MissingPlanProof),
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("plan-1".to_string()),
                markdown: Some("# Repaired plan\n\n- Same block.".to_string()),
                entries: Vec::new(),
            },
        },
    );
    let HistoryEntry::Conversation(first) = &app.history()[0] else {
        panic!("first prompt entry")
    };
    let HistoryEntry::Conversation(second) = &app.history()[1] else {
        panic!("continuation entry")
    };
    assert_eq!(first.blocks.len(), 2);
    assert_eq!(
        first.blocks[1].primary_copy(),
        "# Repaired plan\n\n- Same block."
    );
    assert_eq!(
        second.blocks.len(),
        1,
        "repair updates the prior plan block"
    );

    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::PlanRemoved {
            plan_id: "other-plan".to_string(),
        },
    );
    assert!(rendered_text(&mut app, 100, 25).contains("Repaired plan"));
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("plan-1".to_string()),
                markdown: None,
                entries: vec![AgentPlanEntry {
                    content: "new checklist".to_string(),
                    priority: "low".to_string(),
                    status: "in_progress".to_string(),
                }],
            },
        },
    );
    let rendered = rendered_text(&mut app, 100, 25);
    assert!(rendered.contains("new checklist"));
    assert!(!rendered.contains("Repaired plan"));
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::PlanRemoved {
            plan_id: "plan-1".to_string(),
        },
    );
    let rendered = rendered_text(&mut app, 100, 25);
    assert!(!rendered.contains("new checklist"));
}

#[test]
fn acp_transient_continuation_retains_one_byte_exact_native_plan_block() {
    let (mut app, mut transcript) = acp_transcript_app();
    let exact = "# Retained native plan\n\n- Preserve exact bytes.  \n";
    for event in [
        AgentRunEvent::Prompt {
            text: "initial".into(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some(zevria_workflow::CLAUDE_PLAN_HANDOFF_PLAN_ID.into()),
                markdown: Some(exact.into()),
                entries: Vec::new(),
            },
        },
        AgentRunEvent::Prompt {
            text: "repair now".into(),
            continuation: true,
            repair: Some(AgentRunRepair::MissingPlanProof),
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Resuming,
            detail: Some("transient prompt failure".into()),
        },
        AgentRunEvent::Prompt {
            text: "repair now".into(),
            continuation: true,
            repair: None,
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
    ] {
        apply_agent_event(&mut app, &mut transcript, event);
    }
    let copies = app
        .history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(
                entry
                    .blocks
                    .iter()
                    .map(|block| block.primary_copy())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        copies.iter().filter(|copy| copy.as_str() == exact).count(),
        1
    );
    assert_eq!(
        rendered_text(&mut app, 100, 30)
            .matches("Retained native plan")
            .count(),
        1
    );
}

#[test]
fn acp_replay_replaces_history_suppresses_prompt_echo_and_repairs_report() {
    let (mut app, mut transcript) = acp_transcript_app();
    for event in [
        AgentRunEvent::Prompt {
            text: "old prompt".to_string(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "old answer".to_string(),
            message_id: Some("old".to_string()),
        },
        AgentRunEvent::ReplayBoundary,
        AgentRunEvent::Prompt {
            text: "new prompt".to_string(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::UserMessage {
            text: "new ".to_string(),
            message_id: Some("echo".to_string()),
        },
        AgentRunEvent::UserMessage {
            text: "prompt".to_string(),
            message_id: Some("echo".to_string()),
        },
        AgentRunEvent::AgentMessage {
            text: "partial".to_string(),
            message_id: Some("new".to_string()),
        },
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("replayed-plan".to_string()),
                markdown: Some("# Replayed Markdown plan".to_string()),
                entries: Vec::new(),
            },
        },
        AgentRunEvent::SessionEstablished {
            session_id: "reloaded-session".to_string(),
            capabilities: json!({}),
            safe_mode: "read-only".to_string(),
            recovered: true,
        },
    ] {
        apply_agent_event(&mut app, &mut transcript, event);
    }
    reconcile_agent_report(&mut app, &mut transcript, "complete report");

    let all_copy = app
        .history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(
                entry
                    .blocks
                    .iter()
                    .map(|block| block.primary_copy())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!all_copy.contains("old prompt"));
    assert!(!all_copy.contains("old answer"));
    assert_eq!(all_copy.matches("new prompt").count(), 1);
    assert_eq!(assistant_presentation_texts(&app), ["complete report"]);
    assert!(all_copy.contains("# Replayed Markdown plan"));

    app.detach_follow_for_test();
    let polished = rendered_text(&mut app, 100, 25);
    assert!(!polished.contains("ACP replay"));
    app.handle_event(key(KeyCode::Char('d')));
    let diagnostic = rendered_text(&mut app, 100, 25);
    assert!(diagnostic.contains("ACP replay"));
}

#[test]
fn failed_acp_replay_preserves_the_historical_worker_pane() {
    let (mut app, mut transcript) = acp_transcript_app();
    for event in [
        AgentRunEvent::Prompt {
            text: "original prompt".to_string(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "useful historical answer".to_string(),
            message_id: Some("old".to_string()),
        },
        AgentRunEvent::ToolCall {
            id: "old-tool".to_string(),
            title: "inspect old state".to_string(),
            kind: "read".to_string(),
            status: "completed".to_string(),
            content: vec!["historical tool output".to_string()],
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
        },
        AgentRunEvent::ReplayBoundary,
        AgentRunEvent::AgentMessage {
            text: "uncommitted replay".to_string(),
            message_id: Some("replay".to_string()),
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Interrupted,
            detail: Some("session/load failed".to_string()),
        },
    ] {
        apply_agent_event(&mut app, &mut transcript, event);
    }

    let rendered = rendered_text(&mut app, 100, 25);
    assert!(rendered.contains("useful historical answer"));
    assert!(rendered.contains("inspect old state"));
    assert!(!rendered.contains("uncommitted replay"));
}

#[test]
fn acp_continuation_report_reconciliation_does_not_duplicate_prior_evidence() {
    let (mut app, mut transcript) = acp_transcript_app();
    for event in [
        AgentRunEvent::Prompt {
            text: "original prompt".to_string(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "durable partial".to_string(),
            message_id: Some("old".to_string()),
        },
        AgentRunEvent::Prompt {
            text: "finish without repeating".to_string(),
            continuation: true,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "new conclusion".to_string(),
            message_id: Some("new".to_string()),
        },
    ] {
        apply_agent_event(&mut app, &mut transcript, event);
    }

    reconcile_agent_report(&mut app, &mut transcript, "durable partial\nnew conclusion");

    assert_eq!(
        assistant_presentation_texts(&app),
        ["durable partial", "new conclusion"]
    );
}

#[test]
fn acp_pane_title_tracks_safe_mode_status_usage_and_diagnostic_hint() {
    let turn_id = TurnId::new(101);
    let run_id = EnsembleRunId::new();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "fake".to_string(),
        label: "Fake ACP".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let mut views = test_session_views(configured_app());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id,
        start: EnsembleStart {
            run_id: run_id.clone(),
            workflow: EnsembleWorkflow::Review,
            prompt: "review".into(),
            agents: vec![descriptor.clone()],
        },
        resumed: false,
    });
    for event in [
        AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
        AgentRunEvent::Usage {
            usage: AgentUsage {
                used: 1_200,
                size: 8_000,
                cost: Some(json!({"usd": 99})),
            },
        },
    ] {
        views.apply(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: run_id.clone(),
            agent_run_id: descriptor.id.clone(),
            event,
        });
    }
    views.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('i'),
        KeyModifiers::CONTROL,
    )));
    let hidden = rendered_views_text(&mut views, 360, 24);
    assert!(hidden.contains("ACP · Fake ACP · read-only · running"));
    assert!(hidden.contains("context 1.2k/8.0k"));
    assert!(context_help(views.agent(&descriptor.id).unwrap()).contains("d diagnostics"));
    assert!(!hidden.contains("provider-review/review-model"));
    assert!(!hidden.contains("usd"));
    assert!(!hidden.contains("ACP usage"));

    views.handle_event(key(KeyCode::Char('d')));
    let visible = rendered_views_text(&mut views, 360, 24);
    assert!(context_help(views.agent(&descriptor.id).unwrap()).contains("d hide diagnostics"));
    assert!(visible.contains("ACP usage"));

    views.apply(SessionEvent::AgentRunFinished {
        turn_id,
        ensemble_run_id: run_id,
        outcome: AgentRunOutcome {
            confirmation: None,
            descriptor,
            status: AgentRunStatus::Completed,
            report: "complete".to_string(),
            plan: None,
            partial: false,
            failure: None,
            usage: Some(AgentUsage {
                used: 3_000,
                size: 8_000,
                cost: None,
            }),
            acp_session_id: Some("live-session".to_string()),
            user_decisions: Vec::new(),
            decision_ids: Vec::new(),
            unavailable_decisions: Vec::new(),
        },
    });
    let terminal = rendered_views_text(&mut views, 360, 24);
    assert!(terminal.contains("ACP · Fake ACP · read-only · completed"));
    assert!(terminal.contains("context 3.0k/8.0k"));
}

#[test]
fn acp_pane_transitions_from_resuming_to_running_without_losing_plan() {
    let turn_id = TurnId::new(102);
    let run_id = EnsembleRunId::new();
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "fake".into(),
        label: "Recovery ACP".into(),
        safe_mode: "plan".into(),
    };
    let mut views = test_session_views(configured_app());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id,
        start: EnsembleStart {
            run_id: run_id.clone(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "plan".into(),
            agents: vec![descriptor.clone()],
        },
        resumed: true,
    });
    views.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('i'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        rendered_views_text(&mut views, 180, 24).contains("ACP · Recovery ACP · plan · resuming")
    );
    for event in [
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("native-proof".into()),
                markdown: Some("# Retained recovery proof".into()),
                entries: Vec::new(),
            },
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Resuming,
            detail: Some("transient prompt failure".into()),
        },
        AgentRunEvent::Prompt {
            text: "continue".into(),
            continuation: true,
            repair: None,
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
    ] {
        views.apply(SessionEvent::AgentRunUpdated {
            turn_id,
            ensemble_run_id: run_id.clone(),
            agent_run_id: descriptor.id.clone(),
            event,
        });
    }
    let rendered = rendered_views_text(&mut views, 180, 24);
    assert!(rendered.contains("ACP · Recovery ACP · plan · running"));
    assert_eq!(rendered.matches("Retained recovery proof").count(), 1);
}

#[test]
fn restored_acp_outcome_supplies_context_without_fabricating_a_model() {
    let run_id = EnsembleRunId::from_string("restored-usage-run");
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::from_string("restored-usage-agent"),
        agent: "fake".to_string(),
        label: "Historical ACP".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let mut views = test_session_views(configured_app());
    views.restore_agent_run(vec![
        AgentRunTranscriptRecord::Header {
            header: AgentRunTranscriptHeader {
                version: AGENT_RUN_TRANSCRIPT_VERSION,
                ensemble_run_id: run_id,
                workflow: EnsembleWorkflow::Review,
                descriptor: descriptor.clone(),
                prompt: "historical review".into(),
            },
        },
        AgentRunTranscriptRecord::Outcome {
            outcome: AgentRunOutcome {
                confirmation: None,
                descriptor: descriptor.clone(),
                status: AgentRunStatus::Completed,
                report: "historical report".to_string(),
                plan: None,
                partial: false,
                failure: None,
                usage: Some(AgentUsage {
                    used: 2_500,
                    size: 16_000,
                    cost: Some(json!({"usd": 1.25})),
                }),
                acp_session_id: Some("restored-session".to_string()),
                user_decisions: Vec::new(),
                decision_ids: Vec::new(),
                unavailable_decisions: Vec::new(),
            },
        },
    ]);
    views.handle_event(ctrl('i'));
    let rendered = rendered_views_text(&mut views, 180, 12);
    assert!(rendered.contains("ACP · Historical ACP · read-only · completed · historical"));
    assert!(rendered.contains("context 2.5k/16.0k"));
    assert!(!rendered.contains("provider-review/review-model"));
    assert!(!rendered.contains("usd"));
}

#[test]
fn proofless_completed_plan_worker_is_defensively_rendered_as_failed_red() {
    let mut app = configured_app();
    let run_id = EnsembleRunId::from_string("proofless-row-run");
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::from_string("proofless-row-agent"),
        agent: "fake".to_string(),
        label: "Proofless worker".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let turn_id = TurnId::new(801);
    app.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id,
        start: EnsembleStart {
            run_id: run_id.clone(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "plan".into(),
            agents: vec![descriptor.clone()],
        },
        resumed: false,
    });
    app.reduce_without_effects(SessionEvent::AgentRunFinished {
        turn_id,
        ensemble_run_id: run_id,
        outcome: AgentRunOutcome {
            confirmation: None,
            descriptor,
            status: AgentRunStatus::Completed,
            report: "ordinary prose".to_string(),
            plan: None,
            partial: false,
            failure: None,
            usage: None,
            acp_session_id: Some("proofless".to_string()),
            user_decisions: Vec::new(),
            decision_ids: Vec::new(),
            unavailable_decisions: Vec::new(),
        },
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 12)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let row = (0..12)
        .find(|y| {
            (0..100)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("Proofless worker")
        })
        .expect("worker row");
    let text = (0..100)
        .map(|x| buffer[(x, row)].symbol())
        .collect::<String>();
    assert!(text.contains("failed"));
    assert!(!text.contains("completed"));
    let label_start = text.find("Proofless worker").expect("label position") as u16;
    assert_eq!(buffer[(label_start, row)].fg, ZEVRIA_DARK.feedback.error);
    assert!(
        rendered_text(&mut app, 100, 12)
            .contains("Plan worker reported Completed without final Markdown proof")
    );
}

#[test]
fn restored_structured_claude_handoff_renders_one_plan_without_duplicate_report() {
    let run_id = EnsembleRunId::from_string("historical-claude-run");
    let descriptor = AgentRunDescriptor {
        id: AgentRunId::from_string("historical-claude-agent"),
        agent: "claude".to_string(),
        label: "Historical Claude".to_string(),
        safe_mode: "plan".to_string(),
    };
    let markdown = "# Historical native plan\n\n- Render exactly once.";
    let mut views = test_session_views(configured_app());
    views.restore_agent_run(vec![
        AgentRunTranscriptRecord::Header {
            header: AgentRunTranscriptHeader {
                version: AGENT_RUN_TRANSCRIPT_VERSION,
                ensemble_run_id: run_id,
                workflow: EnsembleWorkflow::Plan,
                descriptor: descriptor.clone(),
                prompt: "historical plan".into(),
            },
        },
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Prompt {
                text: "historical plan".to_string(),
                continuation: false,
                repair: None,
            },
        },
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Plan {
                plan: AgentStructuredPlan {
                    plan_id: Some(CLAUDE_PLAN_HANDOFF_PLAN_ID.to_string()),
                    markdown: Some(markdown.to_string()),
                    entries: vec![],
                },
            },
        },
        AgentRunTranscriptRecord::Outcome {
            outcome: AgentRunOutcome {
                confirmation: None,
                descriptor,
                status: AgentRunStatus::Completed,
                report: String::new(),
                plan: Some(AgentStructuredPlan {
                    plan_id: Some(CLAUDE_PLAN_HANDOFF_PLAN_ID.to_string()),
                    markdown: Some(markdown.to_string()),
                    entries: vec![],
                }),
                partial: true,
                failure: None,
                usage: None,
                acp_session_id: Some("historical-session".to_string()),
                user_decisions: Vec::new(),
                decision_ids: Vec::new(),
                unavailable_decisions: Vec::new(),
            },
        },
    ]);
    views.handle_event(ctrl('i'));
    let rendered = rendered_views_text(&mut views, 180, 18);
    assert!(rendered.contains("Historical native plan"));
    assert_eq!(rendered.matches("Historical native plan").count(), 1);
    assert_eq!(rendered.matches("Render exactly once").count(), 1);
}

#[test]
fn turn_rejection_cannot_settle_correlated_model_management() {
    let mut app = App::new();
    assert_eq!(app.begin_model_management(), Some(SessionMode::Build));
    app.reduce_without_effects(SessionEvent::TurnRejected {
        turn_id: TurnId::new(8),
        error: "unrelated turn".into(),
    });
    assert!(app.is_busy());
    assert!(app.history().is_empty());
    app.finish_model_management();
    assert!(!app.is_busy());
}
