use super::*;
use zevria_foundation::SKILL_READ_TOOL_NAME;

#[tokio::test]
async fn skill_management_preserves_pins_disables_all_paths_and_rejects_stale_mutations() {
    use zevria_instructions::skill::*;
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("activated")),
        Ok(Message::assistant("reapplied")),
    ]);
    let (_dir, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap()
    .with_skill_management(Arc::new(TestSkillManagement), [true, false])
    .unwrap();
    let (events, mut receiver) = session_event_channel(64);
    let name = SkillName::parse("review").unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: name.clone(),
                args: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let identities = engine
        .active_skills()
        .unwrap()
        .snapshots()
        .map(|s| s.digest())
        .collect::<Vec<_>>();
    let before = engine.conversation.items().len();
    while receiver.try_recv().is_ok() {}
    let disable = SkillManagementRequest::SetEnabled {
        expected_revision: engine.skills.catalog.revision().into(),
        name: name.clone(),
        enabled: false,
    };
    engine
        .handle_command(
            SessionCommand::Manage(crate::session::ManagementCommand::Skills {
                request_id: "disable".into(),
                request: disable,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .active_skills()
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        identities
    );
    assert_eq!(
        engine.conversation.items().len(),
        before + 1,
        "disablement appends one durable revocation"
    );
    let context = engine.captured_skill_context(engine.active_skills().unwrap(), true);
    assert!(
        context
            .resolve(&name, SkillInvocationOrigin::Explicit)
            .is_err()
    );
    assert!(
        context
            .resolve(&name, SkillInvocationOrigin::Model)
            .is_err()
    );
    assert!(context.render_active_context().is_none());
    let mut seen_change = false;
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            match event {
                SessionEvent::SkillsChanged { .. } => seen_change = true,
                SessionEvent::SkillsResult {
                    request_id,
                    result: SkillManagementResult::Changed { .. },
                } => {
                    assert_eq!(request_id, "disable");
                    assert!(seen_change, "install/invalidation precedes success");
                }
                _ => {}
            }
        }
    }
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: name.clone(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.conversation.items().len(), before + 1);
    engine
        .handle_command(
            SessionCommand::Manage(crate::session::ManagementCommand::Skills {
                request_id: "enable".into(),
                request: SkillManagementRequest::SetEnabled {
                    expected_revision: engine.skills.catalog.revision().into(),
                    name: name.clone(),
                    enabled: true,
                },
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .active_skills()
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        identities
    );
    assert_eq!(
        engine.active_skills().unwrap().get(&name).unwrap().digest(),
        identities[0]
    );
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: name.clone(),
                args: "again".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .active_skills()
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        identities
    );
    assert_eq!(
        engine
            .conversation
            .items()
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        1
    );
    assert!(!engine.policies.policy(SessionMode::Plan).skills_enabled);
}

#[tokio::test]
async fn missing_skill_cannot_revise_a_ready_plan_and_valid_name_uses_plan_mode() {
    use zevria_instructions::skill::*;
    let (artifact, items) = ready_plan_fixture();
    let (_dir, mut transcript) = test_transcript();
    persist_fixture(&mut transcript, &items);
    let provider = ScriptedProvider::new([Ok(Message::assistant("revised using skill"))]);
    let requests = provider.requests.clone();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap()
    .with_fixture(items.clone())
    .unwrap()
    .with_skill_management(Arc::new(TestSkillManagement), [true, true])
    .unwrap();
    engine
        .policies
        .policy_mut(SessionMode::Plan)
        .allowed_tool_names = Some(vec!["command".into(), "skill".into()]);
    let (events, _receiver) = session_event_channel(32);
    let name = SkillName::parse("review").unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::RevisePlanWithSkill {
                expected: artifact.version,
                name: "missing".parse().unwrap(),
                args: "revision".into(),
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.conversation.items(), items);
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Ready { .. }
    ));
    assert!(requests.lock().unwrap().is_empty());
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::RevisePlanWithSkill {
                expected: artifact.version,
                name,
                args: "revision".into(),
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning { .. }
    ));
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    assert!(engine.conversation.items().iter().any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::RevisionRequested { artifact: recorded }) if recorded.version == artifact.version)));
}

