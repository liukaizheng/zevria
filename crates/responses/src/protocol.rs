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
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed"
        | "response.output_text.annotation.added" => Ok(None),
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
