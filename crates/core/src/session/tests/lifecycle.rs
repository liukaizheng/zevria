use super::*;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

#[tokio::test]
async fn admission_rejections_are_event_only_and_preserve_transcript_bytes() {
    for case in [
        "empty",
        "unknown-skill",
        "mode-policy",
        "ready-plan",
        "stale-plan",
        "capacity",
        "missing-launcher",
        "no-workers",
        "worker-config",
        "degraded",
        "noncanonical-handoff",
        "nonempty-handoff",
        "missing-edit",
        "manual-before-prompt",
        "prompt-commit",
        "ensemble-commit",
        "decision-commit",
    ] {
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
        let (artifact, ready) = ready_plan_fixture();
        let submit = || TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "work".into(),
            mode: SessionMode::Build,
        };
        let ensemble = || TurnCommand::RunEnsemble {
            workflow: EnsembleWorkflow::Review,
            prompt: "review work".into(),
        };
        let command = match case {
            "empty" => TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: " \n\t".into(),
                mode: SessionMode::Build,
            },
            "unknown-skill" => TurnCommand::InvokeSkill {
                name: "missing".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Build,
            },
            "mode-policy" => TurnCommand::InvokeSkill {
                name: "review".parse().unwrap(),
                args: zevria_content::UserPrompt::default(),
                mode: SessionMode::Plan,
            },
            "ready-plan" | "stale-plan" | "decision-commit" => {
                engine.record_required_items(ready).unwrap();
                if case == "ready-plan" {
                    submit()
                } else {
                    TurnCommand::ResolvePlan {
                        expected: PlanVersion {
                            revision: artifact.version.revision + u32::from(case == "stale-plan"),
                            ..artifact.version
                        },
                        decision: PlanDecision::ImplementCurrent,
                    }
                }
            }
            "capacity" => {
                engine = engine.with_compaction_policy(test_compaction_policy(1_000, 80, 0));
                TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "x".repeat(20_000).into(),
                    mode: SessionMode::Build,
                }
            }
            "missing-launcher" => ensemble(),
            "no-workers" => {
                engine = engine.with_ensemble_launcher(Arc::new(StubEnsembleLauncher::successful(
                    Vec::<String>::new(),
                )));
                ensemble()
            }
            "worker-config" => {
                engine = engine.with_ensemble_launcher(Arc::new(StubEnsembleLauncher::invalid(
                    "invalid workers",
                )));
                ensemble()
            }
            "noncanonical-handoff" => {
                let mut handoff = PlanHandoff::new(artifact.clone(), "source");
                handoff.prompt = Message::user("not the artifact");
                TurnCommand::StartFromPlan { handoff }
            }
            "nonempty-handoff" => {
                engine
                    .record_required(TranscriptItem::Message(Message::user("existing")))
                    .unwrap();
                TurnCommand::StartFromPlan {
                    handoff: PlanHandoff::new(artifact.clone(), "source"),
                }
            }
            "missing-edit" => into_turn(prompt_message_edit(99, "replacement", SessionMode::Build)),
            "manual-before-prompt" => TurnCommand::Compact {
                mode: SessionMode::Build,
            },
            "prompt-commit" => TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan work".into(),
                mode: SessionMode::Plan,
            },
            "ensemble-commit" => {
                engine = engine
                    .with_ensemble_launcher(Arc::new(StubEnsembleLauncher::successful(["report"])));
                TurnCommand::RunEnsemble {
                    workflow: EnsembleWorkflow::Plan,
                    prompt: "plan work".into(),
                }
            }
            "degraded" => submit(),
            _ => unreachable!(),
        };
        let blocker = (case == "degraded" || case.ends_with("-commit"))
            .then(|| TranscriptRewriteBlocker::new(&path).expect("block transcript replacement"));
        if case == "degraded" {
            engine
                .record_completed_items(vec![TranscriptItem::Message(Message::assistant(
                    "already completed",
                ))])
                .unwrap_err();
        }
        let durable_path = blocker
            .as_ref()
            .map_or(path.as_path(), |blocker| blocker.backup_path());
        let before = std::fs::read(durable_path).unwrap();
        let items = engine.conversation.items().to_vec();
        let (events, mut receiver) = session_event_channel(16);
        engine
            .handle_command(SessionCommand::Turn(command), &events)
            .await
            .unwrap();
        assert_eq!(std::fs::read(durable_path).unwrap(), before, "{case}");
        assert_eq!(engine.conversation.items(), items, "{case}");
        assert!(
            engine.provider.requests.lock().unwrap().is_empty(),
            "{case}"
        );
        let lifecycle = collect_events(&mut receiver).await;
        assert!(
            matches!(lifecycle.last(), Some(SessionEvent::TurnRejected { turn_id, .. }) if *turn_id == TurnId::new(1)),
            "{case}: {lifecycle:?}"
        );
        assert_eq!(
            lifecycle
                .iter()
                .filter(|event| matches!(event, SessionEvent::TurnRejected { .. }))
                .count(),
            1,
            "{case}"
        );
        assert!(
            !lifecycle.iter().any(|event| matches!(
                event,
                SessionEvent::TurnStarted { .. }
                    | SessionEvent::EnsembleStarted { .. }
                    | SessionEvent::TurnFailed { .. }
                    | SessionEvent::TurnCancelled { .. }
            )),
            "{case}"
        );
    }
}

