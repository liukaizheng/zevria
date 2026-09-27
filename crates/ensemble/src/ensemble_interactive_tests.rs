#[tokio::test]
async fn ensemble_restored_cancellation_settles_queued_feedback_without_starting_a_provider() {
    fn persist(
        writer: &mut AgentRunTranscriptWriter,
        journal: &mut zevria_transcript::WorkerReviewJournal,
        event: AgentRunEvent,
    ) {
        let record = AgentRunTranscriptRecord::Event { event };
        writer.append(&record).unwrap();
        journal.apply(&record).unwrap();
    }
    for mirrored in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let script = directory.path().join("must-not-start.py");
        let trace = directory.path().join("spawned");
        std::fs::write(
            &script,
            "import os\nopen(os.environ['TRACE'], 'w').write('unexpected provider startup')\n",
        )
        .unwrap();
        let logs = directory.path().join("logs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(fake_stdio_agent(
                &script,
                BTreeMap::from([("TRACE".into(), trace.display().to_string())]),
            )),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .unwrap();
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "initial plan".into(),
            agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
        };
        let descriptor = start.agents[0].clone();
        let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
        let header = AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            prompt: start.prompt.clone(),
            descriptor: descriptor.clone(),
        };
        let mut writer = AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap();
        let mut journal = zevria_transcript::WorkerReviewJournal::new(&header);
        for event in [
            AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::InputAccepted {
                    input: WorkerInput {
                        generation: 1,
                        request_id: WorkerControlId::new(),
                        kind: WorkerPromptKind::Initial,
                        text: start.prompt.clone(),
                    },
                }),
            },
            AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::Dispatched {
                    generation: 1,
                    attempt: 1,
                }),
            },
            AgentRunEvent::SessionEstablished {
                session_id: "previous-session".into(),
                capabilities: serde_json::json!({"loadSession":true}),
                safe_mode: "read-only".into(),
                recovered: false,
            },
            AgentRunEvent::Prompt {
                text: start.prompt.display_projection(),
                continuation: false,
                repair: None,
            },
            AgentRunEvent::Plan {
                plan: zevria_workflow::AgentStructuredPlan {
                    plan_id: Some("retained".into()),
                    markdown: Some("# Retained successful proposal".into()),
                    entries: vec![],
                },
            },
        ] {
            persist(&mut writer, &mut journal, event);
        }
        let mut evidence = journal.state.evidence.clone();
        evidence.acp_session_id = Some("previous-session".into());
        evidence.plan = journal
            .state
            .candidate
            .as_ref()
            .map(|snapshot| snapshot.plan.clone());
        persist(
            &mut writer,
            &mut journal,
            AgentRunEvent::Review {
                event: Box::new(WorkerReviewEvent::Settled {
                    generation: 1,
                    failure: None,
                    connected: true,
                    evidence: Box::new(evidence),
                }),
            },
        );
        let mut state = journal.state.clone();
        let retained = state.retained.clone();
        for event in [
            WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: 2,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::UserFeedback,
                    text: "accepted, then cancelled before actor delivery".into(),
                },
            },
            WorkerReviewEvent::CancelRequested { generation: 2 },
        ] {
            state.apply(&event).unwrap();
            if mirrored {
                persist(
                    &mut writer,
                    &mut journal,
                    AgentRunEvent::Review {
                        event: Box::new(event),
                    },
                );
            }
        }
        drop(writer);
        let mut state: WorkerReviewState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let turn = TurnContext::new(TurnId::new(82), SessionMode::Plan, CancellationToken::new());
        let (events, mut receiver) = session_event_channel(256);
        let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        let mut execution = supervisor
            .start_review(
                EnsembleLaunchRequest {
                    start,
                    resume: true,
                },
                vec![state.clone()],
                events,
                turn,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.settled_generation < 2 {
                let update = execution
                    .updates
                    .recv()
                    .await
                    .expect("restored actor update");
                state.apply(&update.event).unwrap();
            }
        })
        .await
        .expect("durably cancelled feedback settles without provider IO");
        assert_eq!(state.cancel_requested, None);
        assert_eq!(state.eligible_snapshot(), retained.as_ref());
        assert!(state.confirmation.is_none());
        assert!(!state.connected);
        assert!(
            state
                .diagnostic
                .as_deref()
                .unwrap()
                .contains("cancelled before startup")
        );
        assert!(
            !trace.exists(),
            "restored cancellation must prevent provider startup, session/new, and session/prompt"
        );
        let records = load_agent_run(&path).unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::Prompt { .. }
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::SessionEstablished { .. }
                    }
                ))
                .count(),
            1
        );
        drop(execution);
        drain.abort();
    }
}

