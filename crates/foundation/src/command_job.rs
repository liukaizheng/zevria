//! Shell command identities, display values, and verbatim completion notices.
use rig_core::message::Message;
use serde::{Deserialize, Serialize, de::Error as _};
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
    Exited {
        code: Option<i32>,
        success: bool,
        #[serde(default)]
        signal: Option<i32>,
    },
    Terminated {
        origin: TerminationOrigin,
    },
    TimedOut {
        seconds: u64,
    },
    Failed {
        error: String,
    },
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
            Self::Exited {
                signal: Some(signal),
                ..
            } => {
                write!(f, "killed by signal {signal}")?;
                #[cfg(unix)]
                if let Some(name) = signal_name(*signal) {
                    write!(f, " ({name})")?;
                }
                Ok(())
            }
            Self::Exited {
                code: Some(code), ..
            } => write!(f, "exited {code}"),
            Self::Exited { code: None, .. } => f.write_str("exited (code unavailable)"),
            Self::Terminated { origin } => f.write_str(match origin {
                TerminationOrigin::Model => "terminated by the agent",
                TerminationOrigin::User => "terminated by the user",
                TerminationOrigin::TurnCancelled => "cancelled with the turn",
                TerminationOrigin::Aborted => "aborted",
                TerminationOrigin::Shutdown => "stopped at session shutdown",
            }),
            Self::TimedOut { seconds } => write!(f, "timed out after {seconds}s"),
            Self::Failed { error } => write!(f, "failed: {error}"),
        }
    }
}

/// Portable Unix signal names; unknown and platform-specific numbers stay numeric.
#[cfg(unix)]
pub fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        libc::SIGURG => "SIGURG",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGVTALRM => "SIGVTALRM",
        libc::SIGPROF => "SIGPROF",
        libc::SIGWINCH => "SIGWINCH",
        _ => return None,
    })
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

/// The common compact job-line format used by notices, tools, and frontends.
pub fn command_job_line(snapshot: &CommandJobSnapshot) -> String {
    let collapsed = snapshot
        .command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = collapsed.chars();
    let mut preview: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    format!(
        "{} · {} · {} · {preview}",
        snapshot.id,
        snapshot.status,
        command_job_duration(snapshot)
    )
}

