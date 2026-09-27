//! Provider-neutral subtask contracts: typed identifiers, lifecycle statuses,
//! and the channel handles connecting the `launch_subtasks` tool and a
//! composition-root supervisor.
//!
//! A subtask is an ordinary blocking tool call: the tool launches the child
//! through a cloneable [`SubtaskLauncher`] and awaits the returned oneshot
//! receiver. The supervisor (owned by the application crate) receives
//! [`SubtaskLaunchRequest`]s, runs each child, and resolves the request's
//! oneshot sender with exactly one terminal [`SubtaskOutcome`] — dropping the
//! sender (an aborted supervisor) surfaces to the tool as a failed call, never
//! a wedge. Launching also announces the child to the frontend immediately via
//! [`SessionEvent::SubtaskLaunched`], so its live pane opens before the report
//! arrives.

use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};

use crate::{SessionEvent, SessionEventSender, TurnContext};
use zevria_foundation::subtask::*;

/// One parent's component-aware workspace ownership table. Launcher clones
/// share it; independent root sessions do not. Never lock across I/O or await.
#[derive(Clone, Default)]
struct WorkspaceReservations(Arc<Mutex<BTreeSet<PathBuf>>>);

/// A rejected reservation, identifying a directory rather than inventing an
/// owner title. Resolution failures also retain the requested path.
#[derive(Debug)]
pub enum WorkspaceConflict {
    Overlap { path: PathBuf },
    InvalidPath { path: PathBuf, error: String },
}

impl fmt::Display for WorkspaceConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overlap { path } => write!(
                f,
                "workspace overlaps reserved directory {}; choose a non-overlapping directory or wait for its child to finish",
                path.display()
            ),
            Self::InvalidPath { path, error } => {
                write!(f, "cannot reserve workspace {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkspaceConflict {}

/// Non-cloneable ownership of a directory. Dropping this guard releases only
/// the table entry: generated and partial artifacts are never removed.
pub struct WorkspaceReservation {
    path: PathBuf,
    reservations: WorkspaceReservations,
}

impl WorkspaceReservation {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for WorkspaceReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceReservation")
            .field("path", &self.path)
            .finish()
    }
}

impl Drop for WorkspaceReservation {
    fn drop(&mut self) {
        self.reservations
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.path);
    }
}

/// A prepared child tool root and its live ownership. Metadata contains only
/// `display`, never the reservation or an absolute lock key.
#[derive(Debug)]
pub struct ChildWorkspace {
    pub path: PathBuf,
    pub display: String,
    _reservation: WorkspaceReservation,
}

impl ChildWorkspace {
    pub fn new(
        path: PathBuf,
        display: String,
        reservation: WorkspaceReservation,
    ) -> anyhow::Result<Self> {
        let canonical = fs::canonicalize(&path)?;
        anyhow::ensure!(
            canonical == path && canonical == reservation.path && canonical.is_dir(),
            "workspace identity changed during preparation; retry the launch"
        );
        Ok(Self {
            path,
            display,
            _reservation: reservation,
        })
    }
}

