//! OpenAI request and server-event protocol conversion.

use rig_core::{
    completion::{CompletionError, CompletionRequest as RigCompletionRequest},
    message::Message,
    providers::openai::responses_api::{
        CompletionRequest, InputItem, streaming::StreamingCompletionChunk,
    },
};
use rig_reqwest::openai_websocket::ResponsesWebSocketEvent;
use serde::Deserialize;

pub fn into_openai_request_with_instructions(
    model: String,
    mut request: RigCompletionRequest,
    instructions: &str,
) -> anyhow::Result<CompletionRequest> {
    anyhow::ensure!(
        !request
            .chat_history
            .iter()
            .any(|message| matches!(message, Message::System { .. })),
        "raw system messages must be typed directives"
    );
    request.preamble = Some(instructions.into());
    let mut request = CompletionRequest::try_from((model, request))?;
    request.instructions = Some(instructions.into());
    Ok(request)
}

/// Convert one legacy/provider-neutral message with Rig's normal Responses
/// conversion. Native assistant replay bypasses this helper entirely.
pub fn message_to_openai_input(message: Message) -> anyhow::Result<Vec<serde_json::Value>> {
    anyhow::ensure!(
        !matches!(message, Message::System { .. }),
        "raw system messages must be typed directives"
    );
    let items = <Vec<InputItem>>::try_from(message)?;
    items
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)
}

pub fn is_known_streaming_event(kind: &str) -> bool {
    matches!(
        kind,
        "response.created"
            | "response.in_progress"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
    )
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use ProgressObservation::*;
    use serde_json::json;

    #[test]
    fn duplicate_and_regressive_statuses_do_not_advance_even_with_new_sequences() {
        let mut progress = ResponseProgress::new(Some("old".into()));
        assert_eq!(
            progress.observe(&json!({"type":"response.completed","response":{"id":"old"}})),
            OtherResponse
        );
        assert_eq!(
            progress.observe(&json!({"type":"relay.metadata","response":{"id":"metadata"}})),
            Unchanged
        );
        for (kind, sequence, expected) in [
            ("response.in_progress", 1, Advanced),
            ("response.in_progress", 2, Unchanged),
            ("response.created", 3, Unchanged),
        ] {
            assert_eq!(
                progress.observe(
                    &json!({"type":kind,"sequence_number":sequence,"response":{"id":"current"}})
                ),
                expected
            );
        }
        assert_eq!(
            progress.observe(&json!({"type":"response.completed","response":{"id":"unrelated"}})),
            OtherResponse
        );
        for (kind, expected) in [
            ("searching", Advanced),
            ("searching", Unchanged),
            ("in_progress", Unchanged),
            ("completed", Advanced),
            ("searching", Unchanged),
        ] {
            let event =
                json!({"type":format!("response.web_search_call.{kind}"),"item_id":"search"});
            assert!(
                parse_server_event(&event.to_string()).unwrap().is_none(),
                "hosted progress bypasses the ordinary accumulator"
            );
            assert_eq!(progress.observe(&event), expected);
        }
    }

    #[test]
    fn output_and_part_advancement_support_gateways_without_sequences() {
        let mut progress = ResponseProgress::default();
        for kind in [
            "response.reasoning_text.delta",
            "response.reasoning_summary_text.delta",
            "response.function_call_arguments.delta",
            "response.refusal.delta",
            "response.output_text.delta",
        ] {
            assert_eq!(
                progress.observe(&json!({"type":kind,"delta":""})),
                Unchanged
            );
            for _ in 0..2 {
                assert_eq!(
                    progress.observe(&json!({"type":kind,"delta":"same token"})),
                    Advanced
                );
            }
        }
        for index in 0..2 {
            let event =
                json!({"type":"response.content_part.done","item_id":"item","content_index":index});
            assert_eq!(progress.observe(&event), Advanced);
            assert_eq!(progress.observe(&event), Unchanged);
        }
        let event =
            json!({"type":"response.output_item.done","item":{"id":"item"},"output_index":0});
        assert_eq!(progress.observe(&event), Advanced);
        assert_eq!(progress.observe(&event), Unchanged);
        assert_eq!(
            progress.observe(
                &json!({"type":"response.output_item.added","item":{"id":"item"},"output_index":0})
            ),
            Unchanged
        );
        assert_eq!(
            progress.observe(
                &json!({"type":"response.output_text.delta","sequence_number":100,"delta":"new"})
            ),
            Advanced
        );
        assert_eq!(progress.observe(&json!({"type":"response.output_text.delta","sequence_number":100,"delta":"replayed"})), Unchanged);
    }
}

fn is_hosted_progress_event(kind: &str) -> bool {
    matches!(
        kind,
        "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.output_text.annotation.added"
    )
}

