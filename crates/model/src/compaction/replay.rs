//! Occurrence-based tool correlation shared with provider projection. Analysis
//! never rewrites an item or splits a native envelope.
use std::collections::{BTreeSet, HashMap};

use crate::ModelRequestItem;
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

    pub fn call(&mut self, handles: BTreeSet<String>, value: T) -> anyhow::Result<()> {
        anyhow::ensure!(
            !handles.is_empty(),
            "historical tool call has no correlation handle"
        );
        anyhow::ensure!(
            !handles
                .iter()
                .filter_map(|handle| self.latest.get(handle))
                .any(|index| !self.calls[*index].completed),
            "ambiguous reused historical tool handle"
        );
        for handle in handles {
            self.latest.insert(handle, self.calls.len());
        }
        self.calls.push(Occurrence {
            value,
            completed: false,
        });
        Ok(())
    }

    pub fn result(&mut self, handles: &BTreeSet<String>) -> anyhow::Result<&T> {
        let candidates = handles
            .iter()
            .filter_map(|handle| self.latest.get(handle).copied())
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            candidates.len() == 1,
            "historical tool result has missing or ambiguous preceding call correlation"
        );
        let call = &mut self.calls[*candidates.first().expect("one candidate")];
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
    let mut result =
        |calls: &mut ToolCorrelations<(usize, bool)>, handles, end| -> anyhow::Result<()> {
            let (start, _) = *calls.result(&handles)?;
            crossings[start + 1] += 1;
            crossings[end + 1] -= 1;
            Ok(())
        };
    for (index, item) in input.iter().enumerate() {
        if let Some(replay) = item.replay_ref() {
            replay.portable_projection()?;
            for native in &replay.items {
                let kind = native
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let handles = ["id", "call_id"]
                    .into_iter()
                    .filter_map(|key| native.get(key)?.as_str())
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>();
                anyhow::ensure!(
                    !matches!(kind, "function_call" | "custom_tool_call") || !handles.is_empty(),
                    "historical tool call has no correlation handle"
                );
                if matches!(kind, "function_call_output" | "custom_tool_call_output") {
                    anyhow::ensure!(
                        native.get("output").is_some(),
                        "malformed native tool output"
                    );
                    result(&mut calls, handles, index)?;
                } else if kind.ends_with("_call") && !handles.is_empty() {
                    calls.call(
                        handles,
                        (index, matches!(kind, "function_call" | "custom_tool_call")),
                    )?;
                }
            }
        } else if let Some(message) = item.message_ref() {
            match message {
                Message::Assistant { content, .. } => {
                    for block in content {
                        if let AssistantContent::ToolCall(call) = block {
                            let mut handles = BTreeSet::from([call.id.to_string()]);
                            if let Some(provider) = &call.provider {
                                handles.insert(provider.call_id.clone());
                                handles.extend(provider.item_id.iter().cloned());
                            }
                            calls.call(handles, (index, true))?;
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserContent::ToolResult(output) = block {
                            let mut handles = BTreeSet::from([output.call.to_string()]);
                            if let Some(provider) = &output.provider {
                                handles.insert(provider.call_id.clone());
                                handles.extend(provider.item_id.iter().cloned());
                            }
                            result(&mut calls, handles, index)?;
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
