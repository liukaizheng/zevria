//! Exact image measurement is needed only until compatible usage covers it.

#[test]
fn persisted_same_mode_append_preserves_measured_image_baseline() {
    use crate::session::kernel::records::{RecordChange, TurnAnchor};
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000))
    .with_mode_management();
    // Establish a real persisted root mode before reporting provider usage.
    engine
        .commit(RecordChange::anchored(
            TurnAnchor::Append,
            vec![TranscriptItem::Message(image_prompt().to_message())],
            SessionMode::Build,
        ))
        .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    engine.report_provider_usage(&policy, 172_483).unwrap();
    let profile = engine.context_policy(ModelRole::Build).profile.clone();
    let before = engine.conversation.items().len();
    let committed = engine
        .commit(RecordChange::anchored(
            TurnAnchor::Append,
            vec![TranscriptItem::Message(Message::user("commit the changes"))],
            SessionMode::Build,
        ))
        .unwrap();
    assert_eq!(
        committed.range.start, before,
        "only the committed suffix is new"
    );
    assert_eq!(
        engine.context.usage[&profile].last_provider_total,
        Some(172_483)
    );
    assert!(!engine.context.usage[&profile].unmeasured_images);
}
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
async fn tool_result_images_trigger_exact_counts_and_usage_covers_only_existing_batches() {
    for usage in [true, false] {
        let (_directory, transcript) = test_transcript();
        let responses = (0..3)
            .map(|index| {
                response(
                    Message::Assistant(rig_core::message::AssistantMessage::new(vec![
                        named_tool_call(&format!("tool-image-{index}"), "rich_output", json!({})),
                    ])),
                    usage,
                )
            })
            .chain((0..2).map(|_| response(Message::assistant("done"), usage)));
        let provider = ScriptedProvider::with_model_responses(responses)
            .with_input_counts((0..8).map(|_| Ok(InputTokenCount::Exact(15_000))));
        let counts = provider.input_count_calls.clone();
        let tools = ToolServer::new()
            .tool(RichOutputTestTool(
                rig_core::tool::ToolOutput::content(vec![
                    ToolResultContent::text("image summary"),
                    zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255])
                        .unwrap()
                        .to_tool_result_content(),
                ])
                .unwrap(),
            ))
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
        for (text, expected) in [
            ("read three images", 3),
            ("text follow-up", if usage { 3 } else { 4 }),
        ] {
            engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: text.into(),
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
            assert_eq!(
                counts.load(Ordering::SeqCst),
                expected,
                "usage={usage}, prompt={text}"
            );
        }
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[0].prompt.images().next().is_none());
        assert_eq!(requests[1].prompt.images().count(), 1);
        assert!(engine.conversation.has_images());
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
    assert_eq!(snapshot.projected_input_tokens, Some(50_000));
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

#[tokio::test]
async fn safe_token_rejection_compacts_and_retries_only_the_pending_round() {
    for reject_again in [false, true] {
        let (_directory, transcript) = test_transcript();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = ToolServer::new()
            .tool(EchoTool {
                calls: calls.clone(),
            })
            .run();
        let rejection = || {
            Err(
                zevria_model::ModelInputTooLarge::pre_execution(anyhow::anyhow!(
                    "synthetic pre-execution token rejection"
                ))
                .into(),
            )
        };
        let responses = vec![
            response(
                Message::Assistant(rig_core::message::AssistantMessage::new(vec![tool_call(
                    "once", "echo",
                )])),
                true,
            ),
            rejection(),
            response(Message::assistant("compact summary"), false),
            if reject_again {
                rejection()
            } else {
                response(Message::assistant("done after recovery"), true)
            },
        ];
        let provider = ScriptedProvider::with_model_responses(responses);
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
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "completed local effects must never be rerun"
        );
        assert_eq!(
            engine.provider.requests.lock().unwrap().len(),
            4,
            "two normal attempts plus previous tool round and one synthetic summary"
        );
        assert_eq!(
            engine
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
                .count(),
            1
        );
        assert_eq!(
            updates
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnCompleted { .. })),
            !reject_again
        );
        assert_eq!(
            updates
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnFailed { .. })),
            reject_again
        );
        let rejections = engine.conversation.items().iter().filter(|item| matches!(item,TranscriptItem::IncompleteResponse(record) if record.input_rejection.is_some())).collect::<Vec<_>>();
        assert_eq!(
            rejections.len(),
            if reject_again { 2 } else { 1 },
            "every rejected ordinary attempt must be durable"
        );
        assert!(
            rejections
                .iter()
                .all(|item| item.model_request_item().is_none())
        );
        assert!(
            engine
                .provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !serde_json::to_string(&request.history)
                    .unwrap()
                    .contains("Zevria diagnostic")),
            "failed attempt diagnostics must never enter replay input"
        );
        if !reject_again {
            let profile = engine.context_policy(ModelRole::Build).profile.clone();
            assert_eq!(
                engine.context.usage[&profile].last_provider_total,
                Some(12_000)
            );
        }
    }
}

