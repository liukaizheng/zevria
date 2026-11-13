//! Ephemeral root command jobs, never restored from transcript or turn-gated.
use std::collections::{BTreeMap, HashMap};
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobNotice, CommandJobSnapshot,
    RETAINED_FINISHED_COMMAND_JOBS, command_job_line_for,
};

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

#[derive(Debug, Default)]
pub struct CommandJobsState {
    jobs: BTreeMap<CommandJobId, CommandJobSnapshot>,
    calls: HashMap<String, CommandJobId>,
}
impl CommandJobsState {
    pub fn update(&mut self, job: CommandJobSnapshot) {
        if self
            .jobs
            .get(&job.id)
            .is_some_and(|old| old.status.is_terminal() && !job.status.is_terminal())
        {
            return;
        }
        if let Some(call) = &job.call_id {
            self.calls.insert(call.clone(), job.id.clone());
        }
        self.jobs.insert(job.id.clone(), job);
        let mut finished: Vec<_> = self
            .jobs
            .values()
            .filter(|j| j.status.is_terminal())
            .map(|j| (j.finished_at, j.id.clone(), j.mode))
            .collect();
        finished.sort_by_key(|j| j.0);
        let mut background = finished
            .iter()
            .filter(|j| j.2 == CommandJobMode::Background)
            .count();
        for (_, id, mode) in finished {
            if mode == CommandJobMode::Foreground || background > RETAINED_FINISHED_COMMAND_JOBS {
                if mode == CommandJobMode::Background {
                    background -= 1;
                }
                if let Some(job) = self.jobs.remove(&id) {
                    if let Some(call) = job.call_id {
                        self.calls.remove(&call);
                    }
                }
            }
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = &CommandJobSnapshot> {
        self.jobs.values()
    }
    pub fn running_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|j| !j.status.is_terminal())
            .count()
    }
    pub fn job_for_call(&self, call: &str) -> Option<&CommandJobSnapshot> {
        self.calls.get(call).and_then(|id| self.jobs.get(id))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::CommandJobStatus;
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
            model.reduce(zevria_session_api::SessionEvent::CommandJobUpdated { job: job.clone() });
            assert_eq!(model.running_job_count(), 1);
            let mut done = job;
            done.status = CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            };
            done.finished_at = Some(std::time::SystemTime::now());
            model.reduce(zevria_session_api::SessionEvent::CommandJobUpdated { job: done });
        }
        assert_eq!(model.running_job_count(), 0);
        assert_eq!(model.jobs().iter().count(), 8);
        assert!(model.job_for_call("call-0").is_none());
    }
}
