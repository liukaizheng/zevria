use rig_core::message::Message;
use zevria_responses::accumulator::{AssistantMessageAccumulator, NativeOutputLedger};
#[derive(Default)]
pub(crate) struct AttemptState {
    pub(crate) result: AssistantMessageAccumulator,
    pub(crate) native_output: NativeOutputLedger,
    pub(crate) attempt: Option<crate::attempt::AttemptStream>,
    pub(crate) stale_replay: bool,
    pub(crate) response_ids: std::collections::HashSet<String>,
    pub(crate) cadence: crate::streaming::StreamCadence,
    #[cfg(test)]
    pub(crate) history: Vec<Message>,
    #[cfg(test)]
    pub(crate) history_replays: Vec<Option<zevria_model::ReplayMessage>>,
}

fn streaming_message(
    result: &AssistantMessageAccumulator,
    attempt: Option<&crate::attempt::AttemptStream>,
) -> anyhow::Result<Message> {
    match attempt {
        Some(attempt) => result.streaming_message_with_attempt(attempt.state()),
        None => result.assistant_message(),
    }
}

fn publish_snapshot(
    result: &AssistantMessageAccumulator,
    attempt: Option<&crate::attempt::AttemptStream>,
    progress: &zevria_session_api::ProgressReporter,
) {
    let snapshot = zevria_content::AssistantStreamSnapshot {
        message: streaming_message(result, attempt).ok(),
        attempt: attempt.map(|attempt| attempt.attempt().clone()),
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

impl AttemptState {
    #[cfg(test)]
    pub(crate) fn streaming_message(&self) -> anyhow::Result<Message> {
        streaming_message(&self.result, self.attempt.as_ref())
    }

    pub(crate) fn observe_attempt_value(&mut self, event: &serde_json::Value) {
        if let Some(attempt) = &mut self.attempt {
            attempt.ingest(event);
        }
        self.cadence.mark_dirty();
    }

    #[cfg(test)]
    pub(crate) async fn observe_attempt(
        &mut self,
        payload: &str,
        progress: &zevria_session_api::ProgressReporter,
    ) {
        if let Ok(event) = serde_json::from_str(payload) {
            self.observe_attempt_value(&event);
            self.flush_stream(progress);
        }
    }

    pub(crate) fn publish_stream(&mut self, progress: &zevria_session_api::ProgressReporter) {
        if self.cadence.take_due(false) {
            publish_snapshot(&self.result, self.attempt.as_ref(), progress);
        }
    }

    pub(crate) fn flush_stream(&mut self, progress: &zevria_session_api::ProgressReporter) {
        if self.cadence.take_due(true) {
            publish_snapshot(&self.result, self.attempt.as_ref(), progress);
        }
    }

    pub(crate) async fn next_event<F: std::future::Future>(
        &mut self,
        next: F,
        progress: &zevria_session_api::ProgressReporter,
    ) -> F::Output {
        let result = &self.result;
        let attempt = &self.attempt;
        self.cadence
            .next(next, || {
                publish_snapshot(result, attempt.as_ref(), progress)
            })
            .await
    }

    pub(crate) fn reset_result(&mut self) {
        self.result.clear();
        self.native_output.clear();
    }

    #[cfg(test)]
    pub(crate) fn record_native_output_item_done(&mut self, payload: &str) {
        self.native_output.record_output_item_done(payload);
    }
    #[cfg(test)]
    pub(crate) fn record_native_terminal_output(&mut self, payload: &str) {
        self.native_output.record_terminal_output(payload);
    }
    pub(crate) fn take_native_output(&mut self) -> Option<Vec<serde_json::Value>> {
        self.native_output.take_complete()
    }
}
