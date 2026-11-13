use super::*;
use zevria_foundation::{
    CommandJobMode, CommandJobNoticeKind, CommandJobStatus, TerminationOrigin,
};
use zevria_session_api::{CommandJobLimits, CommandJobs, CommandStream, JobHandle};

fn jobs() -> CommandJobs {
    CommandJobs::new(
        CommandJobLimits {
            max_running_background: 8,
            retained_finished: 8,
            capture_bytes: 256 * 1024,
            foreground_timeout: Duration::from_secs(30),
            background_timeout: None,
        },
        None,
    )
}
fn job_engine<P: ModelProvider>(
    provider: P,
    transcript: TranscriptWriter,
    jobs: &CommandJobs,
    limit: u32,
) -> SessionEngine<P> {
    let mut policies = test_policies();
    policies.policy_mut(SessionMode::Plan).allowed_tool_names = Some(vec![
        "command_status".into(),
        "echo".into(),
        "create_plan".into(),
        "submit_plan".into(),
        "update_plan".into(),
        "read_plan".into(),
    ]);
    SessionEngine::new(
        provider,
        ToolServer::new()
            .tool(EchoTool {
                calls: Arc::default(),
            })
            .tool(CreatePlanStubTool)
            .tool(UpdatePlanStubTool)
            .tool(ReadPlanStubTool)
            .tool(SubmitPlanStubTool)
            .run(),
        policies,
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_command_jobs(jobs.clone())
    .with_job_continuations(limit)
    .with_subtask_concurrency(2)
    .with_mode_management()
    .with_idle_notifications()
}
async fn job(jobs: &CommandJobs, origin: u64, output: &[u8]) -> JobHandle {
    let job = jobs
        .register(
            "synthetic background".into(),
            None,
            CommandJobMode::Background,
            Some(TurnId::new(origin)),
        )
        .await
        .unwrap();
    job.append(CommandStream::Stdout, output);
    job
}
async fn finish(job: &JobHandle) {
    job.finish(CommandJobStatus::Exited {
        code: Some(1),
        success: false,
        signal: None,
    })
    .await;
}
fn continue_jobs(mode: SessionMode) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::ContinueAfterJobs { mode })
}
fn notice_in(request: &CapturedRequest) -> String {
    request
        .input
        .iter()
        .filter_map(|i| i.as_borrowed().message_ref().cloned())
        .map(|m| serde_json::to_string(&m).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn dispatch_boundary_delivers_once_after_complete_tool_batch_with_static_prefix() {
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let jobs = jobs();
    let job = job(&jobs, 1, b"boundary-output").await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [submit("work")], events, receiver);
    let first = run.call(&mut calls).await;
    let first_request = first.request.clone();
    finish(&job).await;
    first
        .response
        .send(Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![tool_call("echo-1", "tool-batch")]),
        )))
        .unwrap();
    let second = run.call(&mut calls).await;
    assert!(notice_in(&second.request).contains("boundary-output"));
    assert!(second.request.input.starts_with(&first_request.input));
    assert_eq!(second.request.instructions, first_request.instructions);
    assert_eq!(
        second.request.allowed_tool_names,
        first_request.allowed_tool_names
    );
    assert!(jobs.take_unread(&job.snapshot().id).unwrap().0.is_empty());
    assert!(run.lifecycle.iter().any(|e| matches!(e, SessionEvent::CommandJobsReported { notice, .. } if notice.kind() == CommandJobNoticeKind::Update)));
    answer(second, "done");
    run.completed(TurnId::new(1)).await;
    run.shutdown().await;
    let items = zevria_transcript::load(&path).unwrap();
    let index = items
        .iter()
        .position(|i| matches!(i, TranscriptItem::CommandJobNotice(_)))
        .unwrap();
    assert!(matches!(
        items[index - 1],
        TranscriptItem::ToolResults { .. }
    ));
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, TranscriptItem::CommandJobNotice(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn idle_completion_advertises_then_continues_as_a_standard_append_only_request() {
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let jobs = jobs();
    let job = job(&jobs, 1, b"idle-output").await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [submit("start")], events, receiver);
    let first = run.call(&mut calls).await;
    let previous = first.request.clone();
    answer(first, "waiting on job");
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    finish(&job).await;
    run.until(|e| {
        matches!(
            e,
            SessionEvent::JobContinuationChanged {
                mode: Some(SessionMode::Build)
            }
        )
    })
    .await;
    assert!(
        calls.try_recv().is_err(),
        "the engine never starts unsolicited work"
    );
    run.send(continue_jobs(SessionMode::Build));
    let resumed = run.call(&mut calls).await;
    assert!(!resumed.turn.build_subtasks);
    assert!(resumed.request.input.starts_with(&previous.input));
    assert_eq!(resumed.request.instructions, previous.instructions);
    assert_eq!(
        resumed.request.allowed_tool_names,
        previous.allowed_tool_names
    );
    assert!(
        matches!(resumed.request.input.last().unwrap().as_borrowed(), ModelRequestItem::RequestInstruction(d) if d.request.behavior == zevria_foundation::RequestBehavior::Standard)
    );
    assert!(notice_in(&resumed.request).contains("no new user message was sent"));
    assert!(notice_in(&resumed.request).contains("idle-output"));
    assert!(
        run.lifecycle
            .iter()
            .any(|e| matches!(e, SessionEvent::JobContinuationStarted { .. }))
    );
    answer(resumed, "job failed");
    run.completed(TurnId::new(2)).await;
    run.shutdown().await;
    let items = zevria_transcript::load(&path).unwrap();
    let index = items
        .iter()
        .position(|i| matches!(i, TranscriptItem::CommandJobNotice(_)))
        .unwrap();
    assert!(matches!(
        items[index + 1],
        TranscriptItem::RequestDirective(_)
    ));
}

