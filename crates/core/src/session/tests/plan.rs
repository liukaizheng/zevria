use super::*;

#[tokio::test]
async fn restoration_rejects_malformed_plan_records_in_tail_or_prefix() {
    let plan_id = PlanId::new();
    let malformed_ready = PlanRecord::Ready {
        artifact: PlanArtifact {
            version: PlanVersion {
                id: plan_id,
                revision: 1,
            },
            title: "Malformed retained workflow".to_string(),
            markdown: valid_plan_markdown(
                "Malformed retained workflow",
                "Exercise retained-prefix replay.",
            ),
            source_turn_id: TurnId::new(9),
        },
    };

    for prefix in [false, true] {
        let provider = ScriptedProvider::new([]);
        let requests = provider.requests.clone();
        let (_directory, mut transcript) = test_transcript();
        let mut items = vec![
            TranscriptItem::Message(Message::user("a potentially editable prompt")),
            TranscriptItem::Message(Message::assistant("old tail")),
        ];
        items.insert(
            usize::from(!prefix),
            TranscriptItem::Plan(malformed_ready.clone()),
        );
        persist_fixture(&mut transcript, &items);
        let path = transcript.path().to_path_buf();
        let bytes = std::fs::read(&path).unwrap();
        let result = SessionEngine::new(
            provider,
            ToolServer::new().run(),
            test_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .and_then(|engine| engine.with_fixture(items).map_err(anyhow::Error::from));
        assert!(
            matches!(result, Err(error) if matches!(error.downcast_ref::<SessionReplayError>(), Some(SessionReplayError::Plan(_))))
        );
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn plan_recovery_materializes_an_accepted_candidate_after_a_durable_final_response() {
    let title = "Crash-safe ensemble plan";
    let markdown = valid_plan_markdown(title, "Recover without a second submission.");
    let descriptor = zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "recovered".to_string(),
        label: "Recovered".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "recover this plan".into(),
        agents: vec![
            descriptor.clone(),
            zevria_workflow::AgentRunDescriptor {
                id: AgentRunId::new(),
                ..descriptor.clone()
            },
        ],
    };
    let plan_id = PlanId::new();
    let final_message = Message::assistant("The plan was submitted successfully.");
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::Started { id: plan_id }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("durable untrusted plan evidence"),
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
            "recovery-inspection",
            "rtk rg -n ensemble_policy crates/core/src/session.rs",
        )),
        successful_tool_result("recovery-inspection", "command", "inspection completed"),
        TranscriptItem::Message(reconciliation_call(
            "recovery-reconciliation",
            no_disagreement_reconciliation(),
        )),
        successful_tool_result(
            "recovery-reconciliation",
            RECONCILE_REPORTS_TOOL_NAME,
            "reconciliation accepted",
        ),
        TranscriptItem::Message(submit_plan_call(title, &markdown)),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    "submit-plan",
                    SUBMIT_PLAN_TOOL_NAME,
                    vec![ToolResultContent::text("Plan artifact accepted.")],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "submit-plan".to_string(),
                call_id: None,
                tool_name: SUBMIT_PLAN_TOOL_NAME.to_string(),
                outcome: ToolCallOutcome::Success,
                detail: None,
            }],
        },
        TranscriptItem::Message(final_message.clone()),
    ];
    let items = with_explicit_fixture_confirmations(items);
    let recovery = latest_ensemble_recovery(items.iter().filter_map(|item| match item {
        TranscriptItem::Ensemble(record) => Some(record),
        _ => None,
    }))
    .expect("unfinished plan synthesis");
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, mut receiver) = session_event_channel(64);
    let turn = TurnContext::new(
        TurnId::new(406),
        SessionMode::Plan,
        CancellationToken::new(),
    );

    engine
        .resume_ensemble(recovery, &events, &turn)
        .await
        .unwrap();

    assert!(requests.lock().expect("requests").is_empty());
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Published { artifact }
            if artifact.title == title && artifact.markdown == markdown
    ));
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Published { .. })))
            .count(),
        1
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnRecovered { turn_id, .. } if *turn_id == TurnId::new(406)))
    );
}

