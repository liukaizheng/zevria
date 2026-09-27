use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::schema::v1::{
    PermissionOption, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};
use tokio_util::sync::CancellationToken;
use zevria_foundation::SessionMode;
use zevria_foundation::TurnId;
use zevria_session_api::question_channels;
use zevria_session_api::session_event_channel;
use zevria_transcript::AgentRunTranscriptRecord;
use zevria_transcript::load_agent_run;
use zevria_workflow::AgentProtocolDirection;

use super::*;

const PYTHON_COMMAND: &str = if cfg!(windows) { "python" } else { "python3" };
use crate::config::EnsembleAgentConfig;

include!("ensemble_prompt_recovery_tests.rs");
include!("native_handoff_tests.rs");
include!("native_handoff_durability_tests.rs");
include!("ensemble_elicitation_tests.rs");
include!("ensemble_interactive_tests.rs");
include!("ensemble_image_tests.rs");
include!("ensemble_abandon_tests.rs");
include!("ensemble_history_preflight_tests.rs");

#[cfg(windows)]
#[path = "windows_artifact_path_tests.rs"]
mod windows_artifact_path_tests;

#[path = "diagnostics_tests.rs"]
mod diagnostics_tests;

#[path = "fixture_regression_tests.rs"]
mod fixture_regression_tests;

fn python_fixture_script(body: &str) -> String {
    format!("{}\n{body}", include_str!("fixtures/support.py"))
}

fn fixture_diagnostics(outcome: &AgentRunOutcome, records: &[AgentRunTranscriptRecord]) -> String {
    let stderr = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Stderr { text },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{outcome:?}\nfixture stderr:\n{stderr}")
}

fn assert_session_workspace(cwd: &serde_json::Value, workspace: &Path) {
    let cwd = Path::new(cwd.as_str().expect("session cwd is a path string"));
    assert!(cwd.is_absolute(), "session cwd must be absolute: {cwd:?}");
    #[cfg(windows)]
    {
        use zevria_foundation::windows_io::{checked_directory_path, identity, open_directory};
        assert_eq!(
            identity(&open_directory(cwd).expect("open session cwd")).unwrap(),
            identity(&open_directory(workspace).expect("open fixture workspace")).unwrap(),
            "session cwd must identify the fixture workspace: {cwd:?} vs {workspace:?}"
        );
        assert_eq!(
            cwd,
            checked_directory_path(workspace).expect("checked lexical workspace"),
            "the supervisor must preserve checked filesystem aliases"
        );
    }
    #[cfg(not(windows))]
    assert_eq!(
        cwd,
        std::fs::canonicalize(workspace).expect("canonical workspace")
    );
}

#[derive(Default)]
struct WriterProbe {
    appended: AtomicUsize,
    durable: AtomicUsize,
    syncs: AtomicUsize,
}

struct ProbeRunLogWriter {
    probe: Arc<WriterProbe>,
    fail_sync: bool,
    sync_delay: Duration,
}

impl RunLogWriter for ProbeRunLogWriter {
    fn append_buffered(&mut self, _record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        self.probe.appended.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }

    fn sync(&mut self) -> anyhow::Result<()> {
        self.probe.syncs.fetch_add(1, Ordering::SeqCst);
        if !self.sync_delay.is_zero() {
            std::thread::sleep(self.sync_delay);
        }
        if self.fail_sync {
            anyhow::bail!("scripted sync failure");
        }
        self.probe
            .durable
            .store(self.probe.appended.load(Ordering::SeqCst), Ordering::SeqCst);
        Ok(())
    }
}

fn probe_writer(
    probe: Arc<WriterProbe>,
    fail_sync: bool,
    sync_delay: Duration,
) -> ProbeRunLogWriter {
    ProbeRunLogWriter {
        probe,
        fail_sync,
        sync_delay,
    }
}

fn diagnostic_record(text: &str) -> AgentRunTranscriptRecord {
    AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::Stderr {
            text: text.to_string(),
        },
    }
}

fn test_publication(events: SessionEventSender) -> RunLogPublication {
    RunLogPublication {
        events,
        turn_id: TurnId::new(41),
        ensemble_run_id: EnsembleRunId::from_string("ensemble"),
        agent_run_id: AgentRunId::from_string("agent"),
    }
}

fn permission(kind: PermissionOptionKind) -> PermissionOption {
    PermissionOption::new(wire_name(&kind), wire_name(&kind), kind)
}

fn mode_option(current: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        "execution-mode",
        "Execution mode",
        current.to_string(),
        vec![
            SessionConfigSelectOption::new("read-only", "Read only"),
            SessionConfigSelectOption::new("build", "Build"),
        ],
    )
    .category(SessionConfigOptionCategory::Mode)
}

fn collaboration_option(current: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        "collaboration_mode",
        "Collaboration mode",
        current.to_string(),
        vec![
            SessionConfigSelectOption::new("default", "Default"),
            SessionConfigSelectOption::new("plan", "Plan"),
        ],
    )
}

fn claude_handoff_fixture(directory: &tempfile::TempDir) -> (PathBuf, PathBuf, ClaudePlanHandoff) {
    let workspace = directory.path().join("workspace");
    let config_directory = directory.path().join("claude-config");
    let plans = config_directory.join("plans");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(&plans).expect("Claude plans directory");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    #[cfg(windows)]
    let plans = std::fs::canonicalize(plans).expect("canonical Claude plans directory");
    let mut agent = EnsembleConfig::default().agents["claude"].clone();
    agent.env.insert(
        CLAUDE_CONFIG_DIR_ENV.to_string(),
        config_directory.display().to_string(),
    );
    let handoff = ClaudePlanHandoff::new(&agent, &workspace).expect("configured Claude handoff");
    (workspace, plans, handoff)
}

fn acp_update(value: serde_json::Value) -> AcpSessionUpdate {
    serde_json::from_value(value).expect("valid ACP session update")
}

fn permission_request(value: serde_json::Value) -> RequestPermissionRequest {
    serde_json::from_value(value).expect("valid ACP permission request")
}

pub(super) fn named_single_agent_config(name: &str, agent: EnsembleAgentConfig) -> EnsembleConfig {
    EnsembleConfig {
        plan_agents: vec![name.to_string()],
        review_agents: vec![name.to_string()],
        max_concurrent_agents: 1,
        review_startup_timeout_seconds: 5,
        review_turn_timeout_seconds: 5,
        cancel_grace_seconds: 1,
        max_synthesis_bytes_per_agent: 4_096,
        agents: BTreeMap::from([(name.to_string(), agent)]),
    }
}

fn single_agent_config(agent: EnsembleAgentConfig) -> EnsembleConfig {
    named_single_agent_config("fake", agent)
}

pub(super) fn test_questions() -> QuestionRequester {
    let (events, _receiver) = session_event_channel(8);
    question_channels(events).requester
}

pub(super) fn fake_stdio_agent(
    script: &Path,
    env: BTreeMap<String, String>,
) -> EnsembleAgentConfig {
    EnsembleAgentConfig {
        label: "Fake stdio ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env,
        login_hint: "Authenticate the fake agent.".to_string(),
    }
}

impl EnsembleSupervisor {
    /// Observe one interaction per worker, never approve it. Protocol tests
    /// use live Plan actors; only dedicated coordinator tests send consent.
    pub(crate) async fn observe_review_rounds(
        &self,
        request: EnsembleLaunchRequest,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<Vec<AgentRunOutcome>> {
        if request.start.workflow == EnsembleWorkflow::Review {
            return self.launch(request, events, turn).await;
        }
        anyhow::ensure!(
            !request.resume,
            "recovery fixtures must provide authoritative root review history, not one-shot outcomes"
        );
        let mut states = request
            .start
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
                        text: request.start.prompt.clone(),
                    },
                })
                .map_err(anyhow::Error::msg)?;
        }
        let start = request.start.clone();
        let mut execution = self.start_review(request, states.clone(), events, turn.clone())?;
        tokio::time::timeout(Duration::from_secs(15), async {
                while !states.iter().all(WorkerReviewState::quiescent) {
                    tokio::select! {
                        biased;
                        () = turn.cancellation().cancelled() => break,
                        update = execution.updates.recv() => {
                            let update = update.context("actor stopped before round observation")?;
                            states.iter_mut().find(|state| state.descriptor.id == update.worker_id).context("foreign worker")?.apply(&update.event).map_err(anyhow::Error::msg)?;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("protocol fixture did not settle its interaction")??;
        let mut outcomes: Vec<_> = states
            .iter()
            .map(|state| {
                assert!(
                    state.confirmation.is_none(),
                    "protocol evidence is never consent"
                );
                let mut outcome = state.evidence.clone();
                outcome.status = if turn.is_cancelled() {
                    AgentRunStatus::Cancelled
                } else {
                    state.status()
                };
                outcome.partial = outcome.failure.is_some() || turn.is_cancelled();
                outcome
            })
            .collect();
        execution.cancellation.cancel();
        for sender in execution.commands.values() {
            sender.closed().await;
        }
        if turn.is_cancelled() {
            for outcome in &mut outcomes {
                let projection = load_agent_run_projection(&agent_run_path(
                    &self.agent_runs_root,
                    &start.run_id,
                    &outcome.descriptor.id,
                ))?;
                *outcome = projection
                    .recoverable_outcome()
                    .context("cancelled worker must durably settle")?;
            }
        }
        Ok(outcomes)
    }
}

pub(super) async fn launch_fake_worker(
    supervisor: &EnsembleSupervisor,
    logs: &Path,
    workflow: EnsembleWorkflow,
    prompt: &str,
) -> (
    zevria_workflow::EnsembleStart,
    AgentRunOutcome,
    Vec<AgentRunTranscriptRecord>,
) {
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow,
        prompt: prompt.into(),
        agents: supervisor.workers(workflow).expect("worker descriptor"),
    };
    let (events, _receiver) = session_event_channel(1_024);
    let mut outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(
                TurnId::new(91),
                SessionMode::Build,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("fake worker launch succeeds");
    assert_eq!(outcomes.len(), 1);
    let outcome = outcomes.remove(0);
    let path = agent_run_path(logs, &start.run_id, &outcome.descriptor.id);
    let records = load_agent_run(&path).expect("fake worker transcript");
    (start, outcome, records)
}

pub(super) fn protocol_requests(records: &[AgentRunTranscriptRecord]) -> Vec<serde_json::Value> {
    records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event:
                    AgentRunEvent::Protocol {
                        direction: AgentProtocolDirection::ClientToAgent,
                        json,
                    },
            } => serde_json::from_str(json).ok(),
            _ => None,
        })
        .filter(|message: &serde_json::Value| message.get("method").is_some())
        .collect()
}

async fn receive_question(
    receiver: &mut zevria_session_api::SessionEventReceiver,
) -> QuestionRequest {
    loop {
        if let Some(zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::QuestionAsked {
            request,
            ..
        })) = receiver.recv().await
        {
            return request;
        }
    }
}

#[tokio::test]
async fn run_log_publishes_only_after_the_record_is_synced() {
    let probe = Arc::new(WriterProbe::default());
    let (events, mut receiver) = session_event_channel(8);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe.clone(), false, Duration::ZERO),
        test_publication(events),
        failure,
    );
    writer
        .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
            record: diagnostic_record("durable diagnostic"),
            publication: Some(AgentRunEvent::Stderr {
                text: "durable diagnostic".to_string(),
            }),
            force_sync: false,
            acknowledgement: None,
        })))
        .await
        .expect("writer accepts diagnostic");
    let (acknowledgement, barrier) = oneshot::channel();
    writer
        .send(RunLogWriteCommand::Barrier { acknowledgement })
        .await
        .expect("writer accepts barrier");
    barrier
        .await
        .expect("writer answers barrier")
        .expect("barrier succeeds");

    assert_eq!(probe.durable.load(Ordering::SeqCst), 1);
    assert!(matches!(
        receiver.recv().await,
        Some(zevria_session_api::SessionUpdate::Lifecycle(
            SessionEvent::AgentRunUpdated {
                event: AgentRunEvent::Stderr { text },
                ..
            }
        )) if text == "durable diagnostic"
    ));
}

#[tokio::test]
async fn run_log_batches_records_and_a_barrier_into_one_sync() {
    let probe = Arc::new(WriterProbe::default());
    let (events, _receiver) = session_event_channel(8);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe.clone(), false, Duration::ZERO),
        test_publication(events),
        failure,
    );
    for index in 0..16 {
        writer
            .try_send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
                record: diagnostic_record(&format!("record {index}")),
                publication: None,
                force_sync: false,
                acknowledgement: None,
            })))
            .expect("bounded writer queue has capacity");
    }
    let (acknowledgement, barrier) = oneshot::channel();
    writer
        .try_send(RunLogWriteCommand::Barrier { acknowledgement })
        .expect("writer queue accepts barrier");
    barrier
        .await
        .expect("writer answers barrier")
        .expect("barrier succeeds");

    assert_eq!(probe.appended.load(Ordering::SeqCst), 16);
    assert_eq!(probe.durable.load(Ordering::SeqCst), 16);
    assert_eq!(probe.syncs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_log_sync_failure_isolated_and_never_published() {
    let probe = Arc::new(WriterProbe::default());
    let (events, mut receiver) = session_event_channel(8);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe.clone(), true, Duration::ZERO),
        test_publication(events),
        failure.clone(),
    );
    let (acknowledgement, result) = oneshot::channel();
    writer
        .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
            record: diagnostic_record("not durable"),
            publication: Some(AgentRunEvent::Stderr {
                text: "not durable".to_string(),
            }),
            force_sync: true,
            acknowledgement: Some(acknowledgement),
        })))
        .await
        .expect("writer accepts record");
    let error = result
        .await
        .expect("writer answers record")
        .expect_err("sync failure reaches caller");

    assert!(error.contains("scripted sync failure"));
    assert_eq!(probe.durable.load(Ordering::SeqCst), 0);
    assert!(
        failure
            .lock()
            .expect("failure lock")
            .as_deref()
            .is_some_and(|error| error.contains("scripted sync failure"))
    );
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty
            | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test]
async fn accepted_decision_sync_failure_prevents_durable_publication() {
    let probe = Arc::new(WriterProbe::default());
    let (events, mut receiver) = session_event_channel(8);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe.clone(), true, Duration::ZERO),
        test_publication(events),
        failure.clone(),
    );
    let request_id = QuestionRequestId::new("durability-question");
    let event = AgentRunEvent::Elicitation {
        field_count: 1,
        outcome: AgentElicitationOutcome::Accepted,
        decision: Some(AgentUserDecisionBatch {
            request_id: request_id.clone(),
            answers: vec![AgentUserDecisionAnswer {
                decision_id: AgentUserDecisionId::from_question(&request_id, "scope"),
                question_id: "scope".to_string(),
                header: "Scope".to_string(),
                question: "Which scope?".to_string(),
                answer: AgentUserDecisionValue::String {
                    value: "Focused".to_string(),
                },
            }],
        }),
        decision_unavailable: None,
    };
    assert!(run_log_event_requires_sync(&event));
    let log = RunLog {
        path: PathBuf::from("unused-test-log"),
        writer,
        failure,
        evidence: Arc::new(Mutex::new(WorkerEvidenceState::default())),
        review_publication: Arc::new(Mutex::new(None)),
        event_order: Arc::new(tokio::sync::Mutex::new(())),
    };
    let error = log
        .emit(event)
        .await
        .expect_err("accepted decision cannot continue past failed durability");
    assert!(error.to_string().contains("scripted sync failure"));
    assert_eq!(probe.durable.load(Ordering::SeqCst), 0);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty
            | tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_run_log_sync_does_not_stall_the_async_runtime() {
    let probe = Arc::new(WriterProbe::default());
    let (events, _receiver) = session_event_channel(8);
    let failure = Arc::new(Mutex::new(None));
    let writer = spawn_run_log_writer(
        probe_writer(probe, false, Duration::from_millis(75)),
        test_publication(events),
        failure,
    );
    let (acknowledgement, result) = oneshot::channel();
    writer
        .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
            record: diagnostic_record("slow sync"),
            publication: None,
            force_sync: true,
            acknowledgement: Some(acknowledgement),
        })))
        .await
        .expect("writer accepts record");

    tokio::time::timeout(
        Duration::from_millis(30),
        tokio::time::sleep(Duration::from_millis(5)),
    )
    .await
    .expect("runtime timer remains responsive while sync blocks");
    result
        .await
        .expect("writer answers record")
        .expect("sync succeeds");
}

#[test]
fn agent_run_ignore_guard_is_created_once_and_never_overwritten() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    ensure_agent_run_ignore_guard(&workspace).expect("create ignore guard");
    let guard = zevria_foundation::runtime_paths::workspace_state_root(&workspace)
        .join("agent-runs")
        .join(".gitignore");
    assert_eq!(std::fs::read_to_string(&guard).expect("read guard"), "*\n");

    std::fs::write(&guard, "custom policy\n").expect("replace fixture guard");
    ensure_agent_run_ignore_guard(&workspace).expect("preserve existing guard");
    assert_eq!(
        std::fs::read_to_string(guard).expect("read preserved guard"),
        "custom policy\n"
    );
}

#[tokio::test]
async fn every_batch_rejects_unsupported_existing_logs_before_starting_any_worker() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let logs = workspace.join(".zevria/agent-runs/root-session");
    let agent = EnsembleAgentConfig {
        label: "Historical ACP".to_string(),
        command: directory
            .path()
            .join("must-not-be-spawned")
            .display()
            .to_string(),
        args: Vec::new(),
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let descriptor = supervisor
        .workers(EnsembleWorkflow::Review)
        .expect("worker descriptor")
        .pop()
        .expect("one worker");
    let unstarted = AgentRunDescriptor {
        id: AgentRunId::new(),
        ..descriptor.clone()
    };
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review the durable state".into(),
        agents: vec![unstarted.clone(), descriptor.clone()],
    };
    let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
    let writer = AgentRunTranscriptWriter::create(
        path.clone(),
        AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            descriptor: descriptor.clone(),
            prompt: start.prompt.clone(),
        },
    )
    .expect("create worker header");
    for event in [
        AgentRunEvent::Prompt {
            text: worker_prompt(start.workflow, &start.prompt, false).display_projection(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::AgentMessage {
            text: "durable historical report".to_string(),
            message_id: None,
        },
    ] {
        let record = AgentRunTranscriptRecord::Event { event };
        let mut raw = serde_json::to_vec(&record).unwrap();
        raw.push(b'\n');
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            &raw,
        )
        .unwrap();
    }
    drop(writer);
    let prefix = std::fs::read_to_string(&path).unwrap();
    let display = serde_json::to_string(&AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::ResponseDisplay {
            display: Box::new(super::response_display_tests::display_with_terminal_evidence()),
        },
    })
    .unwrap()
    .replace(
        r#""terminal":{"1":"completed"}"#,
        r#""terminal":{"01":"completed"}"#,
    );
    for invalid in [
        r#"{"record":"event","event":{"type":"status","status":"completed","detail":null}}"#
            .to_owned(),
        display,
    ] {
        let original = format!("{prefix}{invalid}\n").into_bytes();
        std::fs::write(&path, &original).unwrap();
        for resume in [false, true] {
            let (events, _receiver) = session_event_channel(16);
            let error = supervisor
                .observe_review_rounds(
                    EnsembleLaunchRequest {
                        start: start.clone(),
                        resume,
                    },
                    events,
                    TurnContext::new(
                        TurnId::new(42),
                        SessionMode::Build,
                        CancellationToken::new(),
                    ),
                )
                .await
                .expect_err("invalid complete history must fail batch preflight");
            assert!(
                error
                    .downcast_ref::<zevria_transcript::transcript::UnsupportedHistory>()
                    .is_some()
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert!(
                !agent_run_path(&logs, &start.run_id, &unstarted.id).exists(),
                "no earlier worker can start before the whole batch passes preflight"
            );
        }
    }
}

#[test]
fn client_advertises_workflow_scoped_plan_operations_without_host_authority() {
    let capabilities = client_capabilities(EnsembleWorkflow::Plan);
    assert!(!capabilities.fs.read_text_file);
    assert!(!capabilities.fs.write_text_file);
    assert!(!capabilities.terminal);
    assert!(
        capabilities
            .session
            .as_ref()
            .and_then(|session| session.config_options.as_ref())
            .is_some()
    );
    let serialized = serde_json::to_value(&capabilities).expect("serialize capabilities");
    assert_eq!(serialized["elicitation"], serde_json::json!({"form": {}}));
    assert!(serialized["elicitation"].get("url").is_none());
    assert_eq!(serialized["terminal"], false);
    assert_eq!(serialized["fs"]["readTextFile"], false);
    assert_eq!(serialized["fs"]["writeTextFile"], false);
    assert_eq!(serialized["plan"], serde_json::json!({}));

    let review = serde_json::to_value(client_capabilities(EnsembleWorkflow::Review))
        .expect("serialize review capabilities");
    assert!(review.get("plan").is_none());
}

#[test]
fn session_bootstrap_gate_accepts_matching_provisional_notifications() {
    let mut gate = SessionBootstrapGate::default();
    gate.observe_notification("session-a")
        .expect("first startup notification is provisional");
    gate.observe_notification("session-a")
        .expect("same provisional session remains valid");
    gate.bind("session-a")
        .expect("session/new confirms provisional ID");
    gate.observe_notification("session-a")
        .expect("bound session accepts notifications");
    assert_eq!(gate.bound_id(), Some("session-a"));
}

#[test]
fn session_bootstrap_gate_rejects_conflicting_ids() {
    let mut conflicting_notification = SessionBootstrapGate::default();
    conflicting_notification
        .observe_notification("session-a")
        .expect("provisional session");
    assert!(
        conflicting_notification
            .observe_notification("session-b")
            .expect_err("second provisional ID must fail")
            .contains("conflicting provisional")
    );

    let mut conflicting_response = SessionBootstrapGate::default();
    conflicting_response
        .observe_notification("session-a")
        .expect("provisional session");
    assert!(
        conflicting_response
            .bind("session-b")
            .expect_err("session/new must confirm the provisional ID")
            .contains("conflicts with provisional")
    );

    let mut recovery = SessionBootstrapGate::default();
    recovery.bind("recovery-session").expect("bind recovery");
    assert!(recovery.observe_notification("other-session").is_err());
}

fn artifact_announcement(id: &str, operation: &str) -> AcpSessionUpdate {
    acp_update(serde_json::json!({
        "sessionUpdate": "tool_call", "toolCallId": id,
        "title": "Preparing file…", "kind": "edit", "status": "pending",
        "rawInput": {}, "locations": [], "content": [],
        "_meta": {"claudeCode": {"toolName": operation}}
    }))
}

fn artifact_status(id: &str, operation: &str, status: &str) -> AcpSessionUpdate {
    acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
        "_meta": {"claudeCode": {"toolName": operation}}
    }))
}

fn artifact_permission(id: &str, operation: &str, path: &Path) -> RequestPermissionRequest {
    permission_request(serde_json::json!({
        "sessionId": "session", "toolCall": {
            "toolCallId": id, "kind": "edit", "rawInput": {"file_path": path},
            "locations": [{"path": path}],
            "_meta": {"claudeCode": {"toolName": operation}}
        },
        "options": [{"optionId": format!("{id}-once"), "name": "Once", "kind": "allow_once"}]
    }))
}

#[test]
fn recorded_claude_pathless_write_preparations_do_not_authorize_or_cancel() {
    let directory = tempfile::tempdir().unwrap();
    let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
    // Minimal synthetic reproduction, not a dependency on a private run log.
    let ids = [
        "toolu_01SZJT9Wz9phyW4wQPxL1Bff",
        "toolu_01UYWv9YuKfkjnwCu7Xqkbxe",
    ];
    for id in ids {
        let update = artifact_announcement(id, "Write");
        handoff.inspect_update(&update).unwrap();
        assert!(
            normalize_update(update)
                .iter()
                .all(|event| matches!(event, AgentRunEvent::ToolCall { .. }))
        );
    }
    let state = handoff.state.lock().unwrap();
    assert_eq!(state.tools.len(), 2);
    assert!(state.permissions.is_empty());
    assert!(state.granted_tool.is_none());
    assert_eq!(state.phase, ClaudePlanHandoffPhase::Active);
    assert!(state.unresolved_artifacts().is_empty());
    let abandoned = state.abandoned_preparations();
    assert!(abandoned.find(ids[0]).unwrap() < abandoned.find(ids[1]).unwrap());
    assert_eq!(abandoned.matches("abandoned preparation").count(), 2);
    assert!(!handoff.completion.lock().unwrap().is_cancelled());
    assert!(!workspace.join(".claude").exists());
    assert_eq!(std::fs::read_dir(plans).unwrap().count(), 0);
}

#[test]
fn claude_refinements_and_permission_supplied_first_targets_can_interleave() {
    for reverse in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
        for id in ["first", "second", "permission-only"] {
            handoff
                .inspect_update(&artifact_announcement(id, "Write"))
                .unwrap();
        }
        let order = if reverse {
            ["second", "first"]
        } else {
            ["first", "second"]
        };
        for id in order {
            let path = plans.join(format!("{id}.md"));
            handoff
                .inspect_update(&acp_update(serde_json::json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id,
                    "rawInput": {"file_path": path}, "locations": [{"path": path}],
                    "content": [{"type": "diff", "path": path, "newText": "draft"}],
                    "_meta": {"claudeCode": {"toolName": "Write"}}
                })))
                .unwrap();
        }
        let permission = artifact_permission("permission-only", "Write", &plans.join("third.md"));
        assert!(matches!(
            handoff.permission(&permission),
            ClaudeHandoffPermission::ArtifactMutation
        ));
        let state = handoff.state.lock().unwrap();
        assert_eq!(state.nonterminal_artifacts().len(), 3);
        assert!(
            state
                .nonterminal_artifacts()
                .iter()
                .all(|(_, _, _, path)| path.is_some())
        );
        assert!(state.granted_tool.is_none());
    }
}

