use super::*;

#[derive(Clone, Copy)]
enum Counting {
    Exact(usize),
    Unsupported,
    Failed,
}

struct PrefixProvider {
    catalog: bool,
    counting: Counting,
    context: ModelContextPolicy,
    responses: VecDeque<anyhow::Result<Message>>,
    requests: Vec<Vec<OwnedModelRequestItem>>,
    evaluated: Mutex<Vec<usize>>,
    counts: Vec<usize>,
    normal_counts: VecDeque<u64>,
    resets: usize,
    cancel: Option<CancellationToken>,
}

impl PrefixProvider {
    fn new(
        counting: Counting,
        responses: impl IntoIterator<Item = anyhow::Result<Message>>,
    ) -> Self {
        Self {
            catalog: true,
            counting,
            context: ModelContextPolicy {
                profile: test_profile(),
                context_window_tokens: 3500,
                input_token_limit: 3500,
                retained_user_tokens: 0,
            },
            responses: responses.into_iter().collect(),
            requests: Vec::new(),
            evaluated: Mutex::new(Vec::new()),
            counts: Vec::new(),
            normal_counts: VecDeque::new(),
            resets: 0,
            cancel: None,
        }
    }
}

impl ModelProvider for PrefixProvider {
    fn model_catalog(&self) -> Vec<zevria_model::models::ModelCandidate> {
        if self.catalog {
            vec![zevria_model::models::ModelCandidate {
                context: self.context.clone(),
                reasoning_levels: zevria_foundation::ReasoningLevel::ALL.to_vec(),
            }]
        } else {
            Vec::new()
        }
    }
    fn model_selection(&self, _: ModelRole) -> Option<zevria_model::models::ModelSelection> {
        Some(zevria_model::models::ModelSelection::new(
            self.context.profile.clone(),
            zevria_foundation::ReasoningLevel::Medium,
        ))
    }
    fn preflight_input(
        &self,
        _: &ModelProfileRef,
        input: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        if input.last().and_then(|item| item.message_ref())
            == Some(&Message::user(zevria_model::SUMMARIZATION_PROMPT))
        {
            self.evaluated.lock().unwrap().push(
                input
                    .iter()
                    .filter(|item| !matches!(item, ModelRequestItem::DeveloperInstruction(_)))
                    .count()
                    - 1,
            );
        }
        let tokens = input
            .iter()
            .filter(|item| !matches!(item, ModelRequestItem::DeveloperInstruction(_)))
            .count() as u64
            * 1000;
        Ok(zevria_model::models::ReplayPreflight::Compatible(
            ContextTokenEstimate::new(tokens, tokens),
        ))
    }
    fn count_profile<'a>(
        &'a mut self,
        _: &'a zevria_model::models::ModelSelection,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            assert_eq!(request.allowed_tool_names, Some([].as_slice()));
            if request.input.last().and_then(|item| item.message_ref())
                == Some(&Message::user(zevria_model::SUMMARIZATION_PROMPT))
            {
                zevria_model::maintenance::validate_maintenance_input(&request.input).unwrap();
            }
            let k = request
                .input
                .iter()
                .filter(|item| !matches!(item, ModelRequestItem::DeveloperInstruction(_)))
                .count()
                - 1;
            self.counts.push(k);
            match self.counting {
                Counting::Exact(max) => {
                    Ok(InputTokenCount::Exact(if k <= max { 100 } else { 3501 }))
                }
                Counting::Unsupported => Ok(InputTokenCount::Unsupported),
                Counting::Failed => anyhow::bail!("count endpoint unavailable"),
            }
        })
    }
    fn count_input_tokens<'a>(&'a mut self, _: ModelRequest<'a>) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            Ok(self
                .normal_counts
                .pop_front()
                .map(InputTokenCount::Exact)
                .unwrap_or(InputTokenCount::Unsupported))
        })
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.requests
                .push(snapshot_model_input(request.input.clone())?);
            if request.input.last().and_then(|item| item.message_ref())
                == Some(&Message::user(zevria_model::SUMMARIZATION_PROMPT))
            {
                zevria_model::maintenance::validate_maintenance_input(&request.input).unwrap();
            }
            if request.input.last().and_then(|item| item.message_ref())
                == Some(&Message::user(zevria_model::SUMMARIZATION_PROMPT))
            {
                assert_eq!(request.allowed_tool_names, Some([].as_slice()));
            }
            if let Some(cancellation) = &self.cancel {
                cancellation.cancel();
            }
            let response = self.responses.pop_front().expect("scripted completion")?;
            progress.stream_updated(response.clone());
            progress
                .usage_updated(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 1,
                    cached_tokens: 0,
                    total_tokens: 11,
                })
                .await;
            ModelResponse::plain(response)
        })
    }
    fn reset(&mut self) {
        self.resets += 1;
    }
}

