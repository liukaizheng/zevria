use super::*;

struct ModeSettings;
impl zevria_model::models::ModelSettingsService for ModeSettings {
    fn validate(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn save(
        &self,
        _: &str,
        _: ModelRole,
        _: &zevria_model::models::ModelSelection,
    ) -> anyhow::Result<String> {
        unreachable!("selection alone must not save model settings")
    }
}

fn root(items: Vec<TranscriptItem>) -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let (directory, mut writer) = test_transcript();
    let mut history = items;
    if !history
        .iter()
        .any(|item| matches!(item, TranscriptItem::SessionMode(_)))
    {
        zevria_transcript::transcript::install_session_mode(&mut history, SessionMode::Build)
            .unwrap();
    }
    writer.rewrite(&history).unwrap();
    let engine = SessionEngine::new(
        ScriptedProvider::new([Ok(Message::assistant("completed"))]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap()
    .with_mode_management();
    (directory, engine)
}

async fn select(
    engine: &mut SessionEngine<ScriptedProvider>,
    mode: SessionMode,
) -> ModeSelectionResult {
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Manage(ManagementCommand::SetMode {
                request_id: "selection".into(),
                mode,
            }),
            &events,
        )
        .await
        .unwrap();
    let SessionUpdate::Lifecycle(SessionEvent::ModeResult { request_id, result }) =
        receiver.try_recv().unwrap()
    else {
        panic!("selection must return only its correlated result");
    };
    assert_eq!(request_id, "selection");
    assert!(receiver.try_recv().is_err());
    result
}

#[tokio::test]
async fn selection_is_durable_model_inert_idempotent_and_does_not_allocate_turns() {
    let (_directory, mut engine) = root(Vec::new());
    let initial_turn = engine.next_turn_id;
    for mode in [SessionMode::Plan, SessionMode::Build] {
        let before_input = snapshot_model_input(engine.model_input()).unwrap();
        assert_eq!(
            select(&mut engine, mode).await,
            ModeSelectionResult::Accepted {
                mode,
                changed: true
            }
        );
        assert_eq!(engine.selected_mode(), mode);
        assert_eq!(engine.next_turn_id, initial_turn);
        assert_eq!(
            snapshot_model_input(engine.model_input()).unwrap(),
            before_input
        );
        assert!(engine.provider.requests.lock().unwrap().is_empty());
        assert_eq!(
            zevria_transcript::transcript::session_mode(
                &zevria_transcript::transcript::load(engine.conversation.path()).unwrap()
            )
            .unwrap(),
            Some(mode)
        );
        let modified = std::fs::metadata(engine.conversation.path())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            select(&mut engine, mode).await,
            ModeSelectionResult::Accepted {
                mode,
                changed: false
            }
        );
        assert_eq!(
            std::fs::metadata(engine.conversation.path())
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
    }
}

#[tokio::test]
async fn every_selection_restriction_applies_even_to_equal_modes() {
    for restriction in [
        "unsupported_profile",
        "read_only",
        "persistence_degraded",
        "busy",
        "plan_ready",
    ] {
        let (_, ready) = ready_plan_fixture();
        let (_directory, mut engine) = root(if restriction == "plan_ready" {
            ready
        } else {
            Vec::new()
        });
        match restriction {
            "unsupported_profile" => engine.capabilities.mode_management = false,
            "read_only" => {
                let items = engine.conversation.items().to_vec();
                let writer =
                    TranscriptWriter::read_only(engine.conversation.path().to_path_buf()).unwrap();
                engine.conversation = Conversation::new(writer);
                engine.conversation.adopt_persisted(items);
            }
            "persistence_degraded" => {
                // A rejected required append latches degraded persistence while
                // preserving the current authoritative branch.
                assert!(
                    engine
                        .conversation
                        .push_required(TranscriptItem::SessionMode(SessionMode::Plan))
                        .is_err()
                );
            }
            "busy" => {
                engine.enter_turn().unwrap();
            }
            "plan_ready" => {}
            _ => unreachable!(),
        }
        let before = engine.conversation.items().to_vec();
        let bytes = std::fs::read(engine.conversation.path()).unwrap();
        let mode = engine.selected_mode();
        assert!(
            matches!(select(&mut engine, mode).await, ModeSelectionResult::Rejected { code, .. } if code == restriction)
        );
        assert_eq!(engine.conversation.items(), before);
        assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn failed_mode_save_keeps_selection_history_and_pending_accounting() {
    let (_directory, mut engine) = root(Vec::new());
    let path = engine.conversation.path().to_path_buf();
    let backup = path.with_extension("backup");
    let before = engine.conversation.items().to_vec();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        matches!(select(&mut engine, SessionMode::Plan).await, ModeSelectionResult::Rejected { code, .. } if code == "save_failed")
    );
    assert_eq!(engine.selected_mode(), SessionMode::Build);
    assert_eq!(engine.conversation.items(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), bytes);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    assert!(matches!(
        select(&mut engine, SessionMode::Plan).await,
        ModeSelectionResult::Accepted { .. }
    ));
}

fn published_history(artifact: &PlanArtifact) -> Vec<TranscriptItem> {
    let mut start = ensemble_start_fixture(
        "published-mode",
        EnsembleWorkflow::Plan,
        "plan a saved selection",
    );
    start.agents.push(zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        ..start.agents[0].clone()
    });
    with_explicit_fixture_confirmations(vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::Started {
            id: artifact.version.id,
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("fixture reports"),
            agents: Vec::new(),
        }),
        TranscriptItem::Plan(PlanRecord::Published {
            artifact: artifact.clone(),
            provenance: PlanPublicationProvenance::Synthesized,
        }),
        TranscriptItem::Ensemble(EnsembleRecord::Completed {
            run_id: start.run_id,
        }),
    ])
}