#[tokio::test]
async fn direct_control_answers_questions_without_allocating_a_turn() {
    let (_directory, transcript) = test_transcript();
    let (events, mut receiver) = session_event_channel(8);
    let questions = question_channels(events.clone());
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .unwrap()
    .with_question_responder(questions.responder);
    let question = tokio::spawn(async move {
        questions
            .requester
            .ask_request(
                QuestionRequest {
                    id: QuestionRequestId::new("direct"),
                    questions: Vec::new(),
                    source_label: None,
                    dismissible: true,
                },
                TurnContext::new(
                    TurnId::new(99),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });
    assert!(matches!(
        recv_event(&mut receiver).await,
        Some(SessionEvent::QuestionAsked { .. })
    ));
    engine
        .handle_command(
            SessionCommand::Control(ControlCommand::AnswerQuestion {
                request_id: QuestionRequestId::new("direct"),
                response: QuestionResponse::Dismissed,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), question)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        QuestionResponse::Dismissed
    );
    assert_eq!(engine.next_turn_id, 1);
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(zevria_transcript::transcript::is_leading_metadata)
    );
}

#[tokio::test]
async fn unconvertible_replay_fails_without_committing_the_fallback_message() {
    let provider = ScriptedProvider::with_model_responses([ModelResponse::from_replay(
        ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "future_output_type",
                "payload": true
            })],
        ),
    )]);
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "reject the fallback".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.history(), [Message::user("reject the fallback")]);
    assert_eq!(engine.provider.resets, 1);
    let lifecycle = collect_events(&mut receiver).await;
    assert!(matches!(
        lifecycle.last(),
        Some(SessionEvent::TurnFailed { error, .. })
            if error.contains("provider replay contained no assistant content")
    ));
    drop(engine);

    let restored = zevria_transcript::transcript::load(&transcript_path)
        .expect("failed turn transcript should restore");
    assert!(matches!(
        conversation_records(&restored).as_slice(),
        [
            TranscriptItem::Message(Message::User { .. }),
            TranscriptItem::Error { error }
        ] if error.contains("provider replay contained no assistant content")
    ));
    let raw = std::fs::read_to_string(transcript_path).expect("transcript should be readable");
    assert!(!raw.contains("must not be committed"));
    assert!(!raw.contains("zevria_provider_replay"));
}

#[tokio::test]
async fn captured_insert_decision_resolves_incident_without_duplicate_root_question() {
    let title = "Captured Insert behavior plan";
    let markdown = valid_plan_markdown(
        title,
        "Auto-exit Insert on turn start while leaving Select mode unchanged.",
    );
    let inspection = "rtk rg -n insert crates/tui/src";
    let decision = insert_scope_decision_batch();
    let reconciliation = captured_insert_reconciliation(&decision);
    let provider = ScriptedProvider::new([
        Ok(command_call("incident-inspection", inspection)),
        Ok(reconciliation_call(
            "incident-reconciliation",
            reconciliation,
        )),
        Ok(submit_plan_call(title, &markdown)),
        Ok(Message::assistant(
            "Plan submitted from the captured decision.",
        )),
    ]);
    let requests = provider.requests.clone();
    let launcher = Arc::new(StubEnsembleLauncher::successful_with_decision(
        [
            "Preserve Insert mode when a turn starts.",
            "Also auto-exit Insert mode when a turn starts.",
        ],
        1,
        decision.clone(),
    ));
    let command_calls = Arc::new(Mutex::new(Vec::new()));
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new()
            .tool(CommandTestTool {
                calls: command_calls.clone(),
            })
            .tool(ReconcileReportsStubTool)
            .tool(SubmitPlanStubTool)
            .run(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_ensemble_launcher(launcher);
    let (events, mut receiver) = session_event_channel(128);

    explicitly_confirm_proposals(
        &mut engine,
        SessionCommand::Turn(crate::session::TurnCommand::RunEnsemble {
            workflow: EnsembleWorkflow::Plan,
            prompt: "reconcile the Insert-mode scope".into(),
        }),
        &events,
    )
    .await
    .unwrap();

    assert_eq!(
        command_calls.lock().expect("command calls").as_slice(),
        [inspection]
    );
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Published { artifact }
            if artifact.markdown.contains("Auto-exit Insert")
                && artifact.markdown.contains("Select mode unchanged")
    ));
    {
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        let synthesis = serde_json::to_string(&requests[0].prompt).expect("synthesis input");
        assert!(synthesis.contains("userDecisions"));
        assert!(synthesis.contains("Also auto-exit Insert on turn start"));
        assert!(synthesis.contains("Leave Select mode as-is (Recommended)"));
    }

    let lifecycle = collect_events(&mut receiver).await;
    assert_eq!(
        lifecycle
            .iter()
            .filter(|event| matches!(event, SessionEvent::QuestionAsked { .. }))
            .count(),
        0
    );
    let tools = lifecycle
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Intermediate { message, .. } => assistant_tool_calls(message)
                .first()
                .map(|call| call.function.name.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tools,
        vec![
            "command".to_string(),
            RECONCILE_REPORTS_TOOL_NAME.to_string(),
            SUBMIT_PLAN_TOOL_NAME.to_string(),
        ]
    );
}