#[tokio::test]
async fn cancelled_and_failed_origins_are_passive_status_only_and_keep_output_unread() {
    for cancelled in [true, false] {
        let (_directory, transcript) = test_transcript();
        let jobs = jobs();
        let job = job(&jobs, 1, b"abandoned-output").await;
        let (provider, mut calls) = GatedProvider::new();
        let engine = job_engine(provider, transcript, &jobs, 3);
        let (events, receiver) = session_event_channel(64);
        let mut run = RunningSession::start(engine, [submit("old work")], events, receiver);
        let first = run.call(&mut calls).await;
        if cancelled {
            run.send(SessionCommand::Control(ControlCommand::CancelTurn {
                turn_id: None,
            }));
            run.until(|e| matches!(e, SessionEvent::TurnCancelled { .. }))
                .await;
        } else {
            first
                .response
                .send(Err(anyhow::anyhow!("provider failed")))
                .unwrap();
            run.until(|e| matches!(e, SessionEvent::TurnFailed { .. }))
                .await;
        }
        run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
        finish(&job).await;
        run.idle_fence("after-finish").await;
        assert!(
            !run.lifecycle
                .iter()
                .any(|e| matches!(e, SessionEvent::JobContinuationChanged { mode: Some(_) }))
        );
        run.send(submit("new work"));
        let next = run.call(&mut calls).await;
        let input = notice_in(&next.request);
        assert!(input.contains("Command job update:"));
        assert!(!input.contains("abandoned-output"));
        assert_eq!(
            jobs.take_unread(&job.snapshot().id).unwrap().0.head,
            "abandoned-output"
        );
        answer(next, "new work done");
        run.completed(TurnId::new(2)).await;
        run.shutdown().await;
    }
}

#[tokio::test]
async fn user_termination_does_not_offer_and_delivers_only_status() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let stopped = job(&jobs, 0, b"stopped-output").await;
    stopped
        .finish(CommandJobStatus::Terminated {
            origin: TerminationOrigin::User,
        })
        .await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    assert_eq!(engine.continuation_offer(), None);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [submit("next")], events, receiver);
    let next = run.call(&mut calls).await;
    assert!(notice_in(&next.request).contains("terminated by the user"));
    assert!(!notice_in(&next.request).contains("stopped-output"));
    answer(next, "done");
    run.completed(TurnId::new(1)).await;
    run.shutdown().await;
}

#[tokio::test]
async fn ready_plan_holds_results_until_implementation_handoff() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let completed = job(&jobs, 0, b"plan-held-output").await;
    finish(&completed).await;
    let (artifact, items) = ready_plan_fixture();
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3)
        .with_transcript_items(items)
        .unwrap();
    assert_eq!(engine.continuation_offer(), None);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [], events, receiver);
    run.idle_fence("ready").await;
    assert_eq!(jobs.pending_completions().len(), 1);
    assert!(!run.lifecycle.iter().any(|e| matches!(
        e,
        SessionEvent::CommandJobsReported { .. }
            | SessionEvent::JobContinuationChanged { mode: Some(_) }
    )));
    run.send(SessionCommand::Turn(TurnCommand::ResolvePlan {
        expected: artifact.version,
        decision: PlanDecision::ImplementCurrent,
    }));
    let handoff = run.call(&mut calls).await;
    assert!(notice_in(&handoff.request).contains("plan-held-output"));
    answer(handoff, "implemented");
    run.completed(TurnId::new(1)).await;
    run.shutdown().await;
}

