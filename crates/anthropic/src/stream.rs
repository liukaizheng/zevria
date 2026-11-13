//! Lossless ordered SSE accumulation. Unknown blocks survive; unknown deltas
//! fail closed, because silently ignoring one could corrupt a signature/input.
use crate::{
    error::{ApiError, ErrorKind},
    usage::Usage,
};
use rig_core::message::Message;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use zevria_foundation::ModelProfileRef;
use zevria_model::ProviderReplay;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    MessageStart {
        message: Value,
    },
    ContentBlockStart {
        index: usize,
        content_block: Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: Value,
        #[serde(default)]
        usage: Value,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta { text: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
    InputJsonDelta { partial_json: String },
    CitationsDelta { citation: Value },
}
struct Block {
    raw: Value,
    partial_json: String,
    stopped: bool,
}
#[derive(Default)]
pub struct MessageAccumulator {
    started: bool,
    stopped: bool,
    blocks: BTreeMap<usize, Block>,
    pub usage: Usage,
    pub served_model: String,
    stop_reason: Option<String>,
    stop_details: Value,
    input_transformations: Value,
}
pub struct FinishedMessage {
    pub blocks: Vec<Value>,
    pub stop_reason: String,
    pub stop_details: Value,
    pub usage: Usage,
    pub served_model: String,
    pub input_transformations: Value,
}
fn protocol(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Retryable("protocol"), message)
}
fn terminal(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Terminal, message)
}
impl MessageAccumulator {
    /// Returns meaningful progress, never advancing the idle watchdog for ping.
    pub fn push(&mut self, payload: &str) -> Result<bool, ApiError> {
        let event: Event = serde_json::from_str(payload)
            .map_err(|error| terminal(format!("invalid Anthropic SSE event: {error}")))?;
        if matches!(event, Event::Ping) {
            return Ok(false);
        }
        if let Event::Error { error } = event {
            return Err(crate::error::classify(
                0,
                &serde_json::json!({"error":error}).to_string(),
                None,
                None,
                None,
            ));
        }
        if self.stopped {
            return Err(protocol("Anthropic event after message_stop"));
        }
        match event {
            Event::MessageStart { message } => {
                if self.started {
                    return Err(protocol("duplicate message_start"));
                }
                self.started = true;
                self.served_model = message["model"].as_str().unwrap_or_default().to_owned();
                self.usage.update(&message["usage"]);
                self.input_transformations = message["input_transformations"].clone();
                if message["content"]
                    .as_array()
                    .is_some_and(|blocks| !blocks.is_empty())
                {
                    return Err(protocol("message_start contained unexpected content"));
                }
            }
            _ if !self.started => return Err(protocol("Anthropic event before message_start")),
            Event::ContentBlockStart {
                index,
                content_block,
            } => {
                if index != self.blocks.len()
                    || self.blocks.values().any(|block| !block.stopped)
                    || !content_block.is_object()
                    || content_block["type"].as_str().is_none()
                {
                    return Err(protocol("invalid content_block_start ordering or shape"));
                }
                self.blocks.insert(
                    index,
                    Block {
                        raw: content_block,
                        partial_json: String::new(),
                        stopped: false,
                    },
                );
            }
            Event::ContentBlockDelta { index, delta } => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.stopped)
                    .ok_or_else(|| protocol("delta without an open content block"))?;
                match delta {
                    Delta::TextDelta { text } => append(&mut block.raw, "text", "text", &text)?,
                    Delta::ThinkingDelta { thinking } => {
                        append(&mut block.raw, "thinking", "thinking", &thinking)?
                    }
                    Delta::SignatureDelta { signature } => {
                        append(&mut block.raw, "thinking", "signature", &signature)?
                    }
                    Delta::InputJsonDelta { partial_json } => {
                        if block.raw["type"] != "tool_use" {
                            return Err(protocol("input_json_delta on a non-tool block"));
                        }
                        block.partial_json.push_str(&partial_json);
                    }
                    Delta::CitationsDelta { citation } => {
                        if block.raw["type"] != "text" {
                            return Err(protocol("citations_delta on a non-text block"));
                        }
                        if block.raw.get("citations").is_none() {
                            block.raw["citations"] = Value::Array(Vec::new());
                        }
                        block.raw["citations"]
                            .as_array_mut()
                            .ok_or_else(|| protocol("invalid citations array"))?
                            .push(citation);
                    }
                }
            }
            Event::ContentBlockStop { index } => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.stopped)
                    .ok_or_else(|| protocol("stop without an open content block"))?;
                if block.raw["type"] == "tool_use" && !block.partial_json.is_empty() {
                    let input: serde_json::Map<String, Value> = serde_json::from_str(&block.partial_json).map_err(|error| ApiError::new(ErrorKind::Retryable("invalid_tool_json"), format!("invalid tool JSON at content_block_stop: {error}; try compatibility.eager_tool_input_streaming: false if this persists")))?;
                    block.raw["input"] = Value::Object(input);
                }
                serde_json::from_value::<zevria_model::anthropic::ContentBlock>(block.raw.clone())
                    .map_err(|error| {
                        protocol(format!("invalid completed Anthropic block: {error}"))
                    })?;
                block.stopped = true;
            }
            Event::MessageDelta { delta, usage } => {
                if self.blocks.values().any(|b| !b.stopped) {
                    return Err(protocol("message_delta before content_block_stop"));
                }
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                if delta.get("stop_details").is_some() {
                    self.stop_details = delta["stop_details"].clone();
                }
                if delta.get("input_transformations").is_some() {
                    self.input_transformations = delta["input_transformations"].clone();
                }
                self.usage.update(&usage);
            }
            Event::MessageStop => {
                if self.blocks.values().any(|b| !b.stopped) {
                    return Err(protocol("message_stop before content_block_stop"));
                }
                self.stopped = true;
            }
            Event::Ping | Event::Error { .. } => unreachable!(),
        }
        Ok(true)
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }
    /// Count replayable client calls without constructing a display projection.
    pub fn tool_uses(&self) -> usize {
        self.blocks
            .values()
            .rev()
            .take_while(|block| block.raw["type"] != "fallback")
            .filter(|block| block.raw["type"] == "tool_use")
            .count()
    }
    pub fn input_transformations(&self) -> &Value {
        &self.input_transformations
    }
    pub fn preview(&self) -> Option<Message> {
        let blocks = self
            .blocks
            .values()
            .filter(|block| {
                block.stopped || matches!(block.raw["type"].as_str(), Some("text" | "thinking"))
            })
            .map(|block| block.raw.clone())
            .collect::<Vec<_>>();
        let blocks = echo_blocks(blocks);
        if blocks.is_empty() {
            return None;
        }
        ProviderReplay::anthropic_messages(ModelProfileRef::new("preview", "preview"), blocks, None)
            .to_message()
            .ok()
    }
    pub fn finish(self) -> Result<FinishedMessage, ApiError> {
        if !self.stopped {
            return Err(protocol("Anthropic stream ended before message_stop"));
        }
        let stop_reason = self
            .stop_reason
            .ok_or_else(|| protocol("Anthropic response omitted stop_reason"))?;
        match stop_reason.as_str() {
            "end_turn" | "stop_sequence" | "tool_use" => {}
            "max_tokens" | "model_context_window_exceeded" => {
                return Err(terminal(format!(
                    "Anthropic stopped with {stop_reason}; review max_output_tokens and context limits; partial output was discarded"
                )));
            }
            "refusal" => {
                return Err(terminal(format!(
                    "Anthropic refusal (category: {}); partial output was discarded",
                    self.stop_details["category"].as_str().unwrap_or("unknown")
                )));
            }
            "pause_turn" => {
                return Err(terminal(
                    "Anthropic pause_turn is unsupported (no server tools in v1)",
                ));
            }
            other => {
                return Err(terminal(format!(
                    "unsupported Anthropic stop_reason {other:?}"
                )));
            }
        }
        let blocks = echo_blocks(self.blocks.into_values().map(|b| b.raw).collect());
        if blocks.is_empty() {
            return Err(terminal(
                "Anthropic response contained no assistant content",
            ));
        }
        ProviderReplay::anthropic_messages(
            ModelProfileRef::new("validation", "validation"),
            blocks.clone(),
            None,
        )
        .to_message()
        .map_err(|e| terminal(e.to_string()))?;
        Ok(FinishedMessage {
            blocks,
            stop_reason,
            stop_details: self.stop_details,
            usage: self.usage,
            served_model: self.served_model,
            input_transformations: self.input_transformations,
        })
    }
}
fn append(block: &mut Value, kind: &str, key: &str, delta: &str) -> Result<(), ApiError> {
    if block["type"] != kind {
        return Err(protocol(format!("{key} delta on a non-{kind} block")));
    }
    let Value::String(text) = &mut block[key] else {
        return Err(protocol(format!("missing {key} string")));
    };
    text.push_str(delta);
    Ok(())
}
/// After the last fallback boundary only text from older attempts may be
/// echoed. In particular a pre-boundary client tool call can never execute.
pub fn echo_blocks(blocks: Vec<Value>) -> Vec<Value> {
    let boundary = blocks.iter().rposition(|block| block["type"] == "fallback");
    blocks
        .into_iter()
        .enumerate()
        .filter_map(|(index, block)| {
            if block["type"] == "fallback"
                || boundary.is_some_and(|boundary| index < boundary && block["type"] != "text")
            {
                None
            } else {
                Some(block)
            }
        })
        .collect()
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_uses_count_matches_replay_after_fallback() {
        let mut accumulator = MessageAccumulator::default();
        for (index, kind) in ["tool_use", "text", "fallback", "tool_use", "tool_use"]
            .into_iter()
            .enumerate()
        {
            accumulator.blocks.insert(
                index,
                Block {
                    raw: json!({"type":kind}),
                    partial_json: String::new(),
                    stopped: true,
                },
            );
        }
        assert_eq!(accumulator.tool_uses(), 2);
        let replay = echo_blocks(accumulator.blocks.into_values().map(|b| b.raw).collect());
        assert_eq!(replay.iter().filter(|b| b["type"] == "tool_use").count(), 2);
    }
}
