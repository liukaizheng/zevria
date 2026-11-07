use rig_core::message::Message;
use zevria_responses::accumulator::{AssistantMessageAccumulator, NativeOutputLedger};
#[derive(Default)]
pub(crate) struct AttemptState {
    pub(crate) result: AssistantMessageAccumulator,
    pub(crate) native_output: NativeOutputLedger,
    pub(crate) attempt: Option<crate::attempt::AttemptStream>,
    pub(crate) stale_replay: bool,
    pub(crate) response_ids: std::collections::HashSet<String>,
    #[cfg(test)]
    pub(crate) history: Vec<Message>,
    #[cfg(test)]
    pub(crate) history_replays: Vec<Option<zevria_model::ReplayMessage>>,
}

impl AttemptState {
    pub(crate) fn streaming_message(&self) -> anyhow::Result<Message> {
        match &self.attempt {
            Some(search) => self.result.streaming_message_with_attempt(search.state()),
            None => self.result.assistant_message(),
        }
    }

    pub(crate) async fn observe_attempt(
        &mut self,
        payload: &str,
        progress: &zevria_session_api::ProgressReporter,
    ) {
        if let Some(search) = &mut self.attempt
            && let Ok(event) = serde_json::from_str(payload)
        {
            search.ingest(&event);
            self.publish_stream(progress);
        }
    }

    pub(crate) fn publish_stream(&self, progress: &zevria_session_api::ProgressReporter) {
        let snapshot = zevria_content::AssistantStreamSnapshot {
            message: self.streaming_message().ok(),
            attempt: self.attempt.as_ref().map(|search| search.attempt().clone()),
        };
        if snapshot
            .attempt
            .as_ref()
            .is_some_and(|attempt| !attempt.has_display())
        {
            if let Some(attempt) = snapshot.attempt {
                progress.collect_attempt(attempt);
            }
        } else {
            progress.stream_snapshot(snapshot);
        }
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
