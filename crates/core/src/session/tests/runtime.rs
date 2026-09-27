//! Characterization of the command-channel boundary, not just individual turns.

use super::*;
use std::{cell::Cell, time::Duration};
use tokio::sync::oneshot;
use zevria_instructions::skill::SkillManagementRequest;
use zevria_instructions::skill::SkillManagementResult;
use zevria_model::models::ModelManagementRequest;
use zevria_model::models::ModelManagementResult;
use zevria_model::models::ModelSelectionPreview;
use zevria_model::models::ModelSelectionScope as Scope;
use zevria_transcript::test_support::TranscriptRewriteBlocker;

const TIMEOUT: Duration = Duration::from_secs(5);

#[path = "maintenance.rs"]
mod maintenance;
#[path = "mode_runtime.rs"]
mod mode_runtime;
#[path = "reasoning.rs"]
mod reasoning;

/// Each provider call announces that it is active and waits for an explicit
/// response. Dropping a cancelled call also releases its in-flight guard.
struct GatedProvider {
    // Exercise the public Send-only provider contract, not an implicit Sync bound.
    _not_sync: Cell<()>,
    calls: mpsc::UnboundedSender<GatedCall>,
    in_flight: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
}

struct GatedCall {
    turn: TurnContext,
    request: CapturedRequest,
    response: oneshot::Sender<anyhow::Result<Message>>,
}

struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        assert_eq!(self.0.fetch_sub(1, Ordering::SeqCst), 1);
    }
}

impl GatedProvider {
    fn new() -> (Self, mpsc::UnboundedReceiver<GatedCall>) {
        let (calls, receiver) = mpsc::unbounded_channel();
        (
            Self {
                _not_sync: Cell::new(()),
                calls,
                in_flight: Arc::new(AtomicUsize::new(0)),
                cancellations: Arc::new(AtomicUsize::new(0)),
            },
            receiver,
        )
    }
}

impl ModelProvider for GatedProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            assert_eq!(self.in_flight.fetch_add(1, Ordering::SeqCst), 0);
            let _in_flight = InFlight(self.in_flight.clone());
            let (response, receive) = oneshot::channel();
            assert!(
                self.calls
                    .send(GatedCall {
                        turn: progress.turn().clone(),
                        request: CapturedRequest::of(&request),
                        response,
                    })
                    .is_ok()
            );
            receive
                .await
                .expect("test must release the provider call")
                .and_then(ModelResponse::plain)
        })
    }

    fn reset(&mut self) {}

    fn cancel(&mut self) {
        self.cancellations.fetch_add(1, Ordering::SeqCst);
    }
}

fn engine<P: ModelProvider>(provider: P, transcript: TranscriptWriter) -> SessionEngine<P> {
    SessionEngine::new(
        provider,
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine")
}

fn submit(text: &str) -> SessionCommand {
    SessionCommand::Turn(crate::session::TurnCommand::Submit {
        behavior: zevria_foundation::RequestBehavior::Standard,
        text: text.into(),
        mode: SessionMode::Build,
    })
}

struct RunningSession {
    commands: Option<mpsc::UnboundedSender<SessionCommand>>,
    receiver: SessionEventReceiver,
    task: tokio::task::JoinHandle<Result<(), SessionReplayError>>,
    lifecycle: Vec<SessionEvent>,
}

impl RunningSession {
    fn start<P: ModelProvider>(
        engine: SessionEngine<P>,
        queued: impl IntoIterator<Item = SessionCommand>,
        events: SessionEventSender,
        receiver: SessionEventReceiver,
    ) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        for command in queued {
            commands.send(command).expect("queue before startup");
        }
        Self {
            commands: Some(commands),
            receiver,
            task: tokio::spawn(engine.run(command_rx, events)),
            lifecycle: Vec::new(),
        }
    }

    fn send(&self, command: SessionCommand) {
        self.commands
            .as_ref()
            .expect("open commands")
            .send(command)
            .expect("running engine");
    }

    async fn until(&mut self, matches: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let event = recv_event(&mut self.receiver)
                    .await
                    .expect("lifecycle event");
                self.lifecycle.push(event.clone());
                if matches(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("expected lifecycle event must arrive")
    }

    async fn call(&mut self, calls: &mut mpsc::UnboundedReceiver<GatedCall>) -> GatedCall {
        let call = tokio::time::timeout(TIMEOUT, async {
            loop {
                tokio::select! {
                    call = calls.recv() => break call.expect("provider call"),
                    event = recv_event(&mut self.receiver) => {
                        self.lifecycle.push(event.expect("engine remains running"));
                    }
                }
            }
        })
        .await
        .expect("provider call must start");
        self.lifecycle
            .extend(collect_events(&mut self.receiver).await);
        call
    }

    /// A correlated query proves idle dispatch is responsive without a startup event.
    async fn idle_fence(&mut self, id: &str) {
        self.send(SessionCommand::Manage(
            crate::session::ManagementCommand::Skills {
                request_id: id.into(),
                request: SkillManagementRequest::List {
                    query: String::new(),
                },
            },
        ));
        self.until(|event| matches!(event,
            SessionEvent::SkillsResult { request_id, result: SkillManagementResult::View { .. } }
                if request_id == id
        )).await;
    }

    /// The correlated reply is a FIFO fence: every earlier command has been
    /// handled while the provider remains gated, rather than merely sent.
    async fn busy_fence(&mut self, request_id: &str) {
        self.send(SessionCommand::Manage(
            crate::session::ManagementCommand::Models {
                request_id: request_id.into(),
                request: ModelManagementRequest::List {
                    scope: Scope::SessionAndDefault,
                    mode: SessionMode::Build,
                },
            },
        ));
        self.expect_model_busy(request_id).await;
    }

    async fn expect_model_busy(&mut self, id: &str) {
        let event = self
            .until(|event| {
                matches!(event,
                    SessionEvent::ModelsResult { request_id, .. } if request_id == id
                )
            })
            .await;
        assert_eq!(
            event,
            SessionEvent::ModelsResult {
                request_id: id.into(),
                result: ModelManagementResult::rejected(
                    "busy",
                    "model changes require an idle root session; request was not queued",
                ),
            }
        );
    }

    async fn completed(&mut self, id: TurnId) {
        self.until(|event| {
            matches!(event,
                SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == id
            )
        })
        .await;
    }

    async fn finish(mut self) -> Vec<SessionEvent> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                tokio::select! {
                    result = &mut self.task => {
                        result.expect("engine task must not panic").expect("valid replay");
                        self.lifecycle.extend(collect_events(&mut self.receiver).await);
                        return self.lifecycle;
                    }
                    Some(event) = recv_event(&mut self.receiver) => self.lifecycle.push(event),
                }
            }
        })
        .await
        .expect("engine must finish its cleanup and exit")
    }

    async fn shutdown(self) -> Vec<SessionEvent> {
        self.send(SessionCommand::Control(
            crate::session::ControlCommand::Shutdown,
        ));
        self.finish().await
    }
}

