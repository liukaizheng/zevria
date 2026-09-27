//! Replay, control, and counting invariants, including terminal live failures.
use super::*;
use zevria_model::models::ModelManagementRequest;
use zevria_model::models::ModelSelectionPreview;
use zevria_model::models::ModelSelectionScope;
use zevria_model::models::ModelSettingsService;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

fn engine() -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let (directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap();
    (directory, engine)
}

fn invalid_skill() -> TranscriptItem {
    TranscriptItem::SkillInvocation(SkillInvocation::new(
        "commit".parse().unwrap(),
        "",
        SkillApplication::Reapply("commit".parse().unwrap()),
    ))
}

fn invalid_plan() -> TranscriptItem {
    let (artifact, _) = revising_plan_fixture();
    TranscriptItem::Plan(PlanRecord::Ready { artifact })
}

#[test]
fn restoration_validates_both_domains_before_adoption_or_projection() {
    for items in [
        vec![invalid_skill()],
        vec![invalid_plan()],
        vec![invalid_skill(), invalid_plan()],
    ] {
        let (_directory, mut engine) = engine();
        engine
            .conversation
            .push_required_batch(items.clone())
            .unwrap();
        let path = engine.conversation.path().to_path_buf();
        let bytes = std::fs::read(&path).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let projection = workspace.path().join("plans");
        let expected = SessionReplayError::validate(&items).unwrap_err();
        assert!(match (&expected, items.len(), &items[0]) {
            (SessionReplayError::Both { skills, plan }, 2, _) =>
                !skills.is_empty() && !plan.is_empty(),
            (SessionReplayError::Skills(_), 1, TranscriptItem::SkillInvocation(_)) => true,
            (SessionReplayError::Plan(_), 1, TranscriptItem::Plan(_)) => true,
            _ => false,
        });
        let result = engine
            .with_plans_dir(projection.clone())
            .with_transcript_items(items);
        assert!(matches!(result, Err(error) if error == expected));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(!projection.exists());
    }
}

#[test]
fn early_instruction_failure_does_not_hide_later_skill_or_plan_errors() {
    let directive_failure = TranscriptItem::Message(Message::System {
        content: "raw authority is invalid".into(),
    });
    for tail in [
        vec![],
        vec![invalid_skill()],
        vec![invalid_plan()],
        vec![invalid_skill(), invalid_plan()],
    ] {
        let mut items = vec![directive_failure.clone()];
        items.extend(tail);
        let actual = SessionReplayError::validate(&items).unwrap_err();
        let skill_error = replay_active_skills(&items)
            .err()
            .map(|error| format!("{error:#}"));
        let plan_error = replay_plan_state(items.iter().filter_map(|item| match item {
            TranscriptItem::Plan(record) => Some(record),
            _ => None,
        }))
        .err()
        .map(|error| format!("{error:#}"));
        match (skill_error, plan_error) {
            (Some(skills), Some(plan)) => {
                assert_eq!(actual, SessionReplayError::Both { skills, plan })
            }
            (Some(skills), None) => assert_eq!(actual, SessionReplayError::Skills(skills)),
            (None, Some(plan)) => assert_eq!(actual, SessionReplayError::Plan(plan)),
            (None, None) => assert!(matches!(actual, SessionReplayError::Directives(_))),
        }
    }
}

#[cfg(feature = "test-support")]
#[test]
fn authoritative_replay_uses_one_reduction_and_failure_only_pin_fallback() {
    use zevria_transcript::replay_probe::measure_instruction_replay;
    let valid = vec![TranscriptItem::Message(Message::user("valid"))];
    let (result, counts) = measure_instruction_replay(|| SessionReplayError::validate(&valid));
    result.unwrap();
    assert_eq!(counts.full_replays, 1);
    assert_eq!(counts.pin_replays, 0);
    let invalid = vec![
        TranscriptItem::Message(Message::System {
            content: "invalid".into(),
        }),
        invalid_skill(),
    ];
    let (result, counts) = measure_instruction_replay(|| SessionReplayError::validate(&invalid));
    assert!(matches!(result, Err(SessionReplayError::Skills(_))));
    assert_eq!(counts.full_replays, 1);
    assert_eq!(counts.pin_replays, 1);
    assert_eq!(counts.pin_records, invalid.len());
}

