use super::*;
use zevria_instructions::DirectivePayload;
use zevria_instructions::skill::SkillCatalog;
use zevria_instructions::skill::SkillPromptCatalog;
use zevria_instructions::skill::SkillsConfig;

fn engine(responses: usize) -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let (dir, writer) = test_transcript();
    let skills = test_skills();
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    (
        dir,
        SessionEngine::new(
            ScriptedProvider::new((0..responses).map(|_| Ok(Message::assistant("done")))),
            tools,
            test_policies(),
            writer,
            skills,
        )
        .unwrap(),
    )
}
fn catalog(engine: &SessionEngine<impl ModelProvider>) -> Option<SkillPromptCatalog> {
    engine
        .instruction_set(engine.policies.policy(engine.selected_mode()))
        .catalog
}
fn instructions(engine: &SessionEngine<impl ModelProvider>) -> String {
    engine.rendered_instructions(engine.policies.policy(engine.selected_mode()))
}
async fn submit(engine: &mut SessionEngine<ScriptedProvider>, text: &str, mode: SessionMode) {
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: text.into(),
                mode,
            }),
            &events,
        )
        .await
        .unwrap();
}
async fn invoke(engine: &mut SessionEngine<ScriptedProvider>) {
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::InvokeSkill {
                name: "commit".parse().unwrap(),
                args: "changes".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn complete_catalog_is_disclosed_or_rejected_without_disabling_management() {
    use zevria_instructions::skill::SkillEnableRule;
    use zevria_instructions::skill::SkillManagementRequest;
    use zevria_instructions::skill::SkillManagementResult;
    use zevria_instructions::skill::SkillManagementService;
    struct ConfigOnly;
    impl SkillManagementService for ConfigOnly {
        fn update<'a>(
            &'a self,
            request: SkillManagementRequest,
            installed: Arc<SkillCatalog>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<Arc<SkillCatalog>>> + Send + 'a>,
        > {
            Box::pin(async move {
                let SkillManagementRequest::SetEnabled { name, enabled, .. } = request else {
                    anyhow::bail!("expected enablement");
                };
                let mut config = installed.config().clone();
                config.rules.push(SkillEnableRule { name, enabled });
                Ok(Arc::new(installed.as_ref().clone().with_config(config)?))
            })
        }
    }
    let installed = Arc::new(
        SkillCatalog::new((0..32).map(|i| {
            zevria_instructions::SkillDefinition::new(
                format!("demo-{i:02}").parse().unwrap(),
                "Complete matching description ".repeat(32),
                "Body",
                zevria_instructions::SkillSource::Programmatic(format!("fixture-{i}")),
            )
            .unwrap()
        }))
        .unwrap(),
    );
    for fits in [true, false] {
        let (_directory, writer) = test_transcript();
        let path = writer.path().to_path_buf();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new(if fits {
                vec![Ok(Message::assistant("done"))]
            } else {
                vec![]
            }),
            test_skill_tools(),
            test_policies(),
            writer,
            installed.clone(),
        )
        .unwrap()
        .with_skill_management(Arc::new(ConfigOnly), [true, false])
        .unwrap()
        .with_compaction_policy(test_compaction_policy(
            if fits { 64_000 } else { 2_000 },
            80,
            0,
        ));
        let before = engine.conversation.items().to_vec();
        let bytes = std::fs::read(&path).unwrap();
        let (events, mut receiver) = session_event_channel(128);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "an ordinary request".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        if fits {
            let catalog = catalog(&engine).unwrap();
            assert_eq!(catalog.entries.len(), 32);
            assert_eq!(catalog.entries.first().unwrap().name.as_str(), "demo-00");
            assert_eq!(catalog.entries.last().unwrap().name.as_str(), "demo-31");
            assert_eq!(engine.provider.requests.lock().unwrap().len(), 1);
            continue;
        }
        assert_eq!(engine.conversation.items(), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(engine.active_skills().unwrap().is_empty());
        assert!(engine.provider.requests.lock().unwrap().is_empty());
        assert!(collect_events(&mut receiver).await.iter().any(|e| matches!(e,
            SessionEvent::TurnRejected { error, .. } if error.contains("irreducible") && error.contains("disable skills through management"))));
        engine
            .handle_command(
                SessionCommand::Manage(ManagementCommand::Skills {
                    request_id: "full".into(),
                    request: SkillManagementRequest::List {
                        query: String::new(),
                    },
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(
            collect_events(&mut receiver)
                .await
                .iter()
                .any(|e| matches!(e,
            SessionEvent::SkillsResult { result: SkillManagementResult::View { view }, .. }
                if view.entries.len() == 32 && view.completions.len() == 32))
        );
        engine
            .handle_command(
                SessionCommand::Manage(ManagementCommand::Skills {
                    request_id: "disable".into(),
                    request: SkillManagementRequest::SetEnabled {
                        expected_revision: engine.skills.catalog.revision().into(),
                        name: "demo-00".parse().unwrap(),
                        enabled: false,
                    },
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(
            collect_events(&mut receiver)
                .await
                .iter()
                .any(|e| matches!(e,
            SessionEvent::SkillsResult { result: SkillManagementResult::Changed { counts, .. }, .. }
                if counts.candidates == 32 && counts.enabled_names == 31))
        );
        assert_eq!(
            engine
                .management_skill_context()
                .unwrap()
                .completions()
                .len(),
            31
        );
    }
}

#[tokio::test]
async fn skill_catalog_lifecycle_reconciles_direct_reload_disable_and_mode_changes() {
    let (_dir, mut engine) = engine(10);
    let before = instructions(&engine);
    assert!(catalog(&engine).is_some());
    submit(&mut engine, "unrelated task", SessionMode::Build).await;
    assert_eq!(instructions(&engine), before);
    assert!(
        engine.active_skills().unwrap().is_empty(),
        "visibility never activates"
    );
    submit(&mut engine, "another task", SessionMode::Build).await;
    assert_eq!(instructions(&engine), before);
    invoke(&mut engine).await;
    assert_eq!(instructions(&engine), before);
    assert_eq!(
        engine
            .conversation
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Directive(_)))
            .count(),
        1
    );
    let pin = engine
        .active_skills()
        .unwrap()
        .snapshots()
        .map(|s| s.digest())
        .collect::<Vec<_>>();
    let replacement = SkillCatalog::new([zevria_instructions::SkillDefinition::new(
        "commit".parse().unwrap(),
        "Changed source description",
        "Changed body",
        zevria_instructions::SkillSource::Programmatic("reload".into()),
    )
    .unwrap()])
    .unwrap();
    engine = engine
        .with_skill_catalog(Arc::new(replacement.clone()))
        .unwrap();
    submit(&mut engine, "after reload", SessionMode::Build).await;
    assert_eq!(
        catalog(&engine).unwrap().entries[0].description,
        "Changed source description"
    );
    let enabled = engine.skills.catalog.clone();
    engine = engine
        .with_skill_catalog(Arc::new(
            replacement
                .with_config(SkillsConfig {
                    enabled: false,
                    rules: vec![],
                })
                .unwrap(),
        ))
        .unwrap();
    submit(&mut engine, "disabled", SessionMode::Build).await;
    assert!(!catalog(&engine).unwrap().enabled);
    assert!(catalog(&engine).unwrap().entries.is_empty());
    assert!(
        !engine
            .directive_state()
            .unwrap()
            .snapshot()
            .directives
            .iter()
            .any(|d| matches!(d.payload, DirectivePayload::SkillBody { .. }))
    );
    engine = engine.with_skill_catalog(enabled).unwrap();
    submit(&mut engine, "reenabled", SessionMode::Build).await;
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    submit(&mut engine, "plan", SessionMode::Plan).await;
    assert!(catalog(&engine).is_none());
    submit(&mut engine, "orchestrate", SessionMode::Build).await;
    assert!(catalog(&engine).unwrap().enabled);
    assert_eq!(
        engine
            .active_skills()
            .unwrap()
            .snapshots()
            .map(|s| s.digest())
            .collect::<Vec<_>>(),
        pin
    );
    let snapshot = engine.directive_state().unwrap().snapshot();
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.directive_state().unwrap().snapshot(), snapshot);
    assert_eq!(engine.active_skills().unwrap().len(), 1);
    let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
    assert_eq!(loaded, conversation_records(engine.conversation.items()));
}

#[tokio::test]
async fn skill_catalog_resume_edit_and_compaction_preserve_recorded_boundaries() {
    let (_dir, mut original) = engine(2);
    submit(&mut original, "ordinary", SessionMode::Build).await;
    invoke(&mut original).await;
    assert_eq!(original.active_skills().unwrap().len(), 1);
    let path = original.conversation.path().to_path_buf();
    let items = zevria_transcript::transcript::load(&path).unwrap();
    let skills = Arc::new(SkillCatalog::default());
    let mut resumed = SessionEngine::new(
        ScriptedProvider::new((0..4).map(|_| Ok(Message::assistant("done")))),
        ToolServer::new()
            .tool(SkillStubTool {
                calls: Arc::new(Mutex::new(Vec::new())),
            })
            .run(),
        test_policies(),
        TranscriptWriter::append_to(path).unwrap(),
        skills,
    )
    .unwrap()
    .with_transcript_items(items)
    .unwrap();
    assert!(
        catalog(&resumed).unwrap().entries.is_empty(),
        "resume renders the installed catalog independently of historical pins"
    );
    let (events, _receiver) = session_event_channel(128);
    resumed
        .handle_command(
            prompt_message_edit(1, "replace the direct invocation", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();
    assert!(resumed.active_skills().unwrap().is_empty());
    let empty = catalog(&resumed).unwrap();
    assert!(empty.enabled && empty.entries.is_empty());
    let snapshot = resumed.directive_state().unwrap().snapshot();
    resumed
        .handle_command(
            SessionCommand::Turn(TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let checkpoint = resumed.conversation.latest_compaction().unwrap().1;
    assert_eq!(resumed.directive_state().unwrap().snapshot(), snapshot);
    assert!(
        checkpoint
            .replacement_history
            .iter()
            .all(|item| !matches!(item, OwnedModelRequestItem::DeveloperInstruction(_)))
    );
    {
        let requests = resumed.provider.requests.lock().unwrap();
        let maintenance = requests.last().unwrap();
        assert!(
            !maintenance
                .input
                .iter()
                .any(|item| matches!(item, OwnedModelRequestItem::DeveloperInstruction(_)))
        );
        assert!(
            maintenance
                .instructions
                .ends_with("## Eligible skills\nSkill selection is unavailable.")
        );
    }
    let before = instructions(&resumed);
    submit(&mut resumed, "continue", SessionMode::Build).await;
    assert_eq!(instructions(&resumed), before);
    assert_eq!(catalog(&resumed).unwrap(), empty);
    zevria_transcript::SessionReplayError::validate(resumed.conversation.items()).unwrap();
}

#[test]
fn skill_catalog_requires_advertised_registered_activation_and_preserves_worker_profiles() {
    let (_dir, engine) = engine(0);
    for policy in [
        TurnPolicy::new("Plan", Some(vec!["command".into()]), ModelRole::Plan, true),
        TurnPolicy::new("Explore", None, ModelRole::Explore, false),
        TurnPolicy::new("Builder", None, ModelRole::Builder, false),
        TurnPolicy::new("Worker", None, ModelRole::Plan, false),
    ] {
        assert!(
            engine
                .rendered_instructions(&policy)
                .ends_with("## Eligible skills\nSkill selection is unavailable.")
        );
    }
    let disabled = engine
        .skills
        .catalog
        .as_ref()
        .clone()
        .with_config(SkillsConfig {
            enabled: false,
            rules: vec![],
        })
        .unwrap();
    let engine = engine.with_skill_catalog(Arc::new(disabled)).unwrap();
    assert!(
        instructions(&engine).contains("Skill selection is unavailable"),
        "a skill-capable root discloses global disablement on its first request"
    );
    let (_dir, writer) = test_transcript();
    let no_tool = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap();
    assert!(
        instructions(&no_tool).ends_with("## Eligible skills\nSkill selection is unavailable.")
    );
}

#[tokio::test]
async fn skill_catalog_prospective_capacity_uses_identical_full_instruction_state() {
    let (_dir, mut engine) = engine(0);
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    let prospective =
        ActiveSkills::from_snapshots([engine.skills.catalog.get("commit").unwrap().snapshot()])
            .unwrap();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let gate = PlanSubmissionGate::inert(None);
    let scope = ToolExecutionScope::new(&engine, SessionMode::Build, &policy, &turn, &gate);
    assert_eq!(
        scope
            .instructions
            .as_ref()
            .unwrap()
            .prepare_updates(scope.instruction_state.as_ref().unwrap(), &prospective)
            .unwrap()
            .instruction_tokens
            + scope.fixed_tokens,
        engine.prospective_skill_overhead(&policy, &prospective)
    );
    let updates = engine
        .reconcile_instruction_state(engine.directive_state().unwrap(), &policy, &prospective)
        .unwrap();
    assert_eq!(updates.len(), 1);
    assert!(
        updates
            .iter()
            .any(|d| matches!(d.payload, DirectivePayload::SkillBody { .. }))
    );
    // Exhausting the full prospective state limit rejects before any pin.
    let limit = engine.prospective_skill_overhead(&policy, &prospective);
    engine.compaction = test_compaction_policy(limit, 100, 0);
    let scope = ToolExecutionScope::new(&engine, SessionMode::Build, &policy, &turn, &gate);
    let calls = assistant_tool_calls(&Message::Assistant {
        id: None,
        content: vec![named_tool_call("one", "skill", json!({"skill":"commit"}))],
    });
    let batch = execute_tool_calls(&scope, &calls, ActiveSkills::default(), &gate).await;
    assert!(batch.skill_applications.is_empty());
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Error);
}

#[test]
fn explicit_activation_accounts_only_for_body_before_admission() {
    use zevria_instructions::skill::SkillInvocationPolicy;
    use zevria_instructions::skill::SkillMetadata;
    let description = "Detailed matching metadata ".repeat(45);
    let definition = zevria_instructions::SkillDefinition::new(
        "explicit".parse().unwrap(),
        &description,
        "Short pinned body",
        zevria_instructions::SkillSource::Programmatic("fixture".into()),
    )
    .unwrap()
    .with_metadata(
        SkillMetadata {
            invocation_policy: SkillInvocationPolicy::ExplicitOnly,
            ..SkillMetadata::new(description)
        },
        None,
    )
    .unwrap();
    let prospective = ActiveSkills::from_snapshots([definition.snapshot()]).unwrap();
    let (_dir, engine) = engine(0);
    let mut engine = engine
        .with_skill_catalog(Arc::new(SkillCatalog::new([definition.clone()]).unwrap()))
        .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    assert_eq!(catalog(&engine).unwrap().entries.len(), 0);
    let limit = engine.prospective_skill_overhead(&policy, &prospective);
    let body_only = engine.prospective_skill_overhead(&policy, &ActiveSkills::default())
        + zevria_model::compaction::approximate_tokens(
            &zevria_instructions::DirectiveContent::skill(&definition.snapshot()).text,
        )
        + 16;
    assert_eq!(
        body_only, limit,
        "explicit-only activation must not change catalog metadata"
    );
    engine.compaction = test_compaction_policy(limit, 100, 0);
    let before = engine.conversation.items().to_vec();
    assert!(
        engine
            .prepare_test_prompt(
                TurnAnchor::Append,
                PromptTurnInput::Skill {
                    name: "explicit".parse().unwrap(),
                    args: "apply".into()
                },
                SessionMode::Build,
            )
            .is_err()
    );
    assert_eq!(engine.conversation.items(), before);
    assert!(engine.active_skills().unwrap().is_empty());
}

struct SynthesisCountProvider {
    instructions: String,
    input: Vec<OwnedModelRequestItem>,
}
impl ModelProvider for SynthesisCountProvider {
    fn complete<'a>(&'a mut self, _: ModelRequest<'a>, _: ProgressReporter) -> ProviderFuture<'a> {
        Box::pin(async { anyhow::bail!("count-only fixture") })
    }
    fn reset(&mut self) {}
    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            self.instructions = request.instructions.into();
            self.input = snapshot_model_input(request.input)?;
            Ok(InputTokenCount::Exact(100))
        })
    }
}

#[tokio::test]
async fn synthesis_preflight_includes_the_same_instructions_and_revocation_as_dispatch() {
    let (_dir, mut original) = engine(1);
    invoke(&mut original).await;
    let items = original.conversation.items().to_vec();
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let (_dir, mut writer) = test_transcript();
        persist_fixture(&mut writer, &items);
        let mut engine = SessionEngine::new(
            SynthesisCountProvider {
                input: Vec::new(),
                instructions: String::new(),
            },
            original.tools.clone(),
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap()
        .with_transcript_items(items.clone())
        .unwrap();
        let mut start = ensemble_start_fixture("preflight", workflow, "synthesize reports");
        if workflow == EnsembleWorkflow::Plan {
            start.agents.push(zevria_workflow::AgentRunDescriptor {
                id: AgentRunId::new(),
                ..start.agents[0].clone()
            });
        }
        let message = Message::user("reports ready");
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, CancellationToken::new());
        engine
            .preflight_synthesis_message(&start, &message, &[], &turn)
            .await
            .unwrap();
        let counted = engine.provider.input.clone();
        engine
            .record_required(TranscriptItem::Message(message))
            .unwrap();
        engine
            .reconcile_before_dispatch(&engine.ensemble_policy(workflow))
            .unwrap();
        assert_eq!(counted, snapshot_model_input(engine.model_input()).unwrap());
        assert_eq!(
            engine.provider.instructions,
            engine.rendered_instructions(&engine.ensemble_policy(workflow))
        );
        assert!(
            engine
                .provider
                .instructions
                .ends_with("## Eligible skills\nSkill selection is unavailable.")
        );
        assert!(counted.iter().any(|item| matches!(item, OwnedModelRequestItem::DeveloperInstruction(d) if matches!(d.payload, DirectivePayload::SkillRevocation { .. }))));
    }
}

struct BoundaryTool<const INDEX: usize>(Arc<AtomicUsize>);
impl<const INDEX: usize> Tool for BoundaryTool<INDEX> {
    const NAME: &'static str = [
        "command",
        "edit",
        "launch_subtasks",
        "submit_plan",
        "skill_read",
    ][INDEX];
    type Args = serde_json::Value;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Must not execute a mixed batch".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, _: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("should not execute".into())
    }
}

#[tokio::test]
async fn mixed_skill_batches_dispatch_nothing_even_concurrent_subtasks_or_plan_tools() {
    let dispatched = Arc::new(AtomicUsize::new(0));
    let skills = test_skills();
    let skill_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: skill_calls.clone(),
        })
        .tool(BoundaryTool::<0>(dispatched.clone()))
        .tool(BoundaryTool::<1>(dispatched.clone()))
        .tool(BoundaryTool::<2>(dispatched.clone()))
        .tool(BoundaryTool::<3>(dispatched.clone()))
        .tool(BoundaryTool::<4>(dispatched.clone()))
        .run();
    let (_dir, writer) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        writer,
        skills,
    )
    .unwrap();
    for mode in SessionMode::ALL {
        for name in [
            "command",
            "edit",
            "launch_subtasks",
            "submit_plan",
            "skill_read",
        ] {
            for reverse in [false, true] {
                for cancelled in [false, true] {
                    let cancellation = CancellationToken::new();
                    if cancelled {
                        cancellation.cancel();
                    }
                    let turn = TurnContext::new(TurnId::new(1), mode, cancellation);
                    let gate = PlanSubmissionGate::inert(None);
                    let scope = ToolExecutionScope::new(
                        &engine,
                        mode,
                        engine.policies.policy(mode),
                        &turn,
                        &gate,
                    );
                    let mut calls = assistant_tool_calls(&Message::Assistant {
                        id: None,
                        content: vec![
                            named_tool_call("activate", "skill", json!({"skill":"commit"})),
                            named_tool_call("other", name, json!({})),
                        ],
                    });
                    if reverse {
                        calls.reverse();
                    }
                    let batch =
                        execute_tool_calls(&scope, &calls, ActiveSkills::default(), &gate).await;
                    assert!(batch.skill_applications.is_empty() && batch.candidate.is_none());
                    assert_eq!(batch.metadata.len(), 2);
                    for (metadata, call) in batch.metadata.iter().zip(&calls) {
                        assert_eq!(metadata.id, call.id.as_str());
                        assert_eq!(
                            metadata.outcome,
                            if cancelled {
                                ToolCallOutcome::Cancelled
                            } else {
                                ToolCallOutcome::Denied
                            }
                        );
                    }
                    if !cancelled {
                        assert!(
                            serde_json::to_string(&batch.message)
                                .unwrap()
                                .contains("skill-only response")
                        );
                    }
                }
            }
        }
    }
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    assert!(skill_calls.lock().unwrap().is_empty());
}
