// Parent-supervisor coverage for Zevria's native mode/proof/recovery contract.
use super::tests::{
    fake_stdio_agent, launch_fake_worker, named_single_agent_config, protocol_requests,
    test_questions,
};
use super::*;
use tokio_util::sync::CancellationToken;
use zevria_foundation::SessionMode;
use zevria_foundation::TurnId;
use zevria_session_api::session_event_channel;
use zevria_transcript::load_agent_run;

#[tokio::test]
async fn zevria_shaped_worker_normalizes_native_plan_and_review_and_rejects_mode_drift() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = directory.path().join("zevria-shaped.py");
    std::fs::write(&script, r#"import json, sys
mode = 'review'
plans = False

def send(value):
    print(json.dumps(value), flush=True)

def modes():
    return {'currentModeId':mode,'availableModes':[{'id':'plan','name':'Plan'},{'id':'review','name':'Review'}]}

for line in sys.stdin:
    msg = json.loads(line)
    method, rid = msg.get('method'), msg.get('id')
    if method == 'initialize':
        plans = 'plan' in msg['params']['clientCapabilities']
        send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True,'sessionCapabilities':{'resume':{}}}}})
    elif method == 'session/new':
        send({'jsonrpc':'2.0','id':rid,'result':{'sessionId':'zevria-shaped','modes':modes()}})
    elif method == 'session/set_mode':
        mode = msg['params']['modeId']
        assert mode in ['plan','review']
        assert mode != 'plan' or plans
        send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'zevria-shaped','update':{'sessionUpdate':'current_mode_update','currentModeId':mode}}})
        send({'jsonrpc':'2.0','id':rid,'result':{}})
    elif method == 'session/prompt':
        prompt = msg['params']['prompt'][0]['text']
        if 'drift-test' in prompt:
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'zevria-shaped','update':{'sessionUpdate':'current_mode_update','currentModeId':'build'}}})
        elif mode == 'plan':
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'zevria-shaped','update':{'sessionUpdate':'plan_update','plan':{'type':'markdown','planId':'native-artifact','content':'# Native Worker Plan\n\n- Preserve exact Markdown.  \n'}}}})
        else:
            assert not plans
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'zevria-shaped','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'Findings first: no actionable defects.'}}}})
        send({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
"#).unwrap();
    let mut agent = fake_stdio_agent(&script, BTreeMap::new());
    agent.label = "Zevria".into();
    agent.plan_mode = Some("plan".into());
    agent.review_mode = Some("review".into());
    let logs = zevria_transcript::agent_runs_dir(&workspace, "parent");
    let supervisor = EnsembleSupervisor::new(
        named_single_agent_config("zevria", agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .unwrap();
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let (_, outcome, records) =
            launch_fake_worker(&supervisor, &logs, workflow, "inspect independently").await;
        assert_eq!(
            outcome.status,
            if workflow == EnsembleWorkflow::Plan {
                AgentRunStatus::AwaitingConfirmation
            } else {
                AgentRunStatus::Completed
            },
            "{outcome:?}"
        );
        assert!(!outcome.partial);
        if workflow == EnsembleWorkflow::Plan {
            assert!(outcome.has_plan_proof());
            assert_eq!(
                outcome.plan.as_ref().unwrap().markdown.as_deref(),
                Some("# Native Worker Plan\n\n- Preserve exact Markdown.  \n")
            );
        } else {
            assert!(outcome.report.starts_with("Findings first"));
            assert!(outcome.plan.is_none());
        }
        let requests = protocol_requests(&records);
        if workflow == EnsembleWorkflow::Plan {
            assert!(
                requests
                    .iter()
                    .any(|request| request["method"] == "session/set_mode"
                        && request["params"]["modeId"] == "plan")
            );
        }
        assert_eq!(
            outcome.descriptor.safe_mode,
            if workflow == EnsembleWorkflow::Plan {
                "plan"
            } else {
                "review"
            }
        );
        if workflow == EnsembleWorkflow::Plan {
            assert!(outcome.confirmation.is_none());
            assert!(
                !records
                    .iter()
                    .any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
            );
        } else {
            assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { outcome: durable } if durable == &outcome)));
        }
    }
    let (_, outcome, _) =
        launch_fake_worker(&supervisor, &logs, EnsembleWorkflow::Plan, "drift-test").await;
    assert_eq!(outcome.status, AgentRunStatus::Blocked);
    assert!(!outcome.has_plan_proof());
}

