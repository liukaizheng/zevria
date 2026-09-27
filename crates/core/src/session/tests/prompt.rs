use super::*;

#[tokio::test]
async fn image_only_messages_and_skill_arguments_are_durable_before_provider_failure() {
    for skill in [false, true] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
        let prompt = zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(
            image.clone(),
        )])
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
        assert!(updates.iter().any(|event| matches!(event, SessionEvent::TurnStarted { message, .. } if zevria_content::prompt::message_has_images(message))));
        assert!(
            updates
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnFailed { .. }))
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
        matches!(collect_events(&mut receiver).await.as_slice(), [SessionEvent::TurnRejected { error, .. }] if error.contains("irreducible instruction-state"))
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
                SessionEvent::TurnStarted { .. } | SessionEvent::PlanStateChanged { .. }
            )),
            "{case}: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnRejected { .. } | SessionEvent::TurnCancelled { .. }
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
        matches!(collect_events(&mut receiver).await.as_slice(), [SessionEvent::TurnCancelled { turn_id }] if *turn_id == turn.id)
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
        .record_required_items(vec![
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: start.clone(),
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                run_id: start.run_id.clone(),
                version: ENSEMBLE_REVIEW_VERSION,
            }),
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
        display_attempt_id: None,
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
        matches!(&items[3], TranscriptItem::Ensemble(EnsembleRecord::Failed { run_id, .. }) if run_id == &start.run_id)
    );
    assert!(matches!(&items[4], TranscriptItem::Error { error } if error.contains("inspection")));
    assert!(
        matches!(collect_events(&mut receiver).await.as_slice(), [SessionEvent::TurnFailed { error, .. }] if error.contains("inspection"))
    );
    assert_eq!(
        zevria_transcript::transcript::load(engine.conversation.path()).unwrap(),
        items
    );
}
