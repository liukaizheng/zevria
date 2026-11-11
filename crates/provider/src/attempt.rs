//! Publication and failed-attempt durability around provider-neutral reduction.
use serde_json::Value;
use zevria_content::{ResponseAttempt, ResponseAttemptOutcome};
use zevria_responses::attempt::AttemptReducer;
use zevria_session_api::ProgressReporter;

pub(crate) struct AttemptStream {
    state: AttemptReducer,
    collector: zevria_session_api::ResponseAttemptCollector,
}

impl Drop for AttemptStream {
    fn drop(&mut self) {
        // Cancellation drops the provider future before its ordinary terminal
        // flush. Retain the latest ingested evidence without cloning on every
        // stream delta or publishing an out-of-cadence frontend snapshot.
        self.collector.collect(self.state.attempt().clone());
    }
}
impl AttemptStream {
    pub(crate) fn new(
        profile: zevria_foundation::ModelProfileRef,
        progress: &ProgressReporter,
    ) -> Self {
        let state = AttemptReducer::new(profile);
        progress.collect_attempt(state.attempt().clone());
        Self {
            state,
            collector: progress.attempt_collector(),
        }
    }
    pub(crate) fn state(&self) -> &AttemptReducer {
        &self.state
    }
    pub(crate) fn attempt(&self) -> &ResponseAttempt {
        self.state.attempt()
    }
    pub(crate) fn id(&self) -> &str {
        &self.attempt().id
    }
    pub(crate) fn ingest(&mut self, event: &Value) {
        self.state.ingest(event);
    }
    pub(crate) async fn close_failed(&mut self, progress: &ProgressReporter) -> anyhow::Result<()> {
        self.state
            .attempt_mut()
            .close(ResponseAttemptOutcome::Failed);
        self.publish(progress);
        progress
            .checkpoint_failed_attempt(self.attempt().clone())
            .await?;
        if self.attempt().has_display() {
            progress.attempt_updated(self.attempt().clone()).await;
        }
        Ok(())
    }
    pub(crate) fn publish(&self, progress: &ProgressReporter) {
        // Always collect, even when there is not yet anything readable to show.
        progress.collect_attempt(self.attempt().clone());
        if self.attempt().has_display() {
            progress.stream_snapshot(zevria_content::AssistantStreamSnapshot {
                message: None,
                attempt: Some(self.attempt().clone()),
            });
        }
    }
    #[cfg(test)]
    pub(crate) fn completed_text(
        &self,
    ) -> &std::collections::BTreeMap<(u64, u64), rig_core::message::Text> {
        self.state.completed_text()
    }
    #[cfg(test)]
    pub(crate) async fn observe(&mut self, event: &Value, progress: &ProgressReporter) {
        self.ingest(event);
        self.publish(progress);
    }
}