#[tokio::test]
async fn durable_root_command_transcript_resumes_without_redispatch() {
    let descriptor = zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "recovered".to_string(),
        label: "Recovered".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "recover this review".into(),
        agents: vec![descriptor.clone()],
    };
    let durable_command = command_call(
        "durable-inspection",
        "rtk rg -n ensemble_policy crates/core/src/session.rs",
    );
    let command = assistant_tool_calls(&durable_command)
        .into_iter()
        .next()
        .expect("durable command call");
    let durable_result = Message::User {
        content: vec![UserContent::ToolResult(ToolResult {
            call: command.id.clone(),
            provider: command.provider.clone(),
            name: command.function.name.clone(),
            content: vec![ToolResultContent::text("durable repository evidence")],
        })],
    };
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("durable untrusted synthesis input"),
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
        TranscriptItem::Message(durable_command.clone()),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: durable_result.clone(),
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: command.id.to_string(),
                call_id: None,
                tool_name: "command".to_string(),
                outcome: ToolCallOutcome::Success,
                detail: None,
            }],
        },
    ];
    let recovery = latest_ensemble_recovery(items.iter().filter_map(|item| match item {
        TranscriptItem::Ensemble(record) => Some(record),
        _ => None,
    }))
    .expect("unfinished synthesis recovery");
    let provider = ScriptedProvider::new([Ok(Message::assistant("recovered synthesis"))]);
    let requests = provider.requests.clone();
    let command_calls = Arc::new(Mutex::new(Vec::new()));
    let launcher = Arc::new(StubEnsembleLauncher::successful(["must not relaunch"]));
    let launches = launcher.launches.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new()
            .tool(CommandTestTool {
                calls: command_calls.clone(),
            })
            .run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap()
    .with_ensemble_launcher(launcher);
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(
        TurnId::new(409),
        SessionMode::Build,
        CancellationToken::new(),
    );

    engine
        .resume_ensemble(recovery, &events, &turn)
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert!(command_calls.lock().expect("command calls").is_empty());
    {
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model_role, ModelRole::Review);
        assert_eq!(
            requests[0].allowed_tool_names,
            Some(vec![
                "command".to_string(),
                zevria_foundation::WEB_SEARCH_TOOL_NAME.to_string()
            ])
        );
        assert_eq!(requests[0].prompt, durable_result);
        assert!(requests[0].history.contains(&durable_command));
    }
    let lifecycle = collect_events(&mut receiver).await;
    assert!(lifecycle.iter().all(|event| !matches!(
        event,
        SessionEvent::AgentRunFinished { .. }
            | SessionEvent::Intermediate { .. }
            | SessionEvent::ToolResults { .. }
    )));
}

#[tokio::test]
async fn review_recovery_finishes_a_durable_final_response_without_another_model_call() {
    let descriptor = zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "recovered".to_string(),
        label: "Recovered".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "recover this review".into(),
        agents: vec![descriptor.clone()],
    };
    let final_message = Message::assistant("already durable review synthesis");
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("durable untrusted evidence"),
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
        TranscriptItem::Message(final_message.clone()),
    ];
    let recovery = latest_ensemble_recovery(items.iter().filter_map(|item| match item {
        TranscriptItem::Ensemble(record) => Some(record),
        _ => None,
    }))
    .expect("unfinished review synthesis");
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let requests = provider.requests.clone();
    let launcher = Arc::new(StubEnsembleLauncher::successful(["must not relaunch"]));
    let launches = launcher.launches.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap()
    .with_ensemble_launcher(launcher);
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(
        TurnId::new(405),
        SessionMode::Build,
        CancellationToken::new(),
    );

    engine
        .resume_ensemble(recovery, &events, &turn)
        .await
        .unwrap();

    assert!(requests.lock().expect("requests").is_empty());
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(
                item,
                TranscriptItem::Ensemble(EnsembleRecord::Completed { run_id })
                    if run_id == &start.run_id
            ))
            .count(),
        1
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnRecovered { turn_id, .. } if *turn_id == TurnId::new(405)))
    );
}

