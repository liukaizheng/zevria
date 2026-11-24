use super::*;

fn image_result_content() -> Vec<ToolResultContent> {
    vec![
        ToolResultContent::text("image summary"),
        zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255])
            .unwrap()
            .to_tool_result_content(),
    ]
}

async fn dispatch_rich_output(output: rig_core::tool::ToolOutput, id: &str) -> ToolCallSlot {
    let tools = ToolServer::new().tool(RichOutputTestTool(output)).run();
    let AssistantContent::ToolCall(call) = named_tool_call(id, "rich_output", json!({})) else {
        unreachable!();
    };
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    dispatch_tool_call(
        &tools,
        &EngineCall::new(&call),
        &turn,
        None,
        None,
        None,
        zevria_foundation::McpToolAccess::None,
    )
    .await
}

#[tokio::test]
async fn image_tool_results_persist_reload_and_reach_the_next_provider_request() {
    let expected_content = image_result_content();
    let tools = ToolServer::new()
        .tool(RichOutputTestTool(
            rig_core::tool::ToolOutput::content(expected_content.clone()).unwrap(),
        ))
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "image-call",
                "rich_output",
                json!({}),
            )]),
        )),
        Ok(Message::assistant("image understood")),
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
    .unwrap();
    let (events, mut receiver) = session_event_channel(256);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "look at the image".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let updates = collect_events(&mut receiver).await;
    assert!(
        updates
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    let (message, metadata) = updates
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .expect("tool results event");
    let Message::User { content } = message else {
        panic!("user results")
    };
    let [UserContent::ToolResult(result)] = content.as_slice() else {
        panic!("single result")
    };
    assert_eq!(result.content, expected_content);
    assert!(!result.is_error);
    assert_eq!(metadata[0].outcome, ToolCallOutcome::Success);
    assert!(metadata[0].diagnostic.is_none());
    assert!(engine.conversation.has_images());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(&requests[1].prompt, message);
    let items = zevria_transcript::transcript::load(&transcript_path).unwrap();
    assert!(items.iter().any(|item| matches!(
        item, TranscriptItem::ToolResults { message: persisted, .. } if persisted == message
    )));
    // Rebuilding conversation state must also recognize persisted tool images.
    drop(engine);
    let loaded = zevria_transcript::transcript::load_report(&transcript_path).unwrap();
    let writer = TranscriptWriter::append_loaded(&loaded).unwrap();
    let restored = Conversation::from_loaded(writer, loaded).unwrap();
    assert!(restored.has_images());
}

#[tokio::test]
async fn multimodal_dispatch_preserves_order_and_renders_only_json_blocks_as_text() {
    let mut content = image_result_content();
    content.insert(1, ToolResultContent::json(json!({"key": [1, 2]})));
    content.push(ToolResultContent::text("after"));
    let slot = dispatch_rich_output(
        rig_core::tool::ToolOutput::content(content.clone()).unwrap(),
        "mixed",
    )
    .await;
    let UserContent::ToolResult(result) = slot.result else {
        panic!("tool result")
    };
    content[1] = ToolResultContent::text("{\"key\":[1,2]}");
    assert_eq!(result.content, content);
    assert!(!result.is_error);
    assert_eq!(slot.metadata.outcome, ToolCallOutcome::Success);
    assert!(slot.metadata.diagnostic.is_none());
}

#[tokio::test]
async fn non_image_outputs_keep_their_exact_historical_rendering() {
    for output in [
        rig_core::tool::ToolOutput::text("  literal\ntext\n"),
        rig_core::tool::ToolOutput::json(json!({"value": [1, "two"]})),
        rig_core::tool::ToolOutput::content(vec![
            ToolResultContent::text("before"),
            ToolResultContent::json(json!({"key": "value"})),
            ToolResultContent::text("after"),
        ])
        .unwrap(),
    ] {
        let expected = output.render();
        let slot = dispatch_rich_output(output, "text").await;
        let UserContent::ToolResult(result) = slot.result else {
            panic!("tool result")
        };
        assert_eq!(result.content, vec![ToolResultContent::text(expected)]);
        assert!(!result.is_error);
        assert!(slot.metadata.diagnostic.is_none());
    }
}

