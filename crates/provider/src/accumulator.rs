use rig_core::message::Message;
use zevria_responses::accumulator::{AssistantMessageAccumulator, NativeOutputLedger};
#[derive(Default)]
pub(crate) struct AttemptState {
    pub(crate) result: AssistantMessageAccumulator,
    pub(crate) native_output: NativeOutputLedger,
    pub(crate) search: Option<crate::search::SearchStream>,
    pub(crate) stale_replay: bool,
    pub(crate) response_ids: std::collections::HashSet<String>,
    #[cfg(test)]
    pub(crate) history: Vec<Message>,
    #[cfg(test)]
    pub(crate) history_replays: Vec<Option<zevria_model::ReplayMessage>>,
}

impl AttemptState {
    pub(crate) fn streaming_message(&self) -> anyhow::Result<Message> {
        match &self.search {
            Some(search) => self.result.streaming_message_with_search(search.state()),
            None => self.result.assistant_message(),
        }
    }

    pub(crate) async fn observe_search(
        &mut self,
        payload: &str,
        progress: &zevria_session_api::ProgressReporter,
    ) {
        if let Some(search) = &mut self.search
            && let Ok(event) = serde_json::from_str(payload)
        {
            search.ingest(&event);
            self.publish_stream(progress);
        }
    }

    pub(crate) fn publish_stream(&self, progress: &zevria_session_api::ProgressReporter) {
        let snapshot = zevria_content::AssistantStreamSnapshot {
            message: self.streaming_message().ok(),
            attempt: self.search.as_ref().map(|search| search.attempt().clone()),
        };
        progress.stream_snapshot(snapshot);
    }

    pub(crate) async fn finish_search(
        &mut self,
        success: bool,
        progress: &zevria_session_api::ProgressReporter,
    ) -> anyhow::Result<()> {
        if let Some(search) = &mut self.search {
            search
                .finish(
                    if success {
                        zevria_content::WebSearchAttemptOutcome::Completed
                    } else {
                        zevria_content::WebSearchAttemptOutcome::Failed
                    },
                    progress,
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) fn reset_result(&mut self) {
        self.result.clear();
        self.native_output.clear();
    }

    pub(crate) fn record_native_output_item_done(&mut self, payload: &str) {
        self.native_output.record_output_item_done(payload);
    }

    pub(crate) fn record_native_terminal_output(&mut self, payload: &str) {
        self.native_output.record_terminal_output(payload);
    }

    pub(crate) fn take_native_output(&mut self) -> Option<Vec<serde_json::Value>> {
        self.native_output.take_complete()
    }
}
