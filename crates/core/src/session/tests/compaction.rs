use super::*;

fn test_policies() -> SessionPolicies {
    super::test_policies_for_tools(&["echo", "skill"])
}

#[cfg(unix)]
#[tokio::test]
async fn guidance_checkpoint_and_maintenance_preserve_both_components_and_cleared_values() {
    let (directory, writer) = test_transcript();
    let global = directory.path().join("global");
    let project = directory.path().join("project");
    std::fs::create_dir(&global).unwrap();
    std::fs::create_dir(&project).unwrap();
    std::fs::write(global.join("AGENTS.md"), "GLOBAL_CHECKPOINT").unwrap();
    std::fs::write(project.join("AGENTS.md"), "PROJECT_CHECKPOINT").unwrap();
    let roots = zevria_instructions::GuidanceRoots::fixture(Some(&global), &project);
    let mut engine = SessionEngine::new(
        ScriptedProvider::new((0..4).map(|_| Ok(Message::assistant("summary")))),
        ToolServer::new().run(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap()
    .with_guidance_roots(roots.clone());
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let path = engine.conversation.path().to_path_buf();
    for cleared in [false, true] {
        if cleared {
            drop(engine);
            std::fs::remove_file(global.join("AGENTS.md")).unwrap();
            engine = SessionEngine::new(
                ScriptedProvider::new([Ok(Message::assistant("summary"))]),
                ToolServer::new().run(),
                test_policies(),
                TranscriptWriter::append_to(path.clone()).unwrap(),
                test_skills(),
            )
            .unwrap()
            .with_guidance_roots(roots.clone());
            assert!(engine.refresh_application_guidance().unwrap().is_empty());
        }
        let snapshot = engine.directive_state().unwrap().snapshot();
        let before_bytes = std::fs::read(&path).unwrap();
        let before_items = engine.conversation.items().to_vec();
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Compact {
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(std::fs::read(&path).unwrap().starts_with(&before_bytes));
        assert!(engine.conversation.items().starts_with(&before_items));
        let checkpoint = engine.conversation.latest_compaction().unwrap().1;
        assert!(
            serde_json::to_value(checkpoint)
                .unwrap()
                .get("instruction_snapshot")
                .is_none()
        );
        assert_eq!(engine.directive_state().unwrap().snapshot(), snapshot);
        let restored = zevria_transcript::transcript::load(&path).unwrap();
        SessionReplayError::validate(&restored).unwrap();
        assert_eq!(
            zevria_transcript::replay_directives(&restored)
                .unwrap()
                .snapshot(),
            zevria_instructions::DirectiveSnapshot::default()
        );
        let requests = engine.provider.requests.lock().unwrap();
        let maintenance = requests.last().unwrap();
        assert!(
            !maintenance
                .input
                .iter()
                .any(|item| matches!(item, OwnedModelRequestItem::DeveloperInstruction(_)))
        );
        assert_eq!(
            maintenance.instructions.contains("GLOBAL_CHECKPOINT"),
            !cleared
        );
        assert!(maintenance.instructions.contains("PROJECT_CHECKPOINT"));
        assert!(
            maintenance
                .instructions
                .contains("## Workflow policy: maintenance")
        );
        assert!(
            maintenance
                .instructions
                .ends_with("## Eligible skills\nSkill selection is unavailable.")
        );
        assert_eq!(engine.conversation.items()[0], before_items[0]);
    }
}

#[tokio::test]
async fn ensemble_reports_run_normal_pre_synthesis_compaction() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("bounded ensemble summary")),
        Ok(Message::assistant("review after compaction")),
    ]);
    let requests = provider.requests.clone();
    let launcher = Arc::new(StubEnsembleLauncher::successful([
        "large worker evidence ".repeat(1_000)
    ]));
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    // Leave room for the production inspection/scratch overlay while keeping
    // worker evidence above the compaction trigger and the summary below it.
    .with_compaction_policy(test_compaction_policy(10_000, 30, 0))
    .with_ensemble_launcher(launcher);
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::RunEnsemble {
                workflow: EnsembleWorkflow::Review,
                prompt: "review the large evidence".into(),
            }),
            &events,
        )
        .await
        .unwrap();

    let emitted = collect_events(&mut receiver).await;
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2, "terminal event: {:?}", emitted.last());
    assert_eq!(
        requests[0].prompt,
        Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
    );
    assert!(format!("{:?}", requests[0].history).contains("large worker evidence"));
    assert_eq!(requests[1].model_role, ModelRole::Review);
    assert_eq!(
        requests[1].prompt,
        Message::user(format!(
            "{}\nbounded ensemble summary",
            zevria_model::SUMMARY_PREFIX
        ))
    );
    let items = engine.conversation().items();
    let reports = items
        .iter()
        .position(|item| {
            matches!(
                item,
                TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
            )
        })
        .expect("reports boundary");
    let checkpoint = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("pre-synthesis checkpoint");
    assert!(reports < checkpoint);
}