fn engine(provider: PrefixProvider) -> (tempfile::TempDir, SessionEngine<PrefixProvider>) {
    let (directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(test_compaction_policy(3500, 50, 0));
    (directory, engine)
}
fn source(n: usize) -> Vec<OwnedModelRequestItem> {
    (0..n)
        .map(|i| OwnedModelRequestItem::message(Message::user(format!("source-{i}"))))
        .collect()
}
fn size_error() -> anyhow::Result<Message> {
    Err(anyhow::Error::new(ModelInputTooLarge::new(anyhow::anyhow!(
        "structured size diagnostic"
    )))
    .context("request-id wrapper"))
}
fn turn() -> TurnContext {
    TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new())
}

#[tokio::test]
async fn descending_admission_exact_fallback_and_full_success() {
    for (counting, expected_k) in [
        (Counting::Exact(4), 4),
        (Counting::Exact(2), 2),
        (Counting::Unsupported, 1),
        (Counting::Failed, 1),
    ] {
        let (_directory, mut engine) = engine(PrefixProvider::new(
            counting,
            [Ok(Message::assistant("summary"))],
        ));
        let source = source(4);
        let original = source.clone();
        let (events, mut receiver) = session_event_channel(32);
        let context = engine.provider.context.clone();
        let result = engine
            .summarize_prefix(
                &source,
                &zevria_instructions::InstructionSet::maintenance("", []),
                &context,
                test_policies().policy(SessionMode::Build),
                &events,
                &turn(),
            )
            .await
            .unwrap();
        assert_eq!(result, ("summary".into(), expected_k));
        assert_eq!(
            *engine.provider.evaluated.lock().unwrap(),
            (expected_k..=4).rev().collect::<Vec<_>>()
        );
        assert_eq!(engine.provider.requests.len(), 1);
        assert_eq!(
            &engine.provider.requests[0][..expected_k],
            &source[..expected_k]
        );
        assert_eq!(
            engine.provider.requests[0][expected_k],
            OwnedModelRequestItem::message(Message::user(zevria_model::SUMMARIZATION_PROMPT))
        );
        let history =
            zevria_model::compaction::summary_tail_history(&source, expected_k, &result.0).unwrap();
        assert_eq!(&history[1..], &original[expected_k..]);
        assert_eq!(source, original);
        assert_eq!(engine.provider.resets, 2);
        assert!(
            collect_events(&mut receiver).await.is_empty(),
            "synthetic usage and streams stay silent"
        );
        if matches!(counting, Counting::Failed) {
            assert_eq!(engine.provider.counts, [4]);
        }
    }
}

