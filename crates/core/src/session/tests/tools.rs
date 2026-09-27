use super::*;

#[tokio::test]
async fn ensemble_plan_gate_denies_out_of_order_calls_and_keeps_progress_recoverable() {
    let command_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(CommandTestTool {
            calls: command_calls.clone(),
        })
        .tool(ReconcileReportsStubTool)
        .tool(SubmitPlanStubTool)
        .run();
    let policy = TurnPolicy::new(
        "gate test",
        Some(vec![
            "command".to_string(),
            RECONCILE_REPORTS_TOOL_NAME.to_string(),
            QUESTION_TOOL_NAME.to_string(),
            SUBMIT_PLAN_TOOL_NAME.to_string(),
        ]),
        ModelRole::Plan,
        false,
    );
    let (_directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let turn = TurnContext::new(TurnId::new(77), SessionMode::Plan, CancellationToken::new());
    let mut gate = PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default());
    let title = "Ordered gate plan";
    let markdown = valid_plan_markdown(title, "Respect every durable gate stage.");
    let call_batch = |message: Message| assistant_tool_calls(&message);

    let premature_submit = call_batch(submit_plan_call(title, &markdown));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &premature_submit, ActiveSkills::default(), &gate).await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Denied);
    assert!(batch.candidate.is_none());
    gate.apply_batch(&batch);

    let premature_reconciliation = call_batch(reconciliation_call(
        "early-reconciliation",
        no_disagreement_reconciliation(),
    ));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(
            &scope,
            &premature_reconciliation,
            ActiveSkills::default(),
            &gate,
        )
        .await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Denied);
    gate.apply_batch(&batch);

    let inspection = call_batch(command_call("gate-inspection", "rtk rg -n gate src"));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &inspection, ActiveSkills::default(), &gate).await
    };
    assert!(batch.inspection_attempted);
    gate.apply_batch(&batch);
    assert!(
        gate.ensemble
            .as_ref()
            .expect("ensemble gate")
            .inspection_completed
    );

    let submit_before_reconciliation = call_batch(submit_plan_call(title, &markdown));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(
            &scope,
            &submit_before_reconciliation,
            ActiveSkills::default(),
            &gate,
        )
        .await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Denied);

    let reconciliation = call_batch(reconciliation_call(
        "gate-reconciliation",
        no_disagreement_reconciliation(),
    ));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &reconciliation, ActiveSkills::default(), &gate).await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Success);
    gate.apply_batch(&batch);
    assert!(!gate.ensemble.as_ref().expect("gate").requires_question());

    let unnecessary_question = call_batch(Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            "duplicate-question",
            QUESTION_TOOL_NAME,
            json!({}),
        )],
    });
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(
            &scope,
            &unnecessary_question,
            ActiveSkills::default(),
            &gate,
        )
        .await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Denied);

    let valid_submit = call_batch(submit_plan_call(title, &markdown));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &valid_submit, ActiveSkills::default(), &gate).await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Success);
    gate.apply_batch(&batch);
    assert!(gate.candidate_accepted());
}

#[tokio::test]
async fn ensemble_plan_question_gate_rejects_same_batch_and_malformed_attempts() {
    let (events, _receiver) = session_event_channel(8);
    let questions = question_channels(events);
    let tools = ToolServer::new()
        .tool(ReconcileReportsStubTool)
        .tool(QuestionStubTool {
            requester: questions.requester,
        })
        .tool(SubmitPlanStubTool)
        .run();
    let policy = TurnPolicy::new(
        "question gate test",
        Some(vec![
            "command".to_string(),
            RECONCILE_REPORTS_TOOL_NAME.to_string(),
            QUESTION_TOOL_NAME.to_string(),
            SUBMIT_PLAN_TOOL_NAME.to_string(),
        ]),
        ModelRole::Plan,
        false,
    );
    let (_directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let turn = TurnContext::new(TurnId::new(78), SessionMode::Plan, CancellationToken::new());
    let mut gate = PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default());
    gate.ensemble.as_mut().expect("gate").inspection_completed = true;

    let reconciliation = assistant_tool_calls(&reconciliation_call(
        "question-reconciliation",
        root_question_reconciliation(),
    ));
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &reconciliation, ActiveSkills::default(), &gate).await
    };
    gate.apply_batch(&batch);
    assert!(gate.ensemble.as_ref().expect("gate").requires_question());

    let title = "Question gated plan";
    let markdown = valid_plan_markdown(title, "Wait for the question result.");
    let mixed = assistant_tool_calls(&Message::Assistant {
        id: None,
        content: vec![
            named_tool_call("same-batch-question", QUESTION_TOOL_NAME, json!({})),
            match submit_plan_call(title, &markdown) {
                Message::Assistant { mut content, .. } => content.remove(0),
                _ => unreachable!(),
            },
        ],
    });
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &mixed, ActiveSkills::default(), &gate).await
    };
    assert!(
        batch
            .metadata
            .iter()
            .all(|metadata| metadata.outcome == ToolCallOutcome::Denied)
    );
    gate.apply_batch(&batch);
    assert!(
        gate.ensemble
            .as_ref()
            .expect("gate")
            .question_disposition
            .is_none()
    );

    let malformed = assistant_tool_calls(&Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            "malformed-question",
            QUESTION_TOOL_NAME,
            serde_json::Value::String("{".to_string()),
        )],
    });
    let batch = {
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        execute_tool_calls(&scope, &malformed, ActiveSkills::default(), &gate).await
    };
    assert_eq!(batch.metadata[0].outcome, ToolCallOutcome::Error);
    assert!(batch.metadata[0].detail.is_none());
    assert!(batch.question_disposition.is_none());
    gate.apply_batch(&batch);
    assert!(!gate.ensemble.as_ref().expect("gate").can_submit());

    for disposition in [
        QuestionTerminalDisposition::Answered,
        QuestionTerminalDisposition::Dismissed,
        QuestionTerminalDisposition::Unavailable,
        QuestionTerminalDisposition::InvalidFrontendResponse,
    ] {
        let mut settled = gate.clone();
        settled
            .ensemble
            .as_mut()
            .expect("gate")
            .question_disposition = Some(disposition);
        assert!(settled.ensemble.as_ref().expect("gate").can_submit());
    }
}