#[tokio::test]
async fn plan_recovery_with_a_published_artifact_only_adds_the_completion_marker() {
    let title = "Already-ready ensemble plan";
    let markdown = valid_plan_markdown(title, "Do not submit it twice.");
    let descriptor = zevria_workflow::AgentRunDescriptor {
        id: AgentRunId::new(),
        agent: "recovered".to_string(),
        label: "Recovered".to_string(),
        safe_mode: "read-only".to_string(),
    };
    let start = EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "recover this ready plan".into(),
        agents: vec![
            descriptor.clone(),
            zevria_workflow::AgentRunDescriptor {
                id: AgentRunId::new(),
                ..descriptor.clone()
            },
        ],
    };
    let plan_id = PlanId::new();
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: plan_id,
            revision: 1,
        },
        title: title.to_string(),
        markdown: markdown.clone(),
        source_turn_id: TurnId::new(407),
    };
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::Started { id: plan_id }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("durable untrusted plan evidence"),
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
            "recovery-inspection",
            "rtk rg -n ensemble_policy crates/core/src/session.rs",
        )),
        successful_tool_result("recovery-inspection", "command", "inspection completed"),
        TranscriptItem::Message(reconciliation_call(
            "recovery-reconciliation",
            no_disagreement_reconciliation(),
        )),
        successful_tool_result(
            "recovery-reconciliation",
            RECONCILE_REPORTS_TOOL_NAME,
            "reconciliation accepted",
        ),
        TranscriptItem::Message(submit_plan_call(title, &markdown)),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    "submit-plan",
                    SUBMIT_PLAN_TOOL_NAME,
                    vec![ToolResultContent::text("Plan artifact accepted.")],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "submit-plan".to_string(),
                call_id: None,
                tool_name: SUBMIT_PLAN_TOOL_NAME.to_string(),
                outcome: ToolCallOutcome::Success,
                detail: None,
            }],
        },
        // Even without the optional final prose, the Published artifact is
        // authoritative and must never trigger another provider synthesis.
        TranscriptItem::Plan(PlanRecord::Published {
            artifact: artifact.clone(),
            provenance: PlanPublicationProvenance::Synthesized,
        }),
    ];
    let items = with_explicit_fixture_confirmations(items);
    let recovery = latest_ensemble_recovery(items.iter().filter_map(|item| match item {
        TranscriptItem::Ensemble(record) => Some(record),
        _ => None,
    }))
    .expect("unfinished ready-plan synthesis");
    let provider = ScriptedProvider::new(std::iter::empty::<anyhow::Result<Message>>());
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        ToolServer::new().run(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, _receiver) = session_event_channel(64);
    let turn = TurnContext::new(
        TurnId::new(408),
        SessionMode::Plan,
        CancellationToken::new(),
    );

    engine
        .resume_ensemble(recovery, &events, &turn)
        .await
        .unwrap();

    assert!(requests.lock().expect("requests").is_empty());
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Published { artifact: recovered } if recovered == &artifact
    ));
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Published { .. })))
            .count(),
        1
    );
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
}