#[tokio::test]
async fn channel_closure_drains_both_sources_before_returning_none() {
    let (sender, mut receiver) = session_event_channel(4);
    let turn_id = TurnId::new(18);
    sender
        .send(SessionEvent::TurnStarted {
            turn_id,
            message: Message::user("question"),
            mode: SessionMode::Build,
        })
        .await
        .expect("lifecycle queues");
    sender.stream_updated(turn_id, Message::assistant("pending preview"));
    drop(sender);

    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Lifecycle(SessionEvent::TurnStarted { .. }))
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(SessionUpdate::Streams(SessionStreamBatch {
            root: Some(SessionStreamState {
                message: Some(Message::Assistant { .. }),
                ..
            }),
            ..
        }))
    ));
    assert_eq!(receiver.recv().await, None);
    assert_eq!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    );
}

#[tokio::test]
async fn question_answer_resumes_the_same_turn_and_persists_as_a_tool_result() {
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![named_tool_call(
                "question-call",
                QUESTION_TOOL_NAME,
                json!({}),
            )],
        }),
        Ok(Message::assistant("Plan updated from the answer.")),
    ]);
    let requests = provider.requests.clone();
    let (events_tx, mut receiver) = session_event_channel(32);
    let questions = question_channels(events_tx.clone());
    let tools = ToolServer::new()
        .tool(QuestionStubTool {
            requester: questions.requester,
        })
        .run();
    let policies = SessionPolicies::new(
        TurnPolicy::new(
            "Build test instructions",
            Some(Vec::new()),
            ModelRole::Build,
            false,
        ),
        TurnPolicy::new(
            "Plan test instructions",
            Some(vec![QUESTION_TOOL_NAME.to_string()]),
            ModelRole::Plan,
            false,
        ),
    );
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let engine = SessionEngine::new(
        provider,
        tools,
        policies,
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_question_responder(questions.responder);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let engine_task = tokio::spawn(engine.run(command_rx, events_tx));

    commands
        .send(SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "plan the change".into(),
            mode: SessionMode::Plan,
        }))
        .expect("submit command");

    let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(SessionEvent::QuestionAsked { request, .. }) =
                recv_event(&mut receiver).await
            {
                break request;
            }
        }
    })
    .await
    .expect("question should arrive promptly");
    let response = QuestionResponse::Answered {
        answers: vec![QuestionAnswer {
            id: "scope".to_string(),
            answer: Some(QuestionAnswerValue::String("Focused".to_string())),
        }],
    };
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::AnswerQuestion {
                request_id: request.id,
                response: response.clone(),
            },
        ))
        .expect("answer command");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if matches!(
                recv_event(&mut receiver).await,
                Some(SessionEvent::TurnCompleted { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("turn should continue after the answer");
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ))
        .expect("shutdown command");
    drop(commands);
    engine_task
        .await
        .expect("engine join")
        .expect("valid replay");

    assert_eq!(requests.lock().expect("request lock").len(), 2);
    let persisted = zevria_transcript::transcript::load(&transcript_path).expect("load transcript");
    let persisted_response = persisted.iter().find_map(|item| {
        let TranscriptItem::ToolResults { message, .. } = item else {
            return None;
        };
        let Message::User { content } = message else {
            return None;
        };
        content.iter().find_map(|content| {
            let UserContent::ToolResult(result) = content else {
                return None;
            };
            result.content.iter().find_map(|content| match content {
                ToolResultContent::Text(text) => {
                    serde_json::from_str::<QuestionResponse>(&text.text).ok()
                }
                ToolResultContent::Image(_) | ToolResultContent::Json { .. } => None,
            })
        })
    });
    assert_eq!(persisted_response, Some(response));
    assert_eq!(
        persisted
            .iter()
            .filter(|item| zevria_transcript::transcript::is_prompt_item(item))
            .count(),
        1,
        "the answer is tool traffic, not a second user prompt"
    );
}

