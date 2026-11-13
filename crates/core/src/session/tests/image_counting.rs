//! Exact image measurement is needed only until compatible usage covers it.
use super::*;

fn image_prompt() -> zevria_content::UserPrompt {
    zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(
        zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
    )])
    .unwrap()
}

fn response(message: Message, usage: bool) -> anyhow::Result<ModelResponse> {
    Ok(
        ModelResponse::plain(message)?.with_usage(usage.then_some(TokenUsage {
            input_tokens: 11_000,
            cached_tokens: 10_000,
            output_tokens: 1_000,
            total_tokens: 12_000,
        })),
    )
}

#[tokio::test]
async fn three_image_tool_rounds_count_once_when_usage_covers_images_and_recount_new_batches() {
    for usage in [true, false] {
        let (_directory, transcript) = test_transcript();
        let responses = (0..3)
            .map(|index| {
                response(
                    Message::Assistant(rig_core::message::AssistantMessage::new(vec![tool_call(
                        &format!("image-call-{index}"),
                        "echo",
                    )])),
                    usage,
                )
            })
            .chain((0..4).map(|_| response(Message::assistant("done"), usage)));
        let provider = ScriptedProvider::with_model_responses(responses)
            .with_input_counts((0..8).map(|_| Ok(InputTokenCount::Exact(15_000))));
        let counts = provider.input_count_calls.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = ToolServer::new()
            .tool(EchoTool {
                calls: calls.clone(),
            })
            .run();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000));
        let (events, mut receiver) = session_event_channel(256);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: image_prompt(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        let updates = collect_events(&mut receiver).await;
        assert!(
            updates
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
        );
        assert!(!updates.iter().any(|event| matches!(
            event,
            SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
        )));
        assert_eq!(engine.provider.requests.lock().unwrap().len(), 4);
        assert_eq!(calls.lock().unwrap().len(), 3);
        assert_eq!(
            counts.load(Ordering::SeqCst),
            if usage { 1 } else { 4 },
            "absent usage must not invent an image baseline"
        );
        if !usage {
            continue;
        }
        // A new image batch counts once; following text with old images reuses
        // the compatible baseline. A different request shape must count again.
        for (prompt, mode, expected) in [
            (image_prompt(), SessionMode::Build, 2),
            ("text only".into(), SessionMode::Build, 2),
            ("different policy".into(), SessionMode::Plan, 3),
        ] {
            engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: prompt,
                        mode,
                    }),
                    &events,
                )
                .await
                .unwrap();
            let updates = collect_events(&mut receiver).await;
            assert!(
                updates
                    .iter()
                    .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
            );
            assert_eq!(counts.load(Ordering::SeqCst), expected);
        }
        assert_eq!(engine.provider.requests.lock().unwrap().len(), 7);
    }
}

#[tokio::test]
async fn measured_images_still_count_near_the_trigger() {
    let (_directory, transcript) = test_transcript();
    let provider =
        ScriptedProvider::new([]).with_input_counts([Ok(InputTokenCount::Exact(50_000))]);
    let counts = provider.input_count_calls.clone();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000));
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    engine
        .commit(TranscriptItem::Message(image_prompt().to_message()))
        .unwrap();
    engine.report_provider_usage(&policy, 900_001).unwrap();
    let profile = engine.context_policy(ModelRole::Build).profile.clone();
    assert!(!engine.context.usage[&profile].unmeasured_images);
    let (events, _receiver) = session_event_channel(32);
    let progress = ProgressReporter::for_turn(
        events,
        TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
        ModelRole::Build,
        engine.context_policy(ModelRole::Build),
    );
    let snapshot = engine
        .assess_current_request(&policy, &progress, true, false)
        .await
        .unwrap();
    assert_eq!(counts.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot.source, ContextTokenSource::Exact);
    assert_eq!(snapshot.projected_input_tokens, 50_000);
}

#[tokio::test]
async fn image_flags_are_profile_local_and_reestimation_discards_measurement() {
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(role_compaction_policy(
        90,
        ("build", 1_000_000, 20_000),
        ("plan", 1_000_000, 20_000),
        ("review", 1_000_000, 20_000),
        ("explore", 1_000_000, 20_000),
        ("builder", 1_000_000, 20_000),
    ));
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    engine
        .commit(TranscriptItem::Message(image_prompt().to_message()))
        .unwrap();
    assert!(
        engine
            .context
            .usage
            .values()
            .all(|tracker| tracker.unmeasured_images)
    );
    let profile = engine.context_policy(ModelRole::Build).profile.clone();
    engine.report_provider_usage(&policy, 12_000).unwrap();
    assert!(!engine.context.usage[&profile].unmeasured_images);
    assert!(
        engine
            .context
            .usage
            .iter()
            .filter(|(key, _)| *key != &profile)
            .all(|(_, tracker)| tracker.unmeasured_images)
    );
    engine
        .commit(TranscriptItem::Message(Message::user("text only")))
        .unwrap();
    assert!(!engine.context.usage[&profile].unmeasured_images);
    engine
        .commit(TranscriptItem::Message(image_prompt().to_message()))
        .unwrap();
    assert!(engine.context.usage[&profile].unmeasured_images);
    engine.report_provider_usage(&policy, 12_000).unwrap();
    engine.reestimate_context_usage();
    assert!(
        engine
            .context
            .usage
            .values()
            .all(|tracker| tracker.unmeasured_images && tracker.last_provider_total.is_none())
    );
}