#[tokio::test]
async fn successive_plan_and_build_submissions_resolve_fresh_policies() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("plan ready")),
        Ok(Message::assistant("implemented")),
    ]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
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
                text: "make a plan".into(),
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
                text: "Implement the approved plan.".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();

    {
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].prompt, Message::user("make a plan"));
        assert!(requests[0].history.is_empty());
        assert_eq!(requests[0].model_role, ModelRole::Plan);
        assert_eq!(
            requests[0].instructions,
            engine.rendered_instructions(engine.policies.policy(SessionMode::Plan))
        );
        assert_eq!(
            requests[0].allowed_tool_names,
            Some(vec!["command".to_string()])
        );
        assert_eq!(
            requests[1].prompt,
            Message::user("Implement the approved plan.")
        );
        assert_eq!(
            requests[1].history,
            vec![
                Message::user("make a plan"),
                Message::assistant("plan ready")
            ]
        );
        assert_eq!(requests[1].model_role, ModelRole::Build);
        assert_eq!(
            requests[1].instructions,
            engine.rendered_instructions(engine.policies.policy(SessionMode::Build))
        );
        assert_eq!(requests[1].allowed_tool_names, None);
    }

    let started_modes = collect_events(&mut receiver)
        .await
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStarted { mode, .. } => Some(mode),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started_modes, [SessionMode::Plan, SessionMode::Build]);
}

#[tokio::test]
async fn plan_policy_allows_command_and_is_fixed_across_continuations() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(CommandTestTool {
            calls: calls.clone(),
        })
        .run();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: Some("plan-command".to_string()),
            content: vec![AssistantContent::ToolCall(ToolCall::from_dual_wire(
                "command-id",
                "provider-command-id",
                ToolFunction::new("command".to_string(), json!({"value": "inspect"})),
            ))],
        }),
        Ok(Message::assistant("plan ready")),
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
                text: "make a plan".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(*calls.lock().expect("calls lock"), ["inspect"]);
    {
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request.model_role == ModelRole::Plan
                && request.instructions
                    == engine.rendered_instructions(engine.policies.policy(SessionMode::Plan))
                && request.allowed_tool_names == Some(vec!["command".to_string()])
        }));
    }
    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        events.first(),
        Some(SessionEvent::TurnStarted {
            mode: SessionMode::Plan,
            ..
        })
    ));
}

#[tokio::test]
async fn plan_policy_denies_file_tools_before_dispatch_and_preserves_correlation() {
    let write_calls = Arc::new(Mutex::new(Vec::new()));
    let tools = ToolServer::new()
        .tool(WriteTestTool {
            calls: write_calls.clone(),
        })
        .run();
    let denied_calls = [
        ("write-id", "write-call", "write"),
        ("edit-id", "edit-call", "edit"),
        ("delete-id", "delete-call", "delete"),
    ]
    .into_iter()
    .map(|(id, call_id, name)| {
        AssistantContent::ToolCall(ToolCall::from_dual_wire(
            id,
            call_id,
            ToolFunction::new(name.to_string(), json!({"value": name})),
        ))
    })
    .collect::<Vec<_>>();
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: denied_calls,
        }),
        Ok(Message::assistant("recovered with a plan")),
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
                text: "plan file changes".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(write_calls.lock().expect("calls lock").is_empty());
    let events = collect_events(&mut receiver).await;
    let metadata = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { metadata, .. } => Some(metadata),
            _ => None,
        })
        .expect("denied tool-results event");
    assert_eq!(metadata.len(), 3);
    assert!(metadata.iter().all(|item| item.detail.is_none()));
    assert_eq!(
        metadata
            .iter()
            .map(|item| (
                item.id.as_str(),
                item.call_id.as_deref(),
                item.tool_name.as_str(),
                item.outcome,
                item.file_changes().len(),
            ))
            .collect::<Vec<_>>(),
        [
            (
                "write-call",
                Some("write-call"),
                "write",
                ToolCallOutcome::Denied,
                0,
            ),
            (
                "edit-call",
                Some("edit-call"),
                "edit",
                ToolCallOutcome::Denied,
                0,
            ),
            (
                "delete-call",
                Some("delete-call"),
                "delete",
                ToolCallOutcome::Denied,
                0,
            ),
        ]
    );
    let requests = requests.lock().expect("requests lock");
    let continuation = serde_json::to_string(&requests[1].prompt).expect("continuation");
    assert_eq!(continuation.matches("status: denied").count(), 3);
    for tool in ["write", "edit", "delete"] {
        assert!(continuation.contains(&format!("tool `{tool}` is unavailable in Plan mode")));
    }
}

