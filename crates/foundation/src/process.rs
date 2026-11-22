//! Bounded subprocess execution; no terminal or global environment changes.
use std::{
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

pub struct ProcessTree {
    #[cfg(windows)]
    job: crate::windows_process::Job,
    #[cfg(unix)]
    group: i32,
}
impl ProcessTree {
    pub fn spawn(command: &mut Command) -> std::io::Result<(Child, Self)> {
        command.kill_on_drop(true);
        #[cfg(windows)]
        {
            let (child, job) = crate::windows_process::Job::spawn(command)?;
            Ok((child, Self { job }))
        }
        #[cfg(unix)]
        {
            command.process_group(0);
            let child = command.spawn()?;
            let group = child.id().expect("live child") as i32;
            Ok((child, Self { group }))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok((command.spawn()?, Self {}))
        }
    }
    pub fn terminate(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGTERM);
        }
    }
    pub fn kill(&self) {
        #[cfg(windows)]
        self.job.kill();
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Run a non-interactive process with bounded runtime and stdout/stderr capture.
/// Each stream is capped independently. The whole process tree is killed on
/// completion, timeout, output overflow, or cancellation of this future.
pub async fn bounded_output(
    command: &mut Command,
    timeout: Duration,
    cap: usize,
) -> anyhow::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, tree) = ProcessTree::spawn(command)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let work = async {
        let read = async |stream: tokio::process::ChildStdout| {
            let mut bytes = Vec::new();
            stream.take(cap as u64 + 1).read_to_end(&mut bytes).await?;
            anyhow::ensure!(
                bytes.len() <= cap,
                "process exceeded its {cap}-byte output limit"
            );
            Ok::<_, anyhow::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr.take(cap as u64 + 1).read_to_end(&mut bytes).await?;
            anyhow::ensure!(
                bytes.len() <= cap,
                "process exceeded its {cap}-byte output limit"
            );
            Ok::<_, anyhow::Error>(bytes)
        };
        let wait = async {
            let status = child.wait().await;
            tree.kill();
            status.map_err(anyhow::Error::from)
        };
        let (status, stdout, stderr) = tokio::try_join!(wait, read(stdout), err)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => {
            tree.kill();
            let _ = child.kill().await;
            anyhow::bail!("process timed out after {} seconds", timeout.as_secs())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_output_enforces_limits_and_reaps_background_pipe_holders() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf partial; sleep 10"]);
        let error = bounded_output(&mut command, Duration::from_millis(100), 1024)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 123456789"]);
        let error = bounded_output(&mut command, Duration::from_secs(2), 4)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("output limit"));

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10 & printf foreground"]);
        let output = bounded_output(&mut command, Duration::from_secs(2), 1024)
            .await
            .unwrap();
        assert_eq!(output.stdout, b"foreground");
    }

    #[tokio::test]
    async fn process_tree_terminate_delivers_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let marker = dir.path().join("sigterm");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap 'printf term > \"$MARKER\"; exit 0' TERM; printf ready > \"$READY\"; while :; do sleep 0.05; done",
            ])
            .env("READY", &ready)
            .env("MARKER", &marker);
        let (mut child, tree) = ProcessTree::spawn(&mut command).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child should install trap");
        tree.terminate();
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("child should exit on SIGTERM")
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "term");
    }
}