/// Resolve canonical existing ancestors while allowing a missing normal suffix.
/// `symlink_metadata` distinguishes a dangling link from a missing directory.
fn reservation_key(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "an absolute workspace target is required"
    );
    anyhow::ensure!(
        !path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
        "workspace target must be normalized"
    );
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut key = fs::canonicalize(ancestor)?;
                anyhow::ensure!(key.is_dir(), "workspace ancestor is not a directory");
                for component in suffix.iter().rev() {
                    key.push(component);
                }
                return Ok(key);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!("no existing workspace ancestor"))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("no existing workspace ancestor"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// One queued request for the supervisor: the launched descriptor (status
/// [`SubtaskStatus::Starting`]), the self-contained child prompt, and the
/// oneshot sender the supervisor must resolve with the terminal outcome.
#[derive(Debug)]
pub struct SubtaskLaunchRequest {
    pub descriptor: SubtaskDescriptor,
    pub prompt: String,
    pub turn: TurnContext,
    pub workspace: Option<ChildWorkspace>,
    /// Resolved exactly once with the child's terminal outcome. Dropping it
    /// unresolved fails the awaiting tool call instead of wedging it.
    pub outcome: oneshot::Sender<SubtaskOutcome>,
}

/// Cloneable launch handle held by the `launch_subtasks` tool. Launching queues
/// a supervisor request, announces the child to the frontend, and returns the
/// receiver the tool awaits for the terminal outcome.
#[derive(Clone)]
pub struct SubtaskLauncher {
    parent_session_id: String,
    requests: Sender<SubtaskLaunchRequest>,
    events: SessionEventSender,
    reservations: WorkspaceReservations,
}

impl SubtaskLauncher {
    pub fn reserve_workspace(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<WorkspaceReservation, WorkspaceConflict> {
        Ok(self
            .reserve_workspaces(&[path.as_ref().to_path_buf()])?
            .remove(0))
    }

    /// Atomically reserve a complete batch. Resolve aliases before locking;
    /// check all pairwise and existing conflicts before inserting any key.
    /// No filesystem I/O or await occurs under the ownership-table lock.
    pub fn reserve_workspaces(
        &self,
        paths: &[PathBuf],
    ) -> Result<Vec<WorkspaceReservation>, WorkspaceConflict> {
        let paths = paths
            .iter()
            .map(|requested| {
                reservation_key(requested).map_err(|error| WorkspaceConflict::InvalidPath {
                    path: requested.clone(),
                    error: error.to_string(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut occupied = self
            .reservations
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (index, path) in paths.iter().enumerate() {
            if let Some(overlap) = occupied
                .iter()
                .chain(&paths[..index])
                .find(|entry| entry.starts_with(path) || path.starts_with(entry))
            {
                return Err(WorkspaceConflict::Overlap {
                    path: overlap.clone(),
                });
            }
        }
        occupied.extend(paths.iter().cloned());
        Ok(paths
            .into_iter()
            .map(|path| WorkspaceReservation {
                path,
                reservations: self.reservations.clone(),
            })
            .collect())
    }

    /// Queue a launch and return its metadata plus the outcome receiver.
    /// `call_id` correlates the frontend's live child row with the pending
    /// tool call. Waits for queue capacity unless the turn is cancelled or the
    /// supervisor has shut down; inconsistent kind/workspace pairs are rejected.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        &self,
        call_id: impl Into<String>,
        entry_index: usize,
        title: impl Into<String>,
        kind: SubtaskKind,
        prompt: impl Into<String>,
        turn: TurnContext,
        workspace: Option<ChildWorkspace>,
    ) -> anyhow::Result<(SubtaskLaunchMetadata, oneshot::Receiver<SubtaskOutcome>)> {
        anyhow::ensure!(
            kind != SubtaskKind::Build || turn.build_subtasks,
            "Builder subtasks require an explicit /orchestrate <prompt> request; use `explore` where available or ask the user to submit `/orchestrate <prompt>` in Build"
        );
        anyhow::ensure!(
            matches!(
                (kind, &workspace),
                (SubtaskKind::Build, Some(_)) | (SubtaskKind::Explore, None)
            ),
            "build subtasks require a workspace; explore subtasks must not have one"
        );
        let cancellation = turn.cancellation().clone();
        let id = SubtaskId::generate();
        let descriptor = SubtaskDescriptor {
            id: id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            title: title.into(),
            kind,
            workspace: workspace
                .as_ref()
                .map(|workspace| workspace.display.clone()),
            status: SubtaskStatus::Starting,
        };
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let request = SubtaskLaunchRequest {
            descriptor: descriptor.clone(),
            prompt: prompt.into(),
            turn: turn.clone(),
            workspace,
            outcome: outcome_tx,
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => anyhow::bail!("subtask launch cancelled before enqueue"),
            sent = self.requests.send(request) => {
                sent.map_err(|_| anyhow::anyhow!("the subtask supervisor is not running; launch_subtasks is unavailable"))?;
            }
        }
        // Announced only after the request is queued, so a row never opens
        // for a launch that failed.
        let _ = self
            .events
            .send(SessionEvent::SubtaskLaunched {
                turn_id: turn.id,
                call_id: call_id.into(),
                entry_index,
                descriptor: descriptor.clone(),
            })
            .await;
        Ok((
            SubtaskLaunchMetadata {
                id,
                title: descriptor.title,
                kind,
                workspace: descriptor.workspace,
            },
            outcome_rx,
        ))
    }
}

/// The connected pair of subtask handles for one root session.
pub struct SubtaskChannels {
    /// Held by the `launch_subtasks` tool.
    pub launcher: SubtaskLauncher,
    /// Consumed by the supervisor's accept loop.
    pub requests: Receiver<SubtaskLaunchRequest>,
}

/// Build the launcher/requests pair. `events` receives the immediate
/// [`SessionEvent::SubtaskLaunched`] announcements.
pub fn subtask_channels(
    parent_session_id: impl Into<String>,
    events: SessionEventSender,
) -> SubtaskChannels {
    subtask_channels_with_capacity(parent_session_id, events, 4)
}

pub fn subtask_channels_with_capacity(
    parent_session_id: impl Into<String>,
    events: SessionEventSender,
    capacity: usize,
) -> SubtaskChannels {
    let (request_tx, request_rx) = tokio::sync::mpsc::channel(capacity.max(1));
    SubtaskChannels {
        launcher: SubtaskLauncher {
            parent_session_id: parent_session_id.into(),
            requests: request_tx,
            events,
            reservations: WorkspaceReservations::default(),
        },
        requests: request_rx,
    }
}

#[cfg(test)]
#[path = "subtask_tests.rs"]
mod workspace_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionMode, TurnId, session_event_channel};
    use tokio_util::sync::CancellationToken;

    fn channels() -> (SubtaskChannels, crate::SessionEventReceiver) {
        let (events_tx, events_rx) = session_event_channel(32);
        (subtask_channels("root-session", events_tx), events_rx)
    }

    async fn launch(
        channels: &SubtaskChannels,
        call_id: &str,
        title: &str,
    ) -> (SubtaskLaunchMetadata, oneshot::Receiver<SubtaskOutcome>) {
        channels
            .launcher
            .launch(
                call_id,
                0,
                title,
                SubtaskKind::Explore,
                format!("{title} prompt with full context"),
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
                None,
            )
            .await
            .expect("launch should queue")
    }

    #[tokio::test]
    async fn launch_queues_a_request_and_announces_the_child() {
        let (mut channels, mut events) = channels();
        let (metadata, _outcome) = launch(&channels, "call-1", "find config loading").await;

        let request = channels.requests.try_recv().expect("request queued");
        assert_eq!(request.descriptor.id, metadata.id);
        assert_eq!(request.descriptor.parent_session_id, "root-session");
        assert_eq!(request.descriptor.status, SubtaskStatus::Starting);
        assert_eq!(
            request.prompt,
            "find config loading prompt with full context"
        );

        let event = events.try_recv().expect("launched event");
        assert!(matches!(
            event,
            crate::SessionUpdate::Lifecycle(SessionEvent::SubtaskLaunched {
                call_id,
                descriptor,
                ..
            })
                if call_id == "call-1"
                    && descriptor.id == metadata.id
                    && descriptor.title == "find config loading"
        ));
    }

    #[tokio::test]
    async fn closed_supervisor_fails_the_launch_without_announcing() {
        let (mut channels, mut events) = channels();
        channels.requests.close();
        let error = channels
            .launcher
            .launch(
                "call-1",
                0,
                "a task title",
                SubtaskKind::Explore,
                "prompt",
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
                None,
            )
            .await
            .expect_err("closed channel should fail");
        assert!(error.to_string().contains("supervisor"));
        assert!(events.try_recv().is_err(), "no row for a failed launch");
    }

    #[tokio::test]
    async fn the_outcome_oneshot_delivers_the_report_to_the_launch_caller() {
        let (mut channels, _events) = channels();
        let (metadata, outcome) = launch(&channels, "call-1", "trace event flow").await;
        let request = channels.requests.try_recv().expect("request queued");
        assert_eq!(request.descriptor.id, metadata.id);

        request
            .outcome
            .send(SubtaskOutcome::Completed {
                report: "the answer".to_string(),
            })
            .expect("receiver is alive");
        assert_eq!(
            outcome.await.expect("resolved outcome"),
            SubtaskOutcome::Completed {
                report: "the answer".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_dropped_request_fails_the_awaiting_caller_instead_of_wedging() {
        let (mut channels, _events) = channels();
        let (_metadata, outcome) = launch(&channels, "call-1", "abandoned child task").await;
        // An aborted supervisor drops the request without resolving it.
        drop(channels.requests.try_recv().expect("request queued"));
        assert!(outcome.await.is_err());
    }

    #[test]
    fn ids_serialize_transparently_and_statuses_snake_case() {
        let id = SubtaskId::new("abc");
        assert_eq!(serde_json::to_string(&id).expect("id"), "\"abc\"");
        assert_eq!(
            serde_json::to_string(&SubtaskStatus::Running).expect("status"),
            "\"running\""
        );
        assert_eq!(SubtaskKind::Explore.to_string(), "explore");
    }
}
