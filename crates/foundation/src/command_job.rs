//! Runtime-only shell command identities and display values.
use serde::{Deserialize, Serialize};
use std::{fmt, time::SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandJobId(String);

impl CommandJobId {
    pub fn generate() -> Self {
        Self(format!(
            "job-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        ))
    }
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for CommandJobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobMode {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationOrigin {
    Model,
    User,
    TurnCancelled,
    Aborted,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandJobStatus {
    Running,
    Exited { code: Option<i32>, success: bool },
    Terminated { origin: TerminationOrigin },
    TimedOut { seconds: u64 },
    Failed { error: String },
}
impl CommandJobStatus {
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}
impl fmt::Display for CommandJobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Exited { code, success } => write!(
                f,
                "exited (code {}, {})",
                code.map_or_else(|| "unavailable".into(), |c| c.to_string()),
                if *success { "success" } else { "failure" }
            ),
            Self::Terminated { origin } => write!(f, "terminated ({origin:?})"),
            Self::TimedOut { seconds } => write!(f, "timed out after {seconds}s"),
            Self::Failed { error } => write!(f, "failed: {error}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobSnapshot {
    pub id: CommandJobId,
    pub call_id: Option<String>,
    pub command: String,
    pub mode: CommandJobMode,
    pub status: CommandJobStatus,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobView {
    pub snapshot: CommandJobSnapshot,
    pub stdout_tail: String,
    pub stderr_tail: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobDisposition {
    StartedInBackground,
    MovedToBackground,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandJobDetail {
    pub id: CommandJobId,
    pub disposition: CommandJobDisposition,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_and_serialization() {
        let id = CommandJobId::generate();
        assert_eq!(id.as_str().len(), 10);
        assert!(id.as_str().starts_with("job-"));
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
        assert!(!CommandJobStatus::Running.is_terminal());
        assert!(
            CommandJobStatus::Terminated {
                origin: TerminationOrigin::User
            }
            .is_terminal()
        );
    }
}