pub fn command_job_duration(snapshot: &CommandJobSnapshot) -> String {
    let elapsed = snapshot
        .finished_at
        .unwrap_or_else(SystemTime::now)
        .duration_since(snapshot.started_at)
        .unwrap_or_default();
    format!("{:.1}s", elapsed.as_secs_f64())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandJobNoticeKind {
    Update,
    Continuation,
}

/// Persisted model text is authoritative. The private user-message cache is
/// derived once from those exact bytes, never re-rendered from job snapshots.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandJobNotice {
    kind: CommandJobNoticeKind,
    jobs: Vec<CommandJobSnapshot>,
    running: Vec<CommandJobSnapshot>,
    text: String,
    model_message: Message,
}
impl Eq for CommandJobNotice {}
impl CommandJobNotice {
    pub fn new(
        kind: CommandJobNoticeKind,
        jobs: Vec<CommandJobSnapshot>,
        running: Vec<CommandJobSnapshot>,
        text: String,
    ) -> Self {
        assert!(!jobs.is_empty(), "a command job notice requires jobs");
        assert!(
            !text.trim().is_empty(),
            "a command job notice requires text"
        );
        let model_message = Message::user(text.clone());
        Self {
            kind,
            jobs,
            running,
            text,
            model_message,
        }
    }
    pub fn kind(&self) -> CommandJobNoticeKind {
        self.kind
    }
    pub fn jobs(&self) -> &[CommandJobSnapshot] {
        &self.jobs
    }
    pub fn running(&self) -> &[CommandJobSnapshot] {
        &self.running
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn model_message(&self) -> &Message {
        &self.model_message
    }
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines: Vec<_> = self.jobs.iter().map(command_job_line).collect();
        if !self.running.is_empty() {
            lines.push("Still running:".into());
            lines.extend(self.running.iter().map(command_job_line));
        }
        lines
    }
}
impl Serialize for CommandJobNotice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            version: u32,
            kind: CommandJobNoticeKind,
            jobs: &'a [CommandJobSnapshot],
            running: &'a [CommandJobSnapshot],
            text: &'a str,
        }
        Fields {
            version: 1,
            kind: self.kind,
            jobs: &self.jobs,
            running: &self.running,
            text: &self.text,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for CommandJobNotice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            version: u32,
            kind: CommandJobNoticeKind,
            jobs: Vec<CommandJobSnapshot>,
            running: Vec<CommandJobSnapshot>,
            text: String,
        }
        let fields = Fields::deserialize(deserializer)?;
        if fields.version != 1 {
            return Err(D::Error::custom(
                "unsupported command job notice version; expected 1",
            ));
        }
        if fields.jobs.is_empty() {
            return Err(D::Error::custom("a command job notice requires jobs"));
        }
        if fields.text.trim().is_empty() {
            return Err(D::Error::custom("a command job notice requires text"));
        }
        Ok(Self::new(
            fields.kind,
            fields.jobs,
            fields.running,
            fields.text,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display_phrases() {
        use CommandJobStatus::*;
        for (status, expected) in [
            (Running, "running"),
            (
                Exited {
                    code: Some(0),
                    success: true,
                    signal: None,
                },
                "exited 0",
            ),
            (
                Exited {
                    code: Some(101),
                    success: false,
                    signal: None,
                },
                "exited 101",
            ),
            (
                Exited {
                    code: None,
                    success: false,
                    signal: None,
                },
                "exited (code unavailable)",
            ),
            (
                Exited {
                    code: None,
                    success: false,
                    signal: Some(12345),
                },
                "killed by signal 12345",
            ),
            (TimedOut { seconds: 300 }, "timed out after 300s"),
            (
                Terminated {
                    origin: TerminationOrigin::Model,
                },
                "terminated by the agent",
            ),
            (
                Terminated {
                    origin: TerminationOrigin::User,
                },
                "terminated by the user",
            ),
            (
                Terminated {
                    origin: TerminationOrigin::TurnCancelled,
                },
                "cancelled with the turn",
            ),
            (
                Terminated {
                    origin: TerminationOrigin::Aborted,
                },
                "aborted",
            ),
            (
                Terminated {
                    origin: TerminationOrigin::Shutdown,
                },
                "stopped at session shutdown",
            ),
            (
                Failed {
                    error: "capture failed".into(),
                },
                "failed: capture failed",
            ),
        ] {
            assert_eq!(status.to_string(), expected);
        }
        #[cfg(unix)]
        {
            let status = Exited {
                code: None,
                success: false,
                signal: Some(libc::SIGKILL),
            };
            assert_eq!(
                status.to_string(),
                format!("killed by signal {} (SIGKILL)", libc::SIGKILL)
            );
            assert_eq!(signal_name(libc::SIGTERM), Some("SIGTERM"));
            assert_eq!(signal_name(12345), None);
        }
    }

    #[test]
    fn identity_and_serialization() {
        let id = CommandJobId::generate();
        assert_eq!(id.as_str().len(), 10);
        assert!(id.as_str().starts_with("job-"));
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
        let exited = CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        };
        assert_eq!(
            serde_json::from_value::<CommandJobStatus>(serde_json::json!({
                "type": "exited", "code": 0, "success": true
            }))
            .unwrap(),
            exited
        );
        let signalled = CommandJobStatus::Exited {
            code: None,
            success: false,
            signal: Some(9),
        };
        let serialized = serde_json::to_value(&signalled).unwrap();
        assert_eq!(serialized["signal"], 9);
        assert_eq!(
            serde_json::from_value::<CommandJobStatus>(serialized).unwrap(),
            signalled
        );
        assert!(!CommandJobStatus::Running.is_terminal());
        assert!(
            CommandJobStatus::Terminated {
                origin: TerminationOrigin::User
            }
            .is_terminal()
        );
    }
}
