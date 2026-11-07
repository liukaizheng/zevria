use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use zevria_foundation::{CommandJobStatus, shell::ShellSpec};
use zevria_session_api::{CommandStream, JobHandle};
pub(super) const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Spawn synchronously so launch errors remain ordinary tool errors. The task
/// owns the child and process-tree guard independently of the tool future.
pub(super) fn spawn_job(
    shell: &ShellSpec,
    cwd: &std::path::Path,
    text: String,
    job: JobHandle,
) -> std::io::Result<()> {
    let mut command = shell.command(text);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let (mut child, mut tree) = ProcessTreeGuard::spawn(&mut command)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let worker = job.clone();
    let task = tokio::spawn(async move {
        // These futures are polled concurrently with the supervisor; aborting
        // it drops the readers too, rather than detaching drainer tasks.
        let stdout = drain(stdout, worker.clone(), CommandStream::Stdout);
        let stderr = drain(stderr, worker.clone(), CommandStream::Stderr);
        let supervise = async {
            let mut termination = worker.termination_requested();
            let mut deadline = worker.deadline();
            loop {
                let requested = *termination.borrow_and_update();
                if let Some(origin) = requested {
                    terminate_process_tree(&mut child, &tree).await;
                    break CommandJobStatus::Terminated { origin };
                }
                let due = *deadline.borrow_and_update();
                tokio::select! {
                    biased;
                    status = child.wait() => {
                        tree.kill_remaining();
                        break match status { Ok(status) => CommandJobStatus::Exited { code: status.code(), success: status.success() }, Err(error) => CommandJobStatus::Failed { error: format!("failed while waiting for command: {error}") } };
                    }
                    _ = termination.changed() => {},
                    _ = deadline.changed() => {},
                    () = async { if let Some(due) = due { tokio::time::sleep_until(due).await; } else { std::future::pending::<()>().await; } } => {
                        let seconds = due.map(|d| worker.deadline_seconds(d)).unwrap_or_default();
                        terminate_process_tree(&mut child, &tree).await;
                        break CommandJobStatus::TimedOut { seconds };
                    }
                }
            }
        };
        let (status, stdout, stderr) = tokio::join!(supervise, stdout, stderr);
        let status = match stdout.and(stderr) {
            Ok(()) => status,
            Err(error) => CommandJobStatus::Failed {
                error: error.to_string(),
            },
        };
        tree.disarm();
        worker.finish(status).await;
    });
    job.set_supervisor(task.abort_handle());
    Ok(())
}
async fn drain(
    mut stream: impl AsyncRead + Unpin,
    job: JobHandle,
    kind: CommandStream,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            return Ok(());
        }
        job.append(kind, &buffer[..n]);
    }
}
#[cfg(test)]
pub(super) async fn capture_stream(
    mut stream: impl AsyncRead + Unpin,
    max: usize,
) -> std::io::Result<String> {
    let mut buffer = zevria_session_api::StreamBuffer::new(max);
    let mut bytes = [0; 8192];
    loop {
        let n = stream.read(&mut bytes).await?;
        if n == 0 {
            return Ok(buffer.render_retained());
        }
        buffer.append(&bytes[..n]);
    }
}

struct ProcessTreeGuard {
    #[cfg(windows)]
    job: zevria_foundation::windows_process::Job,
    #[cfg(unix)]
    process_group: Option<i32>,
    armed: bool,
}

impl ProcessTreeGuard {
    fn spawn(
        command: &mut tokio::process::Command,
    ) -> std::io::Result<(tokio::process::Child, Self)> {
        #[cfg(windows)]
        let (child, job) = zevria_foundation::windows_process::Job::spawn(command)?;
        #[cfg(not(windows))]
        let child = command.spawn()?;
        let guard = Self {
            #[cfg(windows)]
            job,
            #[cfg(unix)]
            process_group: child.id().and_then(|pid| i32::try_from(pid).ok()),
            armed: true,
        };
        Ok((child, guard))
    }

    fn terminate_gracefully(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: the child was launched into a process group whose id is
            // the recorded leader pid. A negative pid targets only that group.
            unsafe {
                libc::kill(-process_group, libc::SIGTERM);
            }
        }
    }

    fn kill_remaining(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: same process-group invariant as `terminate_gracefully`.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            // Synchronous best effort is intentional: this guard also runs
            // when the async command worker itself is aborted.
            self.kill_remaining();
        }
    }
}

async fn terminate_process_tree(
    child: &mut tokio::process::Child,
    process_tree: &ProcessTreeGuard,
) {
    process_tree.terminate_gracefully();
    if matches!(
        tokio::time::timeout(TERMINATION_GRACE, child.wait()).await,
        Ok(Ok(_))
    ) {
        // The leader exited, but descendants may still own output pipes.
        process_tree.kill_remaining();
        return;
    }
    process_tree.kill_remaining();
    let _ = child.start_kill();
    let _ = child.wait().await;
}
