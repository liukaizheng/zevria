// Synthetic supervisor regressions; never depend on private agent-run logs.
#[tokio::test]
async fn transient_continuation_sync_failure_prevents_dispatch_and_retains_original_error() {
    let probe = Arc::new(WriterProbe::default());
    let (events, mut receiver) = session_event_channel(16);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe.clone(), true, Duration::ZERO),
        test_publication(events),
        failure.clone(),
    );
    let log = RunLog {
        path: PathBuf::from("unused-transient-log"),
        writer,
        failure,
        evidence: Arc::new(Mutex::new(WorkerEvidenceState::default())),
        review_publication: Arc::new(Mutex::new(None)),
        event_order: Arc::new(tokio::sync::Mutex::new(())),
    };
    let mut state = TransientRecoveryState::default();
    let error = AcpError::new(-32603, "original ECONNRESET")
        .data(serde_json::json!({"errorKind": "server_error"}));
    state.observe("server_error", &error);
    assert!(
        state
            .failure_context("")
            .unwrap()
            .contains("continuation not scheduled")
    );
    assert!(state.reserve("server_error", &error));
    let failure = log_transient_continuation(EnsembleWorkflow::Plan, &log)
        .await
        .unwrap_err();
    assert!(error_with_login_hint(&failure, "").contains("scripted sync failure"));
    assert_eq!(probe.durable.load(Ordering::SeqCst), 0);
    assert!(log.repair().is_none());
    let context = state.failure_context("").unwrap();
    assert!(context.contains("scheduled but not dispatched"));
    assert!(context.contains("original ECONNRESET"));
    assert!(!state.reserve("overloaded", &AcpError::internal_error()));
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty
            | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn transient_prompt_classifier_accepts_only_structural_allowlisted_kinds() {
    for kind in ["server_error", "overloaded", "rate_limit"] {
        let direct = serde_json::json!({"errorKind": kind});
        let wrapped = serde_json::json!({"spawned_at": "src/jsonrpc.rs:1:1", "data": direct});
        for data in [
            direct,
            wrapped.clone(),
            serde_json::json!({"spawned_at": "outer", "data": wrapped}),
        ] {
            assert_eq!(
                transient_prompt_error_kind(&AcpError::internal_error().data(data)),
                Some(kind)
            );
        }
    }
    for data in [
        serde_json::Value::Null,
        serde_json::json!("server_error: ECONNRESET"),
        serde_json::json!({"errorKind": "unknown"}),
        serde_json::json!({"errorKind": "Server_Error"}),
        serde_json::json!({"errorKind": ["server_error"]}),
        serde_json::json!({"errorKind": null}),
        serde_json::json!({"data": {"errorKind": "server_error"}}),
        serde_json::json!({"spawned_at": 1, "data": {"errorKind": "server_error"}}),
        serde_json::json!({"spawned_at": "source", "extra": true, "data": {"errorKind": "server_error"}}),
        serde_json::json!({"wrapper": {"errorKind": "server_error"}}),
        serde_json::json!("Process exited with exit status: 9"),
        serde_json::json!({"reason": "incoming_transport_closed", "method": "session/prompt", "errorKind": "server_error"}),
        serde_json::json!({"spawned_at": "source", "data": {"reason": "incoming_transport_closed", "method": "session/prompt", "errorKind": "overloaded"}}),
    ] {
        let error = AcpError::internal_error().data(data);
        assert_eq!(transient_prompt_error_kind(&error), None, "{error:?}");
    }
    for error in [
        AcpError::new(
            -32603,
            "Internal error: API Error: Connection dropped (ECONNRESET)",
        ),
        AcpError::invalid_request(),
        AcpError::new(-32000, "Authentication required"),
    ] {
        assert_eq!(transient_prompt_error_kind(&error), None);
    }
}

const TRANSIENT_FAKE_AGENT: &str = r##"import json
import os
import sys
import time

