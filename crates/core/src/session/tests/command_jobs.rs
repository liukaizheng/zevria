use super::*;
use zevria_foundation::{CommandJobMode, CommandJobStatus, TerminationOrigin};
use zevria_session_api::{
    CommandControl, CommandControlAction, CommandControlResult, CommandJobLimits, CommandJobs,
};
fn control(id: &str, action: CommandControlAction) -> SessionCommand {
    SessionCommand::Control(ControlCommand::Command(CommandControl {
        request_id: id.into(),
        action,
    }))
}
#[tokio::test]
async fn job_controls_are_immediate_while_busy_and_leave_queued_turns_untouched() {
    let (_directory, transcript) = test_transcript();
    let jobs = CommandJobs::new(
        CommandJobLimits {
            max_running_background: 8,
            retained_finished: 8,
            capture_bytes: 1024,
            foreground_timeout: Duration::from_secs(30),
            background_timeout: None,
        },
        None,
    );
    let job = jobs
        .register("synthetic".into(), None, CommandJobMode::Foreground, None)
        .await
        .unwrap();
    let (provider, mut calls) = GatedProvider::new();
    let engine = engine(provider, transcript).with_command_jobs(jobs.clone());
    let (events, receiver) = session_event_channel(32);
    let mut run = RunningSession::start(engine, [submit("active")], events, receiver);
    let call = run.call(&mut calls).await;
    run.send(submit("queued"));
    run.send(control(
        "stop",
        CommandControlAction::Terminate {
            id: job.snapshot().id,
        },
    ));
    let result = run.until(|e| matches!(e, SessionEvent::CommandControlResult { request_id, .. } if request_id == "stop")).await;
    assert!(matches!(
        result,
        SessionEvent::CommandControlResult {
            result: CommandControlResult::Accepted(_),
            ..
        }
    ));
    assert_eq!(
        *job.termination_requested().borrow(),
        Some(TerminationOrigin::User)
    );
    assert!(!call.turn.cancellation().is_cancelled());
    assert!(calls.try_recv().is_err());
    job.finish(CommandJobStatus::Terminated {
        origin: TerminationOrigin::User,
    })
    .await;
    answer(call, "active complete");
    run.completed(TurnId::new(1)).await;
    let queued = run.call(&mut calls).await;
    assert_eq!(queued.turn.id, TurnId::new(2));
    answer(queued, "queued complete");
    run.completed(TurnId::new(2)).await;
    run.send(control(
        "inspect",
        CommandControlAction::Inspect { id: None },
    ));
    assert!(
        matches!(run.until(|e| matches!(e, SessionEvent::CommandControlResult { request_id, .. } if request_id == "inspect")).await, SessionEvent::CommandControlResult { result: CommandControlResult::Inspected(v), .. } if v.is_empty())
    );
    run.shutdown().await;
}
#[tokio::test]
async fn direct_and_idle_job_controls_reject_without_capability() {
    let (_directory, transcript) = test_transcript();
    let (provider, _) = GatedProvider::new();
    let mut engine = engine(provider, transcript);
    let (events, mut receiver) = session_event_channel(8);
    engine
        .handle_command(
            control("direct", CommandControlAction::Inspect { id: None }),
            &events,
        )
        .await
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(zevria_session_api::SessionUpdate::Lifecycle(
            SessionEvent::CommandControlResult {
                result: CommandControlResult::Rejected { .. },
                ..
            }
        ))
    ));
    let mut run = RunningSession::start(engine, [], events, receiver);
    run.send(control("idle", CommandControlAction::Inspect { id: None }));
    assert!(matches!(run.until(|e| matches!(e, SessionEvent::CommandControlResult { request_id, .. } if request_id == "idle")).await, SessionEvent::CommandControlResult { result: CommandControlResult::Rejected { .. }, .. }));
    run.shutdown().await;
}
