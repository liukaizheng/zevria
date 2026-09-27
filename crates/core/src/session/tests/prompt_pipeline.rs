//! Work counts around real async acceptance, not just synchronous preparation.
use super::*;
use crate::session::pipeline_probe;
use zevria_transcript::replay_probe::measure_instruction_replay_async;

fn fixture() -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let (directory, writer) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap()
    .with_mode_management()
    .with_subtask_concurrency(1)
    .with_compaction_policy(test_compaction_policy(20_000, 80, 0));
    (directory, engine)
}

fn input(skill: bool, text: impl Into<zevria_content::UserPrompt>) -> PromptTurnInput {
    if skill {
        PromptTurnInput::Skill {
            name: "commit".parse().unwrap(),
            args: text.into(),
        }
    } else {
        PromptTurnInput::Message {
            text: text.into(),
            behavior: zevria_foundation::RequestBehavior::Standard,
        }
    }
}

fn committed_request(engine: &SessionEngine<ScriptedProvider>) -> CapturedRequest {
    let policy = engine.policies.policy(SessionMode::Build);
    CapturedRequest::of(&ModelRequest {
        instructions: &engine.rendered_instructions(policy),
        input: engine.conversation.model_input(),
        model_role: policy.model_role,
        allowed_tool_names: policy.allowed_tool_names.as_deref(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_prepares_once_validates_once_and_reuses_the_exact_counted_request() {
    // Include longer retained prefixes and every former admission benchmark kind.
    for size in [4, 1000] {
        for edit in [false, true] {
            for (skill, reapply) in [(false, false), (true, false), (true, true)] {
                let (_dir, mut engine) = fixture();
                if reapply {
                    let prepared = engine
                        .prepare_test_prompt(
                            TurnAnchor::Append,
                            input(true, "first"),
                            SessionMode::Build,
                        )
                        .unwrap();
                    engine.record_required_items(prepared.records).unwrap();
                }
                engine
                    .record_required_items(
                        (0..size)
                            .map(|i| {
                                TranscriptItem::Message(if i % 2 == 0 {
                                    Message::user("retained")
                                } else {
                                    Message::assistant("answer")
                                })
                            })
                            .collect(),
                    )
                    .unwrap();
                let end = engine.conversation.items().len() - 2;
                let anchor = if edit {
                    TurnAnchor::ReplaceFrom(end)
                } else {
                    TurnAnchor::Append
                };
                let prefix = snapshot_model_input(if edit {
                    zevria_transcript::model_input(&engine.conversation.items()[..end])
                } else {
                    engine.conversation.model_input()
                })
                .unwrap();
                engine.provider = ScriptedProvider::new([Ok(Message::assistant("done"))])
                    .with_input_counts([Ok(InputTokenCount::Exact(42))]);
                let (events, mut receiver) = session_event_channel(64);
                let turn =
                    TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
                let ((result, work), replay) =
                    measure_instruction_replay_async(pipeline_probe::measure(async {
                        tokio::task::yield_now().await;
                        engine
                            .prepare_and_accept_prompt(
                                anchor,
                                input(skill, "next ".repeat(20_000)),
                                SessionMode::Build,
                                &events,
                                &turn,
                            )
                            .await
                    }))
                    .await;
                let accepted = result.unwrap();
                assert_eq!(work.instruction_snapshots, 1);
                assert_eq!(work.instruction_updates, if skill { 2 } else { 1 });
                assert_eq!(work.prompt_preparations, 1);
                assert_eq!(work.plan_prefix_replays, usize::from(edit));
                assert_eq!(work.projections, 1);
                assert_eq!(work.measurements, 1);
                assert_eq!(work.replay_installations, 1);
                assert_eq!(replay.proposals, 1);
                assert_eq!(replay.validations, 1);
                assert_eq!(replay.full_replays, 1 + usize::from(edit));
                assert_eq!(
                    replay.full_records,
                    engine.conversation.items().len() + if edit { end } else { 0 }
                );
                assert_eq!(replay.pin_replays, 0);
                assert_eq!(replay.suffix_applications, 1);
                assert_eq!(replay.header_scans, 0);
                assert!(!accepted.compaction_attempted_for_first_dispatch);
                let counted = engine.provider.counted_requests.lock().unwrap()[0].clone();
                assert_eq!(counted, committed_request(&engine));
                assert!(counted.input.starts_with(&prefix));
                assert_eq!(
                    zevria_transcript::load(engine.conversation.path()).unwrap(),
                    engine.conversation.items()
                );
                let acceptance = collect_events(&mut receiver).await;
                assert!(matches!(
                    acceptance.as_slice(),
                    [
                        SessionEvent::ModeChanged { .. },
                        SessionEvent::TurnStarted { .. }
                    ]
                ));
                let _ = engine.run_turn(&accepted, &events, &turn).await;
                assert_eq!(engine.provider.input_count_calls.load(Ordering::SeqCst), 1);
                assert_eq!(engine.provider.requests.lock().unwrap()[0], counted);
            }
        }
    }
}

#[tokio::test]
async fn failed_required_write_never_installs_replay_or_publishes_acceptance() {
    let (_dir, mut engine) = fixture();
    let path = engine.conversation.path().to_path_buf();
    let backup = path.with_extension("backup");
    let before = engine.conversation.items().to_vec();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let ((result, work), replay) = measure_instruction_replay_async(pipeline_probe::measure(
        engine.prepare_and_accept_prompt(
            TurnAnchor::Append,
            input(true, "must not commit"),
            SessionMode::Build,
            &events,
            &turn,
        ),
    ))
    .await;
    assert!(matches!(result, Err(Rejection::Rejected(_))));
    assert_eq!(replay.proposals, 1);
    assert_eq!(replay.validations, 1);
    assert_eq!(work.replay_installations, 0);
    assert!(engine.active_skills().unwrap().is_empty());
    assert_eq!(engine.conversation.items(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), bytes);
    assert!(collect_events(&mut receiver).await.is_empty());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    engine
        .prepare_and_accept_prompt(
            TurnAnchor::Append,
            input(true, "retry succeeds"),
            SessionMode::Build,
            &events,
            &turn,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn unchanged_unsupported_or_failed_counts_are_not_repeated_when_checkpoint_is_unavailable() {
    for failed in [false, true] {
        let (_dir, mut engine) = fixture();
        engine.provider = ScriptedProvider::new([]).with_input_counts([if failed {
            Err(anyhow::anyhow!("temporary count failure"))
        } else {
            Ok(InputTokenCount::Unsupported)
        }]);
        let (events, mut receiver) = session_event_channel(64);
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        let ((result, work), replay) = measure_instruction_replay_async(pipeline_probe::measure(
            engine.prepare_and_accept_prompt(
                TurnAnchor::Append,
                input(false, "too large ".repeat(20_000)),
                SessionMode::Build,
                &events,
                &turn,
            ),
        ))
        .await;
        assert!(
            matches!(result, Err(Rejection::Rejected(ref reason)) if reason.contains("prompt was not committed"))
        );
        assert_eq!(work.measurements, 1);
        assert_eq!(work.projections, 1);
        assert_eq!(work.replay_installations, 0);
        assert_eq!(replay.proposals, 0);
        assert_eq!(engine.provider.input_count_calls.load(Ordering::SeqCst), 1);
        assert!(engine.conversation.items().is_empty());
        assert!(collect_events(&mut receiver).await.is_empty());
        if failed {
            assert_eq!(
                engine.context.input_count,
                TurnInputCountState::Failed { turn_id: turn.id }
            );
        }
    }
}

#[tokio::test]
async fn checkpoint_versions_reuse_preparation_and_identity_with_one_remeasurement() {
    for edit in [false, true] {
        for rejected in [false, true] {
            let (_dir, mut engine) = fixture();
            engine
                .record_required_items(vec![
                    TranscriptItem::Message(Message::user("earlier request")),
                    TranscriptItem::Message(Message::assistant("earlier answer")),
                    TranscriptItem::Message(Message::user("retained request")),
                    TranscriptItem::Message(Message::assistant("retained answer")),
                    TranscriptItem::Message(Message::user("discard on edit")),
                    TranscriptItem::Message(Message::assistant("discard answer")),
                ])
                .unwrap();
            let before = engine.conversation.items().to_vec();
            let bytes = std::fs::read(engine.conversation.path()).unwrap();
            let anchor = if edit {
                TurnAnchor::ReplaceFrom(4)
            } else {
                TurnAnchor::Append
            };
            engine.provider = ScriptedProvider::new([Ok(Message::assistant("compact summary"))])
                .with_input_counts([
                    Ok(InputTokenCount::Exact(21_000)),
                    Ok(InputTokenCount::Exact(if rejected { 21_000 } else { 42 })),
                ]);
            let (events, mut receiver) = session_event_channel(64);
            let turn =
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
            let ((result, work), replay) = measure_instruction_replay_async(
                pipeline_probe::measure(engine.prepare_and_accept_prompt(
                    anchor,
                    input(true, "next ".repeat(20_000)),
                    SessionMode::Build,
                    &events,
                    &turn,
                )),
            )
            .await;
            assert_eq!(work.prompt_preparations, 1);
            assert_eq!(work.instruction_snapshots, 1);
            assert_eq!(work.instruction_updates, 2);
            assert_eq!(work.plan_prefix_replays, usize::from(edit));
            assert_eq!(work.measurements, 2);
            assert_eq!(work.projections, 2);
            let commits = usize::from(!edit) + usize::from(!rejected);
            assert_eq!(work.replay_installations, commits);
            assert_eq!(replay.proposals, commits);
            assert_eq!(replay.validations, commits);
            assert_eq!(replay.full_replays, commits + usize::from(edit));
            assert_eq!(replay.suffix_applications, 1);
            assert_eq!(replay.header_scans, 0);
            assert_eq!(engine.provider.input_count_calls.load(Ordering::SeqCst), 2);
            {
                let counts = engine.provider.counted_requests.lock().unwrap();
                assert_eq!(counts.len(), 2);
                assert_eq!(counts[0].instructions, counts[1].instructions);
                assert_eq!(counts[0].allowed_tool_names, counts[1].allowed_tool_names);
                let suffix = |request: &CapturedRequest| {
                    request
                        .input
                        .iter()
                        .rev()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    suffix(&counts[0]),
                    suffix(&counts[1]),
                    "invocation, boundary identity, and body directive are not regenerated"
                );
                if rejected {
                    assert!(matches!(result, Err(Rejection::Rejected(_))));
                    assert!(engine.active_skills().unwrap().is_empty());
                    if edit {
                        assert_eq!(engine.conversation.items(), before);
                        assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
                    } else {
                        assert!(matches!(
                            engine.conversation.items().last(),
                            Some(TranscriptItem::Compaction(_))
                        ));
                    }
                } else {
                    assert!(result.unwrap().compaction_attempted_for_first_dispatch);
                    assert_eq!(counts[1], committed_request(&engine));
                }
            }
            let published = collect_events(&mut receiver).await;
            let started = published
                .iter()
                .position(|event| matches!(event, SessionEvent::TurnStarted { .. }));
            assert_eq!(started.is_none(), rejected);
            if let Some(started) = started {
                let compacted = published
                    .iter()
                    .position(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
                    .unwrap();
                let mode = published
                    .iter()
                    .position(|event| matches!(event, SessionEvent::ModeChanged { .. }))
                    .unwrap();
                assert!(compacted < mode && mode < started);
            }
        }
    }
}

#[tokio::test]
async fn editing_away_last_typed_boundary_still_prepares_a_standard_reset() {
    let (_dir, mut engine) = fixture();
    let initial = engine
        .prepare_test_prompt(
            TurnAnchor::Append,
            input(false, "first"),
            SessionMode::Build,
        )
        .unwrap();
    engine.record_required_items(initial.records).unwrap();
    engine.capabilities.subtask_concurrency = None;
    assert!(
        engine
            .instruction_replay()
            .unwrap()
            .has_request_boundaries()
    );
    let (events, _receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let accepted = engine
        .prepare_and_accept_prompt(
            TurnAnchor::ReplaceFrom(0),
            input(false, "replacement"),
            SessionMode::Build,
            &events,
            &turn,
        )
        .await
        .unwrap();
    assert_eq!(
        accepted.request.unwrap().behavior,
        zevria_foundation::RequestBehavior::Standard
    );
    assert!(
        engine
            .instruction_replay()
            .unwrap()
            .has_request_boundaries()
    );
}

#[tokio::test]
async fn rebuild_discards_usage_and_prior_exact_but_keeps_the_failed_count_latch() {
    for outcome in ["exact", "unsupported", "failed"] {
        let (_dir, mut engine) = fixture();
        engine
            .record_required_items(vec![
                TranscriptItem::Message(Message::user("old request")),
                TranscriptItem::Message(Message::assistant("old answer")),
            ])
            .unwrap();
        let policy = engine.policies.policy(SessionMode::Build).clone();
        engine.report_provider_usage(&policy, 21_000).unwrap();
        engine.provider = ScriptedProvider::new([Ok(Message::assistant("short summary"))])
            .with_input_counts([
                match outcome {
                    "exact" => Ok(InputTokenCount::Exact(17_000)),
                    "unsupported" => Ok(InputTokenCount::Unsupported),
                    _ => Err(anyhow::anyhow!("transient count failure")),
                },
                Ok(InputTokenCount::Unsupported),
            ]);
        let (events, _receiver) = session_event_channel(64);
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        let (result, work) = pipeline_probe::measure(engine.prepare_and_accept_prompt(
            TurnAnchor::Append,
            input(false, "short next request"),
            SessionMode::Build,
            &events,
            &turn,
        ))
        .await;
        assert!(result.unwrap().compaction_attempted_for_first_dispatch);
        assert_eq!(work.measurements, 2);
        assert_eq!(work.instruction_snapshots, 1);
        assert_eq!(
            engine.provider.input_count_calls.load(Ordering::SeqCst),
            if outcome == "exact" { 2 } else { 1 }
        );
        assert_eq!(
            engine.context.input_count,
            if outcome == "failed" {
                TurnInputCountState::Failed { turn_id: turn.id }
            } else {
                TurnInputCountState::Empty
            },
            "old exact result cannot survive an Unsupported rebuild"
        );
        assert!(
            engine
                .provider
                .counted_requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .input
                .len()
                >= 2
        );
    }
}

#[tokio::test]
async fn image_detection_uses_the_active_projection_not_compacted_history() {
    let (_dir, mut engine) = fixture();
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let prompt =
        zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(image)]).unwrap();
    engine
        .record_required(TranscriptItem::Message(prompt.to_message()))
        .unwrap();
    let checkpoint = CompactionCheckpoint::new(
        CompactionTrigger::AutomaticPreTurn,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user(
            "text summary",
        ))],
        vec![],
    )
    .unwrap();
    engine
        .record_required(TranscriptItem::Compaction(checkpoint))
        .unwrap();
    engine.reset_after_history_replacement().unwrap();
    let (events, _receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let (result, work) = pipeline_probe::measure(engine.prepare_and_accept_prompt(
        TurnAnchor::Append,
        input(false, "text only"),
        SessionMode::Build,
        &events,
        &turn,
    ))
    .await;
    assert!(result.is_ok());
    assert_eq!(work.measurements, 1);
    assert_eq!(engine.provider.input_count_calls.load(Ordering::SeqCst), 0);
}

struct LaunchCapability;
impl Tool for LaunchCapability {
    const NAME: &'static str = LAUNCH_SUBTASKS_TOOL_NAME;
    type Args = NoArgs;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "admission-only launch capability".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, _: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
        unreachable!()
    }
}

#[tokio::test]
async fn orchestration_is_checked_once_before_any_persistence_repair() {
    let (_dir, mut engine) = fixture();
    engine.tools = ToolServer::new().tool(LaunchCapability).run();
    engine.policies.policy_mut(SessionMode::Build).orchestration = true;
    let path = engine.conversation.path().to_path_buf();
    let backup = path.with_extension("backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        engine
            .record_completed_items(vec![
                TranscriptItem::Message(Message::user("completed request")),
                TranscriptItem::Message(Message::assistant("completed answer")),
            ])
            .is_err()
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert!(engine.conversation.persistence_error().is_some());
    let bytes = std::fs::read(&path).unwrap();
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    for supported in [false, true] {
        engine.capabilities.subtask_concurrency = Some(if supported { 2 } else { 1 });
        let (result, work) = pipeline_probe::measure(engine.prepare_and_accept_prompt(
            TurnAnchor::Append,
            PromptTurnInput::Message {
                text: "delegate independent work".into(),
                behavior: zevria_foundation::RequestBehavior::Orchestrate,
            },
            SessionMode::Build,
            &events,
            &turn,
        ))
        .await;
        assert_eq!(work.orchestration_checks, 1);
        assert_eq!(work.prompt_preparations, usize::from(supported));
        if !supported {
            assert!(matches!(result, Err(Rejection::Rejected(_))));
            assert!(engine.conversation.persistence_error().is_some());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(collect_events(&mut receiver).await.is_empty());
        } else {
            assert!(result.is_ok());
            assert!(engine.conversation.persistence_error().is_none());
            assert!(matches!(
                collect_events(&mut receiver).await.first(),
                Some(SessionEvent::PersistenceChanged { error: None, .. })
            ));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_probe_scopes_do_not_mix_interleaved_tasks() {
    async fn count(
        iterations: usize,
    ) -> (
        pipeline_probe::PipelineCounts,
        zevria_transcript::replay_probe::InstructionReplayCounts,
    ) {
        let (((), work), replay) =
            measure_instruction_replay_async(pipeline_probe::measure(async move {
                for _ in 0..iterations {
                    tokio::task::yield_now().await;
                    pipeline_probe::record(|counts| counts.projections += 1);
                    zevria_transcript::InstructionReplayState::replay(&[]).unwrap();
                }
            }))
            .await;
        (work, replay)
    }
    let (first, second) = tokio::join!(tokio::spawn(count(7)), tokio::spawn(count(13)));
    for ((work, replay), expected) in [(first.unwrap(), 7), (second.unwrap(), 13)] {
        assert_eq!(work.projections, expected);
        assert_eq!(replay.full_replays, expected);
    }
}
