//! Bounded dependency probes; no configuration, terminal, or global environment changes.
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
            Ok::<_, std::io::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr.take(cap as u64 + 1).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let wait = async {
            let status = child.wait().await;
            tree.kill();
            status
        };
        let (status, stdout, stderr) = tokio::try_join!(wait, read(stdout), err)?;
        anyhow::ensure!(
            stdout.len() <= cap && stderr.len() <= cap,
            "dependency probe exceeded its {cap}-byte output limit"
        );
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
            anyhow::bail!(
                "dependency probe timed out after {} seconds",
                timeout.as_secs()
            )
        }
    }
}
