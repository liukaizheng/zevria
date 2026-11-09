//! Ephemeral root command jobs, never restored from transcript or turn-gated.
use std::collections::{BTreeMap, HashMap};
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobNotice, CommandJobSnapshot, command_job_line_for,
};

/// Frontend-only compact lines. The notice's text and default ACP summary stay exact.
pub fn display_job_line(snapshot: &CommandJobSnapshot) -> String {
    command_job_line_for(snapshot, &crate::shell::display(&snapshot.command).text)
}
pub fn display_notice_lines(notice: &CommandJobNotice) -> Vec<String> {
    notice.summary_lines_with(display_job_line)
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
            if mode == CommandJobMode::Foreground || background > 8 {
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