#[test]
fn claude_fifo_grants_follow_requests_and_wait_for_terminal_and_durability() {
    let directory = tempfile::tempdir().unwrap();
    let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let workspace_plans = workspace.join(".claude/plans");
    let attempt = handoff.start_attempt(CancellationToken::new());
    let calls = [
        ("write", "Write", plans.join("same.md")),
        ("edit", "Edit", plans.join("same.md")),
        ("multi", "MultiEdit", workspace_plans.join("different.md")),
    ];
    // Announcement order deliberately differs from request order.
    for index in [2, 0, 1] {
        let (id, op, _) = &calls[index];
        handoff
            .inspect_update(&artifact_announcement(id, op))
            .unwrap();
    }
    let requests = calls
        .iter()
        .map(|(id, op, path)| artifact_permission(id, op, path))
        .collect::<Vec<_>>();
    let tickets = requests
        .iter()
        .map(|request| handoff.register_permission(request, attempt.id).unwrap())
        .collect::<Vec<_>>();
    assert!(!workspace_plans.exists());
    assert_eq!(
        tickets[2].try_admit(&requests[2], || false).unwrap(),
        ClaudePlanAdmission::Waiting
    );
    handoff
        .inspect_update(&artifact_status("multi", "MultiEdit", "in_progress"))
        .unwrap();
    assert!(
        handoff.state.lock().unwrap().granted_tool.is_none(),
        "progress is not authorization"
    );
    for index in 0..3 {
        assert_eq!(
            tickets[index]
                .try_admit(&requests[index], || false)
                .unwrap(),
            ClaudePlanAdmission::Granted
        );
        if index == 2 {
            assert!(workspace_plans.is_dir());
        }
        // A replay of an earlier owner's terminal must not release this owner.
        if index > 0 {
            handoff
                .inspect_update(&artifact_status("write", "Write", "completed"))
                .unwrap();
            assert_eq!(
                handoff.state.lock().unwrap().granted_tool.as_deref(),
                Some(calls[index].0)
            );
        }
        let status = if index == 1 { "failed" } else { "completed" };
        if index < 2 {
            assert_eq!(
                tickets[index + 1]
                    .try_admit(&requests[index + 1], || false)
                    .unwrap(),
                ClaudePlanAdmission::Waiting
            );
        }
        handoff
            .inspect_update(&artifact_status(calls[index].0, calls[index].1, status))
            .unwrap();
        if index < 2 {
            assert_eq!(
                tickets[index + 1]
                    .try_admit(&requests[index + 1], || false)
                    .unwrap(),
                ClaudePlanAdmission::Waiting,
                "terminal does not bypass pending permission sync"
            );
            assert!(!workspace_plans.exists());
        }
        tickets[index].delivered();
        handoff
            .inspect_update(&artifact_status(calls[index].0, calls[index].1, status))
            .unwrap();
    }
    assert!(handoff.unresolved_violation().is_none());
    assert!(
        handoff
            .inspect_update(&artifact_status("edit", "Edit", "completed"))
            .unwrap_err()
            .contains("changed terminal status")
    );
    assert!(
        handoff
            .inspect_update(&artifact_status("multi", "MultiEdit", "pending"))
            .unwrap_err()
            .contains("regressed")
    );
}

#[test]
fn claude_tickets_are_transport_scoped_but_granted_ownership_survives_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let lifetime = CancellationToken::new();
    let attempt = handoff.start_attempt(lifetime.clone());
    for id in ["first", "second"] {
        handoff
            .inspect_update(&artifact_announcement(id, "Write"))
            .unwrap();
    }
    let first = artifact_permission("first", "Write", &plans.join("first.md"));
    let second = artifact_permission("second", "Write", &plans.join("second.md"));
    let owner = handoff.register_permission(&first, attempt.id).unwrap();
    let stale = handoff.register_permission(&second, attempt.id).unwrap();
    assert_eq!(
        owner.try_admit(&first, || false).unwrap(),
        ClaudePlanAdmission::Granted
    );
    owner.delivered();
    drop(owner); // Responding is not completing the mutation.
    drop(attempt);
    assert!(lifetime.is_cancelled());
    assert!(handoff.state.lock().unwrap().permissions.is_empty());
    assert_eq!(
        handoff.state.lock().unwrap().granted_tool.as_deref(),
        Some("first")
    );
    let recovered = handoff.start_attempt(CancellationToken::new());
    assert!(
        handoff.register_permission(&first, recovered.id).is_err(),
        "never blindly regrant an unresolved owner"
    );
    let replacement = handoff.register_permission(&second, recovered.id).unwrap();
    assert_eq!(
        stale.try_admit(&second, || false).unwrap(),
        ClaudePlanAdmission::Cancelled
    );
    drop(stale);
    assert_eq!(
        replacement.try_admit(&second, || false).unwrap(),
        ClaudePlanAdmission::Waiting
    );
    handoff
        .inspect_update(&artifact_status("first", "Write", "completed"))
        .unwrap();
    assert_eq!(
        replacement.try_admit(&second, || false).unwrap(),
        ClaudePlanAdmission::Granted
    );
    replacement.delivered();
    handoff
        .inspect_update(&artifact_status("first", "Write", "completed"))
        .unwrap();
    assert_eq!(
        handoff.state.lock().unwrap().granted_tool.as_deref(),
        Some("second")
    );
}

#[test]
fn claude_cancelled_terminal_and_abandoned_tickets_never_authorize_work() {
    let directory = tempfile::tempdir().unwrap();
    let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let attempt = handoff.start_attempt(CancellationToken::new());
    handoff
        .inspect_update(&artifact_announcement("first", "Write"))
        .unwrap();
    let request = artifact_permission("first", "Write", &workspace.join(".claude/plans/first.md"));
    let ticket = handoff.register_permission(&request, attempt.id).unwrap();
    assert!(handoff.register_permission(&request, attempt.id).is_err());
    let mut mismatched = request.clone();
    mismatched.session_id = SessionId::new("different-session");
    assert!(
        ticket
            .try_admit(&mismatched, || false)
            .unwrap_err()
            .contains("changed session or tool identity")
    );
    assert_eq!(
        ticket.try_admit(&request, || true).unwrap(),
        ClaudePlanAdmission::Cancelled
    );
    assert!(!workspace.join(".claude").exists());
    // This is also the cleanup path for a rejected spawn/unpolled task.
    let abandoned = async move {
        let _ticket = ticket;
        std::future::pending::<()>().await;
    };
    drop(abandoned);
    assert!(handoff.state.lock().unwrap().permissions.is_empty());
    let ticket = handoff.register_permission(&request, attempt.id).unwrap();
    handoff
        .inspect_update(&artifact_status("first", "Write", "failed"))
        .unwrap();
    assert_eq!(
        ticket.try_admit(&request, || false).unwrap(),
        ClaudePlanAdmission::Cancelled
    );
    assert!(!workspace.join(".claude").exists());
    assert!(handoff.state.lock().unwrap().granted_tool.is_none());
    let unknown = artifact_permission("unknown", "Write", &plans.join("unknown.md"));
    assert!(handoff.register_permission(&unknown, attempt.id).is_err());
    let ordinary = permission_request(serde_json::json!({
        "sessionId": "session", "toolCall": {"toolCallId": "ordinary", "kind": "edit"},
        "options": [{"optionId": "once", "name": "Once", "kind": "allow_once"}]
    }));
    assert!(handoff.register_permission(&ordinary, attempt.id).is_err());
    handoff
        .inspect_update(&artifact_announcement("invalid", "Write"))
        .unwrap();
    let mut invalid = artifact_permission("invalid", "Write", &plans.join("invalid.md"));
    invalid.options.clear();
    assert!(handoff.register_permission(&invalid, attempt.id).is_err());
    invalid = artifact_permission("invalid", "Write", &plans.join("invalid.md"));
    invalid.tool_call.fields.raw_input = None;
    invalid.tool_call.fields.locations = None;
    assert!(handoff.register_permission(&invalid, attempt.id).is_err());
    assert!(handoff.state.lock().unwrap().permissions.is_empty());
    assert!(
        handoff
            .unresolved_violation()
            .unwrap()
            .contains("no host permission observed")
    );
}

#[test]
fn claude_invalid_evidence_fences_eligible_waiters_before_waking_them() {
    for invalid_notification in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
        let attempt = handoff.start_attempt(CancellationToken::new());
        handoff
            .inspect_update(&artifact_announcement("queued", "Write"))
            .unwrap();
        let request = artifact_permission(
            "queued",
            "Write",
            &workspace.join(".claude/plans/queued.md"),
        );
        let ticket = handoff.register_permission(&request, attempt.id).unwrap();
        if invalid_notification {
            assert!(
                handoff
                    .inspect_update(&artifact_status("queued", "Edit", "in_progress"))
                    .is_err()
            );
        } else {
            let invalid = artifact_permission("unknown", "Write", &plans.join("unknown.md"));
            assert!(matches!(
                handoff.permission(&invalid),
                ClaudeHandoffPermission::Invalid(_)
            ));
        }
        assert_eq!(
            ticket.try_admit(&request, || false).unwrap(),
            ClaudePlanAdmission::Cancelled
        );
        assert!(!workspace.join(".claude").exists());
    }
}

struct FailingPermissionResponder {
    send_failure: bool,
    sent: Arc<AtomicBool>,
}

impl PermissionResponder for FailingPermissionResponder {
    fn respond_permission(self, response: RequestPermissionResponse) -> Result<(), AcpError> {
        assert_eq!(
            serde_json::to_value(response).unwrap()["outcome"]["optionId"],
            "owner-once"
        );
        self.sent.store(true, AtomicOrdering::Release);
        if self.send_failure {
            Err(acp_error("scripted permission send failure"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn claude_permission_send_and_sync_failures_fence_admission_without_releasing_owner() {
    for send_failure in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
        let attempt = handoff.start_attempt(CancellationToken::new());
        for id in ["owner", "queued"] {
            handoff
                .inspect_update(&artifact_announcement(id, "Write"))
                .unwrap();
        }
        let owner = artifact_permission("owner", "Write", &plans.join("owner.md"));
        let queued = artifact_permission("queued", "Write", &plans.join("queued.md"));
        let ticket = handoff.register_permission(&owner, attempt.id).unwrap();
        let waiter = handoff.register_permission(&queued, attempt.id).unwrap();
        assert_eq!(
            ticket.try_admit(&owner, || false).unwrap(),
            ClaudePlanAdmission::Granted
        );
        let (events, _receiver) = session_event_channel(8);
        let failure = Arc::new(Mutex::new(None));
        let writer = spawn_run_log_writer(
            probe_writer(
                Arc::new(WriterProbe::default()),
                !send_failure,
                Duration::ZERO,
            ),
            test_publication(events),
            failure.clone(),
        );
        let log = RunLog {
            path: PathBuf::from("unused-test-log"),
            writer,
            failure,
            evidence: Arc::new(Mutex::new(WorkerEvidenceState::default())),
            review_publication: Arc::new(Mutex::new(None)),
            event_order: Arc::new(tokio::sync::Mutex::new(())),
        };
        let sent = Arc::new(AtomicBool::new(false));
        let error = {
            let delivery = deliver_artifact_permission(
                &owner,
                FailingPermissionResponder {
                    send_failure,
                    sent: sent.clone(),
                },
                &log,
                &ticket,
                owner.options[0].option_id.clone(),
            );
            assert!(
                sent.load(AtomicOrdering::Acquire),
                "response is sent before polling the persistence future"
            );
            match delivery {
                Ok(persistence) => persistence.await.unwrap_err(),
                Err(error) => error,
            }
        };
        assert!(format!("{error:?}").contains(if send_failure {
            "scripted permission send failure"
        } else {
            "scripted sync failure"
        }));
        drop(ticket); // task failure/abandonment is fenced even before SDK shutdown
        assert_eq!(
            handoff.state.lock().unwrap().granted_tool.as_deref(),
            Some("owner")
        );
        assert_eq!(
            waiter.try_admit(&queued, || false).unwrap(),
            ClaudePlanAdmission::Cancelled
        );
        handoff
            .inspect_update(&artifact_status("owner", "Write", "completed"))
            .unwrap();
        assert_eq!(
            waiter.try_admit(&queued, || false).unwrap(),
            ClaudePlanAdmission::Cancelled,
            "terminal cannot revive a failed delivery attempt"
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_queued_paths_and_directories_are_revalidated_before_preparation() {
    for change_directory in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
        let attempt = handoff.start_attempt(CancellationToken::new());
        for id in ["owner", "queued"] {
            handoff
                .inspect_update(&artifact_announcement(id, "Write"))
                .unwrap();
        }
        let owner = artifact_permission("owner", "Write", &plans.join("owner.md"));
        let path = workspace.join(".claude/plans/queued.md");
        let queued = artifact_permission("queued", "Write", &path);
        let owner_ticket = handoff.register_permission(&owner, attempt.id).unwrap();
        let ticket = handoff.register_permission(&queued, attempt.id).unwrap();
        assert_eq!(
            owner_ticket.try_admit(&owner, || false).unwrap(),
            ClaudePlanAdmission::Granted
        );
        owner_ticket.delivered();
        assert_eq!(
            ticket.try_admit(&queued, || false).unwrap(),
            ClaudePlanAdmission::Waiting
        );
        assert!(!workspace.join(".claude").exists());
        if change_directory {
            std::os::unix::fs::symlink(&plans, workspace.join(".claude")).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(plans.join("escape.md"), &path).unwrap();
        }
        handoff
            .inspect_update(&artifact_status("owner", "Write", "completed"))
            .unwrap();
        assert!(
            ticket
                .try_admit(&queued, || false)
                .unwrap_err()
                .contains("symlink")
        );
        assert!(!plans.join("escape.md").exists());
        assert!(handoff.state.lock().unwrap().granted_tool.is_none());
    }
}

#[test]
fn claude_plan_preparations_overlap_and_capture_requires_terminal_artifacts() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let first_path = plans.join("careful-plan.md");
    let second_path = plans.join("second.md");

    let pending = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "write-plan",
        "title": "Preparing file…",
        "kind": "edit",
        "status": "pending",
        "content": [],
        "locations": [],
        "rawInput": {},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&pending)
        .expect("pathless initial Write is tracked");

    let overlapping = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "second-write",
        "title": "Write second plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": second_path},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&overlapping)
        .expect("preparations may overlap");

    let refined = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "write-plan",
        "title": format!("Write {}", first_path.display()),
        "kind": "edit",
        "rawInput": {"file_path": first_path},
        "locations": [{"path": first_path}],
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&refined)
        .expect("refined direct Markdown child is valid");

    let permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "write-plan",
            "kind": "edit",
            "rawInput": {"file_path": first_path},
            "locations": [{"path": first_path}]
        },
        "options": [
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
            {"optionId": "once", "name": "Once", "kind": "allow_once"}
        ]
    }));
    assert!(matches!(
        handoff.permission(&permission),
        ClaudeHandoffPermission::ArtifactMutation
    ));

    let early_exit = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "exit-plan",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "pending",
        "rawInput": {"plan": "# Final plan"},
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&early_exit)
        .expect("ExitPlanMode announcement may arrive early");
    assert!(
        handoff
            .begin_capture("exit-plan", "# Final plan")
            .unwrap_err()
            .contains("artifacts remain unresolved")
    );

    let completed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "write-plan",
        "status": "completed",
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&completed)
        .expect("validated Write may complete without repeating its path");
    handoff
        .inspect_update(&completed)
        .expect("duplicate terminal status is idempotent");

    handoff
        .inspect_update(&overlapping)
        .expect("terminal first mutation permits a different artifact");
    let second_completed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "second-write",
        "status": "completed",
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&second_completed)
        .expect("second artifact becomes terminal");
    assert!(handoff.unresolved_violation().is_none());

    handoff
        .inspect_update(&early_exit)
        .expect("ExitPlanMode is accepted after all artifacts are terminal");
    let exit_permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "exit-plan",
            "kind": "switch_mode",
            "rawInput": {"plan": "# Final plan"},
            "content": [{"type": "content", "content": {"type": "text", "text": "# Final plan"}}]
        },
        "options": [{"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}]
    }));
    assert!(matches!(
        handoff.permission(&exit_permission),
        ClaudeHandoffPermission::ExitPlanMode { source: ClaudePlanCandidate::Explicit(plan) } if plan == "# Final plan"
    ));
    handoff
        .begin_capture("exit-plan", "# Final plan")
        .expect("terminal artifacts permit capture");
    handoff
        .finish_capture("exit-plan")
        .expect("capture completes");

    let post_handoff = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "post-handoff",
        "title": "Write after handoff",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": plans.join("late.md")},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(
        handoff
            .inspect_update(&post_handoff)
            .expect_err("new artifact after handoff is rejected")
            .contains("after handoff began")
    );
}

#[test]
fn claude_plan_artifact_operations_preserve_identity_and_permission_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let first_path = plans.join("draft.md");
    let second_path = plans.join("final.md");

    let failed_write = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "failed-write",
        "title": "Write draft",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": first_path},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&failed_write)
        .expect("validated Write begins");
    let failed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "failed-write",
        "status": "failed",
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&failed)
        .expect("failed path-resolved mutation is terminal");

    let edit = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "edit-plan",
        "title": "Edit draft",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": first_path},
        "locations": [{"path": first_path}],
        "_meta": {"claudeCode": {"toolName": "Edit"}}
    }));
    handoff
        .inspect_update(&edit)
        .expect("failed mutation permits Edit on the same artifact");

    let edit_permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "edit-plan",
            "kind": "edit",
            "rawInput": {"file_path": first_path},
            "locations": [{"path": first_path}],
            "_meta": {"claudeCode": {"toolName": "Edit"}}
        },
        "options": [{"optionId": "once", "name": "Once", "kind": "allow_once"}]
    }));
    assert!(matches!(
        handoff.permission(&edit_permission),
        ClaudeHandoffPermission::ArtifactMutation
    ));

    let missing_kind_permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "edit-plan",
            "rawInput": {"file_path": first_path},
            "_meta": {"claudeCode": {"toolName": "Edit"}}
        },
        "options": [{"optionId": "once", "name": "Once", "kind": "allow_once"}]
    }));
    assert!(matches!(
        handoff.permission(&missing_kind_permission),
        ClaudeHandoffPermission::Invalid(error)
            if error.contains("non-edit tool kind")
    ));

    let mismatched_permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "edit-plan",
            "kind": "edit",
            "rawInput": {"file_path": first_path},
            "_meta": {"claudeCode": {"toolName": "Write"}}
        },
        "options": [{"optionId": "once", "name": "Once", "kind": "allow_once"}]
    }));
    assert!(matches!(
        handoff.permission(&mismatched_permission),
        ClaudeHandoffPermission::Invalid(error)
            if error.contains("metadata changed tool identity")
    ));

    let changed_operation = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "edit-plan",
        "kind": "edit",
        "rawInput": {"file_path": first_path},
        "_meta": {"claudeCode": {"toolName": "MultiEdit"}}
    }));
    assert!(
        handoff
            .inspect_update(&changed_operation)
            .expect_err("tool operation identity is immutable")
            .contains("changed operation")
    );

    let changed_target = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "edit-plan",
        "kind": "edit",
        "rawInput": {"file_path": second_path},
        "_meta": {"claudeCode": {"toolName": "Edit"}}
    }));
    assert!(
        handoff
            .inspect_update(&changed_target)
            .expect_err("tool target is immutable")
            .contains("changed targets")
    );

    let edit_completed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "edit-plan",
        "status": "completed",
        "_meta": {"claudeCode": {"toolName": "Edit"}}
    }));
    handoff
        .inspect_update(&edit_completed)
        .expect("Edit becomes terminal");
    assert!(matches!(
        handoff.permission(&edit_permission),
        ClaudeHandoffPermission::Invalid(error)
            if error.contains("after reaching terminal status")
    ));

    let regression = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "edit-plan",
        "status": "in_progress",
        "_meta": {"claudeCode": {"toolName": "Edit"}}
    }));
    assert!(
        handoff
            .inspect_update(&regression)
            .expect_err("terminal status cannot regress")
            .contains("regressed from terminal status")
    );

    let multi_edit = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "multi-edit-plan",
        "title": "Edit final plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": second_path},
        "content": [{
            "type": "diff",
            "path": second_path,
            "oldText": "draft",
            "newText": "final"
        }],
        "_meta": {"claudeCode": {"toolName": "MultiEdit"}}
    }));
    handoff
        .inspect_update(&multi_edit)
        .expect("terminal Edit permits MultiEdit on a second artifact");
    let multi_permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "multi-edit-plan",
            "kind": "edit",
            "rawInput": {"file_path": second_path},
            "_meta": {"claudeCode": {"toolName": "MultiEdit"}}
        },
        "options": [{"optionId": "once", "name": "Once", "kind": "allow_once"}]
    }));
    assert!(matches!(
        handoff.permission(&multi_permission),
        ClaudeHandoffPermission::ArtifactMutation
    ));
}

#[test]
fn claude_workspace_plan_artifact_directory_is_prepared_only_for_permission() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (workspace, _external_plans, handoff) = claude_handoff_fixture(&directory);
    let workspace_plans = workspace.join(".claude").join("plans");
    let plan_path = workspace_plans.join("workspace-handoff.md");
    assert!(!workspace_plans.exists());

    let pending = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "write-workspace-plan",
        "title": "Preparing file…",
        "kind": "edit",
        "status": "pending",
        "content": [],
        "locations": [],
        "rawInput": {},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&pending)
        .expect("pathless initial Write is tracked");

    let refined = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "write-workspace-plan",
        "title": format!("Write {}", plan_path.display()),
        "kind": "edit",
        "rawInput": {"file_path": plan_path},
        "locations": [{"path": plan_path}],
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&refined)
        .expect("workspace-local direct Markdown child is valid");
    assert!(
        !workspace_plans.exists(),
        "observing a tool update must not mutate the workspace"
    );

    let permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "write-workspace-plan",
            "kind": "edit",
            "rawInput": {"file_path": plan_path},
            "locations": [{"path": plan_path}]
        },
        "options": [
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
            {"optionId": "once", "name": "Once", "kind": "allow_once"}
        ]
    }));
    assert!(matches!(
        handoff.permission(&permission),
        ClaudeHandoffPermission::ArtifactMutation
    ));
    assert!(!workspace_plans.exists());

    let attempt = handoff.start_attempt(CancellationToken::new());
    let ticket = handoff
        .register_permission(&permission, attempt.id)
        .expect("queued permission");
    assert!(
        !workspace_plans.exists(),
        "queueing must not prepare directories"
    );
    assert_eq!(
        ticket.try_admit(&permission, || false).unwrap(),
        ClaudePlanAdmission::Granted
    );
    ticket.delivered();
    assert!(workspace_plans.is_dir());
    assert!(!workspace_plans.join(".gitignore").exists());
    assert!(!plan_path.exists());
}

#[test]
fn claude_plan_artifact_guard_rejects_every_other_mutation_shape() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let elsewhere = directory.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("external directory");

    for (tool_call_id, path) in [
        ("external", elsewhere.join("plan.md")),
        ("workspace", workspace.join("plan.md")),
        (
            "workspace-claude",
            workspace.join(".claude").join("plan.md"),
        ),
        (
            "workspace-nested",
            workspace
                .join(".claude")
                .join("plans")
                .join("nested")
                .join("plan.md"),
        ),
        ("not-markdown", plans.join("plan.txt")),
        (
            "workspace-not-markdown",
            workspace.join(".claude").join("plans").join("plan.txt"),
        ),
    ] {
        let update = acp_update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": format!("Write {}", path.display()),
            "kind": "edit",
            "status": "pending",
            "rawInput": {"file_path": path},
            "locations": [{"path": path}],
            "_meta": {"claudeCode": {"toolName": "Write"}}
        }));
        assert!(
            handoff.inspect_update(&update).is_err(),
            "invalid Write target {tool_call_id} was accepted"
        );
    }

    let nested = plans.join("nested");
    std::fs::create_dir(&nested).expect("nested plans directory");
    let nested_update = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "nested",
        "title": "Write nested plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": nested.join("plan.md")},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(handoff.inspect_update(&nested_update).is_err());

    let missing_metadata = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "metadata-free",
        "title": "Write plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": plans.join("metadata-free.md")}
    }));
    assert!(handoff.inspect_update(&missing_metadata).is_err());

    for kind in ["edit", "delete", "move"] {
        let mutation = acp_update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": format!("ordinary-{kind}"),
            "title": "Ordinary workspace mutation",
            "kind": kind,
            "status": "pending",
            "locations": [{"path": workspace.join("src.rs")}],
            "_meta": {"claudeCode": {"toolName": "Edit"}}
        }));
        assert!(handoff.inspect_update(&mutation).is_err());
    }

    let unresolved = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "unresolved",
        "title": "Preparing file…",
        "kind": "edit",
        "status": "pending",
        "rawInput": {},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    handoff
        .inspect_update(&unresolved)
        .expect("initial unresolved write is provisional");
    let unresolved_completion = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "unresolved",
        "status": "completed",
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(handoff.inspect_update(&unresolved_completion).is_err());
    assert!(handoff.unresolved_violation().is_none());
    assert!(
        handoff
            .abandoned_preparations()
            .unwrap()
            .contains("unresolved")
    );
}

#[cfg(unix)]
#[test]
fn claude_plan_artifact_guard_rejects_external_and_workspace_symlinks() {
    use std::os::unix::fs::symlink;

    let external_fixture = tempfile::tempdir().expect("temporary directory");
    let (workspace, plans, external_handoff) = claude_handoff_fixture(&external_fixture);
    let workspace_target = workspace.join("source.md");
    std::fs::write(&workspace_target, "workspace source").expect("workspace source");
    let external_symlink = plans.join("linked.md");
    symlink(&workspace_target, &external_symlink).expect("external artifact symlink");
    let external_update = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "external-symlink",
        "title": "Write linked plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": external_symlink},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(external_handoff.inspect_update(&external_update).is_err());

    let target_fixture = tempfile::tempdir().expect("temporary directory");
    let (workspace, _plans, target_handoff) = claude_handoff_fixture(&target_fixture);
    let workspace_plans = workspace.join(".claude").join("plans");
    std::fs::create_dir_all(&workspace_plans).expect("workspace plans directory");
    let target = workspace.join("target.md");
    std::fs::write(&target, "workspace target").expect("workspace target");
    let workspace_symlink = workspace_plans.join("linked.md");
    symlink(&target, &workspace_symlink).expect("workspace artifact symlink");
    let target_update = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "workspace-target-symlink",
        "title": "Write linked workspace plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": workspace_symlink},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(target_handoff.inspect_update(&target_update).is_err());

    let ancestor_fixture = tempfile::tempdir().expect("temporary directory");
    let (workspace, _plans, ancestor_handoff) = claude_handoff_fixture(&ancestor_fixture);
    let escape = ancestor_fixture.path().join("workspace-claude-escape");
    std::fs::create_dir_all(escape.join("plans")).expect("symlink destination");
    symlink(&escape, workspace.join(".claude")).expect("workspace .claude symlink");
    let ancestor_target = workspace.join(".claude").join("plans").join("escaped.md");
    let ancestor_update = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "workspace-ancestor-symlink",
        "title": "Write escaped workspace plan",
        "kind": "edit",
        "status": "pending",
        "rawInput": {"file_path": ancestor_target},
        "_meta": {"claudeCode": {"toolName": "Write"}}
    }));
    assert!(ancestor_handoff.inspect_update(&ancestor_update).is_err());
}