fn workflow_histories() -> Vec<(Vec<TranscriptItem>, SessionMode)> {
    let (artifact, ready) = ready_plan_fixture();
    let started = vec![TranscriptItem::Plan(PlanRecord::Started {
        id: artifact.version.id,
    })];
    let published = published_history(&artifact);
    let mut resolved = ready.clone();
    resolved.push(TranscriptItem::Plan(PlanRecord::Resolved {
        id: artifact.version.id,
        artifact: Some(artifact),
        resolution: PlanResolution::ImplementedCurrent,
    }));
    vec![
        (Vec::new(), SessionMode::Build),
        (started, SessionMode::Plan),
        (ready, SessionMode::Plan),
        (published, SessionMode::Plan),
        (resolved, SessionMode::Build),
    ]
}

#[tokio::test]
async fn fallback_and_direct_build_submissions_respect_all_plan_states() {
    for (history, fallback) in workflow_histories() {
        let (directory, mut writer) = test_transcript();
        writer.rewrite(&history).unwrap();
        let legacy = SessionEngine::new(
            ScriptedProvider::new([]),
            ToolServer::new().run(),
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap();
        assert_eq!(legacy.selected_mode(), fallback);
        drop(legacy);
        drop(directory);
        let (_directory, mut engine) = root(history);
        let original = engine.plan_state().unwrap().clone();
        let ready = matches!(original, PlanWorkflowState::Ready { .. });
        let outcome = select(&mut engine, SessionMode::Build).await;
        assert_eq!(
            matches!(outcome, ModeSelectionResult::Rejected { .. }),
            ready
        );
        assert_eq!(
            engine.plan_state().unwrap(),
            &original,
            "selection cannot abandon or approve a Plan"
        );
        let before = engine.conversation.items().to_vec();
        let (events, _receiver) = session_event_channel(64);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "implement directly".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        if ready {
            assert_eq!(engine.conversation.items(), before);
            assert!(engine.provider.requests.lock().unwrap().is_empty());
        } else {
            assert_eq!(engine.selected_mode(), SessionMode::Build);
            let requests = engine.provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].model_role, ModelRole::Build);
            assert_eq!(
                requests[0].instructions,
                engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
            );
            if matches!(original, PlanWorkflowState::Planning { .. }) {
                assert!(!matches!(
                    engine.plan_state().unwrap(),
                    PlanWorkflowState::Planning { .. }
                ));
            }
        }
    }
}