def send(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

def count(name):
    path = os.path.join(os.environ["STATE_DIR"], name)
    try:
        value = int(open(path).read())
    except FileNotFoundError:
        value = 0
    with open(path, "w") as output:
        output.write(str(value + 1))
    return value + 1

process = count("processes")
scenario = os.environ["SCENARIO"]
session = "same-live-session"
mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

def update(value):
    send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":value}})

def failure(request, original=True):
    send({"jsonrpc":"2.0","id":request,"error":{"code":-32603,"message":"Internal error: API Error: Connection dropped (ECONNRESET)" if original else "continued prompt reset","data":{"errorKind":"server_error"}}})

def finish(request):
    if scenario == "repair-exhausted":
        send({"jsonrpc":"2.0","id":request,"result":{"stopReason":"end_turn"}})
        return
    plan = "# Recovered plan\n\n- Preserve the live session."
    if scenario == "oversize":
        plan += "x" * 5000
    update({"sessionUpdate":"tool_call","toolCallId":"exit","title":"Finish plan","kind":"switch_mode","status":"pending","rawInput":{"plan":plan},"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}})
    send({"jsonrpc":"2.0","id":"exit-permission","method":"session/request_permission","params":{"sessionId":session,"toolCall":{"toolCallId":"exit","kind":"switch_mode","rawInput":{"plan":plan},"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}},"options":[{"optionId":"stay-in-plan","name":"Reject once","kind":"reject_once"}]}})
    for line in sys.stdin:
        response = json.loads(line)
        if response.get("id") == "exit-permission":
            assert response["result"]["outcome"]["optionId"] == "stay-in-plan", response
            if scenario == "native-no-stop":
                return
            send({"jsonrpc":"2.0","id":request,"result":{"stopReason":"end_turn"}})
            return

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request = message.get("id")
    if method == "initialize":
        if scenario == "startup-transient" or (scenario == "failed-relaunch" and process == 2):
            failure(request)
        else:
            send({"jsonrpc":"2.0","id":request,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"transient-fake","version":"1"}}})
    elif method in ["session/new", "session/resume"]:
        if scenario == "session-transient":
            failure(request)
        else:
            result = {"configOptions":mode}
            if method == "session/new":
                result["sessionId"] = session
            send({"jsonrpc":"2.0","id":request,"result":result})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        assert message["params"]["sessionId"] == session, message
        turn = count("prompts")
        if turn == 1:
            raw = {"file_path":os.path.join(os.environ["CLAUDE_CONFIG_DIR"], "plans", "unfinished.md")} if scenario == "actionable-exhaustion" else {}
            update({"sessionUpdate":"tool_call","toolCallId":"abandoned-write","title":"Preparing file","kind":"edit","status":"pending","rawInput":raw,"_meta":{"claudeCode":{"toolName":"Write"}}})
        if scenario == "ordinary":
            send({"jsonrpc":"2.0","id":request,"error":{"code":-32602,"message":"ordinary protocol error"}})
        elif scenario == "auth-prompt":
            send({"jsonrpc":"2.0","id":request,"error":{"code":-32000,"message":"Authentication required"}})
        elif scenario == "abandoned-success":
            finish(request)
        elif (scenario == "crash-first" and turn == 1) or (scenario in ["transient-first", "failed-relaunch"] and turn == 2):
            time.sleep(0.15)
            os._exit(17)
        elif (scenario in ["repair-error", "repair-exhausted"] and turn == 1) or (scenario in ["transient-then-repair", "repair-shared-window"] and turn == 2):
            if scenario == "repair-shared-window":
                time.sleep(2)
            send({"jsonrpc":"2.0","id":request,"result":{"stopReason":"refusal"}})
        elif turn == 1 or (scenario in ["repair-error", "repair-exhausted"] and turn == 2) or scenario in ["exhaustion", "actionable-exhaustion", "crash-first", "transient-first", "backoff-crash"]:
            if scenario == "fresh-deadline":
                time.sleep(1)
            failure(request, original=(turn == 1 or scenario == "repair-error"))
            if scenario == "native-race":
                time.sleep(0.2)
                finish(request)
            elif scenario == "policy-race":
                time.sleep(0.2)
                update({"sessionUpdate":"current_mode_update","currentModeId":"build"})
            elif scenario == "backoff-crash" and process == 1:
                time.sleep(0.2)
                os._exit(18)
        elif scenario in ["cancel-continuation", "timeout", "timeout-exit"]:
            # Stay responsive to cancellation but do not finish this prompt.
            pass
        else:
            if scenario == "fresh-deadline":
                time.sleep(1)
            elif scenario == "repair-shared-window":
                time.sleep(2)
            finish(request)
    elif method == "session/cancel":
        if scenario == "timeout-exit":
            os._exit(19)
"##;

struct TransientFixture {
    _directory: tempfile::TempDir,
    state: PathBuf,
    logs: PathBuf,
    supervisor: EnsembleSupervisor,
}

impl TransientFixture {
    fn new(scenario: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let state = directory.path().join("state");
        let config = directory.path().join("claude-config");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir_all(config.join("plans")).unwrap();
        let script = directory.path().join("transient.py");
        std::fs::write(&script, TRANSIENT_FAKE_AGENT).unwrap();
        let mut agent = fake_stdio_agent(
            &script,
            BTreeMap::from([
                ("STATE_DIR".into(), state.display().to_string()),
                ("SCENARIO".into(), scenario.into()),
                (CLAUDE_CONFIG_DIR_ENV.into(), config.display().to_string()),
            ]),
        );
        // Deliberately not named "claude": only explicit transport enables it.
        agent.plan_handoff_transport = Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
        let mut config = single_agent_config(agent);
        if scenario == "fresh-deadline" {
            // Native capture needs the production grace window on the slower
            // Windows runner; the shared test helper's 1-second budget flakes.
            config.cancel_grace_seconds = 5;
        }
        config.review_turn_timeout_seconds = 3;
        let logs = directory.path().join("agent-runs");
        let supervisor =
            EnsembleSupervisor::new(config, &workspace, logs.clone(), test_questions()).unwrap();
        Self {
            _directory: directory,
            state,
            logs,
            supervisor,
        }
    }

    fn count(&self, name: &str) -> usize {
        std::fs::read_to_string(self.state.join(name))
            .ok()
            .map(|value| value.parse().unwrap())
            .unwrap_or(0)
    }

    async fn run(&self) -> (AgentRunOutcome, Vec<AgentRunTranscriptRecord>) {
        let (_, outcome, records) = launch_fake_worker(
            &self.supervisor,
            &self.logs,
            EnsembleWorkflow::Plan,
            "recover once",
        )
        .await;
        (outcome, records)
    }
}

fn recorded_prompts(records: &[AgentRunTranscriptRecord]) -> Vec<WorkerPrompt> {
    records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event:
                    AgentRunEvent::Prompt {
                        text,
                        continuation,
                        repair,
                    },
            } => Some(WorkerPrompt {
                text: text.clone().into(),
                continuation: *continuation,
                repair: repair.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn assert_unconfirmed_review(records: &[AgentRunTranscriptRecord]) {
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
            .count(),
        0,
        "settled protocol evidence remains in interactive review, never terminal approval"
    );
}

#[tokio::test]
async fn transient_incident_continues_same_session_and_completes_durable_native_handoff() {
    for scenario in ["incident", "abandoned-success", "native-race"] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        assert_eq!(
            outcome.status,
            AgentRunStatus::AwaitingConfirmation,
            "{scenario}: {outcome:?}"
        );
        assert!(!outcome.partial);
        assert!(outcome.failure.is_none());
        assert!(outcome.has_plan_proof());
        assert_eq!(fixture.count("processes"), 1);
        let requests = protocol_requests(&records);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "session/new")
                .count(),
            1
        );
        assert!(!requests.iter().any(|r| {
            ["session/resume", "session/load"]
                .iter()
                .any(|m| r["method"] == *m)
        }));
        let prompts = recorded_prompts(&records);
        assert_eq!(prompts.len(), if scenario == "incident" { 2 } else { 1 });
        if scenario == "incident" {
            assert_eq!(
                prompts[1],
                WorkerPrompt {
                    text: "continue".into(),
                    continuation: true,
                    repair: None
                }
            );
            let statuses = records
                .iter()
                .filter_map(|record| match record {
                    AgentRunTranscriptRecord::Event {
                        event: AgentRunEvent::Status { status, .. },
                    } => Some(*status),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                statuses
                    .windows(2)
                    .any(|pair| pair == [AgentRunStatus::Resuming, AgentRunStatus::Running])
            );
        }
        assert_eq!(fixture.count("prompts"), prompts.len());
        let projection = AgentRunProjection::from_records(&records);
        assert_eq!(
            projection
                .plan
                .as_ref()
                .and_then(|plan| plan.markdown.as_deref()),
            Some("# Recovered plan\n\n- Preserve the live session.")
        );
        assert_unconfirmed_review(&records);
    }
}

#[tokio::test]
async fn exhausted_transient_failure_keeps_acp_error_before_artifact_context() {
    for scenario in ["exhaustion", "actionable-exhaustion"] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        assert_eq!(outcome.status, AgentRunStatus::Blocked);
        assert!(outcome.partial);
        let failure = outcome.failure.unwrap();
        assert!(failure.starts_with("continued prompt reset"), "{failure}");
        assert!(failure.contains("server_error"));
        assert!(failure.contains("ECONNRESET"));
        assert!(failure.contains("continuation 1 of 1 dispatched"));
        if scenario == "actionable-exhaustion" {
            assert_eq!(
                failure
                    .matches("Claude plan artifacts remain unresolved:")
                    .count(),
                1
            );
            assert!(failure.contains("unfinished.md"));
        } else {
            assert_eq!(failure.matches("abandoned preparation").count(), 1);
        }
        assert_eq!(fixture.count("processes"), 1);
        assert_eq!(fixture.count("prompts"), 2);
        assert_unconfirmed_review(&records);
    }
}

#[tokio::test]
async fn transient_continuation_and_semantic_repair_have_independent_budgets() {
    for scenario in ["repair-error", "transient-then-repair", "repair-exhausted"] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        assert_eq!(
            outcome.status,
            if scenario == "repair-exhausted" {
                AgentRunStatus::AwaitingFeedback
            } else {
                AgentRunStatus::AwaitingConfirmation
            },
            "{outcome:?}"
        );
        let prompts = recorded_prompts(&records);
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts.iter().filter(|p| p.repair.is_some()).count(), 1);
        if scenario == "transient-then-repair" {
            assert_eq!(prompts[1].text, "continue".into());
            assert!(prompts[2].repair.is_some());
        } else {
            assert_eq!(
                prompts[1].text,
                semantic_repair_prompt(EnsembleWorkflow::Plan).into()
            );
            assert_eq!(prompts[2].text, prompts[1].text);
            assert!(prompts[2].repair.is_none());
        }
        assert_unconfirmed_review(&records);
    }
}

#[tokio::test]
async fn transient_budget_and_original_error_survive_process_relaunch_in_both_orders() {
    for scenario in [
        "crash-first",
        "transient-first",
        "backoff-crash",
        "failed-relaunch",
    ] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        assert_eq!(
            outcome.status,
            AgentRunStatus::Blocked,
            "{scenario}: {outcome:?}"
        );
        assert_eq!(fixture.count("processes"), 2);
        assert_eq!(
            fixture.count("prompts"),
            if ["backoff-crash", "failed-relaunch"].contains(&scenario) {
                2
            } else {
                3
            }
        );
        let failure = outcome.failure.unwrap();
        assert!(
            failure.contains("automatic same-session transient continuation 1 of 1"),
            "{failure}"
        );
        assert!(failure.contains("automatic process recovery attempt 1 of 1"));
        assert_eq!(records.iter().filter(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::Status { status: AgentRunStatus::Resuming, detail: Some(detail) } } if detail.contains("failed transiently"))).count(), 1);
        if scenario == "backoff-crash" {
            assert!(failure.contains("scheduled but not dispatched"));
        }
        assert_unconfirmed_review(&records);
    }
}