#[tokio::test]
async fn unsupported_image_blocks_become_error_results_without_payloads() {
    use rig_core::message::ImageMediaType;
    for image in [
        ToolResultContent::image_url(
            "https://example.com/image.png",
            Some(ImageMediaType::PNG),
            None,
        ),
        ToolResultContent::image_raw(vec![1, 2, 3], Some(ImageMediaType::PNG), None),
        ToolResultContent::image_base64("AA==", None, None),
        ToolResultContent::image_base64("AA==", Some(ImageMediaType::SVG), None),
    ] {
        let slot =
            dispatch_rich_output(rig_core::tool::ToolOutput::one(image), "unsupported").await;
        let UserContent::ToolResult(result) = slot.result else {
            panic!("tool result")
        };
        assert!(result.is_error);
        assert_eq!(
            result.content,
            vec![ToolResultContent::text(
                "status: error\nerror: tool returned an unsupported image block"
            )]
        );
        assert_eq!(slot.metadata.outcome, ToolCallOutcome::Error);
        assert_eq!(
            slot.metadata.diagnostic.as_deref(),
            Some("tool returned an unsupported image block")
        );
    }
}

fn assert_image_budget_error(result: &ToolResult, metadata: &ToolResultMetadata) {
    let diagnostic = "image result omitted: one response may return at most eight images or 20 MiB of image data; request fewer images per response";
    assert!(result.is_error);
    assert_eq!(
        result.content,
        vec![ToolResultContent::text(format!(
            "status: error\nerror: {diagnostic}"
        ))]
    );
    assert_eq!(metadata.outcome, ToolCallOutcome::Error);
    assert_eq!(metadata.diagnostic.as_deref(), Some(diagnostic));
}

#[tokio::test]
async fn image_batch_admits_eight_images_then_errors_the_ninth_slot_in_call_order() {
    let mut slots = Vec::new();
    for index in 0..9 {
        slots.push(
            dispatch_rich_output(
                rig_core::tool::ToolOutput::content(image_result_content()).unwrap(),
                &format!("image-{index}"),
            )
            .await,
        );
    }
    slots.push(
        dispatch_rich_output(rig_core::tool::ToolOutput::text("text still fits"), "text").await,
    );
    let batch = assemble_tool_result_batch(slots, None);
    assert_eq!(batch.message.images().count(), 8);
    let Message::User { content } = batch.message else {
        panic!("user results")
    };
    for (index, block) in content.iter().enumerate() {
        let UserContent::ToolResult(result) = block else {
            panic!("tool result")
        };
        assert_eq!(
            result.call.to_string(),
            if index < 9 {
                format!("image-{index}")
            } else {
                "text".into()
            }
        );
        if index == 8 {
            assert_image_budget_error(result, &batch.metadata[index]);
        } else {
            assert!(!result.is_error);
            assert!(batch.metadata[index].diagnostic.is_none());
        }
    }
}

// Structural mock: the batch limiter measures base64 bytes without decoding
// raster pixels. Real file validation is covered by content/tools tests.
fn mock_image_of_size(bytes: usize) -> ToolResultContent {
    let mut data = "AAAA".repeat(bytes / 3);
    data.push_str(match bytes % 3 {
        1 => "AA==",
        2 => "AAA=",
        _ => "",
    });
    ToolResultContent::image_base64(data, Some(rig_core::message::ImageMediaType::PNG), None)
}

#[tokio::test]
async fn image_batch_limits_decoded_bytes_and_does_not_charge_rejected_slots() {
    let max = zevria_content::prompt::MAX_IMAGE_BYTES;
    let mut slots = Vec::new();
    // Both one- and two-character base64 padding must be subtracted: the
    // accepted images total exactly 20 MiB, not their expanded wire size.
    for (index, bytes) in [max, max, max, max - 1, 2, 1].into_iter().enumerate() {
        slots.push(
            dispatch_rich_output(
                rig_core::tool::ToolOutput::one(mock_image_of_size(bytes)),
                &format!("bytes-{index}"),
            )
            .await,
        );
    }
    let batch = assemble_tool_result_batch(slots, None);
    assert_eq!(batch.message.images().count(), 5);
    let Message::User { content } = batch.message else {
        panic!("user results")
    };
    for (index, block) in content.iter().enumerate() {
        let UserContent::ToolResult(result) = block else {
            panic!("tool result")
        };
        if index == 4 {
            assert_image_budget_error(result, &batch.metadata[index]);
        } else {
            assert!(!result.is_error);
        }
    }
}