#[tokio::test]
async fn provider_size_retries_have_no_sixteen_candidate_cap_and_mix_with_local_rejections() {
    for catalog in [false, true] {
        let mut provider = PrefixProvider::new(
            Counting::Exact(19),
            (0..18)
                .map(|_| size_error())
                .chain([Ok(Message::assistant("smallest"))]),
        );
        provider.catalog = catalog;
        let (_directory, mut engine) = engine(provider);
        let source = source(if catalog { 21 } else { 19 });
        let context = engine.provider.context.clone();
        let (events, _) = session_event_channel(32);
        let (summary, k) = engine
            .summarize_prefix(
                &source,
                &zevria_instructions::InstructionSet::maintenance("", []),
                &context,
                test_policies().policy(SessionMode::Build),
                &events,
                &turn(),
            )
            .await
            .unwrap();
        assert_eq!((summary.as_str(), k), ("smallest", 1));
        assert_eq!(
            engine
                .provider
                .requests
                .iter()
                .map(|request| request
                    .iter()
                    .filter(|item| !matches!(item, OwnedModelRequestItem::DeveloperInstruction(_)))
                    .count()
                    - 1)
                .collect::<Vec<_>>(),
            (1..=19).rev().collect::<Vec<_>>()
        );
        assert_eq!(engine.provider.requests.len(), 19);
        assert_eq!(engine.provider.resets, 38);
    }
}

#[tokio::test]
async fn irreducible_empty_non_size_and_cancellation_preserve_history() {
    for case in ["local", "provider", "empty", "other", "cancel"] {
        let responses = match case {
            "provider" => vec![size_error(), size_error(), size_error()],
            "other" => vec![Err(anyhow::anyhow!(
                "authentication failed: context too large is incidental prose"
            ))],
            _ => vec![Ok(Message::assistant("summary"))],
        };
        let mut provider = PrefixProvider::new(
            Counting::Exact(if case == "local" { 0 } else { 3 }),
            responses,
        );
        let turn = turn();
        if case == "cancel" {
            provider.cancel = Some(turn.cancellation().clone());
        }
        let (_directory, mut engine) = engine(provider);
        let source = source(if case == "empty" { 0 } else { 3 });
        let context = engine.provider.context.clone();
        let (events, mut receiver) = session_event_channel(32);
        let error = engine
            .summarize_prefix(
                &source,
                &zevria_instructions::InstructionSet::maintenance("", []),
                &context,
                test_policies().policy(SessionMode::Build),
                &events,
                &turn,
            )
            .await
            .unwrap_err();
        if matches!(case, "local" | "provider") {
            assert!(
                error
                    .to_string()
                    .contains("no nonempty replay-safe summary prefix")
            );
        }
        if case == "provider" {
            assert!(format!("{error:#}").contains("structured size diagnostic"));
        }
        assert_eq!(
            engine.provider.requests.len(),
            match case {
                "local" | "empty" => 0,
                "provider" => 3,
                _ => 1,
            }
        );
        assert!(
            engine
                .conversation
                .items()
                .iter()
                .all(zevria_transcript::transcript::is_leading_metadata)
        );
        assert!(collect_events(&mut receiver).await.is_empty());
    }
}

