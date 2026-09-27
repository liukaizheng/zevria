use super::*;

fn assistant() -> Message {
    assistant_message(vec![tool_call(
        "fc-batch",
        Some("batch"),
        "launch_subtasks",
        json!({"tasks": [
            {"title":"Duplicate title","prompt":"SECRET FIRST","type":"build","workspace":"pages/a"},
            {"title":"Duplicate title","prompt":"SECRET SECOND","type":"explore","workspace":null}
        ]}),
    )])
}
fn descriptor(index: usize) -> SubtaskDescriptor {
    SubtaskDescriptor {
        id: SubtaskId::new(format!("child-{index}")),
        parent_session_id: "root".into(),
        title: "Duplicate title".into(),
        kind: if index == 0 {
            SubtaskKind::Build
        } else {
            SubtaskKind::Explore
        },
        workspace: (index == 0).then(|| "pages/a".into()),
        status: SubtaskStatus::Starting,
    }
}
fn metadata() -> ToolResultMetadata {
    ToolResultMetadata {
        diagnostic: None,
        id: "batch".into(),
        call_id: Some("batch".into()),
        tool_name: "launch_subtasks".into(),
        outcome: ToolCallOutcome::Cancelled,
        detail: Some(ToolResultDetail::Subtasks(
            (0..2)
                .map(|index| {
                    let child = descriptor(index);
                    zevria_foundation::SubtaskEntryMetadata {
                        index,
                        status: if index == 0 {
                            SubtaskStatus::Completed
                        } else {
                            SubtaskStatus::Cancelled
                        },
                        launch: Some(SubtaskLaunchMetadata {
                            id: child.id,
                            title: child.title,
                            kind: child.kind,
                            workspace: child.workspace,
                        }),
                    }
                })
                .collect(),
        )),
    }
}
fn result() -> Message {
    tool_result_message(
        "fc-batch",
        Some("batch"),
        "launch_subtasks",
        "SECRET REPORTS",
    )
}

fn rows(
    app: &App,
) -> Vec<(
    crate::presentation::PresentationBlockId,
    usize,
    SubtaskDescriptor,
)> {
    app.history()
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::Conversation(entry) => Some(entry),
            _ => None,
        })
        .flat_map(|entry| &entry.blocks)
        .filter_map(|block| match &block.kind {
            PresentationBlockKind::Subtask {
                entry_index,
                descriptor,
                ..
            } => Some((block.id, *entry_index, descriptor.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn one_batch_rows_keep_stable_selection_and_independent_panes_with_duplicate_titles_and_reordered_events()
 {
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::TurnStarted {
        turn_id: TEST_TURN_ID,
        message: Message::user("launch both"),
        mode: SessionMode::Build,
    });
    views.apply(SessionEvent::Intermediate {
        turn_id: TEST_TURN_ID,
        message: assistant(),
        display_attempt_id: None,
    });
    views.apply(SessionEvent::SubtaskStatus {
        turn_id: TEST_TURN_ID,
        id: descriptor(1).id,
        status: SubtaskStatus::Running,
    });
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "batch".into(),
        entry_index: 1,
        descriptor: descriptor(1),
    });
    let stable_second = rows(views.root())[0].0;
    rendered_views_text(&mut views, 120, 24);
    views.handle_event(key(KeyCode::Esc));
    views.handle_event(key(KeyCode::Esc));
    assert_eq!(views.root().selection(), cursor(1, 1));
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "batch".into(),
        entry_index: 0,
        descriptor: descriptor(0),
    });
    let current = rows(views.root());
    assert_eq!(
        current.iter().map(|row| row.1).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(current[1].0, stable_second);
    assert_eq!(current[1].2.status, SubtaskStatus::Running);
    assert_eq!(
        views.root().selection(),
        cursor(1, 2),
        "selection follows stable child, not old position or duplicate title"
    );
    // Enter promotes block selection then opens the independently selectable child pane.
    views.handle_event(key(KeyCode::Enter));
    views.handle_event(key(KeyCode::Enter));
    assert_eq!(views.visible_child_id(), Some(&descriptor(1).id));
    views.handle_event(ctrl('o'));
    views.handle_event(key(KeyCode::Up));
    views.handle_event(key(KeyCode::Enter));
    assert_eq!(views.visible_child_id(), Some(&descriptor(0).id));
    views.handle_event(ctrl('o'));
    views.apply(SessionEvent::ToolResults {
        turn_id: TEST_TURN_ID,
        message: result(),
        metadata: vec![metadata()],
    });
    // Delayed duplicates cannot add rows or regress individual terminal states.
    for index in [1, 0] {
        views.apply(SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "batch".into(),
            entry_index: index,
            descriptor: descriptor(index),
        });
        views.apply(SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: descriptor(index).id,
            status: SubtaskStatus::Running,
        });
    }
    let current = rows(views.root());
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].2.status, SubtaskStatus::Completed);
    assert_eq!(current[1].2.status, SubtaskStatus::Cancelled);
    let rendered = rendered_views_text(&mut views, 160, 30);
    assert!(!rendered.contains("subtasks ·"));
    assert!(!rendered.contains("subtask launch"));
    assert!(rendered.contains("◆ build · Duplicate title · pages/a ✓"));
    assert!(rendered.contains("◆ explore · Duplicate title ◼"));
    assert!(!rendered.contains("SECRET"));
}

#[test]
fn plural_terminal_fallback_and_restore_create_both_rows_and_panes_without_launch_events() {
    let items = vec![
        TranscriptItem::Message(Message::user("launch both")),
        TranscriptItem::Message(assistant()),
        TranscriptItem::ToolResults {
            message: result(),
            metadata: vec![metadata()],
            skill_applications: vec![],
        },
    ];
    let items: Vec<TranscriptItem> =
        serde_json::from_str(&serde_json::to_string(&items).unwrap()).unwrap();
    for restored in [false, true] {
        let mut root = App::new();
        if restored {
            root.restore(items.clone());
        }
        let mut views = test_session_views(root);
        if restored {
            views.seed_child_creation_order(&items);
        } else {
            views.apply(SessionEvent::TurnStarted {
                turn_id: TEST_TURN_ID,
                message: Message::user("launch both"),
                mode: SessionMode::Build,
            });
            views.apply(SessionEvent::Intermediate {
                turn_id: TEST_TURN_ID,
                message: assistant(),
                display_attempt_id: None,
            });
            views.apply(SessionEvent::ToolResults {
                turn_id: TEST_TURN_ID,
                message: result(),
                metadata: vec![metadata()],
            });
        }
        let current = rows(views.root());
        assert_eq!(current.len(), 2);
        assert_eq!(current[0].2.status, SubtaskStatus::Completed);
        assert_eq!(current[1].2.status, SubtaskStatus::Cancelled);
        for index in 0..2 {
            let child = views
                .child(&descriptor(index).id)
                .expect("accepted child pane even without a launch event or child transcript");
            assert!(child.inspect_only());
            assert!(child.subsession_title().unwrap().contains(if index == 0 {
                "completed"
            } else {
                "cancelled"
            }));
            assert_eq!(
                child.subsession_title().unwrap().contains("historical"),
                restored
            );
        }
    }
}