fn assert_call(call: &GatedCall, id: u64, prompt: &str) {
    assert_eq!(call.turn.id, TurnId::new(id));
    assert_eq!(call.request.prompt, Message::user(prompt));
    assert!(!call.turn.is_cancelled());
}

fn answer(call: GatedCall, text: &str) {
    call.response
        .send(Ok(Message::assistant(text)))
        .expect("active provider call");
}

fn assert_turns_do_not_overlap(events: &[SessionEvent], expected: &[u64]) {
    let mut active = None;
    let mut started = Vec::new();
    for event in events {
        match event {
            SessionEvent::TurnStarted { turn_id, .. } => {
                assert_eq!(active.replace(*turn_id), None, "turns must not overlap");
                started.push(turn_id.get());
            }
            SessionEvent::TurnCompleted { turn_id, .. }
            | SessionEvent::TurnFailed { turn_id, .. }
            | SessionEvent::TurnCancelled { turn_id } => {
                assert_eq!(active.take(), Some(*turn_id));
            }
            _ => {}
        }
    }
    assert_eq!(active, None);
    assert_eq!(started, expected);
}

fn unfinished_review_fixture() -> (EnsembleStart, Vec<TranscriptItem>) {
    let start = ensemble_start_fixture(
        "runtime-recovery",
        EnsembleWorkflow::Review,
        "recover review",
    );
    let items = vec![
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: start.run_id.clone(),
            synthesis_input: Message::user("durable review evidence"),
            agents: vec![AgentRunSummary {
                descriptor: start.agents[0].clone(),
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
        TranscriptItem::Message(Message::assistant("already durable review synthesis")),
    ];
    (start, items)
}

fn assert_no_plan_events(events: &[SessionEvent]) {
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::PlanStateChanged { .. }
                | SessionEvent::PlanProjectionWarning { .. }
                | SessionEvent::PlanHandoffStarted { .. }
                | SessionEvent::FreshPlanHandoffRequested { .. }
        )),
        "unexpected Plan lifecycle: {events:?}"
    );
}

#[tokio::test]
async fn fresh_build_startup_has_no_plan_lifecycle_or_filesystem_work() {
    let (_directory, transcript) = test_transcript();
    let workspace = tempfile::tempdir().unwrap();
    let plans = workspace.path().join("plans");
    let (provider, mut calls) = GatedProvider::new();
    let engine = engine(provider, transcript).with_plans_dir(plans.clone());
    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    let (events, receiver) = session_event_channel(1);
    let mut run = RunningSession::start(engine, [submit("build it")], events, receiver);
    let call = run.call(&mut calls).await;
    assert_call(&call, 1, "build it");
    assert!(matches!(
        run.lifecycle.first(),
        Some(SessionEvent::TurnStarted { .. })
    ));
    answer(call, "built");
    run.completed(TurnId::new(1)).await;
    let lifecycle = run.shutdown().await;
    assert_turns_do_not_overlap(&lifecycle, &[1]);
    assert_no_plan_events(&lifecycle);
    assert!(!plans.exists());
}

