use super::*;

fn build_launch() -> SubtaskLaunchMetadata {
    SubtaskLaunchMetadata {
        id: SubtaskId::new("build-child"),
        title: "Create landing page".into(),
        kind: SubtaskKind::Build,
        workspace: Some("pages/book-1".into()),
    }
}

fn build_assistant() -> Message {
    assistant_message(vec![tool_call(
        "fc-build",
        Some("build"),
        "launch_subtasks",
        json!({"tasks":[{"title":"Create landing page","prompt":"PRIVATE PROMPT","type":"build","workspace":"pages/book-1"}]}),
    )])
}

#[test]
fn late_build_descriptor_reconciles_activity_compaction_and_keeps_telemetry_separate() {
    for compacting in [false, true] {
        let id = SubtaskId::new("build-child");
        let mut views = test_session_views(App::new().with_model_profiles([(
            ModelRole::Builder,
            ModelProfileRef::new("configured", "builder"),
        )]));
        views.apply(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("launch"),
            mode: SessionMode::Build,
        });
        views.apply(SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: build_assistant(),
        });
        views.apply(SessionEvent::SubtaskSession {
            id: id.clone(),
            event: Box::new(SessionEvent::TurnStarted {
                turn_id: TEST_TURN_ID,
                message: Message::user("child"),
                mode: SessionMode::Build,
            }),
        });
        if compacting {
            views.apply(SessionEvent::SubtaskSession {
                id: id.clone(),
                event: Box::new(SessionEvent::CompactionStarted {
                    turn_id: TEST_TURN_ID,
                    trigger: CompactionTrigger::AutomaticMidTurn,
                }),
            });
        }
        for role in [ModelRole::Explore, ModelRole::Builder] {
            views.apply(SessionEvent::SubtaskSession {
                id: id.clone(),
                event: Box::new(SessionEvent::ContextUsageUpdated {
                    turn_id: TEST_TURN_ID,
                    snapshot: ContextTokenSnapshot {
                        profile: ModelProfileRef::new("runtime", role.name()),
                        model_role: role,
                        projected_input_tokens: if role == ModelRole::Builder {
                            7500
                        } else {
                            123
                        },
                        source: ContextTokenSource::Exact,
                        automatic_trigger: 40000,
                        input_token_limit: 50000,
                        context_window_tokens: 64000,
                    },
                }),
            });
        }
        assert_eq!(
            views.child(&id).unwrap().in_flight_role(),
            Some(ModelRole::Explore)
        );
        views.apply(SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: id.clone(),
            status: SubtaskStatus::Failed,
        });
        views.apply(SessionEvent::SubtaskLaunched {
            turn_id: TEST_TURN_ID,
            call_id: "build".into(),
            entry_index: 0,
            descriptor: SubtaskDescriptor {
                id: id.clone(),
                title: "Create landing page".into(),
                kind: SubtaskKind::Build,
                workspace: Some("pages/book-1".into()),
                parent_session_id: "root".into(),
                status: SubtaskStatus::Starting,
            },
        });
        let child = views.child(&id).unwrap();
        assert_eq!(child.in_flight_role(), Some(ModelRole::Builder));
        assert!(
            child
                .subsession_title()
                .unwrap()
                .contains("build · Create landing page · pages/book-1 · failed")
        );
        assert_eq!(
            attached_subtask_status(views.root(), &id),
            Some(SubtaskStatus::Failed)
        );
        views.apply(SessionEvent::SubtaskStatus {
            turn_id: TEST_TURN_ID,
            id: id.clone(),
            status: SubtaskStatus::Running,
        });
        assert_eq!(
            attached_subtask_status(views.root(), &id),
            Some(SubtaskStatus::Failed)
        );
        views.handle_event(ctrl('i'));
        let rendered = rendered_views_text(&mut views, 180, 14);
        assert!(rendered.contains("runtime/builder"));
        assert!(!rendered.contains("runtime/explore"));
    }
}