#[tokio::test]
async fn skill_invocations_expand_history_but_display_the_compact_form() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("committed"))]);
    let requests = provider.requests.clone();
    let tools = test_skill_tools();
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, test_skills())
            .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "commit".parse().unwrap(),
                args: "  ship it  ".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let application = "Apply the active skill \"commit\" to this request:\n\nship it";
    // Transcript/model history carries only the bodyless application; the
    // body is materialized once in the separate request overlay.
    assert_eq!(engine.history()[0], Message::user(application));
    {
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests[0].prompt, Message::user(application));
        assert_eq!(requests[0].model_role, ModelRole::Build);
        let context = requests[0].skill_context.as_deref().expect("skill overlay");
        assert_eq!(context.matches("Commit instructions").count(), 1);
    }
    // The live event shows the compact `$name` form.
    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        &events[0],
        SessionEvent::TurnStarted { message, mode: SessionMode::Build, .. }
            if message == &Message::user("$commit ship it")
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    // First use owns its hidden pin inside the bodyless visible invocation.
    drop(engine);
    let recorded = zevria_transcript::transcript::load(&path).expect("transcript should load");
    assert!(matches!(
        &recorded[0],
        TranscriptItem::SkillInvocation(invocation)
            if invocation.name().as_str() == "commit"
                && invocation.arguments() == &zevria_content::UserPrompt::from_text("ship it")
                && invocation.model_message() == &Message::user(application)
    ));
}

#[test]
fn unsupported_checkpoint_is_rejected_without_normalization_or_model_calls() {
    let provider = ScriptedProvider::new([]);
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    drop(transcript);
    let original = b"{\"zevria_compaction\":{\"version\":7}}\n";
    std::fs::write(&path, original).unwrap();
    assert!(zevria_transcript::transcript::load(&path).is_err());
    assert!(TranscriptWriter::append_to(path.clone()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!path.with_extension("jsonl.pre-v3").exists());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn compaction_omits_skill_overlay_and_next_request_restores_it_once() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("skill turn complete")),
        Ok(Message::assistant("normalization summary")),
        Ok(Message::assistant("post-compaction complete")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: "inspect".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "continue after compaction".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0]
            .skill_context
            .as_deref()
            .expect("first overlay")
            .matches("Review instructions")
            .count(),
        1
    );
    assert_eq!(
        requests[1].prompt,
        Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
    );
    assert!(requests[1].skill_context.is_none());
    assert_eq!(
        requests[2]
            .skill_context
            .as_deref()
            .expect("restored overlay")
            .matches("Review instructions")
            .count(),
        1
    );
    let checkpoint = engine
        .conversation()
        .latest_compaction()
        .expect("checkpoint")
        .1;
    checkpoint.validate().unwrap();
    assert_eq!(
        replay_active_skills(engine.conversation().items()).unwrap(),
        *engine.active_skills().unwrap()
    );
    assert!(
        !serde_json::to_string(&engine.history())
            .expect("history")
            .contains("Review instructions")
    );
}

#[tokio::test]
async fn skill_overhead_uses_the_authoritative_snapshot_for_compaction() {
    let large_body = "x".repeat(1_000);
    let skills = Arc::new(
        SkillCatalog::new([zevria_instructions::skill::SkillDefinition::new(
            SkillName::parse("large").expect("name"),
            "Large instructions",
            large_body,
            zevria_instructions::skill::SkillSource::Programmatic("test".to_string()),
        )
        .expect("definition")])
        .expect("registry"),
    );
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("activated")),
        Ok(Message::assistant("continued without compaction")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        skills,
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(2_000, 10, 0));
    let (events, mut receiver) = session_event_channel(16);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "large".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Compaction(_)))
    );
    assert_eq!(
        collect_events(&mut receiver)
            .await
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn edited_submission_atomically_compacts_its_retained_prefix_before_the_revision() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("second answer")),
        Ok(Message::assistant("prefix summary")),
        Ok(Message::assistant("revised answer")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(64);

    for text in ["first", "second"] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }
    // Enable a tiny window only for the edit so the earlier ordinary
    // submissions establish the tail that the revision must remove.
    engine.compaction = test_compaction_policy(1_000, 1, 0);
    engine.reestimate_context_usage();

    engine
        .handle_command(
            prompt_message_edit(1, "second revised", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    let items = engine.conversation().items();
    let checkpoint = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("retained-prefix checkpoint");
    assert_eq!(
        engine.conversation().prompt_position(1),
        Some(checkpoint + 1)
    );
    assert_eq!(
        engine.history(),
        vec![
            Message::user(format!("{}\nprefix summary", zevria_model::SUMMARY_PREFIX)),
            Message::user("second revised"),
            Message::assistant("revised answer"),
        ]
    );
    {
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[2].prompt,
            Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
        );
        assert_eq!(
            requests[2].history,
            vec![Message::user("first"), Message::assistant("first answer")]
        );
        assert_eq!(
            requests[3].history,
            vec![Message::user(format!(
                "{}\nprefix summary",
                zevria_model::SUMMARY_PREFIX
            ))]
        );
    }

    let events = collect_events(&mut receiver).await;
    let compacted = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::CompactionCompleted {
                    trigger: CompactionTrigger::AutomaticPreTurn,
                    ..
                }
            )
        })
        .expect("edit compaction completion");
    let revised = events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::TurnStarted { .. }))
        .expect("edited turn start");
    assert!(compacted < revised);
}