#[tokio::test]
async fn ordinary_plan_final_remains_planning_without_approval() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("I need one more detail."))]);
    let tools = ToolServer::new().run();
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
                text: "make a plan".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    let events = collect_events(&mut receiver).await;
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning { .. }
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready { .. }
        }
    )));
}

#[tokio::test]
async fn identical_titles_in_different_sessions_have_distinct_projections() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Keep sessions isolated.");
    let workspace = tempfile::tempdir().expect("workspace");
    let plans_dir = workspace.path().join("plans");
    let (first_directory, first_transcript) = test_transcript();
    let (second_directory, second_transcript) = test_transcript();
    let first_session = first_transcript.session_id().to_string();
    let second_session = second_transcript.session_id().to_string();
    assert_ne!(first_session, second_session);

    let mut projected = Vec::new();
    for (transcript, session_id) in [
        (first_transcript, first_session),
        (second_transcript, second_session),
    ] {
        let provider = ScriptedProvider::new([
            Ok(submit_plan_call(title, &markdown)),
            Ok(Message::assistant("Ready.")),
        ]);
        let mut engine = SessionEngine::new(
            provider,
            ToolServer::new().tool(SubmitPlanStubTool).run(),
            plan_submission_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .expect("engine")
        .with_plans_dir(plans_dir.clone());
        let (events, _receiver) = session_event_channel(16);
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "plan it".into(),
                    mode: SessionMode::Plan,
                }),
                &events,
            )
            .await
            .unwrap();
        let artifact = engine
            .plan_state()
            .unwrap()
            .artifact()
            .expect("ready artifact");
        projected.push(plans_dir.join(session_id).join(format!(
            "{}-durable-approval-workflow.md",
            artifact.version.id
        )));
    }

    assert_ne!(projected[0], projected[1]);
    for path in projected {
        assert_eq!(
            std::fs::read_to_string(path).expect("session projection"),
            format!("{markdown}\n")
        );
    }
    drop((first_directory, second_directory));
}

#[test]
fn plan_projection_slugs_are_bounded_and_deterministic() {
    assert_eq!(
        slug_words("Fix The Parser Edge Cases"),
        Some("fix-the-parser-edge-cases".to_string())
    );
    assert_eq!(
        slug_words("One Two Three Four Five Six Seven Eight Nine"),
        Some("one-two-three-four-five-six-seven-eight".to_string())
    );
    assert_eq!(slug_words("!!!"), None);
}

#[tokio::test]
async fn build_turns_and_unconfigured_engines_never_write_plans() {
    let provider = ScriptedProvider::new([Ok(Message::assistant("built"))]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let workspace = tempfile::tempdir().expect("workspace");
    let plans_dir = workspace.path().join("plans");
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
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
                text: "build something".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(!plans_dir.exists());

    let provider = ScriptedProvider::new([Ok(Message::assistant("an unsaved plan"))]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut unconfigured = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine");
    unconfigured
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

    let events = collect_events(&mut receiver).await;
    assert!(events.iter().all(|event| !matches!(
        event,
        SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready { .. }
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn plan_projection_failures_preserve_ready_approval() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Keep transcript authority.");
    let provider = ScriptedProvider::new([
        Ok(submit_plan_call(title, &markdown)),
        Ok(Message::assistant("Ready.")),
    ]);
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let workspace = tempfile::tempdir().expect("workspace");
    // A file occupies the plans-directory path, so create_dir_all fails.
    let blocked_dir = workspace.path().join("plans");
    std::fs::write(&blocked_dir, "occupied").expect("blocker file");
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("valid engine")
    .with_plans_dir(blocked_dir);
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

    let emitted = collect_events(&mut receiver).await;
    let artifact = engine
        .plan_state()
        .unwrap()
        .artifact()
        .expect("Ready despite projection failure")
        .clone();
    assert_eq!(artifact.markdown, markdown);
    let completed = emitted
        .iter()
        .position(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
        .unwrap();
    let ready = emitted
        .iter()
        .position(|event| {
            matches!(
                event,
                SessionEvent::PlanStateChanged {
                    state: PlanWorkflowState::Ready { .. }
                }
            )
        })
        .unwrap();
    let warning = emitted
        .iter()
        .position(|event| matches!(event, SessionEvent::PlanProjectionWarning { .. }))
        .unwrap();
    assert!(completed < ready && ready < warning);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementFresh,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine.plan_state().unwrap(),
        &PlanWorkflowState::Resolved {
            artifact: artifact.clone(),
            resolution: PlanResolution::ImplementedFresh,
        }
    );
    let expected_handoff = PlanHandoff::new(artifact, engine.conversation().session_id());
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event,
                SessionEvent::FreshPlanHandoffRequested { handoff } if handoff == &expected_handoff
            )),
        "approval still uses the canonical transcript artifact"
    );
}

#[tokio::test]
async fn provider_failure_after_submit_plan_leaves_workflow_planning() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Do not commit early.");
    let provider = ScriptedProvider::new([
        Ok(submit_plan_call(title, &markdown)),
        Err(anyhow::anyhow!("provider failed after acceptance")),
    ]);
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan it".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning { .. }
    ));
    let events = collect_events(&mut receiver).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnFailed { .. }))
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready { .. }
        }
    )));
}

