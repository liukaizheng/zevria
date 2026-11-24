//! Occurrence-based tool correlation shared with provider projection. Analysis
//! never rewrites an item or splits a native envelope.
use std::collections::HashMap;

use crate::{ModelRequestItem, provider_replay::responses_tool_handles};
use rig_core::message::{AssistantContent, Message, UserContent};

#[derive(Debug)]
struct Occurrence<T> {
    value: T,
    completed: bool,
}

/// Shared correlation rules for replay and compaction. Each known result handle
/// must identify the same latest preceding occurrence, even if it is completed.
#[derive(Debug)]
pub struct ToolCorrelations<T> {
    calls: Vec<Occurrence<T>>,
    latest: HashMap<String, usize>,
}

impl<T> Default for ToolCorrelations<T> {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            latest: HashMap::new(),
        }
    }
}

impl<T> ToolCorrelations<T> {
    pub fn len(&self) -> usize {
        self.calls.len()
    }
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn call(&mut self, handles: &[Option<&str>], value: T) -> anyhow::Result<()> {
        anyhow::ensure!(
            handles.iter().any(Option::is_some),
            "historical tool call has no correlation handle"
        );
        anyhow::ensure!(
            !handles
                .iter()
                .flatten()
                .filter_map(|&handle| self.latest.get(handle))
                .any(|index| !self.calls[*index].completed),
            "ambiguous reused historical tool handle"
        );
        for &handle in handles.iter().flatten() {
            if let Some(index) = self.latest.get_mut(handle) {
                *index = self.calls.len();
            } else {
                self.latest.insert(handle.to_owned(), self.calls.len());
            }
        }
        self.calls.push(Occurrence {
            value,
            completed: false,
        });
        Ok(())
    }

    pub fn result(&mut self, handles: &[Option<&str>]) -> anyhow::Result<&T> {
        const MISSING_OR_AMBIGUOUS: &str =
            "historical tool result has missing or ambiguous preceding call correlation";
        let mut candidate = None;
        for &handle in handles.iter().flatten() {
            if let Some(&index) = self.latest.get(handle) {
                anyhow::ensure!(
                    candidate.is_none_or(|prior| prior == index),
                    "{MISSING_OR_AMBIGUOUS}"
                );
                candidate = Some(index);
            }
        }
        let index = candidate.ok_or_else(|| anyhow::anyhow!(MISSING_OR_AMBIGUOUS))?;
        let call = &mut self.calls[index];
        anyhow::ensure!(!call.completed, "duplicate historical tool result");
        call.completed = true;
        Ok(&call.value)
    }

    fn unfinished(&self) -> impl Iterator<Item = &T> {
        self.calls
            .iter()
            .filter(|call| !call.completed)
            .map(|call| &call.value)
    }
}

/// `safe[k]` means no paired call/result straddles the exclusive cut `k`.
/// Self-contained native outputs (e.g. web search) need no external result;
/// their `_call` suffix alone does not make the source incomplete. If a later
/// result actually references one, its entire span is still protected.
pub fn replay_safe_prefixes(input: &[ModelRequestItem<'_>]) -> anyhow::Result<Vec<bool>> {
    let mut calls = ToolCorrelations::default();
    let mut crossings = vec![0_i64; input.len() + 2];
    let mut result = |calls: &mut ToolCorrelations<(usize, bool)>,
                      handles: &[Option<&str>],
                      end|
     -> anyhow::Result<()> {
        let (start, _) = *calls.result(handles)?;
        crossings[start + 1] += 1;
        crossings[end + 1] -= 1;
        Ok(())
    };
    for (index, item) in input.iter().enumerate() {
        if let Some(replay) = item.replay_ref() {
            replay.validate_known_items()?;
            if replay.protocol() == Some(crate::ReplayProtocol::Anthropic) {
                for (handles, external) in replay.tool_call_handles() {
                    calls.call(&handles, (index, external))?;
                }
                continue;
            }
            for native in &replay.items {
                let kind = native
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let handles = responses_tool_handles(native);
                let external = matches!(kind, "function_call" | "custom_tool_call");
                let has_handle = handles.iter().any(Option::is_some);
                anyhow::ensure!(
                    !external || has_handle,
                    "historical tool call has no correlation handle"
                );
                if matches!(kind, "function_call_output" | "custom_tool_call_output") {
                    anyhow::ensure!(
                        native.get("output").is_some(),
                        "malformed native tool output"
                    );
                    result(&mut calls, &handles, index)?;
                } else if kind.ends_with("_call") && has_handle {
                    calls.call(&handles, (index, external))?;
                }
            }
        } else if let Some(message) = item.message_ref() {
            match message {
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let AssistantContent::ToolCall(call) = block {
                            let id = call.id.wire();
                            let alias = call
                                .native
                                .as_ref()
                                .and_then(|native| native.item["id"].as_str())
                                .filter(|id| !id.is_empty());
                            calls.call(&[Some(&id), alias], (index, true))?;
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserContent::ToolResult(output) = block {
                            result(&mut calls, &[Some(&output.call.wire())], index)?;
                        }
                    }
                }
                Message::System { .. } => {}
            }
        }
    }
    anyhow::ensure!(
        !calls.unfinished().any(|(_, external)| *external),
        "historical external tool call has no result; cannot summarize an incomplete exchange"
    );
    let mut active = 0;
    Ok(crossings
        .into_iter()
        .take(input.len() + 1)
        .map(|delta| {
            active += delta;
            active == 0
        })
        .collect())
}
