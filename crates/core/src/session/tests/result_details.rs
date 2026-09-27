use super::*;
use zevria_foundation::SubtaskLaunchMetadata;

struct DetailTool {
    detail: Option<ToolResultDetail>,
    fail: bool,
    cancelled: bool,
}

impl Tool for DetailTool {
    // Deliberately not the name of any specialized producer: dispatch must
    // extract the extension, not infer its variant from the tool name.
    const NAME: &'static str = "metadata";
    type Args = NoArgs;
    type Error = ExpectedToolFailure;
    type Output = String;

    fn description(&self) -> String {
        "test exclusive result details".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }

    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
        if let Some(detail) = &self.detail {
            context.insert_result(detail.clone());
        }
        if self.cancelled {
            context.insert_result(ToolCancelled);
        }
        if self.fail {
            Err(ExpectedToolFailure)
        } else {
            Ok("plain model output".into())
        }
    }
}

fn specialized_details() -> [ToolResultDetail; 3] {
    [
        ToolResultDetail::FileChanges(vec![FileChangeOutput {
            path: "PRIVATE_PATH".into(),
            change: FileChange::Add {
                content: "PRIVATE_CONTENT".into(),
            },
        }]),
        ToolResultDetail::Subtasks(vec![zevria_foundation::SubtaskEntryMetadata {
            index: 0,
            status: zevria_foundation::SubtaskStatus::Completed,
            launch: Some(SubtaskLaunchMetadata {
                id: SubtaskId::new("PRIVATE_CHILD_ID"),
                title: "PRIVATE_TITLE".into(),
                kind: SubtaskKind::Build,
                workspace: Some("PRIVATE_WORKSPACE".into()),
            }),
        }]),
        ToolResultDetail::QuestionDisposition(QuestionTerminalDisposition::InvalidFrontendResponse),
    ]
}

#[tokio::test]
async fn dispatch_retains_details_with_partial_and_cancellation_precedence() {
    let mut cases = Vec::new();
    for detail in specialized_details()
        .into_iter()
        .map(Some)
        .chain([None, Some(ToolResultDetail::FileChanges(Vec::new()))])
    {
        let failed_outcome = if detail
            .as_ref()
            .is_some_and(|detail| !detail.file_changes().is_empty())
        {
            ToolCallOutcome::Partial
        } else {
            ToolCallOutcome::Error
        };
        let cancelled_outcome = if failed_outcome == ToolCallOutcome::Partial {
            // Surviving changes take precedence over a cancelled turn, but not
            // over an explicit cancellation observed by the running tool.
            ToolCallOutcome::Partial
        } else {
            ToolCallOutcome::Cancelled
        };
        for (fail, cancelled, turn_cancelled, expected) in [
            (false, false, false, ToolCallOutcome::Success),
            (true, false, false, failed_outcome),
            (true, true, false, ToolCallOutcome::Cancelled),
            (true, false, true, cancelled_outcome),
            (true, true, true, ToolCallOutcome::Cancelled),
            (false, false, true, ToolCallOutcome::Success),
        ] {
            cases.push((detail.clone(), fail, cancelled, turn_cancelled, expected));
        }
    }
    for (detail, fail, cancelled, turn_cancelled, expected) in cases {
        let tools = ToolServer::new()
            .tool(DetailTool {
                detail: detail.clone(),
                fail,
                cancelled,
            })
            .run();
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
        if turn_cancelled {
            turn.cancellation().cancel();
        }
        let AssistantContent::ToolCall(call) =
            named_tool_call("detail-call", "metadata", json!({}))
        else {
            unreachable!()
        };
        let slot = dispatch_tool_call(&tools, &call, &turn, None, None).await;
        assert_eq!(slot.metadata.outcome, expected);
        assert_eq!(slot.metadata.detail, detail);
        assert_eq!(slot.metadata.id, "detail-call");
        assert_eq!(
            slot.question_disposition,
            slot.metadata.question_disposition()
        );
        let wire = serde_json::to_string(&slot.result).unwrap();
        assert!(!wire.contains("PRIVATE_"));
        assert!(!wire.contains("invalid_frontend_response"));
        assert!(
            wire.contains(if fail {
                "expected execution failure"
            } else {
                "plain model output"
            }),
            "unexpected model result: {wire}"
        );
    }
}

struct TerminalQuestionTool(QuestionTerminalDisposition);

impl Tool for TerminalQuestionTool {
    const NAME: &'static str = QUESTION_TOOL_NAME;
    type Args = NoArgs;
    type Error = ExpectedToolFailure;
    type Output = String;

