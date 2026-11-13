use super::*;
use zevria_model::ToolDeclaration;

struct DescribedCommand(&'static str);
impl Tool for DescribedCommand {
    const NAME: &'static str = "command";
    type Error = std::convert::Infallible;
    type Args = EchoArgs;
    type Output = String;
    fn description(&self) -> String {
        self.0.into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, _: &mut ToolContext, args: EchoArgs) -> Result<String, Self::Error> {
        Ok(args.value)
    }
}
fn registry(description: &'static str, writes: Arc<Mutex<Vec<String>>>) -> ToolServerHandle {
    ToolServer::new()
        .tool(WriteTestTool { calls: writes })
        .tool(DescribedCommand(description))
        .run()
}
fn policies() -> SessionPolicies {
    SessionPolicies::new(
        TurnPolicy::new(
            "Build policy",
            Some(vec!["command".into(), "write".into()]),
            ModelRole::Build,
            false,
        ),
        TurnPolicy::new(
            "Plan policy",
            Some(vec!["command".into()]),
            ModelRole::Plan,
            false,
        ),
    )
}
async fn submit(engine: &mut SessionEngine<ScriptedProvider>, mode: SessionMode, text: &str) {
    let (events, mut receiver) = session_event_channel(128);
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
    let events = collect_events(&mut receiver).await;
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
        )),
        "{events:?}"
    );
}
fn names(tools: &[ToolDeclaration]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}
fn records(engine: &SessionEngine<ScriptedProvider>) -> Vec<&ToolDeclarationRecord> {
    engine
        .conversation
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolDeclarations(record) => Some(record),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn mode_changes_append_once_in_registry_order_and_forbidden_declared_calls_are_denied() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("plan one")),
        Ok(Message::assistant("plan two")),
        Ok(Message::assistant("build one")),
        Ok(Message::assistant("build two")),
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "denied",
                "write",
                json!({"value":"must not execute"}),
            )]),
        )),
        Ok(Message::assistant("plan complete")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        registry("command v1", writes.clone()),
        policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    submit(&mut engine, SessionMode::Plan, "first").await;
    assert_eq!(records(&engine).len(), 1);
    assert_eq!(
        records(&engine)[0].placement,
        ToolDeclarationPlacement::Initial
    );
    assert_eq!(names(&records(&engine)[0].tools), ["command"]);
    submit(&mut engine, SessionMode::Plan, "second").await;
    assert_eq!(records(&engine).len(), 1);
    submit(&mut engine, SessionMode::Build, "build").await;
    assert_eq!(records(&engine).len(), 2);
    assert_eq!(
        records(&engine)[1].placement,
        ToolDeclarationPlacement::Additional
    );
    assert_eq!(names(&records(&engine)[1].tools), ["write"]);
    submit(&mut engine, SessionMode::Build, "more build").await;
    submit(&mut engine, SessionMode::Plan, "return to plan").await;
    assert_eq!(records(&engine).len(), 2);
    assert!(writes.lock().unwrap().is_empty());
    let requests = engine.provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| names(&request.tools) == ["command"])
    );
    assert!(requests[1].input.starts_with(&requests[0].input));
    assert!(requests[2].input.starts_with(&requests[1].input));
    assert!(
        matches!(requests[2].input.last(), Some(OwnedModelRequestItem::AdditionalTools(tools)) if names(tools) == ["write"])
    );
    let position = requests[2].input.len() - 1;
    assert!(
        requests[3..]
            .iter()
            .all(|request| request.input[position] == requests[2].input[position])
    );
    assert!(format!("{:?}", requests.last().unwrap().input).contains("unavailable in Plan mode"));
    assert_eq!(
        zevria_transcript::transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn flag_is_window_latched_and_disabled_windows_follow_policy_without_records() {
    for latched in [false, true] {
        let (_directory, writer) = test_transcript();
        let mut engine = SessionEngine::new(
            ScriptedProvider::new((0..4).map(|_| Ok(Message::assistant("reply")))),
            registry("command", Arc::new(Mutex::new(Vec::new()))),
            policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_incremental_tools(latched);
        submit(&mut engine, SessionMode::Plan, "plan").await;
        engine.incremental_tools = false;
        submit(&mut engine, SessionMode::Build, "build").await;
        let source = engine
            .compaction_source(
                engine.conversation.items(),
                engine.policies.policy(SessionMode::Build),
            )
            .unwrap();
        let (events, _) = session_event_channel(128);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Compact {
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(requests[2].instructions, requests[1].instructions);
        assert_eq!(requests[2].tools, requests[1].tools);
        assert_eq!(
            &requests[2].input[..source.request.input.len()],
            &source.request.input
        );
        assert_eq!(
            requests[2].allowed_tool_names,
            requests[1].allowed_tool_names
        );
        if latched {
            assert_eq!(names(&requests[1].tools), ["command"]);
            assert_eq!(records(&engine).len(), 2);
        } else {
            assert_eq!(names(&requests[1].tools), ["write", "command"]);
            assert!(records(&engine).is_empty());
        }
    }
}

#[tokio::test]
async fn resume_redefines_only_changed_tools_without_rewriting_initial_or_addition_positions() {
    let (_directory, writer) = test_transcript();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([Ok(Message::assistant("before resume"))]),
        registry("old description", writes.clone()),
        policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    submit(&mut engine, SessionMode::Build, "first").await;
    let path = engine.conversation.path().to_path_buf();
    let initial = records(&engine)[0].clone();
    drop(engine);
    let mut resumed = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("after resume")),
            Ok(Message::assistant("stable")),
        ]),
        registry("new description", writes),
        policies(),
        TranscriptWriter::append_to(path.clone()).unwrap(),
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    submit(&mut resumed, SessionMode::Build, "second").await;
    submit(&mut resumed, SessionMode::Build, "third").await;
    let declarations = records(&resumed);
    assert_eq!(declarations.len(), 2);
    assert_eq!(declarations[0], &initial);
    assert_eq!(names(&declarations[1].tools), ["command"]);
    assert_eq!(declarations[1].tools[0].description, "new description");
    let requests = resumed.provider.requests.lock().unwrap();
    assert_eq!(requests[0].tools, initial.tools);
    let position = requests[0]
        .input
        .iter()
        .position(|item| matches!(item, OwnedModelRequestItem::AdditionalTools(_)))
        .unwrap();
    assert_eq!(requests[0].input[position], requests[1].input[position]);
    let loaded = zevria_transcript::transcript::load(&path).unwrap();
    assert_eq!(
        snapshot_model_input(zevria_transcript::transcript::model_input(&loaded)).unwrap(),
        snapshot_model_input(resumed.conversation.model_input()).unwrap()
    );
}

