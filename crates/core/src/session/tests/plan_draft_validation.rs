use super::*;

struct InvalidDraftTool;
impl Tool for InvalidDraftTool {
    const NAME: &'static str = CREATE_PLAN_TOOL_NAME;
    type Args = CreatePlanStubArgs;
    type Error = std::convert::Infallible;
    type Output = String;
    fn description(&self) -> String {
        "invalid draft fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<String, Self::Error> {
        let scope = context.scope::<PlanDraftScope>().unwrap();
        context
            .insert_result(PlanDraft {
                target: scope.target,
                sequence: scope.next_sequence + 1,
                markdown: args.markdown,
                source_turn_id: scope.turn_id,
            })
            .unwrap();
        Ok("SHOULD_NOT_BE_ACKNOWLEDGED".into())
    }
}
struct ForgedSubmitTool;
impl Tool for ForgedSubmitTool {
    const NAME: &'static str = SUBMIT_PLAN_TOOL_NAME;
    type Args = NoArgs;
    type Error = std::convert::Infallible;
    type Output = String;
    fn description(&self) -> String {
        "mismatched candidate fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn call(&self, context: &mut ToolContext, _: Self::Args) -> Result<String, Self::Error> {
        let scope = context.scope::<PlanDraftScope>().unwrap();
        let candidate = PlanCandidate::from_markdown(
            scope
                .markdown
                .as_ref()
                .unwrap()
                .replace("Original.", "Forged."),
            MAX_PLAN_ARTIFACT_BYTES,
        )
        .unwrap();
        context.insert_result(candidate).unwrap();
        Ok("candidate accepted".into())
    }
}
#[tokio::test]
async fn reducer_rejection_replaces_the_false_save_acknowledgement_and_denies_submission() {
    let text = valid_plan_markdown("Validated prospective plan draft", "Original.");
    let provider = ScriptedProvider::new([
        Ok(batch(vec![create_plan_call(&text), submit_plan_call()])),
        Ok(Message::assistant("Failed save.")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, writer) = test_transcript();
    let tools = ToolServer::new()
        .tool(InvalidDraftTool)
        .tool(SubmitPlanStubTool)
        .run();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "draft").await;
    assert!(engine.plan_state().unwrap().working_copy().is_none());
    assert!(
        !engine
            .conversation()
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::DraftSaved { .. })))
    );
    assert!(
        !format!("{:?}", requests.lock().unwrap().last().unwrap())
            .contains("SHOULD_NOT_BE_ACKNOWLEDGED")
    );
    let lifecycle = collect_events(&mut receiver).await;
    let results = lifecycle
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { metadata, .. } => Some(metadata),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        results.iter().map(|meta| meta.outcome).collect::<Vec<_>>(),
        [ToolCallOutcome::Error, ToolCallOutcome::Denied]
    );
    assert!(results[0].diagnostic.as_ref().unwrap().contains("sequence"));
}
#[tokio::test]
async fn submission_must_equal_the_prospective_working_copy_byte_for_byte() {
    let text = valid_plan_markdown("Byte exact draft submission", "Original.");
    let provider = ScriptedProvider::new([
        Ok(batch(vec![create_plan_call(&text), submit_plan_call()])),
        Ok(Message::assistant("Candidate rejected.")),
    ]);
    let (_directory, writer) = test_transcript();
    let tools = ToolServer::new()
        .tool(CreatePlanStubTool)
        .tool(ForgedSubmitTool)
        .run();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "draft").await;
    assert_eq!(
        engine
            .plan_state()
            .unwrap()
            .working_copy()
            .unwrap()
            .markdown(),
        text
    );
    assert!(collect_events(&mut receiver).await.iter().any(|event| matches!(event, SessionEvent::ToolResults { metadata, .. } if metadata.last().unwrap().diagnostic.as_ref().is_some_and(|diagnostic| diagnostic.contains("match the stored Plan working copy")))));
}
#[tokio::test]
async fn ensemble_drafting_is_allowed_before_inspection_without_changing_gate_order() {
    let text = valid_plan_markdown("Early ensemble plan draft", "Original.");
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        draft_tools(),
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    engine
        .record_required(TranscriptItem::Plan(PlanRecord::Started {
            id: PlanId::new(),
        }))
        .unwrap();
    let gate = PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default());
    let policy = engine.ensemble_policy(EnsembleWorkflow::Plan);
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, CancellationToken::new());
    let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
    let calls = assistant_tool_calls(&batch(vec![
        create_plan_call(&text),
        update_plan_call("Original.", "Early revision."),
        submit_plan_call(),
    ]));
    let result = execute_tool_calls(&scope, &calls, ActiveSkills::default(), &gate).await;
    assert_eq!(result.drafts.len(), 2);
    assert!(
        result
            .drafts
            .last()
            .unwrap()
            .markdown
            .contains("Early revision.")
    );
    assert_eq!(
        result.metadata.last().unwrap().outcome,
        ToolCallOutcome::Denied
    );
    assert_eq!(
        result.metadata.last().unwrap().diagnostic.as_deref(),
        Some("complete at least one terminal `command` inspection attempt before submit_plan")
    );
    let invalid = assistant_tool_calls(&batch(vec![
        create_plan_call("# Invalid"),
        submit_plan_call(),
    ]));
    let result = execute_tool_calls(&scope, &invalid, ActiveSkills::default(), &gate).await;
    assert_eq!(result.metadata[0].outcome, ToolCallOutcome::Error);
    assert_eq!(
        result.metadata[1].diagnostic.as_deref(),
        Some("complete at least one terminal `command` inspection attempt before submit_plan")
    );
}

