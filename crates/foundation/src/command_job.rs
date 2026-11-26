//! Shell command identities, display values, and verbatim completion notices.
use rig_core::message::Message;
use serde::{Deserialize, Serialize, de::Error as _};
use std::{
    borrow::Cow,
    fmt,
    time::{Duration, SystemTime},
};

pub const RETAINED_FINISHED_COMMAND_JOBS: usize = 8;
pub const COMMAND_JOB_FRONTEND_TAIL_BYTES: usize = 8 * 1024;

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
    command_job_line_for(snapshot, &snapshot.command)
}

pub fn command_job_line_at(snapshot: &CommandJobSnapshot, reference: SystemTime) -> String {
    command_job_line_with_duration(
        snapshot,
        &snapshot.command,
        Some(&command_job_duration_at(snapshot, reference)),
    )
}

/// Format a compact job line using a frontend's display command. The default
/// formatter above always keeps the original command (including for ACP).
pub fn command_job_line_for(snapshot: &CommandJobSnapshot, command: &str) -> String {
    command_job_line_with_duration(snapshot, command, Some(&command_job_duration(snapshot)))
}

fn command_job_line_with_duration(
    snapshot: &CommandJobSnapshot,
    command: &str,
    duration: Option<&str>,
) -> String {
    let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let mut preview: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    let header = format!("{} · {}", snapshot.id, snapshot.status);
    match duration {
        Some(duration) => format!("{header} · {duration} · {preview}"),
        None => format!("{header} · {preview}"),
    }
}

pub fn command_job_elapsed_at(snapshot: &CommandJobSnapshot, reference: SystemTime) -> Duration {
    snapshot
        .finished_at
        .unwrap_or(reference)
        .duration_since(snapshot.started_at)
        .unwrap_or_default()
}

pub fn command_job_duration_at(snapshot: &CommandJobSnapshot, reference: SystemTime) -> String {
    format_elapsed(command_job_elapsed_at(snapshot, reference))
}

pub fn command_job_duration(snapshot: &CommandJobSnapshot) -> String {
    command_job_duration_at(snapshot, SystemTime::now())
}

