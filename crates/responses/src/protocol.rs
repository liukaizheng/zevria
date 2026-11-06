//! OpenAI request and server-event protocol conversion.

use crate::wire::{ResponseChunk, ResponsesWebSocketEvent};
use rig_core::{
    error::ProviderError as CompletionError,
    message::{
        AssistantContent, DocumentData, DocumentSourceKind, Image, Message, MimeType,
        ToolResultContent, UserContent,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Convert one legacy/provider-neutral message with Rig's normal Responses
/// conversion. Native assistant replay bypasses this helper entirely.
pub fn message_to_openai_input(message: Message) -> anyhow::Result<Vec<serde_json::Value>> {
    anyhow::ensure!(
        !matches!(message, Message::System { .. }),
        "raw system messages must be typed directives"
    );
    let user_item = |part| json!({"type":"message", "role":"user", "content":[part]});
    let mut items = Vec::new();
    match message {
        Message::System { .. } => unreachable!("system messages were rejected above"),
        Message::User { content } => {
            for part in content {
                items.push(match part {
                UserContent::Text(text) => user_item(json!({"type":"input_text", "text":text.text})),
                UserContent::Image(image) => user_item(image_input(image)?),
                UserContent::Document(document) => user_item(match document.data {
                    DocumentData::Text(text) => json!({"type":"input_text", "text":text}),
                    DocumentData::File(DocumentSourceKind::FileId(id)) => json!({"type":"input_file", "file_id":id}),
                    DocumentData::File(DocumentSourceKind::Url(url)) => json!({"type":"input_file", "file_url":url}),
                    DocumentData::File(DocumentSourceKind::Base64(data)) => json!({"type":"input_file", "file_data":format!("data:application/pdf;base64,{data}"), "filename":"document.pdf"}),
                    _ => anyhow::bail!("unsupported Responses document source"),
                }),
                UserContent::ToolResult(result) => {
                    let mut parts = result.content.into_iter().map(|part| match part {
                        ToolResultContent::Text(text) => Ok(json!({"type":"input_text", "text":text.text})),
                        ToolResultContent::Json { value } => Ok(json!({"type":"input_text", "text":value.to_string()})),
                        ToolResultContent::Image(image) => image_input(image),
                    }).collect::<anyhow::Result<Vec<_>>>()?;
                    let output = match parts.as_mut_slice() {
                        [part] if part["type"] == "input_text" => part["text"].take(),
                        _ => Value::Array(parts),
                    };
                    json!({"type":"function_call_output", "call_id":result.call.to_string(), "output":output, "status":"completed"})
                }
                _ => anyhow::bail!("unsupported Responses user content"),
            });
            }
        }
        Message::Assistant(assistant) => {
            for block in assistant.content {
                match block {
                    AssistantContent::Text(text) if !text.text.is_empty() => items
                        .push(json!({"type":"message", "role":"assistant", "content":text.text})),
                    AssistantContent::Text(_) => {}
                    AssistantContent::ToolCall(call) => {
                        let mut item = json!({"type":"function_call", "call_id":call.id.to_string(), "name":call.function.name, "arguments":call.function.raw_arguments(), "status":"completed"});
                        if let Some(id) = call
                            .native
                            .as_ref()
                            .and_then(|native| native.item.get("id"))
                            .and_then(Value::as_str)
                            .filter(|id| id.starts_with("fc_"))
                        {
                            item["id"] = id.into();
                        }
                        items.push(item);
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        if let Some(native) = reasoning.native.filter(|native| {
                            native.item.get("id").and_then(Value::as_str).is_some()
                        }) {
                            items.push(native.item);
                        }
                    }
                    _ => anyhow::bail!("unsupported Responses assistant content"),
                }
            }
        }
    }
    Ok(items)
}

fn image_input(image: Image) -> anyhow::Result<Value> {
    let mut part = json!({"type":"input_image", "detail":image.detail.unwrap_or_default()});
    match image.data {
        DocumentSourceKind::Base64(data) => {
            let media_type = image
                .media_type
                .ok_or_else(|| anyhow::anyhow!("base64 images require a media type"))?;
            part["image_url"] = format!("data:{};base64,{data}", media_type.to_mime_type()).into();
        }
        DocumentSourceKind::Url(url) => part["image_url"] = url.into(),
        DocumentSourceKind::FileId(id) => part["file_id"] = id.into(),
        _ => anyhow::bail!("unsupported Responses image source"),
    }
    Ok(part)
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
        kind if matches!(
            kind,
            "response.created"
                | "response.in_progress"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
        ) =>
        {
            serde_json::from_str::<ResponseChunk>(payload)
                .map(|chunk| Some(ResponsesWebSocketEvent::Response(Box::new(chunk))))
                .map_err(CompletionError::from)
        }
        kind if is_known_streaming_event(kind) => serde_json::from_str(payload)
            .map(|item| Some(ResponsesWebSocketEvent::Item(item)))
            .map_err(CompletionError::from),
        kind => {
            tracing::trace!(target: "zevria_provider::protocol", kind, "ignoring an unknown OpenAI response event");
            Ok(None)
        }
    }
}