#[cfg(windows)]
#[test]
fn claude_artifact_paths_accept_both_drive_prefix_forms_without_resolving_aliases() {
    fn ordinary(path: &Path) -> PathBuf {
        let text = path.to_str().expect("Unicode test path");
        PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
    }

    let directory = tempfile::tempdir().unwrap();
    let (workspace, plans, _handoff) = claude_handoff_fixture(&directory);
    let workspace_plans = workspace_claude_plan_artifact_directory(&workspace);
    let ordinary_workspace = ordinary(&workspace);
    let ordinary_plans = ordinary(&plans);
    let ordinary_workspace_plans = ordinary(&workspace_plans);
    // Check the missing-target permission path, then the existing-file path.
    for existing in [false, true] {
        if existing {
            std::fs::create_dir_all(&workspace_plans).unwrap();
            for root in [&plans, &workspace_plans] {
                std::fs::write(root.join("plan 雪 space.md"), "# Plan").unwrap();
            }
        }
        for (root, expected_root) in [
            (&plans, &plans),
            (&ordinary_plans, &plans),
            (&workspace_plans, &workspace_plans),
            (&ordinary_workspace_plans, &workspace_plans),
        ] {
            for (workspace, plans, workspace_plans) in [
                (&workspace, &plans, &workspace_plans),
                (
                    &ordinary_workspace,
                    &ordinary_plans,
                    &ordinary_workspace_plans,
                ),
            ] {
                validate_workspace_claude_plan_artifact_directory(workspace_plans, workspace)
                    .unwrap();
                let target = root.join("plan 雪 space.md");
                assert_eq!(
                    validate_claude_plan_artifact_path(&target, plans, workspace_plans, workspace)
                        .unwrap(),
                    expected_root.join("plan 雪 space.md")
                );
                for invalid in [root.join("plan.md:stream"), root.join(r"..\plan.md")] {
                    assert!(
                        validate_claude_plan_artifact_path(
                            &invalid,
                            plans,
                            workspace_plans,
                            workspace
                        )
                        .is_err()
                    );
                }
            }
        }
    }
    let mut agent = EnsembleConfig::default().agents["claude"].clone();
    for workspace_form in [&workspace, &ordinary_workspace] {
        for config in [
            workspace.join(".claude"),
            ordinary_workspace.join(".claude"),
        ] {
            agent.env.insert(
                CLAUDE_CONFIG_DIR_ENV.to_owned(),
                config.display().to_string(),
            );
            assert_eq!(
                claude_plan_artifact_directory(&agent, workspace_form).unwrap(),
                workspace_plans
            );
        }
    }
}

#[test]
fn claude_plan_artifact_directory_resolution_fails_closed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let mut agent = EnsembleConfig::default().agents["claude"].clone();

    agent.env.insert(
        CLAUDE_CONFIG_DIR_ENV.to_string(),
        directory
            .path()
            .join("missing-config")
            .display()
            .to_string(),
    );
    assert!(ClaudePlanHandoff::new(&agent, &workspace).is_err());

    let in_workspace = workspace.join("claude-config");
    std::fs::create_dir_all(in_workspace.join("plans")).expect("workspace plans directory");
    agent.env.insert(
        CLAUDE_CONFIG_DIR_ENV.to_string(),
        in_workspace.display().to_string(),
    );
    assert!(ClaudePlanHandoff::new(&agent, &workspace).is_err());

    let workspace_claude = workspace.join(".claude");
    agent.env.insert(
        CLAUDE_CONFIG_DIR_ENV.to_string(),
        workspace_claude.display().to_string(),
    );
    ClaudePlanHandoff::new(&agent, &workspace)
        .expect("the exact workspace-local .claude/plans root is permitted");
    assert!(
        !workspace_claude.exists(),
        "handoff initialization must not create the workspace plan directory"
    );

    agent
        .env
        .insert(CLAUDE_CONFIG_DIR_ENV.to_string(), String::new());
    assert!(ClaudePlanHandoff::new(&agent, &workspace).is_err());
}

#[test]
fn claude_exit_plan_mode_refines_a_pending_call_from_permission() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let pending = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "exit-plan",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "pending",
        "rawInput": {},
        "content": [],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&pending)
        .expect("payload-free pending ExitPlanMode is provisional");
    assert!(
        handoff
            .unresolved_violation()
            .is_some_and(|error| error.contains("never resolved to a nonempty plan"))
    );

    let plan = "# Plan\r\n\r\n- Keep the workspace unchanged.\r\n";
    let normalized = "# Plan\n\n- Keep the workspace unchanged.";
    let missing = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "exit-plan",
            "kind": "switch_mode",
            "rawInput": {},
            "content": []
        },
        "options": [{"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}]
    }));
    assert!(matches!(
        handoff.permission(&missing),
        ClaudeHandoffPermission::ExitPlanMode {
            source: ClaudePlanCandidate::Artifact { .. }
        }
    ));

    let permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "exit-plan",
            "kind": "switch_mode",
            "rawInput": {
                "plan": plan,
                "planFilePath": plans.join("native-plan.md")
            },
            "content": [{"type": "content", "content": {"type": "text", "text": normalized}}]
        },
        "options": [
            {"optionId": "default", "name": "Yes", "kind": "allow_once"},
            {"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}
        ]
    }));
    assert!(matches!(
        handoff.permission(&permission),
        ClaudeHandoffPermission::ExitPlanMode { source: ClaudePlanCandidate::Explicit(plan) } if plan == normalized
    ));
    assert!(handoff.unresolved_violation().is_none());

    let mismatched = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "exit-plan",
            "kind": "switch_mode",
            "rawInput": {"plan": "different"}
        },
        "options": [{"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}]
    }));
    assert!(matches!(
        handoff.permission(&mismatched),
        ClaudeHandoffPermission::Invalid(_)
    ));

    handoff
        .begin_capture("exit-plan", normalized)
        .expect("resolved permission begins capture");
    handoff
        .finish_capture("exit-plan")
        .expect("resolved permission completes capture");
    assert!(handoff.is_completed());
    assert!(
        handoff
            .begin_capture("exit-plan", normalized)
            .unwrap_err()
            .contains("more than one")
    );
}

#[test]
fn claude_exit_plan_mode_accepts_failed_terminal_output_after_capture() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, plans, handoff) = claude_handoff_fixture(&directory);
    let plan = "# Recorded plan\n\n- Preserve the exact payload.";

    let pending = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "exit-plan",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "pending",
        "rawInput": {},
        "content": [],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&pending)
        .expect("payload-free pending ExitPlanMode is provisional");

    let populated = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "exit-plan",
        "status": "in_progress",
        "rawInput": {"plan": plan},
        "content": [{"type": "content", "content": {"type": "text", "text": plan}}],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&populated)
        .expect("populated update resolves the pending plan");

    let permission = permission_request(serde_json::json!({
        "sessionId": "session",
        "toolCall": {
            "toolCallId": "exit-plan",
            "kind": "switch_mode",
            "rawInput": {
                "plan": plan,
                "planFilePath": plans.join("recorded-plan.md")
            },
            "content": [{"type": "content", "content": {"type": "text", "text": plan}}]
        },
        "options": [
            {"optionId": "default", "name": "Yes", "kind": "allow_once"},
            {"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}
        ]
    }));
    assert!(matches!(
        handoff.permission(&permission),
        ClaudeHandoffPermission::ExitPlanMode { source: ClaudePlanCandidate::Explicit(captured) } if captured == plan
    ));
    handoff
        .begin_capture("exit-plan", plan)
        .expect("permission rejection begins durable capture");

    let failed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "exit-plan",
        "status": "failed",
        "rawOutput": "User rejected request to exit plan mode.",
        "content": [{
            "type": "content",
            "content": {
                "type": "text",
                "text": "```\nUser rejected request to exit plan mode.\n```"
            }
        }],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&failed)
        .expect("terminal provider output is not a replacement plan while capturing");
    handoff
        .finish_capture("exit-plan")
        .expect("native handoff completes despite the rejected provider tool");
    handoff
        .inspect_update(&failed)
        .expect("the same terminal output is also valid after durable capture");
    assert!(handoff.is_completed());
    assert!(handoff.unresolved_violation().is_none());

    let changed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "exit-plan",
        "status": "failed",
        "rawInput": {"plan": "# Mutated plan"},
        "content": [{
            "type": "content",
            "content": {"type": "text", "text": "Provider result"}
        }],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    assert!(
        handoff
            .inspect_update(&changed)
            .expect_err("terminal raw plan identity remains immutable")
            .contains("changed its plan payload")
    );
}

#[test]
fn claude_exit_plan_mode_rejects_malformed_or_terminal_pending_payloads() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, _plans, handoff) = claude_handoff_fixture(&directory);
    for (tool_call_id, kind, raw_input) in [
        (
            "empty",
            "switch_mode",
            serde_json::json!({"plan": "  \r\n "}),
        ),
        ("wrong-type", "switch_mode", serde_json::json!({"plan": 7})),
        ("wrong-kind", "edit", serde_json::json!({})),
    ] {
        let invalid = acp_update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": "Ready to code?",
            "kind": kind,
            "status": "pending",
            "rawInput": raw_input,
            "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
        }));
        assert!(handoff.inspect_update(&invalid).is_err());
    }

    let pending = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "unresolved",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "pending",
        "rawInput": {},
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&pending)
        .expect("pending ExitPlanMode is provisional");
    let completed = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "unresolved",
        "status": "failed",
        "content": [{
            "type": "content",
            "content": {"type": "text", "text": "# Tool result is not plan proof"}
        }],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    handoff
        .inspect_update(&completed)
        .expect("rejected exit terminal is tool output, not plan proof");
    assert!(handoff.unresolved_violation().is_some());

    let disagreement = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "disagreement",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "in_progress",
        "rawInput": {"plan": "# Raw plan"},
        "content": [{
            "type": "content",
            "content": {"type": "text", "text": "# Rendered plan"}
        }],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    assert!(
        handoff
            .inspect_update(&disagreement)
            .expect_err("nonterminal raw and rendered plans must still agree")
            .contains("raw input and rendered plan content disagree")
    );

    let complete_directory = tempfile::tempdir().expect("temporary directory");
    let (_workspace, _plans, complete_handoff) = claude_handoff_fixture(&complete_directory);
    let complete = acp_update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "complete",
        "title": "Ready to code?",
        "kind": "switch_mode",
        "status": "pending",
        "rawInput": {"plan": "# Complete plan"},
        "content": [{"type": "content", "content": {"type": "text", "text": "# Complete plan"}}],
        "_meta": {"claudeCode": {"toolName": "ExitPlanMode"}}
    }));
    complete_handoff
        .inspect_update(&complete)
        .expect("fully populated initial ExitPlanMode remains supported");
}

#[test]
fn native_handoff_permission_options_follow_acp_semantics_without_fallbacks() {
    let allow_always = vec![permission(PermissionOptionKind::AllowAlways)];
    assert!(artifact_mutation_permission_option(&allow_always).is_none());

    let allow_only = vec![
        permission(PermissionOptionKind::AllowAlways),
        permission(PermissionOptionKind::AllowOnce),
    ];
    assert!(keep_planning_permission_option(&allow_only).is_none());

    let reject_always = vec![PermissionOption::new(
        "reject-forever",
        "Never allow",
        PermissionOptionKind::RejectAlways,
    )];
    assert!(keep_planning_permission_option(&reject_always).is_none());

    for (option_id, name) in [
        ("plan", "No, keep planning"),
        ("reject", "No, keep planning"),
        ("adapter-defined-denial", "Arbitrary adapter label"),
    ] {
        let options = vec![PermissionOption::new(
            option_id,
            name,
            PermissionOptionKind::RejectOnce,
        )];
        let selected = keep_planning_permission_option(&options)
            .expect("every one-shot rejection is a valid keep-planning choice");
        assert_eq!(selected.option_id.to_string(), option_id);
    }
}

#[tokio::test]
async fn oversized_raw_diagnostics_are_truncated_without_failing_the_worker() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("oversized_diagnostic_acp.py");
    std::fs::write(
            &script,
            format!(
                r#"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{{"value":"read-only","name":"Read only"}}]}}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({{"jsonrpc":"2.0","id":request_id,"result":{{"protocolVersion":1,"agentCapabilities":{{}},"agentInfo":{{"name":"noisy-acp","version":"1"}}}}}})
    elif method == "session/new":
        send({{"jsonrpc":"2.0","id":request_id,"result":{{"sessionId":"noisy-session","configOptions":options()}}}})
    elif method == "session/set_config_option":
        send({{"jsonrpc":"2.0","id":request_id,"result":{{"configOptions":options()}}}})
    elif method == "session/prompt":
        for _ in range({line_count}):
            sys.stderr.write("x" * {line_bytes} + "\n")
        sys.stderr.flush()
        send({{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"noisy-session","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"useful report"}},"messageId":"message"}}}}}})
        # The SDK bounds stderr lines itself. An oversized protocol response
        # reaches our raw-line limiter before the worker can observe completion.
        send({{"jsonrpc":"2.0","id":request_id,"result":{{"stopReason":"end_turn","_meta":{{"diagnostic":"x" * {line_bytes}}}}}}})
"#,
                // Exceed the raw per-line limit before image-safe redaction.
                // Sub-limit runs of `x` shrink to a short opaque-token placeholder
                // and only triggered truncation when the queue happened to fill.
                line_count = 3,
                line_bytes = RAW_DIAGNOSTIC_MAX_LINE_BYTES + 1,
            ),
        )
        .expect("noisy ACP script");
    let agent = EnsembleAgentConfig {
        label: "Noisy ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review noisily".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptor"),
    };
    let (events, _receiver) = session_event_channel(256);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(9), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("worker launch");
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    assert_eq!(outcomes[0].report, "useful report");

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("bounded transcript");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Unsupported { context, placeholder }
        } if context == "ACP raw diagnostics" && placeholder.contains("truncated")
    )));
    assert!(records.iter().all(|record| !matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text } | AgentRunEvent::Protocol { json: text, .. }
        } if text.len() > RAW_DIAGNOSTIC_MAX_LINE_BYTES
    )));
}

#[tokio::test]
async fn session_scoped_elicitation_before_session_start_is_declined_not_failed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("early_elicitation_acp.py");
    std::fs::write(
            &script,
            r##"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"early-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":777,"method":"elicitation/create","params":{"mode":"form","sessionId":"future-session","message":"too early","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}}})
        while True:
            early_response = json.loads(sys.stdin.readline())
            if early_response.get("id") == 777:
                break
        sys.stderr.write("early-response:" + json.dumps(early_response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"future-session","configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"future-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"continued after decline"},"messageId":"message"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"##,
        )
        .expect("early elicitation ACP script");
    let agent = EnsembleAgentConfig {
        label: "Early ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "keep starting".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptor"),
    };
    let (events, _receiver) = session_event_channel(256);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(
                TurnId::new(10),
                SessionMode::Build,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("worker launch");
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    assert_eq!(outcomes[0].report, "continued after decline");

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("worker transcript");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Elicitation {
                field_count: 1,
                outcome: AgentElicitationOutcome::Unsupported,
                ..
            }
        }
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("early-response") && text.contains("decline")
    )));
}

fn elicitation_from_json(value: serde_json::Value) -> CreateElicitationRequest {
    serde_json::from_value(value).expect("valid typed elicitation fixture")
}

fn convert_fixture(value: serde_json::Value) -> ConvertedElicitation {
    convert_elicitation(
        &elicitation_from_json(value),
        Some("session-1"),
        "Fixture ACP",
        QuestionRequestId::new("question-1"),
    )
    .expect("supported elicitation")
}

const NATIVE_OTHER: &str = "__zevria_other__";

fn native_elicitation_fixture() -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    for index in 0..3 {
        let name = format!("question_{index}");
        properties.insert(name.clone(), serde_json::json!({
                "type": "string", "title": format!("Choice {index}"), "description": "Choose a scope",
                "oneOf": [
                    {"const": "option_0", "title": "Focused", "description": "Keep it narrow."},
                    {"const": "option_1", "title": "Broad"},
                    {"const": NATIVE_OTHER, "title": "Other"}
                ]
            }));
        properties.insert(format!("{name}_other"), serde_json::json!({
                "type": "string", "title": format!("Choice {index} — Other"),
                "description": "Custom answer used when Other is selected.",
                "_meta": {"zevria": {"questionId": name, "isOtherAnswer": true, "otherValue": NATIVE_OTHER}}
            }));
    }
    serde_json::json!({
        "mode": "form", "sessionId": "session-1", "message": "Zevria needs additional information.",
        "requestedSchema": {"type": "object", "properties": properties,
            "required": ["question_0", "question_1", "question_2"]}
    })
}

fn fixture_answers(
    converted: &ConvertedElicitation,
    answers: Vec<Option<QuestionAnswerValue>>,
) -> QuestionResponse {
    assert_eq!(converted.request.questions.len(), answers.len());
    QuestionResponse::Answered {
        answers: converted
            .request
            .questions
            .iter()
            .zip(answers)
            .map(|(question, answer)| zevria_foundation::QuestionAnswer {
                id: question.id.clone(),
                answer,
            })
            .collect(),
    }
}

#[test]
fn native_three_question_batch_folds_companions_and_records_only_display_decisions() {
    let fixture = native_elicitation_fixture();
    assert_eq!(
        elicitation_field_count(&elicitation_from_json(fixture.clone())),
        3
    );
    let converted = convert_fixture(fixture);
    assert_eq!(converted.fields.len(), 3);
    for (index, question) in converted.request.questions.iter().enumerate() {
        assert_eq!(question.id, format!("question_{index}"));
        assert!(question.required, "required prompts must not offer Skip");
        assert_eq!(
            question.kind,
            QuestionPromptKind::SingleSelect { allow_other: true }
        );
        assert_eq!(
            question
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Focused", "Broad"]
        );
        assert_eq!(question.options[0].description, "Keep it narrow.");
    }
    let answers = ["Focused", "A custom scope", "Broad"];
    let response = fixture_answers(
        &converted,
        answers
            .iter()
            .map(|answer| Some(QuestionAnswerValue::String((*answer).into())))
            .collect(),
    );
    let decision = converted.normalized_decision(&response).unwrap();
    assert_eq!(decision.answers.len(), 3);
    for (index, answer) in decision.answers.iter().enumerate() {
        assert_eq!(answer.question_id, format!("question_{index}"));
        assert_eq!(
            answer.decision_id,
            AgentUserDecisionId::from_question(&converted.request.id, &answer.question_id)
        );
        assert_eq!(
            answer.answer,
            AgentUserDecisionValue::String {
                value: answers[index].into()
            }
        );
    }
    let wire = converted.accepted_content(response).unwrap();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        serde_json::json!({
            "question_0": "option_0", "question_1": NATIVE_OTHER,
            "question_1_other": "A custom scope", "question_2": "option_1"
        })
    );
    for invalid in [
        None,
        Some(QuestionAnswerValue::String(" \n".into())),
        Some(QuestionAnswerValue::Strings(vec!["Focused".into()])),
    ] {
        let response = fixture_answers(
            &converted,
            vec![
                invalid,
                Some(QuestionAnswerValue::String("Focused".into())),
                Some(QuestionAnswerValue::String("Broad".into())),
            ],
        );
        assert!(converted.accepted_content(response).is_err());
    }
    assert!(
        converted
            .accepted_content(QuestionResponse::Dismissed)
            .is_err()
    );
}

#[test]
fn native_single_and_multi_defaults_skip_and_custom_tokens_round_trip() {
    let mut fixture = native_elicitation_fixture();
    fixture["requestedSchema"]["required"] = serde_json::json!(["question_1", "question_2"]);
    let properties = &mut fixture["requestedSchema"]["properties"];
    properties["question_0"]["default"] = serde_json::json!(NATIVE_OTHER);
    properties["question_0_other"]["default"] = serde_json::json!("Custom single");
    properties["question_1"]["default"] = serde_json::json!("option_1");
    properties["question_2"] = serde_json::json!({
        "type": "array", "items": {"anyOf": [
            {"const": "option_0", "title": "Other", "description": "A real option named Other"},
            {"const": NATIVE_OTHER, "title": "Other"},
            {"const": "option_1", "title": "Other"}
        ]}, "minItems": 2, "maxItems": 2, "default": ["option_0", NATIVE_OTHER]
    });
    properties["question_2_other"]["default"] = serde_json::json!("Custom multi");
    let converted = convert_fixture(fixture);
    let prompts = &converted.request.questions;
    assert!(!prompts[0].required);
    assert_eq!(
        prompts[0].default,
        Some(QuestionAnswerValue::String("Custom single".into()))
    );
    assert_eq!(
        prompts[1].default,
        Some(QuestionAnswerValue::String("Broad".into()))
    );
    assert_eq!(
        prompts[2]
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>(),
        ["Other (option)", "Other (option 3)"]
    );
    assert_eq!(
        prompts[2].options[0].description,
        "A real option named Other"
    );
    assert_eq!(
        prompts[2].kind,
        QuestionPromptKind::MultiSelect {
            min_selections: Some(2),
            max_selections: Some(2),
            allow_other: true
        }
    );
    assert_eq!(
        prompts[2].default,
        Some(QuestionAnswerValue::Strings(vec![
            "Other (option)".into(),
            "Custom multi".into()
        ]))
    );
    let wire = converted
        .accepted_content(fixture_answers(
            &converted,
            prompts
                .iter()
                .map(|prompt| prompt.default.clone())
                .collect(),
        ))
        .unwrap();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        serde_json::json!({
            "question_0": NATIVE_OTHER, "question_0_other": "Custom single", "question_1": "option_1",
            "question_2": ["option_0", NATIVE_OTHER], "question_2_other": "Custom multi"
        })
    );
    let normal = Some(QuestionAnswerValue::String("Focused".into()));
    let response = fixture_answers(
        &converted,
        vec![
            None,
            normal.clone(),
            Some(QuestionAnswerValue::Strings(vec![
                "Other (option)".into(),
                "Other (option 3)".into(),
            ])),
        ],
    );
    let decision = converted.normalized_decision(&response).unwrap();
    assert_eq!(decision.answers[0].answer, AgentUserDecisionValue::Skipped);
    let wire = converted.accepted_content(response).unwrap();
    assert!(!wire.contains_key("question_0"));
    assert!(!wire.contains_key("question_0_other"));
    assert!(!wire.contains_key("question_2_other"));
    assert_eq!(
        wire["question_2"],
        ElicitationContentValue::StringArray(vec!["option_0".into(), "option_1".into()])
    );
    for invalid in [
        vec!["Other (option)", " "],
        vec!["custom one", "custom two"],
    ] {
        assert!(
            converted
                .accepted_content(fixture_answers(
                    &converted,
                    vec![
                        None,
                        normal.clone(),
                        Some(QuestionAnswerValue::Strings(
                            invalid.into_iter().map(str::to_string).collect()
                        ))
                    ]
                ))
                .is_err()
        );
    }
}

fn assert_native_declined(fixture: serde_json::Value) {
    let request = serde_json::from_value(fixture.clone())
        .unwrap_or_else(|error| panic!("invalid typed fixture: {error}: {fixture}"));
    assert!(
        matches!(
            convert_elicitation(
                &request,
                Some("session-1"),
                "Native",
                QuestionRequestId::new("invalid")
            ),
            Err(ElicitationConversionError::Decline { .. })
        ),
        "must decline {request:?}"
    );
}

#[test]
fn malformed_native_companions_and_unrepresentable_defaults_are_declined() {
    use serde_json::json;
    for marker in [
        json!(true),
        json!({"isOtherAnswer": "true", "questionId": "question_0", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": 0, "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": " ", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": "missing", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": "question_0_other", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": "question_1", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": "question_1_other", "otherValue": NATIVE_OTHER}),
        json!({"isOtherAnswer": true, "questionId": "question_0"}),
        json!({"isOtherAnswer": true, "questionId": "question_0", "otherValue": false}),
        json!({"isOtherAnswer": true, "questionId": "question_0", "otherValue": " "}),
    ] {
        let mut fixture = native_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0_other"]["_meta"]["zevria"] = marker;
        assert_native_declined(fixture);
    }
    for (key, value) in [
        ("pattern", json!(".*")),
        ("format", json!("email")),
        ("minLength", json!(0)),
        ("maxLength", json!(80)),
        ("enum", json!(["custom"])),
        ("oneOf", json!([{"const": "custom", "title": "Custom"}])),
        ("type", json!("boolean")),
    ] {
        let mut fixture = native_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0_other"][key] = value;
        assert_native_declined(fixture);
    }
    let mut required_companion = native_elicitation_fixture();
    required_companion["requestedSchema"]["required"] = json!(["question_0_other"]);
    assert_native_declined(required_companion);
    let mut secret = native_elicitation_fixture();
    secret["requestedSchema"]["properties"]["question_0_other"]["_meta"]["zevria"]["isSecret"] =
        json!(true);
    assert_native_declined(secret);
    for primary in [json!({"type": "string"}), json!({"type": "boolean"})] {
        let mut fixture = native_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0"] = primary;
        assert_native_declined(fixture);
    }
    for multi in [false, true] {
        for choices in [
            json!(["option_0"]),
            json!([NATIVE_OTHER]),
            json!(["option_0", NATIVE_OTHER, NATIVE_OTHER]),
            json!(["option_0", "option_0", NATIVE_OTHER]),
        ] {
            let mut fixture = native_elicitation_fixture();
            fixture["requestedSchema"]["properties"]["question_0"] = if multi {
                json!({"type": "array", "items": {"type": "string", "enum": choices}})
            } else {
                json!({"type": "string", "enum": choices})
            };
            assert_native_declined(fixture);
        }
        for custom in [
            None,
            Some(json!(" \n")),
            Some(json!("Focused")),
            Some(json!(" Focused ")),
        ] {
            let mut fixture = native_elicitation_fixture();
            let properties = &mut fixture["requestedSchema"]["properties"];
            if multi {
                properties["question_0"] = json!({"type": "array", "items": {"anyOf": properties["question_0"]["oneOf"].clone()}, "default": ["option_0", NATIVE_OTHER]});
            } else {
                properties["question_0"]["default"] = json!(NATIVE_OTHER);
            }
            if let Some(custom) = custom {
                properties["question_0_other"]["default"] = custom;
            }
            assert_native_declined(fixture);
        }
    }
    for (min, max, defaults) in [
        (4, 4, json!([])),
        (2, 1, json!([])),
        (2, 2, json!([NATIVE_OTHER])),
        (0, 1, json!(["option_0", NATIVE_OTHER])),
        (0, 3, json!([NATIVE_OTHER, NATIVE_OTHER])),
        (0, 3, json!(["unknown"])),
    ] {
        let mut fixture = native_elicitation_fixture();
        let properties = &mut fixture["requestedSchema"]["properties"];
        properties["question_0"] = json!({"type": "array", "items": {"anyOf": properties["question_0"]["oneOf"].clone()}, "minItems": min, "maxItems": max, "default": defaults});
        properties["question_0_other"]["default"] = json!("Custom");
        assert_native_declined(fixture);
    }
}

#[test]
fn native_marker_is_explicit_and_legacy_companion_restrictions_remain_distinct() {
    use serde_json::json;
    for marker in [json!({}), json!({"zevria": {"isOtherAnswer": false}})] {
        let mut fixture = native_elicitation_fixture();
        for index in 0..3 {
            fixture["requestedSchema"]["properties"][format!("question_{index}_other")]["_meta"] =
                marker.clone();
        }
        let request = elicitation_from_json(fixture.clone());
        assert_eq!(elicitation_field_count(&request), 6);
        let converted = convert_fixture(fixture);
        assert_eq!(converted.request.questions.len(), 6);
        assert_eq!(
            converted.request.questions[0].kind,
            QuestionPromptKind::SingleSelect { allow_other: false }
        );
        assert_eq!(
            converted.request.questions[0].options[2].label,
            "Other (option)"
        );
    }
    for (namespace, flag) in [
        ("codex", "isOtherAnswer"),
        ("_askUserQuestionCustomAnswer", "isCustomAnswer"),
    ] {
        for required in [false, true] {
            let mut fixture = native_elicitation_fixture();
            fixture["requestedSchema"]["properties"]["question_0_other"]["_meta"] =
                json!({namespace: {"questionId": "question_0", flag: true}});
            if !required {
                fixture["requestedSchema"]["required"] = json!([]);
                fixture["requestedSchema"]["properties"]["question_0_other"]["default"] =
                    json!("Custom");
            }
            assert_native_declined(fixture);
        }
    }
}

#[test]
fn codex_titled_select_and_other_companion_preserve_wire_values() {
    let fixture = serde_json::json!({
        "mode": "form",
        "sessionId": "session-1",
        "toolCallId": "tool-1",
        "message": "Choose a scope",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "title": "Scope",
                    "oneOf": [
                        {"const": "narrow_wire", "title": "Focused", "description": "Keep it narrow."},
                        {"const": "broad_wire", "title": "Broad"}
                    ]
                },
                "scope_other": {
                    "type": "string",
                    "title": "Other",
                    "_meta": {"codex": {"questionId": "scope", "isOtherAnswer": true, "isSecret": false}}
                }
            }
        }
    });
    let request = elicitation_from_json(fixture.clone());
    assert_eq!(elicitation_field_count(&request), 1);
    let converted = convert_fixture(fixture);
    assert_eq!(
        converted.request.source_label.as_deref(),
        Some("Fixture ACP")
    );
    assert_eq!(converted.request.questions.len(), 1);
    assert!(matches!(
        converted.request.questions[0].kind,
        QuestionPromptKind::SingleSelect { allow_other: true }
    ));
    let selected_response = QuestionResponse::Answered {
        answers: vec![zevria_foundation::QuestionAnswer {
            id: "scope".to_string(),
            answer: Some(QuestionAnswerValue::String("Focused".to_string())),
        }],
    };
    let selected_decision = converted
        .normalized_decision(&selected_response)
        .expect("selected display decision");
    assert_eq!(
        selected_decision.answers[0].answer,
        AgentUserDecisionValue::String {
            value: "Focused".to_string()
        }
    );
    let selected = converted
        .accepted_content(selected_response)
        .expect("selected content");
    assert_eq!(
        selected.get("scope"),
        Some(&ElicitationContentValue::String("narrow_wire".to_string()))
    );

    let custom_response = QuestionResponse::Answered {
        answers: vec![zevria_foundation::QuestionAnswer {
            id: "scope".to_string(),
            answer: Some(QuestionAnswerValue::String("A custom scope".to_string())),
        }],
    };
    assert_eq!(
        converted
            .normalized_decision(&custom_response)
            .expect("custom display decision")
            .answers[0]
            .answer,
        AgentUserDecisionValue::String {
            value: "A custom scope".to_string()
        }
    );
    let custom = converted
        .accepted_content(custom_response)
        .expect("custom content");
    assert!(!custom.contains_key("scope"));
    assert_eq!(
        custom.get("scope_other"),
        Some(&ElicitationContentValue::String(
            "A custom scope".to_string()
        ))
    );
}