#[tokio::test]
async fn mode_selection_cannot_implicitly_change_plan_workflow() {
    for planning in [true, false] {
        let (_directory, transcript) = test_transcript();
        let jobs = jobs();
        let completed = job(&jobs, 0, b"mode-output").await;
        finish(&completed).await;
        let (provider, _) = GatedProvider::new();
        let mut engine = job_engine(provider, transcript, &jobs, 3);
        if planning {
            engine
                .commit(TranscriptItem::Plan(PlanRecord::Started {
                    id: PlanId::new(),
                }))
                .unwrap();
        }
        let mode = if planning {
            SessionMode::Build
        } else {
            SessionMode::Plan
        };
        let (events, mut receiver) = session_event_channel(32);
        engine
            .handle_command(
                SessionCommand::Manage(ManagementCommand::SetMode {
                    request_id: "mode".into(),
                    mode,
                }),
                &events,
            )
            .await
            .unwrap();
        assert_eq!(engine.continuation_offer(), None);
        assert!(
            !collect_events(&mut receiver)
                .await
                .iter()
                .any(|e| matches!(e, SessionEvent::JobContinuationChanged { mode: Some(_) }))
        );
    }
}

#[tokio::test]
async fn cap_resets_on_user_work_and_stale_rejection_loses_no_completion() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let completed = job(&jobs, 0, b"first").await;
    finish(&completed).await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 1);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(
        engine,
        [continue_jobs(SessionMode::Build)],
        events,
        receiver,
    );
    let first = run.call(&mut calls).await;
    answer(first, "done");
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    let second_job = job(&jobs, 1, b"second").await;
    finish(&second_job).await;
    run.idle_fence("cap").await;
    assert!(
        !run.lifecycle
            .iter()
            .rev()
            .take_while(|e| !matches!(e, SessionEvent::EngineIdle))
            .any(|e| matches!(e, SessionEvent::JobContinuationChanged { mode: Some(_) }))
    );
    run.send(submit("user reset"));
    let user = run.call(&mut calls).await;
    assert!(notice_in(&user.request).contains("second"));
    let third_job = job(&jobs, 2, b"third").await;
    finish(&third_job).await;
    answer(user, "done again");
    run.until(|e| {
        matches!(
            e,
            SessionEvent::JobContinuationChanged {
                mode: Some(SessionMode::Build)
            }
        )
    })
    .await;
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    run.send(continue_jobs(SessionMode::Plan));
    run.until(|e| matches!(e, SessionEvent::TurnRejected { .. }))
        .await;
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    assert_eq!(jobs.pending_completions().len(), 1);
    run.send(submit("after stale"));
    let next = run.call(&mut calls).await;
    assert!(!notice_in(&next.request).contains("<stdout>\\nthird"));
    assert_eq!(
        jobs.take_unread(&third_job.snapshot().id).unwrap().0.head,
        "third"
    );
    answer(next, "done");
    run.completed(TurnId::new(4)).await;
    run.shutdown().await;
}

#[tokio::test]
async fn accepted_edit_unfollows_older_jobs_and_rejected_continuation_does_not_commit() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let old = job(&jobs, 1, b"edit-abandoned-output").await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [submit("old prompt")], events, receiver);
    let first = run.call(&mut calls).await;
    answer(first, "old answer");
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    finish(&old).await;
    run.send(SessionCommand::Turn(TurnCommand::EditTranscript(
        TranscriptEdit {
            target: TranscriptEditTarget::PromptOrdinal(0),
            replacement: TranscriptEditReplacement::Message {
                text: "edited prompt".into(),
                mode: SessionMode::Build,
                behavior: zevria_foundation::RequestBehavior::Standard,
            },
        },
    )));
    let edit = run.call(&mut calls).await;
    assert!(!notice_in(&edit.request).contains("edit-abandoned-output"));
    answer(edit, "new answer");
    run.completed(TurnId::new(2)).await;
    run.shutdown().await;

    let (_directory, transcript) = test_transcript();
    let (provider, _) = GatedProvider::new();
    let mut engine = job_engine(provider, transcript, &jobs, 3);
    let before = engine.conversation.items().to_vec();
    let (events, _) = session_event_channel(64);
    engine
        .handle_command(continue_jobs(SessionMode::Build), &events)
        .await
        .unwrap();
    assert_eq!(engine.conversation.items(), before);
}