#[tokio::test]
async fn same_name_first_use_edit_preserves_full_snapshot_after_catalog_change() {
    use zevria_instructions::skill::*;
    let mut metadata = SkillMetadata::new("Review changes");
    metadata.interface.display_name = Some("Pinned metadata".into());
    let original = test_skills()
        .get(&SkillName::parse("review").unwrap())
        .unwrap()
        .clone()
        .with_metadata(metadata.clone(), None)
        .unwrap();
    let identity = original.digest();
    let registry = Arc::new(SkillCatalog::new([original.clone()]).unwrap());
    let (_dir, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("first")),
            Ok(Message::assistant("edited")),
        ]),
        test_skill_tools(),
        test_policies(),
        transcript,
        registry,
    )
    .unwrap();
    let (events, _receiver) = session_event_channel(32);
    let name = (original.name()).clone();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: name.clone(),
                args: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    metadata.interface.display_name = Some("Replacement metadata".into());
    let changed = SkillDefinition::new(
        original.name().clone(),
        metadata.description.clone(),
        "Replacement body",
        SkillSource::Programmatic("replacement".into()),
    )
    .unwrap()
    .with_metadata(metadata, None)
    .unwrap();
    engine.skills.catalog = Arc::new(SkillCatalog::new([changed]).unwrap());
    let name = (original.name()).clone();
    assert_eq!(
        engine.active_skills().unwrap().get(&name).unwrap().digest(),
        identity
    );
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::EditTranscript(
                TranscriptEdit {
                    target: TranscriptEditTarget::PromptOrdinal(0),
                    replacement: TranscriptEditReplacement::Skill {
                        name: name.clone(),
                        args: "edited".into(),
                        mode: SessionMode::Build,
                    },
                },
            )),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .active_skills()
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        vec![identity]
    );
    let persisted = zevria_transcript::transcript::load(&path).unwrap();
    assert_eq!(
        zevria_transcript::transcript::replay_active_skills(&persisted)
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        vec![identity]
    );
    assert_eq!(
        persisted
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn removed_first_use_snapshot_survives_resume_and_edit_but_a_new_name_resolves_normally() {
    use zevria_instructions::skill::*;
    let (directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let roots = FixedSkillRoots::capture(directory.path());
    let package = roots.project().join("review");
    std::fs::create_dir_all(&package).unwrap();
    let manifest = package.join("SKILL.md");
    std::fs::write(&manifest, "---\nname: review\ndescription: Original description\nmetadata:\n  short-description: Original metadata\n---\nOriginal body\n").unwrap();
    let (definition, diagnostics) = validate_skill_path(&roots, &manifest).unwrap();
    assert!(diagnostics.is_empty());
    let snapshot = definition.snapshot();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([Ok(Message::assistant("first"))]),
        test_skill_tools(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::new([definition]).unwrap()),
    )
    .unwrap();
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::InvokeSkill {
                name: snapshot.name().clone(),
                args: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let anchor = engine.conversation.prompt_position(0).unwrap();
    let retained_prefix = conversation_records(&engine.conversation.items()[..anchor]);
    drop(engine);
    std::fs::remove_file(&manifest).unwrap();
    let saved = zevria_transcript::transcript::load(&path).unwrap();
    let commit = test_skills().get("commit").unwrap().clone();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("edited")),
            Ok(Message::assistant("different skill")),
        ]),
        test_skill_tools(),
        test_policies(),
        TranscriptWriter::append_to(path.clone()).unwrap(),
        Arc::new(SkillCatalog::new([commit.clone()]).unwrap()),
    )
    .unwrap()
    .with_fixture(saved)
    .unwrap();
    let edit = |name: SkillName| {
        SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
            target: TranscriptEditTarget::PromptOrdinal(0),
            replacement: TranscriptEditReplacement::Skill {
                name,
                args: "edited".into(),
                mode: SessionMode::Build,
            },
        }))
    };
    let before = engine.conversation.items().to_vec();
    engine
        .handle_command(edit("missing".parse().unwrap()), &events)
        .await
        .unwrap();
    assert_eq!(engine.conversation.items(), before);
    engine
        .handle_command(edit(snapshot.name().clone()), &events)
        .await
        .unwrap();
    assert_eq!(
        engine.active_skills().unwrap().get(snapshot.name()),
        Some(&snapshot)
    );
    assert!(engine.conversation.items().starts_with(&retained_prefix));
    let saved = zevria_transcript::transcript::load(&path).unwrap();
    assert_eq!(saved, conversation_records(engine.conversation.items()));
    assert_eq!(
        replay_active_skills(&saved).unwrap().get(snapshot.name()),
        Some(&snapshot)
    );
    assert_eq!(
        saved
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        1
    );
    engine
        .handle_command(edit(commit.name().clone()), &events)
        .await
        .unwrap();
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    assert_eq!(
        engine.active_skills().unwrap().get(commit.name()),
        Some(&commit.snapshot())
    );
    assert!(
        engine
            .active_skills()
            .unwrap()
            .get(snapshot.name())
            .is_none()
    );
    assert_eq!(
        zevria_transcript::transcript::load(&path).unwrap(),
        conversation_records(engine.conversation.items())
    );
}