#[test]
fn recovery_distinguishes_unchanged_resubmission_from_legacy_one_shot_arguments() {
    let markdown = valid_plan_markdown("Retained submitted plan revision", "Old submitted goal.");
    let id = PlanId::new();
    let artifact = PlanCandidate::from_markdown(markdown.clone(), MAX_PLAN_ARTIFACT_BYTES)
        .unwrap()
        .into_artifact(PlanVersion { id, revision: 1 }, TurnId::new(1));
    let run_id = EnsembleRunId::new();
    let prefix = vec![
        TranscriptItem::Plan(PlanRecord::Started { id }),
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::RevisionRequested { artifact }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: run_id.clone(),
            synthesis_input: Message::user("reports"),
            agents: Vec::new(),
        }),
        TranscriptItem::Message(command_call("inspect", "source inspection")),
        successful_tool_result("inspect", "command", "done"),
        TranscriptItem::Message(reconciliation_call(
            "reconcile",
            no_disagreement_reconciliation(),
        )),
        successful_tool_result("reconcile", RECONCILE_REPORTS_TOOL_NAME, "accepted"),
    ];
    for legacy in [false, true] {
        let mut items = prefix.clone();
        items.push(TranscriptItem::Message(if legacy {
            Message::Assistant(rig_core::message::AssistantMessage::new(vec![named_tool_call("submit-plan", SUBMIT_PLAN_TOOL_NAME,
                json!({"title":"Changed legacy plan revision", "markdown": valid_plan_markdown("Changed legacy plan revision", "New one-shot goal.")}))]))
        } else { submit_plan_call() }));
        items.push(successful_tool_result(
            "submit-plan",
            SUBMIT_PLAN_TOOL_NAME,
            "accepted",
        ));
        let gate = plan_submission_gate_after_reports(&items, &run_id)
            .unwrap()
            .unwrap();
        if legacy {
            assert!(
                gate.candidate.is_none(),
                "never publish the old retained text as a legacy one-shot candidate"
            );
        } else {
            assert_eq!(gate.candidate.unwrap().markdown(), markdown);
        }
    }
}