#[tokio::test]
async fn ambiguous_or_upload_size_failures_never_authorize_context_replay() {
    for error in [
        zevria_model::ModelInputTooLarge::new(anyhow::anyhow!(
            "failed response; hosted execution may have happened"
        ))
        .into(),
        zevria_model::ModelRequestBodyTooLarge(anyhow::anyhow!("HTTP 413")).into(),
        anyhow::anyhow!("authentication failure"),
    ] {
        let (_directory, transcript) = test_transcript();
        let provider = ScriptedProvider::with_model_responses([Err(error)]);
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000));
        let (events, mut receiver) = session_event_channel(128);
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
        assert!(
            collect_events(&mut receiver)
                .await
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnFailed { .. }))
        );
        assert_eq!(engine.provider.requests.lock().unwrap().len(), 1);
        assert!(
            !engine
                .conversation
                .items()
                .iter()
                .any(|item| matches!(item, TranscriptItem::Compaction(_)))
        );
    }
}

#[tokio::test]
async fn unresolved_image_admission_is_explicit_and_transitions_back_to_usage() {
    let (_directory, transcript) = test_transcript();
    let provider =
        ScriptedProvider::with_model_responses([response(Message::assistant("measured"), true)]);
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000))
    .with_mode_management();
    let (events, mut receiver) = session_event_channel(128);
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
    let before = updates
        .iter()
        .find_map(|event| match event {
            SessionEvent::ContextUsageUpdated { snapshot, .. } => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(before.projected_input_tokens, None);
    assert_eq!(before.unresolved_media.images, 1);
    assert!(before.known_input_tokens > 0);
    assert!(before.known_input_tokens < 100_000);
    assert_eq!(before.source, ContextTokenSource::IncompleteEstimate);
    assert!(
        updates
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    let restored = engine.restored_model_contexts().unwrap();
    // An in-memory compatible baseline survives local restoration snapshots.
    assert!(
        restored
            .iter()
            .any(|snapshot| snapshot.model_role == ModelRole::Build
                && snapshot.projected_input_tokens.is_some()
                && snapshot.source == ContextTokenSource::UsagePlusDelta)
    );
    // Losing in-memory measurements (the resume case) derives local facts only.
    engine.reestimate_context_usage();
    assert!(
        engine
            .restored_model_contexts()
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.projected_input_tokens.is_none())
    );
    engine
        .report_provider_usage(&engine.policies.policy(SessionMode::Build).clone(), 12_000)
        .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    let (_, usage) = engine.local_context_candidates(&policy).unwrap();
    assert!(usage.unwrap().complete_tokens().is_some());
}

#[tokio::test]
async fn observed_output_hosted_activity_or_accepted_response_veto_pre_execution_marker() {
    struct ObservedFailure {
        kind: u8,
        calls: Arc<AtomicUsize>,
    }
    impl ModelProvider for ObservedFailure {
        fn complete<'a>(
            &'a mut self,
            _request: ModelRequest<'a>,
            progress: ProgressReporter,
        ) -> ProviderFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut attempt = zevria_content::ResponseAttempt::new(test_profile());
                if self.kind == 0 {
                    attempt.response_id = Some("accepted-upstream".into());
                } else if self.kind == 1 {
                    attempt
                        .hosted_activity
                        .push(zevria_content::HostedActivity {
                            kind: zevria_content::HostedActivityKind::WebSearch,
                            item_id: None,
                            output_index: 0,
                            status: zevria_content::HostedActivityStatus::Searching,
                            action: None,
                        });
                } else {
                    attempt
                        .presentation
                        .push(zevria_content::AssistantPresentationPart {
                            source: zevria_content::AssistantSourceAddress {
                                output_index: 0,
                                part: zevria_content::AssistantPartIdentity::Tool,
                                item_id: None,
                            },
                            content: zevria_content::AssistantPresentationContent::NativeTool {
                                call_id: "partial-failed-call".into(),
                            },
                        });
                }
                attempt.close_uncommitted(zevria_content::ResponseAttemptOutcome::Failed);
                progress.collect_attempt(attempt);
                Err(
                    zevria_model::ModelInputTooLarge::pre_execution(anyhow::anyhow!(
                        "contradictory adapter proof"
                    ))
                    .into(),
                )
            })
        }
        fn reset(&mut self) {}
    }
    for kind in 0..3 {
        let (_directory, transcript) = test_transcript();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = ObservedFailure {
            kind,
            calls: calls.clone(),
        };
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000));
        let (events, mut receiver) = session_event_channel(128);
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
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "observed activity forbids a new upstream invocation"
        );
        assert!(collect_events(&mut receiver).await.iter().any(|event| matches!(event,SessionEvent::TurnFailed {error,..} if error.contains("refusing automatic replay"))));
        assert!(
            !engine
                .conversation
                .items()
                .iter()
                .any(|item| matches!(item, TranscriptItem::Compaction(_)))
        );
    }
}

