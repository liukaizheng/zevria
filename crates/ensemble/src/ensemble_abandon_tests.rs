#[cfg(unix)]
#[tokio::test]
async fn ensemble_abandon_stops_process_closes_question_and_releases_sibling_capacity() {
    for hard_abort in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let script = directory.path().join("abandon.py");
        let pid_path = directory.path().join("worker.pid");
        std::fs::write(&script, r##"import json, os, sys
sid = os.environ['NAME']
if sid == 'a':
    with open(os.environ['PID'], 'w') as f: f.write(str(os.getpid()))
def send(v): print(json.dumps(v), flush=True)
def options(): return [{'id':'mode','name':'Mode','category':'mode','type':'select','currentValue':'read-only','options':[{'value':'read-only','name':'Read only'}]}]
for line in sys.stdin:
    m = json.loads(line); method = m.get('method'); rid = m.get('id')
    if method == 'initialize': send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentCapabilities':{},'agentInfo':{'name':'abandon-fixture','version':'1'}}})
    elif method == 'session/new': send({'jsonrpc':'2.0','id':rid,'result':{'sessionId':sid,'configOptions':options()}})
    elif method == 'session/set_config_option': send({'jsonrpc':'2.0','id':rid,'result':{'configOptions':options()}})
    elif method == 'session/prompt':
        if sid == 'a':
            send({'jsonrpc':'2.0','id':902,'method':'elicitation/create','params':{'mode':'form','sessionId':sid,'message':'Abandon this worker','requestedSchema':{'type':'object','properties':{'answer':{'type':'string'}},'required':['answer']}}})
            for n in range(300):
                send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'plan_update','plan':{'type':'markdown','planId':'excluded','content':'# Excluded ' + str(n)}}}})
            # Never settle, even after session/cancel. Host must stop us.
        else:
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'plan_update','plan':{'type':'markdown','planId':'survivor','content':'# Survivor only'}}}})
            send({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
"##).unwrap();
        let make_agent = |name: &str| fake_stdio_agent(&script, BTreeMap::from([("NAME".into(), name.into()), ("PID".into(), pid_path.display().to_string())]));
        let config = EnsembleConfig {
            plan_agents: vec!["a".into(), "b".into()], review_agents: vec!["a".into(), "b".into()],
            max_concurrent_agents: 1, review_startup_timeout_seconds: 1, review_turn_timeout_seconds: 1,
            cancel_grace_seconds: 1, max_synthesis_bytes_per_agent: 16_384,
            agents: BTreeMap::from([("a".into(), make_agent("a")), ("b".into(), make_agent("b"))]),
        };
        let logs = directory.path().join("logs");
        let (events, mut receiver) = session_event_channel(256);
        let questions = question_channels(events.clone());
        let mut supervisor = EnsembleSupervisor::new(config, &workspace, logs.clone(), questions.requester).unwrap();
        if hard_abort {
            // Test-only fault injection: expire both driver and persistence
            // deadlines immediately, including drop-scoped callback cleanup.
            supervisor.config.cancel_grace_seconds = 0;
        }
        let start = zevria_workflow::EnsembleStart { run_id: EnsembleRunId::new(), workflow: EnsembleWorkflow::Plan, prompt: "initial".into(), agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap() };
        let mut states = start.agents.iter().cloned().map(WorkerReviewState::new).collect::<Vec<_>>();
        let initial = WorkerInput { generation: 1, request_id: WorkerControlId::new(), kind: WorkerPromptKind::Initial, text: start.prompt.clone() };
        states[0].apply(&WorkerReviewEvent::InputAccepted { input: initial.clone() }).unwrap();
        let turn = TurnContext::new(TurnId::new(91), SessionMode::Plan, CancellationToken::new());
        let mut execution = supervisor.start_review(EnsembleLaunchRequest { start: start.clone(), resume: false }, states.clone(), events, turn.clone()).unwrap();
        let question = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver)).await.unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let question_id = question.id.clone();
        let drain = tokio::spawn(async move {
            let mut closed_tx = Some(closed_tx);
            while let Some(update) = receiver.recv().await {
                if matches!(update, zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::QuestionClosed { request_id, .. }) if request_id == question_id)
                    && let Some(tx) = closed_tx.take() { let _ = tx.send(()); }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while execution.updates.len() < WORKER_CONTROL_CAPACITY {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("fixture saturates bounded updates before the preceding mirror");
        // Queue both discarded feedback and sibling work before exclusion.
        execution.commands[&states[0].descriptor.id].send(WorkerActorCommand::Prompt(WorkerInput { generation: 2, request_id: WorkerControlId::new(), kind: WorkerPromptKind::UserFeedback, text: "must never dispatch".into() })).unwrap();
        states[1].apply(&WorkerReviewEvent::InputAccepted { input: initial.clone() }).unwrap();
        execution.commands[&states[1].descriptor.id].send(WorkerActorCommand::Prompt(initial)).unwrap();
        // A preceding mirror exercises the independently polled command pump.
        execution.commands[&states[0].descriptor.id].send(WorkerActorCommand::Mirror(WorkerReviewEvent::CancelRequested { generation: 1 })).unwrap();
        let request_id = WorkerControlId::new();
        states[0].apply(&WorkerReviewEvent::Abandoned { request_id: request_id.clone() }).unwrap();
        execution.commands[&states[0].descriptor.id].send(WorkerActorCommand::Abandon { outcome: Box::new(states[0].outcome()), request_id }).unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            while states[1].eligible_snapshot().is_none() {
                let update = execution.updates.recv().await.unwrap();
                if update.worker_id == states[1].descriptor.id { states[1].apply(&update.event).unwrap(); }
            }
        }).await.expect("abandoned worker releases its active-work capacity before mirror persistence");
        tokio::time::timeout(Duration::from_secs(5), closed_rx).await.unwrap().unwrap();
        assert!(!questions.responder.respond(&question.id, QuestionResponse::Dismissed));
        assert!(!turn.is_cancelled());
        let pid = std::fs::read_to_string(&pid_path).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::process::Command::new("kill").args(["-0", pid.trim()]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap().success() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("ACP subprocess must actually exit, not merely lose its host task");
        tokio::time::timeout(Duration::from_secs(5), execution.commands[&states[0].descriptor.id].closed()).await.unwrap();
        let records = load_agent_run(&agent_run_path(&logs, &start.run_id, &states[0].descriptor.id)).unwrap();
        assert!(!records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Review { event } } if matches!(event.as_ref(), WorkerReviewEvent::Dispatched { generation: 2, .. }))));
        if !hard_abort {
            assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { outcome } if outcome.is_sanitized_abandonment())));
        }
        execution.cancellation.cancel();
        for sender in execution.commands.values() { sender.closed().await; }
        drain.abort();
    }
}