#[tokio::test]
async fn failed_replay_has_no_payload_and_rejects_every_command_and_reseeding() {
    let (_directory, mut engine) = engine();
    let failure = engine
        .record_completed_items(vec![invalid_skill(), invalid_plan()])
        .unwrap_err();
    let error = failure.downcast::<SessionReplayError>().unwrap();
    assert!(matches!(engine.replay, SessionReplayState::Failed(_)));
    assert_eq!(engine.active_skills().unwrap_err(), error);
    assert_eq!(engine.plan_state().unwrap_err(), error);
    assert_eq!(engine.context_tokens().unwrap_err(), error);
    assert_eq!(engine.restored_model_contexts().unwrap_err(), error);
    for anchor in [TurnAnchor::Append, TurnAnchor::ReplaceFrom(0)] {
        assert!(matches!(
            engine.prepare_test_prompt(anchor, PromptTurnInput::Message { behavior: zevria_foundation::RequestBehavior::Standard, text: "cannot repair here".into() }, SessionMode::Build),
            Err(Rejection::Fatal(actual)) if actual == error
        ));
    }
    let path = engine.conversation.path().to_path_buf();
    let bytes = std::fs::read(&path).unwrap();
    let (artifact, _) = revising_plan_fixture();
    let commands = vec![
        SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "must not run".into(),
            mode: SessionMode::Build,
        }),
        prompt_message_edit(0, "cannot repair here", SessionMode::Build),
        SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
            name: "commit".parse().unwrap(),
            args: zevria_content::UserPrompt::default(),
            mode: SessionMode::Build,
        }),
        SessionCommand::Turn(crate::session::TurnCommand::Compact {
            mode: SessionMode::Build,
        }),
        SessionCommand::Turn(crate::session::TurnCommand::RunEnsemble {
            workflow: EnsembleWorkflow::Review,
            prompt: "must not run".into(),
        }),
        SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
            expected: artifact.version,
            decision: PlanDecision::Revise,
        }),
        SessionCommand::Turn(crate::session::TurnCommand::StartFromPlan {
            handoff: PlanHandoff::new(artifact, "source"),
        }),
        SessionCommand::Manage(crate::session::ManagementCommand::Models {
            request_id: "picker".into(),
            request: ModelManagementRequest::Cancel,
        }),
        SessionCommand::Manage(crate::session::ManagementCommand::Skills {
            request_id: "query".into(),
            request: zevria_instructions::skill::SkillManagementRequest::List {
                query: String::new(),
            },
        }),
        SessionCommand::Control(crate::session::ControlCommand::CancelTurn { turn_id: None }),
        SessionCommand::Control(crate::session::ControlCommand::Shutdown),
    ];
    let (events, mut receiver) = session_event_channel(1);
    let turn = TurnContext::new(TurnId::new(8), SessionMode::Build, CancellationToken::new());
    turn.cancellation().cancel();
    for command in commands {
        assert_eq!(
            engine
                .handle_command(command.clone(), &events)
                .await
                .unwrap_err(),
            error
        );
        if let SessionCommand::Turn(command) = command {
            assert_eq!(
                engine
                    .handle_turn(command, &turn, &events)
                    .await
                    .unwrap_err(),
                error
            );
        }
    }
    assert!(receiver.try_recv().is_err());
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(matches!(engine.with_history(Vec::new()), Err(actual) if actual == error));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn fatal_shutdown_never_repairs_invalid_in_memory_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    let writer = TranscriptWriter::create(&sessions).unwrap();
    let path = writer.path().to_path_buf();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap();
    engine
        .record_required(TranscriptItem::Message(Message::user("durable")))
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut blocker = TranscriptRewriteBlocker::new(&path).expect("block transcript replacement");
    let error = engine
        .record_completed_items(vec![
            invalid_plan(),
            TranscriptItem::Message(Message::assistant("completed")),
        ])
        .unwrap_err()
        .downcast::<SessionReplayError>()
        .unwrap();
    assert!(engine.conversation.persistence_error().is_some());
    blocker.restore().expect("restore transcript filename");
    let (_commands, receive) = mpsc::unbounded_channel();
    let (events, _receiver) = session_event_channel(1);
    assert_eq!(engine.run(receive, events).await.unwrap_err(), error);
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn invalid_uncommitted_edit_does_not_poison_or_mutate_current_history() {
    let (_directory, mut engine) = engine();
    engine
        .record_required(TranscriptItem::Message(Message::user("valid")))
        .unwrap();
    let bytes = std::fs::read(engine.conversation.path()).unwrap();
    assert!(
        engine
            .commit_anchored_records(
                TurnAnchor::ReplaceFrom(0),
                vec![invalid_plan()],
                SessionMode::Build
            )
            .is_err()
    );
    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
}