#[test]
fn claude_multi_select_custom_boolean_skip_defaults_and_bounds_convert() {
    let converted = convert_fixture(serde_json::json!({
        "mode": "form",
        "sessionId": "session-1",
        "message": "Answer the form",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "details": {"type": "string", "minLength": 2, "maxLength": 8, "default": "ok"},
                "enabled": {"type": "boolean", "default": true},
                "targets": {
                    "type": "array",
                    "items": {"anyOf": [
                        {"const": "core_wire", "title": "Other"},
                        {"const": "tui_wire", "title": "Other"}
                    ]},
                    "minItems": 1,
                    "maxItems": 3,
                    "default": ["core_wire"]
                },
                "targets_custom": {
                    "type": "string",
                    "_meta": {"_askUserQuestionCustomAnswer": {"questionId": "targets", "isCustomAnswer": true}}
                },
                "optional_note": {"type": "string"}
            },
            "required": ["details", "enabled"]
        }
    }));
    assert_eq!(converted.request.questions.len(), 4);
    let targets = converted
        .request
        .questions
        .iter()
        .find(|question| question.id == "targets")
        .expect("targets prompt");
    assert!(!targets.required);
    assert!(matches!(
        targets.kind,
        QuestionPromptKind::MultiSelect {
            min_selections: Some(1),
            max_selections: Some(3),
            allow_other: true,
        }
    ));
    assert_ne!(targets.options[0].label, "Other");
    assert_ne!(targets.options[0].label, targets.options[1].label);
    assert_eq!(
        targets.default,
        Some(QuestionAnswerValue::Strings(vec![
            targets.options[0].label.clone()
        ]))
    );

    let answers = converted
        .fields
        .iter()
        .map(|field| zevria_foundation::QuestionAnswer {
            id: field.property.clone(),
            answer: match field.property.as_str() {
                "details" => Some(QuestionAnswerValue::String("valid".to_string())),
                "enabled" => Some(QuestionAnswerValue::String("No".to_string())),
                "targets" => Some(QuestionAnswerValue::Strings(vec![
                    targets.options[1].label.clone(),
                    "custom target".to_string(),
                ])),
                "optional_note" => None,
                _ => unreachable!(),
            },
        })
        .collect();
    let response = QuestionResponse::Answered { answers };
    let normalized = converted
        .normalized_decision(&response)
        .expect("exact display response");
    assert!(normalized.answers.iter().any(|answer| {
        answer.question_id == "enabled"
            && answer.answer
                == AgentUserDecisionValue::String {
                    value: "No".to_string(),
                }
    }));
    assert!(normalized.answers.iter().any(|answer| {
        answer.question_id == "targets"
            && answer.answer
                == AgentUserDecisionValue::Strings {
                    values: vec![
                        targets.options[1].label.clone(),
                        "custom target".to_string(),
                    ],
                }
    }));
    assert!(normalized.answers.iter().any(|answer| {
        answer.question_id == "optional_note" && answer.answer == AgentUserDecisionValue::Skipped
    }));
    let content = converted.accepted_content(response).expect("typed content");
    assert_eq!(
        content.get("enabled"),
        Some(&ElicitationContentValue::Boolean(false))
    );
    assert_eq!(
        content.get("targets"),
        Some(&ElicitationContentValue::StringArray(vec![
            "tui_wire".to_string()
        ]))
    );
    assert_eq!(
        content.get("targets_custom"),
        Some(&ElicitationContentValue::String(
            "custom target".to_string()
        ))
    );
    assert!(!content.contains_key("optional_note"));
}

#[test]
fn oversized_normalized_decisions_become_explicit_unavailable_markers() {
    let converted = convert_fixture(serde_json::json!({
        "mode": "form",
        "sessionId": "session-1",
        "message": "Capture exactly",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "details": {"type": "string", "title": "Details"}
            },
            "required": ["details"]
        }
    }));
    let response = QuestionResponse::Answered {
        answers: vec![zevria_foundation::QuestionAnswer {
            id: "details".to_string(),
            answer: Some(QuestionAnswerValue::String(
                "x".repeat(MAX_NORMALIZED_USER_DECISION_BYTES),
            )),
        }],
    };
    let decision = converted
        .normalized_decision(&response)
        .expect("display answer normalizes exactly");
    match bounded_captured_decision(decision, 1) {
        CapturedDecision::Unavailable(unavailable) => {
            assert_eq!(unavailable.field_count, 1);
            assert_eq!(
                unavailable.reason,
                zevria_workflow::AgentUnavailableDecisionReason::NormalizedPayloadTooLarge
            );
            assert_eq!(unavailable.request_id, converted.request.id);
        }
        CapturedDecision::Available(_) => panic!("oversized decision must not be truncated"),
    }
}

#[test]
fn unsupported_and_invalid_elicitation_shapes_decline_or_violate_session() {
    for requested_schema in [
        serde_json::json!({"type": "object", "properties": {}}),
        serde_json::json!({"type": "object", "properties": {"count": {"type": "integer"}}}),
        serde_json::json!({"type": "object", "properties": {"nested": {"type": "object", "properties": {}}}}),
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string", "pattern": "x+"}}}),
        serde_json::json!({"type": "object", "properties": {"password": {"type": "string", "_meta": {"isSecret": true}}}}),
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["missing"]}),
    ] {
        let request = elicitation_from_json(serde_json::json!({
            "mode": "form",
            "sessionId": "session-1",
            "message": "unsupported",
            "requestedSchema": requested_schema
        }));
        assert!(matches!(
            convert_elicitation(
                &request,
                Some("session-1"),
                "Fixture",
                QuestionRequestId::new("id")
            ),
            Err(ElicitationConversionError::Decline { .. })
        ));
    }

    let wrong_session = elicitation_from_json(serde_json::json!({
        "mode": "form",
        "sessionId": "different",
        "message": "wrong",
        "requestedSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
    }));
    assert!(matches!(
        convert_elicitation(
            &wrong_session,
            Some("session-1"),
            "Fixture",
            QuestionRequestId::new("id")
        ),
        Err(ElicitationConversionError::ProtocolViolation { .. })
    ));

    let before_session = elicitation_from_json(serde_json::json!({
        "mode": "form",
        "sessionId": "not-established-yet",
        "message": "early",
        "requestedSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
    }));
    assert!(matches!(
        convert_elicitation(
            &before_session,
            None,
            "Fixture",
            QuestionRequestId::new("id")
        ),
        Err(ElicitationConversionError::Decline { field_count: 1, .. })
    ));

    let required_custom_target = elicitation_from_json(serde_json::json!({
        "mode": "form",
        "sessionId": "session-1",
        "message": "inconsistent custom answer",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["focused"]},
                "scope_other": {
                    "type": "string",
                    "_meta": {"codex": {"questionId": "scope", "isOtherAnswer": true}}
                }
            },
            "required": ["scope"]
        }
    }));
    assert!(matches!(
        convert_elicitation(
            &required_custom_target,
            Some("session-1"),
            "Fixture",
            QuestionRequestId::new("id")
        ),
        Err(ElicitationConversionError::Decline { field_count: 1, .. })
    ));

    for scoped_mode in [
        serde_json::json!({
            "mode": "form",
            "requestId": 7,
            "message": "request scoped",
            "requestedSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }),
        serde_json::json!({
            "mode": "url",
            "sessionId": "session-1",
            "message": "open",
            "elicitationId": "url-1",
            "url": "https://example.invalid"
        }),
    ] {
        let request = elicitation_from_json(scoped_mode);
        assert!(matches!(
            convert_elicitation(
                &request,
                Some("session-1"),
                "Fixture",
                QuestionRequestId::new("id")
            ),
            Err(ElicitationConversionError::Decline { .. })
        ));
    }

    let request_scoped_before_session = elicitation_from_json(serde_json::json!({
        "mode": "form",
        "requestId": 8,
        "message": "request scoped before session startup",
        "requestedSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
    }));
    assert!(matches!(
        convert_elicitation(
            &request_scoped_before_session,
            None,
            "Fixture",
            QuestionRequestId::new("id")
        ),
        Err(ElicitationConversionError::Decline { .. })
    ));
}

#[test]
fn workflow_permission_policies_are_distinct_and_never_allow_always() {
    let options = vec![
        permission(PermissionOptionKind::AllowAlways),
        permission(PermissionOptionKind::RejectAlways),
        permission(PermissionOptionKind::AllowOnce),
        permission(PermissionOptionKind::RejectOnce),
    ];
    let plan = WorkflowConstraintPolicy::for_workflow(EnsembleWorkflow::Plan);
    let review = WorkflowConstraintPolicy::for_workflow(EnsembleWorkflow::Review);

    for kind in [
        Some(ToolKind::Read),
        Some(ToolKind::Execute),
        Some(ToolKind::Edit),
        None,
    ] {
        assert_eq!(
            select_permission_option(review, &options, kind, false).map(|option| option.kind),
            Some(PermissionOptionKind::AllowOnce)
        );
    }
    assert_eq!(
        select_permission_option(plan, &options, Some(ToolKind::Read), false)
            .map(|option| option.kind),
        Some(PermissionOptionKind::AllowOnce)
    );
    for kind in [
        Some(ToolKind::Read),
        Some(ToolKind::Search),
        Some(ToolKind::Fetch),
        Some(ToolKind::Execute),
    ] {
        assert_eq!(
            select_permission_option(plan, &options, kind, false).map(|option| option.kind),
            Some(PermissionOptionKind::AllowOnce)
        );
    }
    for kind in [
        Some(ToolKind::Edit),
        Some(ToolKind::Delete),
        Some(ToolKind::Move),
        None,
    ] {
        assert_eq!(
            select_permission_option(plan, &options, kind, false).map(|option| option.kind),
            Some(PermissionOptionKind::RejectOnce)
        );
    }
    for policy in [plan, review] {
        assert!(select_permission_option(policy, &options, Some(ToolKind::Read), true).is_none());
        assert!(
            select_permission_option(
                policy,
                &[permission(PermissionOptionKind::AllowAlways)],
                Some(ToolKind::Read),
                false,
            )
            .is_none()
        );
    }
    assert!(
        select_permission_option(
            review,
            &[permission(PermissionOptionKind::RejectOnce)],
            Some(ToolKind::Execute),
            false,
        )
        .is_none(),
        "review must cancel instead of converting a missing one-shot grant into a rejection"
    );
}

#[tokio::test]
async fn semantic_early_stops_repair_but_successful_plan_prose_waits_for_feedback() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("semantic_repair_acp.py");
    std::fs::write(
            &script,
            r##"import json
import os
import sys

scenario = os.environ["SCENARIO"]
prompt_count = 0
is_plan = False

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_options():
    return [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

def finish_success(request_id):
    if is_plan:
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"repair-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"final-plan","content":"Implementation plan\n\n- Deliver durable proof."}}}})
    else:
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"repair-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"findings-first review"},"messageId":"review"}}})
    send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        is_plan = message["params"]["clientCapabilities"].get("plan") == {}
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"semantic-repair","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"repair-session","configOptions":mode_options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options()}})
    elif method == "session/prompt":
        prompt_count += 1
        if scenario == "execute_success":
            send({"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"repair-session","toolCall":{"toolCallId":"execute-1","kind":"execute"},"options":[{"optionId":"execute-always","name":"Always","kind":"allow_always"},{"optionId":"execute-once","name":"Once","kind":"allow_once"},{"optionId":"execute-reject","name":"Reject","kind":"reject_once"}]}})
            response = json.loads(sys.stdin.readline())
            sys.stderr.write("permission:" + json.dumps(response, separators=(",", ":")) + "\n")
            sys.stderr.flush()
            finish_success(request_id)
        elif scenario in ["denied_cancel", "denied_then_activity_cancel", "shared_budget"] and prompt_count == 1:
            send({"jsonrpc":"2.0","id":901,"method":"session/request_permission","params":{"sessionId":"repair-session","toolCall":{"toolCallId":"edit-1","kind":"edit"},"options":[{"optionId":"edit-always","name":"Always","kind":"allow_always"},{"optionId":"edit-reject","name":"Reject","kind":"reject_once"}]}})
            response = json.loads(sys.stdin.readline())
            sys.stderr.write("permission:" + json.dumps(response, separators=(",", ":")) + "\n")
            sys.stderr.flush()
            if scenario == "denied_cancel":
                send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"repair-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"edit-1","status":"failed"}}})
            elif scenario == "denied_then_activity_cancel":
                send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"repair-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I continued with an unrelated response."},"messageId":"after-denial"}}})
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"cancelled"}})
        elif scenario == "denied_cancel":
            finish_success(request_id)
        elif scenario == "shared_budget":
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
        elif scenario.startswith("repair_") and prompt_count == 1:
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":scenario[len("repair_"):]}})
        elif scenario.startswith("repair_"):
            finish_success(request_id)
        elif scenario == "oversize":
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"repair-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"oversize-plan","content":"Oversize plan\n\n" + ("x" * 5000)}}}})
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
        elif scenario == "early_twice":
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"refusal"}})
        elif scenario == "missing_twice":
            send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
        else:
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32603,"message":"unknown scenario"}})
"##,
        )
        .expect("semantic repair ACP script");

    let cases = [
        ("execute_success", EnsembleWorkflow::Plan, true, 0usize),
        ("denied_cancel", EnsembleWorkflow::Plan, true, 1),
        (
            "denied_then_activity_cancel",
            EnsembleWorkflow::Plan,
            false,
            0,
        ),
        ("repair_refusal", EnsembleWorkflow::Plan, true, 1),
        ("repair_max_tokens", EnsembleWorkflow::Plan, true, 1),
        ("repair_max_turn_requests", EnsembleWorkflow::Plan, true, 1),
        ("repair_refusal", EnsembleWorkflow::Review, true, 1),
        ("repair_max_tokens", EnsembleWorkflow::Review, true, 1),
        (
            "repair_max_turn_requests",
            EnsembleWorkflow::Review,
            true,
            1,
        ),
        ("early_twice", EnsembleWorkflow::Plan, false, 1),
        ("missing_twice", EnsembleWorkflow::Plan, false, 0),
        ("shared_budget", EnsembleWorkflow::Plan, false, 1),
    ];
    for (index, (scenario, workflow, succeeds, repair_count)) in cases.into_iter().enumerate() {
        let logs = directory.path().join(format!("logs-{index}"));
        let agent = fake_stdio_agent(
            &script,
            BTreeMap::from([("SCENARIO".to_string(), scenario.to_string())]),
        );
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(agent),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .expect("valid supervisor");
        let (_start, outcome, records) =
            launch_fake_worker(&supervisor, &logs, workflow, scenario).await;
        let prompt_requests = protocol_requests(&records)
            .into_iter()
            .filter(|message| message["method"] == "session/prompt")
            .count();
        assert_eq!(prompt_requests, 1 + repair_count, "{scenario} {workflow}");
        let repairs = records
            .iter()
            .filter_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        AgentRunEvent::Prompt {
                            repair: Some(repair),
                            ..
                        },
                } => Some(repair),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repairs.len(), repair_count, "{scenario} {workflow}");
        if succeeds {
            assert_eq!(
                outcome.status,
                if workflow == EnsembleWorkflow::Plan {
                    AgentRunStatus::AwaitingConfirmation
                } else {
                    AgentRunStatus::Completed
                },
                "{scenario}"
            );
            assert!(!outcome.partial);
            if workflow == EnsembleWorkflow::Plan {
                assert!(outcome.has_plan_proof(), "{scenario}");
            }
        } else if matches!(scenario, "missing_twice" | "shared_budget") {
            assert_eq!(outcome.status, AgentRunStatus::AwaitingFeedback);
            assert!(!outcome.partial);
            assert!(outcome.failure.is_none());
        } else {
            assert_eq!(outcome.status, AgentRunStatus::Blocked, "{scenario}");
            assert!(outcome.partial, "{scenario}");
        }
        if workflow == EnsembleWorkflow::Plan {
            assert!(outcome.confirmation.is_none());
            assert!(
                !records
                    .iter()
                    .any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
            );
        }
        if scenario == "execute_success" {
            assert!(records.iter().any(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Permission {
                        decision,
                        option_id: Some(option_id),
                        ..
                    }
                } if decision == "allow_once" && option_id == "execute-once"
            )));
        }
        if matches!(scenario, "denied_cancel" | "shared_budget") {
            assert!(matches!(
                repairs.as_slice(),
                [AgentRunRepair::HostDeniedPermission {
                    tool_kind: Some(kind),
                    option_id: Some(option_id),
                }] if kind == "edit" && option_id == "edit-reject"
            ));
        }
        if scenario == "early_twice" {
            assert!(
                outcome
                    .failure
                    .as_deref()
                    .is_some_and(|failure| failure.contains("\"refusal\""))
            );
        }
        if scenario == "denied_then_activity_cancel" {
            assert!(
                outcome
                    .failure
                    .as_deref()
                    .is_some_and(|failure| { failure.contains("cancelled") })
            );
        }
        if matches!(scenario, "missing_twice" | "shared_budget") {
            assert!(!outcome.has_plan_proof());
            assert!(
                outcome.failure.is_none(),
                "successful prose is not an error or repair trigger"
            );
        }
    }

    let oversize_logs = directory.path().join("logs-oversize");
    let mut oversize_config = single_agent_config(fake_stdio_agent(
        &script,
        BTreeMap::from([("SCENARIO".to_string(), "oversize".to_string())]),
    ));
    oversize_config.max_synthesis_bytes_per_agent = 256;
    let supervisor = EnsembleSupervisor::new(
        oversize_config,
        &workspace,
        oversize_logs.clone(),
        test_questions(),
    )
    .expect("oversize supervisor");
    let (_start, outcome, _records) = launch_fake_worker(
        &supervisor,
        &oversize_logs,
        EnsembleWorkflow::Plan,
        "oversize",
    )
    .await;
    assert_eq!(outcome.status, AgentRunStatus::AwaitingConfirmation);
    assert!(!outcome.partial);
    let error = validate_worker_synthesis_payload(EnsembleWorkflow::Plan, &outcome, 256)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("mandatory final-plan payload requires")
            && error.contains("max_synthesis_bytes_per_agent (256)")
    );
    let plan = outcome.plan.expect("full durable plan evidence remains");
    assert!(
        plan.markdown
            .as_deref()
            .is_some_and(|markdown| markdown.len() > 5_000)
    );
    assert!(plan.entries.is_empty(), "no fabricated truncated checklist");
}

#[test]
fn ordinary_plan_workers_keep_generic_mutation_reports_as_evidence() {
    let edit: AcpSessionUpdate = serde_json::from_value(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "edit-1",
        "title": "Inspect generated output",
        "kind": "edit",
        "status": "pending",
        "content": [],
        "locations": [],
        "rawInput": {}
    }))
    .expect("valid ACP edit update");

    assert!(matches!(
        normalize_update(edit).as_slice(),
        [AgentRunEvent::ToolCall { kind, .. }] if kind == "edit"
    ));
}

#[test]
fn only_nonempty_inline_markdown_survives_as_plan_completion_proof() {
    let exact = " \r\n# Exact ACP plan\n\n- Keep trailing spaces.  \n";
    let markdown = acp_update(serde_json::json!({
        "sessionUpdate": "plan_update",
        "plan": {
            "type": "markdown",
            "planId": "inline",
            "content": exact
        }
    }));
    let file = acp_update(serde_json::json!({
        "sessionUpdate": "plan_update",
        "plan": {
            "type": "file",
            "planId": "file-backed",
            "uri": "file:///tmp/never-read.md"
        }
    }));
    let checklist = acp_update(serde_json::json!({
        "sessionUpdate": "plan",
        "entries": [{
            "content": "Inspect",
            "priority": "high",
            "status": "pending"
        }]
    }));
    let empty = acp_update(serde_json::json!({
        "sessionUpdate": "plan_update",
        "plan": {
            "type": "markdown",
            "planId": "empty",
            "content": "  \n"
        }
    }));

    let mut state = WorkerEvidenceState::default();
    for event in normalize_update(checklist) {
        state.apply(&event);
    }
    assert!(!state.has_plan_proof());
    for event in normalize_update(file) {
        state.apply(&event);
    }
    assert!(!state.has_plan_proof());
    for event in normalize_update(empty) {
        state.apply(&event);
    }
    assert!(!state.has_plan_proof());
    state.apply(&AgentRunEvent::AgentMessage {
        text: "ordinary final prose".to_string(),
        message_id: Some("final".to_string()),
    });
    assert!(!state.has_plan_proof());
    for event in normalize_update(markdown) {
        state.apply(&event);
    }
    assert!(state.has_plan_proof());
    assert_eq!(
        state
            .plan
            .as_ref()
            .and_then(|plan| plan.markdown.as_deref()),
        Some(exact)
    );
    state.apply(&AgentRunEvent::PlanRemoved {
        plan_id: "other".to_string(),
    });
    assert!(state.has_plan_proof());
    state.apply(&AgentRunEvent::PlanRemoved {
        plan_id: "inline".to_string(),
    });
    assert!(!state.has_plan_proof());
}

#[test]
fn claude_review_metadata_is_attached_to_new_and_recovery_requests() {
    let workspace = Path::new("/tmp/zevria-review-workspace");
    let transport = Some(ReviewSystemPromptTransport::ClaudeCodeAppend);
    let meta = worker_session_meta(transport, EnsembleWorkflow::Review)
        .expect("configured review system-prompt metadata");
    let session_id = SessionId::new("review-session");
    let requests = [
        serde_json::to_value(new_session_request(workspace, &Some(meta.clone())))
            .expect("new session request"),
        serde_json::to_value(resume_session_request(
            session_id.clone(),
            workspace,
            &Some(meta.clone()),
        ))
        .expect("resume session request"),
        serde_json::to_value(load_session_request(session_id, workspace, &Some(meta)))
            .expect("load session request"),
    ];
    for request in requests {
        assert_eq!(
            request["_meta"]["systemPrompt"]["append"],
            REVIEW_WORKER_INSTRUCTION
        );
    }
    assert!(worker_session_meta(transport, EnsembleWorkflow::Plan).is_none());
    assert!(worker_session_meta(None, EnsembleWorkflow::Review).is_none());
}

#[test]
fn unexpected_process_termination_classifier_is_structural_and_narrow() {
    let incoming_close = AcpError::internal_error().data(serde_json::json!({
        "reason": "incoming_transport_closed",
        "method": "session/prompt"
    }));
    let process_exit = AcpError::internal_error().data("Process exited with exit status: 17");
    let wrapped_process_exit = AcpError::internal_error().data(serde_json::json!({
        "spawned_at": "src/jsonrpc.rs:1:1",
        "data": "Process exited with exit status: 18"
    }));
    let wrapped_incoming_close = AcpError::internal_error().data(serde_json::json!({
        "spawned_at": "src/jsonrpc.rs:1:1",
        "data": {
            "reason": "incoming_transport_closed",
            "method": "session/prompt"
        }
    }));
    assert!(is_unexpected_process_termination(&incoming_close));
    assert!(is_unexpected_process_termination(&process_exit));
    assert!(is_unexpected_process_termination(&wrapped_process_exit));
    assert!(is_unexpected_process_termination(&wrapped_incoming_close));

    let rejected = [
        AcpError::new(-32000, "Authentication required"),
        AcpError::internal_error().data("ordinary internal failure"),
        AcpError::invalid_request().data("protocol failure"),
        AcpError::request_cancelled().data("worker turn timed out"),
        acp_error("ensemble turn cancelled by the user"),
        acp_error("agent reported a mutating tool operation"),
        AcpError::internal_error()
            .data(serde_json::json!({ "message": "Process exited with exit status: 2" })),
        AcpError::new(-32603, "Process exited with exit status: 2"),
    ];
    for error in rejected {
        assert!(
            !is_unexpected_process_termination(&error),
            "unexpectedly classified {error:?} as a process termination"
        );
    }
}

