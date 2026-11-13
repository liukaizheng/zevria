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
mod detached_output {
    use super::*;
    use zevria_foundation::{SessionMode, TurnId};

    const HOLDER: &str = r#"perl -MPOSIX -e 'POSIX::setsid() >= 0 or die "setsid: $!"; open my $f, ">", "holder.pid" or die $!; print $f $$; close $f; sleep 60' & while [ ! -s holder.pid ]; do sleep 0.05; done"#;

    fn perl_available() -> bool {
        if std::process::Command::new("perl")
            .args(["-e", "1"])
            .status()
            .is_ok_and(|s| s.success())
        {
            true
        } else {
            eprintln!("skipping detached pipe-holder test: perl is unavailable");
            false
        }
    }
    struct HolderGuard(PathBuf);
    impl Drop for HolderGuard {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(&self.0)
                && let Ok(pid) = pid.trim().parse::<i32>()
                && pid > 0
            {
                // SAFETY: this is the escaped holder's pid recorded by our fixture.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    async fn wait_for_holder(holder: &HolderGuard) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !std::fs::metadata(&holder.0).is_ok_and(|m| m.len() > 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached holder did not start");
    }
    fn detached_limits() -> CommandJobLimits {
        CommandJobLimits {
            foreground_timeout: Duration::from_secs(30),
            ..limits()
        }
    }

    #[tokio::test]
    async fn foreground_detached_pipe_holder_does_not_pin_completion() {
        if !perl_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _holder = HolderGuard(directory.path().join("holder.pid"));
        let jobs = CommandJobs::new(detached_limits(), None);
        let tool = CommandTool::with_jobs(directory.path().into(), jobs);
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tool.call(
                &mut ToolContext::new(),
                CommandArgs {
                    command: format!("{HOLDER}; echo leader-done"),
                    background: None,
                },
            ),
        )
        .await
        .expect("foreground capture was pinned by detached output")
        .unwrap();
        assert!(output.starts_with("exit_code: 0\nduration: "), "{output}");
        assert!(output.contains("leader-done"), "{output}");
        assert!(output.contains(render::DETACHED_OUTPUT_NOTE), "{output}");
    }

