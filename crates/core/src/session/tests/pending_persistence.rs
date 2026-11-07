use super::*;

fn pending_engine() -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::allocate(directory.path()).unwrap();
    let engine = SessionEngine::new(
        ScriptedProvider::new([Err(anyhow::anyhow!("provider failed after acceptance"))]),
        test_skill_tools(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap()
    .with_mode_management();
    (directory, engine)
}

fn input(skill: bool, text: impl Into<zevria_content::UserPrompt>) -> PromptTurnInput {
    if skill {
        PromptTurnInput::Skill {
            name: "review".parse().unwrap(),
            args: text.into(),
        }
    } else {
        PromptTurnInput::Message {
            text: text.into(),
            behavior: zevria_foundation::RequestBehavior::Standard,
        }
    }
}

#[tokio::test]
async fn pending_prompt_and_skill_acceptance_materialize_before_events_and_provider_failure() {
    for skill in [false, true] {
        for image in [false, true] {
            let (_directory, mut engine) = pending_engine();
            let path = engine.conversation.path().to_path_buf();
            let prompt = if image {
                zevria_content::UserPrompt::new(vec![zevria_content::PromptBlock::Image(
                    zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap(),
                )])
                .unwrap()
            } else {
                "first prompt".into()
            };
            assert!(!path.exists());
            let (events, mut receiver) = session_event_channel(64);
            let turn =
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
            let accepted = engine
                .prepare_and_accept_prompt(
                    TurnAnchor::Append,
                    input(skill, prompt),
                    SessionMode::Build,
                    &events,
                    &turn,
                )
                .await
                .unwrap();
            assert_eq!(
                zevria_transcript::load(&path).unwrap(),
                engine.conversation.items()
            );
            assert!(
                engine.provider.requests.lock().unwrap().is_empty(),
                "no generation during admission"
            );
            assert!(
                collect_events(&mut receiver)
                    .await
                    .iter()
                    .any(|event| matches!(event, SessionEvent::TurnStarted { .. }))
            );
            let _ = engine.run_turn(&accepted, &events, &turn).await;
            assert!(path.is_file());
            assert_eq!(
                zevria_transcript::load(&path).unwrap(),
                engine.conversation.items()
            );
            assert!(engine.conversation.has_real_user_prompt());
        }
    }
}

#[tokio::test]
async fn failed_first_publication_has_no_acceptance_or_dispatch_and_can_retry() {
    let (_directory, mut engine) = pending_engine();
    let path = engine.conversation.path().to_path_buf();
    std::fs::write(&path, b"competing history\n").unwrap();
    let before = engine.conversation.items().to_vec();
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let result = engine
        .prepare_and_accept_prompt(
            TurnAnchor::Append,
            input(true, "rejected"),
            SessionMode::Build,
            &events,
            &turn,
        )
        .await;
    assert!(matches!(result, Err(Rejection::Rejected(_))));
    assert_eq!(engine.conversation.items(), before);
    assert!(engine.active_skills().unwrap().is_empty());
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    assert!(collect_events(&mut receiver).await.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), b"competing history\n");
    assert!(!engine.conversation.ensure_durable().unwrap());
    std::fs::remove_file(&path).unwrap();
    assert!(!engine.conversation.ensure_durable().unwrap());
    assert!(!path.exists());
    engine
        .prepare_and_accept_prompt(
            TurnAnchor::Append,
            input(true, "accepted retry"),
            SessionMode::Build,
            &events,
            &turn,
        )
        .await
        .unwrap();
    assert_eq!(
        zevria_transcript::load(&path).unwrap(),
        engine.conversation.items()
    );
    assert_eq!(engine.active_skills().unwrap().len(), 1);
}

#[tokio::test]
async fn rejected_and_pre_cancelled_input_and_guidance_never_materialize() {
    let (_directory, mut engine) = pending_engine();
    let path = engine.conversation.path().to_path_buf();
    engine.refresh_application_guidance().unwrap();
    let (events, mut receiver) = session_event_channel(64);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, cancel);
    assert!(
        engine
            .prepare_and_accept_prompt(
                TurnAnchor::Append,
                input(false, "cancel before acceptance"),
                SessionMode::Build,
                &events,
                &turn,
            )
            .await
            .is_err()
    );
    assert!(!path.exists());
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::InvokeSkill {
                name: "missing".parse().unwrap(),
                args: "rejected".into(),
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
            .all(|event| !matches!(event, SessionEvent::TurnStarted { .. }))
    );
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                text: " \n\t ".into(),
                mode: SessionMode::Build,
                behavior: zevria_foundation::RequestBehavior::Standard,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(!engine.conversation.ensure_durable().unwrap());
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    assert!(!path.exists());
}