#[test]
fn live_retry_gate_requires_durable_in_progress_continuation_work() {
    fn crashed() -> WorkerAttemptResult {
        WorkerAttemptResult {
            connection_result: Some(Err(
                AcpError::internal_error().data("Process exited with exit status: 9")
            )),
            startup_timed_out: false,
            cancelled_during_startup: false,
            durable_session_id: Some("session".to_string()),
            prompt_dispatched: true,
            semantic_end: None,
            violation: None,
        }
    }

    assert!(crashed().recoverable_process_error().is_some());

    let mut before_session = crashed();
    before_session.durable_session_id = None;
    assert!(before_session.recoverable_process_error().is_none());

    let mut before_prompt = crashed();
    before_prompt.prompt_dispatched = false;
    assert!(before_prompt.recoverable_process_error().is_none());

    let mut startup_timeout = crashed();
    startup_timeout.startup_timed_out = true;
    assert!(startup_timeout.recoverable_process_error().is_none());

    let mut cancelled = crashed();
    cancelled.cancelled_during_startup = true;
    assert!(cancelled.recoverable_process_error().is_none());

    let mut policy_failure = crashed();
    policy_failure.violation = Some("safe-mode violation".to_string());
    assert!(policy_failure.recoverable_process_error().is_none());

    for end in [
        WorkerEnd::PromptResponse {
            stop_reason: "end_turn".to_string(),
        },
        WorkerEnd::TimedOut,
        WorkerEnd::LocalCancelled,
    ] {
        let mut ended = crashed();
        ended.semantic_end = Some(end);
        assert!(ended.recoverable_process_error().is_none());
    }

    for error in [
        AcpError::new(-32000, "Authentication required"),
        AcpError::internal_error().data("ordinary ACP response error"),
    ] {
        let mut ordinary_error = crashed();
        ordinary_error.connection_result = Some(Err(error));
        assert!(ordinary_error.recoverable_process_error().is_none());
    }
}

#[test]
fn attempt_prompt_selection_keeps_restart_and_live_recovery_distinct() {
    for workflow in [EnsembleWorkflow::Plan, EnsembleWorkflow::Review] {
        let base = worker_prompt(workflow, &"inspect the workspace".into(), false);
        assert_eq!(
            worker_attempt_prompt(&WorkerAttemptMode::Fresh, workflow, &base, None),
            WorkerPrompt {
                text: base.clone(),
                continuation: false,
                repair: None,
            }
        );
        assert_eq!(
            worker_attempt_prompt(
                &WorkerAttemptMode::DurableRecovery {
                    session_id: Some("durable-session".to_string()),
                    repair: None,
                },
                workflow,
                &base,
                None,
            ),
            WorkerPrompt {
                text: continuation_prompt(workflow, &base),
                continuation: true,
                repair: None,
            }
        );
        let live = worker_attempt_prompt(
            &WorkerAttemptMode::LiveProcessRecovery {
                session_id: "live-session".to_string(),
            },
            workflow,
            &base,
            None,
        );
        assert_eq!(live.text, "continue".into());
        assert_eq!(live.text.text_len(), 8);
        assert!(live.continuation);
    }
}

#[test]
fn every_review_prompt_contains_the_source_read_only_fallback_envelope() {
    let request = "review the changes\nfocus on recovery";
    let fallback =
        worker_prompt(EnsembleWorkflow::Review, &request.into(), false).text_projection();
    assert!(fallback.contains(REVIEW_WORKER_INSTRUCTION));
    assert!(fallback.contains(request));
    assert!(!fallback.contains("invoke mutating tools"));
}

#[test]
fn all_worker_envelopes_defer_to_active_policy_without_blanket_scratch_denial() {
    for (workflow, handoff) in [
        (EnsembleWorkflow::Plan, false),
        (EnsembleWorkflow::Plan, true),
        (EnsembleWorkflow::Review, false),
    ] {
        let prompt =
            worker_prompt(workflow, &"inspect diagnostics".into(), handoff).text_projection();
        for pin in [
            "Protect non-scratch files",
            "external reads, downloads, and private OS-temp scratch-contained execution",
            "only if your own active policy permits them",
            "no additional capabilities or ACP mutation permissions",
            "Your own policies and sandboxes remain authoritative",
        ] {
            assert!(prompt.contains(pin), "missing envelope pin: {pin}");
        }
        assert!(!prompt.contains("Do not modify files,"));
        assert!(!prompt.contains("only permitted filesystem mutations"));
        assert_eq!(
            continuation_prompt(workflow, &prompt.into()),
            "continue".into()
        );
    }
}

#[test]
fn planning_prompt_defers_scratch_permission_and_preserves_configured_native_handoff() {
    let configured =
        worker_prompt(EnsembleWorkflow::Plan, &"plan it".into(), true).text_projection();
    assert!(configured.contains("Inspect the workspace first"));
    assert!(
        configured.contains(
            "native structured-question mechanism for every user-visible behavioral fork"
        )
    );
    assert!(configured.contains("before drafting any plan or report"));
    assert!(configured.contains("batch them into a single request"));
    assert!(configured.contains("before creating or modifying the first artifact"));
    assert!(configured.contains("configured Claude plans directory"));
    assert!(configured.contains("workspace-local .claude/plans directory"));
    assert!(configured.contains("sequentially create or revise multiple"));
    assert!(configured.contains("Write, Edit, or MultiEdit"));
    assert!(configured.contains("only separately configured native handoff mutation exception"));
    assert!(configured.contains("final nonempty implementation-ready plan through ExitPlanMode"));
    assert!(configured.contains("Do not modify other workspace files"));
    assert!(configured.contains("or enter implementation mode"));
    assert!(!configured.contains("exactly one generated Markdown"));
    assert!(!configured.contains("that single artifact"));

    let ordinary =
        worker_prompt(EnsembleWorkflow::Plan, &"plan it".into(), false).text_projection();
    assert!(ordinary.contains("Inspect the workspace first"));
    assert!(
        ordinary.contains(
            "native structured-question mechanism for every user-visible behavioral fork"
        )
    );
    assert!(ordinary.contains("are not evidence of user preference"));
    assert!(ordinary.contains("by recording it as an assumption"));
    assert!(ordinary.contains("as soon as inspection reveals them"));
    assert!(ordinary.contains("batch them into a single request"));
    assert!(ordinary.contains("Do not ask factual questions"));
    assert!(
        ordinary
            .contains("complete implementation-ready plan as one nonempty inline Markdown update")
    );
    assert!(ordinary.contains("Do not modify non-scratch files"));
    assert!(ordinary.contains("write canonical plan artifacts through the shell"));
    assert!(ordinary.contains("Native Zevria workers publish through submit_plan"));
    assert!(ordinary.contains("not canonical Plan publications"));
    assert!(ordinary.contains("cannot establish confirmation or implementation authorization"));
    assert!(ordinary.contains("or begin implementation"));
    assert!(!ordinary.contains("only permitted filesystem mutations"));

    for prompt in [&ordinary, &configured] {
        assert!(prompt.contains("temporary-looking path does not grant a mutation permission"));
        assert!(prompt.contains("Other mutations remain prohibited"));
        let recovered = continuation_prompt(EnsembleWorkflow::Plan, &prompt.clone().into());
        // Loaded ACP context retains the original constraints and images.
        assert_eq!(recovered, "continue".into());
    }
}

#[test]
fn safe_mode_drift_requires_the_selected_option_to_remain_verifiable() {
    let configured = Some(SessionConfigurationExpectation {
        safe_mode: SafeModeExpectation {
            desired: "read-only".to_string(),
            config_id: Some("execution-mode".to_string()),
        },
        workflow: EnsembleWorkflow::Plan,
        workflow_options: BTreeMap::new(),
    });
    assert!(
        session_configuration_violation(
            &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(Vec::new())),
            &configured,
        )
        .is_some()
    );
    assert!(
        session_configuration_violation(
            &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![mode_option(
                "read-only",
            )])),
            &configured,
        )
        .is_none()
    );

    let legacy = Some(SessionConfigurationExpectation {
        safe_mode: SafeModeExpectation {
            desired: "read-only".to_string(),
            config_id: None,
        },
        workflow: EnsembleWorkflow::Review,
        workflow_options: BTreeMap::new(),
    });
    assert!(
        session_configuration_violation(
            &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![mode_option(
                "build",
            )])),
            &legacy,
        )
        .is_some()
    );
    assert!(
        session_configuration_violation(
            &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![mode_option(
                "read-only",
            )])),
            &legacy,
        )
        .is_none()
    );
}

#[test]
fn workflow_config_option_drift_detects_reverted_and_omitted_values() {
    let expectation = Some(SessionConfigurationExpectation {
        safe_mode: SafeModeExpectation {
            desired: "read-only".to_string(),
            config_id: Some("execution-mode".to_string()),
        },
        workflow: EnsembleWorkflow::Plan,
        workflow_options: BTreeMap::from([("collaboration_mode".to_string(), "plan".to_string())]),
    });
    let reverted = session_configuration_violation(
        &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![
            mode_option("read-only"),
            collaboration_option("default"),
        ])),
        &expectation,
    )
    .expect("reverted workflow option must fail");
    assert!(reverted.contains("workflow-configuration violation"));
    assert!(reverted.contains("collaboration_mode"));

    let omitted = session_configuration_violation(
        &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![mode_option(
            "read-only",
        )])),
        &expectation,
    )
    .expect("omitted workflow option must fail");
    assert!(omitted.contains("workflow-configuration violation"));
    assert!(omitted.contains("stopped reporting"));

    assert!(
        session_configuration_violation(
            &AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![
                mode_option("read-only"),
                collaboration_option("plan"),
            ])),
            &expectation,
        )
        .is_none()
    );
}

#[test]
fn cancelling_one_worker_scope_does_not_cancel_its_siblings_or_root() {
    let root = CancellationToken::new();
    let first = root.child_token();
    let second = root.child_token();
    first.cancel();
    assert!(first.is_cancelled());
    assert!(!second.is_cancelled());
    assert!(!root.is_cancelled());
    root.cancel();
    assert!(second.is_cancelled());
}

#[test]
fn native_runtime_fatal_error_data_survives_ensemble_failure_conversion() {
    let diagnostic = "session engine failed: invalid skill replay: original worker diagnostic";
    let rendered = error_with_login_hint(
        &AcpError::internal_error().data(diagnostic),
        "unused login hint",
    );
    assert!(rendered.contains(diagnostic));
    assert_eq!(rendered.matches(diagnostic).count(), 1);
    assert!(!rendered.contains("unused login hint"));
}

#[test]
fn authentication_errors_include_only_the_configured_login_guidance() {
    let rendered = error_with_login_hint(
        &AcpError::auth_required(),
        "Authenticate this agent in a terminal first.",
    );
    assert!(rendered.contains("Authentication required"));
    assert!(rendered.contains("Authenticate this agent"));
}

#[tokio::test]
async fn workflows_apply_distinct_modes_permissions_and_mutation_policies() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let claude_config = directory.path().join("claude-config");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(claude_config.join("plans")).expect("Claude plans directory");
    let script = directory.path().join("workflow_mode_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

selected_mode = "plan"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_options(current):
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":current,"options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"workflow-mode-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"workflow-mode-session","configOptions":mode_options(selected_mode)}})
    elif method == "session/set_config_option":
        selected_mode = message["params"]["value"]
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options(selected_mode)}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"workflow-mode-session","toolCall":{"toolCallId":"execute-1","kind":"execute"},"options":[{"optionId":"reject","name":"Reject","kind":"reject_once"},{"optionId":"once","name":"Once","kind":"allow_once"},{"optionId":"always","name":"Always","kind":"allow_always"}]}})
        permission_response = json.loads(sys.stdin.readline())
        sys.stderr.write("execute-permission-response:" + json.dumps(permission_response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        if selected_mode == "plan":
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-mode-session","update":{"sessionUpdate":"tool_call","toolCallId":"write-1","title":"Preparing file...","kind":"edit","status":"pending","content":[],"locations":[],"rawInput":{}}}})
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-mode-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"workflow-plan","content":"Workflow plan\n\n- Preserve generic mutation evidence."}}}})
        else:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-mode-session","update":{"sessionUpdate":"tool_call","toolCallId":"edit-1","title":"Agent-reported edit during review","kind":"edit","status":"pending","content":[],"locations":[],"rawInput":{}}}})
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-mode-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"review returned directly"},"messageId":"message-1"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("fake workflow-mode ACP script");

    let agent = EnsembleAgentConfig {
        label: "Workflow Mode ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("plan".to_string()),
        review_mode: Some("default".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::from([(
            CLAUDE_CONFIG_DIR_ENV.to_string(),
            claude_config.display().to_string(),
        )]),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        named_single_agent_config("claude", agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    assert_eq!(
        supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("plan descriptor")[0]
            .safe_mode,
        "plan"
    );
    let agents = supervisor
        .workers(EnsembleWorkflow::Review)
        .expect("review descriptor");
    assert_eq!(agents[0].safe_mode, "default");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review the changes\nwithout writing files".into(),
        agents,
    };
    let (events, _receiver) = session_event_channel(128);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("review launch succeeds");
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    assert_eq!(outcomes[0].report, "review returned directly");

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("durable worker transcript");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Prompt {
                text,
                continuation: false,
                ..
            }
        } if text.contains(REVIEW_WORKER_INSTRUCTION)
            && text.contains("review the changes\nwithout writing files")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Permission {
                tool_kind,
                decision,
                ..
            }
        } if tool_kind.as_deref() == Some("execute") && decision == "allow_once"
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("execute-permission-response")
            && text.contains("once")
            && !text.contains("always\"")
    )));
    let client_requests = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event:
                    AgentRunEvent::Protocol {
                        direction: AgentProtocolDirection::ClientToAgent,
                        json,
                    },
            } => serde_json::from_str::<serde_json::Value>(json).ok(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let new_session = client_requests
        .iter()
        .find(|request| request["method"] == "session/new")
        .expect("new-session request");
    assert!(new_session["params"].get("_meta").is_none());
    let prompt_request = client_requests
        .iter()
        .find(|request| request["method"] == "session/prompt")
        .expect("prompt request");
    assert_eq!(
        prompt_request["params"]["prompt"][0]["text"],
        worker_prompt(EnsembleWorkflow::Review, &start.prompt, false).text_projection()
    );
    let selected_mode = client_requests
        .iter()
        .find(|request| request["method"] == "session/set_config_option")
        .expect("review mode selection");
    assert_eq!(selected_mode["params"]["value"], "default");

    let plan_start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan the changes".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("plan descriptor"),
    };
    let (events, _receiver) = session_event_channel(128);
    let plan_outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: plan_start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(2), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("plan launch returns its worker failure");
    assert_eq!(
        plan_outcomes[0].status,
        AgentRunStatus::AwaitingConfirmation
    );
    assert!(plan_outcomes[0].confirmation.is_none());
    assert!(!plan_outcomes[0].partial);
    assert!(plan_outcomes[0].failure.is_none());
    assert!(plan_outcomes[0].has_plan_proof());
    let plan_path = agent_run_path(&logs, &plan_start.run_id, &plan_outcomes[0].descriptor.id);
    let plan_records = load_agent_run(&plan_path).expect("durable planning transcript");
    assert!(plan_records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Permission {
                tool_kind,
                decision,
                ..
            }
        } if tool_kind.as_deref() == Some("execute") && decision == "allow_once"
    )));
    assert!(plan_records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::ToolCall { kind, .. }
        } if kind == "edit"
    )));
}

#[tokio::test]
async fn fake_claude_native_plan_handoff_queues_overlapping_artifact_permissions() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let config_directory = directory.path().join("claude-config");
    let plans = config_directory.join("plans");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(&plans).expect("Claude plans directory");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let source = workspace.join("source.txt");
    std::fs::write(&source, "workspace stays unchanged\n").expect("workspace fixture");
    let workspace_plans = workspace.join(".claude").join("plans");
    let draft_path = workspace_plans.join("native-draft.md");
    let final_path = plans.join("native-final.md");
    assert!(!workspace_plans.exists());
    let script = directory.path().join("claude_plan_handoff_acp.py");
    std::fs::write(
            &script,
            python_fixture_script(r##"import json
import os
import sys

reader = JsonLineReader()

draft_path = os.environ["DRAFT_PATH"]
final_path = os.environ["FINAL_PATH"]
draft = "# Native draft\n\n- Initial analysis."
edited_draft = "# Native draft\n\n- Initial analysis.\n- Questions resolved."
revised_draft = "# Native draft\n\n- Initial analysis.\n- Questions resolved.\n- Validation added."
plan = "# Native plan\n\n- Preserve the workspace.\n- Implement the requested change."
selected_mode = "plan"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"plan","name":"Plan"},{"value":"default","name":"Default"}]}]

def request_artifact_permission(request_id, tool_call_id, tool_name, path, label):
    send({"jsonrpc":"2.0","id":request_id,"method":"session/request_permission","params":{"sessionId":"claude-plan-session","toolCall":{"toolCallId":tool_call_id,"kind":"edit","rawInput":{"file_path":path},"locations":[{"path":path}],"_meta":{"claudeCode":{"toolName":tool_name}}},"options":[{"optionId":"always","name":"Always","kind":"allow_always"},{"optionId":label + "-once","name":"Allow Once","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}})

def update(value):
    send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-plan-session","update":value}})

def terminal(tool_id, operation):
    update({"sessionUpdate":"tool_call_update","toolCallId":tool_id,"status":"completed","_meta":{"claudeCode":{"toolName":operation}}})

while True:
    message = reader.receive(3)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"claude-handoff-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-plan-session","update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"claude-plan-session","configOptions":mode_options()}})
    elif method == "session/set_config_option":
        selected_mode = message["params"]["value"]
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options()}})
    elif method == "session/prompt":
        calls = [
            (900, "write-draft", "Write", draft_path, "draft-write", draft),
            (901, "edit-draft", "Edit", draft_path, "draft-edit", edited_draft),
            (902, "multi-edit-draft", "MultiEdit", draft_path, "draft-multi-edit", revised_draft),
            (903, "write-final", "Write", final_path, "final-write", plan),
        ]
        # Both pathless Writes precede every permission; refinement and request
        # ordering need not follow announcement order.
        for index in [0, 3, 2, 1]:
            _, tool_id, operation, _, _, _ = calls[index]
            update({"sessionUpdate":"tool_call","toolCallId":tool_id,"title":"Preparing file…","kind":"edit","status":"pending","content":[],"locations":[],"rawInput":{},"_meta":{"claudeCode":{"toolName":operation}}})
        for permission_id, tool_id, operation, path, label, _ in calls:
            request_artifact_permission(permission_id, tool_id, operation, path, label)
        # Early announcement is legal; capture permission will follow terminals.
        update({"sessionUpdate":"tool_call","toolCallId":"exit-plan","title":"Ready to code?","kind":"switch_mode","status":"pending","rawInput":{},"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}})
        for index, (permission_id, tool_id, operation, path, label, text) in enumerate(calls):
            response = reader.receive(2)
            assert response["id"] == permission_id, response
            assert response["result"]["outcome"] == {"outcome":"selected", "optionId":label + "-once"}, response
            sys.stderr.write(label + "-permission-response:" + json.dumps(response, separators=(",", ":")) + "\n")
            sys.stderr.flush()
            update({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"dispatcher progressed while permissions queued"}})
            update({"sessionUpdate":"tool_call_update","toolCallId":tool_id,"status":"in_progress","_meta":{"claudeCode":{"toolName":operation}}})
            if index > 0:
                terminal("write-draft", "Write") # replay cannot release a later owner
            reader.assert_no_response(0.05, "another grant before terminal")
            with open(path, "w", encoding="utf-8", newline="\n") as artifact:
                artifact.write(text)
            terminal(tool_id, operation)
        if selected_mode != "plan":
            sys.exit(94)
        send({"jsonrpc":"2.0","id":904,"method":"session/request_permission","params":{"sessionId":"claude-plan-session","toolCall":{"toolCallId":"exit-plan","kind":"switch_mode","rawInput":{"plan":plan,"planFilePath":final_path},"content":[{"type":"content","content":{"type":"text","text":plan}}]},"options":[{"optionId":"exit-plan-default","name":"Yes, manually approve edits","kind":"allow_once"},{"optionId":"exit-plan-clear-auto","name":"Yes, clear context and use auto mode","kind":"allow_always"},{"optionId":"exit-plan-auto","name":"Yes, and use auto mode","kind":"allow_always"},{"optionId":"reject","name":"No, keep planning","kind":"reject_once"}]}})
        exit_response = reader.receive(3)
        assert exit_response["id"] == 904, exit_response
        assert exit_response["result"]["outcome"] == {"outcome":"selected", "optionId":"reject"}, exit_response
        sys.stderr.write("exit-mode:" + selected_mode + "\n")
        sys.stderr.write("exit-permission-response:" + json.dumps(exit_response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-plan-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"exit-plan","status":"failed","rawOutput":"User rejected request to exit plan mode.","content":[{"type":"content","content":{"type":"text","text":"```\nUser rejected request to exit plan mode.\n```"}}],"_meta":{"claudeCode":{"toolName":"ExitPlanMode","nonExecutionKind":"permission-rule"}}}}})
        while True:
            cancellation = reader.receive(3)
            if cancellation.get("method") == "session/cancel":
                send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"cancelled"}})
                break
"##),
        )
        .expect("fake Claude handoff ACP script");

    let agent = EnsembleAgentConfig {
        label: "Claude Handoff ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("plan".to_string()),
        review_mode: Some("default".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: Some(PlanHandoffTransport::ClaudeCodeExitPlanMode),
        review_system_prompt_transport: None,
        env: BTreeMap::from([
            (
                CLAUDE_CONFIG_DIR_ENV.to_string(),
                config_directory.display().to_string(),
            ),
            ("DRAFT_PATH".to_string(), draft_path.display().to_string()),
            ("FINAL_PATH".to_string(), final_path.display().to_string()),
        ]),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        named_single_agent_config("claude", agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "plan the implementation".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("planning worker"),
    };
    let (events, _receiver) = session_event_channel(256);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(8), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("native handoff launch succeeds");

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("native handoff transcript");
    let expected_draft =
        "# Native draft\n\n- Initial analysis.\n- Questions resolved.\n- Validation added.";
    let expected_plan =
        "# Native plan\n\n- Preserve the workspace.\n- Implement the requested change.";
    assert_eq!(
        outcomes[0].status,
        AgentRunStatus::AwaitingConfirmation,
        "overlapping artifact permissions: {}",
        fixture_diagnostics(&outcomes[0], &records)
    );
    assert!(outcomes[0].confirmation.is_none());
    assert!(!outcomes[0].partial);
    assert_eq!(outcomes[0].failure, None);
    assert!(outcomes[0].report.is_empty());
    assert_eq!(
        outcomes[0]
            .plan
            .as_ref()
            .and_then(|plan| plan.markdown.as_deref()),
        Some(expected_plan)
    );
    assert_eq!(
        std::fs::read_to_string(&draft_path).expect("workspace draft artifact"),
        expected_draft
    );
    assert_eq!(
        std::fs::read_to_string(&final_path).expect("configured final artifact"),
        expected_plan
    );
    assert!(!workspace_plans.join(".gitignore").exists());
    assert_eq!(
        std::fs::read_to_string(&source).expect("workspace fixture remains"),
        "workspace stays unchanged\n"
    );

    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Prompt { text, .. }
        } if text.contains("workspace-local .claude/plans directory")
            && text.contains("native structured-question mechanism")
            && text.contains("sequentially create or revise multiple")
            && text.contains("only separately configured native handoff mutation exception")
            && text.contains("only if your own active policy permits them")
            && text.contains("no additional capabilities or ACP mutation permissions")
    )));
    let durable_plans = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::NativePlanCaptured { plan, .. },
            } if plan.plan_id.as_deref() == Some(CLAUDE_PLAN_HANDOFF_PLAN_ID) => {
                plan.markdown.as_deref()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(durable_plans, [expected_plan]);
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::ToolCallUpdate {
                status: Some(status),
                content: Some(content),
                ..
            }
        } if status == "failed"
            && content.iter().any(|text| text.contains("User rejected request to exit plan mode."))
    )));
    assert!(!records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Failure { .. }
        }
    )));
    assert!(!records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::AgentMessage { message_id, .. }
        } if message_id.as_deref() == Some(CLAUDE_PLAN_HANDOFF_PLAN_ID)
    )));
    let option_ids = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Permission { option_id, .. },
            } => option_id.as_deref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        option_ids,
        [
            "draft-write-once",
            "draft-edit-once",
            "draft-multi-edit-once",
            "final-write-once",
            "reject"
        ]
    );
    let decisions = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Permission { decision, .. },
            } => Some(decision.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        [
            "allow_once",
            "allow_once",
            "allow_once",
            "allow_once",
            "reject_once"
        ]
    );
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Permission {
                decision,
                option_id: Some(option_id),
                ..
            }
        } if decision == "reject_once" && option_id == "reject"
    )));
    for label in [
        "draft-write-permission-response",
        "draft-edit-permission-response",
        "draft-multi-edit-permission-response",
        "final-write-permission-response",
    ] {
        assert!(records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Stderr { text }
            } if text.contains(label)
                && text.contains("-once")
                && !text.contains("always\"")
        )));
    }
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("exit-permission-response")
            && text.contains("\"optionId\":\"reject\"")
            && !text.contains("exit-plan-default")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("exit-mode:plan")
    )));
}