#[tokio::test]
async fn edited_skill_atomically_compacts_before_its_compact_turn_started_display() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("second answer")),
        Ok(Message::assistant("prefix summary")),
        Ok(Message::assistant("skill answer")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(64);

    for text in ["first", "second"] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }
    // Keep the complete catalog/body irreducible state admissible, while the
    // one-percent trigger still compacts the retained conversation prefix.
    engine.compaction = test_compaction_policy(4_000, 1, 0);
    engine.reestimate_context_usage();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::EditTranscript(
                TranscriptEdit {
                    target: TranscriptEditTarget::PromptOrdinal(1),
                    replacement: TranscriptEditReplacement::Skill {
                        name: "review".parse().unwrap(),
                        args: "  second revised  ".into(),
                        mode: SessionMode::Build,
                    },
                },
            )),
            &events,
        )
        .await
        .unwrap();

    let items = engine.conversation().items();
    let checkpoint = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("retained-prefix checkpoint");
    let TranscriptItem::SkillInvocation(invocation) = &items[checkpoint + 1] else {
        panic!("bodyless skill invocation");
    };
    assert_eq!(invocation.name().as_str(), "review");
    assert_eq!(
        invocation.arguments(),
        &zevria_content::UserPrompt::from_text("second revised")
    );
    assert_eq!(
        engine.conversation().prompt_position(1),
        Some(checkpoint + 1)
    );
    let expanded = invocation.model_message().clone();
    assert_eq!(
        engine.history(),
        vec![
            Message::user(format!("{}\nprefix summary", zevria_model::SUMMARY_PREFIX)),
            expanded.clone(),
            Message::assistant("skill answer"),
        ]
    );
    {
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3].prompt, expanded);
        assert_eq!(
            requests[3].history,
            vec![Message::user(format!(
                "{}\nprefix summary",
                zevria_model::SUMMARY_PREFIX
            ))]
        );
    }

    let events = collect_events(&mut receiver).await;
    let compacted = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::CompactionCompleted {
                    trigger: CompactionTrigger::AutomaticPreTurn,
                    ..
                }
            )
        })
        .expect("edit compaction completion");
    let started = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::TurnStarted { message, .. }
                    if message == &Message::user("$review second revised")
            )
        })
        .expect("compact skill display");
    assert!(compacted < started);
}

#[tokio::test]
async fn edited_overflow_reports_a_prepared_but_uninstalled_checkpoint() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("second answer")),
        Ok(Message::assistant("prefix summary")),
    ])
    .with_input_counts([
        Ok(InputTokenCount::Exact(1_200)),
        Ok(InputTokenCount::Exact(1_100)),
    ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(64);

    for text in ["first ".repeat(400), "second".to_string()] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }
    let original = engine.conversation().items().to_vec();
    engine.compaction = test_compaction_policy(1_000, 50, 0);
    engine.reestimate_context_usage();

    engine
        .handle_command(
            prompt_message_edit(1, "second revised", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.lock().expect("requests").len(), 3);
    assert_eq!(engine.conversation().items(), original.as_slice());
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::CompactionStarted {
            trigger: CompactionTrigger::AutomaticPreTurn,
            ..
        }
    )));
    assert!(events.iter().all(|event| !matches!(
        event,
        SessionEvent::CompactionCompleted {
            trigger: CompactionTrigger::AutomaticPreTurn,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnRejected { error, .. }
            if error.contains("prepared for the edited prefix")
                && error.contains("checkpoint was not installed")
                && !error.contains("automatic compaction completed")
    )));
}

#[tokio::test]
async fn manual_local_compaction_installs_checkpoint_and_replaces_next_request_history() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("handoff summary")),
        Ok(Message::assistant("second answer")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    {
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.model_role == ModelRole::Build)
        );
        assert_eq!(
            requests[1].prompt,
            Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
        );
        assert_eq!(requests[1].allowed_tool_names, Some(Vec::new()));
        assert_eq!(
            requests[2].history,
            vec![Message::user(format!(
                "{}\nhandoff summary",
                zevria_model::SUMMARY_PREFIX
            ))]
        );
        assert_eq!(requests[2].prompt, Message::user("second question"));
    }

    assert!(matches!(
        engine.conversation().items()[2],
        TranscriptItem::Compaction(_)
    ));
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::CompactionStarted {
            trigger: CompactionTrigger::Manual,
            ..
        }
    )));
    let completed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::CompactionCompleted {
                    trigger: CompactionTrigger::Manual,
                    backend: CompactionBackend::LocalSummary,
                    ..
                }
            )
        })
        .expect("manual compaction completion");
    let calls = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ModelCallStarted { turn_id, call } => Some((*turn_id, *call)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // The manual summary consumes an engine ID and a provider request, but no loop call.
    assert_eq!(calls, [(TurnId::new(1), 1), (TurnId::new(3), 1)]);
    assert!(matches!(
        events.get(completed.saturating_sub(1)),
        Some(SessionEvent::ContextUsageUpdated { .. })
    ));
}