struct LaunchCapability;
impl Tool for LaunchCapability {
    const NAME: &'static str = LAUNCH_SUBTASKS_TOOL_NAME;
    type Args = NoArgs;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "admission-only fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, _: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
        panic!("admission must not dispatch tools")
    }
}

#[tokio::test]
async fn pending_orchestrated_acceptance_is_durable_before_dispatch_and_survives_cancellation() {
    let (_directory, mut engine) = pending_engine();
    engine.tools = ToolServer::new().tool(LaunchCapability).run();
    engine.policies.policy_mut(SessionMode::Build).orchestration = true;
    engine = engine.with_subtask_concurrency(2);
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let accepted = engine
        .prepare_and_accept_prompt(
            TurnAnchor::Append,
            PromptTurnInput::Message {
                text: "independent work".into(),
                behavior: zevria_foundation::RequestBehavior::Orchestrate,
            },
            SessionMode::Build,
            &events,
            &turn,
        )
        .await
        .unwrap();
    assert_eq!(
        zevria_transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
    assert!(
        matches!(&engine.conversation.items()[1], TranscriptItem::RequestPrompt { request, .. } if request.behavior == zevria_foundation::RequestBehavior::Orchestrate)
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnStarted { .. }))
    );
    turn.cancellation().cancel();
    let _ = engine.run_turn(&accepted, &events, &turn).await;
    assert!(engine.provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        zevria_transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn pending_plan_handoff_and_ensemble_start_publish_before_acceptance_effects() {
    for ensemble in [false, true] {
        let (_directory, mut engine) = pending_engine();
        let launcher = Arc::new(StubEnsembleLauncher::successful(["report"]));
        let launches = launcher.launches.clone();
        engine = engine.with_ensemble_launcher(launcher);
        let path = engine.conversation.path().to_path_buf();
        let (events, mut receiver) = session_event_channel(64);
        let handoff = PlanHandoff::new(ready_plan_fixture().0, "source-session");
        for collision in [true, false] {
            if collision {
                std::fs::write(&path, b"other history").unwrap();
            }
            let command = if ensemble {
                TurnCommand::RunEnsemble {
                    workflow: EnsembleWorkflow::Review,
                    prompt: "review".into(),
                }
            } else {
                TurnCommand::StartFromPlan {
                    handoff: handoff.clone(),
                }
            };
            engine
                .handle_command(SessionCommand::Turn(command), &events)
                .await
                .unwrap();
            let updates = collect_events(&mut receiver).await;
            if collision {
                assert_eq!(launches.load(Ordering::SeqCst), 0);
                assert!(engine.provider.requests.lock().unwrap().is_empty());
                assert!(matches!(
                    updates.as_slice(),
                    [SessionEvent::TurnRejected { .. }]
                ));
                assert_eq!(std::fs::read(&path).unwrap(), b"other history");
                assert!(!engine.conversation.ensure_durable().unwrap());
                std::fs::remove_file(&path).unwrap();
            } else {
                assert_eq!(launches.load(Ordering::SeqCst), usize::from(ensemble));
                assert_eq!(
                    zevria_transcript::load(&path).unwrap(),
                    engine.conversation.items()
                );
                assert!(updates.iter().any(|event| if ensemble {
                    matches!(event, SessionEvent::EnsembleStarted { .. })
                } else {
                    matches!(event, SessionEvent::PlanHandoffStarted { .. })
                }));
                assert!(!engine.conversation.has_real_user_prompt());
                assert!(engine.conversation.items().iter().any(|item| if ensemble {
                    matches!(
                        item,
                        TranscriptItem::Ensemble(EnsembleRecord::Started { .. })
                    )
                } else {
                    matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { .. }))
                }));
            }
        }
    }
}

#[test]
fn missing_existing_writer_is_not_treated_as_a_pending_new_session() {
    let (directory, writer) = test_transcript();
    let path = writer.path().to_path_buf();
    std::fs::remove_file(path).unwrap();
    assert!(
        SessionEngine::new(
            ScriptedProvider::new([]),
            test_skill_tools(),
            test_policies(),
            writer,
            test_skills(),
        )
        .is_err()
    );
    drop(directory);
}