#[test]
fn post_reports_tail_reconstructs_reconciliation_and_terminal_question() {
    let descriptor = zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "worker".to_string(),
        label: "Worker".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let run_id = EnsembleRunId::new();
    let reconciliation = root_question_reconciliation();
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: run_id.clone(),
            synthesis_input: Message::user("evidence"),
            agents: vec![AgentRunSummary {
                descriptor,
                status: AgentRunStatus::Completed,
                partial: false,
                failure: None,
                has_report: true,
                has_plan_proof: true,
                confirmation: None,
                decision_ids: Vec::new(),
                unavailable_decisions: Vec::new(),
            }],
        }),
        TranscriptItem::Message(command_call(
            "tail-inspection",
            "rtk rg -n insert crates/tui/src",
        )),
        successful_tool_result("tail-inspection", "command", "inspected"),
        TranscriptItem::Message(reconciliation_call("tail-reconciliation", reconciliation)),
        successful_tool_result(
            "tail-reconciliation",
            RECONCILE_REPORTS_TOOL_NAME,
            "accepted",
        ),
        TranscriptItem::Message(Message::Assistant {
            id: None,
            content: vec![named_tool_call(
                "tail-question",
                QUESTION_TOOL_NAME,
                json!({}),
            )],
        }),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::tool_result(
                "tail-question",
                QUESTION_TOOL_NAME,
                r#"{"status":"dismissed"}"#,
            ),
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "tail-question".to_string(),
                call_id: None,
                tool_name: QUESTION_TOOL_NAME.to_string(),
                outcome: ToolCallOutcome::Success,
                detail: Some(ToolResultDetail::QuestionDisposition(
                    QuestionTerminalDisposition::Dismissed,
                )),
            }],
        },
    ];

    for (disposition, outcome) in [
        (None, ToolCallOutcome::Error),
        (None, ToolCallOutcome::Cancelled),
        (
            Some(QuestionTerminalDisposition::Answered),
            ToolCallOutcome::Success,
        ),
        (
            Some(QuestionTerminalDisposition::Dismissed),
            ToolCallOutcome::Success,
        ),
        (
            Some(QuestionTerminalDisposition::Unavailable),
            ToolCallOutcome::Error,
        ),
        (
            Some(QuestionTerminalDisposition::InvalidFrontendResponse),
            ToolCallOutcome::Error,
        ),
    ] {
        for correlated in [false, true] {
            let mut items = items.clone();
            let Some(TranscriptItem::ToolResults { metadata, .. }) = items.last_mut() else {
                unreachable!()
            };
            metadata[0].detail = disposition.map(ToolResultDetail::QuestionDisposition);
            metadata[0].outcome = outcome;
            if !correlated {
                metadata[0].id = "unrelated-question".into();
            }
            let items: Vec<TranscriptItem> =
                serde_json::from_value(serde_json::to_value(items).unwrap()).unwrap();
            let gate = plan_submission_gate_after_reports(&items, &run_id)
                .expect("valid catalog")
                .expect("recovered gate");
            let ensemble = gate.ensemble.expect("ensemble state");
            assert!(ensemble.inspection_completed);
            assert!(ensemble.reconciliation.is_some());
            let expected = disposition.filter(|_| correlated);
            assert_eq!(ensemble.question_disposition, expected);
            assert_eq!(ensemble.can_submit(), expected.is_some());
        }
    }
}