#[tokio::test]
async fn plan_continuation_has_no_deadline_and_review_retains_timeout_context() {
    let fixture = TransientFixture::new("fresh-deadline");
    let started = tokio::time::Instant::now();
    let (outcome, _) = fixture.run().await;
    assert!(started.elapsed() > Duration::from_secs(3));
    assert_eq!(
        outcome.status,
        AgentRunStatus::AwaitingConfirmation,
        "{outcome:?}"
    );
    for scenario in ["timeout", "timeout-exit"] {
        let fixture = TransientFixture::new(scenario);
        let (_, outcome, records) = launch_fake_worker(
            &fixture.supervisor,
            &fixture.logs,
            EnsembleWorkflow::Review,
            "review with bounded timeout",
        )
        .await;
        assert_eq!(
            outcome.status,
            AgentRunStatus::TimedOut,
            "{scenario}: {outcome:?}"
        );
        let failure = outcome.failure.unwrap();
        assert!(failure.starts_with("worker turn timed out"));
        assert!(failure.contains("ECONNRESET"));
        assert!(failure.contains("continuation 1 of 1 dispatched"));
        assert_eq!(fixture.count("prompts"), 2);
        assert_eq!(fixture.count("processes"), 1);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn plan_semantic_repair_has_no_deadline_while_review_shares_its_window() {
    let fixture = TransientFixture::new("repair-shared-window");
    let (outcome, records) = fixture.run().await;
    assert_eq!(
        outcome.status,
        AgentRunStatus::AwaitingConfirmation,
        "{outcome:?}"
    );
    assert_eq!(fixture.count("processes"), 1);
    assert_eq!(fixture.count("prompts"), 3);
    assert_eq!(
        recorded_prompts(&records)
            .iter()
            .filter(|prompt| prompt.repair.is_some())
            .count(),
        1
    );
    assert!(outcome.failure.is_none());
    assert_unconfirmed_review(&records);
    let fixture = TransientFixture::new("repair-shared-window");
    let (_, outcome, _) = launch_fake_worker(
        &fixture.supervisor,
        &fixture.logs,
        EnsembleWorkflow::Review,
        "bounded review",
    )
    .await;
    assert_eq!(outcome.status, AgentRunStatus::TimedOut);
    assert!(outcome.failure.unwrap().contains("ECONNRESET"));
}

#[tokio::test]
async fn transient_context_survives_policy_cancellation_but_successful_oversize_stays_a_proposal() {
    for scenario in ["policy-race", "oversize"] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        if scenario == "policy-race" {
            assert_eq!(outcome.status, AgentRunStatus::Blocked, "{outcome:?}");
            let failure = outcome.failure.unwrap();
            assert!(failure.contains("ECONNRESET"));
            assert_eq!(failure.matches("abandoned preparation").count(), 1);
            assert!(failure.contains("required safe mode"), "{failure}");
            assert!(failure.contains("scheduled but not dispatched"));
            assert_eq!(fixture.count("prompts"), 1);
        } else {
            assert_eq!(outcome.status, AgentRunStatus::AwaitingConfirmation);
            assert!(outcome.failure.is_none());
            assert!(
                validate_worker_synthesis_payload(
                    EnsembleWorkflow::Plan,
                    &outcome,
                    fixture.supervisor.config.max_synthesis_bytes_per_agent
                )
                .is_err()
            );
            assert_eq!(fixture.count("prompts"), 2);
        }
        assert_unconfirmed_review(&records);
    }
}

#[tokio::test]
async fn transient_cancellation_during_backoff_or_continuation_never_retries_again() {
    for during_continuation in [false, true] {
        let fixture = TransientFixture::new("cancel-continuation");
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "cancel recovery".into(),
            agents: fixture.supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
        };
        let cancellation = CancellationToken::new();
        let (events, mut receiver) = session_event_channel(1024);
        let launch = fixture.supervisor.observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(91), SessionMode::Build, cancellation.clone()),
        );
        let cancel = async {
            loop {
                if let zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
                    event,
                    ..
                }) = receiver
                    .recv()
                    .await
                    .expect("worker event channel closed before cancellation")
                {
                    let ready = match event {
                        AgentRunEvent::Status {
                            status: AgentRunStatus::Resuming,
                            ..
                        } => !during_continuation,
                        AgentRunEvent::Protocol {
                            direction: AgentProtocolDirection::ClientToAgent,
                            json,
                        } if during_continuation => {
                            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
                            value["method"] == "session/prompt"
                                && value["params"]["prompt"][0]["text"] == "continue"
                        }
                        _ => false,
                    };
                    if ready {
                        cancellation.cancel();
                        break;
                    }
                }
            }
        };
        let (outcomes, ()) = tokio::time::timeout(Duration::from_secs(12), async {
            tokio::join!(launch, cancel)
        })
        .await
        .unwrap();
        let outcome = &outcomes.unwrap()[0];
        assert_eq!(outcome.status, AgentRunStatus::Cancelled, "{outcome:?}");
        let failure = outcome.failure.as_deref().unwrap();
        assert!(failure.starts_with("ensemble turn cancelled"));
        assert!(failure.contains("ECONNRESET"));
        assert!(failure.contains(if during_continuation {
            "1 of 1 dispatched"
        } else {
            "scheduled but not dispatched"
        }));
        let records = load_agent_run(&agent_run_path(
            &fixture.logs,
            &start.run_id,
            &outcome.descriptor.id,
        ))
        .unwrap();
        assert_eq!(
            recorded_prompts(&records).len(),
            if during_continuation { 2 } else { 1 }
        );
        assert_eq!(fixture.count("processes"), 1);
        assert_eq!(records.iter().filter(|record| matches!(record, AgentRunTranscriptRecord::Outcome { outcome } if outcome.status == AgentRunStatus::Cancelled)).count(), 1);
    }
}

