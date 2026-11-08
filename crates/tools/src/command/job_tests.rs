use super::*;
use crate::{CommandStatusArgs, CommandStatusTool, CommandTerminateArgs, CommandTerminateTool};
use zevria_foundation::{CommandJobId, SessionMode, TurnId};
use zevria_session_api::{CommandControl, CommandControlAction, CommandControlResult};
fn jobs() -> CommandJobs {
    CommandJobs::new(
        CommandJobLimits {
            max_running_background: 1,
            retained_finished: 8,
            capture_bytes: 4096,
            foreground_timeout: Duration::from_secs(5),
            background_timeout: None,
        },
        None,
    )
}
async fn start(tool: &CommandTool, command: &str) -> CommandJobId {
    let mut context = ToolContext::new();
    context.insert_scope(TurnContext::new(
        TurnId::new(7),
        SessionMode::Build,
        Default::default(),
    ));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(
            &mut context,
            CommandArgs {
                command: command.into(),
                background: Some(true),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let detail = context.result::<ToolResultDetail>().unwrap().unwrap();
    let id = detail.command_job().unwrap().id.clone();
    assert_eq!(
        result,
        format!(
            "id: {id}\nstatus: running\nnote: started in the background; use command_status to read output or wait, and command_terminate to stop it"
        )
    );
    id
}
async fn status(jobs: &CommandJobs, id: &CommandJobId, wait: Option<u64>) -> String {
    CommandStatusTool::new(jobs.clone())
        .call(
            &mut ToolContext::new(),
            CommandStatusArgs {
                id: Some(id.to_string()),
                wait_seconds: wait,
            },
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn background_returns_promptly_incremental_output_cap_and_no_footers() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = jobs();
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    let id = start(
        &tool,
        "printf first; sleep 0.3; printf second; printf done > done",
    )
    .await;
    assert!(
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: "sleep 10".into(),
                background: Some(true)
            }
        )
        .await
        .is_err()
    );
    let first = status(&jobs, &id, Some(0)).await;
    let last = status(&jobs, &id, Some(5)).await;
    assert_eq!(
        first.matches("first").count() + last.matches("first").count(),
        1
    );
    assert!(last.contains("second"));
    assert!(directory.path().join("done").exists());
    let reread = status(&jobs, &id, None).await;
    assert!(reread.starts_with(&format!("id: {id}\nstatus: exited 0\nduration: ")));
    assert_eq!(reread.lines().count(), 3);
    assert!(!reread.contains("<stdout>"));
    assert!(!reread.contains("<stderr>"));
    let id2 = start(&tool, "printf update").await;
    jobs.wait_terminal(&id2, Duration::from_secs(5), &Default::default())
        .await
        .unwrap();
    let result = tool
        .call(
            &mut ToolContext::new(),
            CommandArgs {
                command: "printf other".into(),
                background: None,
            },
        )
        .await
        .unwrap();
    assert!(!result.contains("job updates:"));
    let pending = jobs.pending_completions();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].snapshot.id, id2);
    assert_eq!(pending[0].origin, Some(TurnId::new(7)));
    let result = tool
        .call(
            &mut ToolContext::new(),
            CommandArgs {
                command: "printf other".into(),
                background: None,
            },
        )
        .await
        .unwrap();
    assert!(!result.contains("job updates:"));
    assert_eq!(jobs.pending_completions().len(), 1);
    assert!(status(&jobs, &id2, None).await.contains("update"));
    assert!(jobs.pending_completions().is_empty());
}
#[cfg(any(unix, windows))]
#[tokio::test]
async fn terminate_and_shutdown_kill_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = jobs();
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    let id = start(&tool, "(sleep 0.5; printf leak > leak) & sleep 10").await;
    let result = CommandTerminateTool::new(jobs.clone())
        .call(
            &mut ToolContext::new(),
            CommandTerminateArgs { id: id.to_string() },
        )
        .await
        .unwrap();
    assert!(result.starts_with(&format!(
        "id: {id}\nstatus: terminated by the agent\nduration: "
    )));
    let already_finished = CommandTerminateTool::new(jobs.clone())
        .call(
            &mut ToolContext::new(),
            CommandTerminateArgs { id: id.to_string() },
        )
        .await
        .unwrap();
    assert_eq!(already_finished, result);
    assert_eq!(already_finished.lines().count(), 3);
    let id = start(&tool, "sleep 0.5; printf leak > leak").await;
    jobs.shutdown(Duration::from_secs(3)).await;
    assert!(jobs.snapshot(&id).unwrap().status.is_terminal());
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!directory.path().join("leak").exists());
}
#[tokio::test]
async fn user_terminate_and_move_do_not_cancel_turn() {
    for move_it in [false, true] {
        let jobs = jobs();
        let tool = CommandTool::with_jobs(PathBuf::from("."), jobs.clone());
        let token = tokio_util::sync::CancellationToken::new();
        let turn = TurnContext::new(TurnId::new(1), SessionMode::Build, token.clone());
        let worker = tokio::spawn(async move {
            let mut context = ToolContext::new();
            context.insert_scope(turn);
            let result = tool
                .call(
                    &mut context,
                    CommandArgs {
                        command: "sleep 0.5; printf done".into(),
                        background: None,
                    },
                )
                .await;
            (result, context)
        });
        let id = loop {
            let result = jobs.handle_control(CommandControl {
                request_id: "inspect".into(),
                action: CommandControlAction::Inspect { id: None },
            });
            if let CommandControlResult::Inspected(v) = result {
                if let Some(job) = v.first() {
                    break job.snapshot.id.clone();
                }
            }
            tokio::task::yield_now().await;
        };
        let action = if move_it {
            CommandControlAction::MoveToBackground { id: id.clone() }
        } else {
            CommandControlAction::Terminate { id: id.clone() }
        };
        assert!(matches!(
            jobs.handle_control(CommandControl {
                request_id: "action".into(),
                action
            }),
            CommandControlResult::Accepted(_)
        ));
        let (result, context) = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(!token.is_cancelled());
        if move_it {
            let result = result.unwrap();
            assert!(result.starts_with(&format!("id: {id}\nstatus: running\nduration: ")));
            assert!(result.contains("\nnote: the user moved this command to the background;"));
            assert!(context.result::<ToolResultDetail>().unwrap().is_some());
            assert!(status(&jobs, &id, Some(5)).await.contains("done"));
        } else {
            let error = result.unwrap_err();
            assert!(matches!(error, CommandError::Terminated { .. }));
            let rendered = error.to_string();
            assert!(rendered.starts_with("the user terminated this command\nduration: "));
            assert!(!rendered.contains("status:"));
            assert!(!rendered.contains("exit_code:"));
        }
    }
}
#[tokio::test]
async fn lists_share_job_lines_and_do_not_consume_unread_streams() {
    let jobs = jobs();
    let command = format!("  rg\t-n\n{}   src  ", "é".repeat(150));
    let job = jobs
        .register(command, None, CommandJobMode::Background, None)
        .await
        .unwrap();
    job.append(zevria_session_api::CommandStream::Stdout, b"still unread");
    job.finish(CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    })
    .await;
    let list = CommandStatusTool::new(jobs.clone())
        .call(
            &mut ToolContext::new(),
            CommandStatusArgs {
                id: None,
                wait_seconds: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(list.lines().count(), 1);
    assert!(list.starts_with(&format!("{} · exited 0 · ", job.snapshot().id)));
    let preview = list.rsplit_once(" · ").unwrap().1;
    assert_eq!(preview.chars().count(), 120);
    assert!(preview.starts_with("rg -n "));
    assert!(preview.ends_with('…'));
    assert!(!list.contains("Background"));
    assert!(!list.contains("job updates:"));
    let report = status(&jobs, &job.snapshot().id, None).await;
    assert!(report.ends_with("\n<stdout>\nstill unread\n</stdout>"));
    assert!(!report.contains("job updates:"));
}

#[tokio::test]
async fn interrupted_error_keeps_its_variant_and_does_not_consume_completions() {
    let jobs = CommandJobs::new(
        CommandJobLimits {
            foreground_timeout: Duration::from_millis(100),
            ..jobs().limits()
        },
        None,
    );
    let finished = jobs
        .register(
            "printf update".into(),
            None,
            CommandJobMode::Background,
            None,
        )
        .await
        .unwrap();
    finished
        .finish(CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        })
        .await;
    let tool = CommandTool::with_jobs(PathBuf::from("."), jobs.clone());
    let error = tool
        .call(
            &mut ToolContext::new(),
            CommandArgs {
                command: "sleep 10".into(),
                background: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Timeout { .. }));
    let rendered = error.to_string();
    assert!(!rendered.contains("job updates:"));
    assert_eq!(jobs.pending_completions().len(), 1);
    assert_eq!(
        jobs.pending_completions()[0].snapshot.id,
        finished.snapshot().id
    );
    let next = CommandStatusTool::new(jobs)
        .call(
            &mut ToolContext::new(),
            CommandStatusArgs {
                id: None,
                wait_seconds: None,
            },
        )
        .await
        .unwrap();
    assert!(!next.contains("job updates:"));
}

#[tokio::test]
async fn cancelling_status_wait_leaves_job_running_and_background_survives_turn() {
    let jobs = jobs();
    let tool = CommandTool::with_jobs(PathBuf::from("."), jobs.clone());
    let id = start(&tool, "sleep 10").await;
    let token = tokio_util::sync::CancellationToken::new();
    let mut context = ToolContext::new();
    context.insert_scope(TurnContext::new(
        TurnId::new(1),
        SessionMode::Plan,
        token.clone(),
    ));
    token.cancel();
    let error = CommandStatusTool::new(jobs.clone())
        .call(
            &mut context,
            CommandStatusArgs {
                id: Some(id.to_string()),
                wait_seconds: Some(5),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CommandError::Cancelled { .. }));
    assert!(context.result::<ToolCancelled>().unwrap().is_some());
    assert_eq!(
        jobs.snapshot(&id).unwrap().status,
        CommandJobStatus::Running
    );
    jobs.shutdown(Duration::from_secs(3)).await;
}
