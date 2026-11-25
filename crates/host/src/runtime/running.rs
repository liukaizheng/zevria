//! Background task ownership and ordered session shutdown.

use std::{future::Future, sync::Arc};

use anyhow::Context as _;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zevria_session_api::{CommandJobs, SessionCommand, SessionEventReceiver, SessionEventSender};
use zevria_transcript::transcript;

use super::SessionIdentity;

#[cfg(test)]
use tokio::sync::mpsc::unbounded_channel;

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
pub(super) const MCP_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

async fn join_with_grace(
    mut task: tokio::task::JoinHandle<()>,
    name: &str,
    grace: std::time::Duration,
) -> anyhow::Result<()> {
    match tokio::time::timeout(grace, &mut task).await {
        Ok(result) => result.with_context(|| format!("{name} task failed")),
        Err(_) => {
            tracing::warn!(target: "zevria::runtime",
                task = name,
                "task exceeded the shutdown grace period; aborting"
            );
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => Ok(()),
                Ok(()) => Ok(()),
                Err(error) => Err(anyhow::Error::from(error)
                    .context(format!("{name} task failed while being aborted"))),
            }
        }
    }
}

/// One observed engine/supervisor task ending while a frontend is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTaskExit {
    pub component: &'static str,
    pub error: Option<String>,
}

pub(super) struct SubtaskSupervisor {
    pub(super) shutdown: tokio_util::sync::CancellationToken,
    pub(super) task: tokio::task::JoinHandle<()>,
}

/// Complete running-session ownership. Frontends receive cloned command
/// senders and take the sole event receiver, while this handle retains task,
/// transcript, and shutdown ownership.
pub struct RunningSession {
    pub(super) mcp: Option<Arc<zevria_mcp::McpManager>>,
    pub(super) command_jobs: Option<CommandJobs>,
    // Only existing histories use conservative legacy cleanup. A new pending
    // path may acquire a competing file after allocation; shutdown must not
    // delete someone else's file after rejected no-clobber publication.
    pub(super) cleanup_abandoned_history: bool,
    pub(super) identity: SessionIdentity,
    // Held until background shutdown and abandoned-root cleanup finish. The
    // engine also owns an Arc: dropping a frontend must not release a live engine.
    pub(super) _lease: Option<Arc<crate::session_lease::RootSessionLease>>,
    pub(super) command_tx: Option<UnboundedSender<SessionCommand>>,
    pub(super) event_tx: Option<SessionEventSender>,
    pub(super) event_rx: Option<SessionEventReceiver>,
    pub(super) exit_rx: Option<UnboundedReceiver<RuntimeTaskExit>>,
    pub(super) engine_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) supervisor: Option<SubtaskSupervisor>,
}

impl RunningSession {
    pub fn identity(&self) -> &SessionIdentity {
        &self.identity
    }

    pub fn command_sender(&self) -> UnboundedSender<SessionCommand> {
        self.command_tx
            .as_ref()
            .expect("running session command endpoint already closed")
            .clone()
    }

    pub fn take_event_receiver(&mut self) -> anyhow::Result<SessionEventReceiver> {
        self.event_rx
            .take()
            .context("running session event receiver was already taken")
    }

    pub fn take_exit_receiver(&mut self) -> anyhow::Result<UnboundedReceiver<RuntimeTaskExit>> {
        self.exit_rx
            .take()
            .context("running session background-exit receiver was already taken")
    }

