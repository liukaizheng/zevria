use super::*;
use zevria_session_api::{PlanEvent, TurnEvent};

#[path = "image_counting.rs"]
mod image_counting;

#[tokio::test]
async fn image_only_messages_and_skill_arguments_are_durable_before_provider_failure() {
    for skill in [false, true] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let original = zevria_content::test_support::oversized_image();
        let image = zevria_content::image_preparation::prepare_encoded(
            original.mime_type(),
            original.encoded_bytes().to_vec(),
        )
        .unwrap()
        .image;
        assert!(image.dimensions().0 < original.dimensions().0);
        let prompt =
            zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(original)])
                .unwrap();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([Err(anyhow::anyhow!("vision endpoint rejected the input"))]),
            test_skill_tools(),
            test_policies(),
            transcript,
            test_skills(),
        )
        .unwrap();
        let command = if skill {
            TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: prompt.clone(),
                mode: SessionMode::Build,
            }
        } else {
            TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: prompt.clone(),
                mode: SessionMode::Build,
            }
        };
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_command(SessionCommand::Turn(command), &events)
            .await
            .unwrap();
        let updates = collect_events(&mut receiver).await;
        assert!(updates.iter().any(|event| matches!(event, SessionEvent::Turn(TurnEvent::Started { message, .. }) if zevria_content::prompt::message_has_images(message))));
        assert!(
            updates
                .iter()
                .any(|event| matches!(event, SessionEvent::Turn(TurnEvent::Failed { .. })))
        );
        assert!(
            engine.provider.input_count_calls.load(Ordering::SeqCst) > 0,
            "image input prefers exact endpoint counting"
        );
        let loaded = zevria_transcript::transcript::load(&path).unwrap();
        let retained = loaded
            .iter()
            .filter_map(TranscriptItem::message)
            .find(|message| zevria_content::prompt::message_has_images(message))
            .unwrap();
        let restored = zevria_content::UserPrompt::from_message(retained).unwrap();
        assert_eq!(restored.images().next(), Some(&image));
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(
            zevria_content::UserPrompt::from_message(&requests[0].prompt)
                .unwrap()
                .images()
                .next(),
            Some(&image)
        );
    }
}

#[tokio::test]
async fn recalled_new_submission_and_intentional_edit_prepare_only_the_new_suffix() {
    let (_directory, mut transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let historical = zevria_content::PromptImage::from_file_bytes(
        zevria_content::test_support::padded_png(4_730_667),
    )
    .unwrap();
    let recalled = zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(
        historical.clone(),
    )])
    .unwrap();
    let items = vec![
        TranscriptItem::Message(Message::user("fixed prefix")),
        TranscriptItem::Message(Message::assistant("fixed answer")),
        TranscriptItem::Message(recalled.to_message()),
        TranscriptItem::Message(Message::assistant("old image")),
    ];
    persist_fixture(&mut transcript, &items);
    let prefix = std::fs::read(&path).unwrap();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("new image")),
            Ok(Message::assistant("edited image")),
        ]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_fixture(items.clone())
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: recalled.clone(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    collect_events(&mut receiver).await;
    assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
    let request = engine.provider.requests.lock().unwrap()[0].clone();
    assert!(request.history.iter().any(|m| {
        zevria_content::UserPrompt::from_message(m)
            .is_ok_and(|p| p.images().next() == Some(&historical))
    }));
    let accepted = zevria_content::UserPrompt::from_message(&request.prompt).unwrap();
    assert!(accepted.images().next().unwrap().encoded_len() < 1024);
    assert_eq!(
        &engine.conversation.items()[..items.len()],
        items.as_slice()
    );
    engine
        .handle_command(
            prompt_message_edit(1, recalled, SessionMode::Build),
            &events,
        )
        .await
        .unwrap();
    collect_events(&mut receiver).await;
    let requests = engine.provider.requests.lock().unwrap();
    assert_eq!(
        zevria_content::UserPrompt::from_message(&requests[1].prompt).unwrap(),
        accepted
    );
    assert_eq!(&engine.conversation.items()[..2], &items[..2]);
}

#[tokio::test]
async fn failed_or_cancelled_preparation_never_commits_a_new_prompt() {
    for cancelled in [false, true] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let before = std::fs::read(&path).unwrap();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([]),
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let prompt = zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(
            zevria_content::test_support::oversized_animated_gif(),
        )])
        .unwrap();
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        if cancelled {
            turn.cancellation().cancel();
        }
        let (events, _) = session_event_channel(32);
        let result = engine
            .prepare_and_accept_prompt(
                crate::session::kernel::records::TurnAnchor::Append,
                crate::session::workflow::prompt::PromptTurnInput::Message {
                    text: prompt,
                    behavior: zevria_foundation::RequestBehavior::Standard,
                },
                SessionMode::Build,
                &events,
                &turn,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(engine.provider.requests.lock().unwrap().is_empty());
        assert!(engine.conversation.items().is_empty());
    }
}