#[tokio::test]
async fn build_commands_publish_real_restored_workflow_transitions_without_reading_markdown() {
    for approve in [false, true] {
        let (artifact, items) = if approve {
            ready_plan_fixture()
        } else {
            revising_plan_fixture()
        };
        let (_directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let handoff = PlanHandoff::new(artifact.clone(), transcript.session_id());
        let workspace = tempfile::tempdir().unwrap();
        let plans = workspace.path().join("plans");
        let projection = plans.join(transcript.session_id()).join(format!(
            "{}-durable-approval-workflow.md",
            artifact.version.id
        ));
        std::fs::create_dir_all(projection.parent().unwrap()).unwrap();
        std::fs::write(&projection, "manual edit must not enter Build input").unwrap();
        let (provider, mut calls) = GatedProvider::new();
        let engine = engine(provider, transcript)
            .with_fixture(items)
            .unwrap()
            .with_plans_dir(plans);
        assert_eq!(engine.plan_state().unwrap().artifact(), Some(&artifact));
        let (events, receiver) = session_event_channel(1);
        let command = if approve {
            SessionCommand::Turn(crate::session::TurnCommand::ResolvePlan {
                expected: artifact.version,
                decision: PlanDecision::ImplementCurrent,
            })
        } else {
            submit("build instead")
        };
        let mut run = RunningSession::start(engine, [command], events, receiver);
        let call = run.call(&mut calls).await;
        assert_eq!(call.turn.mode, SessionMode::Build);
        assert_eq!(call.turn.id, TurnId::new(1));
        assert_eq!(
            call.request.prompt,
            if approve {
                handoff.prompt.clone()
            } else {
                Message::user("build instead")
            }
        );
        if approve {
            assert!(run.lifecycle.iter().any(|event| matches!(event,
                SessionEvent::PlanHandoffStarted { handoff: actual, .. } if actual == &handoff
            )));
        }
        answer(call, "built");
        run.completed(TurnId::new(1)).await;
        let lifecycle = run.shutdown().await;
        let transitions = lifecycle
            .iter()
            .filter_map(|event| match event {
                SessionEvent::PlanStateChanged { state } => Some(state.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            transitions,
            vec![if approve {
                PlanWorkflowState::Resolved {
                    artifact,
                    resolution: PlanResolution::ImplementedCurrent,
                }
            } else {
                PlanWorkflowState::Idle
            }],
            "only the actual Build-driven transition should publish"
        );
        assert!(
            !lifecycle
                .iter()
                .any(|event| matches!(event, SessionEvent::PlanProjectionWarning { .. }))
        );
        assert_eq!(
            std::fs::read_to_string(&projection).unwrap(),
            "manual edit must not enter Build input"
        );
    }
}

#[tokio::test]
async fn restored_ready_does_not_recreate_missing_projections_or_warn_about_blocked_paths() {
    for blocked in [false, true] {
        let (artifact, items) = ready_plan_fixture();
        let (_directory, mut transcript) = test_transcript();
        persist_fixture(&mut transcript, &items);
        let workspace = tempfile::tempdir().unwrap();
        let plans = workspace.path().join("plans");
        if blocked {
            std::fs::write(&plans, "not a directory").unwrap();
        }
        let provider = ScriptedProvider::new([]);
        let requests = provider.requests.clone();
        let engine = engine(provider, transcript)
            .with_fixture(items)
            .unwrap()
            .with_plans_dir(plans.clone());
        assert_eq!(
            engine.plan_state().unwrap(),
            &PlanWorkflowState::Ready { artifact }
        );
        let (events, receiver) = session_event_channel(1);
        let mut run = RunningSession::start(engine, [], events, receiver);
        run.idle_fence("restored-ready").await;
        let lifecycle = run.shutdown().await;
        assert_eq!(lifecycle.len(), 1, "only the explicit query should publish");
        assert_no_plan_events(&lifecycle);
        assert!(requests.lock().unwrap().is_empty());
        if blocked {
            assert_eq!(std::fs::read_to_string(plans).unwrap(), "not a directory");
        } else {
            assert!(!plans.exists());
        }
    }
}

#[tokio::test]
async fn fresh_and_restored_idle_exit_without_an_event_consumer() {
    for restored in [false, true] {
        for explicit in [false, true] {
            let (_, items) = ready_plan_fixture();
            let items = if restored { items } else { Vec::new() };
            let (_directory, mut transcript) = test_transcript();
            persist_fixture(&mut transcript, &items);
            let path = transcript.path().to_path_buf();
            let before = std::fs::read(&path).unwrap();
            let engine = engine(ScriptedProvider::new([]), transcript)
                .with_fixture(items)
                .unwrap();
            let (events, mut receiver) = session_event_channel(1);
            let marker = SessionEvent::SkillsResult {
                request_id: "unconsumed-lifecycle-marker".into(),
                result: SkillManagementResult::error("marker", "keep the queue full"),
            };
            events.send(marker.clone()).await.unwrap();
            let (commands, command_rx) = mpsc::unbounded_channel();
            if explicit {
                commands
                    .send(SessionCommand::Control(
                        crate::session::ControlCommand::Shutdown,
                    ))
                    .unwrap();
            } else {
                drop(commands);
            }
            tokio::time::timeout(TIMEOUT, engine.run(command_rx, events))
                .await
                .expect("idle exit must not wait for a lifecycle consumer")
                .unwrap();
            assert_eq!(collect_events(&mut receiver).await, vec![marker]);
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}

#[tokio::test]
async fn restored_plan_is_available_before_run_without_startup_events_or_projection_repair() {
    let (artifact, items) = revising_plan_fixture();
    let (_directory, mut transcript) = test_transcript();
    persist_fixture(&mut transcript, &items);
    let workspace = tempfile::tempdir().expect("workspace");
    let plans = workspace.path().join("plans");
    let projection = plans.join(transcript.session_id()).join(format!(
        "{}-durable-approval-workflow.md",
        artifact.version.id
    ));
    std::fs::create_dir_all(projection.parent().expect("projection parent")).unwrap();
    std::fs::write(&projection, "stale manual edit").unwrap();
    let (provider, mut calls) = GatedProvider::new();
    let engine = engine(provider, transcript)
        .with_fixture(items)
        .unwrap()
        .with_plans_dir(plans);
    assert_eq!(
        engine.plan_state().unwrap(),
        &PlanWorkflowState::Planning {
            id: artifact.version.id,
            previous: Some(artifact.clone()),
        }
    );
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(
        engine,
        [SessionCommand::Turn(crate::session::TurnCommand::Submit {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "revise this plan".into(),
            mode: SessionMode::Plan,
        })],
        events,
        receiver,
    );

    let call = run.call(&mut calls).await;
    assert_call(&call, 1, "revise this plan");
    assert_eq!(call.turn.mode, SessionMode::Plan);
    assert!(matches!(
        run.lifecycle.first(),
        Some(SessionEvent::TurnStarted { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&projection).unwrap(),
        "stale manual edit"
    );
    answer(call, "revision in progress");
    run.completed(TurnId::new(1)).await;
    let lifecycle = run.shutdown().await;
    assert_turns_do_not_overlap(&lifecycle, &[1]);
    assert_no_plan_events(&lifecycle);
    assert_eq!(
        std::fs::read_to_string(projection).unwrap(),
        "stale manual edit"
    );
}

#[tokio::test]
async fn unfinished_ensemble_recovery_precedes_queued_frontend_work() {
    let (start, items) = unfinished_review_fixture();
    let (_directory, mut transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    persist_fixture(&mut transcript, &items);
    let launcher = Arc::new(StubEnsembleLauncher::successful(["must not relaunch"]));
    let (provider, mut calls) = GatedProvider::new();
    let engine = engine(provider, transcript)
        .with_fixture(items)
        .unwrap()
        .with_ensemble_launcher(launcher.clone());
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(engine, [submit("queued frontend work")], events, receiver);

    let call = run.call(&mut calls).await;
    assert_call(&call, 2, "queued frontend work");
    assert_no_plan_events(&run.lifecycle);
    let recovery = run
        .lifecycle
        .iter()
        .position(|event| {
            matches!(event,
                SessionEvent::EnsembleStarted { turn_id, start: resumed, resumed: true }
                    if *turn_id == TurnId::new(1) && resumed == &start
            )
        })
        .expect("automatic recovery started");
    let recovered = run
        .lifecycle
        .iter()
        .position(|event| {
            matches!(event,
                SessionEvent::TurnRecovered { turn_id, .. } if *turn_id == TurnId::new(1)
            )
        })
        .expect("automatic recovery finished");
    let frontend = run
        .lifecycle
        .iter()
        .position(|event| {
            matches!(event,
                SessionEvent::TurnStarted { turn_id, .. } if *turn_id == TurnId::new(2)
            )
        })
        .expect("frontend work started");
    assert!(recovery < recovered && recovered < frontend);
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 0);
    let persisted = conversation_records(&zevria_transcript::transcript::load(&path).unwrap());
    assert!(matches!(&persisted[persisted.len() - 2],
        TranscriptItem::Ensemble(EnsembleRecord::Completed { run_id }) if run_id == &start.run_id
    ));
    assert_eq!(
        persisted.last(),
        Some(&TranscriptItem::Message(Message::user(
            "queued frontend work"
        )))
    );
    answer(call, "frontend result");
    run.completed(TurnId::new(2)).await;
    run.shutdown().await;
}

#[tokio::test]
async fn read_only_startup_and_shutdown_skip_recovery_and_projection_writes() {
    let (artifact, mut items) = ready_plan_fixture();
    let (_, recovery) = unfinished_review_fixture();
    items.extend(recovery);
    let (_directory, mut transcript) = test_transcript();
    persist_fixture(&mut transcript, &items);
    let path = transcript.path().to_path_buf();
    let workspace = tempfile::tempdir().expect("workspace");
    let plans = workspace.path().join("plans");
    let projection = plans.join(transcript.session_id()).join(format!(
        "{}-durable-approval-workflow.md",
        artifact.version.id
    ));
    std::fs::create_dir_all(projection.parent().unwrap()).unwrap();
    std::fs::write(&projection, "keep this projection unchanged").unwrap();
    drop(transcript);
    // Inspection must preserve the original bytes, including a missing final newline.
    let mut original = std::fs::read(&path).unwrap();
    assert_eq!(original.pop(), Some(b'\n'));
    std::fs::write(&path, &original).unwrap();
    let provider = ScriptedProvider::new([]);
    let requests = provider.requests.clone();
    let launcher = Arc::new(StubEnsembleLauncher::successful(["must not launch"]));
    let engine = engine(provider, TranscriptWriter::read_only(path.clone()).unwrap())
        .with_fixture(items)
        .unwrap()
        .with_plans_dir(plans)
        .with_ensemble_launcher(launcher.clone());
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(
        engine,
        [SessionCommand::Manage(
            crate::session::ManagementCommand::Skills {
                request_id: "read-only-query".into(),
                request: SkillManagementRequest::List {
                    query: String::new(),
                },
            },
        )],
        events,
        receiver,
    );
    run.until(|event| matches!(event, SessionEvent::SkillsResult { request_id, result: SkillManagementResult::View { .. } } if request_id == "read-only-query")).await;
    let lifecycle = run.shutdown().await;
    assert_eq!(
        lifecycle.len(),
        1,
        "inspection must only answer the explicit query"
    );
    assert!(
        matches!(&lifecycle[0], SessionEvent::SkillsResult { request_id, .. } if request_id == "read-only-query")
    );
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(launcher.launches.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(projection).unwrap(),
        "keep this projection unchanged"
    );
}

#[tokio::test]
async fn queued_submissions_execute_fifo_without_overlap_and_recover_from_provider_failure() {
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let (provider, mut calls) = GatedProvider::new();
    let in_flight = provider.in_flight.clone();
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(
        engine(provider, transcript),
        [submit("first")],
        events,
        receiver,
    );
    let first = run.call(&mut calls).await;
    assert_call(&first, 1, "first");
    run.send(submit("second"));
    run.send(submit("third"));
    run.busy_fence("queued").await;
    assert!(calls.try_recv().is_err());
    assert_eq!(in_flight.load(Ordering::SeqCst), 1);
    answer(first, "first result");

    let second = run.call(&mut calls).await;
    assert_call(&second, 2, "second");
    second
        .response
        .send(Err(anyhow::anyhow!("scripted provider failure")))
        .unwrap();
    let third = run.call(&mut calls).await;
    assert_call(&third, 3, "third");
    assert!(run.lifecycle.iter().any(|event| matches!(event,
        SessionEvent::TurnFailed { turn_id, error } if *turn_id == TurnId::new(2) && error == "scripted provider failure"
    )));
    answer(third, "third result");
    run.completed(TurnId::new(3)).await;
    let lifecycle = run.shutdown().await;
    assert_turns_do_not_overlap(&lifecycle, &[1, 2, 3]);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        conversation_records(&zevria_transcript::transcript::load(&path).unwrap()),
        vec![
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant("first result")),
            TranscriptItem::Message(Message::user("second")),
            TranscriptItem::Error {
                error: "scripted provider failure".into()
            },
            TranscriptItem::Message(Message::user("third")),
            TranscriptItem::Message(Message::assistant("third result")),
        ]
    );
}

#[tokio::test]
async fn idle_and_stale_cancellation_are_harmless_and_cancelled_turns_leave_later_work_usable() {
    let (_directory, transcript) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let cancellations = provider.cancellations.clone();
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(
        engine(provider, transcript),
        [
            SessionCommand::Control(crate::session::ControlCommand::CancelTurn { turn_id: None }),
            SessionCommand::Control(crate::session::ControlCommand::CancelTurn {
                turn_id: Some(TurnId::new(1)),
            }),
            submit("first"),
        ],
        events,
        receiver,
    );
    let first = run.call(&mut calls).await;
    assert_call(&first, 1, "first");
    assert_eq!(cancellations.load(Ordering::SeqCst), 0);
    run.send(submit("second"));
    run.send(SessionCommand::Control(
        crate::session::ControlCommand::CancelTurn {
            turn_id: Some(first.turn.id),
        },
    ));
    let second = run.call(&mut calls).await;
    assert!(first.turn.is_cancelled());
    assert!(first.response.is_closed());
    assert_call(&second, 2, "second");
    run.send(SessionCommand::Control(
        crate::session::ControlCommand::CancelTurn {
            turn_id: Some(first.turn.id),
        },
    ));
    run.busy_fence("stale-cancel-handled").await;
    assert!(!second.turn.is_cancelled());
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    answer(second, "second result");
    run.completed(TurnId::new(2)).await;

    run.send(submit("third"));
    let third = run.call(&mut calls).await;
    assert_call(&third, 3, "third");
    run.send(submit("fourth"));
    run.send(SessionCommand::Control(
        crate::session::ControlCommand::CancelTurn { turn_id: None },
    ));
    let fourth = run.call(&mut calls).await;
    assert!(third.turn.is_cancelled());
    assert!(third.response.is_closed());
    assert_call(&fourth, 4, "fourth");
    answer(fourth, "fourth result");
    run.completed(TurnId::new(4)).await;
    assert_turns_do_not_overlap(&run.shutdown().await, &[1, 2, 3, 4]);
    assert_eq!(cancellations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn queued_cancellation_takes_effect_before_ready_turn_work_is_polled() {
    for turn_id in [None, Some(TurnId::new(1))] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let provider = ScriptedProvider::new([Ok(Message::assistant("done"))]);
        let requests = provider.requests.clone();
        let (events, receiver) = session_event_channel(32);
        let mut run = RunningSession::start(
            engine(provider, transcript),
            [
                submit("must not run"),
                SessionCommand::Control(ControlCommand::CancelTurn { turn_id }),
            ],
            events,
            receiver,
        );
        let terminal = run
            .until(|event| {
                matches!(
                    event,
                    SessionEvent::TurnCancelled { .. }
                        | SessionEvent::TurnCompleted { .. }
                        | SessionEvent::TurnFailed { .. }
                        | SessionEvent::TurnRejected { .. }
                )
            })
            .await;
        assert_eq!(
            terminal,
            SessionEvent::TurnCancelled {
                turn_id: TurnId::new(1),
            }
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            conversation_records(&zevria_transcript::transcript::load(&path).unwrap()).is_empty()
        );

        // Cancellation must not prevent a subsequent turn from running.
        run.send(submit("after cancellation"));
        run.completed(TurnId::new(2)).await;
        run.shutdown().await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].prompt, Message::user("after cancellation"));
    }
}

#[tokio::test]
async fn idle_question_answers_and_maintenance_do_not_allocate_turns() {
    let (_directory, transcript) = test_transcript();
    let provider = ScriptedProvider::new([Ok(Message::assistant("answer"))]);
    let (events, receiver) = session_event_channel(32);
    let questions = question_channels(events.clone());
    let engine = engine(provider, transcript).with_question_responder(questions.responder);
    let mut run = RunningSession::start(engine, [], events, receiver);
    let question = tokio::spawn(async move {
        questions
            .requester
            .ask_request(
                QuestionRequest {
                    id: QuestionRequestId::new("idle-question"),
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
    run.until(|event| matches!(event, SessionEvent::QuestionAsked { .. }))
        .await;
    run.send(SessionCommand::Control(
        crate::session::ControlCommand::AnswerQuestion {
            request_id: QuestionRequestId::new("idle-question"),
            response: QuestionResponse::Dismissed,
        },
    ));
    assert_eq!(
        tokio::time::timeout(TIMEOUT, question)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        QuestionResponse::Dismissed
    );
    run.send(SessionCommand::Manage(
        crate::session::ManagementCommand::Skills {
            request_id: "idle-skills".into(),
            request: SkillManagementRequest::List {
                query: String::new(),
            },
        },
    ));
    run.until(|event| matches!(event, SessionEvent::SkillsResult { request_id, result: SkillManagementResult::View { .. } } if request_id == "idle-skills")).await;
    run.send(SessionCommand::Manage(
        crate::session::ManagementCommand::Models {
            request_id: "idle-models".into(),
            request: ModelManagementRequest::Cancel,
        },
    ));
    run.until(|event| matches!(event, SessionEvent::ModelsResult { request_id, result: ModelManagementResult::Cancelled } if request_id == "idle-models")).await;
    run.send(submit("first actual turn"));
    run.completed(TurnId::new(1)).await;
    assert_turns_do_not_overlap(&run.shutdown().await, &[1]);
}

#[tokio::test]
async fn model_management_requests_during_turns_are_correlated_busy_traffic() {
    let (_directory, transcript) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(
        engine(provider, transcript),
        [submit("active")],
        events,
        receiver,
    );
    let first = run.call(&mut calls).await;
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let preview = ModelSelectionPreview {
            scope,
            request_id: "confirm".into(),
            session_generation: "test".into(),
            generation: 0,
            mode: SessionMode::Build,
            target: zevria_model::models::ModelSelection::new(
                test_profile(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            source: zevria_model::models::ModelSelection::new(
                test_profile(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            revision: "test".into(),
            reason: "test".into(),
        };
        for (id, request) in [
            (
                "list",
                ModelManagementRequest::List {
                    scope,
                    mode: SessionMode::Build,
                },
            ),
            (
                "select",
                ModelManagementRequest::Select {
                    scope,
                    mode: SessionMode::Build,
                    target: zevria_model::models::ModelSelection::new(
                        test_profile(),
                        zevria_foundation::ReasoningLevel::Medium,
                    ),
                    revision: "test".into(),
                },
            ),
            ("confirm", ModelManagementRequest::Confirm { preview }),
            ("cancel", ModelManagementRequest::Cancel),
        ] {
            run.send(SessionCommand::Manage(
                crate::session::ManagementCommand::Models {
                    request_id: id.into(),
                    request,
                },
            ));
            run.expect_model_busy(id).await;
        }
    }
    assert!(
        !first.turn.is_cancelled(),
        "model cancellation is not turn cancellation"
    );
    answer(first, "active result");
    run.completed(TurnId::new(1)).await;
    run.send(submit("later"));
    let second = run.call(&mut calls).await;
    assert_call(&second, 2, "later");
    answer(second, "later result");
    run.completed(TurnId::new(2)).await;
    let lifecycle = run.shutdown().await;
    assert_turns_do_not_overlap(&lifecycle, &[1, 2]);
    assert_eq!(
        lifecycle
            .iter()
            .filter(|event| matches!(event, SessionEvent::ModelsResult { .. }))
            .count(),
        8
    );
}

#[tokio::test]
async fn explicit_shutdown_and_channel_exhaustion_terminate_idle_sessions() {
    for explicit in [true, false] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let (events, receiver) = session_event_channel(1);
        let mut run = RunningSession::start(
            engine(ScriptedProvider::new([]), transcript),
            [],
            events,
            receiver,
        );
        run.idle_fence("idle-ready").await;
        if explicit {
            run.send(SessionCommand::Control(
                crate::session::ControlCommand::Shutdown,
            ));
        } else {
            run.commands.take();
        }
        let lifecycle = run.finish().await;
        assert!(
            matches!(lifecycle.as_slice(), [SessionEvent::SkillsResult { request_id, .. }] if request_id == "idle-ready")
        );
        assert!(
            zevria_transcript::transcript::load(&path)
                .unwrap()
                .iter()
                .all(zevria_transcript::transcript::is_leading_metadata)
        );
    }
}

#[tokio::test]
async fn queued_shutdown_and_channel_exhaustion_cancel_before_ready_work_starts() {
    for explicit in [true, false] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let provider = ScriptedProvider::new([Ok(Message::assistant("must not run"))]);
        let requests = provider.requests.clone();
        let (events, receiver) = session_event_channel(32);
        let mut queued = vec![submit("must not run")];
        if explicit {
            queued.push(SessionCommand::Control(ControlCommand::Shutdown));
            queued.push(submit("must not run either"));
        }
        let mut run = RunningSession::start(engine(provider, transcript), queued, events, receiver);
        if !explicit {
            run.commands.take();
        }
        assert_eq!(
            run.finish().await,
            vec![SessionEvent::TurnCancelled {
                turn_id: TurnId::new(1),
            }]
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            conversation_records(&zevria_transcript::transcript::load(&path).unwrap()).is_empty()
        );
    }
}

#[tokio::test]
async fn ready_work_is_polled_between_immediately_ready_commands() {
    let (_directory, transcript) = test_transcript();
    let provider = ScriptedProvider::new([Ok(Message::assistant("done"))]);
    let requests = provider.requests.clone();
    let (events, receiver) = session_event_channel(32);
    let mut queued = vec![submit("active")];
    queued.extend((0..8).map(|_| {
        SessionCommand::Control(ControlCommand::CancelTurn {
            turn_id: Some(TurnId::new(99)),
        })
    }));
    queued.push(SessionCommand::Control(ControlCommand::Shutdown));
    let run = RunningSession::start(engine(provider, transcript), queued, events, receiver);
    let lifecycle = run.finish().await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_turns_do_not_overlap(&lifecycle, &[1]);
    assert!(lifecycle.iter().any(|event| {
        matches!(event, SessionEvent::TurnCompleted { turn_id, .. } if *turn_id == TurnId::new(1))
    }));
}

#[tokio::test]
async fn active_shutdown_and_channel_exhaustion_await_cleanup_and_discard_pending_work() {
    for explicit in [true, false] {
        let (_directory, transcript) = test_transcript();
        let path = transcript.path().to_path_buf();
        let (provider, mut calls) = GatedProvider::new();
        let cancellations = provider.cancellations.clone();
        let in_flight = provider.in_flight.clone();
        let (events, receiver) = session_event_channel(1);
        let backpressure = events.clone();
        let mut run = RunningSession::start(
            engine(provider, transcript),
            [submit("active")],
            events,
            receiver,
        );
        let call = run.call(&mut calls).await;
        run.send(submit("pending must not run"));
        run.busy_fence("pending-handled").await;

        // Deliberately block terminal event publication. Cancellation must not
        // drop the command future to bypass this awaited cleanup boundary.
        backpressure
            .send(SessionEvent::SkillsResult {
                request_id: "cleanup-backpressure".into(),
                result: SkillManagementResult::error("test", "occupy the lifecycle slot"),
            })
            .await
            .unwrap();
        drop(backpressure);
        if explicit {
            run.send(SessionCommand::Control(
                crate::session::ControlCommand::Shutdown,
            ));
        } else {
            run.commands.take();
        }
        tokio::time::timeout(TIMEOUT, call.turn.cancellation().cancelled())
            .await
            .expect("active turn cancelled");
        assert!(
            !run.task.is_finished(),
            "terminal publication must finish before exit"
        );
        let lifecycle = run.finish().await;
        assert_turns_do_not_overlap(&lifecycle, &[1]);
        assert!(lifecycle.iter().any(|event| matches!(event, SessionEvent::TurnCancelled { turn_id } if *turn_id == call.turn.id)));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        assert!(call.response.is_closed());
        assert!(calls.try_recv().is_err());
        assert_eq!(
            conversation_records(&zevria_transcript::transcript::load(&path).unwrap()),
            vec![
                TranscriptItem::Message(Message::user("active")),
                TranscriptItem::Error {
                    error: "turn cancelled by the user".into()
                },
            ]
        );
    }
}

#[tokio::test]
async fn clean_turn_completion_still_delivers_backpressured_control_reply() {
    let (_directory, transcript) = test_transcript();
    let (provider, mut calls) = GatedProvider::new();
    let (events, receiver) = session_event_channel(1);
    let backpressure = events.clone();
    let mut run = RunningSession::start(
        engine(provider, transcript),
        [submit("active")],
        events,
        receiver,
    );
    let call = run.call(&mut calls).await;
    backpressure
        .try_send(SessionEvent::SkillsResult {
            request_id: "occupy-slot".into(),
            result: SkillManagementResult::error("test", "backpressure"),
        })
        .unwrap();
    run.send(SessionCommand::Manage(
        crate::session::ManagementCommand::Skills {
            request_id: "owed-reply".into(),
            request: SkillManagementRequest::List {
                query: String::new(),
            },
        },
    ));
    tokio::task::yield_now().await;
    answer(call, "done");
    let reply = run.until(|event| matches!(event, SessionEvent::SkillsResult { request_id, .. } if request_id == "owed-reply")).await;
    assert!(matches!(
        reply,
        SessionEvent::SkillsResult {
            result: SkillManagementResult::View { .. },
            ..
        }
    ));
    run.shutdown().await;
}

#[tokio::test]
async fn fatal_active_replay_bypasses_control_backpressure_and_discards_queued_work() {
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let (provider, mut calls) = GatedProvider::new();
    let cancellations = provider.cancellations.clone();
    let in_flight = provider.in_flight.clone();
    let (events, receiver) = session_event_channel(1);
    let backpressure = events.clone();
    let mut run = RunningSession::start(
        engine(provider, transcript),
        [submit("active")],
        events,
        receiver,
    );
    let repeated = Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            "repeated",
            SKILL_TOOL_NAME,
            json!({"skill":"commit"}),
        )],
    };
    let first = run.call(&mut calls).await;
    first.response.send(Ok(repeated.clone())).unwrap();
    let second = run.call(&mut calls).await;
    run.send(submit("pending must not run"));
    run.busy_fence("pending-handled").await;
    backpressure
        .try_send(SessionEvent::SkillsResult {
            request_id: "occupy-slot".into(),
            result: SkillManagementResult::error("test", "backpressure"),
        })
        .unwrap();
    run.send(SessionCommand::Manage(
        crate::session::ManagementCommand::Skills {
            request_id: "blocked-query".into(),
            request: SkillManagementRequest::List {
                query: String::new(),
            },
        },
    ));
    // Even a control reply already blocked in send cannot prevent polling the
    // active future and observing its typed fatal result.
    tokio::task::yield_now().await;
    let Message::Assistant { mut content, .. } = repeated else {
        unreachable!()
    };
    content.push(content[0].clone());
    second
        .response
        .send(Ok(Message::Assistant { id: None, content }))
        .unwrap();
    let error = tokio::time::timeout(TIMEOUT, &mut run.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate unresolved tool call id")
    );
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    assert!(calls.try_recv().is_err());
    let lifecycle = collect_events(&mut run.receiver).await;
    assert!(!lifecycle.iter().any(|event| matches!(
        event,
        SessionEvent::TurnCompleted { .. }
            | SessionEvent::TurnFailed { .. }
            | SessionEvent::TurnCancelled { .. }
    )));
    let bytes = std::fs::read_to_string(path).unwrap();
    assert!(!bytes.contains("pending must not run"));
    assert!(!bytes.contains("turn cancelled"));
    let lines = bytes.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        4,
        "prompt, first call, tool result, invalid repeated call"
    );
    assert_ne!(
        lines[1], lines[3],
        "the failed second response contains duplicate batch-local calls"
    );
}

#[tokio::test]
async fn writable_shutdown_repairs_a_recoverably_degraded_transcript() {
    for explicit in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        let transcript = TranscriptWriter::create(&sessions).unwrap();
        let path = transcript.path().to_path_buf();
        let mut engine = engine(ScriptedProvider::new([]), transcript);
        let original = TranscriptItem::Message(Message::user("durable request"));
        engine.conversation.push_required(original.clone()).unwrap();
        let mut blocker =
            TranscriptRewriteBlocker::new(&path).expect("block transcript replacement");
        let completed = TranscriptItem::Message(Message::assistant("completed work"));
        engine
            .conversation
            .push_completed_batch(vec![completed.clone()])
            .expect_err("transcript replacement is obstructed");
        assert!(engine.conversation.persistence_error().is_some());
        assert_eq!(
            zevria_transcript::transcript::load(blocker.backup_path()).unwrap(),
            vec![original.clone()]
        );
        blocker.restore().expect("restore transcript filename");
        let (events, receiver) = session_event_channel(1);
        let mut run = RunningSession::start(engine, [], events, receiver);
        run.idle_fence("idle-ready").await;
        assert_eq!(
            zevria_transcript::transcript::load(&path).unwrap(),
            vec![original.clone()],
            "startup does not repair the transcript"
        );
        if explicit {
            run.send(SessionCommand::Control(
                crate::session::ControlCommand::Shutdown,
            ));
        } else {
            run.commands.take();
        }
        run.finish().await;
        assert_eq!(
            zevria_transcript::transcript::load(&path).unwrap(),
            vec![original, completed]
        );
    }
}