#[tokio::test]
async fn prompt_only_transient_recovery_does_not_retry_startup_auth_or_protocol_errors() {
    for scenario in [
        "startup-transient",
        "session-transient",
        "auth-prompt",
        "ordinary",
    ] {
        let fixture = TransientFixture::new(scenario);
        let (outcome, records) = fixture.run().await;
        assert_eq!(
            outcome.status,
            AgentRunStatus::Blocked,
            "{scenario}: {outcome:?}"
        );
        assert_eq!(fixture.count("processes"), 1);
        assert!(fixture.count("prompts") <= 1);
        assert!(!records.iter().any(|r| matches!(
            r,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Status {
                    status: AgentRunStatus::Resuming,
                    ..
                }
            }
        )));
    }
}

#[tokio::test]
async fn native_capture_without_a_settled_prompt_never_becomes_confirmable() {
    let fixture = TransientFixture::new("native-no-stop");
    let (outcome, records) = fixture.run().await;
    assert_eq!(outcome.status, AgentRunStatus::Blocked);
    assert!(
        outcome
            .failure
            .as_deref()
            .unwrap()
            .contains("native proposal stop did not settle")
    );
    assert!(outcome.confirmation.is_none());
    let projection = AgentRunProjection::from_records(&records);
    assert!(
        projection
            .review
            .unwrap()
            .state
            .eligible_snapshot()
            .is_none()
    );
    assert_eq!(fixture.count("processes"), 1);
    assert_eq!(fixture.count("prompts"), 2);
    assert_unconfirmed_review(&records);
}

