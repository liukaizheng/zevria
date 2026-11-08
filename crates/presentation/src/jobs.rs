//! Ephemeral root command jobs, never restored from transcript or turn-gated.
use std::collections::{BTreeMap, HashMap};
use zevria_foundation::{CommandJobId, CommandJobMode, CommandJobSnapshot};
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