#[tokio::test]
async fn ready_message_revision_commits_before_feedback_and_runs_only_in_plan_mode() {
    for text in ["revise it", "/literal feedback", "$literal feedback"] {
        let (artifact, mut items) = ready_plan_fixture();
        items.insert(0, TranscriptItem::SessionMode(SessionMode::Plan));
        let (_directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([Ok(Message::assistant("Still revising."))]),
            test_skill_tools(),
            test_policies(),
            transcript,
            test_skills(),
        )
        .unwrap()
        .with_fixture(items.clone())
        .unwrap()
        .with_mode_management()
        .with_subtask_concurrency(1);
        assert_eq!(engine.selected_mode(), SessionMode::Plan);
        let (events, mut receiver) = session_event_channel(128);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::RevisePlanWithMessage {
                    expected: artifact.version,
                    text: text.into(),
                }),
                &events,
            )
            .await
            .unwrap();
        let tail = &engine.conversation.items()[items.len()..];
        assert!(
            matches!(tail[0].plan_record(), Some(PlanRecord::RevisionRequested { artifact: revised }) if revised == &artifact)
        );
        let prompt_index = tail.iter().position(|item| matches!(item,
            TranscriptItem::RequestPrompt { message, request }
                if message == &Message::user(text) && request.behavior == zevria_foundation::RequestBehavior::Standard
        )).unwrap();
        assert!(prompt_index > 0);
        assert!(
            matches!(&tail[prompt_index + 1], TranscriptItem::RequestDirective(directive)
            if directive.request.behavior == zevria_foundation::RequestBehavior::Standard)
        );
        assert_eq!(
            engine.plan_state().unwrap(),
            &PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact.clone()),
                draft: None,
            }
        );
        assert_eq!(engine.selected_mode(), SessionMode::Plan);
        assert!(!tail.iter().any(|item| matches!(
            item.plan_record(),
            Some(PlanRecord::Resolved { .. } | PlanRecord::Handoff { .. })
        )));
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model_role, ModelRole::Plan);
        assert!(requests[0].instructions.contains("Plan test instructions"));
        drop(requests);
        let updates = collect_events(&mut receiver).await;
        let started = updates.iter().position(|event| matches!(event,
            SessionEvent::Turn(TurnEvent::Started { mode: SessionMode::Plan, message, .. })
                if message == &Message::user(if text.starts_with('/') || text.starts_with('$') {
                    format!(" {text}")
                } else { text.to_string() })
        )).unwrap();
        let planning = updates.iter().position(|event| matches!(event,
            SessionEvent::Plan(PlanEvent::StateChanged { state })
                 if matches!(state.as_ref(), PlanWorkflowState::Planning { previous: Some(previous), draft: None, .. } if previous == &artifact))).unwrap();
        assert!(started < planning);
        assert!(updates[..started].iter().any(|event| matches!(
            event,
            SessionEvent::Turn(TurnEvent::ModeChanged {
                mode: SessionMode::Plan
            })
        )));
        assert!(!updates.iter().any(|event| matches!(
            event,
            SessionEvent::Plan(PlanEvent::HandoffStarted { .. })
                | SessionEvent::Plan(PlanEvent::FreshHandoffRequested { .. })
                | SessionEvent::Turn(TurnEvent::ModeChanged {
                    mode: SessionMode::Build
                })
        )));
    }
}