#[tokio::test]
async fn skill_reload_empty_to_nonempty_keeps_captured_plan_permission() {
    use zevria_instructions::skill::*;
    let (_dir, transcript) = test_transcript();
    let mut policies = test_policies();
    policies.policy_mut(SessionMode::Build).skills_enabled = false;
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        policies,
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_skill_management(Arc::new(TestSkillManagement), [true, false])
    .unwrap();
    assert!(engine.policies.policy(SessionMode::Build).skills_enabled);
    let tools_before = engine
        .policies
        .policy(SessionMode::Build)
        .allowed_tool_names
        .clone();
    let (events, _receiver) = session_event_channel(16);
    let request = SkillManagementRequest::Reload {
        expected_revision: engine.skills.catalog.revision().into(),
    };
    engine
        .handle_command(
            SessionCommand::Manage(crate::session::ManagementCommand::Skills {
                request_id: "reload".into(),
                request,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(engine.policies.policy(SessionMode::Build).skills_enabled);
    assert!(!engine.policies.policy(SessionMode::Plan).skills_enabled);
    assert!(
        engine
            .policies
            .policy(SessionMode::Build)
            .allows_tool(SKILL_READ_TOOL_NAME)
    );
    assert!(
        !engine
            .policies
            .policy(SessionMode::Plan)
            .allows_tool(SKILL_TOOL_NAME)
    );
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(zevria_transcript::transcript::is_leading_metadata)
    );
    assert_eq!(
        engine
            .policies
            .policy(SessionMode::Build)
            .allowed_tool_names,
        tools_before
    );
}

#[tokio::test]
async fn management_completions_obey_selected_mode_and_registered_activation_capability() {
    use zevria_instructions::skill::*;
    for (enabled, advertised, allowed) in [
        (true, true, true),
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let (_directory, transcript) = test_transcript();
        let mut policies = test_policies();
        policies.policy_mut(SessionMode::Plan).skills_enabled = enabled;
        policies.policy_mut(SessionMode::Plan).allowed_tool_names =
            Some(vec![if allowed { "skill" } else { "command" }.into()]);
        let tools = if advertised {
            test_skill_tools()
        } else {
            ToolServer::new().run()
        };
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([]),
            tools,
            policies,
            transcript,
            test_skills(),
        )
        .unwrap()
        .with_mode_management();
        let (events, mut receiver) = session_event_channel(32);
        engine
            .handle_command(
                SessionCommand::Manage(ManagementCommand::SetMode {
                    request_id: "plan".into(),
                    mode: SessionMode::Plan,
                }),
                &events,
            )
            .await
            .unwrap();
        engine
            .handle_command(
                SessionCommand::Manage(ManagementCommand::Skills {
                    request_id: "names".into(),
                    request: SkillManagementRequest::List {
                        query: String::new(),
                    },
                }),
                &events,
            )
            .await
            .unwrap();
        let events = collect_events(&mut receiver).await;
        let view = events
            .iter()
            .find_map(|event| match event {
                SessionEvent::SkillsResult {
                    result: SkillManagementResult::View { view },
                    ..
                } => Some(view),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            view.entries.len(),
            2,
            "inspection remains available without activation"
        );
        assert_eq!(
            !view.completions.is_empty(),
            enabled && advertised && allowed
        );
        let busy = engine.enter_turn().unwrap();
        assert_eq!(busy.lock().unwrap().completions(), view.completions);
        engine.publish_skill_query_context().unwrap();
        assert_eq!(busy.lock().unwrap().completions(), view.completions);
        engine.exit_turn();
    }
}

#[tokio::test]
async fn skill_management_queries_while_busy_but_mutations_are_not_queued() {
    use zevria_instructions::skill::*;
    let (_dir, transcript) = test_transcript();
    let engine = SessionEngine::new(
        PendingProvider {
            cancelled: Arc::new(AtomicBool::new(false)),
        },
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap()
    .with_skill_management(Arc::new(TestSkillManagement), [true, false])
    .unwrap();
    let revision = engine.skills.catalog.revision().to_string();
    let name = SkillName::parse("review").unwrap();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (events, mut receiver) = session_event_channel(64);
    let task = tokio::spawn(engine.run(command_rx, events));
    commands
        .send(SessionCommand::Turn(
            crate::session::TurnCommand::InvokeSkill {
                name: name.clone(),
                args: "wait".into(),
                mode: SessionMode::Build,
            },
        ))
        .unwrap();
    loop {
        if matches!(
            receiver.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::TurnStarted { .. }))
        ) {
            break;
        }
    }
    commands
        .send(SessionCommand::Manage(
            crate::session::ManagementCommand::Skills {
                request_id: "read".into(),
                request: SkillManagementRequest::List {
                    query: String::new(),
                },
            },
        ))
        .unwrap();
    commands
        .send(SessionCommand::Manage(
            crate::session::ManagementCommand::Skills {
                request_id: "write".into(),
                request: SkillManagementRequest::Reload {
                    expected_revision: revision.clone(),
                },
            },
        ))
        .unwrap();
    let mut results = 0;
    while results < 2 {
        if let Some(SessionUpdate::Lifecycle(SessionEvent::SkillsResult { request_id, result })) =
            tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
        {
            match (request_id.as_str(), result) {
                ("read", SkillManagementResult::View { view: page }) => {
                    assert_eq!(page.revision, revision);
                    assert_eq!(
                        page.counts.active, 1,
                        "queries observe newly committed activations during the turn"
                    );
                    assert!(
                        page.entries
                            .iter()
                            .any(|entry| entry.name.as_str() == "review" && entry.active)
                    );
                }
                ("write", SkillManagementResult::Error { code, .. }) => {
                    assert_eq!(code, "busy")
                }
                other => panic!("unexpected {other:?}"),
            }
            results += 1;
        }
    }
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    while let Ok(update) = receiver.try_recv() {
        assert!(!matches!(
            update,
            SessionUpdate::Lifecycle(SessionEvent::SkillsChanged { .. })
        ));
    }
}

#[tokio::test]
async fn skill_replacements_enforce_the_selected_modes_tool_policy_before_commit() {
    let old_start =
        ensemble_start_fixture("skill-policy-edit", EnsembleWorkflow::Review, "old review");
    let original = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: old_start.clone(),
        }),
        TranscriptItem::Message(Message::assistant("old tail")),
    ];
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let requests = provider.requests.clone();
    let (_directory, mut transcript) = test_transcript();
    persist_fixture(&mut transcript, &original);
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("engine")
    .with_fixture(original.clone())
    .unwrap();
    let (events, mut receiver) = session_event_channel(8);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::EditTranscript(
                TranscriptEdit {
                    target: TranscriptEditTarget::EnsembleRun(old_start.run_id),
                    replacement: TranscriptEditReplacement::Skill {
                        name: "review".parse().unwrap(),
                        args: "inspect".into(),
                        mode: SessionMode::Plan,
                    },
                },
            )),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.conversation().items(), original.clone());
    assert_eq!(
        zevria_transcript::transcript::load(&transcript_path).expect("durable transcript"),
        original
    );
    assert!(requests.lock().expect("requests").is_empty());
    assert!(matches!(
        collect_events(&mut receiver).await.last(),
        Some(SessionEvent::TurnRejected { error, .. })
            if error.contains("skill capability is unavailable in Plan mode")
    ));
}