#[tokio::test]
async fn fake_claude_queued_permissions_handle_cancellation_violations_and_recovery() {
    for scenario in [
        "peer",
        "terminal-queued",
        "root",
        "timeout",
        "mode",
        "path",
        "session",
        "duplicate",
        "missing-option",
        "missing-target",
        "unknown",
        "early-exit",
        "connection-loss",
    ] {
        if scenario == "path" && !cfg!(unix) {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let (workspace, plans, _handoff) = claude_handoff_fixture(&directory);
        let script = directory.path().join("queued_permissions.py");
        std::fs::write(&script, python_fixture_script(r##"import json
import os
import sys
import time

reader = JsonLineReader()
scenario = os.environ["SCENARIO"]
owner_path = os.environ["OWNER_PATH"]
queued_path = os.environ["QUEUED_PATH"]
third_path = os.environ["THIRD_PATH"]
attempt_file = os.environ["ATTEMPT_FILE"]
attempt = int(open(attempt_file).read()) if os.path.exists(attempt_file) else 0
open(attempt_file, "w").write(str(attempt + 1))

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def update(value):
    send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session","update":value}})

def announce(tool_id):
    update({"sessionUpdate":"tool_call","toolCallId":tool_id,"title":"Preparing file…","kind":"edit","status":"pending","rawInput":{},"_meta":{"claudeCode":{"toolName":"Write"}}})

def terminal(tool_id, status="completed"):
    update({"sessionUpdate":"tool_call_update","toolCallId":tool_id,"status":status,"_meta":{"claudeCode":{"toolName":"Write"}}})

def permission(request_id, tool_id, path):
    params = {"sessionId":"session","toolCall":{"toolCallId":tool_id,"kind":"edit","rawInput":{"file_path":path},"_meta":{"claudeCode":{"toolName":"Write"}}},"options":[{"optionId":tool_id + "-once","name":"Once","kind":"allow_once"}]}
    if request_id == 901:
        if scenario == "session": params["sessionId"] = "other-session"
        if scenario == "missing-option": params["options"] = []
        if scenario == "missing-target": params["toolCall"]["rawInput"] = {}
        if scenario == "unknown": params["toolCall"]["toolCallId"] = "unknown"
    send({"jsonrpc":"2.0","id":request_id,"method":"session/request_permission","params":params})

def selected(request_id, tool_id):
    response = reader.receive(3)
    assert response["id"] == request_id, response
    assert response["result"]["outcome"] == {"outcome":"selected","optionId":tool_id + "-once"}, response

def finish(prompt_id):
    update({"sessionUpdate":"tool_call","toolCallId":"exit","title":"Ready?","kind":"switch_mode","status":"pending","rawInput":{"plan":"# Queued plan"},"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}})
    send({"jsonrpc":"2.0","id":950,"method":"session/request_permission","params":{"sessionId":"session","toolCall":{"toolCallId":"exit","kind":"switch_mode","rawInput":{"plan":"# Queued plan"}},"options":[{"optionId":"stay","name":"Keep planning","kind":"reject_once"}]}})
    response = reader.receive(3)
    assert response["id"] == 950, response
    assert response["result"]["outcome"] == {"outcome":"selected", "optionId":"stay"}, response
    wait_cancel(prompt_id)

def wait_cancel(prompt_id):
    while True:
        response = reader.receive(3)
        if response.get("method") == "session/cancel":
            # Even a terminal racing with timeout/root cancellation cannot
            # authorize the queued write after cancellation.
            terminal("owner")
            send({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"cancelled"}})
            return
        assert response.get("result", {}).get("outcome", {}).get("outcome") != "selected", response

mode = [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"plan","options":[{"value":"plan","name":"Plan"},{"value":"build","name":"Build"}]}]
while True:
    message = reader.receive(3)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"queued-fixture","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"session","configOptions":mode}})
    elif method in ["session/resume", "session/set_config_option"]:
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        if attempt > 0:
            assert scenario == "connection-loss", "unexpected process recovery"
            assert message["params"]["prompt"][0]["text"] == "continue"
            announce("queued")
            permission(901, "queued", queued_path)
            reader.assert_no_response(0.1, "unresolved old grant was lost")
            assert not os.path.exists(os.path.dirname(queued_path)), "queued directory prepared"
            terminal("owner")
            selected(901, "queued")
            terminal("queued")
            finish(request_id)
            continue
        announce("owner")
        announce("queued")
        permission(900, "owner", owner_path)
        selected(900, "owner")
        permission(901, "queued", queued_path)
        update({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"queue-ready"}})
        update({"sessionUpdate":"tool_call","toolCallId":"queue-ready","title":"Read while queued","kind":"read","status":"completed","_meta":{"claudeCode":{"toolName":"Read"}}})
        if scenario in ["peer", "terminal-queued"]:
            if scenario == "peer":
                send({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":901}})
            else:
                terminal("queued", "failed")
            response = reader.receive(3)
            assert response["id"] == 901 and response["result"]["outcome"]["outcome"] == "cancelled", response
            assert not os.path.exists(os.path.dirname(queued_path)), "cancelled queue prepared directory"
            announce("third")
            permission(902, "third", third_path)
            reader.assert_no_response(0.05, "cancellation released another tool's grant")
            terminal("owner")
            selected(902, "third")
            terminal("third")
            terminal("queued", "failed")
            finish(request_id)
        elif scenario == "connection-loss":
            time.sleep(0.15)
            os._exit(29)
        else:
            if scenario == "mode":
                update({"sessionUpdate":"current_mode_update","currentModeId":"build"})
            elif scenario == "path":
                os.symlink(os.path.dirname(owner_path), os.path.dirname(os.path.dirname(queued_path)))
                terminal("owner")
            elif scenario == "duplicate":
                permission(902, "queued", queued_path)
            elif scenario == "early-exit":
                update({"sessionUpdate":"tool_call","toolCallId":"exit","title":"Ready?","kind":"switch_mode","status":"pending","rawInput":{"plan":"# Too early"},"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}})
                send({"jsonrpc":"2.0","id":950,"method":"session/request_permission","params":{"sessionId":"session","toolCall":{"toolCallId":"exit","kind":"switch_mode","rawInput":{"plan":"# Too early"}},"options":[{"optionId":"stay","name":"Keep planning","kind":"reject_once"}]}})
            wait_cancel(request_id)
"##)).unwrap();
        let mut agent = fake_stdio_agent(
            &script,
            BTreeMap::from([
                (
                    CLAUDE_CONFIG_DIR_ENV.to_string(),
                    plans.parent().unwrap().display().to_string(),
                ),
                ("SCENARIO".to_string(), scenario.to_string()),
                (
                    "OWNER_PATH".to_string(),
                    plans.join("owner.md").display().to_string(),
                ),
                (
                    "QUEUED_PATH".to_string(),
                    workspace
                        .join(".claude/plans/queued.md")
                        .display()
                        .to_string(),
                ),
                (
                    "THIRD_PATH".to_string(),
                    plans.join("third.md").display().to_string(),
                ),
                (
                    "ATTEMPT_FILE".to_string(),
                    directory.path().join("attempts").display().to_string(),
                ),
            ]),
        );
        agent.plan_mode = Some("plan".to_string());
        agent.plan_handoff_transport = Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
        let mut config = single_agent_config(agent);
        config.review_turn_timeout_seconds = 2;
        let logs = directory.path().join("agent-runs");
        let supervisor =
            EnsembleSupervisor::new(config, &workspace, logs.clone(), test_questions()).unwrap();
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "test queued permission cancellation".into(),
            agents: supervisor.workers(EnsembleWorkflow::Plan).unwrap(),
        };
        let cancellation = CancellationToken::new();
        let root_cancel = cancellation.clone();
        if scenario == "timeout" {
            let stop = cancellation.clone();
            // Plan ignores the former two-second prompt deadline. Stop
            // this deliberately hung fixture explicitly after it passes.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                stop.cancel();
            });
        }
        let (events, mut receiver) = session_event_channel(512);
        let observe = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if scenario == "root"
                    && matches!(event, zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::AgentRunUpdated {
                        event: AgentRunEvent::ToolCall { id, .. }, ..
                    }) if id == "queue-ready")
                {
                    root_cancel.cancel();
                }
            }
        });
        let outcomes = tokio::time::timeout(
            Duration::from_secs(8),
            supervisor.observe_review_rounds(
                EnsembleLaunchRequest {
                    start: start.clone(),
                    resume: false,
                },
                events,
                TurnContext::new(TurnId::new(81), SessionMode::Build, cancellation),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{scenario}: dispatcher deadlock"))
        .unwrap();
        observe.abort();
        let outcome = &outcomes[0];
        let success = matches!(scenario, "peer" | "terminal-queued" | "connection-loss");
        let expected = match scenario {
            "root" => AgentRunStatus::Cancelled,
            "timeout" => AgentRunStatus::Cancelled,
            _ if success => AgentRunStatus::AwaitingConfirmation,
            _ => AgentRunStatus::Blocked,
        };
        let records = zevria_transcript::load_agent_run(&agent_run_path(
            &logs,
            &start.run_id,
            &outcome.descriptor.id,
        ))
        .unwrap();
        assert_eq!(
            outcome.status,
            expected,
            "{scenario}: {}",
            fixture_diagnostics(outcome, &records)
        );
        assert_eq!(
            outcome.partial,
            !success,
            "{scenario}: {}",
            fixture_diagnostics(outcome, &records)
        );
        let grants = records
            .iter()
            .filter_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        AgentRunEvent::Permission {
                            decision,
                            option_id: Some(option_id),
                            ..
                        },
                } if decision == "allow_once" => Some(option_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected_grants = if scenario == "connection-loss" {
            vec!["owner-once", "queued-once"]
        } else if success {
            vec!["owner-once", "third-once"]
        } else {
            vec!["owner-once"]
        };
        assert_eq!(
            grants,
            expected_grants,
            "{scenario}: {}",
            fixture_diagnostics(outcome, &records)
        );
        assert_eq!(outcome.has_plan_proof(), success, "{scenario}");
        assert!(
            !records.iter().any(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Prompt {
                        repair: Some(_),
                        ..
                    }
                }
            )),
            "{scenario}: queue contention must not consume semantic repair"
        );
        if scenario != "connection-loss" && scenario != "path" {
            assert!(
                !workspace.join(".claude").exists(),
                "{scenario}: ungranted directory created"
            );
        }
    }
}

#[tokio::test]
async fn live_process_crash_resumes_the_same_session_with_literal_continue() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let attempts = directory.path().join("attempts");
    let script = directory.path().join("resume_after_crash.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def fail(message):
    sys.stderr.write(message + "\n")
    sys.stderr.flush()
    sys.exit(91)

selected_mode = "build"
collaboration_mode = "default"

def options():
    return [
        {"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]},
        {"id":"collaboration_mode","name":"Collaboration mode","type":"select","currentValue":collaboration_mode,"options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]}
    ]

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"resume-fake","version":"1"}}})
    elif method == "session/new":
        if attempt != 0:
            fail("second process unexpectedly received session/new")
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"durable-session","configOptions":options()}})
    elif method == "session/resume":
        if attempt != 1 or message["params"]["sessionId"] != "durable-session":
            fail("session/resume did not use the durable session")
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/set_config_option":
        config_id = message["params"]["configId"]
        value = message["params"]["value"]
        if config_id == "execution-mode":
            if value != "read-only":
                fail("safe mode was not re-enforced")
            selected_mode = value
        elif config_id == "collaboration_mode":
            if selected_mode != "read-only" or value != "plan":
                fail("planning collaboration was not applied after safe mode")
            collaboration_mode = value
        else:
            fail("unexpected configuration option")
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        if selected_mode != "read-only" or collaboration_mode != "plan":
            fail("session prompt arrived before complete workflow configuration")
        prompt = message["params"]["prompt"]
        if len(prompt) != 1 or prompt[0].get("type") != "text":
            fail("prompt did not contain exactly one text block")
        text = prompt[0].get("text")
        if attempt == 0:
            if text == "continue":
                fail("fresh process received continuation prompt")
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"durable-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial evidence"},"messageId":"report"}}})
            time.sleep(0.15)
            os._exit(17)
        if text != "continue":
            fail("live recovery prompt was not literal continue")
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"durable-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"finished evidence"},"messageId":"report"}}})
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"durable-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"recovered-plan","content":"Recovered implementation plan\n\n- Finish after process recovery."}}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("fake recovery ACP script");

    let mut agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("ATTEMPT_FILE".to_string(), attempts.display().to_string())]),
    );
    agent.plan_config_options =
        BTreeMap::from([("collaboration_mode".to_string(), "plan".to_string())]);
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Plan,
        "plan crash recovery",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2",
        "{outcome:?}"
    );
    assert_eq!(outcome.status, AgentRunStatus::AwaitingConfirmation);
    assert!(outcome.confirmation.is_none());
    assert!(!outcome.partial);
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.acp_session_id.as_deref(), Some("durable-session"));
    assert_eq!(outcome.report, "partial evidence\nfinished evidence");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status {
                        status: AgentRunStatus::Resuming,
                        detail: Some(_),
                    }
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
            .count(),
        0
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status { status, .. }
                } if status.is_terminal()
            ))
            .count(),
        0
    );
    let requests = protocol_requests(&records);
    let resume = requests
        .iter()
        .find(|request| request["method"] == "session/resume")
        .expect("resume request");
    assert_eq!(resume["params"]["sessionId"], "durable-session");
    let prompts = requests
        .iter()
        .filter(|request| request["method"] == "session/prompt")
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2);
    assert_eq!(
        prompts[1]["params"]["prompt"],
        serde_json::json!([{"type":"text","text":"continue"}])
    );
    let configured = requests
        .iter()
        .filter(|request| request["method"] == "session/set_config_option")
        .map(|request| {
            (
                request["params"]["configId"].as_str().expect("config ID"),
                request["params"]["value"].as_str().expect("config value"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        configured,
        [
            ("execution-mode", "read-only"),
            ("collaboration_mode", "plan"),
            ("execution-mode", "read-only"),
            ("collaboration_mode", "plan"),
        ]
    );
}

#[tokio::test]
async fn live_review_crash_loads_replayed_history_without_duplication() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let attempts = directory.path().join("attempts");
    let script = directory.path().join("load_after_crash.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def fail(message):
    sys.stderr.write(message + "\n")
    sys.stderr.flush()
    sys.exit(92)

selected_mode = "build"
review_profile = "default"

def options():
    return [
        {"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]},
        {"id":"review_profile","name":"Review profile","type":"select","currentValue":review_profile,"options":[{"value":"default","name":"Default"},{"value":"audit","name":"Audit"}]}
    ]

def validate_review_meta(message):
    appended = message.get("params", {}).get("_meta", {}).get("systemPrompt", {}).get("append")
    if not isinstance(appended, str) or "independent review worker" not in appended:
        fail("review system-prompt metadata was missing")

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":True},"agentInfo":{"name":"load-fake","version":"1"}}})
    elif method == "session/new":
        validate_review_meta(message)
        if attempt != 0:
            fail("second process unexpectedly received session/new")
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"review-session","configOptions":options()}})
    elif method == "session/load":
        validate_review_meta(message)
        if attempt != 1 or message["params"]["sessionId"] != "review-session":
            fail("session/load did not use the durable session")
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"review-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"review prefix"},"messageId":"review"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/set_config_option":
        config_id = message["params"]["configId"]
        value = message["params"]["value"]
        if config_id == "execution-mode":
            if value != "read-only":
                fail("safe mode was not re-enforced")
            selected_mode = value
        elif config_id == "review_profile":
            if selected_mode != "read-only" or value != "audit":
                fail("review workflow configuration was not applied after safe mode")
            review_profile = value
        else:
            fail("unexpected configuration option")
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        if selected_mode != "read-only" or review_profile != "audit":
            fail("session prompt arrived before complete review configuration")
        prompt = message["params"]["prompt"]
        if len(prompt) != 1 or prompt[0].get("type") != "text":
            fail("prompt did not contain exactly one text block")
        text = prompt[0].get("text")
        if attempt == 0:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"review-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"review prefix"},"messageId":"review"}}})
            time.sleep(0.15)
            os._exit(18)
        if text != "continue":
            fail("load recovery prompt was not literal continue")
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"review-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"review suffix"},"messageId":"review"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("fake load-recovery ACP script");

    let mut agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("ATTEMPT_FILE".to_string(), attempts.display().to_string())]),
    );
    agent.review_system_prompt_transport = Some(ReviewSystemPromptTransport::ClaudeCodeAppend);
    agent.review_config_options =
        BTreeMap::from([("review_profile".to_string(), "audit".to_string())]);
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Review,
        "review crash recovery",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2"
    );
    assert_eq!(outcome.status, AgentRunStatus::Completed);
    assert!(!outcome.partial);
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.report, "review prefix\nreview suffix");
    assert_eq!(outcome.report.matches("review prefix").count(), 1);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::ReplayBoundary
                }
            ))
            .count(),
        1
    );
    let requests = protocol_requests(&records);
    assert!(
        !requests
            .iter()
            .any(|request| request["method"] == "session/resume")
    );
    let load = requests
        .iter()
        .find(|request| request["method"] == "session/load")
        .expect("load request");
    assert_eq!(load["params"]["sessionId"], "review-session");
    assert_eq!(
        load["params"]["_meta"]["systemPrompt"]["append"],
        REVIEW_WORKER_INSTRUCTION
    );
    let continuation = requests
        .iter()
        .filter(|request| request["method"] == "session/prompt")
        .nth(1)
        .expect("recovery prompt");
    assert_eq!(
        continuation["params"]["prompt"],
        serde_json::json!([{"type":"text","text":"continue"}])
    );
    let configured = requests
        .iter()
        .filter(|request| request["method"] == "session/set_config_option")
        .map(|request| {
            (
                request["params"]["configId"].as_str().expect("config ID"),
                request["params"]["value"].as_str().expect("config value"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        configured,
        [
            ("execution-mode", "read-only"),
            ("review_profile", "audit"),
            ("execution-mode", "read-only"),
            ("review_profile", "audit"),
        ]
    );
}

#[tokio::test]
async fn repeated_process_crash_exhausts_one_relaunch_and_finishes_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let attempts = directory.path().join("attempts");
    let script = directory.path().join("crash_twice.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_options():
    return [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"crash-fake","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"crash-session","configOptions":mode_options()}})
    elif method == "session/resume":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options()}})
    elif method == "session/prompt":
        text = "first crash evidence" if attempt == 0 else "second crash evidence"
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"crash-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text},"messageId":"crash-report"}}})
        time.sleep(0.15)
        os._exit(20 + attempt)
"#,
        )
        .expect("double-crash ACP script");

    let agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("ATTEMPT_FILE".to_string(), attempts.display().to_string())]),
    );
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Plan,
        "keep the crash evidence",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2"
    );
    assert_eq!(outcome.status, AgentRunStatus::Blocked);
    assert!(outcome.partial);
    assert_eq!(
        outcome.report,
        "first crash evidence\nsecond crash evidence"
    );
    let failure = outcome.failure.as_deref().expect("failure diagnostic");
    // The SDK formats the native ExitStatus; Windows uses "exit code".
    let process_exit = if cfg!(windows) {
        "Process exited with exit code: 21"
    } else {
        "Process exited with exit status: 21"
    };
    assert!(
        failure.contains(process_exit) || failure.contains("Incoming transport closed"),
        "{failure}"
    );
    assert!(failure.contains("automatic process recovery attempt 1 of 1"));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status {
                        status: AgentRunStatus::Resuming,
                        ..
                    }
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
            .count(),
        0
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status { status, .. }
                } if status.is_terminal()
            ))
            .count(),
        0
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Failure { .. }
                }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn unavailable_session_recovery_remains_interrupted_with_partial_evidence() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let attempts = directory.path().join("attempts");
    let script = directory.path().join("recovery_unavailable.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        capabilities = {"sessionCapabilities":{"resume":{}}} if attempt == 0 else {}
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":capabilities,"agentInfo":{"name":"unavailable-fake","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"unavailable-session","configOptions":mode}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"unavailable-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"evidence before interruption"},"messageId":"partial"}}})
        time.sleep(0.15)
        os._exit(25)
"#,
        )
        .expect("unavailable recovery ACP script");

    let agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("ATTEMPT_FILE".to_string(), attempts.display().to_string())]),
    );
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Review,
        "retain partial evidence",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2"
    );
    assert_eq!(outcome.status, AgentRunStatus::Interrupted);
    assert!(outcome.partial);
    assert_eq!(outcome.report, "evidence before interruption");
    let failure = outcome.failure.as_deref().expect("interruption diagnostic");
    assert!(failure.contains("neither session/resume nor session/load"));
    assert!(failure.contains("automatic process recovery attempt 1 of 1"));
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
        1,
        "no continuation prompt is sent when session recovery is unavailable"
    );
}

#[tokio::test]
async fn startup_auth_and_ordinary_acp_failures_do_not_relaunch() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("non_retryable_failures.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))
scenario = os.environ["SCENARIO"]
mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        if scenario == "auth":
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32000,"message":"Authentication required"}})
        else:
            send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"failure-fake","version":"1"}}})
    elif method == "session/new":
        if scenario == "before-session":
            os._exit(30)
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"failure-session","configOptions":mode}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32603,"message":"ordinary prompt failure","data":{"kind":"scripted"}}})
"#,
        )
        .expect("non-retryable ACP script");

    for scenario in ["before-session", "auth", "ordinary"] {
        let scenario_dir = directory.path().join(scenario);
        std::fs::create_dir(&scenario_dir).expect("scenario directory");
        let attempts = scenario_dir.join("attempts");
        let agent = fake_stdio_agent(
            &script,
            BTreeMap::from([
                ("ATTEMPT_FILE".to_string(), attempts.display().to_string()),
                ("SCENARIO".to_string(), scenario.to_string()),
            ]),
        );
        let logs = scenario_dir.join("agent-runs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(agent),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .expect("valid supervisor");
        let (_start, outcome, records) =
            launch_fake_worker(&supervisor, &logs, EnsembleWorkflow::Review, scenario).await;

        assert_eq!(
            std::fs::read_to_string(attempts).expect("attempt count"),
            "1",
            "scenario {scenario} relaunched unexpectedly: {outcome:?}"
        );
        assert_eq!(outcome.status, AgentRunStatus::Failed);
        assert!(!records.iter().any(|record| matches!(
            record,
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
async fn end_turn_observed_before_nonzero_exit_does_not_duplicate_the_prompt() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let attempts = directory.path().join("attempts");
    let script = directory.path().join("end_turn_then_exit.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"late-exit-fake","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"late-exit-session","configOptions":mode}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"late-exit-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"complete before exit"},"messageId":"complete"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
        time.sleep(0.2)
        os._exit(23)
"#,
        )
        .expect("late-exit ACP script");

    let agent = fake_stdio_agent(
        &script,
        BTreeMap::from([("ATTEMPT_FILE".to_string(), attempts.display().to_string())]),
    );
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) =
        launch_fake_worker(&supervisor, &logs, EnsembleWorkflow::Review, "finish once").await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "1"
    );
    assert_eq!(outcome.status, AgentRunStatus::Completed);
    assert!(!outcome.partial);
    assert_eq!(outcome.report, "complete before exit");
    assert!(outcome.failure.is_none());
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
    assert!(!records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Resuming,
                ..
            }
        }
    )));
}

#[tokio::test]
async fn claude_handoff_state_and_unresolved_work_survive_process_recovery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let claude_config = directory.path().join("claude-config");
    let plans = claude_config.join("plans");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(&plans).expect("Claude plans directory");
    let attempts = directory.path().join("attempts");
    let second_plan = plans.join("second-plan.md");
    let script = directory.path().join("handoff_crash.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"handoff-crash-fake","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"handoff-session","configOptions":mode}})
    elif method == "session/resume":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        if attempt == 0:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"handoff-session","update":{"sessionUpdate":"tool_call","toolCallId":"first-write","title":"Preparing file…","kind":"edit","status":"pending","content":[],"locations":[],"rawInput":{},"_meta":{"claudeCode":{"toolName":"Write"}}}}})
            time.sleep(0.15)
            os._exit(24)
        prompt = message["params"]["prompt"]
        if len(prompt) != 1 or prompt[0].get("text") != "continue":
            sys.exit(94)
        path = os.environ["SECOND_PLAN_PATH"]
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"handoff-session","update":{"sessionUpdate":"tool_call","toolCallId":"second-write","title":"Write second plan","kind":"edit","status":"pending","content":[],"locations":[{"path":path}],"rawInput":{"file_path":path},"_meta":{"claudeCode":{"toolName":"Write"}}}}})
        time.sleep(0.1)
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("handoff recovery ACP script");

    let mut agent = fake_stdio_agent(
        &script,
        BTreeMap::from([
            ("ATTEMPT_FILE".to_string(), attempts.display().to_string()),
            (
                CLAUDE_CONFIG_DIR_ENV.to_string(),
                claude_config.display().to_string(),
            ),
            (
                "SECOND_PLAN_PATH".to_string(),
                second_plan.display().to_string(),
            ),
        ]),
    );
    agent.plan_handoff_transport = Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Plan,
        "exercise handoff recovery",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2"
    );
    assert_eq!(outcome.status, AgentRunStatus::Blocked);
    assert!(outcome.partial);
    assert!(
        outcome
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("first-write")
                && failure.contains("abandoned preparation")
                && failure.contains("second-write")
                && failure
                    .matches("Claude plan artifacts remain unresolved:")
                    .count()
                    == 1),
        "{outcome:?}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status {
                        status: AgentRunStatus::Resuming,
                        ..
                    }
                }
            ))
            .count(),
        1,
        "the unresolved first write must not terminate the logical worker before recovery"
    );
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::ToolCall { id, .. }
        } if id == "first-write"
    )));
}

#[tokio::test]
async fn terminal_claude_handoff_state_permits_later_artifact_after_process_recovery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let claude_config = directory.path().join("claude-config");
    let plans = claude_config.join("plans");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir_all(&plans).expect("Claude plans directory");
    let attempts = directory.path().join("attempts");
    let first_plan = plans.join("first-plan.md");
    let second_plan = plans.join("second-plan.md");
    let script = directory.path().join("terminal_handoff_recovery.py");
    std::fs::write(
            &script,
            r##"import json
import os
import sys
import time

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

attempt_path = os.environ["ATTEMPT_FILE"]
try:
    attempt = int(open(attempt_path).read())
except FileNotFoundError:
    attempt = 0
open(attempt_path, "w").write(str(attempt + 1))

first_path = os.environ["FIRST_PLAN_PATH"]
second_path = os.environ["SECOND_PLAN_PATH"]
plan = "# Recovered plan\n\n- Preserve terminal handoff state."

def permit(tool_id, path, permission_id):
    send({"jsonrpc":"2.0","id":permission_id,"method":"session/request_permission","params":{"sessionId":"terminal-handoff-session","toolCall":{"toolCallId":tool_id,"kind":"edit","rawInput":{"file_path":path},"_meta":{"claudeCode":{"toolName":"Write"}}},"options":[{"optionId":tool_id + "-once","name":"Once","kind":"allow_once"}]}})
    response = json.loads(sys.stdin.readline())
    assert response["id"] == permission_id, response
    assert response["result"]["outcome"]["optionId"] == tool_id + "-once", response
    with open(path, "w") as artifact:
        artifact.write("# permitted artifact\n")

mode = [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"terminal-handoff-recovery-fake","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"terminal-handoff-session","configOptions":mode}})
    elif method == "session/resume":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode}})
    elif method == "session/prompt":
        if attempt == 0:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call","toolCallId":"first-write","title":"Write first plan","kind":"edit","status":"pending","rawInput":{"file_path":first_path},"locations":[{"path":first_path}],"_meta":{"claudeCode":{"toolName":"Write"}}}}})
            permit("first-write", first_path, 948)
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"first-write","status":"completed","_meta":{"claudeCode":{"toolName":"Write"}}}}})
            time.sleep(0.15)
            os._exit(25)
        prompt = message["params"]["prompt"]
        if len(prompt) != 1 or prompt[0].get("text") != "continue":
            sys.exit(95)
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call","toolCallId":"second-write","title":"Write second plan","kind":"edit","status":"pending","rawInput":{"file_path":second_path},"locations":[{"path":second_path}],"_meta":{"claudeCode":{"toolName":"Write"}}}}})
        permit("second-write", second_path, 949)
        # A replayed earlier terminal must not affect the current grant.
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"first-write","status":"completed","_meta":{"claudeCode":{"toolName":"Write"}}}}})
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"second-write","status":"completed","_meta":{"claudeCode":{"toolName":"Write"}}}}})
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"terminal-handoff-session","update":{"sessionUpdate":"tool_call","toolCallId":"exit-plan","title":"Ready to code?","kind":"switch_mode","status":"pending","rawInput":{"plan":plan},"content":[{"type":"content","content":{"type":"text","text":plan}}],"_meta":{"claudeCode":{"toolName":"ExitPlanMode"}}}}})
        send({"jsonrpc":"2.0","id":950,"method":"session/request_permission","params":{"sessionId":"terminal-handoff-session","toolCall":{"toolCallId":"exit-plan","kind":"switch_mode","rawInput":{"plan":plan,"planFilePath":second_path},"content":[{"type":"content","content":{"type":"text","text":plan}}]},"options":[{"optionId":"exit-plan-default","name":"Yes, manually approve edits","kind":"allow_once"},{"optionId":"reject","name":"No, keep planning","kind":"reject_once"}]}})
        exit_response = json.loads(sys.stdin.readline())
        sys.stderr.write("exit-permission-response:" + json.dumps(exit_response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        while True:
            cancellation = json.loads(sys.stdin.readline())
            if cancellation.get("method") == "session/cancel":
                send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"cancelled"}})
                break