#[tokio::test]
async fn cancellation_after_submit_plan_acceptance_never_commits_ready() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Cancellation keeps planning open.");
    let cancelled = Arc::new(AtomicBool::new(false));
    let provider = FirstResponseThenPendingProvider {
        first: Some(submit_plan_call(title, &markdown)),
        cancelled: cancelled.clone(),
    };
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_path_buf();
    let engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(32);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let engine_task = tokio::spawn(engine.run(command_rx, events));

    commands
        .send(SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "plan it".into(),
            mode: SessionMode::Plan,
        }))
        .expect("submission command");
    let mut turn_id = None;
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(1), recv_event(&mut receiver))
                .await
                .expect("candidate acceptance should arrive promptly")
                .expect("event channel remains open");
        match event {
            SessionEvent::TurnStarted { turn_id: id, .. } => turn_id = Some(id),
            SessionEvent::ToolResults { metadata, .. }
                if metadata.iter().any(|entry| {
                    entry.tool_name == SUBMIT_PLAN_TOOL_NAME
                        && entry.outcome == ToolCallOutcome::Success
                }) =>
            {
                break;
            }
            _ => {}
        }
    }
    let turn_id = turn_id.expect("turn started before its tool result");
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
                .expect("cancellation should arrive promptly")
                .expect("event channel remains open");
        if matches!(event, SessionEvent::TurnCancelled { turn_id: id } if id == turn_id) {
            break;
        }
    }
    commands
        .send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ))
        .expect("shutdown command");
    drop(commands);
    tokio::time::timeout(std::time::Duration::from_secs(1), engine_task)
        .await
        .expect("engine should stop promptly")
        .expect("engine task should not panic")
        .expect("valid replay");

    assert!(cancelled.load(Ordering::SeqCst));
    let items =
        zevria_transcript::transcript::load(&transcript_path).expect("transcript should load");
    assert!(
        items
            .iter()
            .all(|item| { !matches!(item, TranscriptItem::Plan(PlanRecord::Ready { .. })) })
    );
    assert!(matches!(
        replay_plan_state(items.iter().filter_map(|item| match item {
            TranscriptItem::Plan(record) => Some(record),
            _ => None,
        }))
        .expect("workflow should replay"),
        PlanWorkflowState::Planning { .. }
    ));
}