#[tokio::test]
async fn manual_compaction_uses_the_selected_mode_model_role() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant("plan-mode summary")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model_role, ModelRole::Build);
    assert_eq!(requests[1].model_role, ModelRole::Plan);
    assert_eq!(requests[1].allowed_tool_names, Some(Vec::new()));
}

#[tokio::test]
async fn compaction_reinjects_the_latest_successful_task_snapshot() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("handoff summary")),
        Ok(Message::assistant("continued")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(32);

    let arguments = json!({
        "explanation": "Implementation is underway.",
        "tasks": [
            {"step": "Preserve this exact step", "status": "in_progress"},
            {"step": "Run tests", "status": "pending"}
        ]
    });
    let snapshot = TaskList::from_tool_arguments(&arguments).expect("valid task snapshot");
    engine
        .record_required(TranscriptItem::Message(Message::user("first question")))
        .expect("prompt persists");
    engine
        .record_completed(TranscriptItem::Message(Message::Assistant {
            id: None,
            content: vec![named_tool_call(
                "task-one",
                zevria_foundation::TASK_TOOL_NAME,
                arguments,
            )],
        }))
        .expect("task call persists");
    engine
        .record_completed(TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    "task-one",
                    zevria_foundation::TASK_TOOL_NAME,
                    vec![ToolResultContent::text(snapshot.update_summary())],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "task-one".to_string(),
                call_id: None,
                tool_name: zevria_foundation::TASK_TOOL_NAME.to_string(),
                outcome: ToolCallOutcome::Success,
                detail: None,
            }],
        })
        .expect("task result persists");

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history,
        vec![
            Message::user(format!("{}\nhandoff summary", zevria_model::SUMMARY_PREFIX)),
            Message::user(snapshot.checkpoint_context()),
        ]
    );
    assert_eq!(requests[1].prompt, Message::user("second question"));
}

#[tokio::test]
async fn manual_compaction_without_a_prompt_makes_no_model_request_or_checkpoint() {
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(16);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(requests.lock().expect("requests").is_empty());
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(zevria_transcript::transcript::is_leading_metadata)
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::TurnRejected { error, .. }
                    if error.contains("before its first user prompt")
            ))
    );
}

#[tokio::test]
async fn successful_checkpoint_is_not_automatically_recompacted_before_new_work() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Ok(Message::assistant(
            "summary much larger than the tiny window",
        )),
        Ok(Message::assistant("second answer")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(10_000, 90, 0));
    let (events, _receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(requests.lock().expect("requests").len(), 3);
}

#[tokio::test]
async fn failed_local_compaction_installs_nothing_and_keeps_existing_history() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("first answer")),
        Err(anyhow::anyhow!("summary failed")),
    ]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "first".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let before = engine.history();

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.history(), before);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnFailed { error, .. } if error.contains("summary failed")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
    );
}

#[test]
fn role_compaction_policy_exposes_independent_profile_limits() {
    let policy = role_compaction_policy(
        80,
        ("build-model", 1_000, 100),
        ("plan-model", 500, 40),
        ("review-model", 250, 20),
        ("explore-model", 125, 10),
        ("builder-model", 1500, 50),
    );
    for (role, model, window, retained, trigger) in [
        (ModelRole::Build, "build-model", 1_000, 100, 800),
        (ModelRole::Plan, "plan-model", 500, 40, 400),
        (ModelRole::Review, "review-model", 250, 20, 200),
        (ModelRole::Explore, "explore-model", 125, 10, 100),
        (ModelRole::Builder, "builder-model", 1500, 50, 1200),
    ] {
        let context = policy.for_role(role);
        assert_eq!(context.profile.model, model);
        assert_eq!(context.context_window_tokens, window);
        assert_eq!(context.retained_user_tokens, retained);
        assert_eq!(policy.trigger_tokens(role), trigger);
    }
}

#[tokio::test]
async fn irreducible_small_plan_profile_rejects_without_compaction_or_prompt_commit() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("short summary"))]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![Message::user("x".repeat(800))])
    .unwrap()
    .with_compaction_policy(role_compaction_policy(
        90,
        ("build-large", 2_000, 0),
        ("plan-small", 100, 0),
        ("review", 2_000, 0),
        ("explore", 2_000, 0),
        ("builder", 3_000, 0),
    ));
    let (events, mut receiver) = session_event_channel(16);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "new Plan prompt".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(!engine.conversation().items().iter().any(|item| matches!(
        item,
        TranscriptItem::Compaction(_) | TranscriptItem::Directive(_)
    )));
    assert!(engine.conversation().items().iter().all(|item| {
        item.message()
            .is_none_or(|message| message != &Message::user("new Plan prompt"))
    }));
    assert!(requests.lock().expect("requests").is_empty());
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnRejected { error, .. }
            if error.contains("prepared plan request")
                && error.contains("plan-small")
                && error.contains("irreducible")
    )));
}