#[tokio::test]
async fn real_parent_recovers_a_ready_native_zevria_child_from_public_transcript_records() {
    use zevria_foundation::ModelProfileRef;
    use zevria_transcript::transcript;
    use zevria_transcript::transcript::TranscriptItem;
    use zevria_transcript::transcript::TranscriptWriter;
    use zevria_workflow::EnsembleRecord;
    use zevria_workflow::PlanArtifact;
    use zevria_workflow::PlanId;
    use zevria_workflow::PlanRecord;
    use zevria_workflow::PlanVersion;
    // Cargo builds this binary for cli_acp as part of `cargo test -p zevria`.
    // No production test-only protocol method or command is needed.
    let test_executable = std::env::current_exe().unwrap();
    // Accommodate both Cargo's deps layout and its per-package artifact layout.
    let binary = test_executable.ancestors().skip(1).take(8)
        .map(|directory| directory.join(format!("zevria{}", std::env::consts::EXE_SUFFIX)))
        .find(|path| path.is_file() && path != &test_executable)
        .expect("build the real process fixture with cargo build -p zevria before running this unit test alone");
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let home = directory.path().join("home");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::write(workspace.join("source.txt"), "unchanged recovery source\n").unwrap();
    let config = directory.path().join("config.toml");
    let mut assignments = String::from("[modes]\n");
    for role in ["build", "plan", "review", "explore", "builder"] {
        assignments.push_str(&format!(
            "{role} = {{ provider = 'test', model = 'test-model', reasoning_level = 'medium' }}\n"
        ));
    }
    std::fs::write(&config, assignments).unwrap();
    std::fs::write(directory.path().join("models.jsonc"), serde_json::to_string_pretty(&serde_json::json!({
        "providers": {"test": {
            "base_url": "http://127.0.0.1:1/v1/responses", "api_key": "fixture-only", "supports_websockets": false,
            "models": {"test-model": {
                "context_window_tokens": 272000, "retained_user_tokens": 20000,
                "reasoning_levels": ["low", "medium", "high"], "reasoning_summary_level": "detailed"
            }}
        }}
    })).unwrap()).unwrap();
    let mut agent = EnsembleConfig::default().agents.remove("zevria").unwrap();
    agent.command = binary.to_str().unwrap().into();
    agent.env = BTreeMap::from([
        ("ZEVRIA_CONFIG".into(), config.to_str().unwrap().into()),
        ("HOME".into(), home.to_str().unwrap().into()),
    ]);
    let logs = zevria_transcript::agent_runs_dir(&workspace, "parent");
    let supervisor = EnsembleSupervisor::new(
        named_single_agent_config("zevria", agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .unwrap();
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "Recover independent analysis".into(),
        agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    let models = zevria_model::models::SessionModels::new(
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("test", "test-model"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("test", "test-model"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap();
    let artifact = PlanArtifact {
        version: PlanVersion { id: PlanId::new(), revision: 1 },
        title: "Recover Native Worker Proof".into(),
        markdown: "# Recover Native Worker Proof\n\n## Goal\nRecover.\n## Decisions\nRetain proof.\n## Implementation\nUse native Ready.\n## Validation\nTest recovery.\n## Risks\nNo provider is reachable.\n".into(),
        source_turn_id: TurnId::new(1),
    };
    let root_dir = transcript::sessions_dir(&workspace);
    let mut parent = TranscriptWriter::create_with_id(&root_dir, "parent").unwrap();
    let mut root_items = vec![
        TranscriptItem::SessionModels(models.clone()),
        TranscriptItem::Plan(PlanRecord::Started { id: PlanId::new() }),
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: start.run_id.clone(),
            version: ENSEMBLE_REVIEW_VERSION,
        }),
    ];
    let parent_path = parent.path().to_path_buf();
    let worker_dir = zevria_foundation::runtime_paths::workspace_state_root(&workspace)
        .join("ensemble-sessions");
    let mut child = TranscriptWriter::create_with_id(&worker_dir, "native-worker").unwrap();
    child
        .rewrite(&[
            TranscriptItem::SessionModels(models),
            TranscriptItem::Plan(PlanRecord::Started {
                id: artifact.version.id,
            }),
            TranscriptItem::Message(rig_core::message::Message::user("independent analysis")),
            TranscriptItem::Message(rig_core::message::Message::assistant(
                "worker report complete",
            )),
            TranscriptItem::Plan(PlanRecord::Ready {
                artifact: artifact.clone(),
            }),
        ])
        .unwrap();
    let child_path = child.path().to_path_buf();
    drop(child);
    let child_before = std::fs::read(&child_path).unwrap();
    let descriptor = start.agents[0].clone();
    let evidence_path = agent_run_path(&logs, &start.run_id, &descriptor.id);
    let mut evidence = AgentRunTranscriptWriter::create(
        evidence_path.clone(),
        AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            descriptor: descriptor.clone(),
            prompt: start.prompt.clone(),
        },
    )
    .unwrap();
    let mut state = WorkerReviewState::new(descriptor.clone());
    let input = WorkerInput {
        generation: 1,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::Initial,
        text: start.prompt.clone(),
    };
    for event in [
        WorkerReviewEvent::InputAccepted { input },
        WorkerReviewEvent::Dispatched {
            generation: 1,
            attempt: 1,
        },
    ] {
        state.apply(&event).unwrap();
        root_items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
            run_id: start.run_id.clone(),
            worker_id: descriptor.id.clone(),
            event: Box::new(event.clone()),
            result: None,
        }));
        evidence
            .append(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Review {
                    event: Box::new(event),
                },
            })
            .unwrap();
    }
    for event in [
        AgentRunEvent::SessionEstablished {
            session_id: "native-worker".into(),
            capabilities: serde_json::json!({"loadSession":true,"sessionCapabilities":{"resume":{}}}),
            safe_mode: "plan".into(),
            recovered: false,
        },
        AgentRunEvent::Prompt {
            text: "independent analysis".into(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
    ] {
        evidence
            .append(&AgentRunTranscriptRecord::Event { event })
            .unwrap();
    }
    let plan = zevria_workflow::AgentStructuredPlan {
        plan_id: Some("retained-native".into()),
        markdown: Some(artifact.markdown.clone()),
        entries: vec![],
    };
    evidence
        .append(&AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Plan { plan: plan.clone() },
        })
        .unwrap();
    let published = WorkerReviewEvent::Published {
        generation: 1,
        plan: plan.clone(),
        replay: false,
    };
    state.apply(&published).unwrap();
    root_items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: descriptor.id.clone(),
        event: Box::new(published),
        result: None,
    }));
    let mut settled_evidence = state.evidence.clone();
    settled_evidence.plan = Some(plan);
    settled_evidence.acp_session_id = Some("native-worker".into());
    let settled = WorkerReviewEvent::Settled {
        generation: 1,
        failure: None,
        connected: true,
        evidence: Box::new(settled_evidence),
    };
    state.apply(&settled).unwrap();
    root_items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkerReview {
        run_id: start.run_id.clone(),
        worker_id: descriptor.id.clone(),
        event: Box::new(settled.clone()),
        result: None,
    }));
    evidence
        .append(&AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Review {
                event: Box::new(settled),
            },
        })
        .unwrap();
    drop(evidence);
    parent.rewrite(&root_items).unwrap();
    let root = transcript::load(&parent_path).unwrap();
    let restored_start = root
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Ensemble(EnsembleRecord::Started { start }) => Some(start.clone()),
            _ => None,
        })
        .unwrap();
    let (events, _receiver) = session_event_channel(512);
    let turn = TurnContext::new(TurnId::new(2), SessionMode::Plan, CancellationToken::new());
    let mut execution = supervisor
        .start_review(
            EnsembleLaunchRequest {
                start: restored_start,
                resume: true,
            },
            vec![state.clone()],
            events,
            turn.clone(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let update = execution.updates.recv().await.unwrap();
            state.apply(&update.event).unwrap();
            if matches!(
                update.event,
                WorkerReviewEvent::Connection {
                    connected: true,
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("native Ready session reconnects without unsolicited model work");
    assert!(state.confirmation.is_none());
    assert_eq!(state.accepted_generation, 1);
    let receipt = WorkerConfirmationReceipt {
        request_id: WorkerControlId::new(),
        target: WorkerControlTarget {
            turn_id: turn.id,
            run_id: start.run_id.clone(),
            worker_id: descriptor.id.clone(),
        },
        revision: state.eligible_snapshot().unwrap().revision.clone(),
    };
    let result = WorkerControlResult {
        control: WorkerControl {
            request_id: receipt.request_id.clone(),
            target: receipt.target.clone(),
            action: WorkerControlAction::Confirm {
                expected_revision: receipt.revision.clone(),
            },
        },
        accepted: true,
        detail: "Explicit test user confirmation".into(),
    };
    state
        .apply(&WorkerReviewEvent::Confirmed { receipt })
        .unwrap();
    let outcomes = vec![state.outcome()];
    root_items.push(TranscriptItem::Ensemble(EnsembleRecord::WorkersConfirmed {
        run_id: start.run_id.clone(),
        final_confirmation: result,
        outcomes: outcomes.clone(),
    }));
    parent.rewrite(&root_items).unwrap();
    transcript::load(&parent_path).unwrap();
    let (acknowledgement, received) = oneshot::channel();
    execution.commands[&descriptor.id]
        .send(WorkerActorCommand::Finish {
            outcome: Box::new(outcomes[0].clone()),
            acknowledgement,
        })
        .unwrap();
    received.await.unwrap().unwrap();
    drop(execution);
    drop(parent);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].status,
        AgentRunStatus::Completed,
        "{:?}",
        outcomes[0]
    );
    assert!(!outcomes[0].partial);
    assert!(outcomes[0].has_plan_proof());
    assert_eq!(
        outcomes[0].plan.as_ref().unwrap().markdown.as_deref(),
        Some(artifact.markdown.as_str())
    );
    let records = load_agent_run(&evidence_path).unwrap();
    let requests = protocol_requests(&records);
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "session/resume"
                && request["params"]["sessionId"] == "native-worker")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["method"] == "session/new")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["method"] == "session/prompt")
    );
    assert!(outcomes[0].confirmation.is_some());
    assert_eq!(outcomes[0].descriptor.safe_mode, "plan");
    assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::SessionEstablished { safe_mode, recovered: true, .. } } if safe_mode == "plan")));
    assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { outcome } if outcome.has_plan_proof())));
    assert_eq!(
        std::fs::read(child_path).unwrap(),
        child_before,
        "Ready restoration and explicit host confirmation are not model turns"
    );
    assert_eq!(
        transcript::latest_session_file(&root_dir).unwrap(),
        Some(parent_path)
    );
    assert_eq!(transcript::list_sessions(&root_dir).unwrap().len(), 1);
    assert!(!transcript::plans_dir(&workspace).exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("source.txt")).unwrap(),
        "unchanged recovery source\n"
    );
}