#[tokio::test]
async fn duplicate_plan_submission_is_denied_after_first_acceptance() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "Accept once.");
    let provider = ScriptedProvider::new([
        Ok(Message::Assistant {
            id: None,
            content: vec![
                named_tool_call(
                    "submit-one",
                    SUBMIT_PLAN_TOOL_NAME,
                    json!({"title": title, "markdown": markdown}),
                ),
                named_tool_call(
                    "submit-two",
                    SUBMIT_PLAN_TOOL_NAME,
                    json!({"title": title, "markdown": markdown}),
                ),
            ],
        }),
        Ok(Message::assistant("Ready.")),
    ]);
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan it".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(
        engine
            .plan_state()
            .unwrap()
            .version()
            .map(|version| version.revision),
        Some(1)
    );
    let outcomes = collect_events(&mut receiver)
        .await
        .into_iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { metadata, .. } => Some(
                metadata
                    .into_iter()
                    .map(|entry| entry.outcome)
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .expect("tool result outcomes");
    assert_eq!(
        outcomes,
        [ToolCallOutcome::Success, ToolCallOutcome::Denied]
    );
}

#[tokio::test]
async fn stale_resolution_is_rejected_and_fresh_handoff_contains_exact_artifact() {
    let (artifact, items) = ready_plan_fixture();
    let provider = ScriptedProvider::new(Vec::<anyhow::Result<Message>>::new());
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    for item in &items {
        transcript.append(item).expect("seed transcript");
    }
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: PlanVersion {
                    id: artifact.version.id,
                    revision: 2,
                },
                decision: PlanDecision::ImplementFresh,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Ready { .. }
    ));

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementFresh,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Resolved {
            artifact: resolved,
            resolution: PlanResolution::ImplementedFresh,
        } if resolved == &artifact
    ));
    let handoff = collect_events(&mut receiver)
        .await
        .into_iter()
        .find_map(|event| match event {
            SessionEvent::FreshPlanHandoffRequested { handoff } => Some(handoff),
            _ => None,
        })
        .expect("fresh handoff");
    assert_eq!(handoff.artifact, artifact);
    assert!(handoff.is_canonical());
}

#[tokio::test]
async fn current_resolution_persists_semantic_handoff_then_runs_build() {
    let (artifact, items) = ready_plan_fixture();
    let provider = ScriptedProvider::new([Ok(Message::assistant("Implemented."))]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    for item in &items {
        transcript.append(item).expect("seed transcript");
    }
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, mut receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementCurrent,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Resolved {
            artifact: resolved,
            resolution: PlanResolution::ImplementedCurrent,
        } if resolved == &artifact
    ));
    let events = collect_events(&mut receiver).await;
    let handoff_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::PlanHandoffStarted { .. }))
        .expect("handoff event");
    let completed_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
        .expect("Build completion");
    assert!(handoff_index < completed_index);
    let handoff = events.iter().find_map(|event| match event {
        SessionEvent::PlanHandoffStarted { handoff, .. } => Some(handoff),
        _ => None,
    });
    assert_eq!(handoff.map(|handoff| &handoff.artifact), Some(&artifact));
    let request_prompt = requests.lock().expect("requests")[0].prompt.clone();
    assert_eq!(
        &request_prompt,
        &handoff.expect("semantic handoff event").prompt
    );
    assert!(engine.conversation.items().iter().any(|item| matches!(
        item,
        TranscriptItem::Plan(PlanRecord::Handoff { handoff })
            if handoff.artifact == artifact
    )));
}