#[tokio::test]
async fn prospective_overflow_compacts_then_rejects_without_committing_prompt_if_still_too_large() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("short summary"))]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![Message::user("old"), Message::assistant("old answer")])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000, 90, 0));
    let oversized = "z".repeat(8_000);
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: oversized.clone().into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    {
        let captured = requests.lock().expect("requests");
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].prompt,
            Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
        );
    }
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Compaction(_)))
    );
    assert!(engine.conversation().items().iter().all(|item| {
        item.message()
            .is_none_or(|message| message != &Message::user(oversized.clone()))
    }));
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnRejected { error, .. }
            if error.contains("is estimated at")
                && error.contains("prompt was not committed")
    )));
}

#[tokio::test]
async fn automatic_compaction_triggers_at_the_exact_threshold_before_next_prompt() {
    let usage = TokenUsage {
        total_tokens: 1000,
        ..TokenUsage::default()
    };
    let provider = ScriptedProvider::with_model_responses([
        Ok(model_response(Message::assistant("first answer")).with_usage(Some(usage))),
        Ok(model_response(Message::assistant("summary"))),
        Ok(model_response(Message::assistant("second answer"))),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let compaction = test_compaction_policy(2_000, 50, 20);
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(compaction);
    let (events, _receiver) = session_event_channel(64);

    for text in ["first question", "second question"] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }

    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].prompt,
        Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
    );
    drop(requests);
    let checkpoint_index = engine
        .conversation()
        .items()
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("automatic checkpoint");
    let second_prompt_index = engine
        .conversation()
        .prompt_position(1)
        .expect("second prompt");
    assert!(checkpoint_index < second_prompt_index);
}

#[tokio::test]
async fn automatic_compaction_does_not_trigger_one_token_below_threshold() {
    let usage = TokenUsage {
        // The semantic estimate for the next `second` prompt is two
        // tokens, leaving the fully prepared request one below 1000.
        total_tokens: 997,
        ..TokenUsage::default()
    };
    let provider = ScriptedProvider::with_model_responses([
        Ok(model_response(Message::assistant("first answer")).with_usage(Some(usage))),
        Ok(model_response(Message::assistant("second answer"))),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(2_000, 50, 20));
    let (events, _receiver) = session_event_channel(32);

    for text in ["first", "second"] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }

    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
}

#[tokio::test]
async fn exact_count_compacts_once_rechecks_and_dispatches_when_it_fits() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("s".repeat(1_600))),
        Ok(Message::assistant("fits after compaction")),
    ])
    .with_input_counts([
        Ok(InputTokenCount::Exact(600)),
        Ok(InputTokenCount::Exact(400)),
    ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "p".repeat(2_100).into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.lock().expect("requests").len(), 2);
    let events = collect_events(&mut receiver).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ContextUsageUpdated { snapshot, .. }
            if snapshot.source == ContextTokenSource::Exact
                && snapshot.projected_input_tokens == 400
    )));
    assert!(matches!(
        events.last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[tokio::test]
async fn exact_over_limit_after_one_compaction_rejects_truthfully_without_looping() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("s".repeat(1_600)))])
        .with_input_counts([
            Ok(InputTokenCount::Exact(1_200)),
            Ok(InputTokenCount::Exact(1_100)),
        ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![
        Message::user("earlier prompt"),
        Message::assistant("earlier response"),
    ])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "p".repeat(2_100).into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.lock().expect("requests").len(), 1);
    let events = collect_events(&mut receiver).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnRejected { error, .. }
            if error.contains("requires 1100 input tokens")
                && error.contains("automatic compaction completed")
    )));
}

#[tokio::test]
async fn repeated_oversized_prompt_installs_only_one_checkpoint() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("summary"))]).with_input_counts([
        Ok(InputTokenCount::Exact(1_200)),
        Ok(InputTokenCount::Exact(1_100)),
        Ok(InputTokenCount::Exact(1_100)),
    ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![
        Message::user("earlier prompt"),
        Message::assistant("earlier response"),
    ])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    let (events, mut receiver) = session_event_channel(64);

    for _ in 0..2 {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "p".repeat(2_100).into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
    }

    assert_eq!(count_calls.load(Ordering::SeqCst), 3);
    assert_eq!(requests.lock().expect("requests").len(), 1);
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
            .count(),
        1
    );
    let events = collect_events(&mut receiver).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        1
    );
    let failures = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnRejected { error, .. } => Some(error.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 2);
    assert!(failures[0].contains("automatic compaction completed"));
    assert!(failures[1].contains("already-compacted request"));
}