    /// Preserve the two-phase ownership sequence: signal Shutdown, drop
    /// command and event endpoints, await the engine first, then concurrently
    /// shut down the independent leaf subsystems (subtask supervisor, command
    /// jobs, and MCP manager), and finally remove an eligible existing
    /// abandoned transcript best-effort. An absent pending transcript is normal
    /// and shutdown does not materialize it.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(command_tx) = self.command_tx.take() {
            let _ = command_tx.send(SessionCommand::Control(
                zevria_session_api::ControlCommand::Shutdown,
            ));
            drop(command_tx);
        }
        self.event_rx.take();
        self.event_tx.take();
        self.exit_rx.take();

        // Phase 1: Stop the coordinator (engine) first so transcript durability
        // completes and no new subtasks, command jobs, or MCP calls can start.
        let engine_shutdown = if let Some(engine_task) = self.engine_task.take() {
            join_with_grace(engine_task, "session engine", SHUTDOWN_GRACE).await
        } else {
            Ok(())
        };

        // Phase 2: Tear down all remaining independent leaf subsystems concurrently.
        let supervisor = self.supervisor.take();
        let command_jobs = self.command_jobs.take();
        let mcp = self.mcp.take();

        let (supervisor_shutdown, (), ()) = tokio::join!(
            async {
                if let Some(supervisor) = supervisor {
                    supervisor.shutdown.cancel();
                    join_with_grace(supervisor.task, "subtask supervisor", SHUTDOWN_GRACE).await
                } else {
                    Ok(())
                }
            },
            async {
                if let Some(jobs) = command_jobs {
                    jobs.shutdown(SHUTDOWN_GRACE).await;
                }
            },
            async {
                if let Some(mcp) = mcp {
                    mcp.shutdown(MCP_SHUTDOWN_GRACE).await;
                }
            },
        );

        let path = self.identity.transcript_path;
        let cleanup = self.cleanup_abandoned_history;
        let lease = self._lease.take();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            if cleanup && transcript::is_abandoned_root(&path)
                && let Err(error) = std::fs::remove_file(&path)
            {
                tracing::warn!(target: "zevria::runtime", path = %path.display(), %error, "failed to remove the empty session file");
            }
            drop(lease);
        }).await {
            tracing::warn!(target: "zevria::runtime", %error, "session cleanup task failed");
        }

        match (engine_shutdown, supervisor_shutdown) {
            (Err(engine), Err(supervisor)) => Err(anyhow::anyhow!("{engine:#}; {supervisor:#}")),
            (result, Ok(())) | (Ok(()), result) => result,
        }
    }
}

pub(super) trait RuntimeTaskOutcome {
    fn error(self) -> Option<String>;
}

impl RuntimeTaskOutcome for () {
    fn error(self) -> Option<String> {
        None
    }
}

impl RuntimeTaskOutcome for Result<(), zevria_transcript::SessionReplayError> {
    fn error(self) -> Option<String> {
        self.err().map(|error| error.to_string())
    }
}

pub(super) fn spawn_observed<F, T, G>(
    component: &'static str,
    future: F,
    exits: UnboundedSender<RuntimeTaskExit>,
    guard: G,
) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = T> + Send + 'static,
    T: RuntimeTaskOutcome,
    G: Send + 'static,
{
    struct ExitGuard<G> {
        component: &'static str,
        exits: UnboundedSender<RuntimeTaskExit>,
        reported: bool,
        _retained: G,
    }

    impl<G> Drop for ExitGuard<G> {
        fn drop(&mut self) {
            if !self.reported {
                let _ = self.exits.send(RuntimeTaskExit {
                    component: self.component,
                    error: Some(format!("{} task aborted or panicked", self.component)),
                });
            }
        }
    }

    // Construct the guard outside the task so aborting it before its first
    // poll still reports an exit and releases retained resources.
    let mut guard = ExitGuard {
        component,
        exits,
        reported: false,
        _retained: guard,
    };
    tokio::spawn(async move {
        let error = future.await.error();
        guard.reported = true;
        let _ = guard.exits.send(RuntimeTaskExit { component, error });
    })
}

#[cfg(test)]
#[tokio::test]
async fn shutdown_timeout_drops_the_observed_future_and_guard() {
    let (exits, mut received) = unbounded_channel();
    let future_resource = Arc::new(());
    let guard_resource = Arc::new(());
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = spawn_observed(
        "test",
        {
            let resource = future_resource.clone();
            async move {
                let _resource = resource;
                let _ = started.send(());
                std::future::pending::<()>().await;
            }
        },
        exits,
        guard_resource.clone(),
    );
    ready.await.unwrap();
    join_with_grace(task, "test", std::time::Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(Arc::strong_count(&future_resource), 1);
    assert_eq!(Arc::strong_count(&guard_resource), 1);
    let exit = received.recv().await.unwrap();
    assert_eq!(exit.component, "test");
    assert!(exit.error.unwrap().contains("aborted or panicked"));
    assert!(received.recv().await.is_none());
}