#[tokio::test]
async fn retained_resolution_keeps_revision_context_and_uses_the_submitted_artifact() {
    let (artifact, items) = revising_plan_fixture();
    let provider = ScriptedProvider::new([Ok(Message::assistant("Implemented."))]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    for item in &items {
        transcript.append(item).expect("seed transcript");
    }
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, mut receiver) = session_event_channel(1024);

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning {
            previous: Some(previous),
            ..
        } if previous == &artifact
    ));
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementCurrent,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Resolved {
            artifact: resolved,
            resolution: PlanResolution::ImplementedCurrent,
        } if resolved == &artifact
    ));
    let emitted = collect_events(&mut receiver).await;
    let handoff = emitted
        .iter()
        .find_map(|event| match event {
            SessionEvent::PlanHandoffStarted { handoff, .. } => Some(handoff),
            _ => None,
        })
        .expect("semantic handoff event");
    assert_eq!(handoff.artifact, artifact);
    let requests = requests.lock().expect("requests");
    assert_eq!(requests[0].prompt, handoff.prompt);
    assert!(
        requests[0]
            .history
            .contains(&Message::user("Consider another edge case."))
    );
    assert!(requests[0].history.contains(&Message::assistant(
        "I am still working through that revision."
    )));
}

#[tokio::test]
async fn retained_fresh_resolution_is_versioned_and_revision_cannot_repeat() {
    let (artifact, items) = revising_plan_fixture();
    let provider = ScriptedProvider::new(Vec::<anyhow::Result<Message>>::new());
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    for item in &items {
        transcript.append(item).expect("seed transcript");
    }
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(items)
    .unwrap();
    let (events, mut receiver) = session_event_channel(1024);

    for (expected, decision) in [
        (
            PlanVersion {
                id: artifact.version.id,
                revision: artifact.version.revision + 1,
            },
            PlanDecision::ImplementFresh,
        ),
        (artifact.version, PlanDecision::Revise),
    ] {
        engine
            .handle_command(
                SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                    expected,
                    decision,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(matches!(
            engine.plan_state().unwrap(),
            PlanWorkflowState::Planning {
                previous: Some(previous),
                ..
            } if previous == &artifact
        ));
    }

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementFresh,
            }),
            &events,
        )
        .await
        .unwrap();

    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Resolved {
            artifact: resolved,
            resolution: PlanResolution::ImplementedFresh,
        } if resolved == &artifact
    ));
    let handoff = collect_events(&mut receiver)
        .await
        .into_iter()
        .find_map(|event| match event {
            SessionEvent::FreshPlanHandoffRequested { handoff } => Some(handoff),
            _ => None,
        })
        .expect("fresh handoff");
    assert_eq!(handoff.artifact, artifact);
    assert!(handoff.is_canonical());
}

#[tokio::test]
async fn fresh_session_starts_only_from_a_canonical_typed_handoff() {
    let (artifact, _) = ready_plan_fixture();
    let handoff = PlanHandoff::new(artifact.clone(), "source-session");
    let provider = ScriptedProvider::new([Ok(Message::assistant("Implemented fresh."))]);
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, mut receiver) = session_event_channel(32);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::StartFromPlan {
                handoff: handoff.clone(),
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    assert_eq!(requests.lock().expect("requests")[0].prompt, handoff.prompt);
    assert!(matches!(
        engine.conversation.items().iter().find(|item| !zevria_transcript::transcript::is_leading_metadata(item)),
        Some(TranscriptItem::Plan(PlanRecord::Handoff { handoff: stored }))
            if stored.artifact == artifact
    ));
    assert!(collect_events(&mut receiver).await.iter().any(|event| {
        matches!(event, SessionEvent::PlanHandoffStarted { handoff: live, .. }
            if live.artifact == artifact)
    }));

    let mut tampered = PlanHandoff::new(artifact, "source-session");
    tampered.prompt = Message::user("frontend-authored prompt");
    let provider = ScriptedProvider::new(Vec::<anyhow::Result<Message>>::new());
    let requests = provider.requests.clone();
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut rejected = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    rejected
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::StartFromPlan { handoff: tampered }),
            &events,
        )
        .await
        .unwrap();
    assert!(requests.lock().expect("requests").is_empty());
    assert!(
        rejected
            .conversation
            .items()
            .iter()
            .all(|item| { !matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { .. })) })
    );
}