"##,
        )
        .expect("terminal handoff recovery ACP script");

    let mut agent = fake_stdio_agent(
        &script,
        BTreeMap::from([
            ("ATTEMPT_FILE".to_string(), attempts.display().to_string()),
            (
                CLAUDE_CONFIG_DIR_ENV.to_string(),
                claude_config.display().to_string(),
            ),
            (
                "FIRST_PLAN_PATH".to_string(),
                first_plan.display().to_string(),
            ),
            (
                "SECOND_PLAN_PATH".to_string(),
                second_plan.display().to_string(),
            ),
        ]),
    );
    agent.plan_handoff_transport = Some(PlanHandoffTransport::ClaudeCodeExitPlanMode);
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let (_start, outcome, records) = launch_fake_worker(
        &supervisor,
        &logs,
        EnsembleWorkflow::Plan,
        "exercise terminal handoff recovery",
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(attempts).expect("attempt count"),
        "2"
    );
    assert_eq!(outcome.status, AgentRunStatus::AwaitingConfirmation);
    assert!(outcome.confirmation.is_none());
    assert!(!outcome.partial);
    assert!(outcome.report.is_empty());
    assert_eq!(
        outcome
            .plan
            .as_ref()
            .and_then(|plan| plan.markdown.as_deref()),
        Some("# Recovered plan\n\n- Preserve terminal handoff state.")
    );
    assert!(outcome.failure.is_none());
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Status {
                        status: AgentRunStatus::Resuming,
                        ..
                    }
                }
            ))
            .count(),
        1
    );
    for id in ["first-write", "second-write"] {
        assert!(records.iter().any(|record| matches!(record,
            AgentRunTranscriptRecord::Event { event: AgentRunEvent::Permission {
                decision, option_id: Some(option_id), ..
            }} if decision == "allow_once" && option_id == &format!("{id}-once")
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::ToolCall { id: observed, .. }
            } if observed == id
        )));
    }
}

#[tokio::test]
async fn fake_stdio_agent_negotiates_v1_permissions_and_report_persistence() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("fake_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"fake-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"fake-session","modes":{"currentModeId":"build","availableModes":[{"id":"read-only","name":"Read only"},{"id":"build","name":"Build"}]},"configOptions":[{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]}]}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":[{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]}]}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"fake-session","toolCall":{"toolCallId":"read-1","kind":"read"},"options":[{"optionId":"always","name":"Always","kind":"allow_always"},{"optionId":"once","name":"Once","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}})
        permission_response = json.loads(sys.stdin.readline())
        sys.stderr.write("permission-response:" + json.dumps(permission_response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"independent report"},"messageId":"message-1"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("fake ACP script");

    let agent = EnsembleAgentConfig {
        label: "Fake ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: "Authenticate the fake agent.".to_string(),
    };
    let config = single_agent_config(agent);
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(config, &workspace, logs.clone(), test_questions())
        .expect("valid supervisor");
    let agents = supervisor
        .workers(EnsembleWorkflow::Review)
        .expect("worker descriptor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review the workspace".into(),
        agents,
    };
    let (events, _receiver) = session_event_channel(256);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("launch succeeds");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    assert_eq!(outcomes[0].report, "independent report");
    assert_eq!(outcomes[0].acp_session_id.as_deref(), Some("fake-session"));

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("durable worker transcript");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Prompt {
                text,
                continuation: false,
                ..
            }
        } if text.contains(REVIEW_WORKER_INSTRUCTION)
            && text.contains("review the workspace")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Permission { decision, .. }
        } if decision == "allow_once"
    )));
    assert!(records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Stderr { text }
            } if text.contains("permission-response") && text.contains("once") && !text.contains("always\"")
        )));

    let client_requests = records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event:
                    AgentRunEvent::Protocol {
                        direction: AgentProtocolDirection::ClientToAgent,
                        json,
                    },
            } => serde_json::from_str::<serde_json::Value>(json).ok(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let initialize = client_requests
        .iter()
        .find(|request| request["method"] == "initialize")
        .expect("initialize request");
    assert_eq!(
        initialize["params"]["clientCapabilities"]["fs"]["writeTextFile"],
        false
    );
    assert_eq!(
        initialize["params"]["clientCapabilities"]["terminal"],
        false
    );
    let new_session = client_requests
        .iter()
        .find(|request| request["method"] == "session/new")
        .expect("new-session request");
    assert_eq!(new_session["params"]["mcpServers"], serde_json::json!([]));
    assert_session_workspace(&new_session["params"]["cwd"], &workspace);
    let safe_mode = client_requests
        .iter()
        .find(|request| request["method"] == "session/set_config_option")
        .expect("categorized mode configuration must be preferred over legacy modes");
    assert_eq!(safe_mode["params"]["configId"], "execution-mode");
    assert_eq!(safe_mode["params"]["value"], "read-only");
}

#[tokio::test]
async fn workflow_config_option_startup_failures_precede_prompt_dispatch() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("workflow_config_failures.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys

scenario = os.environ["SCENARIO"]
selected_mode = "build"
collaboration_mode = "default"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    configured = [
        {"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]}
    ]
    if scenario == "missing":
        return configured
    if scenario == "unsupported_kind":
        configured.append({"id":"collaboration_mode","name":"Collaboration mode","type":"boolean","currentValue":False})
        return configured
    values = [{"value":"default","name":"Default"}]
    if scenario != "unsupported_value":
        values.append({"value":"plan","name":"Plan"})
    configured.append({"id":"collaboration_mode","name":"Collaboration mode","type":"select","currentValue":collaboration_mode,"options":values})
    return configured

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"workflow-config-failure-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"workflow-config-failure-session","configOptions":options()}})
    elif method == "session/set_config_option":
        config_id = message["params"]["configId"]
        value = message["params"]["value"]
        if config_id == "execution-mode":
            selected_mode = value
            send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
        elif scenario == "rejected":
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32602,"message":"scripted workflow option rejection"}})
        else:
            if scenario != "unverified":
                collaboration_mode = value
            send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        sys.stderr.write("PROMPT_DISPATCHED\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-config-failure-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"unexpected prompt"},"messageId":"message"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("workflow configuration failure ACP script");

    for (scenario, expected) in [
        ("missing", "exact option ID is absent"),
        (
            "unsupported_value",
            "does not advertise the desired select value",
        ),
        ("unsupported_kind", "unsupported non-select option kind"),
        ("unverified", "failed to verify"),
        ("rejected", "ACP rejected"),
        ("overlap", "overlaps the authoritative safe-mode setting"),
    ] {
        let scenario_root = directory.path().join(scenario);
        std::fs::create_dir(&scenario_root).expect("scenario directory");
        let mut agent = fake_stdio_agent(
            &script,
            BTreeMap::from([("SCENARIO".to_string(), scenario.to_string())]),
        );
        let (id, desired) = if scenario == "overlap" {
            ("execution-mode", "read-only")
        } else {
            ("collaboration_mode", "plan")
        };
        agent.plan_config_options = BTreeMap::from([(id.to_string(), desired.to_string())]);
        let logs = scenario_root.join("agent-runs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(agent),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .expect("valid supervisor");
        let (_start, outcome, records) =
            launch_fake_worker(&supervisor, &logs, EnsembleWorkflow::Plan, scenario).await;
        assert_eq!(outcome.status, AgentRunStatus::Blocked, "{scenario}");
        let failure = outcome.failure.as_deref().expect("startup failure");
        assert!(failure.contains(expected), "{scenario}: {failure}");
        assert!(failure.contains("/ensemble-plan"), "{scenario}: {failure}");
        assert!(failure.contains(id), "{scenario}: {failure}");
        assert!(failure.contains(desired), "{scenario}: {failure}");
        assert!(
            !protocol_requests(&records)
                .iter()
                .any(|request| request["method"] == "session/prompt"),
            "{scenario} dispatched a prompt despite incompatible workflow configuration"
        );
        assert!(!records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Stderr { text }
            } if text.contains("PROMPT_DISPATCHED")
        )));
    }
}

#[tokio::test]
async fn workflow_config_option_drift_cancels_only_the_affected_worker() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("workflow_config_drift.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys

scenario = os.environ["SCENARIO"]
selected_mode = "build"
collaboration_mode = "default"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_option():
    return {"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]}

def collaboration_option():
    return {"id":"collaboration_mode","name":"Collaboration mode","type":"select","currentValue":collaboration_mode,"options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]}

def options():
    return [mode_option(), collaboration_option()]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"workflow-config-drift-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"workflow-config-drift-session","configOptions":options()}})
    elif method == "session/set_config_option":
        config_id = message["params"]["configId"]
        if config_id == "execution-mode":
            selected_mode = message["params"]["value"]
        elif config_id == "collaboration_mode":
            collaboration_mode = message["params"]["value"]
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        drifted = [mode_option()]
        if scenario == "reverted":
            collaboration_mode = "default"
            drifted.append(collaboration_option())
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"workflow-config-drift-session","update":{"sessionUpdate":"config_option_update","configOptions":drifted}}})
        for cancellation in sys.stdin:
            cancellation = json.loads(cancellation)
            if cancellation.get("method") == "session/cancel":
                send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"cancelled"}})
                break
"#,
        )
        .expect("workflow configuration drift ACP script");

    for scenario in ["reverted", "omitted"] {
        let scenario_root = directory.path().join(scenario);
        std::fs::create_dir(&scenario_root).expect("scenario directory");
        let mut agent = fake_stdio_agent(
            &script,
            BTreeMap::from([("SCENARIO".to_string(), scenario.to_string())]),
        );
        agent.plan_config_options =
            BTreeMap::from([("collaboration_mode".to_string(), "plan".to_string())]);
        let logs = scenario_root.join("agent-runs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(agent),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .expect("valid supervisor");
        let (_start, outcome, records) =
            launch_fake_worker(&supervisor, &logs, EnsembleWorkflow::Plan, scenario).await;
        assert_eq!(outcome.status, AgentRunStatus::Blocked, "{scenario}");
        let failure = outcome.failure.as_deref().expect("drift failure");
        assert!(
            failure.contains("workflow-configuration violation"),
            "{scenario}: {failure}"
        );
        assert!(failure.contains("collaboration_mode"));
        if scenario == "omitted" {
            assert!(failure.contains("stopped reporting"));
        } else {
            assert!(failure.contains("changed required option"));
        }
        assert!(records.iter().any(|record| matches!(
            record,
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Failure { error }
            } if error.contains("workflow-configuration violation")
        )));
    }
}

#[tokio::test]
async fn codex_plan_collaboration_enables_conditional_structured_question_only_for_plan() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let source = workspace.join("source.txt");
    std::fs::write(&source, "workspace remains read-only\n").expect("workspace fixture");
    let script = directory.path().join("codex_collaboration_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

selected_mode = "default"
collaboration_mode = "default"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [
        {"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":selected_mode,"options":[{"value":"read-only","name":"Read only"},{"value":"agent","name":"Agent"}]},
        {"id":"collaboration_mode","name":"Collaboration mode","type":"select","currentValue":collaboration_mode,"options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]}
    ]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        form = message["params"]["clientCapabilities"].get("elicitation", {}).get("form")
        if form != {}:
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32602,"message":"form elicitation missing"}})
            continue
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"codex-shaped-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"codex-shaped-session","configOptions":options()}})
    elif method == "session/set_config_option":
        config_id = message["params"]["configId"]
        value = message["params"]["value"]
        if config_id == "mode":
            selected_mode = value
        elif config_id == "collaboration_mode":
            collaboration_mode = value
        else:
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32602,"message":"unknown option"}})
            continue
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        if collaboration_mode == "plan":
            send({"jsonrpc":"2.0","id":900,"method":"elicitation/create","params":{
                "mode":"form",
                "sessionId":"codex-shaped-session",
                "message":"Choose the consequential compatibility policy.",
                "requestedSchema":{
                    "type":"object",
                    "properties":{
                        "compatibility":{"type":"string","title":"Compatibility","oneOf":[{"const":"preserve","title":"Preserve API"},{"const":"simplify","title":"Simplify API"}]}
                    },
                    "required":["compatibility"]
                }
            }})
            while True:
                response = json.loads(sys.stdin.readline())
                if response.get("id") == 900:
                    break
            if response.get("result", {}).get("action") != "accept":
                send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32603,"message":"question was not accepted"}})
                continue
            sys.stderr.write("codex-question-response:" + json.dumps(response, separators=(",", ":")) + "\n")
            sys.stderr.flush()
            report = "plan report after accepted question"
        else:
            report = "review report without planning question"
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"codex-shaped-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":report},"messageId":"report"}}})
        if collaboration_mode == "plan":
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"codex-shaped-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"codex-plan","content":"Codex implementation plan\n\n- Preserve the selected API compatibility."}}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("Codex-shaped ACP script");

    let mut agent = fake_stdio_agent(&script, BTreeMap::new());
    agent.label = "Codex-shaped ACP".to_string();
    agent.review_mode = Some("agent".to_string());
    agent.plan_config_options =
        BTreeMap::from([("collaboration_mode".to_string(), "plan".to_string())]);
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(512);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        questions.requester,
    )
    .expect("valid supervisor");

    let plan_start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Plan,
        prompt: "inspect first, then resolve the compatibility preference".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("planning worker"),
    };
    let launched = supervisor.clone();
    let launch_events = events.clone();
    let launch_start = plan_start.clone();
    let plan_launch = tokio::spawn(async move {
        launched
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start: launch_start,
                    resume: false,
                },
                launch_events,
                TurnContext::new(
                    TurnId::new(101),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });
    let question = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("Codex planning question appears");
    assert_eq!(question.source_label.as_deref(), Some("Codex-shaped ACP"));
    assert_eq!(question.questions.len(), 1);
    assert!(questions.responder.respond(
        &question.id,
        QuestionResponse::Answered {
            answers: vec![zevria_foundation::QuestionAnswer {
                id: "compatibility".to_string(),
                answer: Some(QuestionAnswerValue::String("Preserve API".to_string())),
            }],
        }
    ));
    let plan_outcomes = plan_launch
        .await
        .expect("planning launch task")
        .expect("planning launch");
    let plan_outcome = &plan_outcomes[0];
    assert_eq!(plan_outcome.status, AgentRunStatus::AwaitingConfirmation);
    assert!(plan_outcome.confirmation.is_none());
    assert_eq!(plan_outcome.report, "plan report after accepted question");
    assert_eq!(plan_outcome.user_decisions.len(), 1);
    assert_eq!(plan_outcome.user_decisions[0].request_id, question.id);
    assert_eq!(
        plan_outcome.user_decisions[0].answers[0].answer,
        AgentUserDecisionValue::String {
            value: "Preserve API".to_string()
        }
    );
    let plan_path = agent_run_path(&logs, &plan_start.run_id, &plan_outcome.descriptor.id);
    let plan_records = load_agent_run(&plan_path).expect("planning transcript");
    assert!(plan_records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Elicitation {
                outcome: AgentElicitationOutcome::Accepted,
                decision: Some(_),
                ..
            }
        }
    )));
    let ordered_methods = plan_records
        .iter()
        .filter_map(|record| match record {
            AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Protocol { json, .. },
            } => serde_json::from_str::<serde_json::Value>(json).ok(),
            _ => None,
        })
        .filter_map(|message| message["method"].as_str().map(str::to_string))
        .filter(|method| {
            matches!(
                method.as_str(),
                "initialize"
                    | "session/new"
                    | "session/set_config_option"
                    | "session/prompt"
                    | "elicitation/create"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_methods,
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option",
            "session/prompt",
            "elicitation/create",
        ]
    );
    let plan_requests = protocol_requests(&plan_records);
    let initialize = plan_requests
        .iter()
        .find(|request| request["method"] == "initialize")
        .expect("initialize request");
    assert_eq!(
        initialize["params"]["clientCapabilities"]["elicitation"]["form"],
        serde_json::json!({})
    );
    let configured = plan_requests
        .iter()
        .filter(|request| request["method"] == "session/set_config_option")
        .map(|request| {
            (
                request["params"]["configId"].as_str().expect("config ID"),
                request["params"]["value"].as_str().expect("config value"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        configured,
        [("mode", "read-only"), ("collaboration_mode", "plan")]
    );
    assert_eq!(
        std::fs::read_to_string(&source).expect("read-only workspace fixture"),
        "workspace remains read-only\n"
    );

    while receiver.try_recv().is_ok() {}
    let review_start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review without entering planning collaboration".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("review worker"),
    };
    let launched = supervisor.clone();
    let launch_events = events.clone();
    let launch_start = review_start.clone();
    let mut review_launch = tokio::spawn(async move {
        launched
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start: launch_start,
                    resume: false,
                },
                launch_events,
                TurnContext::new(
                    TurnId::new(102),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });
    let review_outcomes = tokio::select! {
        result = &mut review_launch => result.expect("review launch task").expect("review launch"),
        question = receive_question(&mut receiver) => panic!("review unexpectedly asked a planning question: {question:?}"),
    };
    let review_outcome = &review_outcomes[0];
    assert_eq!(review_outcome.status, AgentRunStatus::Completed);
    assert_eq!(
        review_outcome.report,
        "review report without planning question"
    );
    assert!(review_outcome.user_decisions.is_empty());
    let review_path = agent_run_path(&logs, &review_start.run_id, &review_outcome.descriptor.id);
    let review_records = load_agent_run(&review_path).expect("review transcript");
    let review_requests = protocol_requests(&review_records);
    let review_configured = review_requests
        .iter()
        .filter(|request| request["method"] == "session/set_config_option")
        .map(|request| {
            (
                request["params"]["configId"].as_str().expect("config ID"),
                request["params"]["value"].as_str().expect("config value"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(review_configured, [("mode", "agent")]);
    assert!(!review_records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Protocol { json, .. }
        } if serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .is_some_and(|message| message["method"] == "elicitation/create")
    )));
    assert_eq!(
        std::fs::read_to_string(source).expect("review workspace fixture"),
        "workspace remains read-only\n"
    );
}

#[tokio::test]
async fn fake_stdio_form_elicitation_round_trips_for_plan_and_review() {
    stdio_form_elicitation_round_trips(StdioElicitationFixture::Generic).await;
}

#[tokio::test]
async fn fake_stdio_native_companions_round_trip_and_persist_only_visible_decisions() {
    stdio_form_elicitation_round_trips(StdioElicitationFixture::Native).await;
}

#[tokio::test]
async fn fake_stdio_codex_1_13_1_notes_round_trip_and_persist_only_visible_decisions() {
    stdio_form_elicitation_round_trips(StdioElicitationFixture::Codex).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdioElicitationFixture {
    Generic,
    Native,
    Codex,
}

async fn stdio_form_elicitation_round_trips(fixture_kind: StdioElicitationFixture) {
    let native = fixture_kind == StdioElicitationFixture::Native;
    let codex = fixture_kind == StdioElicitationFixture::Codex;
    let field_count = if native || codex { 3 } else { 5 };
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("elicitation_acp.py");
    let response_path = directory.path().join("elicitation-response.json");
    std::fs::write(
            &script,
            r#"import json
import os
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def mode_options():
    return [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

is_plan = False
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        is_plan = message["params"]["clientCapabilities"].get("plan") == {}
        form = message["params"]["clientCapabilities"].get("elicitation", {}).get("form")
        if form != {}:
            send({"jsonrpc":"2.0","id":request_id,"error":{"code":-32602,"message":"form elicitation missing"}})
            continue
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"elicitation-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"elicitation-session","configOptions":mode_options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":mode_options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":900,"method":"elicitation/create","params":{
            "mode":"form",
            "sessionId":"elicitation-session",
            "toolCallId":"question-tool",
            "message":"Please answer\r\nthe review questions.\r",
            "requestedSchema":json.loads(os.environ.get("ELICITATION_SCHEMA", "null")) or {
                "type":"object",
                "properties":{
                    "choice":{"type":"string","title":"Choi\rce","oneOf":[{"const":"wire_a\r\n","title":"Visible A\r"},{"const":"wire_b","title":"Visible B"}]},
                    "confirmed":{"type":"boolean","title":"Confirmed"},
                    "flags":{"type":"array","title":"Fla\rgs","items":{"anyOf":[{"const":"flag_x","title":"Flag X\r"},{"const":"flag_y","title":"Flag Y\r"}]}},
                    "name":{"type":"string","title":"Name","minLength":1},
                    "note":{"type":"string","title":"Note"}
                },
                "required":["choice","confirmed","flags","name"]
            }
        }})
        response = json.loads(sys.stdin.readline())
        # Close the peer's receipt before stdout can complete the turn. Stderr
        # is an independent best-effort stream, not a receipt acknowledgement.
        with open(os.environ["ELICITATION_RESPONSE"], "w", encoding="utf-8", newline="\n") as capture:
            json.dump(response, capture, separators=(",", ":"))
            capture.write("\n")
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"elicitation-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"question answered report"},"messageId":"message-1"}}})
        if is_plan:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"elicitation-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"elicitation-plan","content":"Elicitation plan\n\n- Apply the captured decision."}}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("fake elicitation ACP script");

    let mut env = BTreeMap::from([(
        "ELICITATION_RESPONSE".to_string(),
        response_path.display().to_string(),
    )]);
    if native {
        let mut fixture = native_elicitation_fixture();
        let properties = &mut fixture["requestedSchema"]["properties"];
        properties["question_2"] = serde_json::json!({
            "type": "array", "items": {"anyOf": properties["question_2"]["oneOf"].clone()},
            "minItems": 2, "maxItems": 2, "default": ["option_0", NATIVE_OTHER]
        });
        properties["question_2_other"]["default"] = serde_json::json!("Custom target");
        properties["question_0"]["title"] = serde_json::json!("Choice\r\nzero\r");
        properties["question_0"]["description"] = serde_json::json!("Choose\r\na scope\r");
        properties["question_0"]["oneOf"][0]["title"] = serde_json::json!("Focu\rsed");
        properties["question_0"]["oneOf"][0]["const"] = serde_json::json!("option_0\r\n");
        env.insert(
            "ELICITATION_SCHEMA".to_string(),
            fixture["requestedSchema"].to_string(),
        );
    } else if codex {
        env.insert(
            "ELICITATION_SCHEMA".to_string(),
            codex_acp_1_13_1_elicitation_fixture()["requestedSchema"].to_string(),
        );
    }
    let source_label = if codex { "Codex ACP" } else { "Question ACP" };
    let agent = EnsembleAgentConfig {
        label: source_label.to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env,
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(512);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        questions.requester,
    )
    .expect("valid supervisor");

    for (turn_number, workflow, dismiss) in [
        (11, EnsembleWorkflow::Plan, false),
        (12, EnsembleWorkflow::Review, false),
        (13, EnsembleWorkflow::Review, true),
        (14, EnsembleWorkflow::Plan, true),
    ] {
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow,
            prompt: "inspect and ask if needed".into(),
            agents: supervisor.workers(workflow).expect("worker descriptor"),
        };
        let launched = supervisor.clone();
        let launch_events = events.clone();
        let launch_start = start.clone();
        let launch = tokio::spawn(async move {
            launched
                .observe_review_rounds(
                    EnsembleLaunchRequest {
                        start: launch_start,
                        resume: false,
                    },
                    launch_events,
                    TurnContext::new(
                        TurnId::new(turn_number),
                        SessionMode::Build,
                        CancellationToken::new(),
                    ),
                )
                .await
        });
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(zevria_session_api::SessionUpdate::Lifecycle(event)) =
                    receiver.recv().await
                {
                    match event {
                        SessionEvent::QuestionAsked { request, .. } => break request,
                        SessionEvent::AgentRunFinished { outcome, .. } => {
                            panic!("worker finished before asking: {outcome:?}")
                        }
                        _ => {}
                    }
                }
            }
        })
        .await
        .expect("ACP question appears");
        assert_eq!(request.source_label.as_deref(), Some(source_label));
        assert_eq!(request.questions.len(), field_count);
        for question in &request.questions {
            assert!(!question.header.contains('\r'));
            assert!(!question.question.contains('\r'));
            for option in &question.options {
                assert!(!option.label.contains('\r'));
                assert!(!option.description.contains('\r'));
            }
        }
        if native || codex {
            for question in &request.questions {
                assert!(!question.id.ends_with("_other"));
                assert!(!question.id.ends_with("_note"));
                if codex {
                    assert_eq!(
                        question.kind,
                        QuestionPromptKind::SingleSelect { allow_other: true }
                    );
                }
                assert!(question.required);
                assert_eq!(question.options.len(), 2);
                assert!(question.options.iter().all(|option| option.label != "Other"
                    && option.label != NATIVE_OTHER
                    && option.label != "None of the above"));
            }
        }
        if native {
            assert_eq!(
                request.questions[2].default,
                Some(QuestionAnswerValue::Strings(vec![
                    "Focused".into(),
                    "Custom target".into()
                ]))
            );
        }
        let response = if dismiss {
            QuestionResponse::Dismissed
        } else {
            let answers = request
                .questions
                .iter()
                .map(|question| zevria_foundation::QuestionAnswer {
                    id: question.id.clone(),
                    answer: match question.id.as_str() {
                        "question_0" => Some(QuestionAnswerValue::String("Focused".into())),
                        "question_1" => Some(QuestionAnswerValue::String("Custom scope".into())),
                        "question_2" if codex => Some(QuestionAnswerValue::String("Broad".into())),
                        "question_2" => question.default.clone(),
                        "choice" => Some(QuestionAnswerValue::String("Visible A".to_string())),
                        "confirmed" => Some(QuestionAnswerValue::String("Yes".to_string())),
                        "flags" => Some(QuestionAnswerValue::Strings(vec![
                            "Flag X".to_string(),
                            "Flag Y".to_string(),
                        ])),
                        "name" => Some(QuestionAnswerValue::String("Ada".to_string())),
                        "note" => None,
                        other => panic!("unexpected field {other}"),
                    },
                })
                .collect();
            QuestionResponse::Answered { answers }
        };
        assert!(questions.responder.respond(&request.id, response));
        let outcomes = launch.await.expect("launch task").expect("ensemble launch");
        let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
        let records = load_agent_run(&path).expect("worker records");
        let scenario =
            format!("fixture={fixture_kind:?}, workflow={workflow:?}, dismiss={dismiss}");
        assert_eq!(
            outcomes[0].status,
            if workflow == EnsembleWorkflow::Plan {
                AgentRunStatus::AwaitingConfirmation
            } else {
                AgentRunStatus::Completed
            },
            "{scenario}: {}",
            fixture_diagnostics(&outcomes[0], &records)
        );
        assert!(outcomes[0].confirmation.is_none());
        assert_eq!(outcomes[0].report, "question answered report");

        // The peer closes this capture before reporting completion. Consume it
        // so every round must produce fresh evidence, even without stderr logs.
        let response_bytes = std::fs::read(&response_path).unwrap_or_else(|error| {
            panic!(
                "{scenario}: missing peer response: {error}; {}",
                fixture_diagnostics(&outcomes[0], &records)
            )
        });
        std::fs::remove_file(&response_path).expect("consume peer response before the next round");
        let response: serde_json::Value =
            serde_json::from_slice(&response_bytes).unwrap_or_else(|error| {
                panic!(
                    "{scenario}: invalid peer response: {error}; {}",
                    fixture_diagnostics(&outcomes[0], &records)
                )
            });
        assert_eq!(response["jsonrpc"], "2.0", "{scenario}");
        assert_eq!(response["id"], 900, "{scenario}");
        assert_eq!(
            response["result"]["action"],
            if dismiss { "decline" } else { "accept" }
        );
        if dismiss {
            assert!(response["result"].get("content").is_none());
        } else if codex {
            assert_eq!(
                response["result"]["content"],
                serde_json::json!({
                    "question_0": "Focused", "question_1": "None of the above",
                    "question_1_note": "Custom scope", "question_2": "Broad"
                })
            );
        } else if native {
            assert_eq!(
                response["result"]["content"],
                serde_json::json!({
                    "question_0": "option_0\r\n", "question_1": NATIVE_OTHER, "question_1_other": "Custom scope",
                    "question_2": ["option_0", NATIVE_OTHER], "question_2_other": "Custom target"
                })
            );
        } else {
            assert_eq!(response["result"]["content"]["choice"], "wire_a\r\n");
            assert_eq!(response["result"]["content"]["confirmed"], true);
            assert_eq!(
                response["result"]["content"]["flags"],
                serde_json::json!(["flag_x", "flag_y"])
            );
            assert_eq!(response["result"]["content"]["name"], "Ada");
            assert!(response["result"]["content"].get("note").is_none());
        }
        let expected_outcome = if dismiss {
            AgentElicitationOutcome::Declined
        } else {
            AgentElicitationOutcome::Accepted
        };
        let (diagnostic, captured) = records
            .iter()
            .find_map(|record| match record {
                AgentRunTranscriptRecord::Event {
                    event:
                        event @ AgentRunEvent::Elicitation {
                            outcome, decision, ..
                        },
                } if *outcome == expected_outcome => Some((
                    serde_json::to_string(event).expect("diagnostic JSON"),
                    decision.clone(),
                )),
                _ => None,
            })
            .expect("safe elicitation diagnostic");
        assert!(!diagnostic.contains("wire_a"));
        let event: serde_json::Value = serde_json::from_str(&diagnostic).unwrap();
        assert_eq!(event["field_count"], field_count);
        if native || codex {
            assert!(!diagnostic.contains(NATIVE_OTHER));
            assert!(!diagnostic.contains("_other"));
            assert!(!diagnostic.contains("None of the above"));
            assert!(!diagnostic.contains("_note"));
        } else {
            assert!(!diagnostic.contains("Keep it narrow"));
        }
        if dismiss {
            assert!(captured.is_none());
            assert!(outcomes[0].user_decisions.is_empty());
            assert!(outcomes[0].decision_ids.is_empty());
        } else {
            let captured = captured.expect("accepted display decision is durable");
            assert_eq!(captured.request_id, request.id);
            assert_eq!(captured.answers.len(), field_count);
            for answer in &captured.answers {
                assert_eq!(
                    answer.decision_id,
                    AgentUserDecisionId::from_question(&request.id, &answer.question_id)
                );
            }
            assert!(
                captured
                    .answers
                    .iter()
                    .all(|answer| !answer.header.contains('\r') && !answer.question.contains('\r'))
            );
            let accepted = records
                .iter()
                .position(|record| {
                    matches!(
                        record,
                        AgentRunTranscriptRecord::Event {
                            event: AgentRunEvent::Elicitation {
                                outcome: AgentElicitationOutcome::Accepted,
                                ..
                            }
                        }
                    )
                })
                .unwrap();
            let response_sent = records
                .iter()
                .position(|record| match record {
                    AgentRunTranscriptRecord::Event {
                        event:
                            AgentRunEvent::Protocol {
                                direction: AgentProtocolDirection::ClientToAgent,
                                json,
                            },
                    } => serde_json::from_str::<serde_json::Value>(json).is_ok_and(|value| {
                        value["id"] == 900 && value["result"]["action"] == "accept"
                    }),
                    _ => false,
                })
                .unwrap();
            assert!(
                accepted < response_sent,
                "decision must be synced before accept is sent"
            );
            if native || codex {
                assert_eq!(
                    captured
                        .answers
                        .iter()
                        .map(|answer| answer.question_id.as_str())
                        .collect::<Vec<_>>(),
                    ["question_0", "question_1", "question_2"]
                );
                assert_eq!(
                    captured.answers[0].answer,
                    AgentUserDecisionValue::String {
                        value: "Focused".into()
                    }
                );
                assert_eq!(
                    captured.answers[1].answer,
                    AgentUserDecisionValue::String {
                        value: "Custom scope".into()
                    }
                );
                assert_eq!(
                    captured.answers[2].answer,
                    if codex {
                        AgentUserDecisionValue::String {
                            value: "Broad".into(),
                        }
                    } else {
                        AgentUserDecisionValue::Strings {
                            values: vec!["Focused".into(), "Custom target".into()],
                        }
                    }
                );
            } else {
                assert_eq!(
                    captured.answers[0].answer,
                    AgentUserDecisionValue::String {
                        value: "Visible A".to_string()
                    }
                );
                assert_eq!(
                    captured.answers[1].answer,
                    AgentUserDecisionValue::String {
                        value: "Yes".to_string()
                    }
                );
                assert_eq!(
                    captured.answers[2].answer,
                    AgentUserDecisionValue::Strings {
                        values: vec!["Flag X".to_string(), "Flag Y".to_string()]
                    }
                );
                assert_eq!(
                    captured.answers[3].answer,
                    AgentUserDecisionValue::String {
                        value: "Ada".to_string()
                    }
                );
                assert_eq!(captured.answers[4].answer, AgentUserDecisionValue::Skipped);
                assert!(diagnostic.contains("Ada"));
            }
            assert_eq!(outcomes[0].user_decisions, vec![captured.clone()]);
            assert_eq!(
                outcomes[0].decision_ids,
                captured.decision_ids().cloned().collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn concurrent_workers_queue_one_global_question_at_a_time() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("queued_question_acp.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys

label = os.environ["AGENT_LABEL"]
session_id = "session-" + label

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":label,"version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":session_id,"configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":901,"method":"elicitation/create","params":{"mode":"form","sessionId":session_id,"message":"Question from " + label,"requestedSchema":{"type":"object","properties":{"answer":{"type":"string","minLength":1}},"required":["answer"]}}})
        response = json.loads(sys.stdin.readline())
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"report-" + label},"messageId":"message"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("queued question ACP script");
    let make_agent = |label: &str| EnsembleAgentConfig {
        label: label.to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::from([("AGENT_LABEL".to_string(), label.to_string())]),
        login_hint: String::new(),
    };
    let config = EnsembleConfig {
        plan_agents: vec!["first".to_string(), "second".to_string()],
        review_agents: vec!["first".to_string(), "second".to_string()],
        max_concurrent_agents: 2,
        review_startup_timeout_seconds: 5,
        review_turn_timeout_seconds: 5,
        cancel_grace_seconds: 1,
        max_synthesis_bytes_per_agent: 4096,
        agents: BTreeMap::from([
            ("first".to_string(), make_agent("First ACP")),
            ("second".to_string(), make_agent("Second ACP")),
        ]),
    };
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(512);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(config, &workspace, logs, questions.requester)
        .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review concurrently".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptors"),
    };
    let launched = supervisor.clone();
    let launch_events = events.clone();
    let launch = tokio::spawn(async move {
        launched
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start,
                    resume: false,
                },
                launch_events,
                TurnContext::new(
                    TurnId::new(21),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });

    let first = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("first queued question");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), receive_question(&mut receiver))
            .await
            .is_err(),
        "a second global modal appeared before the first resolved"
    );
    assert!(questions.responder.respond(
        &first.id,
        QuestionResponse::Answered {
            answers: vec![zevria_foundation::QuestionAnswer {
                id: "answer".to_string(),
                answer: Some(QuestionAnswerValue::String("first answer".to_string())),
            }],
        }
    ));
    let second = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("second queued question");
    assert_ne!(first.source_label, second.source_label);
    assert!(questions.responder.respond(
        &second.id,
        QuestionResponse::Answered {
            answers: vec![zevria_foundation::QuestionAnswer {
                id: "answer".to_string(),
                answer: Some(QuestionAnswerValue::String("second answer".to_string())),
            }],
        }
    ));
    let outcomes = launch.await.expect("launch task").expect("ensemble launch");
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.status == AgentRunStatus::Completed)
    );
}