#[tokio::test]
async fn unsafe_boundaries_are_skipped_without_counting_or_dispatch_and_malformed_sources_fail() {
    let mut source = vec![
        OwnedModelRequestItem::message(Message::user("oldest")),
        OwnedModelRequestItem::message(Message::Assistant {
            id: None,
            content: vec![tool_call("a", "one"), tool_call("b", "two")],
        }),
        OwnedModelRequestItem::message(Message::User {
            content: vec![
                UserContent::tool_result("a", "echo", vec![ToolResultContent::text("one")]),
                UserContent::tool_result("b", "echo", vec![ToolResultContent::text("two")]),
            ],
        }),
        OwnedModelRequestItem::message(Message::user("newest")),
    ];
    let (_directory, mut engine) = engine(PrefixProvider::new(
        Counting::Exact(2),
        [Ok(Message::assistant("summary"))],
    ));
    let context = engine.provider.context.clone();
    let (events, _) = session_event_channel(32);
    let (_, k) = engine
        .summarize_prefix(
            &source,
            &zevria_instructions::InstructionSet::maintenance("", []),
            &context,
            test_policies().policy(SessionMode::Build),
            &events,
            &turn(),
        )
        .await
        .unwrap();
    assert_eq!(k, 1);
    assert_eq!(engine.provider.counts, [4, 3, 1]);
    assert_eq!(*engine.provider.evaluated.lock().unwrap(), [4, 3, 1]);
    source.remove(2);
    let error = engine
        .summarize_prefix(
            &source,
            &zevria_instructions::InstructionSet::maintenance("", []),
            &context,
            test_policies().policy(SessionMode::Build),
            &events,
            &turn(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("incomplete exchange"));
    assert_eq!(engine.provider.requests.len(), 1);
}

#[tokio::test]
async fn partial_edited_prefix_is_installed_only_with_an_accepted_revision() {
    for fits in [true, false] {
        let mut provider = PrefixProvider::new(
            Counting::Exact(2),
            [
                Ok(Message::assistant("edited summary")),
                Ok(Message::assistant("revised answer")),
            ],
        );
        provider.normal_counts = [5000, if fits { 100 } else { 5000 }].into();
        let (_directory, mut engine) = engine(provider);
        for i in 0..3 {
            engine
                .conversation
                .push_required(TranscriptItem::Message(Message::user(
                    format!("old-user-{i} ").repeat(1000),
                )))
                .unwrap();
            engine
                .conversation
                .push_required(TranscriptItem::Message(Message::assistant(format!(
                    "old-answer-{i}"
                ))))
                .unwrap();
        }
        engine.reestimate_context_usage();
        let original = engine.conversation.items().to_vec();
        let original_disk = std::fs::read(engine.conversation.path()).unwrap();
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_command(
                prompt_message_edit(2, "accepted revision", SessionMode::Build),
                &events,
            )
            .await
            .unwrap();
        let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
        if fits {
            let checkpoint = loaded
                .iter()
                .find_map(|item| {
                    if let TranscriptItem::Compaction(c) = item {
                        Some(c)
                    } else {
                        None
                    }
                })
                .expect("accepted edit checkpoint");
            let source = snapshot_conversation_input(zevria_transcript::transcript::model_input(
                &original[..4],
            ))
            .unwrap();
            assert_eq!(&checkpoint.replacement_history[1..], &source[2..]);
            assert_eq!(&engine.provider.requests[0][..2], &source[..2]);
            assert_eq!(
                engine.provider.requests[1].last().unwrap(),
                &OwnedModelRequestItem::message(Message::user("accepted revision"))
            );
        } else {
            assert_eq!(engine.conversation.items(), original);
            assert_eq!(loaded, original);
            assert_eq!(
                std::fs::read(engine.conversation.path()).unwrap(),
                original_disk
            );
        }
        let emitted = collect_events(&mut receiver).await;
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
                .count(),
            usize::from(fits)
        );
    }
}

#[tokio::test]
async fn mid_turn_partial_checkpoint_preserves_a_complete_batch_even_if_dispatch_remains_blocked() {
    let mut provider = PrefixProvider::new(
        Counting::Exact(3),
        [
            Ok(Message::Assistant {
                id: None,
                content: vec![tool_call("a", "one"), tool_call("b", "two")],
            }),
            Ok(Message::assistant("mid-turn summary")),
        ],
    );
    provider.normal_counts = [100, 5000, 5000].into();
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
    .unwrap()
    .with_compaction_policy(test_compaction_policy(3500, 50, 0));
    for i in 0..2 {
        engine
            .conversation
            .push_required(TranscriptItem::Message(Message::user(
                format!("source-{i} ").repeat(1200),
            )))
            .unwrap();
    }
    engine.reestimate_context_usage();
    let (events, mut receiver) = session_event_channel(64);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "use tools".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
    let checkpoint_index = loaded
        .iter()
        .position(|item| matches!(item, TranscriptItem::Compaction(_)))
        .expect("mid-turn checkpoint");
    let TranscriptItem::Compaction(checkpoint) = &loaded[checkpoint_index] else {
        unreachable!()
    };
    assert_eq!(checkpoint.trigger, CompactionTrigger::AutomaticMidTurn);
    assert!(matches!(
        &loaded[checkpoint_index - 1],
        TranscriptItem::ToolResults { .. }
    ));
    let source = snapshot_conversation_input(zevria_transcript::transcript::model_input(
        &loaded[..checkpoint_index],
    ))
    .unwrap();
    assert_eq!(&checkpoint.replacement_history[1..], &source[3..]);
    assert_eq!(checkpoint.replacement_history.len(), 3);
    assert_eq!(engine.provider.counts, [5, 3]);
    assert_eq!(
        engine.provider.requests.len(),
        2,
        "no oversized normal dispatch"
    );
    let emitted = collect_events(&mut receiver).await;
    assert_eq!(
        emitted
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        1
    );
    assert!(emitted.iter().any(|event| matches!(event, SessionEvent::TurnFailed { error, .. } if error.contains("automatic compaction completed"))));
}

