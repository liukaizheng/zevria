//! Local request measurements. These values never enter a provider request.
use std::{collections::BTreeSet, time::Instant};

pub(crate) struct RequestTelemetry {
    started: Instant,
    first_event_ms: Option<u64>,
    function_indices: BTreeSet<u64>,
    terminal_function_calls: Option<usize>,
}

impl RequestTelemetry {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            first_event_ms: None,
            function_indices: BTreeSet::new(),
            terminal_function_calls: None,
        }
    }

    pub(crate) fn request_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub(crate) fn first_event_ms(&self) -> Option<u64> {
        self.first_event_ms
    }

    pub(crate) fn observe_event(&mut self) {
        if self.first_event_ms.is_none() {
            self.first_event_ms = Some(self.request_ms());
        }
    }

    pub(crate) fn observe_openai(&mut self, event: &serde_json::Value) {
        self.observe_event();
        match event["type"].as_str() {
            Some("response.output_item.done") => {
                if event["item"]["type"] == "function_call"
                    && let Some(index) = event["output_index"].as_u64()
                {
                    self.function_indices.insert(index);
                }
            }
            Some(
                "response.completed" | "response.done" | "response.failed" | "response.incomplete",
            ) => {
                if let Some(output) = event["response"]["output"].as_array() {
                    self.terminal_function_calls = Some(
                        output
                            .iter()
                            .filter(|item| item["type"] == "function_call")
                            .count(),
                    );
                }
            }
            _ => {}
        }
    }

    pub(crate) fn function_calls(&self) -> usize {
        self.terminal_function_calls
            .unwrap_or(self.function_indices.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn indexed_calls_are_deduplicated_and_terminal_output_wins() {
        let mut telemetry = RequestTelemetry::new();
        assert_eq!(telemetry.first_event_ms(), None);
        let call = json!({"type":"response.output_item.done", "output_index":3,
            "item":{"type":"function_call"}});
        telemetry.observe_openai(&call);
        let first = telemetry.first_event_ms();
        telemetry.observe_openai(&call);
        assert_eq!(telemetry.function_calls(), 1);
        assert_eq!(telemetry.first_event_ms(), first);
        telemetry.observe_openai(&json!({"type":"response.completed", "response":{"output":[
            {"type":"function_call"}, {"type":"message"}, {"type":"function_call"}
        ]}}));
        assert_eq!(telemetry.function_calls(), 2);
        telemetry.observe_openai(&json!({"type":"response.done", "response":{"output":[]}}));
        assert_eq!(telemetry.function_calls(), 0);
    }
}