#[test]
fn native_handoff_generations_reset_capture_and_reject_stale_refinements() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    let mut previous: Option<String> = None;
    for generation in 1..=3 {
        handoff.begin_generation().unwrap();
        assert!(!handoff.is_completed());
        assert!(!handoff.completion.lock().unwrap().is_cancelled());
        if let Some(previous) = &previous {
            assert!(
                handoff
                    .begin_capture(previous, "# Same plan")
                    .unwrap_err()
                    .contains("stale")
            );
        }
        let abandoned = format!("preparation-{generation}");
        handoff
            .inspect_update(&artifact_announcement(&abandoned, "Write"))
            .unwrap();
        let id = format!("exit-{generation}");
        handoff.inspect_update(&acp_update(serde_json::json!({
            "sessionUpdate":"tool_call", "toolCallId":id, "title":"Propose", "kind":"switch_mode", "status":"pending",
            "rawInput":{"plan":"# Same plan"}, "_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}
        }))).unwrap();
        handoff.begin_capture(&id, "# Same plan").unwrap();
        handoff.finish_capture(&id).unwrap();
        assert!(handoff.is_completed());
        assert!(handoff.completion.lock().unwrap().is_cancelled());
        previous = Some(id);
    }
    handoff.begin_generation().unwrap();
    assert!(
        matches!(handoff.permission(&artifact_permission("preparation-1", "Write", &plans.join("late.md"))), ClaudeHandoffPermission::Invalid(error) if error.contains("stale"))
    );
    assert!(handoff.inspect_update(&acp_update(serde_json::json!({
        "sessionUpdate":"tool_call_update", "toolCallId":"preparation-2", "rawInput":{"file_path":plans.join("late.md")},
        "_meta":{"claudeCode":{"toolName":"Write"}}
    }))).unwrap_err().contains("stale"));
    assert!(handoff.unresolved_violation().is_none());
}