#[tokio::test]
async fn partial_manual_checkpoints_roundtrip_and_next_source_keeps_previous_tail_with_zero_retention()
 {
    let (_directory, mut engine) = engine(PrefixProvider::new(
        Counting::Exact(2),
        [
            Ok(Message::assistant("first summary")),
            Ok(Message::assistant("second summary")),
        ],
    ));
    for item in source(5) {
        engine
            .conversation
            .push_required(TranscriptItem::Message(item.message_ref().unwrap().clone()))
            .unwrap();
    }
    let original = snapshot_model_input(engine.conversation.model_input()).unwrap();
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let first = snapshot_model_input(engine.conversation.model_input()).unwrap();
    assert_eq!(&first[1..], &original[2..]);
    engine.provider.counting = Counting::Exact(4);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(&engine.provider.requests[1][..4], first.as_slice());
    let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
    let checkpoints = loaded
        .iter()
        .filter_map(|item| {
            if let TranscriptItem::Compaction(c) = item {
                Some(c)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 2);
    assert!(
        checkpoints
            .iter()
            .all(|c| c.retained_user_messages.is_empty())
    );
    assert_eq!(
        snapshot_model_input(zevria_transcript::transcript::model_input(&loaded)).unwrap(),
        snapshot_model_input(engine.conversation.model_input()).unwrap()
    );
    let emitted = collect_events(&mut receiver).await;
    assert_eq!(
        emitted
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        emitted
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn explore_partial_checkpoint_dispatch_or_capacity_failure_is_durable_without_recompaction() {
    for fits in [true, false] {
        let mut provider = PrefixProvider::new(
            Counting::Exact(2),
            [
                Ok(Message::assistant("Explore summary")),
                Ok(Message::assistant("Explore answer")),
            ],
        );
        provider.normal_counts = [5000, if fits { 100 } else { 5000 }].into();
        let (_directory, mut engine) = engine(provider);
        engine.policies = SessionPolicies::new(
            TurnPolicy::new(
                "Explore instructions",
                Some(Vec::new()),
                ModelRole::Explore,
                false,
            ),
            test_policies().policy(SessionMode::Plan).clone(),
        );
        for i in 0..4 {
            engine
                .conversation
                .push_required(TranscriptItem::Message(Message::user(
                    format!("source-{i} ").repeat(1200),
                )))
                .unwrap();
        }
        engine.reestimate_context_usage();
        let original = snapshot_model_input(engine.conversation.model_input()).unwrap();
        let (events, mut receiver) = session_event_channel(64);
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "next Explore request".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
        let checkpoint = loaded
            .iter()
            .find_map(|item| {
                if let TranscriptItem::Compaction(c) = item {
                    Some(c)
                } else {
                    None
                }
            })
            .expect("durable partial checkpoint");
        assert_eq!(&checkpoint.replacement_history[1..], &original[2..]);
        assert_eq!(engine.provider.requests.len(), if fits { 2 } else { 1 });
        if fits {
            assert_eq!(
                &engine.provider.requests[1][..3],
                &checkpoint.replacement_history
            );
        } else {
            assert_eq!(
                loaded.len(),
                5,
                "only original history and checkpoint persist"
            );
            assert!(
                !loaded
                    .iter()
                    .any(|item| item.message() == Some(&Message::user("next Explore request")))
            );
        }
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
        if !fits {
            assert!(emitted.iter().any(|event| matches!(event, SessionEvent::TurnRejected { error, .. } if error.contains("automatic compaction completed") && error.contains("3500-token input limit"))));
        }
    }
}