/// Repeated provider skill-call IDs are invalid committed replay. Cancelling
/// inside the second ready response tests fatal precedence over cancellation.
struct RepeatedSkillProvider {
    calls: usize,
    cancel_on_failure: bool,
}
impl ModelProvider for RepeatedSkillProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.calls += 1;
            assert!(
                self.calls <= 2,
                "invalid replay must stop before a third completion"
            );
            if self.calls == 2 && self.cancel_on_failure {
                progress.turn().cancellation().cancel();
            }
            ModelResponse::plain(Message::Assistant {
                id: None,
                content: vec![
                    named_tool_call(
                        "repeated",
                        SKILL_TOOL_NAME,
                        json!({"skill":"commit"})
                    );
                    if self.calls == 2 { 2 } else { 1 }
                ],
            })
        })
    }
    fn reset(&mut self) {}
}

#[tokio::test]
async fn live_committed_failure_wins_cancellation_without_synthetic_terminal_evidence() {
    for cancel_on_failure in [false, true] {
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            RepeatedSkillProvider {
                calls: 0,
                cancel_on_failure,
            },
            ToolServer::new().run(),
            test_policies(),
            transcript,
            test_skills(),
        )
        .unwrap();
        let (events, mut receiver) = session_event_channel(32);
        let error = engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "go".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SessionReplayError::Directives(_)));
        assert!(
            error
                .to_string()
                .contains("duplicate unresolved tool call id")
        );
        assert_eq!(engine.provider.calls, 2);
        assert!(matches!(engine.phase, EnginePhase::Idle));
        assert!(
            !engine
                .conversation
                .items()
                .iter()
                .any(|item| matches!(item, TranscriptItem::Error { .. }))
        );
        let events = collect_events(&mut receiver).await;
        assert!(!events.iter().any(|event| matches!(
            event,
            SessionEvent::TurnCompleted { .. }
                | SessionEvent::TurnFailed { .. }
                | SessionEvent::TurnCancelled { .. }
        )));
        let path = engine.conversation.path().to_path_buf();
        let bytes = std::fs::read(&path).unwrap();
        let (commands, receive) = mpsc::unbounded_channel();
        commands
            .send(SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "queued".into(),
                mode: SessionMode::Build,
            }))
            .unwrap();
        let (blocked, _receiver) = session_event_channel(1);
        blocked
            .try_send(SessionEvent::TurnCancelled {
                turn_id: TurnId::new(99),
            })
            .unwrap();
        let returned = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            engine.run(receive, blocked),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(returned, error);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