#[test]
fn build_restore_and_all_terminal_outcomes_keep_workspace_correlation() {
    for (outcome, status) in [
        (ToolCallOutcome::Success, SubtaskStatus::Completed),
        (ToolCallOutcome::Error, SubtaskStatus::Failed),
        (ToolCallOutcome::Cancelled, SubtaskStatus::Cancelled),
    ] {
        let mut metadata = launch_metadata("build", "build-child", "Create landing page", outcome);
        metadata.detail = Some(ToolResultDetail::Subtasks(vec![
            zevria_foundation::SubtaskEntryMetadata {
                index: 0,
                status,
                launch: Some(build_launch()),
            },
        ]));
        let mut root = App::new();
        root.restore(vec![
            TranscriptItem::Message(Message::user("launch")),
            TranscriptItem::Message(build_assistant()),
            TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: tool_result_message(
                    "fc-build",
                    Some("build"),
                    "launch_subtasks",
                    "PRIVATE REPORT",
                ),
                metadata: vec![metadata],
            },
        ]);
        let descriptor = root
            .history()
            .iter()
            .find_map(|entry| {
                let HistoryEntry::Conversation(entry) = entry else {
                    return None;
                };
                entry.blocks.iter().find_map(|block| {
                    block
                        .native_tool()
                        .and_then(|(_, state)| state.subtasks.values().next())
                })
            })
            .unwrap();
        assert_eq!(descriptor.workspace.as_deref(), Some("pages/book-1"));
        assert_eq!(descriptor.status, status);
        let rendered = rendered_text(&mut root, 120, 18);
        assert!(rendered.contains(&format!(
            "◆ build · Create landing page · pages/book-1 {}",
            crate::presentation::subtask_icon(status).glyph(0)
        )));
        assert!(!rendered.contains("PRIVATE"));
        let mut views = test_session_views(root.with_model_profiles([(
            ModelRole::Builder,
            ModelProfileRef::new("configured", "builder"),
        )]));
        views.restore_child(
            SubtaskId::new("build-child"),
            Some(build_launch()),
            vec![TranscriptItem::Message(Message::assistant(
                "historical output",
            ))],
        );
        let child = views.child(&SubtaskId::new("build-child")).unwrap();
        assert_eq!(
            child.subsession_title(),
            Some("build · Create landing page · pages/book-1 · historical")
        );
        assert!(child.inspect_only());
        views.handle_event(ctrl('i'));
        assert!(rendered_views_text(&mut views, 180, 14).contains("configured/builder"));
    }
}

#[test]
fn descriptorless_failed_build_rows_use_argument_kind_without_inventing_a_child() {
    let mut app = App::new();
    app.restore(vec![
        TranscriptItem::Message(Message::user("launch")),
        TranscriptItem::Message(build_assistant()),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: tool_result_message(
                "fc-build",
                Some("build"),
                "launch_subtasks",
                format!(
                    "status: error\nerror: workspace overlaps reserved directory {}",
                    "x".repeat(500)
                ),
            ),
            metadata: vec![
                unlaunched_metadata("build", ToolCallOutcome::Error).with_diagnostic(format!(
                    "workspace overlaps reserved directory {}",
                    "x".repeat(500)
                )),
            ],
        },
    ]);
    let rendered = rendered_text(&mut app, 100, 20);
    assert!(rendered.contains("subtask launch ✗ · workspace overlaps reserved"));
    assert!(!rendered.contains("PRIVATE PROMPT"));
    assert!(!rendered.contains(&"x".repeat(500)));
    assert_eq!(
        attached_subtask_status(&app, &SubtaskId::new("build-child")),
        None
    );
}

#[test]
fn role_setter_only_changes_inspect_activity_not_root_or_worker_capabilities() {
    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    root.set_inspect_model_role(ModelRole::Builder);
    assert_eq!(root.in_flight_role(), Some(ModelRole::Build));
    let mut child = App::subtask_inspect("placeholder");
    start_empty_turn(&mut child, TEST_TURN_ID, SessionMode::Build);
    child.set_inspect_model_role(ModelRole::Builder);
    assert_eq!(child.in_flight_role(), Some(ModelRole::Builder));
    assert!(child.inspect_only());
}