#[tokio::test]
async fn cancelling_an_active_turn_stops_the_provider_and_keeps_the_engine_responsive() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let provider = PendingProvider {
        cancelled: cancelled.clone(),
    };
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(16);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let engine_task = tokio::spawn(engine.run(command_rx, events));

    commands
        .send(SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "wait forever".into(),
            mode: SessionMode::Build,
        }))
        .expect("engine command channel");
    let turn_id =
        match tokio::time::timeout(std::time::Duration::from_secs(1), recv_event(&mut receiver))
            .await
            .expect("turn should start promptly")
            .expect("event channel remains open")
        {
            SessionEvent::TurnStarted { turn_id, .. } => turn_id,
            event => panic!("expected TurnStarted, got {event:?}"),
        };

    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(1), recv_event(&mut receiver))
                .await
                .expect("dispatch marker should arrive promptly")
                .expect("event channel");
        if let SessionEvent::ModelCallStarted { turn_id: id, call } = event {
            assert_eq!(id, turn_id);
            assert_eq!(call, 1);
            break;
        }
    }
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::CancelTurn {
                turn_id: Some(TurnId::new(turn_id.get() + 1)),
            },
        ))
        .expect("stale cancellation command");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(
        !cancelled.load(Ordering::SeqCst),
        "a mismatched turn ID must not cancel the active provider"
    );

    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::CancelTurn {
                turn_id: Some(turn_id),
            },
        ))
        .expect("cancellation command");
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(1), recv_event(&mut receiver))
                .await
                .expect("cancellation should resolve promptly")
                .expect("event channel remains open");
        if matches!(event, SessionEvent::TurnCancelled { turn_id: id } if id == turn_id) {
            break;
        }
    }
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(
        !engine_task.is_finished(),
        "the engine remains ready for commands"
    );

    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ))
        .expect("shutdown command");
    drop(commands);
    tokio::time::timeout(std::time::Duration::from_secs(1), engine_task)
        .await
        .expect("engine shutdown should not wedge")
        .expect("engine task should not panic")
        .expect("valid replay");
    assert!(matches!(
        conversation_records(&zevria_transcript::transcript::load(&transcript_path).expect("cancelled transcript remains readable")).as_slice(),
        [
            TranscriptItem::Message(Message::User { .. }),
            TranscriptItem::Error { error }
        ] if error == "turn cancelled by the user"
    ));
}

#[tokio::test]
async fn valid_submission_commits_ready_after_completion_and_projects_markdown() {
    let title = "Refactor approval workflow";
    let markdown = valid_plan_markdown(title, "Make Plan durable.");
    let provider = ScriptedProvider::new([
        Ok(submit_plan_call(title, &markdown)),
        Ok(Message::assistant("Plan ready for approval.")),
    ]);
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let workspace = tempfile::tempdir().expect("workspace");
    let plans_dir = workspace.path().join("plans");
    let session_id = transcript.session_id().to_string();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_plans_dir(plans_dir.clone());
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "make a plan".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    let artifact = match engine.plan_state().unwrap() {
        PlanWorkflowState::Ready { artifact } => artifact,
        state => panic!("expected Ready, got {state:?}"),
    };
    assert_eq!(artifact.version.revision, 1);
    let path = plans_dir.join(session_id).join(format!(
        "{}-refactor-approval-workflow.md",
        artifact.version.id
    ));
    assert_eq!(
        std::fs::read_to_string(path).expect("projection"),
        format!("{markdown}\n")
    );

    let events = collect_events(&mut receiver).await;
    let completed = events
        .iter()
        .position(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
        .expect("completion");
    let ready = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::PlanStateChanged {
                    state: PlanWorkflowState::Ready { .. }
                }
            )
        })
        .expect("ready state");
    assert!(completed < ready);
}

#[tokio::test]
async fn disabled_mode_suppresses_and_enabled_mode_restores_active_overlay() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("build activation complete")),
        Ok(Message::assistant("plan response")),
        Ok(Message::assistant("build response")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        test_skill_tools(),
        test_policies(),
        transcript,
        test_skills(),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::InvokeSkill {
                name: "commit".parse().unwrap(),
                args: "ship".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan without skills".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "return to build".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests");
    assert!(requests[0].skill_context.is_some());
    assert!(requests[1].skill_context.is_none());
    assert!(
        requests[1]
            .allowed_tool_names
            .as_ref()
            .is_some_and(|names| !names.iter().any(|name| name == SKILL_TOOL_NAME))
    );
    assert_eq!(
        requests[2]
            .skill_context
            .as_deref()
            .expect("restored Build overlay")
            .matches("Commit instructions")
            .count(),
        1
    );
}