#[tokio::test]
async fn image_batch_rejects_whole_multimodal_slots_instead_of_truncating_them() {
    let mut slots = Vec::new();
    for index in 0..7 {
        slots.push(
            dispatch_rich_output(
                rig_core::tool::ToolOutput::content(image_result_content()).unwrap(),
                &format!("single-{index}"),
            )
            .await,
        );
    }
    slots.push(
        dispatch_rich_output(
            rig_core::tool::ToolOutput::content(
                image_result_content()
                    .into_iter()
                    .chain(image_result_content())
                    .collect(),
            )
            .unwrap(),
            "two-images",
        )
        .await,
    );
    slots.push(
        dispatch_rich_output(
            rig_core::tool::ToolOutput::content(image_result_content()).unwrap(),
            "last",
        )
        .await,
    );
    let batch = assemble_tool_result_batch(slots, None);
    assert_eq!(batch.message.images().count(), 8);
    let Message::User { content } = batch.message else {
        panic!("user results")
    };
    let UserContent::ToolResult(result) = &content[7] else {
        panic!("tool result")
    };
    assert_image_budget_error(result, &batch.metadata[7]);
    let UserContent::ToolResult(result) = &content[8] else {
        panic!("tool result")
    };
    assert!(!result.is_error);
}

#[tokio::test]
async fn ensemble_plan_gate_denies_out_of_order_calls_and_keeps_progress_recoverable() {
    let command_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(CommandTestTool {
            calls: command_calls.clone(),
        })
        .tool(ReconcileReportsStubTool)
        .tool(CreatePlanStubTool)
        .tool(UpdatePlanStubTool)
        .tool(ReadPlanStubTool)
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
    let plan_id = PlanId::new();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_fixture(vec![
        TranscriptItem::Plan(Box::new(PlanRecord::Started { id: plan_id })),
        saved_plan_record(
            plan_id,
            &valid_plan_markdown("Ordered gate plan", "Respect every durable gate stage."),
        ),
    ])
    .unwrap();
    let turn = TurnContext::new(TurnId::new(77), SessionMode::Plan, CancellationToken::new());
    let mut gate = PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default());
    let call_batch = |message: Message| assistant_tool_calls(&message);

    let premature_submit = call_batch(submit_plan_call());
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

    let submit_before_reconciliation = call_batch(submit_plan_call());
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

    let unnecessary_question = call_batch(Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![named_tool_call(
            "duplicate-question",
            QUESTION_TOOL_NAME,
            json!({}),
        )]),
    ));
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

    let valid_submit = call_batch(submit_plan_call());
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
        .tool(CreatePlanStubTool)
        .tool(UpdatePlanStubTool)
        .tool(ReadPlanStubTool)
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
    let plan_id = PlanId::new();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_fixture(vec![
        TranscriptItem::Plan(Box::new(PlanRecord::Started { id: plan_id })),
        saved_plan_record(
            plan_id,
            &valid_plan_markdown("Ordered gate plan", "Respect every durable gate stage."),
        ),
    ])
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

    let mixed = assistant_tool_calls(&Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![
            named_tool_call("same-batch-question", QUESTION_TOOL_NAME, json!({})),
            match submit_plan_call() {
                Message::Assistant(rig_core::message::AssistantMessage { mut content, .. }) => {
                    content.remove(0)
                }
                _ => unreachable!(),
            },
        ]),
    ));
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

    let malformed = assistant_tool_calls(&Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![named_tool_call(
            "malformed-question",
            QUESTION_TOOL_NAME,
            serde_json::Value::String("{".to_string()),
        )]),
    ));
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
        TranscriptItem::Ensemble(Box::new(EnsembleRecord::ReportsReady {
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
        })),
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
        TranscriptItem::Message(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "tail-question",
                QUESTION_TOOL_NAME,
                json!({}),
            )]),
        )),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::tool_result(
                rig_core::message::CallId::from_wire("tail-question"),
                rig_core::message::ToolName::new(QUESTION_TOOL_NAME).expect("nonempty tool name"),
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
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![
                named_tool_call("commit-call", SKILL_TOOL_NAME, json!({"skill": "commit"})),
                named_tool_call(
                    "review-call",
                    SKILL_TOOL_NAME,
                    json!({"skill": "review", "args": "inspect"}),
                ),
            ]),
        )),
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
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![
                tool_call("call-echo", "ordinary"),
                launch_call("call-launch", "new explore task"),
            ]),
        )),
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
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![
                launch_call("call-a", "first explore task"),
                launch_call("call-b", "second explore task"),
            ]),
        )),
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