#[tokio::test]
async fn skill_blocked_history_does_not_repair_plan_projections_on_startup() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("blocked.jsonl");
    let (_, items) = ready_plan_fixture();
    let mut original = items
        .iter()
        .map(|item| serde_json::to_string(item).expect("record"))
        .collect::<Vec<_>>()
        .join("\n");
    original.push_str("\n{\"zevria_skill_activation_v999\":{}}\n");
    std::fs::write(&path, &original).expect("blocked fixture");
    assert!(zevria_transcript::transcript::load_report(&path).is_err());
    assert!(TranscriptWriter::read_only(path.clone()).is_err());
    assert!(TranscriptWriter::append_to(path.clone()).is_err());
    let projections = directory.path().join("projections");
    assert!(!projections.exists());
    assert_eq!(
        std::fs::read_to_string(path).expect("preserved source"),
        original
    );
}

#[tokio::test]
async fn resubmitting_first_direct_invocation_replaces_its_activation_atomically() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first application")),
        Ok(Message::assistant("revised application")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(16);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::EditTranscript(
                TranscriptEdit {
                    target: TranscriptEditTarget::PromptOrdinal(0),
                    replacement: TranscriptEditReplacement::Skill {
                        name: "review".parse().unwrap(),
                        args: "revised".into(),
                        mode: SessionMode::Build,
                    },
                },
            )),
            &events,
        )
        .await
        .unwrap();

    let items = engine.conversation().items();
    assert_eq!(
        items
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        1
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, TranscriptItem::SkillInvocation(_)))
            .count(),
        1
    );
    let TranscriptItem::SkillInvocation(invocation) = &items[0] else {
        panic!("replacement invocation");
    };
    assert!(matches!(
        invocation.application(),
        SkillApplication::Activate(_)
    ));
    assert_eq!(
        invocation.arguments(),
        &zevria_content::UserPrompt::from_text("revised")
    );
    assert_eq!(engine.conversation().prompt_position(0), Some(0));
    assert_eq!(engine.active_skills().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_skill_invocations_fail_locally_without_a_model_call() {
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let tools = test_skill_tools();
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, test_skills())
            .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "missing".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(engine.history().is_empty());
    assert!(requests.lock().expect("requests lock").is_empty());
    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        events.as_slice(),
        [SessionEvent::TurnRejected { error, .. }]
            if error.contains("unknown or unavailable skill missing") && error.contains("management") && !error.contains("commit, review")
    ));
}