#[tokio::test]
async fn plan_handoff_pre_turn_compaction_remeasures_and_enforces_the_hard_limit() {
    for (exact, fits) in [(false, true), (false, false), (true, true), (true, false)] {
        let (mut artifact, _) = ready_plan_fixture();
        artifact.markdown.push_str(&" approved detail".repeat(300));
        let handoff = PlanHandoff::new(artifact, "source-session");
        // Exact counts deliberately disagree with the rebuilt estimate:
        // a large summary may fit, and a short one may still be too large.
        let summary = if exact == fits {
            "s".repeat(4_400)
        } else {
            "short summary".to_string()
        };
        let mut responses = vec![Ok(Message::assistant(summary.clone()))];
        if fits {
            responses.push(Ok(Message::assistant("implemented")));
        }
        let mut provider = ScriptedProvider::new(responses);
        if exact {
            provider = provider.with_input_counts([
                Ok(InputTokenCount::Exact(1_200)),
                Ok(InputTokenCount::Exact(if fits { 600 } else { 1_100 })),
            ]);
        }
        let count_calls = provider.input_count_calls.clone();
        let requests = provider.requests.clone();
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .expect("engine")
        .with_compaction_policy(test_compaction_policy(1_000, 50, 200));
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::StartFromPlan {
                    handoff: handoff.clone(),
                }),
                &events,
            )
            .await
            .unwrap();

        assert_eq!(
            count_calls.load(Ordering::SeqCst),
            2,
            "recount the rebuilt dispatch"
        );
        let loaded =
            zevria_transcript::transcript::load(engine.conversation().path()).expect("reload");
        assert_eq!(loaded, conversation_records(engine.conversation().items()));
        assert!(
            loaded
                .iter()
                .all(|item| !zevria_transcript::transcript::is_prompt_item(item))
        );
        assert!(
            matches!(&loaded[0], TranscriptItem::Plan(PlanRecord::Handoff { handoff: durable }) if durable == &handoff)
        );
        let TranscriptItem::Compaction(checkpoint) = &loaded[1] else {
            panic!("handoff-only history must install a pre-turn checkpoint");
        };
        assert_eq!(checkpoint.trigger, CompactionTrigger::AutomaticPreTurn);
        assert_eq!(checkpoint.retained_user_messages.len(), 1);
        assert!(checkpoint.retained_user_messages[0].contains("tokens truncated"));
        assert_eq!(
            loaded
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
                .count(),
            1
        );
        let emitted = collect_events(&mut receiver).await;
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
                .count(),
            1
        );
        let rebuilt = emitted
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::ContextUsageUpdated { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .expect("rebuilt measurement");
        assert_eq!(
            rebuilt.source,
            if exact {
                ContextTokenSource::Exact
            } else {
                ContextTokenSource::ConservativeEstimate
            }
        );
        assert_eq!(
            rebuilt.projected_input_tokens <= rebuilt.input_token_limit,
            fits
        );
        if exact {
            assert_eq!(
                rebuilt.projected_input_tokens,
                if fits { 600 } else { 1_100 }
            );
            // Even above the trigger, one rebuilt dispatch cannot compact twice.
            assert!(rebuilt.projected_input_tokens >= rebuilt.automatic_trigger);
        }
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), if fits { 2 } else { 1 });
        assert_eq!(requests[0].history, vec![handoff.prompt.clone()]);
        assert_eq!(
            requests[0].prompt,
            Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
        );
        assert_eq!(requests[0].allowed_tool_names, Some(Vec::new()));
        if fits {
            assert!(matches!(
                emitted.last(),
                Some(SessionEvent::TurnCompleted { .. })
            ));
            assert!(requests[1].history.is_empty());
            assert_eq!(
                requests[1].prompt,
                Message::user(format!("{}\n{summary}", zevria_model::SUMMARY_PREFIX))
            );
        } else {
            assert!(
                matches!(emitted.last(), Some(SessionEvent::TurnFailed { error, .. })
                    if error.contains("automatic compaction completed")
                        && error.contains("1000-token input limit")
                        && !error.contains("unavailable")
                )
            );
        }
    }
}

