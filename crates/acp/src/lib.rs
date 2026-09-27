//! ACP V1 frontend for Zevria's provider-neutral session engine.
//!
//! The crate owns protocol and frontend state only. Provider connections,
//! production tools, transcript writers, and supervisors are supplied by a
//! [`SessionRuntimeFactory`] implemented by the binary composition root.

mod agent;
#[cfg(test)]
mod display_tests;
mod elicitation;
mod project;
pub mod prompt;
mod replay;
mod session;
pub mod skills;
mod stream;

use std::{future::Future, path::PathBuf, pin::Pin, time::SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zevria_foundation::SessionMode;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEventReceiver;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanWorkflowState;

pub use agent::serve_stdio;

/// Explicit runtime contract; never inferred from a client identity or prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionProfile {
    #[default]
    Interactive,
    EnsembleWorker,
}

impl ExecutionProfile {
    pub fn is_worker(self) -> bool {
        self == Self::EnsembleWorker
    }
}

/// ACP server resource and discovery settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcpConfig {
    /// Maximum number of simultaneously active Zevria runtimes.
    pub max_sessions: usize,
    /// Whether `session/list` is advertised and accepted.
    pub expose_session_list: bool,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            max_sessions: 4,
            expose_session_list: true,
        }
    }
}

impl AcpConfig {
    /// Validate resource limits before opening the stdio transport.
    pub fn validate(self) -> anyhow::Result<()> {
        if self.max_sessions == 0 {
            anyhow::bail!("acp.max_sessions must be greater than zero");
        }
        Ok(())
    }
}

/// Whether a runtime starts a fresh durable transcript or an exact existing
/// transcript selected by its public session ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStart {
    New,
    Existing { session_id: String },
}

/// Provider-neutral request passed from the ACP frontend to the composition
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartSessionRequest {
    pub workspace: PathBuf,
    pub start: SessionStart,
}

/// Bounded metadata exposed through ACP `session/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub id: String,
    pub workspace: PathBuf,
    pub modified: SystemTime,
    pub preview: Option<String>,
}

/// Terminal state of one engine or supervisor background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeExit {
    pub component: String,
    pub error: Option<String>,
}

impl RuntimeExit {
    pub fn clean(component: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            error: None,
        }
    }

    pub fn failed(component: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            error: Some(error.into()),
        }
    }
}

/// Object-safe owner of one production runtime's shutdown sequence.
pub trait SessionRuntimeLifecycle: Send {
    /// Cancel active work and release all provider, transcript, and supervisor
    /// resources. Implementations must be safe to call after a background task
    /// has already exited.
    fn shutdown(self: Box<Self>) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
}

/// One fully started, transcript-backed runtime handed to the ACP frontend.
///
/// The frontend owns the command sender and event receiver. It must drop both
/// before awaiting [`SessionRuntimeLifecycle::shutdown`], preserving Zevria's
/// established ownership order.
pub struct StartedSession {
    pub session_id: String,
    pub workspace: PathBuf,
    pub transcript_items: Vec<TranscriptItem>,
    /// Explicit engine selection, independent of retained Plan artifacts.
    pub selected_mode: SessionMode,
    pub plan_state: PlanWorkflowState,
    pub startup_notices: Vec<String>,
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub events: SessionEventReceiver,
    pub background_exit: Pin<Box<dyn Future<Output = RuntimeExit> + Send>>,
    pub lifecycle: Box<dyn SessionRuntimeLifecycle>,
}

/// Composition-root seam used by the ACP frontend and scripted tests.
pub trait SessionRuntimeFactory: Send + Sync + 'static {
    /// The frontend and runtime share this one typed profile. Ordinary host
    /// implementations retain their existing behavior by default.
    fn profile(&self) -> ExecutionProfile {
        ExecutionProfile::Interactive
    }

    fn start(
        &self,
        request: StartSessionRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<StartedSession>> + Send + '_>>;

    fn list(
        &self,
        workspace: PathBuf,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<SessionDescriptor>>> + Send + '_>>;
}

/// Cloneable initialized-client state shared by active sessions.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientState {
    pub form_elicitation: bool,
    pub plan_operations: bool,
}

#[cfg(test)]
mod tests;