struct Settings;
impl ModelSettingsService for Settings {
    fn validate(&self, _revision: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn save(
        &self,
        _revision: &str,
        _role: ModelRole,
        _profile: &zevria_model::models::ModelSelection,
    ) -> anyhow::Result<String> {
        Ok("saved".into())
    }
}

#[tokio::test]
async fn control_transitions_preserve_management_but_never_restore_confirmation() {
    for managed in [false, true] {
        let (_directory, mut engine) = engine();
        if managed {
            engine = engine.with_model_management(Arc::new(Settings), "revision".into());
            engine
                .set_model_preview(ModelSelectionPreview {
                    request_id: "picker".into(),
                    generation: 0,
                    session_generation: engine
                        .model_management()
                        .unwrap()
                        .session_generation
                        .clone(),
                    mode: SessionMode::Build,
                    scope: ModelSelectionScope::SessionOnly,
                    target: zevria_model::models::ModelSelection::new(
                        test_profile(),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    source: zevria_model::models::ModelSelection::new(
                        test_profile(),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    revision: "revision".into(),
                    reason: "test".into(),
                })
                .unwrap();
            let stored = engine.model_preview().unwrap().clone();
            let (events, _receiver) = session_event_channel(4);
            engine
                .handle_command(
                    SessionCommand::Manage(crate::session::ManagementCommand::Models {
                        request_id: "another-picker".into(),
                        request: ModelManagementRequest::Cancel,
                    }),
                    &events,
                )
                .await
                .unwrap();
            assert_eq!(engine.model_preview(), Some(&stored));
        }
        let queries = engine.enter_turn().unwrap();
        assert!(engine.model_preview().is_none());
        assert_eq!(
            engine.model_management().map(|models| models.generation),
            managed.then_some(1)
        );
        engine.context.count_failures.insert(test_profile());
        engine.invalidate_model_preview();
        assert!(engine.context.count_failures.is_empty());
        assert!(
            matches!(&engine.phase, EnginePhase::Turn { skill_queries, .. } if Arc::ptr_eq(skill_queries, &queries))
        );
        engine
            .record_required_items(vec![TranscriptItem::SkillInvocation(SkillInvocation::new(
                SkillName::parse("commit").unwrap(),
                "test",
                SkillApplication::Activate(test_skills().get("commit").unwrap().snapshot()),
            ))])
            .unwrap();
        assert_eq!(queries.lock().unwrap().pins.len(), 1);
        engine.exit_turn();
        assert!(matches!(engine.phase, EnginePhase::Idle));
        assert_eq!(
            engine
                .model_management()
                .map(|models| models.revision.as_str()),
            managed.then_some("revision")
        );
    }
}

#[test]
fn prepared_counts_are_one_use_and_bound_to_turn_and_role() {
    let turn = TurnId::new(2);
    let mut state = TurnInputCountState::Prepared {
        turn_id: turn,
        role: ModelRole::Plan,
        identity: RequestShapeFingerprint([0; 32]),
        tokens: 42,
    };
    assert_eq!(
        state.take_prepared(turn, ModelRole::Build, RequestShapeFingerprint([0; 32])),
        None
    );
    assert_eq!(state, TurnInputCountState::Empty);
    state = TurnInputCountState::Prepared {
        turn_id: turn,
        role: ModelRole::Plan,
        identity: RequestShapeFingerprint([0; 32]),
        tokens: 42,
    };
    assert_eq!(
        state.take_prepared(turn, ModelRole::Plan, RequestShapeFingerprint([0; 32])),
        Some(42)
    );
    assert_eq!(
        state.take_prepared(turn, ModelRole::Plan, RequestShapeFingerprint([0; 32])),
        None
    );
    state = TurnInputCountState::Prepared {
        turn_id: turn,
        role: ModelRole::Build,
        identity: RequestShapeFingerprint([0; 32]),
        tokens: 7,
    };
    assert_eq!(
        state.take_prepared(
            TurnId::new(3),
            ModelRole::Build,
            RequestShapeFingerprint([0; 32])
        ),
        None
    );
}

#[tokio::test]
async fn cancellation_after_prompt_commit_preserves_count_until_the_next_measurement() {
    let (_directory, mut engine) = engine();
    engine.provider = ScriptedProvider::new([Ok(Message::assistant("done"))]).with_input_counts([
        Ok(InputTokenCount::Exact(400)),
        Err(anyhow::anyhow!("next count failed")),
    ]);
    engine.policies = test_policies_for_tools(&[]);
    engine = engine.with_compaction_policy(test_compaction_policy(2_000, 50, 0));
    let count_calls = engine.provider.input_count_calls.clone();
    let path = engine.conversation.path().to_path_buf();
    let first = TurnContext::new(
        TurnId::new(11),
        SessionMode::Build,
        CancellationToken::new(),
    );
    let (events, mut receiver) = session_event_channel(1);
    events
        .try_send(SessionEvent::TurnCancelled {
            turn_id: TurnId::new(0),
        })
        .unwrap();
    {
        let execution = engine.handle_turn(
            TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "x".repeat(2_400).into(),
                mode: SessionMode::Build,
            },
            &first,
            &events,
        );
        tokio::pin!(execution);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(execution.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains(&"x".repeat(2_400)),
            "prompt accepted before TurnStarted publication"
        );
        first.cancellation().cancel();
        receiver.try_recv().unwrap();
        let (result, ()) = tokio::join!(execution, async {
            while let Some(update) = receiver.recv().await {
                if matches!(update, SessionUpdate::Lifecycle(SessionEvent::TurnCancelled { turn_id }) if turn_id == first.id)
                {
                    break;
                }
            }
        });
        result.unwrap();
    }
    assert!(
        matches!(engine.context.input_count, TurnInputCountState::Prepared { turn_id, tokens: 400, .. } if turn_id == first.id)
    );
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    // This test isolates counting rather than asking for a summary of the
    // cancelled prompt. It fits the hard limit even after counting fails.
    engine.context.automatic_compaction_armed = [false; ModelRole::COUNT];
    let second = TurnContext::new(
        TurnId::new(12),
        SessionMode::Build,
        CancellationToken::new(),
    );
    let (events, _receiver) = session_event_channel(32);
    engine
        .handle_turn(
            TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "next".into(),
                mode: SessionMode::Build,
            },
            &second,
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine.context.input_count,
        TurnInputCountState::Failed { turn_id: second.id }
    );
    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(engine.provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_prepared_count_is_replaced_by_next_turn_failure_and_invalidation_keeps_suppression()
{
    let (_directory, mut engine) = engine();
    engine.provider = ScriptedProvider::new([]).with_input_counts([
        Err(anyhow::anyhow!("temporary count failure")),
        Ok(InputTokenCount::Exact(42)),
    ]);
    let calls = engine.provider.input_count_calls.clone();
    engine.context.input_count = TurnInputCountState::Prepared {
        turn_id: TurnId::new(1),
        role: ModelRole::Build,
        identity: RequestShapeFingerprint([0; 32]),
        tokens: 7,
    };
    let turn = TurnContext::new(TurnId::new(2), SessionMode::Build, CancellationToken::new());
    let prompt = Message::user("count");
    let request = || ModelRequest {
        instructions: "test instructions",
        input: vec![ModelRequestItem::message(&prompt)],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    assert!(
        count_input_tokens_for_turn(
            &mut engine.provider,
            &mut engine.context.input_count,
            request(),
            &turn
        )
        .await
        .is_err()
    );
    engine.reset_after_history_replacement().unwrap();
    let catalog = engine.skills.catalog.clone();
    engine = engine.with_skill_catalog(catalog).unwrap();
    assert_eq!(
        engine.context.input_count,
        TurnInputCountState::Failed { turn_id: turn.id }
    );
    assert_eq!(
        count_input_tokens_for_turn(
            &mut engine.provider,
            &mut engine.context.input_count,
            request(),
            &turn
        )
        .await
        .unwrap(),
        InputTokenCount::Unsupported
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = TurnContext::new(TurnId::new(3), SessionMode::Build, CancellationToken::new());
    assert_eq!(
        count_input_tokens_for_turn(
            &mut engine.provider,
            &mut engine.context.input_count,
            request(),
            &next
        )
        .await
        .unwrap(),
        InputTokenCount::Exact(42)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