#[tokio::test]
async fn accepted_submit_plan_never_delivers_completions_before_approval() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let completed = job(&jobs, 1, b"approval-held-output").await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(
        engine,
        [SessionCommand::Turn(TurnCommand::Submit {
            text: "plan work".into(),
            mode: SessionMode::Plan,
            behavior: zevria_foundation::RequestBehavior::Standard,
        })],
        events,
        receiver,
    );
    let create = run.call(&mut calls).await;
    create
        .response
        .send(Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "create",
                "create_plan",
                json!({"markdown": valid_plan_markdown("Test completion handoff", "Implement it safely."), "replace": false}),
            )]),
        )))
        .unwrap();
    let submit = run.call(&mut calls).await;
    finish(&completed).await;
    submit
        .response
        .send(Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "submit",
                "submit_plan",
                json!({}),
            )]),
        )))
        .unwrap();
    let final_call = run.call(&mut calls).await;
    assert!(
        !notice_in(&final_call.request).contains("approval-held-output"),
        "{}",
        notice_in(&final_call.request)
    );
    assert!(!notice_in(&final_call.request).contains("Command job update:"));
    answer(final_call, "Ready for approval");
    run.until(|e| matches!(e, SessionEvent::EngineIdle)).await;
    assert_eq!(jobs.pending_completions().len(), 1);
    assert!(!run.lifecycle.iter().any(|e| matches!(
        e,
        SessionEvent::JobContinuationChanged { mode: Some(_) }
            | SessionEvent::CommandJobsReported { .. }
    )));
    run.shutdown().await;
}

#[tokio::test]
async fn failed_continuation_commit_preserves_pending_output_and_transcript() {
    let (_directory, transcript) = test_transcript();
    let path = transcript.path().to_path_buf();
    let jobs = jobs();
    let completed = job(&jobs, 0, b"not-acknowledged").await;
    finish(&completed).await;
    let mut engine = job_engine(ScriptedProvider::new([]), transcript, &jobs, 3);
    engine
        .commit(TranscriptItem::Message(Message::user("durable user")))
        .unwrap();
    let original = engine.conversation.items().to_vec();
    let mut blocker = TranscriptRewriteBlocker::new(&path).unwrap();
    let (events, mut receiver) = session_event_channel(64);
    engine
        .handle_command(continue_jobs(SessionMode::Build), &events)
        .await
        .unwrap();
    assert_eq!(engine.conversation.items(), original);
    assert_eq!(jobs.pending_completions().len(), 1);
    assert_eq!(
        jobs.pending_completions()[0].peek_unread().0.head,
        "not-acknowledged"
    );
    assert_eq!(engine.continuation_offer(), None);
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnRejected { .. }))
    );
    blocker.restore().unwrap();
}

#[tokio::test]
async fn zero_limit_and_closed_registry_disable_only_idle_work() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let completed = job(&jobs, 0, b"passive-output").await;
    finish(&completed).await;
    let engine = job_engine(ScriptedProvider::new([]), transcript, &jobs, 0);
    assert_eq!(engine.continuation_offer(), None);
    let engine = engine.with_job_continuations(3);
    assert_eq!(engine.continuation_offer(), Some(SessionMode::Build));
    jobs.close();
    assert_eq!(engine.continuation_offer(), None);
    assert_eq!(
        jobs.pending_completions()[0].peek_unread().0.head,
        "passive-output"
    );
}

#[tokio::test]
async fn burst_output_budget_omits_later_streams_without_consuming_or_evicting_them() {
    let (_directory, transcript) = test_transcript();
    let jobs = jobs();
    let first = job(&jobs, 0, &vec![b'a'; 64 * 1024]).await;
    first.append(CommandStream::Stderr, &vec![b'e'; 64 * 1024]);
    finish(&first).await;
    let later = job(&jobs, 0, b"later-output-still-readable").await;
    finish(&later).await;
    let (provider, mut calls) = GatedProvider::new();
    let engine = job_engine(provider, transcript, &jobs, 3);
    let (events, receiver) = session_event_channel(64);
    let mut run = RunningSession::start(engine, [submit("receive burst")], events, receiver);
    let next = run.call(&mut calls).await;
    let input = notice_in(&next.request);
    assert!(input.contains("output not included; use command_status to read it"));
    assert!(!input.contains("later-output-still-readable"));
    assert!(jobs.take_unread(&first.snapshot().id).unwrap().0.is_empty());
    assert_eq!(
        jobs.take_unread(&later.snapshot().id).unwrap().0.head,
        "later-output-still-readable"
    );
    answer(next, "received");
    run.completed(TurnId::new(1)).await;
    run.shutdown().await;
}