/// Stateful meaningful-progress classification beside the protocol decoder.
/// Optional sequence numbers suppress replays, but compatible gateways need
/// not supply them. A status can advance only once per response/item/part.
#[derive(Default)]
pub struct ResponseProgress {
    response_id: Option<String>,
    excluded_responses: std::collections::HashSet<String>,
    sequence: Option<u64>,
    advancements: std::collections::HashMap<String, u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProgressObservation {
    /// Known to belong to a different response; do not accumulate it either.
    OtherResponse,
    Unchanged,
    Advanced,
}

impl ResponseProgress {
    pub fn new(previous_response_id: Option<String>) -> Self {
        Self {
            excluded_responses: previous_response_id.into_iter().collect(),
            ..Self::default()
        }
    }

    pub fn exclude_response(&mut self, id: String) {
        self.excluded_responses.insert(id);
    }

    pub fn observe(&mut self, event: &serde_json::Value) -> ProgressObservation {
        use ProgressObservation::*;
        let kind = event["type"].as_str().unwrap_or_default();
        let id = event["response"]["id"]
            .as_str()
            .or_else(|| event["response_id"].as_str());
        if let Some(id) = id {
            if self.excluded_responses.contains(id)
                || self
                    .response_id
                    .as_deref()
                    .is_some_and(|current| current != id)
            {
                return OtherResponse;
            }
        }
        if !(is_known_streaming_event(kind)
            || is_hosted_progress_event(kind)
            || kind == "response.done")
        {
            return Unchanged;
        }
        if self.response_id.is_none() {
            self.response_id = id.map(ToOwned::to_owned);
        }
        if let Some(sequence) = event["sequence_number"].as_u64() {
            if self.sequence.is_some_and(|last| sequence <= last) {
                return Unchanged;
            }
            self.sequence = Some(sequence);
        }
        if kind.ends_with(".delta") {
            return if event["delta"]
                .as_str()
                .is_some_and(|delta| !delta.is_empty())
            {
                Advanced
            } else {
                Unchanged
            };
        }
        // Identity rather than sequence controls status advancement: gateways
        // sometimes emit the same in_progress status with increasing sequence.
        let identity = event["item_id"]
            .as_str()
            .or_else(|| event["item"]["id"].as_str());
        let (family, rank) = match kind {
            "response.created" => ("response", 1),
            "response.in_progress" => ("response", 2),
            "response.completed" | "response.failed" | "response.incomplete" | "response.done" => {
                ("response", 3)
            }
            "response.web_search_call.in_progress" => ("response.web_search_call", 1),
            "response.web_search_call.searching" => ("response.web_search_call", 2),
            "response.web_search_call.completed" => ("response.web_search_call", 3),
            _ if kind.ends_with(".added") => (kind.trim_end_matches(".added"), 1),
            _ if kind.ends_with(".done") => (kind.trim_end_matches(".done"), 2),
            _ => (kind, 1),
        };
        let index = if identity.is_some() {
            &serde_json::Value::Null
        } else {
            &event["output_index"]
        };
        let key = serde_json::json!([
            family,
            identity,
            index,
            event["content_index"],
            event["summary_index"],
            event["annotation_index"]
        ])
        .to_string();
        let previous = self.advancements.entry(key).or_default();
        if rank > *previous {
            *previous = rank;
            Advanced
        } else {
            Unchanged
        }
    }
}

pub fn parse_server_event(
    payload: &str,
) -> Result<Option<ResponsesWebSocketEvent>, CompletionError> {
    #[derive(Deserialize)]
    struct EventType {
        #[serde(rename = "type")]
        kind: String,
    }

    let event_type = serde_json::from_str::<EventType>(payload)?;
    match event_type.kind.as_str() {
        "error" => serde_json::from_str(payload)
            .map(|e| Some(ResponsesWebSocketEvent::Error(e)))
            .map_err(CompletionError::from),
        // SearchStream owns these events and has already observed their raw
        // payloads. They must not enter Rig's function-tool stream conversion.
        kind if is_hosted_progress_event(kind) => Ok(None),
        "response.done" => serde_json::from_str(payload)
            .map(|d| Some(ResponsesWebSocketEvent::Done(d)))
            .map_err(CompletionError::from),
        kind if is_known_streaming_event(kind) => match serde_json::from_str(payload)? {
            StreamingCompletionChunk::Response(response) => {
                Ok(Some(ResponsesWebSocketEvent::Response(response)))
            }
            StreamingCompletionChunk::Delta(item) => Ok(Some(ResponsesWebSocketEvent::Item(item))),
        },
        kind => {
            tracing::trace!(target: "zevria_provider::protocol", kind, "ignoring an unknown OpenAI response event");
            Ok(None)
        }
    }
}