#[tokio::test]
async fn direct_skill_invocation_obeys_the_mode_policy() {
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let policies = SessionPolicies::new(
        TurnPolicy::new(
            "Build test instructions",
            Some(vec![SKILL_TOOL_NAME.to_string()]),
            ModelRole::Build,
            true,
        ),
        TurnPolicy::new(
            "Plan test instructions",
            Some(vec!["command".to_string()]),
            ModelRole::Plan,
            false,
        ),
    );
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        policies,
        transcript,
        test_skills(),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(8);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    assert!(engine.history().is_empty());
    assert!(requests.lock().expect("requests").is_empty());
    assert!(matches!(
        collect_events(&mut receiver).await.as_slice(),
        [SessionEvent::TurnRejected { error, .. }]
            if error.contains("skill capability is unavailable in Plan mode")
    ));
}

#[tokio::test]
async fn repeat_skill_applications_use_the_same_path_without_replacing_pins() {
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: dispatched.clone(),
        })
        .run();
    let skill_call =
        |id: &str, name: &str| named_tool_call(id, SKILL_TOOL_NAME, json!({"skill": name}));
    let provider = ScriptedProvider::new([
        // One batch asks for `commit` twice: the first call dispatches,
        // the batch-local guard answers the duplicate.
        Ok(Message::Assistant {
            id: Some("load-twice".to_string()),
            content: vec![
                skill_call("call-1", "commit"),
                skill_call("call-2", "commit"),
            ],
        }),
        // The next batch reapplies `commit`: typed active state answers it.
        Ok(Message::Assistant {
            id: Some("load-again".to_string()),
            content: vec![named_tool_call(
                "call-3",
                SKILL_TOOL_NAME,
                json!({"skill": "commit", "args": "later application"}),
            )],
        }),
        Ok(Message::assistant("done")),
        // A direct `$review` invocation already activated the snapshot,
        // so the tool path is guarded symmetrically.
        Ok(Message::Assistant {
            id: Some("load-review".to_string()),
            content: vec![skill_call("call-4", "review")],
        }),
        Ok(Message::assistant("done again")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, test_skills())
            .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    for command in [
        SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "use the commit skill".into(),
            mode: SessionMode::Build,
        }),
        SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
            name: "review".parse().unwrap(),
            args: zevria_content::UserPrompt::default(),
            mode: SessionMode::Build,
        }),
    ] {
        engine.handle_command(command, &events).await.unwrap();
    }

    // Every request uses engine-owned admission without a tool dispatch round trip.
    assert!(
        dispatched.lock().unwrap().is_empty(),
        "engine must not raw-dispatch skill activation"
    );

    let events = collect_events(&mut receiver).await;
    let results: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => {
                assert!(metadata.iter().all(|entry| entry.outcome.is_success()));
                Some(serde_json::to_string(message).expect("result json"))
            }
            _ => None,
        })
        .collect();
    let serialized = results.join("\n");
    assert_eq!(serialized.matches("Commit instructions").count(), 0);
    assert_eq!(serialized.matches("status: already_active").count(), 3);
    assert!(serialized.contains("later application"));
    assert!(!serialized.contains("Review instructions"));
}

