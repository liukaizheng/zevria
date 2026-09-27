//! Subtask identities and results; reservations and execution belong to session API.
use crate::ModelRole;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// The registered name of the subtask-launching tool.
pub use crate::tool_names::LAUNCH_SUBTASKS_TOOL_NAME;

/// Unique subtask identifier. The UUID string doubles as the child transcript
/// file stem under the root session's subsession directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubtaskId(String);

impl SubtaskId {
    /// Generate a fresh v4 UUID identifier.
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SubtaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The kind of independent child agent. Tool roots and directory ownership
/// are cooperative isolation, not an OS sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtaskKind {
    Explore,
    Build,
}

impl SubtaskKind {
    pub const fn model_role(self) -> ModelRole {
        match self {
            Self::Explore => ModelRole::Explore,
            Self::Build => ModelRole::Builder,
        }
    }
}

impl fmt::Display for SubtaskKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Explore => "explore",
            Self::Build => "build",
        })
    }
}

/// Lifecycle status of one subtask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtaskStatus {
    /// Launched but not yet running a child session.
    Starting,
    /// The child session is executing.
    Running,
    /// The child produced a final report.
    Completed,
    /// The child failed before producing a final report.
    Failed,
    Cancelled,
}

impl SubtaskStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for SubtaskStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}

/// A subtask's identity and current lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtaskDescriptor {
    pub id: SubtaskId,
    pub parent_session_id: String,
    pub title: String,
    pub kind: SubtaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub status: SubtaskStatus,
}

/// Structured child identity carried by the subtask tool-result detail so it
/// reaches events and transcripts without entering model-visible messages.
/// Retained once launched, even if the child later fails or is cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtaskLaunchMetadata {
    pub id: SubtaskId,
    pub title: String,
    pub kind: SubtaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// One input-indexed terminal batch slot. Unaccepted entries have no child
/// identity. Reports and errors remain exclusively in the model-visible result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtaskEntryMetadata {
    pub index: usize,
    pub status: SubtaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<SubtaskLaunchMetadata>,
}

/// The terminal result of one subtask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubtaskOutcome {
    Completed { report: String },
    Failed { error: String },
    Cancelled,
}
