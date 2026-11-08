use super::*;
use crate::{CommandStatusArgs, CommandStatusTool};
fn limits() -> CommandJobLimits {
    CommandJobLimits {
        max_running_background: 8,
        retained_finished: 8,
        capture_bytes: 1024,
        foreground_timeout: Duration::from_millis(40),
        background_timeout: None,
    }
}
#[tokio::test]
async fn background_has_no_default_deadline_and_status_wait_is_bounded() {
    let jobs = CommandJobs::new(limits(), None);
    let tool = CommandTool::with_jobs(PathBuf::from("."), jobs.clone());
    tool.call(
        &mut ToolContext::new(),
        CommandArgs {
            command: "sleep 1; printf complete".into(),
            background: Some(true),
        },
    )
    .await
    .unwrap();
    let id = jobs.list_model_jobs()[0].id.clone();
    let status = CommandStatusTool::new(jobs.clone())
        .call(
            &mut ToolContext::new(),
            CommandStatusArgs {
                id: Some(id.to_string()),
                wait_seconds: Some(5),
            },
        )
        .await
        .unwrap();
    assert!(status.contains("status: running"));
    assert_eq!(
        jobs.snapshot(&id).unwrap().status,
        CommandJobStatus::Running
    );
    assert!(
        jobs.wait_terminal(&id, Duration::from_secs(5), &Default::default())
            .await
            .unwrap()
    );
    assert!(matches!(
        jobs.snapshot(&id).unwrap().status,
        CommandJobStatus::Exited { success: true, .. }
    ));
}
#[cfg(unix)]
#[tokio::test]
async fn model_termination_escalates_when_the_shell_ignores_sigterm() {
    use crate::{CommandTerminateArgs, CommandTerminateTool};
    let directory = tempfile::tempdir().unwrap();
    let jobs = CommandJobs::new(limits(), None);
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    tool.call(&mut ToolContext::new(), CommandArgs { command:"trap '' TERM; echo $$ > pid; printf ready > ready; (sleep 3; printf leak > leaked) & wait".into(), background:Some(true) }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !directory.path().join("ready").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let id = jobs.list_model_jobs()[0].id.clone();
    let result = CommandTerminateTool::new(jobs)
        .call(
            &mut ToolContext::new(),
            CommandTerminateArgs { id: id.to_string() },
        )
        .await
        .unwrap();
    assert!(result.contains("status: terminated by the agent\nduration: "));
    let pid: i32 = std::fs::read_to_string(directory.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal zero probes only the shell pid recorded by this fixture.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!directory.path().join("leaked").exists());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn turn_cancel_and_foreground_future_drop_leave_background_jobs_alive() {
    use zevria_foundation::{SessionMode, TurnId};
    use zevria_session_api::{CommandControl, CommandControlAction, CommandControlResult};
    let directory = tempfile::tempdir().unwrap();
    let jobs = CommandJobs::new(
        CommandJobLimits {
            foreground_timeout: Duration::from_secs(5),
            ..limits()
        },
        None,
    );
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    let token = tokio_util::sync::CancellationToken::new();
    let mut context = ToolContext::new();
    context.insert_scope(TurnContext::new(
        TurnId::new(1),
        SessionMode::Build,
        token.clone(),
    ));
    tool.call(
        &mut context,
        CommandArgs {
            command: "sleep 10".into(),
            background: Some(true),
        },
    )
    .await
    .unwrap();
    let background = jobs.list_model_jobs()[0].id.clone();
    token.cancel();
    drop(context);
    let worker = tokio::spawn(async move {
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: "sleep 0.4; printf leak > leaked".into(),
                background: None,
            },
        )
        .await
    });
    let foreground = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let CommandControlResult::Inspected(views) = jobs.handle_control(CommandControl {
                request_id: "inspect".into(),
                action: CommandControlAction::Inspect { id: None },
            }) && let Some(view) = views
                .iter()
                .find(|v| v.snapshot.mode == CommandJobMode::Foreground)
            {
                break jobs.job(&view.snapshot.id).unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    let _ = worker.await;
    assert!(
        foreground
            .wait_terminal(Duration::from_secs(5), &Default::default())
            .await
    );
    assert_eq!(
        foreground.snapshot().status,
        CommandJobStatus::Terminated {
            origin: TerminationOrigin::Aborted
        }
    );
    assert_eq!(
        jobs.snapshot(&background).unwrap().status,
        CommandJobStatus::Running
    );
    jobs.shutdown(Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!directory.path().join("leaked").exists());
}

#[tokio::test]
async fn optional_background_deadline_and_restart_errors() {
    let jobs = CommandJobs::new(
        CommandJobLimits {
            background_timeout: Some(Duration::from_millis(150)),
            ..limits()
        },
        None,
    );
    let tool = CommandTool::with_jobs(PathBuf::from("."), jobs.clone());
    tool.call(
        &mut ToolContext::new(),
        CommandArgs {
            command: "sleep 10".into(),
            background: Some(true),
        },
    )
    .await
    .unwrap();
    let id = jobs.list_model_jobs()[0].id.clone();
    assert!(
        jobs.wait_terminal(&id, Duration::from_secs(5), &Default::default())
            .await
            .unwrap()
    );
    assert!(matches!(
        jobs.snapshot(&id).unwrap().status,
        CommandJobStatus::TimedOut { .. }
    ));
    let error = CommandStatusTool::new(jobs)
        .call(
            &mut ToolContext::new(),
            CommandStatusArgs {
                id: Some("job-old-session".into()),
                wait_seconds: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Jobs do not survive restarts"));
}
#[cfg(any(unix, windows))]
#[tokio::test]
async fn registry_drop_kills_background_process_trees() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = CommandJobs::new(limits(), None);
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    tool.call(
        &mut ToolContext::new(),
        CommandArgs {
            command: "sleep 0.4; printf leak > leaked".into(),
            background: Some(true),
        },
    )
    .await
    .unwrap();
    drop(tool);
    drop(jobs);
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!directory.path().join("leaked").exists());
}
