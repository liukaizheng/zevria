//! Ephemeral root command jobs, never restored from transcript or turn-gated.
use crate::{shell, status_icon::StatusIcon};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use zevria_foundation::{
    COMMAND_JOB_FRONTEND_TAIL_BYTES, CommandJobId, CommandJobMode, CommandJobNotice,
    CommandJobSnapshot, CommandJobStatus, CommandJobView, RETAINED_FINISHED_COMMAND_JOBS,
    command_job_duration, command_job_line_for,
};
use zevria_session_api::JobEvent;
use zevria_session_api::{
    CommandControl, CommandControlAction as Request, CommandControlResult as Reply, ControlCommand,
    SessionCommand, SessionEvent,
};

pub const fn job_status_icon(status: &CommandJobStatus) -> StatusIcon {
    match status {
        CommandJobStatus::Running => StatusIcon::Running,
        CommandJobStatus::Exited { success: true, .. } => StatusIcon::Done,
        CommandJobStatus::Exited { .. }
        | CommandJobStatus::TimedOut { .. }
        | CommandJobStatus::Failed { .. } => StatusIcon::Failed,
        CommandJobStatus::Terminated { .. } => StatusIcon::Interrupted,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    pub icon: StatusIcon,
    pub command: shell::ShellDisplay,
    pub mode: &'static str,
    pub duration: String,
    pub status: String,
}

impl From<&CommandJobSnapshot> for JobRow {
    fn from(job: &CommandJobSnapshot) -> Self {
        Self {
            icon: job_status_icon(&job.status),
            command: compact_command(&job.command),
            mode: match job.mode {
                CommandJobMode::Foreground => "fg",
                CommandJobMode::Background => "bg",
            },
            duration: command_job_duration(job),
            status: job
                .status
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// Collapse the display text without re-lexing multi-line commands as a single shell statement.
fn compact_command(command: &str) -> shell::ShellDisplay {
    let display = shell::display(command);
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut space = false;
    let mut append = |text: &mut String, part: &str| {
        for ch in part.chars() {
            if ch.is_whitespace() {
                space = !text.is_empty();
            } else if !ch.is_control() {
                if std::mem::take(&mut space) {
                    text.push(' ');
                }
                text.push(ch);
            }
        }
    };
    let mut cursor = 0;
    for (range, token) in display.spans {
        append(&mut text, &display.text[cursor..range.start]);
        let mut start = text.len();
        append(&mut text, &display.text[range.clone()]);
        if text.as_bytes().get(start) == Some(&b' ') {
            start += 1;
        }
        if start < text.len() {
            spans.push((start..text.len(), token));
        }
        cursor = range.end;
    }
    append(&mut text, &display.text[cursor..]);
    shell::ShellDisplay { text, spans }
}

pub fn notice_rows(notice: &CommandJobNotice) -> (Vec<JobRow>, Vec<JobRow>) {
    (
        notice.jobs().iter().map(JobRow::from).collect(),
        notice.running().iter().map(JobRow::from).collect(),
    )
}

pub fn summary_title(jobs: &[CommandJobSnapshot]) -> String {
    let running = jobs.iter().filter(|job| !job.status.is_terminal()).count();
    format!(
        "Jobs · {running} running · {} finished",
        jobs.len() - running
    )
}

#[derive(Debug)]
enum Purpose {
    Inspect(Option<CommandJobId>, u64),
    Control(u64),
}

#[derive(Debug)]
pub enum JobsMessage {
    Accepted,
    Rejected(String),
}

/// Tails are the only per-inspection data kept outside the snapshot store.
#[derive(Debug)]
pub struct JobOutput {
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl From<&CommandJobView> for JobOutput {
    fn from(view: &CommandJobView) -> Self {
        Self {
            stdout_tail: view.stdout_tail.clone(),
            stderr_tail: view.stderr_tail.clone(),
            stdout_bytes: view.snapshot.stdout_bytes,
            stderr_bytes: view.snapshot.stderr_bytes,
        }
    }
}

/// The session-global command control surface shared by the desktop and TUI.
/// Inspect reads bounded, non-consuming tails; controls only target the given job.
#[derive(Debug)]
pub struct JobsInspector {
    prefix: &'static str,
    open: bool,
    jobs: Vec<CommandJobSnapshot>,
    inspect: Option<CommandJobId>,
    detail: Option<JobOutput>,
    pending: HashMap<String, Purpose>,
    generation: u64,
    last_refresh: Option<Instant>,
    refresh_again: bool,
    message: Option<JobsMessage>,
    pub commands: crate::outbox::CommandOutbox,
}

impl Default for JobsInspector {
    fn default() -> Self {
        Self::new("jobs-")
    }
}

impl JobsInspector {
    pub fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            open: false,
            jobs: Vec::new(),
            inspect: None,
            detail: None,
            pending: HashMap::new(),
            generation: 0,
            last_refresh: None,
            refresh_again: false,
            message: None,
            commands: Default::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn jobs(&self) -> &[CommandJobSnapshot] {
        &self.jobs
    }

    pub fn running_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| !job.status.is_terminal())
            .count()
    }

    pub fn job_for_call(&self, call: &str) -> Option<&CommandJobSnapshot> {
        self.jobs
            .iter()
            .rev()
            .find(|job| job.call_id.as_deref() == Some(call))
    }

    pub fn disconnect(&mut self) {
        self.open = false;
        self.inspect = None;
        self.detail = None;
        self.pending.clear();
        self.commands.clear();
        self.refresh_again = false;
        self.last_refresh = None;
        self.message = None;
        self.prune();
    }

    pub fn inspected(&self) -> Option<&CommandJobId> {
        self.inspect.as_ref()
    }

    pub fn detail(&self) -> Option<&JobOutput> {
        self.detail.as_ref()
    }

    pub fn message(&self) -> Option<&JobsMessage> {
        self.message.as_ref()
    }

    pub fn show(&mut self) {
        if !self.open {
            self.open = true;
            self.message = None;
            self.refresh();
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.back();
        self.message = None;
        self.refresh_again = false;
    }

    pub fn inspect(&mut self, id: CommandJobId) {
        if self.open && self.inspect.as_ref() != Some(&id) {
            self.inspect = Some(id);
            self.detail = None;
            self.prune();
            self.refresh();
        }
    }

    pub fn back(&mut self) {
        if self.inspect.take().is_some() {
            self.detail = None;
            self.prune();
            self.refresh();
        }
    }

    pub fn can_terminate(job: &CommandJobSnapshot) -> bool {
        !job.status.is_terminal()
    }

    pub fn can_background(job: &CommandJobSnapshot) -> bool {
        Self::can_terminate(job) && job.mode == CommandJobMode::Foreground
    }

    pub fn terminate(&mut self, id: CommandJobId) -> Option<String> {
        self.control(Request::Terminate { id })
    }

    pub fn move_to_background(&mut self, id: CommandJobId) -> Option<String> {
        self.control(Request::MoveToBackground { id })
    }

    fn control(&mut self, action: Request) -> Option<String> {
        if self.send(action, Purpose::Control(self.generation)) {
            return None;
        }
        self.reject("Too many pending job controls; wait for their results".into())
    }

    fn reject(&mut self, message: String) -> Option<String> {
        if self.open {
            self.message = Some(JobsMessage::Rejected(message));
            None
        } else {
            Some(message)
        }
    }

    fn send(&mut self, action: Request, purpose: Purpose) -> bool {
        if self.pending.len() >= 32 {
            return false;
        }
        let request_id = crate::outbox::RequestIdAllocator::allocate(self.prefix);
        self.pending.insert(request_id.clone(), purpose);
        self.commands
            .push_back(SessionCommand::Control(ControlCommand::Command(
                CommandControl { request_id, action },
            )));
        true
    }

    pub fn refresh(&mut self) {
        self.refresh_at(Instant::now());
    }

    fn refresh_at(&mut self, now: Instant) {
        if !self.open || self.inspecting_finished_foreground() {
            return;
        }
        if self
            .pending
            .values()
            .any(|p| matches!(p, Purpose::Inspect(..)))
        {
            self.refresh_again = true;
            return;
        }
        self.last_refresh = Some(now);
        self.refresh_again = !self.send(
            Request::Inspect {
                id: self.inspect.clone(),
            },
            Purpose::Inspect(self.inspect.clone(), self.generation),
        );
    }

    pub fn tick(&mut self, now: Instant) {
        if self
            .last_refresh
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            self.refresh_at(now);
        }
    }

    fn update(&mut self, job: &CommandJobSnapshot, observe: &mut impl FnMut(&CommandJobSnapshot)) {
        if let Some(old) = self.jobs.iter_mut().find(|j| j.id == job.id) {
            if old.status.is_terminal() && !job.status.is_terminal() {
                return;
            }
            old.clone_from(job);
        } else {
            self.jobs.push(job.clone());
        }
        observe(job);
        self.jobs.sort_by_key(|j| j.started_at);
        self.prune();
    }

    fn inspecting_finished_foreground(&self) -> bool {
        self.jobs.iter().any(|job| {
            self.inspect.as_ref() == Some(&job.id)
                && job.mode == CommandJobMode::Foreground
                && job.status.is_terminal()
        })
    }

    fn prune(&mut self) {
        self.jobs.retain(|j| {
            j.mode == CommandJobMode::Background
                || !j.status.is_terminal()
                || self.inspect.as_ref() == Some(&j.id)
        });
        let mut finished: Vec<_> = self
            .jobs
            .iter()
            .filter(|j| j.mode == CommandJobMode::Background && j.status.is_terminal())
            .map(|j| (j.finished_at, j.id.clone()))
            .collect();
        if finished.len() > RETAINED_FINISHED_COMMAND_JOBS {
            finished.sort_unstable();
            let expired = &finished[..finished.len() - RETAINED_FINISHED_COMMAND_JOBS];
            self.jobs
                .retain(|job| !expired.iter().any(|(_, id)| id == &job.id));
        }
    }

    /// Apply job events and correlated replies. Closed-surface control failures
    /// go back to the caller for display in the conversation.
    #[cfg(test)]
    fn event(&mut self, event: &SessionEvent) -> Option<String> {
        self.reduce(event, |_| {})
    }

    pub(crate) fn reduce(
        &mut self,
        event: &SessionEvent,
        mut observe: impl FnMut(&CommandJobSnapshot),
    ) -> Option<String> {
        match event {
            SessionEvent::Job(JobEvent::Updated { job }) => {
                self.generation = self.generation.wrapping_add(1);
                self.update(job, &mut observe);
                self.refresh();
            }
            SessionEvent::Job(JobEvent::ControlResult { request_id, result }) => {
                let purpose = self.pending.remove(request_id)?;
                let mut notice = None;
                match (&purpose, result.as_ref()) {
                    (Purpose::Inspect(id, generation), Reply::Inspected(views))
                        if *generation == self.generation =>
                    {
                        if id.is_none() {
                            self.jobs
                                .retain(|j| views.iter().any(|v| v.snapshot.id == j.id));
                        }
                        for view in views {
                            self.update(&view.snapshot, &mut observe);
                        }
                        if self.open && id == &self.inspect {
                            self.detail = id.as_ref().and_then(|id| {
                                views
                                    .iter()
                                    .find(|v| &v.snapshot.id == id)
                                    .map(JobOutput::from)
                            });
                        }
                    }
                    (Purpose::Inspect(..), Reply::Inspected(_)) => {
                        self.refresh_again = true;
                    }
                    (Purpose::Control(generation), Reply::Accepted(job)) => {
                        if *generation == self.generation {
                            self.generation = self.generation.wrapping_add(1);
                            self.update(job, &mut observe);
                        }
                        if self.open {
                            self.message = Some(JobsMessage::Accepted);
                        }
                    }
                    (Purpose::Control(_), Reply::Rejected { message }) => {
                        notice = self.reject(message.clone());
                    }
                    (Purpose::Inspect(id, _), Reply::Rejected { message })
                        if self.open
                            && id == &self.inspect
                            && !self.inspecting_finished_foreground() =>
                    {
                        self.message = Some(JobsMessage::Rejected(message.clone()));
                        if let Some(id) = id {
                            self.jobs.retain(|job| &job.id != id);
                            self.back();
                        }
                    }
                    _ => {}
                }
                if matches!(purpose, Purpose::Control(_)) || self.refresh_again {
                    self.refresh();
                }
                return notice;
            }
            _ => {}
        }
        None
    }
}

/// Frontend-only compact lines. The notice's text and default ACP summary stay exact.
pub fn display_job_line(snapshot: &CommandJobSnapshot) -> String {
    command_job_line_for(snapshot, &crate::shell::display(&snapshot.command).text)
}
pub fn display_notice_lines(notice: &CommandJobNotice) -> Vec<String> {
    notice.summary_lines_with(display_job_line)
}
/// Sanitize a terminal tail for plain frontend display, not as a terminal emulator.
/// Model output, copies, and persisted transcripts retain their original bytes.
pub fn terminal_tail_text(raw: &str) -> String {
    let mut plain = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            plain.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\u{7}' {
                        break;
                    }
                    if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {} // Two-character escapes and an unterminated final ESC.
        }
    }
    let mut text = String::with_capacity(plain.len());
    for (index, line) in plain.split('\n').enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = line.rsplit('\r').next().unwrap_or_default();
        text.extend(line.chars().filter(|ch| !ch.is_control() || *ch == '\t'));
    }
    text
}