#[tokio::test]
async fn resumed_plan_handoff_history_compacts_manually_or_before_a_new_prompt() {
    for (manual, tool_history) in [(true, false), (true, true), (false, true)] {
        let (artifact, _) = ready_plan_fixture();
        let handoff = PlanHandoff::new(artifact, "source-session");
        let mut items = vec![TranscriptItem::Plan(PlanRecord::Handoff {
            handoff: handoff.clone(),
        })];
        if tool_history {
            let evidence = "tool evidence ".repeat(180);
            items.extend([
                TranscriptItem::Message(Message::Assistant {
                    id: None,
                    content: vec![tool_call("prior-tool", &evidence)],
                }),
                TranscriptItem::ToolResults {
                    skill_applications: Vec::new(),
                    message: Message::User {
                        content: vec![UserContent::tool_result(
                            "prior-tool",
                            "echo",
                            vec![ToolResultContent::text(evidence)],
                        )],
                    },
                    metadata: vec![ToolResultMetadata {
                        diagnostic: None,
                        id: "prior-tool".to_string(),
                        call_id: None,
                        tool_name: "echo".to_string(),
                        outcome: ToolCallOutcome::Success,
                        detail: None,
                    }],
                },
            ]);
        }
        let (_directory, mut transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        for item in &items {
            transcript.append(item).expect("persist handoff history");
        }
        drop(transcript);
        let loaded = zevria_transcript::transcript::load(&path).expect("reload handoff history");
        assert_eq!(loaded, items);
        let mut responses = vec![Ok(Message::assistant("resumed summary"))];
        if !manual {
            responses.push(Ok(Message::assistant("continued")));
        }
        let provider = ScriptedProvider::new(responses);
        let requests = provider.requests.clone();
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            TranscriptWriter::append_to(path.clone()).expect("reopen transcript"),
            Arc::new(SkillCatalog::default()),
        )
        .expect("resumed engine")
        .with_transcript_items(loaded)
        .unwrap()
        .with_compaction_policy(test_compaction_policy(1_000, 50, 200));
        assert!(!engine.conversation().has_real_user_prompt());
        assert!(engine.conversation().has_compaction_prompt());
        let (events, mut receiver) = session_event_channel(64);
        let command = if manual {
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            })
        } else {
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "continue".into(),
                mode: SessionMode::Build,
            })
        };
        engine.handle_command(command, &events).await.unwrap();

        let reloaded = zevria_transcript::transcript::load(&path).expect("reload checkpoint");
        assert_eq!(
            reloaded,
            conversation_records(engine.conversation().items())
        );
        assert_eq!(reloaded[..items.len()], items);
        let TranscriptItem::Compaction(checkpoint) = &reloaded[items.len()] else {
            panic!("compact the existing handoff/tool prefix before appending a prompt");
        };
        let trigger = if manual {
            CompactionTrigger::Manual
        } else {
            CompactionTrigger::AutomaticPreTurn
        };
        assert_eq!(checkpoint.trigger, trigger);
        assert_eq!(
            checkpoint
                .retained_user_messages
                .iter()
                .map(Message::user)
                .collect::<Vec<_>>(),
            vec![handoff.prompt.clone()]
        );
        let emitted = collect_events(&mut receiver).await;
        assert!(
            emitted
                .iter()
                .all(|event| !matches!(event, SessionEvent::TurnFailed { .. }))
        );
        assert!(emitted.iter().any(|event| matches!(event, SessionEvent::CompactionCompleted { trigger: actual, .. } if *actual == trigger)));
        let requests = requests.lock().expect("requests");
        assert_eq!(
            requests[0].history,
            zevria_transcript::transcript::model_history(&items)
        );
        assert_eq!(
            requests[0].prompt,
            Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
        );
        if manual {
            assert_eq!(requests.len(), 1);
            assert_eq!(engine.conversation().prompt_position(0), None);
            assert!(matches!(
                emitted.last(),
                Some(SessionEvent::CompactionCompleted { .. })
            ));
        } else {
            assert_eq!(requests.len(), 2);
            assert_eq!(
                engine.conversation().prompt_position(0),
                Some(items.len() + 1)
            );
            assert_eq!(
                requests[1].history,
                vec![Message::user(format!(
                    "{}\nresumed summary",
                    zevria_model::SUMMARY_PREFIX
                ))]
            );
            assert_eq!(requests[1].prompt, Message::user("continue"));
            assert!(matches!(
                emitted.last(),
                Some(SessionEvent::TurnCompleted { .. })
            ));
        }
    }
}