#[test]
fn abandoned_preparations_allow_capture_but_cannot_acquire_targets_afterward() {
    for completed in [false, true] {
        for permission_target in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let (_, plans, handoff) = claude_handoff_fixture(&directory);
            handoff
                .inspect_update(&artifact_announcement("abandoned", "Write"))
                .unwrap();
            assert!(handoff.unresolved_violation().is_none());
            handoff.inspect_update(&acp_update(serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "exit", "title": "Finish", "kind": "switch_mode", "status": "pending",
                "rawInput": {"plan": "# Plan"}, "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
            }))).unwrap();
            handoff.begin_capture("exit", "# Plan").unwrap();
            if completed {
                handoff.finish_capture("exit").unwrap();
            }
            let path = plans.join("late.md");
            if permission_target {
                assert!(
                    matches!(handoff.permission(&artifact_permission("abandoned", "Write", &path)), ClaudeHandoffPermission::Invalid(error) if error.contains("after handoff began"))
                );
            } else {
                assert!(handoff.inspect_update(&acp_update(serde_json::json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "abandoned", "rawInput": {"file_path": path},
                    "_meta": {"claudeCode": {"toolName": "Write"}}
                }))).unwrap_err().contains("after handoff began"));
            }
            assert!(
                handoff
                    .state
                    .lock()
                    .unwrap()
                    .unresolved_artifacts()
                    .is_empty()
            );
            assert!(
                handoff
                    .abandoned_preparations()
                    .unwrap()
                    .contains("abandoned")
            );
        }
    }
}