#[tokio::test]
async fn ensemble_abandoned_recovery_skips_startup_finalization_and_valid_late_evidence() {
    for mirror in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let logs = directory.path().join("logs");
        let supervisor = EnsembleSupervisor::new(single_agent_config(fake_stdio_agent(&directory.path().join("must-not-exist.py"), BTreeMap::new())), &workspace, logs.clone(), test_questions()).unwrap();
        let start = zevria_workflow::EnsembleStart { run_id: EnsembleRunId::new(), workflow: EnsembleWorkflow::Plan, prompt: "initial".into(), agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap() };
        let descriptor = start.agents[0].clone();
        let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
        let mut state = WorkerReviewState::new(descriptor.clone());
        let event = WorkerReviewEvent::Abandoned { request_id: WorkerControlId::new() };
        state.apply(&event).unwrap();
        if mirror {
            let mut writer = AgentRunTranscriptWriter::create(path.clone(), AgentRunTranscriptHeader { version: AGENT_RUN_TRANSCRIPT_VERSION, ensemble_run_id: start.run_id.clone(), workflow: start.workflow, prompt: start.prompt.clone(), descriptor: descriptor.clone() }).unwrap();
            writer.append(&AgentRunTranscriptRecord::Event { event: AgentRunEvent::Review { event: Box::new(event.clone()) } }).unwrap();
            writer.append(&AgentRunTranscriptRecord::Outcome { outcome: state.outcome() }).unwrap();
            drop(writer);
            assert!(supervisor.recover_review(&start, &[]).is_err(), "worker-only abandonment is not consent");
        }
        let before = std::fs::read(&path).ok();
        assert!(supervisor.recover_review(&start, &[(descriptor.id.clone(), event)]).unwrap().is_empty());
        let (events, _receiver) = session_event_channel(256);
        let turn = TurnContext::new(TurnId::new(92), SessionMode::Plan, CancellationToken::new());
        let request = EnsembleLaunchRequest { start: start.clone(), resume: true };
        let execution = supervisor.start_review(request.clone(), vec![state.clone()], events.clone(), turn.clone()).unwrap();
        assert!(execution.commands.is_empty());
        assert_eq!(supervisor.finalize_review(request, vec![state.outcome()], events, turn).await.unwrap(), vec![state.outcome()]);
        assert_eq!(std::fs::read(&path).ok(), before, "root exclusion never repairs or creates an abandoned sidecar");
    }
}