#[tokio::test]
async fn direct_build_abandons_planning_but_ready_rejects_direct_submission() {
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("Still planning.")),
        Ok(Message::assistant("Built directly.")),
    ]);
    let tools = ToolServer::new().run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(1024);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan first".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Planning { .. }
    ));
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "build now".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);

    let (artifact, ready_items) = ready_plan_fixture();
    let provider = ScriptedProvider::new(Vec::<anyhow::Result<Message>>::new());
    let tools = ToolServer::new().run();
    let (_directory, mut transcript) = test_transcript();
    for item in &ready_items {
        transcript.append(item).expect("seed transcript");
    }
    let requests = provider.requests.clone();
    let mut ready = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
    .with_fixture(ready_items)
    .unwrap();
    ready
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "bypass approval".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        ready.plan_state().unwrap().version(),
        Some(artifact.version)
    );
    assert!(requests.lock().expect("requests").is_empty());
}

#[tokio::test]
async fn editing_earlier_prompt_truncates_later_plan_records_and_recomputes_state() {
    let title = "Durable approval workflow";
    let markdown = valid_plan_markdown(title, "This will be invalidated.");
    let provider = ScriptedProvider::new([
        Ok(Message::assistant("Initial answer.")),
        Ok(submit_plan_call(title, &markdown)),
        Ok(Message::assistant("Ready.")),
        Ok(Message::assistant("Revised initial answer.")),
    ]);
    let tools = ToolServer::new().tool(SubmitPlanStubTool).run();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine");
    let (events, _receiver) = session_event_channel(1024);
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "initial question".into(),
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
                text: "plan the change".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        engine.plan_state().unwrap(),
        PlanWorkflowState::Ready { .. }
    ));

    engine
        .handle_command(
            prompt_message_edit(0, "edited initial question", SessionMode::Build),
            &events,
        )
        .await
        .unwrap();

    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    assert!(
        engine
            .conversation
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::Plan(_)))
    );
    assert_eq!(
        engine.history(),
        [
            Message::user("edited initial question"),
            Message::assistant("Revised initial answer."),
        ]
    );
}

#[tokio::test]
async fn plan_revision_retains_id_path_and_increments_revision() {
    let first_title = "Durable approval workflow";
    let first = valid_plan_markdown(first_title, "First version.");
    let second_title = "Retitled durable workflow";
    let second = valid_plan_markdown(second_title, "Second version.");
    let provider = ScriptedProvider::new([
        Ok(submit_plan_call(first_title, &first)),
        Ok(Message::assistant("Ready one.")),
        Ok(submit_plan_call(second_title, &second)),
        Ok(Message::assistant("Ready two.")),
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
    let (events, _receiver) = session_event_channel(1024);

    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "plan it".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    let first_version = engine
        .plan_state()
        .unwrap()
        .version()
        .expect("first version");
    let projection = plans_dir
        .join(&session_id)
        .join(format!("{}-durable-approval-workflow.md", first_version.id));
    assert_eq!(
        std::fs::read_to_string(&projection).unwrap(),
        format!("{first}\n")
    );
    std::fs::write(&projection, "manual content is not canonical").unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: first_version,
                decision: PlanDecision::Revise,
            }),
            &events,
        )
        .await
        .unwrap();
    engine
        .handle_command(
            SessionCommand::Turn(crate::session::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "revise it".into(),
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(
        engine.plan_state().unwrap().artifact().unwrap().markdown,
        second
    );
    let second_version = engine
        .plan_state()
        .unwrap()
        .version()
        .expect("second version");
    assert_eq!(second_version.id, first_version.id);
    assert_eq!(second_version.revision, 2);
    let directory = plans_dir.join(session_id);
    let files = std::fs::read_dir(&directory)
        .expect("projection directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("entries");
    assert_eq!(files.len(), 1);
    assert!(
        files[0]
            .file_name()
            .to_string_lossy()
            .ends_with("-durable-approval-workflow.md")
    );
    assert_eq!(
        std::fs::read_to_string(files[0].path()).expect("revised projection"),
        format!("{second}\n")
    );
}