#[test]
fn abandoned_preparation_becomes_blocking_on_first_target_and_terminal_replay_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let (_, plans, handoff) = claude_handoff_fixture(&directory);
    handoff
        .inspect_update(&artifact_announcement("write", "Write"))
        .unwrap();
    assert!(
        handoff
            .inspect_update(&artifact_status("write", "Write", "completed"))
            .unwrap_err()
            .contains("without a validated path")
    );
    assert!(matches!(
        handoff.permission(&artifact_permission(
            "write",
            "Write",
            &plans.join("plan.md")
        )),
        ClaudeHandoffPermission::ArtifactMutation
    ));
    assert!(handoff.unresolved_violation().unwrap().contains("write"));
    assert!(handoff.abandoned_preparations().is_none());
    handoff
        .inspect_update(&artifact_status("write", "Write", "completed"))
        .unwrap();
    handoff.inspect_update(&acp_update(serde_json::json!({
        "sessionUpdate": "tool_call", "toolCallId": "exit", "title": "Finish", "kind": "switch_mode", "status": "pending",
        "rawInput": {"plan": "# Plan"}, "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }))).unwrap();
    handoff.begin_capture("exit", "# Plan").unwrap();
    handoff.finish_capture("exit").unwrap();
    handoff
        .inspect_update(&artifact_status("write", "Write", "completed"))
        .unwrap();
    assert!(handoff.unresolved_violation().is_none());
}