#[tokio::test]
async fn ensemble_interactive_worker_cancellation_cancels_held_elicitation_and_keeps_session() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = directory.path().join("held_elicitation_acp.py");
    let trace = directory.path().join("requests.jsonl");
    std::fs::write(
        &script,
        r#"import json
import os
import sys

sid = "held-cancel-session"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    with open(os.environ["TRACE"], "a") as output:
        output.write(json.dumps(message, separators=(",", ":")) + "\n")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"held-elicitation-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":sid,"configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        prompt = message["params"]["prompt"][0]["text"]
        if "hold-elicitation" in prompt:
            send({"jsonrpc":"2.0","id":902,"method":"elicitation/create","params":{"mode":"form","sessionId":sid,"message":"Wait for worker cancellation","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}}})
            cancelled = False
            while True:
                response = json.loads(sys.stdin.readline())
                with open(os.environ["TRACE"], "a") as output:
                    output.write(json.dumps(response, separators=(",", ":")) + "\n")
                if response.get("method") == "session/cancel":
                    cancelled = True
                    continue
                if response.get("id") == 902:
                    sys.stderr.write("worker-cancel-response:" + json.dumps(response, separators=(",", ":")) + "\n")
                    sys.stderr.flush()
                    break
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"cancelled" if cancelled else "end_turn"}})
        else:
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
    )
    .unwrap();
    let agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("TRACE".into(), trace.display().to_string())]),
    );
    let logs = directory.path().join("logs");
    let (events, mut receiver) = session_event_channel(256);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        questions.requester,
    )
    .unwrap();
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "hold-elicitation".into(),
        agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
    };
    let descriptor = start.agents[0].clone();
    let mut state = WorkerReviewState::new(descriptor.clone());
    let initial = WorkerInput {
        generation: 1,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::Initial,
        text: start.prompt.clone(),
    };
    state
        .apply(&WorkerReviewEvent::InputAccepted { input: initial })
        .unwrap();
    let turn = TurnContext::new(TurnId::new(83), SessionMode::Plan, CancellationToken::new());
    let mut execution = supervisor
        .start_review(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            vec![state.clone()],
            events,
            turn,
        )
        .unwrap();

    let question = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("held elicitation appears");
    let successor = WorkerInput {
        generation: 2,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: "after cancellation".into(),
    };
    state
        .apply(&WorkerReviewEvent::InputAccepted {
            input: successor.clone(),
        })
        .unwrap();
    execution.commands[&descriptor.id]
        .send(WorkerActorCommand::Prompt(successor))
        .unwrap();
    execution.commands[&descriptor.id]
        .send(WorkerActorCommand::CancelPrompt { generation: 1 })
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::QuestionClosed {
                request_id,
                ..
            })) = receiver.recv().await
                && request_id == question.id
            {
                break;
            }
        }
    })
    .await
    .expect("worker-local cancellation closes the held question");
    assert!(
        !questions
            .responder
            .respond(&question.id, QuestionResponse::Dismissed),
        "a cancelled worker question must not accept a late frontend answer"
    );

    let mut settled_first = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.settled_generation < 2 {
            let update = execution
                .updates
                .recv()
                .await
                .expect("worker update while cancelling held elicitation");
            if matches!(
                &update.event,
                WorkerReviewEvent::Settled { generation: 1, .. }
            ) {
                settled_first = true;
            }
            assert!(
                !matches!(
                    &update.event,
                    WorkerReviewEvent::Dispatched { generation: 2, .. }
                ) || settled_first,
                "a queued successor must not dispatch before cancellation settlement"
            );
            state.apply(&update.event).unwrap();
        }
    })
    .await
    .expect("held elicitation cancellation and queued successor settle");
    assert!(settled_first);
    assert!(state.connected, "worker-local cancellation keeps the ACP connection");
    assert_eq!(
        state.evidence.acp_session_id.as_deref(),
        Some("held-cancel-session")
    );

    let records = load_agent_run(&agent_run_path(&logs, &start.run_id, &descriptor.id)).unwrap();
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("worker-cancel-response") && text.contains("cancel")
    )));
    let messages = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        messages.iter().any(|message| {
            message["id"].as_i64() == Some(902) && message["result"]["action"] == "cancel"
        }),
        "worker-local cancellation must answer the held ACP elicitation with action=cancel"
    );
    let requests = protocol_requests(&records);
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "session/cancel")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "session/new")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "session/prompt")
            .count(),
        2
    );

    execution.cancellation.cancel();
    for sender in execution.commands.values() {
        sender.closed().await;
    }
}