pub fn byte_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn tail_byte_label(bytes: usize) -> String {
    if bytes > COMMAND_JOB_FRONTEND_TAIL_BYTES {
        format!(
            "last {} KiB of {}",
            COMMAND_JOB_FRONTEND_TAIL_BYTES / 1024,
            byte_label(bytes)
        )
    } else {
        byte_label(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use zevria_foundation::TerminationOrigin;

    fn running(mode: CommandJobMode) -> CommandJobSnapshot {
        CommandJobSnapshot {
            id: CommandJobId::new("job-test"),
            call_id: None,
            command: "rtk proxy cargo\n test".into(),
            mode,
            status: CommandJobStatus::Running,
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }

    fn exited(job: &mut CommandJobSnapshot, code: i32) {
        job.status = CommandJobStatus::Exited {
            code: Some(code),
            success: code == 0,
            signal: None,
        };
        job.finished_at = Some(job.started_at + Duration::from_millis(1234));
    }

    fn request(inspector: &mut JobsInspector) -> CommandControl {
        let SessionCommand::Control(ControlCommand::Command(request)) = inspector
            .commands
            .pop_front()
            .expect("expected job request")
        else {
            panic!("expected command control")
        };
        request
    }

    fn reply(
        inspector: &mut JobsInspector,
        request: CommandControl,
        result: Reply,
    ) -> Option<String> {
        inspector.event(&SessionEvent::Job(JobEvent::ControlResult {
            request_id: request.request_id,
            result: Box::new(result),
        }))
    }

    fn view(snapshot: CommandJobSnapshot, stdout: &str) -> CommandJobView {
        CommandJobView {
            snapshot,
            stdout_tail: stdout.into(),
            stderr_tail: String::new(),
        }
    }

    #[test]
    fn inspector_retains_eight_most_recent_finishes() {
        let mut inspector = JobsInspector::new("tui.jobs.");
        for n in 0..10 {
            let mut job = running(CommandJobMode::Background);
            job.id = CommandJobId::new(format!("job-{n}"));
            job.started_at += Duration::from_secs(10 - n);
            exited(&mut job, 0);
            inspector.event(&SessionEvent::Job(JobEvent::Updated { job: Box::new(job) }));
        }
        assert_eq!(inspector.jobs().len(), 8);
        assert_eq!(inspector.jobs()[0].id, CommandJobId::new("job-7"));
    }

    #[test]
    fn stale_inspection_cannot_resurrect_a_finished_foreground_job_or_reopen_surface() {
        let mut inspector = JobsInspector::new("tui.jobs.");
        inspector.show();
        let first = request(&mut inspector);
        let stale = running(CommandJobMode::Foreground);
        inspector.event(&SessionEvent::Job(JobEvent::Updated {
            job: Box::new(stale.clone()),
        }));
        let mut job = stale.clone();
        exited(&mut job, 0);
        inspector.event(&SessionEvent::Job(JobEvent::Updated { job: Box::new(job) }));
        reply(
            &mut inspector,
            first,
            Reply::Inspected(vec![view(stale, "stale")]),
        );
        assert!(inspector.jobs().is_empty());
        assert_eq!(inspector.commands.len(), 1);
        inspector.close();
        let next = request(&mut inspector);
        reply(&mut inspector, next, Reply::Inspected(Vec::new()));
        assert!(!inspector.is_open());
        assert!(inspector.detail().is_none());
    }

    #[test]
    fn controls_are_correlated_while_closed_and_do_not_open_or_refresh_the_surface() {
        let mut inspector = JobsInspector::new("desktop.jobs.");
        let mut job = running(CommandJobMode::Foreground);
        assert!(JobsInspector::can_background(&job));
        inspector.move_to_background(job.id.clone());
        let background = request(&mut inspector);
        assert!(background.request_id.starts_with("desktop.jobs."));
        let background_id = background.request_id.clone();
        assert!(matches!(&background.action, Request::MoveToBackground { id } if id == &job.id));
        job.mode = CommandJobMode::Background;
        assert!(reply(&mut inspector, background, Reply::Accepted(job.clone())).is_none());
        assert!(!JobsInspector::can_background(&inspector.jobs()[0]));
        assert!(JobsInspector::can_terminate(&inspector.jobs()[0]));

        inspector.terminate(job.id.clone());
        let stop = request(&mut inspector);
        assert_ne!(stop.request_id, background_id);
        assert!(matches!(&stop.action, Request::Terminate { id } if id == &job.id));
        let rejected = Reply::Rejected {
            message: "job unavailable".into(),
        };
        assert!(
            inspector
                .event(&SessionEvent::Job(JobEvent::ControlResult {
                    request_id: "tui.jobs.2".into(),
                    result: Box::new(rejected.clone()),
                }))
                .is_none()
        );
        assert_eq!(
            reply(&mut inspector, stop, rejected).as_deref(),
            Some("job unavailable")
        );
        assert!(!inspector.is_open());
        assert!(inspector.message().is_none());
        assert!(inspector.commands.is_empty());
    }

    #[test]
    fn changing_inspection_waits_for_one_request_and_never_shows_another_jobs_tail() {
        let mut inspector = JobsInspector::new("tui.jobs.");
        let job = running(CommandJobMode::Background);
        inspector.show();
        inspector.tick(Instant::now() + Duration::from_secs(2));
        assert_eq!(inspector.commands.len(), 1);
        let list = request(&mut inspector);
        inspector.inspect(job.id.clone());
        reply(
            &mut inspector,
            list,
            Reply::Inspected(vec![view(job.clone(), "not the tail")]),
        );
        assert!(inspector.detail().is_none());
        let detail = request(&mut inspector);
        assert!(matches!(&detail.action, Request::Inspect { id: Some(id) } if id == &job.id));
        let other = CommandJobId::new("job-other");
        inspector.inspect(other.clone());
        reply(
            &mut inspector,
            detail,
            Reply::Inspected(vec![view(job, "old tail")]),
        );
        assert!(inspector.detail().is_none());
        let detail = request(&mut inspector);
        assert!(matches!(detail.action, Request::Inspect { id: Some(id) } if id == other));
        inspector.close();
        assert!(inspector.commands.is_empty());
    }

    #[test]
    fn job_rows_share_shell_spans_and_status_tones() {
        let mut job = running(CommandJobMode::Background);
        job.command = "rtk proxy cargo --release\n echo 'hello  world'".into();
        exited(&mut job, 2);
        let row = JobRow::from(&job);
        assert_eq!(row.command.text, "cargo --release echo 'hello world'");
        assert!(row.command.spans.iter().any(|(range, token)| {
            &row.command.text[range.clone()] == "echo" && *token == shell::ShellToken::Program
        }));
        assert_eq!(
            (
                row.icon,
                row.mode,
                row.duration.as_str(),
                row.status.as_str()
            ),
            (StatusIcon::Failed, "bg", "1.2s", "exited 2")
        );
        assert_eq!(
            summary_title(&[running(CommandJobMode::Foreground), job]),
            "Jobs · 1 running · 1 finished"
        );
        for (status, icon) in [
            (CommandJobStatus::Running, StatusIcon::Running),
            (
                CommandJobStatus::Exited {
                    code: Some(0),
                    success: true,
                    signal: None,
                },
                StatusIcon::Done,
            ),
            (
                CommandJobStatus::Terminated {
                    origin: TerminationOrigin::User,
                },
                StatusIcon::Interrupted,
            ),
            (
                CommandJobStatus::TimedOut { seconds: 60 },
                StatusIcon::Failed,
            ),
            (
                CommandJobStatus::Failed {
                    error: "launch failed".into(),
                },
                StatusIcon::Failed,
            ),
        ] {
            assert_eq!(job_status_icon(&status), icon);
        }
    }

    #[test]
    fn terminal_tails_strip_escapes_and_keep_final_progress_state() {
        for (raw, expected) in [
            ("\u{1b}[31mred\u{1b}[0m", "red"),
            (
                "\u{1b}]8;;https://example.test\u{1b}\\label\u{1b}]8;;\u{1b}\\",
                "label",
            ),
            ("\u{1b}]title\u{7}body", "body"),
            ("first\r\nsecond\r\n", "first\nsecond\n"),
            ("10%\r20%\r30%", "30%"),
            ("10%\r20%\r\nnext", "20%\nnext"),
            ("kept\u{1b}[", "kept"),
            ("kept\u{1b}[31", "kept"),
            ("kept\u{1b}]unterminated", "kept"),
            ("kept\u{1b}", "kept"),
            ("a\u{1b}Mb", "ab"),
            ("a\tb\n", "a\tb\n"),
            ("a\0\u{1}\u{7}\u{8}\u{b}\u{c}\u{7f}b", "ab"),
            ("你好\u{1b}[32m🐚\u{1b}[0m", "你好🐚"),
        ] {
            assert_eq!(terminal_tail_text(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn byte_labels_use_binary_units() {
        for (bytes, expected) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536 * 1024, "1.5 MiB"),
        ] {
            assert_eq!(byte_label(bytes), expected);
        }
    }

    #[test]
    fn background_classification_does_not_confuse_live_foreground_snapshots() {
        use crate::blocks::{NativeToolCallStatus, NativeToolState};
        use zevria_foundation::{
            CommandJobDetail, CommandJobDisposition, ToolCallOutcome, ToolResultDetail,
            ToolResultMetadata,
        };
        let mut state = NativeToolState::new(
            NativeToolCallStatus::Finished,
            &serde_json::json!({"command":"test"}),
        );
        let job = CommandJobSnapshot {
            id: CommandJobId::new("job-classify"),
            call_id: Some("call".into()),
            command: "test".into(),
            mode: CommandJobMode::Foreground,
            status: CommandJobStatus::Running,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        assert!(!state.is_background_command());
        state.command_job = Some(job.clone());
        assert!(!state.is_background_command());
        state.command_job.as_mut().unwrap().mode = CommandJobMode::Background;
        assert!(state.is_background_command());
        state.command_job = None;
        state.metadata = Some(ToolResultMetadata {
            id: "call".into(),
            call_id: None,
            tool_name: "command".into(),
            outcome: ToolCallOutcome::Success,
            diagnostic: None,
            detail: Some(ToolResultDetail::CommandJob(CommandJobDetail {
                id: job.id,
                disposition: CommandJobDisposition::MovedToBackground,
            })),
        });
        assert!(state.is_background_command());
        state.metadata = None;
        state.arguments = Some(serde_json::json!({"background":true}));
        assert!(state.is_background_command());
        state.arguments = Some(serde_json::json!({"background":false}));
        assert!(!state.is_background_command());
    }

    #[test]
    fn notice_display_elides_routing_without_changing_default_summary_or_model_text() {
        let job = CommandJobSnapshot {
            id: CommandJobId::new("job-display"),
            call_id: None,
            command: "rtk proxy cargo test".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Running,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        let notice = CommandJobNotice::new(
            zevria_foundation::CommandJobNoticeKind::Update,
            vec![job.clone()],
            vec![job],
            "model sees rtk proxy cargo test".into(),
        );
        let lines = display_notice_lines(&notice);
        assert_eq!(
            lines,
            [
                "job-display · running · 0.0s · cargo test",
                "Still running:",
                "job-display · running · 0.0s · cargo test"
            ]
        );
        assert!(notice.summary_lines()[0].ends_with("rtk proxy cargo test"));
        assert_eq!(notice.text(), "model sees rtk proxy cargo test");
    }

    #[test]
    fn updates_are_runtime_only_and_retention_is_bounded() {
        let mut model = crate::SessionModel::default();
        for n in 0..10 {
            let job = CommandJobSnapshot {
                id: CommandJobId::new(format!("job-{n}")),
                call_id: Some(format!("call-{n}")),
                command: "test".into(),
                mode: CommandJobMode::Background,
                status: CommandJobStatus::Running,
                started_at: std::time::SystemTime::now(),
                finished_at: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
            };
            model.reduce(zevria_session_api::SessionEvent::Job(JobEvent::Updated {
                job: Box::new(job.clone()),
            }));
            assert_eq!(model.running_job_count(), 1);
            let mut done = job;
            done.status = CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            };
            done.finished_at = Some(std::time::SystemTime::now());
            model.reduce(zevria_session_api::SessionEvent::Job(JobEvent::Updated {
                job: Box::new(done),
            }));
        }
        assert_eq!(model.running_job_count(), 0);
        assert_eq!(model.jobs().iter().count(), 8);
        assert!(model.job_for_call("call-0").is_none());
    }
}