fn format_elapsed(elapsed: Duration) -> String {
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
    reported_at: Option<SystemTime>,
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
            reported_at: None,
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
    /// Add display timing without touching the persisted model message.
    pub fn with_reported_at(mut self, at: SystemTime) -> Self {
        self.reported_at = Some(at);
        self
    }
    pub fn reported_at(&self) -> Option<SystemTime> {
        self.reported_at
    }
    pub fn recorded_elapsed(&self, snapshot: &CommandJobSnapshot) -> Option<Duration> {
        let at = snapshot.finished_at.or(self.reported_at)?;
        Some(command_job_elapsed_at(snapshot, at))
    }
    pub fn recorded_duration_text(&self, snapshot: &CommandJobSnapshot) -> Option<String> {
        if let Some(elapsed) = self.recorded_elapsed(snapshot) {
            return Some(format_elapsed(elapsed));
        }
        // Prefer the trailing running section so captured output earlier in a
        // legacy notice doesn't masquerade as a frozen job duration.
        let text = self
            .text
            .rsplit_once("\n\nStill running:\n")
            .map_or(self.text.as_str(), |(_, running)| running);
        text.lines().find_map(|line| {
            let (id, rest) = line.split_once(" · ")?;
            if id != snapshot.id.as_str() {
                return None;
            }
            let (duration, _) = rest.strip_prefix("running · ")?.split_once(" · ")?;
            let seconds = duration.strip_suffix('s')?.parse().ok()?;
            Duration::try_from_secs_f64(seconds).ok()?;
            Some(duration.to_owned())
        })
    }
    pub fn summary_lines(&self) -> Vec<String> {
        self.summary_lines_with(|command| Cow::Borrowed(command))
    }
    /// Customize the displayed command, not the snapshot status or recorded timing.
    pub fn summary_lines_with(&self, command: impl Fn(&str) -> Cow<'_, str>) -> Vec<String> {
        let line = |job: &CommandJobSnapshot| {
            command_job_line_with_duration(
                job,
                &command(&job.command),
                self.recorded_duration_text(job).as_deref(),
            )
        };
        let mut lines: Vec<_> = self.jobs.iter().map(&line).collect();
        if !self.running.is_empty() {
            lines.push("Also running at the time:".into());
            lines.extend(self.running.iter().map(line));
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
            #[serde(skip_serializing_if = "Option::is_none")]
            reported_at: Option<SystemTime>,
        }
        Fields {
            version: 1,
            kind: self.kind,
            jobs: &self.jobs,
            running: &self.running,
            text: &self.text,
            reported_at: self.reported_at,
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
            #[serde(default)]
            reported_at: Option<SystemTime>,
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
        let mut notice = Self::new(fields.kind, fields.jobs, fields.running, fields.text);
        notice.reported_at = fields.reported_at;
        Ok(notice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished_job() -> CommandJobSnapshot {
        CommandJobSnapshot {
            id: CommandJobId::new("job-line"),
            call_id: None,
            command: "rtk proxy cargo\n test".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            },
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(2)),
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }

    #[test]
    fn compact_lines_and_default_notices_keep_exact_commands() {
        let job = finished_job();
        let exact = "job-line · exited 0 · 2.0s · rtk proxy cargo test";
        assert_eq!(command_job_line(&job), exact);
        assert_eq!(command_job_line_for(&job, &job.command), exact);
        assert_eq!(
            command_job_line_for(&job, "cargo test"),
            "job-line · exited 0 · 2.0s · cargo test"
        );
        let notice = CommandJobNotice::new(
            CommandJobNoticeKind::Update,
            vec![job.clone()],
            vec![job],
            "exact model text".into(),
        );
        assert_eq!(
            notice.summary_lines(),
            [exact, "Also running at the time:", exact]
        );
        let display = "job-line · exited 0 · 2.0s · cargo test";
        assert_eq!(
            notice.summary_lines_with(|_| Cow::Borrowed("cargo test")),
            [display, "Also running at the time:", display]
        );
        assert_eq!(notice.text(), "exact model text");
        let mut long = notice.jobs()[0].clone();
        long.command = "🐚".repeat(121);
        assert!(command_job_line(&long).ends_with(&format!("{}…", "🐚".repeat(119))));
    }

    #[test]
    fn timed_and_legacy_notices_keep_the_verbatim_model_message() {
        let finished = finished_job();
        let mut running = finished.clone();
        running.id = CommandJobId::new("job-running");
        running.status = CommandJobStatus::Running;
        running.finished_at = None;
        let at = running.started_at + Duration::from_millis(14_200);
        let line = "job-running · running · 14.2s · rtk proxy cargo test";
        assert_eq!(command_job_line_at(&running, at), line);
        assert_eq!(
            command_job_elapsed_at(&finished, at),
            Duration::from_secs(2)
        );
        assert_eq!(
            command_job_elapsed_at(&running, running.started_at),
            Duration::ZERO
        );
        let text = format!(
            "Command job update: these background jobs finished.\n\n<stdout>\njob-running · running · 999.9s · fake\n</stdout>\n\nStill running:\n{line}\n"
        );
        let notice = CommandJobNotice::new(
            CommandJobNoticeKind::Update,
            vec![finished],
            vec![running.clone()],
            text.clone(),
        )
        .with_reported_at(at);
        let mut json = serde_json::to_value(&notice).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(
            serde_json::from_value::<CommandJobNotice>(json.clone()).unwrap(),
            notice
        );
        json.as_object_mut().unwrap().remove("reported_at").unwrap();
        let legacy: CommandJobNotice = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&legacy).unwrap(), json);
        assert_eq!(notice.reported_at(), Some(at));
        assert_eq!(legacy.reported_at(), None);
        assert_eq!(
            notice.recorded_elapsed(&running),
            Some(Duration::from_millis(14_200))
        );
        assert_eq!(legacy.recorded_elapsed(&running), None);
        for restored in [&legacy, &notice] {
            assert_eq!(restored.text(), text);
            assert_eq!(restored.model_message(), &Message::user(text.clone()));
            assert_eq!(
                restored.recorded_duration_text(&running).as_deref(),
                Some("14.2s")
            );
            assert_eq!(restored.summary_lines().last().unwrap(), line);
        }
        let no_duration = CommandJobNotice::new(
            legacy.kind(),
            legacy.jobs().to_vec(),
            vec![running.clone()],
            "job-running · running · NaNs · invalid".into(),
        );
        assert_eq!(no_duration.recorded_duration_text(&running), None);
        assert_eq!(
            no_duration.summary_lines().last().unwrap(),
            "job-running · running · rtk proxy cargo test"
        );
    }

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