    #[tokio::test]
    async fn background_detached_pipe_holder_finishes_with_real_status_and_note() {
        if !perl_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _holder = HolderGuard(directory.path().join("holder.pid"));
        let jobs = CommandJobs::new(detached_limits(), None);
        let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: format!("{HOLDER}; echo leader-done"),
                background: Some(true),
            },
        )
        .await
        .unwrap();
        let id = jobs.list_model_jobs()[0].id.clone();
        assert!(
            jobs.wait_terminal(&id, Duration::from_secs(10), &Default::default())
                .await
                .unwrap()
        );
        assert!(matches!(
            jobs.snapshot(&id).unwrap().status,
            CommandJobStatus::Exited { code: Some(0), .. }
        ));
        let output = CommandStatusTool::new(jobs.clone())
            .call(
                &mut ToolContext::new(),
                CommandStatusArgs {
                    id: Some(id.to_string()),
                    wait_seconds: None,
                },
            )
            .await
            .unwrap();
        assert!(output.contains(render::DETACHED_OUTPUT_NOTE), "{output}");
        assert!(output.contains("leader-done"), "{output}");
        let output = crate::CommandTerminateTool::new(jobs)
            .call(
                &mut ToolContext::new(),
                crate::CommandTerminateArgs { id: id.to_string() },
            )
            .await
            .unwrap();
        assert!(output.contains(render::DETACHED_OUTPUT_NOTE), "{output}");
    }

    #[tokio::test]
    async fn foreground_timeout_releases_detached_pipe_holder() {
        if !perl_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let _holder = HolderGuard(directory.path().join("holder.pid"));
        let jobs = CommandJobs::new(
            CommandJobLimits {
                foreground_timeout: Duration::from_secs(2),
                ..detached_limits()
            },
            None,
        );
        let tool = CommandTool::with_jobs(directory.path().into(), jobs);
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            tool.call(
                &mut ToolContext::new(),
                CommandArgs {
                    command: format!("{HOLDER}; sleep 30"),
                    background: None,
                },
            ),
        )
        .await
        .expect("timeout was pinned by detached output")
        .unwrap_err();
        assert!(matches!(error, CommandError::Timeout { seconds: 2, .. }));
        assert!(error.to_string().contains(render::DETACHED_OUTPUT_NOTE));
    }

    #[tokio::test]
    async fn model_termination_finishes_within_its_wait_with_detached_output() {
        if !perl_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let holder = HolderGuard(directory.path().join("holder.pid"));
        let jobs = CommandJobs::new(detached_limits(), None);
        let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: format!("trap '' TERM; {HOLDER}; sleep 30"),
                background: Some(true),
            },
        )
        .await
        .unwrap();
        wait_for_holder(&holder).await;
        let id = jobs.list_model_jobs()[0].id.clone();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            crate::CommandTerminateTool::new(jobs).call(
                &mut ToolContext::new(),
                crate::CommandTerminateArgs { id: id.to_string() },
            ),
        )
        .await
        .expect("termination was pinned by detached output")
        .unwrap();
        assert!(
            output.contains("status: terminated by the agent"),
            "{output}"
        );
        assert!(output.contains(render::DETACHED_OUTPUT_NOTE), "{output}");
    }

    #[tokio::test]
    async fn turn_cancellation_releases_foreground_with_detached_pipe_holder() {
        if !perl_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let holder = HolderGuard(directory.path().join("holder.pid"));
        let jobs = CommandJobs::new(detached_limits(), None);
        let tool = CommandTool::with_jobs(directory.path().into(), jobs);
        let token = tokio_util::sync::CancellationToken::new();
        let mut context = ToolContext::new();
        context.insert_scope(TurnContext::new(
            TurnId::new(1),
            SessionMode::Build,
            token.clone(),
        ));
        let worker = tokio::spawn(async move {
            let result = tool
                .call(
                    &mut context,
                    CommandArgs {
                        command: format!("{HOLDER}; sleep 30"),
                        background: None,
                    },
                )
                .await;
            (result, context)
        });
        wait_for_holder(&holder).await;
        token.cancel();
        let (result, context) = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("turn cancellation was pinned by detached output")
            .unwrap();
        let error = result.unwrap_err();
        assert!(matches!(error, CommandError::Cancelled { .. }));
        assert!(error.to_string().contains(render::DETACHED_OUTPUT_NOTE));
        assert!(context.result::<ToolCancelled>().unwrap().is_some());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_termination_wait_leaves_termination_request_active() {
    use crate::{CommandTerminateArgs, CommandTerminateTool};
    use zevria_foundation::{SessionMode, TurnId};
    let directory = tempfile::tempdir().unwrap();
    let jobs = CommandJobs::new(limits(), None);
    let tool = CommandTool::with_jobs(directory.path().into(), jobs.clone());
    tool.call(
        &mut ToolContext::new(),
        CommandArgs {
            command: "trap '' TERM; printf ready > ready; sleep 30".into(),
            background: Some(true),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !directory.path().join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = jobs.list_model_jobs()[0].id.clone();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let mut context = ToolContext::new();
    context.insert_scope(TurnContext::new(TurnId::new(1), SessionMode::Build, token));
    let error = tokio::time::timeout(
        Duration::from_millis(500),
        CommandTerminateTool::new(jobs.clone())
            .call(&mut context, CommandTerminateArgs { id: id.to_string() }),
    )
    .await
    .expect("termination wait ignored turn cancellation")
    .unwrap_err();
    assert!(
        matches!(error, CommandError::Cancelled { ref output } if output == "Termination wait cancelled; the termination request still applies and completes independently.")
    );
    assert!(context.result::<ToolCancelled>().unwrap().is_some());
    assert!(
        jobs.wait_terminal(&id, Duration::from_secs(10), &Default::default())
            .await
            .unwrap()
    );
    assert_eq!(
        jobs.snapshot(&id).unwrap().status,
        CommandJobStatus::Terminated {
            origin: TerminationOrigin::Model
        }
    );
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