#[tokio::test]
async fn plan_handoff_mid_turn_compaction_obeys_usage_threshold_and_complete_tool_batch() {
    // Leave room for the protocol and retained handoff after compaction while
    // still exercising the exact one-token boundary in provider usage.
    for projected_tokens in [999, 1_000] {
        let due = projected_tokens == 1_000;
        let (artifact, _) = ready_plan_fixture();
        let handoff = PlanHandoff::new(artifact, "source-session");
        let tool_response = Message::Assistant {
            id: None,
            content: vec![tool_call("first", "one"), tool_call("second", "two")],
        };
        let tool_results = Message::User {
            content: vec![
                UserContent::tool_result("first", "echo", vec![ToolResultContent::text("one")]),
                UserContent::tool_result("second", "echo", vec![ToolResultContent::text("two")]),
            ],
        };
        let delta = zevria_model::compaction::estimate_message_tokens(&tool_results).payload_tokens;
        let usage = TokenUsage {
            total_tokens: projected_tokens - delta,
            ..TokenUsage::default()
        };
        let mut responses = vec![Ok(
            model_response(tool_response.clone()).with_usage(Some(usage))
        )];
        if due {
            responses.push(Ok(model_response(Message::assistant("mid-turn summary"))));
        }
        responses.push(Ok(model_response(Message::assistant("finished"))));
        // The provider's default counter is Unsupported, just like an
        // endpoint whose 404 has been cached as a missing capability.
        let provider = ScriptedProvider::with_model_responses(responses);
        let count_calls = provider.input_count_calls.clone();
        let requests = provider.requests.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = ToolServer::new()
            .tool(EchoTool {
                calls: calls.clone(),
            })
            .run();
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .expect("engine")
        .with_compaction_policy(test_compaction_policy(2_000, 50, 200));
        let (events, mut receiver) = session_event_channel(64);

        // No ordinary Submit may accidentally make this history eligible.
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::StartFromPlan {
                    handoff: handoff.clone(),
                }),
                &events,
            )
            .await
            .unwrap();

        assert_eq!(*calls.lock().expect("calls"), ["one", "two"]);
        let loaded =
            zevria_transcript::transcript::load(engine.conversation().path()).expect("reload");
        assert_eq!(loaded, conversation_records(engine.conversation().items()));
        assert!(
            loaded
                .iter()
                .all(|item| !zevria_transcript::transcript::is_prompt_item(item))
        );
        assert_eq!(engine.conversation().prompt_position(0), None);
        assert!(matches!(
            &loaded[2],
            TranscriptItem::ToolResults { message, metadata, .. }
                if message == &tool_results
                    && metadata.len() == 2
                    && metadata.iter().all(|result| result.outcome.is_success())
        ));
        assert_eq!(
            loaded
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
                .count(),
            usize::from(due),
            "projected input: {projected_tokens}"
        );
        let emitted = collect_events(&mut receiver).await;
        assert!(emitted.iter().any(|event| matches!(
            event,
            SessionEvent::ContextUsageUpdated { snapshot, .. }
                if snapshot.source == ContextTokenSource::UsagePlusDelta
                    && snapshot.projected_input_tokens == projected_tokens
                    && snapshot.automatic_trigger == 1_000
        )));
        assert!(matches!(
            emitted.last(),
            Some(SessionEvent::TurnCompleted { .. })
        ));
        let requests = requests.lock().expect("requests");
        assert_eq!(requests[0].prompt, handoff.prompt);
        if due {
            // Both correlated results are durable before the checkpoint,
            // and the summarizer receives the whole batch, never a prefix.
            let TranscriptItem::Compaction(checkpoint) = &loaded[3] else {
                panic!("checkpoint must immediately follow the completed tool batch");
            };
            assert_eq!(checkpoint.trigger, CompactionTrigger::AutomaticMidTurn);
            assert_eq!(checkpoint.backend, CompactionBackend::LocalSummary);
            assert_eq!(
                checkpoint
                    .retained_user_messages
                    .iter()
                    .map(Message::user)
                    .collect::<Vec<_>>(),
                vec![handoff.prompt.clone()]
            );
            assert_eq!(requests.len(), 3);
            assert_eq!(
                requests[1].history,
                vec![handoff.prompt.clone(), tool_response, tool_results]
            );
            assert_eq!(
                requests[1].prompt,
                Message::user(zevria_model::compaction::SUMMARIZATION_PROMPT)
            );
            assert_eq!(requests[1].allowed_tool_names, Some(Vec::new()));
            assert!(requests[2].history.is_empty());
            assert_eq!(
                requests[2].prompt,
                Message::user(format!(
                    "{}\nmid-turn summary",
                    zevria_model::SUMMARY_PREFIX
                ))
            );
            assert_eq!(
                count_calls.load(Ordering::SeqCst),
                2,
                "remeasure after compaction"
            );
            let tool_event = emitted
                .iter()
                .position(|event| matches!(event, SessionEvent::ToolResults { .. }))
                .expect("tool event");
            let compact_event = emitted
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        SessionEvent::CompactionStarted {
                            trigger: CompactionTrigger::AutomaticMidTurn,
                            ..
                        }
                    )
                })
                .expect("compaction event");
            assert!(tool_event < compact_event);
            assert_eq!(
                emitted
                    .iter()
                    .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
                    .count(),
                1
            );
            assert!(emitted[compact_event..].iter().any(|event| matches!(
                event,
                SessionEvent::ContextUsageUpdated { snapshot, .. }
                    if snapshot.source == ContextTokenSource::ConservativeEstimate
                        && snapshot.projected_input_tokens < snapshot.automatic_trigger
            )));
        } else {
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1].history, vec![handoff.prompt, tool_response]);
            assert_eq!(requests[1].prompt, tool_results);
            assert_eq!(
                count_calls.load(Ordering::SeqCst),
                0,
                "the initial handoff stays below the exact-count threshold"
            );
            assert!(
                emitted
                    .iter()
                    .all(|event| !matches!(event, SessionEvent::CompactionStarted { .. }))
            );
        }
    }
}

#[tokio::test]
async fn mid_turn_compaction_waits_for_the_complete_tool_result_batch() {
    let usage = TokenUsage {
        total_tokens: 1000,
        ..TokenUsage::default()
    };
    let provider = ScriptedProvider::with_model_responses([
        Ok(model_response(Message::Assistant {
            id: None,
            content: vec![tool_call("call-mid", "value")],
        })
        .with_usage(Some(usage))),
        Ok(model_response(Message::assistant("mid-turn summary"))),
        Ok(model_response(Message::assistant("finished"))),
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: calls.clone(),
        })
        .run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(2_000, 50, 20));
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "use the tool".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(*calls.lock().expect("calls"), ["value"]);
    let items = engine.conversation().items();
    let tool_result_index = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::ToolResults { .. }))
        .expect("tool results");
    let checkpoint_index = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("mid-turn checkpoint");
    assert!(tool_result_index < checkpoint_index);

    let events = collect_events(&mut receiver).await;
    let tool_event = events
        .iter()
        .position(|event| matches!(event, SessionEvent::ToolResults { .. }))
        .expect("tool event");
    let compact_event = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::CompactionStarted {
                    trigger: CompactionTrigger::AutomaticMidTurn,
                    ..
                }
            )
        })
        .expect("compaction event");
    assert!(tool_event < compact_event);
    let calls = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionEvent::ModelCallStarted { call, .. } => Some((index, *call)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls.iter().map(|(_, call)| *call).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(calls[0].0 < tool_event);
    assert!(calls[1].0 > compact_event);
    assert!(
        events[compact_event..calls[1].0]
            .iter()
            .any(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}