    fn description(&self) -> String {
        "test terminal question details".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }

    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
        context.insert_result(ToolResultDetail::QuestionDisposition(self.0));
        if matches!(
            self.0,
            QuestionTerminalDisposition::Unavailable
                | QuestionTerminalDisposition::InvalidFrontendResponse
        ) {
            Err(ExpectedToolFailure)
        } else {
            Ok("plain question result".into())
        }
    }
}

#[tokio::test]
async fn live_ensemble_question_gate_accepts_terminal_details_even_on_frontend_errors() {
    for disposition in [
        QuestionTerminalDisposition::Answered,
        QuestionTerminalDisposition::Dismissed,
        QuestionTerminalDisposition::Unavailable,
        QuestionTerminalDisposition::InvalidFrontendResponse,
    ] {
        let tools = ToolServer::new()
            .tool(TerminalQuestionTool(disposition))
            .run();
        let (_directory, transcript) = test_transcript();
        let engine = SessionEngine::new(
            ScriptedProvider::new([]),
            tools,
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let policy = TurnPolicy::new(
            "terminal question test",
            Some(vec![QUESTION_TOOL_NAME.into()]),
            ModelRole::Plan,
            false,
        );
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Plan, CancellationToken::new());
        let mut gate = PlanSubmissionGate::ensemble(ReportReconciliationCatalog::default());
        let ensemble = gate.ensemble.as_mut().unwrap();
        ensemble.inspection_completed = true;
        ensemble.reconciliation = Some(
            root_question_reconciliation()
                .validate(&ensemble.catalog)
                .unwrap(),
        );
        assert!(ensemble.requires_question());
        assert!(!ensemble.can_submit());
        let calls = assistant_tool_calls(&Message::Assistant {
            id: None,
            content: vec![named_tool_call(
                "terminal-question",
                QUESTION_TOOL_NAME,
                json!({}),
            )],
        });
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        let batch = execute_tool_calls(&scope, &calls, ActiveSkills::default(), &gate).await;
        assert_eq!(batch.metadata[0].question_disposition(), Some(disposition));
        assert_eq!(batch.question_disposition, Some(disposition));
        let expected_outcome = if matches!(
            disposition,
            QuestionTerminalDisposition::Unavailable
                | QuestionTerminalDisposition::InvalidFrontendResponse
        ) {
            ToolCallOutcome::Error
        } else {
            ToolCallOutcome::Success
        };
        assert_eq!(batch.metadata[0].outcome, expected_outcome);
        gate.apply_batch(&batch);
        assert!(gate.ensemble.as_ref().unwrap().can_submit());
        let scope = ToolExecutionScope::new(&engine, SessionMode::Plan, &policy, &turn, &gate);
        let repeated = execute_tool_calls(&scope, &calls, ActiveSkills::default(), &gate).await;
        assert_eq!(repeated.metadata[0].outcome, ToolCallOutcome::Denied);
        assert!(repeated.metadata[0].detail.is_none());
    }
}

#[test]
fn locally_denied_and_cancelled_calls_never_have_details() {
    let AssistantContent::ToolCall(call) = launch_call("not-started", "Never launched") else {
        unreachable!()
    };
    for slot in [
        denied_slot(&call, SessionMode::Plan),
        cancelled_slot(&call),
        candidate_locked_slot(&call),
    ] {
        assert!(slot.metadata.detail.is_none());
        assert!(slot.question_disposition.is_none());
    }
}

#[tokio::test]
async fn every_detail_variant_stays_out_of_provider_continuations() {
    for detail in specialized_details() {
        let tools = ToolServer::new()
            .tool(DetailTool {
                detail: Some(detail.clone()),
                fail: false,
                cancelled: false,
            })
            .run();
        let provider = ScriptedProvider::new([
            Ok(Message::Assistant {
                id: None,
                content: vec![named_tool_call("detail-call", "metadata", json!({}))],
            }),
            Ok(Message::assistant("done")),
        ]);
        let requests = provider.requests.clone();
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let (events, mut receiver) = session_event_channel(1024);
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "run the tool".into(),
                    mode: SessionMode::Build,
                }),
                &events,
            )
            .await
            .unwrap();
        let events = collect_events(&mut receiver).await;
        let metadata = events
            .iter()
            .find_map(|event| match event {
                SessionEvent::ToolResults { metadata, .. } => Some(metadata),
                _ => None,
            })
            .expect("result event");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].detail.as_ref(), Some(&detail));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let continuation = serde_json::to_string(&requests[1].prompt).unwrap();
        assert!(continuation.contains("plain model output"));
        for hidden in [
            "PRIVATE_",
            "invalid_frontend_response",
            "zevria_tool_result_metadata",
        ] {
            assert!(!continuation.contains(hidden), "leaked {hidden}");
        }
    }
}