#[tokio::test]
async fn summary_keeps_full_prefix_including_skill_and_request_directives_then_freshens_window() {
    let (_directory, writer) = test_transcript();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(WriteTestTool { calls: writes })
        .tool(DescribedCommand("command"))
        .tool(SkillStubTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let mut policies = policies();
    policies
        .policy_mut(SessionMode::Build)
        .allowed_tool_names
        .as_mut()
        .unwrap()
        .push("skill".into());
    policies.policy_mut(SessionMode::Build).skills_enabled = true;
    let mut engine = SessionEngine::new(
        ScriptedProvider::new((0..4).map(|_| Ok(Message::assistant("reply")))),
        tools,
        policies,
        writer,
        test_skills(),
    )
    .unwrap()
    .with_subtask_concurrency(2);
    submit(&mut engine, SessionMode::Plan, "plan").await;
    let (events, _) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: "review".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    let source = engine
        .compaction_source(engine.conversation.items(), &policy)
        .unwrap();
    assert!(
        source
            .request
            .input
            .iter()
            .any(|item| matches!(item, OwnedModelRequestItem::AdditionalTools(_)))
    );
    assert!(
        source
            .request
            .input
            .iter()
            .any(|item| matches!(item, OwnedModelRequestItem::DeveloperInstruction(_)))
    );
    assert!(
        source
            .request
            .input
            .iter()
            .any(|item| matches!(item, OwnedModelRequestItem::RequestInstruction(_)))
    );
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Compact {
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    {
        let requests = engine.provider.requests.lock().unwrap();
        let summary = &requests[2];
        assert_eq!(summary.instructions, requests[1].instructions);
        assert_eq!(summary.tools, requests[1].tools);
        assert_eq!(summary.model_role, requests[1].model_role);
        assert_eq!(summary.allowed_tool_names, requests[1].allowed_tool_names);
        assert_eq!(
            &summary.input[..source.request.input.len()],
            &source.request.input
        );
        assert_eq!(summary.input.len(), source.request.input.len() + 1);
        assert_eq!(
            summary.prompt,
            zevria_model::compaction::summary_request_message(engine.compaction.summary_prompt())
        );
    }
    assert!(tool_window(engine.conversation.items()).initial.is_none());
    let checkpoint = engine.conversation.latest_compaction().unwrap().1;
    assert!(checkpoint.replacement_history.iter().all(|item| !matches!(
        item,
        OwnedModelRequestItem::AdditionalTools(_)
            | OwnedModelRequestItem::DeveloperInstruction(_)
            | OwnedModelRequestItem::RequestInstruction(_)
    )));
    submit(&mut engine, SessionMode::Build, "after").await;
    let requests = engine.provider.requests.lock().unwrap();
    assert_eq!(names(&requests[3].tools), ["write", "command", "skill"]);
    assert!(
        !requests[3]
            .input
            .iter()
            .any(|item| matches!(item, OwnedModelRequestItem::AdditionalTools(_)))
    );
    assert_eq!(
        records(&engine)
            .iter()
            .filter(|record| record.placement == ToolDeclarationPlacement::Initial)
            .count(),
        2
    );
}

#[tokio::test]
async fn synthetic_tool_calls_retry_once_with_identical_prefix_and_never_execute() {
    for repeated in [false, true] {
        let (_directory, writer) = test_transcript();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let call = Message::Assistant(rig_core::message::AssistantMessage::new(vec![
            named_tool_call("synthetic", "write", json!({"value":"never"})),
        ]));
        let provider = ScriptedProvider::new([
            Ok(call.clone()),
            Ok(if repeated {
                call
            } else {
                Message::assistant("summary")
            }),
        ]);
        let mut engine = SessionEngine::new(
            provider,
            registry("command", writes.clone()),
            policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_history(vec![Message::user("task")])
        .unwrap();
        let (events, _) = session_event_channel(128);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Compact {
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(writes.lock().unwrap().is_empty());
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].forbid_tool_calls);
        assert!(requests[1].forbid_tool_calls);
        let mut expected = requests[0].clone();
        expected.forbid_tool_calls = true;
        assert_eq!(expected, requests[1]);
        assert_eq!(engine.conversation.latest_compaction().is_some(), !repeated);
    }
}

#[tokio::test]
async fn prospective_addition_is_in_the_exact_counted_request_and_reuses_its_count() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("plan")),
            Ok(Message::assistant("build")),
        ]),
        registry("command", Arc::new(Mutex::new(Vec::new()))),
        policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    submit(&mut engine, SessionMode::Plan, "plan").await;
    engine
        .provider
        .input_counts
        .push_back(Ok(InputTokenCount::Exact(42)));
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let prompt = zevria_content::UserPrompt::new(vec![
        zevria_content::PromptBlock::Text("build".into()),
        zevria_content::PromptBlock::Image(image),
    ])
    .unwrap();
    let (events, _) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: prompt,
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let counted = engine.provider.counted_requests.lock().unwrap();
    let dispatched = engine.provider.requests.lock().unwrap();
    assert_eq!(counted.len(), 1);
    assert_eq!(counted[0], dispatched[1]);
    assert!(matches!(
        counted[0].input.last(),
        Some(OwnedModelRequestItem::AdditionalTools(_))
    ));
}