#[tokio::test]
async fn multiple_tools_execute_sequentially_and_are_correlated() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool {
            calls: calls.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("tools".to_string()),
            content: vec![tool_call("first", "one"), tool_call("second", "two")],
        }),
        Ok(Message::assistant("done")),
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
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "go".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(*calls.lock().expect("calls lock"), ["one", "two"]);
    let history = engine.history();
    let Message::User { content } = &history[2] else {
        panic!("third message should be tool results");
    };
    assert_eq!(content.len(), 2);
}

#[tokio::test]
async fn structured_file_metadata_is_emitted_but_never_enters_model_messages() {
    let tools = ToolServer::new().tool(MetadataTool).run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![named_tool_call("file-call", "metadata", json!({}))],
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
    .expect("valid engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "change a file".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    let metadata = events.iter().find_map(|event| match event {
        SessionEvent::ToolResults {
            message, metadata, ..
        } => {
            let serialized = serde_json::to_string(message).expect("message");
            assert!(serialized.contains("plain model output"));
            assert!(!serialized.contains("secret/path.rs"));
            assert!(!serialized.contains("metadata-only content"));
            Some(metadata)
        }
        _ => None,
    });
    let metadata = metadata.expect("tool-results event");
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].outcome, ToolCallOutcome::Success);
    assert_eq!(metadata[0].file_changes().len(), 1);

    let requests = requests.lock().expect("requests lock");
    let continuation = serde_json::to_string(&requests[1].prompt).expect("prompt");
    assert!(continuation.contains("plain model output"));
    assert!(!continuation.contains("secret/path.rs"));
    assert!(!continuation.contains("metadata-only content"));
    assert!(!continuation.contains("zevria_tool_result_metadata"));
}

#[tokio::test]
async fn structured_failures_have_typed_outcomes_without_file_changes() {
    let tools = ToolServer::new().tool(FailingTool).run();
    let calls = vec![
        named_tool_call("failed", "explode", json!({})),
        named_tool_call("missing", "unknown", json!({})),
    ]
    .into_iter()
    .map(|content| match content {
        AssistantContent::ToolCall(call) => call,
        _ => unreachable!("helper always builds a tool call"),
    })
    .collect::<Vec<_>>();

    let policies = test_policies();
    let (_directory, transcript) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        tools,
        policies.clone(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
    let submission_gate = PlanSubmissionGate::inert(None);
    let scope = ToolExecutionScope::new(
        &engine,
        SessionMode::Build,
        policies.policy(SessionMode::Build),
        &turn,
        &submission_gate,
    );
    let batch = execute_tool_calls(&scope, &calls, ActiveSkills::default(), &submission_gate).await;
    assert!(
        batch
            .metadata
            .iter()
            .all(|metadata| metadata.outcome == ToolCallOutcome::Error)
    );
    assert!(
        batch
            .metadata
            .iter()
            .all(|metadata| metadata.detail.is_none())
    );
    let serialized = serde_json::to_string(&batch.message).expect("message");
    assert_eq!(serialized.matches("status: error").count(), 2);
}

#[tokio::test]
async fn tool_dispatch_failures_are_model_visible_and_recoverable() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(EchoTool { calls })
        .tool(FailingTool)
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![
                named_tool_call("bad_args", "echo", json!({})),
                named_tool_call("unknown", "missing", json!({})),
                named_tool_call("failed", "explode", json!({})),
            ],
        }),
        Ok(Message::assistant("recovered")),
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
    .expect("valid engine");
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "exercise failures".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests lock");
    let Message::User { content } = &requests[1].prompt else {
        panic!("continuation prompt should contain tool results");
    };
    let serialized = serde_json::to_string(content).expect("results should serialize");
    for expected in [
        "bad_args",
        "missing field",
        "unknown",
        "tool `missing` not found",
        "failed",
        "expected execution failure",
    ] {
        assert!(
            serialized.contains(expected),
            "tool results missing {expected:?}: {serialized}"
        );
    }
    assert_eq!(
        engine.history().last(),
        Some(&Message::assistant("recovered"))
    );
}

#[tokio::test]
async fn transcript_records_authoritative_messages_in_order() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let writer = TranscriptWriter::create(directory.path()).expect("transcript writer");
    let path = writer.path().to_path_buf();
    let provider = ScriptedProvider::new([Ok(Message::assistant("recorded answer"))]);
    let tools = ToolServer::new().run();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "recorded question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    let expected = engine.history().to_vec();
    drop(engine);

    let recorded = zevria_transcript::transcript::load(&path).expect("transcript should load");
    let messages = recorded
        .iter()
        .filter_map(|item| item.message().cloned())
        .collect::<Vec<_>>();
    assert_eq!(messages, expected);
}