#[tokio::test]
async fn tool_and_direct_paths_share_one_activation_per_name() {
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let skills = test_skills();
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: dispatched.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("activate-commit".to_string()),
            content: vec![named_tool_call(
                "commit-tool",
                SKILL_TOOL_NAME,
                json!({"skill": "commit", "args": "first tool application"}),
            )],
        }),
        Ok(Message::assistant("tool activation complete")),
        Ok(Message::assistant("direct commit complete")),
        Ok(Message::assistant("first direct review complete")),
        Ok(Message::assistant("second direct review complete")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, skills).expect("engine");
    let (events, _receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "activate commit through the tool".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    for (name, args) in [
        ("commit", "second application"),
        ("review", "first review"),
        ("review", "second review"),
    ] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                    name: name.parse().unwrap(),
                    args: args.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }

    assert!(
        dispatched.lock().unwrap().is_empty(),
        "engine must not raw-dispatch skill activation"
    );
    assert_eq!(engine.active_skills().unwrap().len(), 2);
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        2
    );
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::SkillInvocation(_)))
            .count(),
        3
    );

    let requests = requests.lock().expect("requests");
    assert!(requests[0].skill_context.is_none());
    for request in &requests[1..=2] {
        let context = request.skill_context.as_deref().expect("commit overlay");
        assert_eq!(context.matches("Commit instructions").count(), 1);
        assert!(!context.contains("Review instructions"));
    }
    for request in &requests[3..=4] {
        let context = request.skill_context.as_deref().expect("two-skill overlay");
        assert_eq!(context.matches("Commit instructions").count(), 1);
        assert_eq!(context.matches("Review instructions").count(), 1);
    }
}

#[tokio::test]
async fn engine_admits_valid_names_without_dispatch_and_rejects_malformed_arguments() {
    let skills = test_skills();
    let tools = ToolServer::new().tool(MissingRequestSkillTool).run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("missing-request".to_string()),
            content: vec![
                named_tool_call(
                    "missing-request-call",
                    SKILL_TOOL_NAME,
                    json!({"skill": "commit"}),
                ),
                named_tool_call(
                    "malformed-skill-call",
                    SKILL_TOOL_NAME,
                    json!({"skill": "review", "unexpected": true}),
                ),
            ],
        }),
        Ok(Message::assistant("recovered")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, skills).expect("engine");
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "try invalid activation".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    assert!(
        engine
            .active_skills()
            .unwrap()
            .contains(&"commit".parse().unwrap())
    );
    let serialized = collect_events(&mut receiver)
        .await
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => {
                assert_eq!(metadata[0].outcome, ToolCallOutcome::Success);
                assert_eq!(metadata[1].outcome, ToolCallOutcome::Error);
                Some(serde_json::to_string(message).expect("message"))
            }
            _ => None,
        })
        .collect::<String>();
    assert!(serialized.contains("unknown field"));
}