#[tokio::test]
async fn obsolete_window_declarations_can_be_compacted_instead_of_falsely_irreducible() {
    let (_directory, writer) = test_transcript();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let old_description: &'static str =
        Box::leak("old schema documentation ".repeat(1000).into_boxed_str());
    let new_description: &'static str =
        Box::leak("new schema documentation ".repeat(1000).into_boxed_str());
    let mut original = SessionEngine::new(
        ScriptedProvider::new([Ok(Message::assistant("original reply"))]),
        registry(old_description, writes.clone()),
        policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let policy = original.policies.policy(SessionMode::Build).clone();
    let limit = original.fixed_input_tokens(&policy) + 1500;
    original = original.with_compaction_policy(test_compaction_policy(limit, 90, 0));
    submit(&mut original, SessionMode::Build, "original task").await;
    let initial = records(&original)[0].tools.clone();
    let path = original.conversation.path().to_path_buf();
    drop(original);
    let mut resumed = SessionEngine::new(
        ScriptedProvider::new([
            Ok(Message::assistant("compacted")),
            Ok(Message::assistant("continued")),
        ]),
        registry(new_description, writes),
        policies(),
        TranscriptWriter::append_to(path).unwrap(),
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_compaction_policy(test_compaction_policy(limit, 90, 0));
    assert!(resumed.fixed_input_tokens(&policy) > limit);
    assert!(
        resumed.irreducible_input_tokens(&resumed.rendered_instructions(&policy), &policy) + 500
            < limit
    );
    submit(&mut resumed, SessionMode::Build, "continue").await;
    assert!(resumed.conversation.latest_compaction().is_some());
    assert!(
        records(&resumed)
            .iter()
            .all(|record| record.placement == ToolDeclarationPlacement::Initial)
    );
    assert_eq!(records(&resumed).len(), 2);
    let requests = resumed.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools, initial);
    assert_eq!(requests[1].tools[1].description, new_description);
    assert_eq!(requests[0].instructions, requests[1].instructions);
}

#[test]
fn empty_initial_still_latches_the_window_when_the_flag_is_disabled() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    assert!(
        tool_window(engine.conversation.items())
            .initial
            .unwrap()
            .is_empty()
    );
    engine.incremental_tools = false;
    engine.tools = registry("command", Arc::new(Mutex::new(Vec::new())));
    // Production registries are frozen; this test deliberately replaces one.
    engine.invalidate_request_plans();
    engine.reconcile_before_dispatch(&policy).unwrap();
    assert!(
        engine
            .request_tools(HistoryView::Committed, &policy)
            .is_empty()
    );
    assert_eq!(records(&engine).len(), 2);
    assert_eq!(
        records(&engine)[1].placement,
        ToolDeclarationPlacement::Additional
    );
    assert_eq!(names(&records(&engine)[1].tools), ["write", "command"]);
}

#[test]
fn prospective_additions_become_input_without_changing_shape_or_double_counting() {
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        registry("command", Arc::new(Mutex::new(Vec::new()))),
        policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let plan = engine.policies.policy(SessionMode::Plan).clone();
    engine.reconcile_before_dispatch(&plan).unwrap();
    let build = engine.policies.policy(SessionMode::Build).clone();
    let shape = engine.request_shape(&build).unwrap();
    let fixed = engine.fixed_input_tokens(&build);
    let before =
        estimate_model_input(engine.conversation.model_input()).conservative_tokens + fixed;
    engine.reconcile_before_dispatch(&build).unwrap();
    let after = estimate_model_input(engine.conversation.model_input()).conservative_tokens
        + engine.fixed_input_tokens(&build);
    assert_eq!(shape, engine.request_shape(&build).unwrap());
    assert!(fixed > engine.fixed_input_tokens(&build));
    assert_eq!(before, after);
}