#[tokio::test]
async fn resumed_history_is_owned_and_replayed_by_the_engine() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("new answer"))]);
    let requests = provider.requests.clone();
    let prior = vec![
        Message::user("old question"),
        Message::assistant("old answer"),
    ];
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_history(prior.clone())
    .unwrap();
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "new question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests[0].history, prior);
    assert_eq!(requests[0].prompt, Message::user("new question"));
    assert_eq!(engine.history().len(), 4);
}

#[tokio::test]
async fn resumed_provider_messages_keep_their_native_replay_records() {
    let native_items = vec![json!({
        "type": "message",
        "id": "msg_old",
        "role": "assistant",
        "status": "completed",
        "content": [
            {"type": "output_text", "text": "old"},
            {"type": "output_text", "text": "answer"}
        ],
        "future_field": {"kept": true}
    })];
    let replay = ProviderReplay::openai_responses(test_profile(), native_items);
    let provider = ScriptedProvider::new([Ok(Message::assistant("new answer"))]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_fixture(vec![
        TranscriptItem::Message(Message::user("old question")),
        TranscriptItem::provider_message(replay.clone())
            .expect("native replay should derive a message"),
    ])
    .unwrap();
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "new question".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let requests = requests.lock().expect("requests lock");
    assert_eq!(requests[0].history_replays, vec![None, Some(replay)]);
    assert_eq!(requests[0].prompt_replay, None);
}

#[tokio::test]
async fn denied_launches_never_reach_the_supervisor() {
    let (events, mut receiver) = session_event_channel(1024);
    let mut channels = subtask_channels("root-session", events.clone());
    let tools = ToolServer::new()
        .tool(LaunchTestTool {
            launcher: channels.launcher.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![launch_call("call-a", "denied explore task")],
        }),
        Ok(Message::assistant("planned without it")),
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

    // The test Plan policy allows `command` only, so the launch is denied
    // before dispatch.
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan something".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(channels.requests.try_recv().is_err(), "nothing was queued");
    let events = collect_events(&mut receiver).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, SessionEvent::SubtaskLaunched { .. }))
    );
    let metadata = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { metadata, .. } => Some(metadata),
            _ => None,
        })
        .expect("tool results event");
    assert_eq!(metadata[0].outcome, ToolCallOutcome::Denied);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn supervisor_shutdown_mid_call_fails_the_launch_without_wedging() {
    let (events, mut receiver) = session_event_channel(1024);
    let channels = subtask_channels("root-session", events.clone());
    // Fake aborted supervisor: accept the request, then drop it — and the
    // whole channel — without resolving the outcome.
    let mut requests_rx = channels.requests;
    tokio::spawn(async move {
        let request = requests_rx.recv().await.expect("launch request");
        drop(request);
        drop(requests_rx);
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
            content: vec![launch_call("call-a", "abandoned explore task")],
        }),
        Ok(Message::assistant("noted the shutdown")),
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

    // Reaching a completed turn at all proves the dropped oneshot failed
    // the call instead of wedging the batch.
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "explore it".into(),
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
        .expect("tool results event");
    assert_eq!(metadata[0].outcome, ToolCallOutcome::Error);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn failed_launches_are_model_visible_and_recoverable() {
    let (events, mut receiver) = session_event_channel(1024);
    let mut channels = subtask_channels("root-session", events.clone());
    channels.requests.close();
    let tools = ToolServer::new()
        .tool(LaunchTestTool {
            launcher: channels.launcher.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![launch_call("call-a", "doomed explore task")],
        }),
        Ok(Message::assistant("noted the failure")),
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
                text: "try to launch".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, SessionEvent::SubtaskLaunched { .. }))
    );
    let tool_results = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults {
                message, metadata, ..
            } => Some((message, metadata)),
            _ => None,
        })
        .expect("tool results event");
    assert_eq!(tool_results.1[0].outcome, ToolCallOutcome::Error);
    let entries = tool_results.1[0].subtasks();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, SubtaskStatus::Failed);
    assert!(entries[0].launch.is_none());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
}

#[test]
fn provider_usage_baselines_require_the_same_request_shape() {
    let provider = ScriptedProvider::new([]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    engine.policies = test_policies_for_tools(&[]);
    let build = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&build).unwrap();
    engine.report_provider_usage(&build, 900).unwrap();

    assert_eq!(
        engine
            .local_context_candidates(&build)
            .expect("Build candidates")
            .2,
        Some(900)
    );

    let plan = engine.policies.policy(SessionMode::Plan).clone();
    assert_eq!(
        engine
            .local_context_candidates(&plan)
            .expect("Plan candidates")
            .2,
        None,
        "a same-profile baseline from another role must not be reused"
    );

    let mut changed_tools = build;
    changed_tools.allowed_tool_names = Some(vec!["echo".into()]);
    assert_eq!(
        engine
            .local_context_candidates(&changed_tools)
            .expect("changed-tool candidates")
            .2,
        None,
        "policy changes must invalidate the provider-usage baseline"
    );
}

