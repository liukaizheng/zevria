//! Provider-owned publication and durability around deterministic hosted-search reduction.
#[cfg(test)]
use rig_core::message::Text;
use serde_json::Value;
use zevria_content::{WebSearchAttemptOutcome, WebSearchAttemptRecord};
use zevria_responses::search::SearchState;
use zevria_session_api::ProgressReporter;

pub(crate) struct SearchStream {
    state: SearchState,
}
impl SearchStream {
    pub(crate) fn new(
        profile: zevria_foundation::ModelProfileRef,
        progress: &ProgressReporter,
    ) -> Self {
        let state = SearchState::new(profile);
        progress.collect_web_search(state.attempt().clone());
        Self { state }
    }
    pub(crate) fn state(&self) -> &SearchState {
        &self.state
    }
    pub(crate) fn attempt(&self) -> &WebSearchAttemptRecord {
        self.state.attempt()
    }
    pub(crate) fn ingest(&mut self, event: &Value) {
        self.state.ingest(event);
    }
    pub(crate) fn reconcile(&mut self, items: &[Value]) {
        self.state.reconcile(items);
    }
    #[cfg(test)]
    pub(crate) fn completed_text(&self) -> &std::collections::BTreeMap<(u64, u64), Text> {
        self.state.completed_text()
    }
    pub(crate) async fn finish(
        &mut self,
        outcome: WebSearchAttemptOutcome,
        progress: &ProgressReporter,
    ) -> anyhow::Result<()> {
        self.state.finish(outcome);
        self.publish(progress);
        progress
            .checkpoint_web_search(self.attempt().clone())
            .await?;
        // Publish the finalized attempt before the next attempt can replace a
        // lossy preview. Durability acknowledgement itself never waits for UI.
        if self.attempt().has_display() {
            progress.web_search_updated(self.attempt().clone()).await;
        }
        Ok(())
    }

    pub(crate) fn publish(&self, progress: &ProgressReporter) {
        progress.stream_snapshot(zevria_content::AssistantStreamSnapshot {
            message: None,
            attempt: self.attempt().has_display().then(|| self.attempt().clone()),
        });
    }

    #[cfg(test)]
    pub(crate) async fn observe(&mut self, event: &Value, progress: &ProgressReporter) {
        self.ingest(event);
        self.publish(progress);
    }
}
