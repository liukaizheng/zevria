use super::*;

#[tokio::test]
async fn skill_batch_stages_pins_and_keeps_earlier_acceptance_when_later_capacity_fails() {
    let skills = test_skills();
    let calls_seen = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: calls_seen.clone(),
        })
        .run();
    let (_dir, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        writer,
        skills.clone(),
    )
    .unwrap();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    let one = ActiveSkills::from_snapshots([skills.get("commit").unwrap().snapshot()]).unwrap();
    let two = ActiveSkills::from_snapshots(skills.iter().map(|definition| definition.snapshot()))
        .unwrap();
    let limit = engine.prospective_skill_overhead(&policy, &two);
    assert!(engine.prospective_skill_overhead(&policy, &one) < limit);
    engine.compaction = test_compaction_policy(limit, 100, 0);
    let assistant = Message::Assistant {
        id: None,
        content: vec![
            named_tool_call("first", "skill", json!({"skill":"commit"})),
            named_tool_call("again", "skill", json!({"skill":"commit"})),
            named_tool_call("oversized", "skill", json!({"skill":"review"})),
            named_tool_call("missing", "skill", json!({"skill":"not-installed"})),
            named_tool_call(
                "malformed",
                "skill",
                json!({"skill":"commit", "unexpected":true}),
            ),
        ],
    };
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let gate = PlanSubmissionGate::inert(None);
    let scope = ToolExecutionScope::new(&engine, SessionMode::Build, &policy, &turn, &gate);
    let batch = execute_tool_calls(
        &scope,
        &assistant_tool_calls(&assistant),
        ActiveSkills::default(),
        &gate,
    )
    .await;
    assert_eq!(
        batch
            .metadata
            .iter()
            .map(|entry| entry.outcome)
            .collect::<Vec<_>>(),
        [
            ToolCallOutcome::Success,
            ToolCallOutcome::Success,
            ToolCallOutcome::Error,
            ToolCallOutcome::Error,
            ToolCallOutcome::Error
        ]
    );
    assert_eq!(batch.skill_applications.len(), 2);
    assert!(matches!(
        batch.skill_applications[0].application,
        SkillApplication::Activate(_)
    ));
    assert!(matches!(
        batch.skill_applications[1].application,
        SkillApplication::Reapply(_)
    ));
    let mut items = engine.conversation.items().to_vec();
    items.push(TranscriptItem::Message(assistant));
    items.push(TranscriptItem::ToolResults {
        message: batch.message,
        metadata: batch.metadata,
        skill_applications: batch.skill_applications,
    });
    assert_eq!(replay_active_skills(&items).unwrap(), one);
    SessionReplayError::validate(&items).unwrap();
    assert!(
        engine.active_skills().unwrap().is_empty(),
        "preparation only stages pins"
    );
}

#[tokio::test]
async fn skill_cancelled_before_execution_neither_submits_nor_pins() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: calls.clone(),
        })
        .run();
    let (_dir, writer) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, token);
    let gate = PlanSubmissionGate::inert(None);
    let scope = ToolExecutionScope::new(
        &engine,
        SessionMode::Build,
        engine.policies.policy(SessionMode::Build),
        &turn,
        &gate,
    );
    let assistant = Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            "cancelled",
            "skill",
            json!({"skill":"commit"}),
        )],
    };
    let batch = execute_tool_calls(
        &scope,
        &assistant_tool_calls(&assistant),
        ActiveSkills::default(),
        &gate,
    )
    .await;
    assert!(calls.lock().unwrap().is_empty());
    assert!(batch.skill_applications.is_empty());
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Cancelled);
}

struct CorruptSkillRequest;
impl Tool for CorruptSkillRequest {
    const NAME: &'static str = "skill";
    type Error = std::convert::Infallible;
    type Args = SkillRequest;
    type Output = String;
    fn description(&self) -> String {
        "Corrupt request fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        _request: SkillRequest,
    ) -> Result<String, Self::Error> {
        Ok("UNVALIDATED TOOL SUCCESS".into())
    }
}

#[tokio::test]
async fn skill_engine_never_dispatches_or_trusts_tool_success_prose() {
    for _ in 0..2 {
        let (_dir, writer) = test_transcript();
        let tools = ToolServer::new().tool(CorruptSkillRequest).run();
        let engine = SessionEngine::new(
            ScriptedProvider::new([]),
            tools,
            test_policies(),
            writer,
            test_skills(),
        )
        .unwrap();
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        let gate = PlanSubmissionGate::inert(None);
        let scope = ToolExecutionScope::new(
            &engine,
            SessionMode::Build,
            engine.policies.policy(SessionMode::Build),
            &turn,
            &gate,
        );
        let assistant = Message::Assistant {
            id: None,
            content: vec![named_tool_call("call", "skill", json!({"skill":"commit"}))],
        };
        let batch = execute_tool_calls(
            &scope,
            &assistant_tool_calls(&assistant),
            ActiveSkills::default(),
            &gate,
        )
        .await;
        assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Success);
        assert_eq!(batch.skill_applications.len(), 1);
        assert!(
            !serde_json::to_string(&batch.message)
                .unwrap()
                .contains("UNVALIDATED TOOL SUCCESS")
        );
    }
}