#[tokio::test]
async fn rejection_recovery_stops_for_resumed_disarm_spent_predictive_budget_or_failed_summary() {
    use crate::session::kernel::records::{RecordChange, TurnAnchor};
    for case in ["resumed", "predictive", "summary_failure"] {
        let (_directory, mut transcript) = test_transcript();
        if case == "resumed" {
            transcript
                .rewrite(&[
                    TranscriptItem::SessionMode(SessionMode::Build),
                    TranscriptItem::Compaction(
                        zevria_model::CompactionCheckpoint::new(
                            zevria_model::CompactionTrigger::AutomaticPreTurn,
                            zevria_model::CompactionBackend::LocalSummary,
                            vec![zevria_model::OwnedModelRequestItem::message(Message::user(
                                "saved checkpoint summary",
                            ))],
                            vec![],
                        )
                        .unwrap(),
                    ),
                ])
                .unwrap();
        }
        let rejection = || -> anyhow::Result<ModelResponse> {
            Err(
                zevria_model::ModelInputTooLarge::pre_execution(anyhow::anyhow!(
                    "pre-execution token window"
                ))
                .into(),
            )
        };
        let responses = match case {
            "resumed" => vec![rejection()],
            "predictive" => vec![
                response(Message::assistant("checkpoint summary"), false),
                rejection(),
            ],
            _ => vec![rejection(), Err(anyhow::anyhow!("summary unavailable"))],
        };
        let mut engine = SessionEngine::new(
            ScriptedProvider::with_model_responses(responses),
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_compaction_policy(test_compaction_policy(1_000_000, 90, 20_000))
        .with_mode_management();
        if case == "predictive" {
            engine
                .commit(RecordChange::anchored(
                    TurnAnchor::Append,
                    vec![TranscriptItem::Message(Message::user(
                        "old compactable context",
                    ))],
                    SessionMode::Build,
                ))
                .unwrap();
            let policy = engine.policies.policy(SessionMode::Build).clone();
            engine.reconcile_before_dispatch(&policy).unwrap();
            engine.report_provider_usage(&policy, 900_001).unwrap();
        }
        if case == "resumed" {
            assert!(!engine.context.is_armed(ModelRole::Build));
        }
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
                .any(|event| matches!(event, SessionEvent::TurnFailed { .. })),
            "{case}"
        );
        assert_eq!(
            engine.provider.requests.lock().unwrap().len(),
            if case == "resumed" { 1 } else { 2 },
            "{case}"
        );
        assert_eq!(
            engine
                .conversation
                .items()
                .iter()
                .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
                .count(),
            if case == "summary_failure" { 0 } else { 1 },
            "{case}"
        );
        assert_eq!(engine.conversation.items().iter().filter(|item| matches!(item,TranscriptItem::IncompleteResponse(record) if record.input_rejection.is_some())).count(),1,"{case}");
    }
}

#[test]
fn tool_revisions_keep_inline_baselines_but_invalidate_folded_fixed_prefixes() {
    use crate::session::kernel::accounting::{ContextAccounting, ContextUsageTracker};
    for inline in [false, true] {
        let profile = test_profile();
        let shape = crate::session::kernel::state::RequestShapeFingerprint([7; 32]);
        let mut tracker = ContextUsageTracker::default();
        tracker.provider_reported(172_483, 100, shape);
        let mut context = ContextAccounting::default();
        context.usage.insert(profile.clone(), tracker);
        let tools = zevria_model::ToolDeclarations::new(vec![zevria_model::ToolDeclaration {
            name: "inspect".into(),
            description: "revised declaration".into(),
            parameters: json!({"type":"object","properties":{}}),
        }])
        .unwrap();
        let item = TranscriptItem::ToolDeclarations(
            zevria_transcript::ToolDeclarationRecord::new(
                zevria_transcript::ToolDeclarationPlacement::Additional,
                tools,
            )
            .unwrap(),
        );
        context.on_committed(&[item], |_| inline);
        assert_eq!(
            context.projected_estimate(&profile, shape, 100).is_some(),
            inline
        );
        assert_eq!(
            context.usage[&profile].last_provider_total,
            Some(172_483),
            "last response usage remains available independently"
        );
    }
}