#[tokio::test]
async fn ready_message_revision_rejections_preserve_plan_and_durable_history() {
    for case in [
        "stale",
        "blank",
        "planning",
        "published",
        "idle",
        "cancelled",
        "persistence",
        "capacity",
    ] {
        let (artifact, ready) = ready_plan_fixture();
        let items = match case {
            "planning" => ready[..1].to_vec(),
            "published" => super::mode::published_history(&artifact),
            "idle" => Vec::new(),
            _ => ready,
        };
        let (_directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let path = transcript.path().to_path_buf();
        let before_bytes = std::fs::read(&path).unwrap();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([]),
            test_skill_tools(),
            test_policies(),
            transcript,
            test_skills(),
        )
        .unwrap()
        .with_fixture(items.clone())
        .unwrap()
        .with_mode_management();
        if case == "capacity" {
            engine = engine.with_compaction_policy(test_compaction_policy(1, 80, 0));
        }
        let before_plan = engine.plan_state().unwrap().clone();
        let before_mode = engine.selected_mode();
        let mut expected = artifact.version;
        if case == "stale" {
            expected.revision += 1;
        }
        let backup = path.with_extension("saved");
        if case == "persistence" {
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        let turn = TurnContext::new(TurnId::new(77), SessionMode::Plan, CancellationToken::new());
        if case == "cancelled" {
            turn.cancellation().cancel();
        }
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_turn(
                TurnCommand::RevisePlanWithMessage {
                    expected,
                    text: if case == "blank" { " \n " } else { "revise it" }.into(),
                },
                &turn,
                &events,
            )
            .await
            .unwrap();
        assert_eq!(engine.conversation.items(), items, "{case}");
        assert_eq!(engine.plan_state().unwrap(), &before_plan, "{case}");
        assert_eq!(engine.selected_mode(), before_mode, "{case}");
        assert!(
            engine.provider.requests.lock().unwrap().is_empty(),
            "{case}"
        );
        let updates = collect_events(&mut receiver).await;
        assert!(
            !updates.iter().any(|event| matches!(
                event,
                SessionEvent::Turn(TurnEvent::Started { .. })
                    | SessionEvent::Plan(PlanEvent::StateChanged { .. })
            )),
            "{case}: {updates:?}"
        );
        assert!(
            updates.iter().any(|event| matches!(
                event,
                SessionEvent::Turn(TurnEvent::Rejected { .. })
                    | SessionEvent::Turn(TurnEvent::Cancelled { .. })
            )),
            "{case}: {updates:?}"
        );
        if case == "persistence" {
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes, "{case}");
        assert_eq!(
            zevria_transcript::transcript::load(&path).unwrap(),
            items,
            "{case}"
        );
    }
}

#[tokio::test]
async fn skill_revision_is_not_committed_when_prompt_admission_fails() {
    let (artifact, items) = ready_plan_fixture();
    let (_directory, mut transcript) = test_transcript();
    persist_fixture(&mut transcript, &items);
    let path = transcript.path().to_path_buf();
    let before = std::fs::read(&path).unwrap();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap()
    .with_fixture(items.clone())
    .unwrap()
    .with_skill_management(Arc::new(TestSkillManagement), [true, true])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1, 80, 0));
    engine
        .policies
        .policy_mut(SessionMode::Plan)
        .allowed_tool_names = Some(vec!["command".into(), "skill".into()]);
    let (events, mut receiver) = session_event_channel(16);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::RevisePlanWithSkill {
                expected: artifact.version,
                name: "review".parse().unwrap(),
                args: "revise".into(),
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(engine.conversation.items(), items);
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Ready { .. }
    ));
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    assert!(
        matches!(collect_events(&mut receiver).await.as_slice(), [SessionEvent::Turn(TurnEvent::Rejected { error, .. })] if error.contains("irreducible instruction-state"))
    );
}