#[tokio::test]
async fn resumed_skill_pin_is_reapplied_by_stateless_dispatch() {
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: dispatched.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("load-resumed".to_string()),
            content: vec![named_tool_call(
                "call-1",
                SKILL_TOOL_NAME,
                json!({"skill": "commit"}),
            )],
        }),
        Ok(Message::assistant("done")),
    ]);
    let (_directory, transcript) = test_transcript();
    let skills = test_skills();
    let prior = vec![
        TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship it",
            SkillApplication::Activate(skills.get("commit").expect("commit").snapshot()),
        )),
        TranscriptItem::Message(Message::assistant("committed")),
    ];
    let mut engine = SessionEngine::new(provider, tools, test_policies(), transcript, skills)
        .expect("valid engine")
        .with_fixture(prior)
        .unwrap();
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "load the commit skill".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    // Dispatch submits a request; the engine reapplies the restored pin.
    assert!(
        dispatched.lock().unwrap().is_empty(),
        "engine must not raw-dispatch skill activation"
    );
    let serialized = collect_events(&mut receiver)
        .await
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResults { message, .. } => {
                Some(serde_json::to_string(message).expect("result json"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(serialized.matches("status: already_active").count(), 1);
    assert!(!serialized.contains("Commit instructions"));
}

#[tokio::test]
async fn oversized_prospective_activation_fails_before_state_mutation() {
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(8, 90, 0));
    let (events, mut receiver) = session_event_channel(8);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "commit".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(engine.active_skills().unwrap().is_empty());
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| { !matches!(item, TranscriptItem::SkillInvocation(_)) })
    );
    assert!(requests.lock().expect("requests").is_empty());
    assert!(matches!(
        collect_events(&mut receiver).await.as_slice(),
        [SessionEvent::TurnRejected { error, .. }]
            if error.contains("irreducible instruction-state tokens")
    ));
}

#[tokio::test]
async fn oversized_tool_activation_fails_without_mutating_typed_state() {
    let skills = Arc::new(
        SkillCatalog::new([zevria_instructions::skill::SkillDefinition::new(
            SkillName::parse("commit").expect("name"),
            "Large commit instructions",
            "x".repeat(3_000),
            zevria_instructions::skill::SkillSource::Programmatic("test".to_string()),
        )
        .expect("definition")])
        .expect("registry"),
    );
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("oversized-tool".to_string()),
            content: vec![named_tool_call(
                "oversized-tool-call",
                SKILL_TOOL_NAME,
                json!({"skill": "commit"}),
            )],
        }),
        Ok(Message::assistant("recovered")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies_for_tools(&["skill"]),
        transcript,
        skills,
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(1_200, 100, 0));
    // Leave room for metadata disclosure and ordinary history, but not the
    // authoritative body. Exact continuation counts cannot authorize that pin.
    engine.provider.input_counts = [
        Ok(InputTokenCount::Exact(100)),
        Ok(InputTokenCount::Exact(100)),
    ]
    .into();
    let (events, mut receiver) = session_event_channel(16);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "try a large tool activation".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(engine.active_skills().unwrap().is_empty());
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .flat_map(recorded_skill_applications)
            .next()
            .is_none()
    );
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolResults { metadata, .. }
            if metadata[0].outcome == ToolCallOutcome::Error
    )));
}

#[tokio::test]
async fn submit_edit_replaces_a_skill_turn_with_the_plain_revision() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("skill answer")),
        Ok(Message::assistant("revised answer")),
    ]);
    let requests = provider.requests.clone();
    let tools = test_skill_tools();
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, test_skills())
            .expect("valid engine");
    let (events, _receiver) = session_event_channel(1024);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: "the diff".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    // A `$name` row is a prompt row like any other, so its ordinal
    // resolves without exposing its embedded application snapshot.
    engine
        .handle_command(
            prompt_message_edit(0, "review it again", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(
        engine.history(),
        vec![
            Message::user("review it again"),
            Message::assistant("revised answer")
        ]
    );
    {
        let requests = requests.lock().expect("requests lock");
        assert!(requests[1].history.is_empty());
    }
    let loaded = zevria_transcript::transcript::load(engine.conversation().path())
        .expect("transcript loads");
    assert_eq!(
        conversation_records(&loaded),
        vec![
            TranscriptItem::Message(Message::user("review it again")),
            TranscriptItem::Message(Message::assistant("revised answer")),
        ]
    );
}