#[tokio::test]
async fn cancelling_a_queued_worker_never_opens_its_question_modal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("cancel_queued_question_acp.py");
    std::fs::write(
            &script,
            r#"import json
import os
import sys
import time

label = os.environ["AGENT_LABEL"]
session_id = "session-" + label
delay = float(os.environ.get("QUESTION_DELAY", "0"))
violate = os.environ.get("VIOLATE_MODE") == "1"

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"},{"value":"build","name":"Build"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":label,"version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":session_id,"configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        time.sleep(delay)
        send({"jsonrpc":"2.0","id":901,"method":"elicitation/create","params":{"mode":"form","sessionId":session_id,"message":"Question from " + label,"requestedSchema":{"type":"object","properties":{"answer":{"type":"string","minLength":1}},"required":["answer"]}}})
        if violate:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"sessionUpdate":"current_mode_update","currentModeId":"build"}}})
        while True:
            response = json.loads(sys.stdin.readline())
            if response.get("id") == 901:
                break
        sys.stderr.write("queued-response:" + json.dumps(response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        if not violate:
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"report-" + label},"messageId":"message"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("queued cancellation ACP script");
    let make_agent = |label: &str, delay: &str, violate: bool| EnsembleAgentConfig {
        label: label.to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::from([
            ("AGENT_LABEL".to_string(), label.to_string()),
            ("QUESTION_DELAY".to_string(), delay.to_string()),
            (
                "VIOLATE_MODE".to_string(),
                if violate { "1" } else { "0" }.to_string(),
            ),
        ]),
        login_hint: String::new(),
    };
    let config = EnsembleConfig {
        plan_agents: vec!["first".to_string(), "queued".to_string()],
        review_agents: vec!["first".to_string(), "queued".to_string()],
        max_concurrent_agents: 2,
        review_startup_timeout_seconds: 5,
        review_turn_timeout_seconds: 5,
        cancel_grace_seconds: 1,
        max_synthesis_bytes_per_agent: 4096,
        agents: BTreeMap::from([
            ("first".to_string(), make_agent("First ACP", "0", false)),
            ("queued".to_string(), make_agent("Queued ACP", "0.2", true)),
        ]),
    };
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(512);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(config, &workspace, logs, questions.requester)
        .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "cancel one queued question".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptors"),
    };
    let launched = supervisor.clone();
    let launch_events = events.clone();
    let launch = tokio::spawn(async move {
        launched
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start,
                    resume: false,
                },
                launch_events,
                TurnContext::new(
                    TurnId::new(22),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });

    let active = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("first question appears");
    assert_eq!(active.source_label.as_deref(), Some("First ACP"));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), receive_question(&mut receiver))
            .await
            .is_err(),
        "the cancelled queued worker opened a modal"
    );
    assert!(questions.responder.respond(
        &active.id,
        QuestionResponse::Answered {
            answers: vec![zevria_foundation::QuestionAnswer {
                id: "answer".to_string(),
                answer: Some(QuestionAnswerValue::String("continue".to_string())),
            }],
        }
    ));

    let outcomes = launch.await.expect("launch task").expect("ensemble launch");
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    assert_eq!(outcomes[1].status, AgentRunStatus::Failed);
    assert!(
        outcomes[1]
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("switched away"))
    );
    while let Ok(update) = receiver.try_recv() {
        if let zevria_session_api::SessionUpdate::Lifecycle(SessionEvent::QuestionAsked {
            request,
            ..
        }) = update
        {
            assert_ne!(request.source_label.as_deref(), Some("Queued ACP"));
        }
    }
}

#[tokio::test]
async fn peer_cancel_request_returns_standard_error_and_closes_any_modal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("cancel_question_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"cancel-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"cancel-session","configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":900,"method":"elicitation/create","params":{"mode":"form","sessionId":"cancel-session","message":"Unsupported question","requestedSchema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}}})
        while True:
            unsupported = json.loads(sys.stdin.readline())
            if unsupported.get("id") == 900:
                break
        sys.stderr.write("unsupported-response:" + json.dumps(unsupported, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","id":901,"method":"elicitation/create","params":{"mode":"form","sessionId":"cancel-session","message":"Cancelled question","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}}})
        send({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":901}})
        while True:
            response = json.loads(sys.stdin.readline())
            if response.get("id") == 901:
                break
        sys.stderr.write("cancel-response:" + json.dumps(response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"cancel-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"cancel handled"},"messageId":"message"}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("cancel question ACP script");
    let agent = EnsembleAgentConfig {
        label: "Cancel ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(256);
    let questions = question_channels(events.clone());
    let requester = questions.requester.clone();
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        questions.requester,
    )
    .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "cancel the question".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptor"),
    };
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start: start.clone(),
                resume: false,
            },
            events.clone(),
            TurnContext::new(
                TurnId::new(31),
                SessionMode::Build,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("launch succeeds");
    assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
    let mut asked = Vec::new();
    let mut closed = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        if let zevria_session_api::SessionUpdate::Lifecycle(event) = update {
            match event {
                SessionEvent::QuestionAsked { request, .. } => asked.push(request.id),
                SessionEvent::QuestionClosed { request_id, .. } => closed.push(request_id),
                _ => {}
            }
        }
    }
    assert!(asked.iter().all(|request_id| closed.contains(request_id)));

    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("worker records");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Elicitation {
                outcome: AgentElicitationOutcome::RequestCancelled,
                ..
            }
        }
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("cancel-response") && text.contains("-32800")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("unsupported-response") && text.contains("decline")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Elicitation {
                outcome: AgentElicitationOutcome::Unsupported,
                ..
            }
        }
    )));

    let fresh = QuestionRequest {
        id: QuestionRequestId::new("fresh-after-cancel"),
        questions: vec![QuestionPrompt {
            id: "answer".to_string(),
            header: "Answer".to_string(),
            question: "Still available?".to_string(),
            options: Vec::new(),
            kind: QuestionPromptKind::Text {
                min_length: None,
                max_length: None,
            },
            required: true,
            default: None,
        }],
        source_label: Some("Probe".to_string()),
        dismissible: true,
    };
    let waiter = tokio::spawn(async move {
        requester
            .ask_request(
                fresh,
                TurnContext::new(
                    TurnId::new(32),
                    SessionMode::Build,
                    CancellationToken::new(),
                ),
            )
            .await
    });
    let request = receive_question(&mut receiver).await;
    assert!(
        questions
            .responder
            .respond(&request.id, QuestionResponse::Dismissed)
    );
    assert_eq!(
        waiter.await.expect("waiter task").expect("broker response"),
        QuestionResponse::Dismissed
    );
}

#[tokio::test]
async fn root_cancellation_cancels_active_elicitation_and_releases_modal() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("root_cancel_question_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"root-cancel-acp","version":"1"}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":request_id,"result":{"sessionId":"root-cancel-session","configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","id":902,"method":"elicitation/create","params":{"mode":"form","sessionId":"root-cancel-session","message":"Wait for cancellation","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}}})
        while True:
            response = json.loads(sys.stdin.readline())
            if response.get("id") == 902:
                break
        sys.stderr.write("root-cancel-response:" + json.dumps(response, separators=(",", ":")) + "\n")
        sys.stderr.flush()
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("root cancellation ACP script");
    let agent = EnsembleAgentConfig {
        label: "Root Cancel ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let (events, mut receiver) = session_event_channel(256);
    let questions = question_channels(events.clone());
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        questions.requester,
    )
    .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "cancel while asking".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("worker descriptor"),
    };
    let cancellation = CancellationToken::new();
    let launched = supervisor.clone();
    let launch_events = events.clone();
    let launch_start = start.clone();
    let child_cancellation = cancellation.clone();
    let launch = tokio::spawn(async move {
        launched
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start: launch_start,
                    resume: false,
                },
                launch_events,
                TurnContext::new(TurnId::new(41), SessionMode::Build, child_cancellation),
            )
            .await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), receive_question(&mut receiver))
        .await
        .expect("active question");
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(zevria_session_api::SessionUpdate::Lifecycle(
                SessionEvent::QuestionClosed { request_id, .. },
            )) = receiver.recv().await
                && request_id == request.id
            {
                break;
            }
        }
    })
    .await
    .expect("question close after root cancellation");
    assert!(
        !questions
            .responder
            .respond(&request.id, QuestionResponse::Dismissed)
    );
    let outcomes = launch.await.expect("launch task").expect("ensemble launch");
    assert_eq!(outcomes[0].status, AgentRunStatus::Cancelled);
    let path = agent_run_path(&logs, &start.run_id, &outcomes[0].descriptor.id);
    let records = load_agent_run(&path).expect("worker records");
    assert!(records.iter().any(|record| matches!(
        record,
        AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Stderr { text }
        } if text.contains("root-cancel-response") && text.contains("cancel")
    )));
}

#[cfg(unix)]
#[tokio::test]
async fn startup_timeout_drops_the_connection_and_kills_the_process_group() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let marker = directory.path().join("orphan-marker");
    let script = directory.path().join("hanging_acp.py");
    std::fs::write(
        &script,
        r#"import os
import subprocess
import sys
import time

sys.stdin.readline()
subprocess.Popen([
    sys.executable,
    "-c",
    "import os,time; time.sleep(2); open(os.environ['ORPHAN_MARKER'],'w').write('orphaned')",
])
time.sleep(60)
"#,
    )
    .expect("hanging ACP script");
    let agent = EnsembleAgentConfig {
        label: "Hanging ACP".to_string(),
        command: PYTHON_COMMAND.to_string(),
        args: vec!["-u".to_string(), script.display().to_string()],
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::from([("ORPHAN_MARKER".to_string(), marker.display().to_string())]),
        login_hint: String::new(),
    };
    let config = EnsembleConfig {
        review_startup_timeout_seconds: 1,
        ..single_agent_config(agent)
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(config, &workspace, logs, test_questions())
        .expect("valid supervisor");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "hang".into(),
        agents: supervisor
            .workers(EnsembleWorkflow::Review)
            .expect("descriptor"),
    };
    let (events, _receiver) = session_event_channel(256);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start,
                resume: false,
            },
            events,
            TurnContext::new(TurnId::new(2), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("supervisor returns after timeout");
    assert_eq!(outcomes[0].status, AgentRunStatus::TimedOut);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !marker.exists(),
        "dropping an ACP connection must kill descendants in its process group"
    );
}

#[tokio::test]
async fn one_shot_plan_completion_is_rejected_even_with_a_repair_marker() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let script = directory.path().join("plan_recovery_repair_acp.py");
    std::fs::write(
            &script,
            r#"import json
import sys

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def options():
    return [{"id":"execution-mode","name":"Execution mode","category":"mode","type":"select","currentValue":"read-only","options":[{"value":"read-only","name":"Read only"}]}]

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":request_id,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},"agentInfo":{"name":"plan-recovery","version":"1"}}})
    elif method == "session/resume":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/set_config_option":
        send({"jsonrpc":"2.0","id":request_id,"result":{"configOptions":options()}})
    elif method == "session/prompt":
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"recoverable-plan-session","update":{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"repaired-plan","content":"Recovered plan\n\n- Reuse the durable session."}}}})
        send({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})
"#,
        )
        .expect("recovery ACP script");
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(fake_stdio_agent(&script, BTreeMap::new())),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");

    for (ordinal, repair_already_marked) in [false, true].into_iter().enumerate() {
        let descriptor = supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("descriptor")
            .remove(0);
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "recover the proof".into(),
            agents: vec![descriptor.clone()],
        };
        let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
        let mut writer = AgentRunTranscriptWriter::create(
            path.clone(),
            AgentRunTranscriptHeader {
                version: AGENT_RUN_TRANSCRIPT_VERSION,
                ensemble_run_id: start.run_id.clone(),
                workflow: start.workflow,
                descriptor: descriptor.clone(),
                prompt: start.prompt.clone(),
            },
        )
        .expect("worker log");
        writer
            .append(&AgentRunTranscriptRecord::Outcome {
                outcome: AgentRunOutcome {
                    confirmation: None,
                    descriptor: descriptor.clone(),
                    status: AgentRunStatus::Completed,
                    report: "legacy prose only".to_string(),
                    plan: None,
                    partial: true,
                    failure: None,
                    usage: None,
                    acp_session_id: Some("recoverable-plan-session".to_string()),
                    user_decisions: Vec::new(),
                    decision_ids: Vec::new(),
                    unavailable_decisions: Vec::new(),
                },
            })
            .expect_err("current writer refuses proof-only completion");
        if repair_already_marked {
            writer
                .append(&AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Prompt {
                        text: semantic_repair_prompt(EnsembleWorkflow::Plan),
                        continuation: true,
                        repair: Some(AgentRunRepair::MissingPlanProof),
                    },
                })
                .expect("durable repair boundary");
        }
        drop(writer);

        let (events, _receiver) = session_event_channel(64);
        let original = std::fs::read(&path).unwrap();
        let error = supervisor
            .launch(
                EnsembleLaunchRequest {
                    start,
                    resume: true,
                },
                events,
                TurnContext::new(
                    TurnId::new(300 + ordinal as u64),
                    SessionMode::Plan,
                    CancellationToken::new(),
                ),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("interactive review"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let records = load_agent_run(&path).unwrap();
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, AgentRunTranscriptRecord::Outcome { .. }))
        );
        assert!(protocol_requests(&records).is_empty());
    }
}

#[tokio::test]
async fn proof_only_history_and_historical_claude_markers_fail_closed_without_spawn() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let agent = EnsembleAgentConfig {
        label: "Historical Plan".to_string(),
        command: directory.path().join("must-not-run").display().to_string(),
        args: Vec::new(),
        plan_mode: Some("read-only".to_string()),
        review_mode: Some("read-only".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");

    for (ordinal, historical_marker, repair_consumed) in
        [(0u64, false, false), (1, false, true), (2, true, false)]
    {
        let descriptor = supervisor
            .workers(EnsembleWorkflow::Plan)
            .expect("descriptor")
            .remove(0);
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Plan,
            prompt: "recover historical plan".into(),
            agents: vec![descriptor.clone()],
        };
        let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
        let mut writer = AgentRunTranscriptWriter::create(
            path.clone(),
            AgentRunTranscriptHeader {
                version: AGENT_RUN_TRANSCRIPT_VERSION,
                ensemble_run_id: start.run_id.clone(),
                workflow: start.workflow,
                descriptor: descriptor.clone(),
                prompt: start.prompt.clone(),
            },
        )
        .expect("worker log");
        if historical_marker {
            let record = AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::AgentMessage {
                    text: "# Historical native plan".to_string(),
                    message_id: Some(CLAUDE_PLAN_HANDOFF_PLAN_ID.to_string()),
                },
            };
            let mut raw = serde_json::to_vec(&record).unwrap();
            raw.push(b'\n');
            std::io::Write::write_all(
                &mut std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
                &raw,
            )
            .unwrap();
        }
        if repair_consumed {
            writer
                .append(&AgentRunTranscriptRecord::Event {
                    event: AgentRunEvent::Prompt {
                        text: "already repaired".to_string(),
                        continuation: true,
                        repair: Some(AgentRunRepair::MissingPlanProof),
                    },
                })
                .expect("repair marker");
        }
        let legacy = AgentRunTranscriptRecord::Outcome {
            outcome: AgentRunOutcome {
                confirmation: None,
                descriptor: descriptor.clone(),
                status: AgentRunStatus::Completed,
                report: "old proof-only report".into(),
                plan: None,
                partial: true,
                failure: None,
                usage: None,
                acp_session_id: None,
                user_decisions: vec![],
                decision_ids: vec![],
                unavailable_decisions: vec![],
            },
        };
        assert!(writer.append(&legacy).is_err());
        drop(writer);
        // Deliberately bypass the current writer to model incompatible
        // complete historical bytes, not recoverable current-format debris.
        let mut bytes = serde_json::to_vec(&legacy).unwrap();
        bytes.push(b'\n');
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            &bytes,
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();

        let (events, _receiver) = session_event_channel(64);
        let result = supervisor.start_review(
            EnsembleLaunchRequest {
                start,
                resume: true,
            },
            vec![WorkerReviewState::new(descriptor)],
            events,
            TurnContext::new(
                TurnId::new(400 + ordinal),
                SessionMode::Plan,
                CancellationToken::new(),
            ),
        );
        assert!(result.is_err());
        assert!(load_agent_run(&path).is_err());
        assert!(AgentRunTranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}

#[tokio::test]
async fn recovery_reuses_a_durable_terminal_worker_without_spawning_it_again() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let agent = EnsembleAgentConfig {
        label: "Recovered ACP".to_string(),
        command: directory
            .path()
            .join("executable-that-must-not-run")
            .display()
            .to_string(),
        args: Vec::new(),
        plan_mode: Some("plan".to_string()),
        review_mode: Some("default".to_string()),
        plan_config_options: BTreeMap::new(),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::new(),
        login_hint: String::new(),
    };
    let logs = directory.path().join("agent-runs");
    let supervisor = EnsembleSupervisor::new(
        single_agent_config(agent),
        &workspace,
        logs.clone(),
        test_questions(),
    )
    .expect("valid supervisor");
    let descriptor = supervisor
        .workers(EnsembleWorkflow::Review)
        .expect("descriptor")
        .remove(0);
    assert_eq!(descriptor.safe_mode, "default");
    let start = zevria_workflow::EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "recover me".into(),
        agents: vec![descriptor.clone()],
    };
    let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
    let header = AgentRunTranscriptHeader {
        version: AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: start.run_id.clone(),
        workflow: start.workflow,
        descriptor: descriptor.clone(),
        prompt: start.prompt.clone(),
    };
    let expected = AgentRunOutcome {
        confirmation: None,
        descriptor,
        status: AgentRunStatus::Completed,
        report: "already durable".to_string(),
        plan: None,
        partial: false,
        failure: None,
        usage: None,
        acp_session_id: Some("old-session".to_string()),
        user_decisions: Vec::new(),
        decision_ids: Vec::new(),
        unavailable_decisions: Vec::new(),
    };
    let mut writer = AgentRunTranscriptWriter::create(path, header).expect("worker log");
    writer
        .append(&AgentRunTranscriptRecord::Outcome {
            outcome: expected.clone(),
        })
        .expect("durable outcome");
    drop(writer);

    let (events, _receiver) = session_event_channel(64);
    let outcomes = supervisor
        .observe_review_rounds(
            EnsembleLaunchRequest {
                start,
                resume: true,
            },
            events,
            TurnContext::new(TurnId::new(3), SessionMode::Build, CancellationToken::new()),
        )
        .await
        .expect("terminal outcome is reused");
    assert_eq!(outcomes, [expected]);
}