#[tokio::test]
async fn plan_implementation_and_fresh_handoff_commit_ordinary_build_atomically() {
    for decision in [
        PlanDecision::ImplementCurrent,
        PlanDecision::ImplementFresh,
        PlanDecision::Revise,
    ] {
        let (artifact, history) = revising_plan_fixture();
        // Revision itself requires Ready; implementation also accepts retained artifacts.
        let history = if decision == PlanDecision::Revise {
            published_history(&artifact)
        } else {
            history
        };
        let (_directory, mut engine) = root(history);
        select(&mut engine, SessionMode::Plan).await;
        let (events, _receiver) = session_event_channel(64);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::ResolvePlan {
                    expected: artifact.version,
                    decision,
                }),
                &events,
            )
            .await
            .unwrap();
        let mode = if decision == PlanDecision::Revise {
            SessionMode::Plan
        } else {
            SessionMode::Build
        };
        assert_eq!(engine.selected_mode(), mode);
        assert_eq!(
            zevria_transcript::transcript::session_mode(
                &zevria_transcript::transcript::load(engine.conversation.path()).unwrap()
            )
            .unwrap(),
            Some(mode)
        );
        if decision == PlanDecision::ImplementCurrent {
            assert_eq!(
                engine.provider.requests.lock().unwrap()[0].instructions,
                engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
            );
        }
        if decision == PlanDecision::ImplementFresh {
            select(&mut engine, SessionMode::Plan).await;
            engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::ResolvePlan {
                        expected: artifact.version,
                        decision,
                    }),
                    &events,
                )
                .await
                .unwrap();
            assert_eq!(
                engine.selected_mode(),
                SessionMode::Build,
                "retry is still an ordinary Build action"
            );
        }
    }
    for fail in [false, true] {
        let (_directory, mut engine) = root(Vec::new());
        select(&mut engine, SessionMode::Plan).await;
        let (artifact, _) = ready_plan_fixture();
        let path = engine.conversation.path().to_path_buf();
        let backup = path.with_extension("backup");
        let before = engine.conversation.items().to_vec();
        if fail {
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        let (events, _receiver) = session_event_channel(64);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::StartFromPlan {
                    handoff: PlanHandoff::new(artifact, "source"),
                }),
                &events,
            )
            .await
            .unwrap();
        if fail {
            assert_eq!(engine.conversation.items(), before);
            assert_eq!(engine.selected_mode(), SessionMode::Plan);
            assert!(engine.provider.requests.lock().unwrap().is_empty());
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
        } else {
            assert_eq!(engine.selected_mode(), SessionMode::Build);
            assert!(
                engine
                    .conversation
                    .items()
                    .iter()
                    .any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { .. })))
            );
            assert_eq!(
                engine.provider.requests.lock().unwrap()[0].instructions,
                engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
            );
        }
    }
}