#[tokio::test]
async fn ready_skill_revision_rejections_preserve_plan_history_and_pins() {
    use zevria_instructions::skill::SkillEnableRule;
    use zevria_instructions::skill::SkillsConfig;
    for case in [
        "missing",
        "disabled_name",
        "disabled_global",
        "workflow",
        "tool_policy",
        "unregistered",
        "stale",
        "cancelled",
        "persistence",
    ] {
        let (artifact, items) = ready_plan_fixture();
        let (_directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let path = transcript.path().to_path_buf();
        let before_bytes = std::fs::read(&path).unwrap();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new([]),
            test_skill_tools(),
            test_policies(),
            transcript,
            test_skills(),
        )
        .unwrap()
        .with_fixture(items)
        .unwrap()
        .with_mode_management();
        engine.policies.policy_mut(SessionMode::Plan).skills_enabled = true;
        engine
            .policies
            .policy_mut(SessionMode::Plan)
            .allowed_tool_names = Some(vec!["skill".into()]);
        let mut expected = artifact.version;
        let mut name: zevria_instructions::skill::SkillName = "review".parse().unwrap();
        match case {
            "missing" => name = "missing".parse().unwrap(),
            "disabled_name" | "disabled_global" => {
                engine.skills.catalog = Arc::new(
                    engine
                        .skills
                        .catalog
                        .as_ref()
                        .clone()
                        .with_config(SkillsConfig {
                            enabled: case != "disabled_global",
                            rules: vec![SkillEnableRule {
                                name: name.clone(),
                                enabled: false,
                            }],
                        })
                        .unwrap(),
                );
            }
            "workflow" => engine.policies.policy_mut(SessionMode::Plan).skills_enabled = false,
            "tool_policy" => {
                engine
                    .policies
                    .policy_mut(SessionMode::Plan)
                    .allowed_tool_names = Some(vec!["command".into()])
            }
            "unregistered" => engine.tools = ToolServer::new().run(),
            "stale" => expected.revision += 1,
            _ => {}
        }
        let before = engine.conversation.items().to_vec();
        let backup = path.with_extension("saved");
        if case == "persistence" {
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        let turn = TurnContext::new(TurnId::new(77), SessionMode::Plan, CancellationToken::new());
        if case == "cancelled" {
            turn.cancellation().cancel();
        }
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_turn(
                TurnCommand::RevisePlanWithSkill {
                    expected,
                    name,
                    args: "revise with the skill".into(),
                },
                &turn,
                &events,
            )
            .await
            .unwrap();
        assert_eq!(engine.conversation.items(), before, "{case}");
        assert!(
            matches!(engine.plan_state().unwrap(), PlanWorkflowState::Ready { artifact: current } if current.version == artifact.version),
            "{case}"
        );
        assert!(engine.active_skills().unwrap().is_empty(), "{case}");
        assert!(
            engine.provider.requests.lock().unwrap().is_empty(),
            "{case}"
        );
        let events = collect_events(&mut receiver).await;
        assert!(
            !events.iter().any(|e| matches!(
                e,
                SessionEvent::Turn(TurnEvent::Started { .. })
                    | SessionEvent::Plan(PlanEvent::StateChanged { .. })
            )),
            "{case}: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::Turn(TurnEvent::Rejected { .. })
                    | SessionEvent::Turn(TurnEvent::Cancelled { .. })
            )),
            "{case}: {events:?}"
        );
        if case == "persistence" {
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes, "{case}");
        let restored = zevria_transcript::transcript::load(&path).unwrap();
        assert_eq!(restored, before, "{case}");
        assert!(
            zevria_transcript::transcript::replay_active_skills(&restored)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn cancelled_child_admission_uses_parent_identity_without_a_transcript_row() {
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(4);
    let turn = TurnContext::new(
        TurnId::new(42),
        SessionMode::Build,
        CancellationToken::new(),
    );
    turn.cancellation().cancel();
    engine
        .handle_turn(
            TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "child".into(),
                mode: SessionMode::Build,
            },
            &turn,
            &events,
        )
        .await
        .unwrap();
    assert!(
        matches!(collect_events(&mut receiver).await.as_slice(), [SessionEvent::Turn(TurnEvent::Cancelled { turn_id })] if *turn_id == turn.id)
    );
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(zevria_transcript::transcript::is_leading_metadata)
    );
    assert_eq!(engine.next_turn_id, 1);
    assert_eq!(engine.provider.resets, 1);
}

#[tokio::test]
async fn finalized_provider_work_is_retained_when_failure_races_cancellation() {
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap();
    let start = ensemble_start_fixture("retained", EnsembleWorkflow::Plan, "plan");
    engine
        .commit(vec![
            TranscriptItem::Ensemble(Box::new(EnsembleRecord::Started {
                start: start.clone(),
            })),
            TranscriptItem::Ensemble(Box::new(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            })),
        ])
        .unwrap();
    let accepted = AcceptedTurn::new(AcceptedKind::Ensemble {
        run_id: start.run_id.clone(),
        workflow: EnsembleWorkflow::Plan,
        resumed: false,
    });
    let final_message = Message::assistant("completed without the required inspection");
    let final_item = TranscriptItem::Message(final_message.clone());
    let output = ModelTurnOutput {
        attempt_id: None,
        message: Some(final_message),
        final_item: Some(final_item.clone()),
        usage: Some(TokenUsage {
            total_tokens: 123,
            ..TokenUsage::default()
        }),
        submission_gate: PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default()),
    };
    let turn = TurnContext::new(TurnId::new(4), SessionMode::Plan, CancellationToken::new());
    let (events, mut receiver) = session_event_channel(8);
    let outcome = engine
        .finalize_turn(
            &accepted,
            output,
            FinalResponsePersistence::Deferred,
            &events,
            &turn,
        )
        .await;
    turn.cancellation().cancel();
    engine
        .finish_accepted(&accepted, &turn, &events, outcome)
        .await
        .unwrap();
    let items = engine.conversation.items();
    assert_eq!(items[2], final_item);
    assert!(
        matches!(items[3].ensemble_record(), Some(EnsembleRecord::Failed { run_id, .. }) if run_id == &start.run_id)
    );
    assert!(matches!(&items[4], TranscriptItem::Error { error } if error.contains("inspection")));
    assert!(matches!(
        collect_events(&mut receiver).await.as_slice(),
        [
            SessionEvent::Turn(TurnEvent::UsageUpdated { turn_id, usage, .. }),
            SessionEvent::Turn(TurnEvent::Failed { error, .. })
        ] if *turn_id == turn.id && usage.total_tokens == 123 && error.contains("inspection")
    ));
    assert_eq!(
        zevria_transcript::transcript::load(engine.conversation.path()).unwrap(),
        items
    );
}