#[tokio::test]
async fn exact_below_trigger_overrides_a_high_usage_tracker() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("done"))])
        .with_input_counts([Ok(InputTokenCount::Exact(400))]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![
        Message::user("earlier prompt"),
        Message::assistant("earlier answer"),
    ])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    engine.policies = test_policies_for_tools(&[]);
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.report_provider_usage(&policy, 1_500).unwrap();
    assert!(engine.compactable_context_tokens(&policy).unwrap() >= 500);
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "continue".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(requests.lock().expect("requests").len(), 1);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::ContextUsageUpdated { snapshot, .. }
                    if snapshot.source == ContextTokenSource::Exact
                        && snapshot.projected_input_tokens == 400
            ))
    );
}

#[tokio::test]
async fn exact_above_trigger_overrides_a_low_usage_tracker() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("summary")),
        Ok(Message::assistant("done")),
    ])
    .with_input_counts([
        Ok(InputTokenCount::Exact(600)),
        Ok(InputTokenCount::Exact(400)),
    ]);
    let count_calls = provider.input_count_calls.clone();
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_history(vec![
        Message::user("x".repeat(2_400)),
        Message::assistant("earlier answer"),
    ])
    .unwrap()
    .with_compaction_policy(test_compaction_policy(1_000, 50, 0));
    engine.policies = test_policies_for_tools(&[]);
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.report_provider_usage(&policy, 100).unwrap();
    assert!(engine.compactable_context_tokens(&policy).unwrap() < 500);
    let (events, mut receiver) = session_event_channel(32);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "continue".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(count_calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Compaction(_)))
            .count(),
        1
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(
                event,
                SessionEvent::ContextUsageUpdated { snapshot, .. }
                    if snapshot.source == ContextTokenSource::Exact
                        && snapshot.projected_input_tokens == 400
            ))
    );
}

#[tokio::test]
async fn opaque_replay_regression_uses_usage_delta_instead_of_legacy_wire_maximum() {
    let profile = test_profile();
    let mut history = Vec::new();
    for index in 0..120 {
        let replay = ProviderReplay::openai_responses(
            profile.clone(),
            vec![json!({
                "type": "reasoning",
                "id": format!("reasoning-{index}"),
                "summary": [],
                "content": [],
                "encrypted_content": "x".repeat(6_000),
                "status": null
            })],
        );
        history.push(TranscriptItem::provider_message(replay).expect("reasoning replay"));
    }
    history.push(TranscriptItem::Message(Message::user("r".repeat(400_000))));

    let legacy_tokens = history
        .iter()
        .filter_map(TranscriptItem::model_request_item)
        .map(|item| {
            let bytes = match item {
                ModelRequestItem::Message(message) => serde_json::to_vec(message),
                ModelRequestItem::RequestInstruction(directive) => {
                    serde_json::to_vec(&directive.render())
                }
                ModelRequestItem::DeveloperInstruction(directive) => {
                    serde_json::to_vec(&directive.text)
                }
                ModelRequestItem::ReplayBacked(content) => {
                    serde_json::to_vec(&content.replay().items)
                }
                ModelRequestItem::ReplayOnly(replay) => serde_json::to_vec(&replay.items),
            }
            .expect("legacy serialization");
            zevria_model::compaction::approximate_tokens_from_bytes(bytes.len())
        })
        .fold(0_u64, u64::saturating_add);
    assert!(legacy_tokens > 272_000, "legacy estimate: {legacy_tokens}");

    let provider = ScriptedProvider::new([Ok(Message::assistant("dispatched"))]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(history)
    .unwrap()
    .with_compaction_policy(test_compaction_policy(272_000, 90, 20_000));
    let build_policy = engine.policies.policy(SessionMode::Build).clone();
    engine
        .report_provider_usage(&build_policy, 177_700)
        .unwrap();
    let (events, mut receiver) = session_event_channel(64);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "continue".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(requests.lock().expect("requests").len(), 1);
    assert!(
        engine
            .conversation()
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Compaction(_)))
    );
    let events = collect_events(&mut receiver).await;
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ContextUsageUpdated { snapshot, .. }
            if snapshot.source == ContextTokenSource::UsagePlusDelta
                && snapshot.projected_input_tokens < 244_800
                && snapshot.input_token_limit == 272_000
    )));
    assert!(matches!(
        events.last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}