#[tokio::test]
async fn rejected_plan_decisions_never_commit_selection_or_handoff() {
    for decision in [
        PlanDecision::ImplementCurrent,
        PlanDecision::ImplementFresh,
        PlanDecision::Revise,
    ] {
        for stale in [false, true] {
            let (artifact, _) = ready_plan_fixture();
            let (_directory, mut engine) = root(published_history(&artifact));
            select(&mut engine, SessionMode::Plan).await;
            let path = engine.conversation.path().to_path_buf();
            let backup = path.with_extension("backup");
            let before = engine.conversation.items().to_vec();
            let bytes = std::fs::read(&path).unwrap();
            if !stale {
                std::fs::rename(&path, &backup).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            let (events, _receiver) = session_event_channel(64);
            let mut expected = artifact.version;
            if stale {
                expected.revision += 1;
            }
            engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::ResolvePlan { expected, decision }),
                    &events,
                )
                .await
                .unwrap();
            assert_eq!(engine.selected_mode(), SessionMode::Plan);
            assert_eq!(engine.conversation.items(), before);
            assert!(engine.provider.requests.lock().unwrap().is_empty());
            if !stale {
                std::fs::remove_dir(&path).unwrap();
                std::fs::rename(&backup, &path).unwrap();
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}

#[tokio::test]
async fn accepted_skills_and_transcript_edits_commit_their_explicit_mode() {
    for command in [
        SessionCommand::Turn(TurnCommand::InvokeSkill {
            name: "review".parse().unwrap(),
            args: "check mode".into(),
            mode: SessionMode::Build,
        }),
        prompt_message_edit(0, "edited in Build", SessionMode::Build),
    ] {
        let (_directory, mut engine) = root(vec![
            TranscriptItem::SessionMode(SessionMode::Plan),
            TranscriptItem::Message(Message::user("old prompt")),
        ]);
        engine.tools = test_skill_tools();
        let (events, _receiver) = session_event_channel(64);
        engine.handle_command(command, &events).await.unwrap();
        assert_eq!(engine.selected_mode(), SessionMode::Build);
        assert_eq!(
            zevria_transcript::transcript::session_mode(
                &zevria_transcript::transcript::load(engine.conversation.path()).unwrap()
            )
            .unwrap(),
            Some(SessionMode::Build)
        );
        assert_eq!(
            engine.provider.requests.lock().unwrap()[0].instructions,
            engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
        );
    }
}

#[tokio::test]
async fn modes_keep_distinct_policy_identity_capacity_and_skill_permissions() {
    let (_directory, mut engine) = root(Vec::new());
    engine.policies.policy_mut(SessionMode::Plan).instructions =
        "large orchestrate guidance ".repeat(2000);
    let build_policy = engine.policies.policy(SessionMode::Build).clone();
    let orchestrate_policy = engine.policies.policy(SessionMode::Plan).clone();
    assert_ne!(
        engine.request_shape(&build_policy).unwrap(),
        engine.request_shape(&orchestrate_policy).unwrap()
    );
    assert_ne!(
        engine.logical_request_identity(&build_policy).unwrap(),
        engine
            .logical_request_identity(&orchestrate_policy)
            .unwrap()
    );
    let build = engine.prospective_skill_overhead(&build_policy, engine.active_skills().unwrap());
    let orchestrate =
        engine.prospective_skill_overhead(&orchestrate_policy, engine.active_skills().unwrap());
    assert!(orchestrate > build + 1000);
    let initial = engine.context_tokens().unwrap();
    let before = engine.restored_model_contexts().unwrap();
    engine = engine.with_model_management(Arc::new(ModeSettings), "revision".into());
    let management = engine.capabilities.models.as_mut().unwrap();
    management.preview = Some(zevria_model::models::ModelSelectionPreview {
        request_id: "old-preview".into(),
        session_generation: management.context.session_generation.clone(),
        generation: management.context.generation,
        mode: SessionMode::Build,
        scope: zevria_model::models::ModelSelectionScope::SessionOnly,
        target: zevria_model::models::ModelSelection::new(
            test_profile(),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        source: zevria_model::models::ModelSelection::new(
            test_profile(),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        revision: "revision".into(),
        reason: "conversion".into(),
    });
    engine.context.input_count = TurnInputCountState::Prepared {
        turn_id: TurnId::new(77),
        role: ModelRole::Build,
        identity: engine.request_shape(&build_policy).unwrap(),
        tokens: 1,
    };
    select(&mut engine, SessionMode::Plan).await;
    assert_eq!(engine.context.input_count, TurnInputCountState::Empty);
    assert!(engine.context.last_snapshots.is_empty());
    assert!(
        engine
            .capabilities
            .models
            .as_ref()
            .unwrap()
            .preview
            .is_none()
    );
    assert!(engine.context_tokens().unwrap() > initial + 1000);
    let after = engine.restored_model_contexts().unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after
            .iter()
            .filter(|snapshot| snapshot.model_role == ModelRole::Build)
            .count(),
        1
    );
    assert_eq!(
        after[0].projected_input_tokens,
        before[0].projected_input_tokens
    );
    for mode in SessionMode::ALL {
        assert_eq!(
            engine.policies.policy(mode).skills_enabled,
            mode != SessionMode::Plan
        );
        assert_eq!(
            zevria_model::models::mode_role(mode),
            if mode == SessionMode::Plan {
                ModelRole::Plan
            } else {
                ModelRole::Build
            }
        );
    }
    engine = engine
        .with_skill_management(Arc::new(TestSkillManagement), [true, false])
        .unwrap();
    assert!(engine.policies.policy(SessionMode::Build).skills_enabled);
    assert!(!engine.policies.policy(SessionMode::Plan).skills_enabled);
}