#[tokio::test]
async fn ensemble_interactive_workers_keep_sessions_release_capacity_and_queue_feedback() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let script = directory.path().join("interactive.py");
    let trace = directory.path().join("requests.jsonl");
    std::fs::write(&script, r##"import json, os, sys, time
sid = 'persistent-' + os.environ['NAME']
hanging = None
def send(value):
    print(json.dumps(value), flush=True)
def options():
    return [{'id':'mode','name':'Mode','category':'mode','type':'select','currentValue':'read-only','options':[{'value':'read-only','name':'Read only'}]}]
def plan(text):
    send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'plan_update','plan':{'type':'markdown','planId':'plan','content':text}}}})
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method'); rid = message.get('id')
    with open(os.environ['TRACE'], 'a') as output:
        output.write(json.dumps({'worker':os.environ['NAME'], 'message':message}) + '\n')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':True},'agentInfo':{'name':'interactive','version':'1'}}})
    elif method == 'session/new':
        time.sleep(1.1)
        send({'jsonrpc':'2.0','id':rid,'result':{'sessionId':sid,'configOptions':options()}})
    elif method == 'session/load':
        assert message['params']['sessionId'] == sid
        plan('# Old history replay')
        send({'jsonrpc':'2.0','id':rid,'result':{'configOptions':options()}})
    elif method == 'session/set_config_option':
        send({'jsonrpc':'2.0','id':rid,'result':{'configOptions':options()}})
    elif method == 'session/prompt':
        assert message['params']['sessionId'] == sid
        text = message['params']['prompt'][0]['text']
        if text == 'hang':
            hanging = rid
            continue
        if text == 'fail':
            plan('# Failed round draft')
            send({'jsonrpc':'2.0','id':rid,'error':{'code':-32603,'message':'scripted provider failure'}})
        else:
            if text != 'prose': plan('# Proposal ' + os.environ['NAME'] + (' revised' if text == 'revise' else ''))
            time.sleep(.05)
            send({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
    elif method == 'session/cancel' and hanging is not None:
        send({'jsonrpc':'2.0','id':hanging,'result':{'stopReason':'cancelled'}})
        hanging = None
"##).unwrap();
    let make_agent = |name: &str| {
        fake_stdio_agent(
            &script,
            BTreeMap::from([
                ("NAME".into(), name.into()),
                ("TRACE".into(), trace.display().to_string()),
            ]),
        )
    };
    let config = EnsembleConfig {
        plan_agents: vec!["a".into(), "b".into()],
        review_agents: vec!["a".into(), "b".into()],
        max_concurrent_agents: 1,
        review_startup_timeout_seconds: 1,
        review_turn_timeout_seconds: 1,
        cancel_grace_seconds: 1,
        max_synthesis_bytes_per_agent: 16_384,
        agents: BTreeMap::from([("a".into(), make_agent("a")), ("b".into(), make_agent("b"))]),
    };
    let logs = directory.path().join("logs");
    let supervisor =
        EnsembleSupervisor::new(config, &workspace, logs.clone(), test_questions()).unwrap();
    let agents = supervisor.workers(EnsembleWorkflow::Plan).unwrap();
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan".into(),
        agents,
    };
    let mut states = start
        .agents
        .iter()
        .cloned()
        .map(WorkerReviewState::new)
        .collect::<Vec<_>>();
    for state in &mut states {
        state
            .apply(&WorkerReviewEvent::InputAccepted {
                input: WorkerInput {
                    generation: 1,
                    request_id: WorkerControlId::new(),
                    kind: WorkerPromptKind::Initial,
                    text: "plan".into(),
                },
            })
            .unwrap();
    }
    let turn = TurnContext::new(TurnId::new(81), SessionMode::Plan, CancellationToken::new());
    let (events, mut receiver) = session_event_channel(256);
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let mut execution = supervisor
        .start_review(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            states.clone(),
            events,
            turn.clone(),
        )
        .unwrap();
    async fn until(
        execution: &mut EnsembleReviewExecution,
        states: &mut [WorkerReviewState],
        done: impl Fn(&[WorkerReviewState]) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done(states) {
                let update = execution.updates.recv().await.expect("worker update");
                states
                    .iter_mut()
                    .find(|state| state.descriptor.id == update.worker_id)
                    .unwrap()
                    .apply(&update.event)
                    .unwrap();
            }
        })
        .await
        .expect("workers reach expected review state without approval");
    }
    until(&mut execution, &mut states, |states| {
        states
            .iter()
            .all(|state| state.eligible_snapshot().is_some())
    })
    .await;
    assert!(states.iter().all(|state| state.confirmation.is_none()));
    for state in &states {
        let records =
            load_agent_run(&agent_run_path(&logs, &start.run_id, &state.descriptor.id)).unwrap();
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
        );
    }
    let first_session = states[0].evidence.acp_session_id.clone();
    for text in ["prose", "revise"] {
        let state = &mut states[0];
        let input = WorkerInput {
            generation: state.accepted_generation + 1,
            request_id: WorkerControlId::new(),
            kind: WorkerPromptKind::UserFeedback,
            text: text.into(),
        };
        state
            .apply(&WorkerReviewEvent::InputAccepted {
                input: input.clone(),
            })
            .unwrap();
        execution.commands[&state.descriptor.id]
            .send(WorkerActorCommand::Prompt(input))
            .unwrap();
    }
    until(&mut execution, &mut states, |states| {
        states[0].settled_generation == 3
    })
    .await;
    assert_eq!(states[0].evidence.acp_session_id, first_session);
    assert_eq!(
        states[0]
            .eligible_snapshot()
            .unwrap()
            .plan
            .markdown
            .as_deref(),
        Some("# Proposal a revised")
    );
    let retained = states[0].retained.clone();
    let input = WorkerInput {
        generation: 4,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: "fail".into(),
    };
    states[0]
        .apply(&WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        })
        .unwrap();
    execution.commands[&states[0].descriptor.id]
        .send(WorkerActorCommand::Prompt(input))
        .unwrap();
    until(&mut execution, &mut states, |states| {
        states[0].settled_generation == 4
    })
    .await;
    assert_eq!(states[0].eligible_snapshot(), retained.as_ref());
    assert!(!states[0].connected);
    assert!(states[0].confirmation.is_none());
    // A subsequent input recovers the established conversation, not a new session.
    for text in ["revise", "hang"] {
        let input = WorkerInput {
            generation: states[0].accepted_generation + 1,
            request_id: WorkerControlId::new(),
            kind: WorkerPromptKind::UserFeedback,
            text: text.into(),
        };
        states[0]
            .apply(&WorkerReviewEvent::InputAccepted {
                input: input.clone(),
            })
            .unwrap();
        execution.commands[&states[0].descriptor.id]
            .send(WorkerActorCommand::Prompt(input))
            .unwrap();
        if text == "revise" {
            until(&mut execution, &mut states, |states| {
                states[0].settled_generation == 5
            })
            .await;
        }
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::read_to_string(&trace)
                .unwrap()
                .lines()
                .any(|line| {
                    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
                        value["message"]["params"]["prompt"][0]["text"] == "hang"
                    })
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hanging feedback dispatched");
    execution.commands[&states[0].descriptor.id]
        .send(WorkerActorCommand::CancelPrompt { generation: 6 })
        .unwrap();
    let input = WorkerInput {
        generation: 7,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: "revise".into(),
    };
    states[0]
        .apply(&WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        })
        .unwrap();
    execution.commands[&states[0].descriptor.id]
        .send(WorkerActorCommand::Prompt(input))
        .unwrap();
    until(&mut execution, &mut states, |states| {
        states[0].settled_generation >= 6
    })
    .await;
    assert!(
        states[0].connected,
        "cooperative prompt cancellation preserves the healthy connection"
    );
    assert!(
        states[0].eligible_snapshot().is_none(),
        "queued feedback cannot be bypassed by cancellation"
    );
    // A delayed cancellation for the old generation must not cancel its successor.
    execution.commands[&states[0].descriptor.id]
        .send(WorkerActorCommand::CancelPrompt { generation: 6 })
        .unwrap();
    until(&mut execution, &mut states, |states| {
        states[0].settled_generation == 7
    })
    .await;
    assert_eq!(states[0].evidence.acp_session_id, first_session);
    assert!(states[0].evidence.failure.is_none());
    assert!(states[0].connected);
    let retained_b = states[1].retained.clone();
    let input = WorkerInput {
        generation: 2,
        request_id: WorkerControlId::new(),
        kind: WorkerPromptKind::UserFeedback,
        text: "fail".into(),
    };
    states[1]
        .apply(&WorkerReviewEvent::InputAccepted {
            input: input.clone(),
        })
        .unwrap();
    execution.commands[&states[1].descriptor.id]
        .send(WorkerActorCommand::Prompt(input))
        .unwrap();
    until(&mut execution, &mut states, |states| {
        states[1].settled_generation == 2
    })
    .await;
    assert!(!states[1].connected);
    assert_eq!(states[1].eligible_snapshot(), retained_b.as_ref());
    // Explicit confirmation/baseline marking of b's disconnected fallback does not reconnect.
    // No prompt settlement or native publication implicitly confirmed a worker.
    for state in &mut states {
        state
            .apply(&WorkerReviewEvent::Confirmed {
                receipt: WorkerConfirmationReceipt {
                    request_id: WorkerControlId::new(),
                    target: WorkerControlTarget {
                        turn_id: turn.id,
                        run_id: start.run_id.clone(),
                        worker_id: state.descriptor.id.clone(),
                    },
                    revision: state.eligible_snapshot().unwrap().revision.clone(),
                },
            })
            .unwrap();
        if !state.connected {
            let mut marking = state.confirmation.clone().unwrap();
            marking.request_id = WorkerControlId::new();
            marking.target.turn_id = TurnId::new(turn.id.get() + 1);
            state.apply(&WorkerReviewEvent::BaselineMarked { receipt: marking }).unwrap();
        }
        let (tx, rx) = oneshot::channel();
        execution.commands[&state.descriptor.id]
            .send(WorkerActorCommand::Finish {
                outcome: Box::new(state.outcome()),
                acknowledgement: tx,
            })
            .unwrap();
        rx.await.unwrap().unwrap();
    }
    for state in &states {
        let records = load_agent_run(&agent_run_path(&logs, &start.run_id, &state.descriptor.id)).unwrap();
        let marks = records.iter().filter_map(|record| match record {
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Review { event } } => match event.as_ref() { WorkerReviewEvent::BaselineMarked { receipt } => Some(receipt), _ => None },
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(marks, state.baseline.iter().collect::<Vec<_>>());
        assert_eq!(records.last(), Some(&AgentRunTranscriptRecord::Outcome { outcome: state.outcome() }));
        if state.baseline.is_some() {
            // Crash after the authoritative root seal but before any terminal
            // audit mirror/outcome: retain only execution evidence in this log.
            let retained = records.iter().filter(|record| match record {
                AgentRunTranscriptRecord::Outcome { .. } => false,
                AgentRunTranscriptRecord::Event { event: AgentRunEvent::Review { event } } => !matches!(event.as_ref(), WorkerReviewEvent::Confirmed { .. } | WorkerReviewEvent::BaselineMarked { .. } | WorkerReviewEvent::Sealed),
                _ => true,
            }).map(|record| serde_json::to_string(record).unwrap()).collect::<Vec<_>>().join("\n") + "\n";
            std::fs::write(agent_run_path(&logs, &start.run_id, &state.descriptor.id), retained).unwrap();
        }
    }
    let outcomes = states.iter().map(WorkerReviewState::outcome).collect::<Vec<_>>();
    let (events, mut receiver) = session_event_channel(256);
    let replay_drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let finalized = supervisor.finalize_review(EnsembleLaunchRequest { start: start.clone(), resume: true }, outcomes.clone(), events, turn.clone()).await.unwrap();
    assert_eq!(finalized, outcomes);
    replay_drain.await.unwrap();
    let selected = states.iter().find(|state| state.baseline.is_some()).unwrap();
    let records = load_agent_run(&agent_run_path(&logs, &start.run_id, &selected.descriptor.id)).unwrap();
    assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Review { event } } if matches!(event.as_ref(), WorkerReviewEvent::BaselineMarked { receipt } if Some(receipt) == selected.baseline.as_ref()))));
    assert_eq!(records.last(), Some(&AgentRunTranscriptRecord::Outcome { outcome: selected.outcome() }));
    let requests = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .filter(|value| value["message"]["method"] == "session/new")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|value| value["message"]["method"] == "session/prompt")
            .count(),
        9
    );
    assert_eq!(
        requests
            .iter()
            .filter(|value| value["message"]["method"] == "session/load")
            .count(),
        1,
        "local cancellation does not reconnect a healthy process"
    );
    drop(execution);
    drain.abort();
}