#[tokio::test]
async fn one_batch_activates_each_distinct_skill_once() {
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let skills = test_skills();
    let tools = ToolServer::new()
        .tool(SkillStubTool {
            calls: dispatched.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("two-skills".to_string()),
            content: vec![
                named_tool_call("commit-call", SKILL_TOOL_NAME, json!({"skill": "commit"})),
                named_tool_call(
                    "review-call",
                    SKILL_TOOL_NAME,
                    json!({"skill": "review", "args": "inspect"}),
                ),
            ],
        }),
        Ok(Message::assistant("done")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine =
        SessionEngine::new(provider, tools, test_policies(), transcript, skills).expect("engine");
    let (events, _receiver) = session_event_channel(16);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "activate both".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(dispatched.lock().unwrap().is_empty());
    assert_eq!(engine.active_skills().unwrap().len(), 2);
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .flat_map(recorded_skill_applications)
            .filter(|app| matches!(app, SkillApplication::Activate(_)))
            .count(),
        2
    );
    let requests = requests.lock().expect("requests");
    let context = requests[1].skill_context.as_deref().expect("overlay");
    assert_eq!(context.matches("Commit instructions").count(), 1);
    assert_eq!(context.matches("Review instructions").count(), 1);
}

#[tokio::test]
async fn mixed_batches_return_ordinary_and_subtask_results_together() {
    let (events, mut receiver) = session_event_channel(1024);
    let channels = subtask_channels("root-session", events.clone());
    autocomplete_requests(channels.requests);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: calls.clone(),
        })
        .tool(LaunchTestTool {
            launcher: channels.launcher.clone(),
            calls: calls.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![
                tool_call("call-echo", "ordinary"),
                launch_call("call-launch", "new explore task"),
            ],
        }),
        Ok(Message::assistant("all wrapped up")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "run the batch".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    // The launcher announced the child row with the assistant call id.
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::SubtaskLaunched { call_id, descriptor, .. }
            if call_id == "call-launch" && descriptor.title == "new explore task"
    )));
    let tool_results = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .expect("tool results event");
    // One correlated message carries both results in assistant order,
    // with the subtask report as an ordinary tool result.
    assert_eq!(user_tool_result_count(tool_results.0), 2);
    assert_eq!(
        tool_results
            .1
            .iter()
            .map(|metadata| metadata.tool_name.as_str())
            .collect::<Vec<_>>(),
        ["echo", LAUNCH_SUBTASKS_TOOL_NAME]
    );
    assert_eq!(
        tool_results.1[1]
            .subtasks()
            .first()
            .and_then(|entry| entry.launch.as_ref())
            .map(|m| m.title.as_str()),
        Some("new explore task")
    );
    let serialized = serde_json::to_string(tool_results.0).expect("message json");
    assert!(serialized.contains("report for new explore task"));
    assert!(!serialized.contains("parentSessionId"));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnCompleted { message, .. }
            if format!("{message:?}").contains("all wrapped up")
    )));

    // One batch, one final: the report needed no extra delivery turn, and
    // the continuation prompt carries it as the tool result.
    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests.len(), 2);
    let continuation = serde_json::to_string(&requests[1].prompt).expect("continuation");
    assert!(continuation.contains("report for new explore task"));

    let items = zevria_transcript::transcript::load(&transcript_path).expect("reload transcript");
    assert!(items.iter().any(|item| matches!(
        item,
        TranscriptItem::ToolResults { metadata, .. }
            if metadata.len() == 2
    )));
}

#[tokio::test]
async fn parallel_launches_in_one_batch_run_concurrently() {
    let (events, mut receiver) = session_event_channel(1024);
    let channels = subtask_channels("root-session", events.clone());
    // Fake supervisor: resolve nothing until BOTH requests have arrived.
    // Sequential dispatch would deadlock here — the first call could
    // never finish before the second launch queued its request.
    let mut requests_rx = channels.requests;
    tokio::spawn(async move {
        let first = requests_rx.recv().await.expect("first request");
        let second = requests_rx.recv().await.expect("second request");
        for request in [first, second] {
            let report = format!("report for {}", request.descriptor.title);
            let _ = request.outcome.send(SubtaskOutcome::Completed { report });
        }
    });

    let tools = ToolServer::new()
        .tool(LaunchTestTool {
            launcher: channels.launcher.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![
                launch_call("call-a", "first explore task"),
                launch_call("call-b", "second explore task"),
            ],
        }),
        Ok(Message::assistant("both reports considered")),
    ]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "explore both".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    for (call_id, title) in [
        ("call-a", "first explore task"),
        ("call-b", "second explore task"),
    ] {
        assert!(
            events.iter().any(|event| matches!(
                event,
                SessionEvent::SubtaskLaunched { call_id: id, descriptor, .. }
                    if id == call_id && descriptor.title == title
            )),
            "missing SubtaskLaunched for {call_id}"
        );
    }
    let tool_results = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool results event");
    // Both reports return together as the two correlated tool results.
    assert_eq!(user_tool_result_count(tool_results), 2);
    let serialized = serde_json::to_string(tool_results).expect("message json");
    assert!(serialized.contains("report for first explore task"));
    assert!(serialized.contains("report for second explore task"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}
