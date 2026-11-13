//! Transcript projection responsibilities.
use super::item::TranscriptItem;
use crate::ModelRequestItem;
use crate::compaction::CompactionCheckpoint;
use crate::compaction::is_summary_message;
use crate::ensemble::EnsembleRecord;
use crate::plan::PlanRecord;
use crate::task::TASK_TOOL_NAME;
use crate::task::TaskList;
use crate::tool_result::ToolResultMetadata;
use rig_core::message::AssistantContent;
use rig_core::message::Message;
use rig_core::message::ProviderCallId;
use rig_core::message::ToolResult;
use rig_core::message::UserContent;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// Whether `item` is a *prompt item*: one a frontend renders as a user
/// message row. These are exactly the plain user messages and `$skill`
/// invocations, in the order they were recorded, so a frontend and the engine
/// number the same turns without exchanging any message content.
///
/// Tool-result batches carry no prompt and are deliberately excluded — as are
/// assistant messages, subtask result blocks, and error notices. Typed Plan
/// handoffs supply model instructions but do not consume editable prompt rows.
pub fn is_prompt_item(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::SkillInvocation(_)
            | TranscriptItem::Message(Message::User { .. })
            | TranscriptItem::RequestPrompt { .. }
    )
}

/// Whether a record establishes conversational content for a newly allocated
/// session. This is intentionally independent of historical abandonment rules
/// and editable-prompt numbering. Responses/tool evidence are substantive even
/// without a projected user message; directives, headers and notices are not.
pub fn is_conversation_item(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Message(_)
            | TranscriptItem::RequestPrompt { .. }
            | TranscriptItem::ProviderMessage(_)
            | TranscriptItem::ToolResults { .. }
            | TranscriptItem::IncompleteResponse(_)
            | TranscriptItem::CommandJobNotice(_)
            | TranscriptItem::SkillInvocation(_)
            | TranscriptItem::Plan(PlanRecord::Handoff { .. })
            | TranscriptItem::Ensemble(EnsembleRecord::Started { .. })
            | TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
            | TranscriptItem::Compaction(_)
    )
}

/// Whether an item supplies user-role model input that can anchor context
/// compaction. Typed Plan handoffs and ensemble evidence are included without
/// becoming editable/numbered frontend prompt rows. Not every anchor supplies
/// retained user instructions: ensemble evidence remains summary material.
pub fn is_compaction_prompt_item(item: &TranscriptItem) -> bool {
    is_prompt_item(item)
        || matches!(
            item,
            TranscriptItem::Plan(PlanRecord::Handoff { .. })
                | TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
        )
}

/// Extract only ordinary Rig messages for resumed model history.
///
/// Compound tool-result metadata is intentionally inaccessible through this
/// path, preserving the structural model/UI boundary.
pub fn model_history(items: &[TranscriptItem]) -> Vec<Message> {
    model_input(items)
        .into_iter()
        .filter_map(ModelRequestItem::message_ref)
        .cloned()
        .collect()
}

/// Exact checkpoint-aware model projection for a raw transcript slice.
pub fn model_input(items: &[TranscriptItem]) -> Vec<ModelRequestItem<'_>> {
    model_input_from_records(items.iter())
}

/// Project a prospective request from borrowed history/checkpoint/suffix parts.
/// No full transcript payload copy is needed for estimation or exact counting.
pub fn model_input_from_records<'a>(
    items: impl Iterator<Item = &'a TranscriptItem> + Clone,
) -> Vec<ModelRequestItem<'a>> {
    let latest = items
        .clone()
        .enumerate()
        .filter_map(|(index, item)| match item {
            TranscriptItem::Compaction(checkpoint) => Some((index, checkpoint)),
            _ => None,
        })
        .last();
    let (mut input, start) = match latest {
        Some((index, checkpoint)) => (
            model_input_with_checkpoint(items.clone().take(index), checkpoint),
            index.saturating_add(1),
        ),
        None => (Vec::new(), 0),
    };
    input.extend(
        items
            .skip(start)
            .filter_map(TranscriptItem::model_request_item),
    );
    input
}

/// Project a checkpoint against its retained prefix without installing or
/// cloning the checkpoint. Prepared prompt records may extend this projection.
pub fn model_input_with_checkpoint<'a>(
    prefix: impl Iterator<Item = &'a TranscriptItem> + Clone,
    checkpoint: &'a CompactionCheckpoint,
) -> Vec<ModelRequestItem<'a>> {
    let mut input = checkpoint
        .replacement_history
        .iter()
        .map(zevria_model::OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    input.extend(
        zevria_instructions::directive::effective_directives(prefix.clone().filter_map(|item| {
            match item {
                TranscriptItem::Directive(directive) => Some(directive),
                _ => None,
            }
        }))
        .into_iter()
        .map(ModelRequestItem::DeveloperInstruction),
    );
    input.extend(
        crate::request_replay::effective_request_directives(prefix)
            .into_iter()
            .map(ModelRequestItem::RequestInstruction),
    );
    input
}

#[derive(Debug)]
struct PendingTaskCall {
    id: String,
    call_id: Option<String>,
    item_index: usize,
    content_index: usize,
    snapshot: TaskList,
}

/// Replay the task-call/result lifecycle and return only the latest snapshot
/// confirmed by a correlated successful result. Invalid, failed, denied, and
/// interrupted calls cannot replace the previously installed checklist.
pub fn latest_successful_task_snapshot(items: &[TranscriptItem]) -> Option<TaskList> {
    let mut pending = Vec::new();
    let mut latest = None;

    for (item_index, item) in items.iter().enumerate() {
        let Some(message) = item.message() else {
            continue;
        };
        match message {
            Message::Assistant(assistant) => {
                for (content_index, content) in assistant.content.iter().enumerate() {
                    let AssistantContent::ToolCall(call) = content else {
                        continue;
                    };
                    if call.function.name != TASK_TOOL_NAME
                        || call.function.invalid_arguments.is_some()
                    {
                        continue;
                    }
                    let Ok(snapshot) =
                        TaskList::from_tool_arguments(&call.function.arguments_value())
                    else {
                        continue;
                    };
                    pending.push(PendingTaskCall {
                        id: call.id.to_string(),
                        call_id: provider_call_id(call.id.provider()).map(str::to_owned),
                        item_index,
                        content_index,
                        snapshot,
                    });
                }
            }
            Message::User { content } => {
                let metadata = match item {
                    TranscriptItem::ToolResults { metadata, .. } => metadata.as_slice(),
                    _ => &[],
                };
                let mut available_metadata = (0..metadata.len()).collect::<Vec<_>>();
                for content in content {
                    let UserContent::ToolResult(result) = content else {
                        continue;
                    };
                    if result.name != TASK_TOOL_NAME {
                        continue;
                    }
                    let Some(pending_index) = select_pending_task_call(&pending, result) else {
                        continue;
                    };
                    let completed = pending.remove(pending_index);
                    let successful =
                        take_matching_metadata(metadata, &mut available_metadata, result)
                            .is_some_and(|metadata| metadata.outcome.is_success());
                    if successful {
                        latest = Some(completed.snapshot);
                    }
                }
            }
            Message::System { .. } => {}
        }
    }

    latest
}

fn select_pending_task_call(pending: &[PendingTaskCall], result: &ToolResult) -> Option<usize> {
    let result_call_id = provider_call_id(result.call.provider());
    [true, false].into_iter().find_map(|exact_call_id| {
        pending
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                call.id == result.call.wire().as_ref()
                    && call_ids_match(call.call_id.as_deref(), result_call_id, exact_call_id)
            })
            .max_by_key(|(_, call)| (call.item_index, std::cmp::Reverse(call.content_index)))
            .map(|(index, _)| index)
    })
}

fn take_matching_metadata<'metadata>(
    metadata: &'metadata [ToolResultMetadata],
    available: &mut Vec<usize>,
    result: &ToolResult,
) -> Option<&'metadata ToolResultMetadata> {
    let result_call_id = provider_call_id(result.call.provider());
    [true, false].into_iter().find_map(|exact_call_id| {
        let position = available.iter().position(|&index| {
            let metadata = &metadata[index];
            metadata.id == result.call.wire().as_ref()
                && metadata.tool_name == result.name
                && call_ids_match(metadata.call_id.as_deref(), result_call_id, exact_call_id)
        })?;
        Some(&metadata[available.remove(position)])
    })
}

fn call_ids_match(left: Option<&str>, right: Option<&str>, exact_call_id: bool) -> bool {
    let exact = left == right;
    if exact_call_id {
        exact
    } else {
        exact || left.is_none() || right.is_none()
    }
}

fn provider_call_id(provider: Option<&ProviderCallId>) -> Option<&str> {
    provider.map(ProviderCallId::as_str)
}

/// User/direct-skill prompts and typed Plan handoff instructions eligible for
/// bounded retention in a checkpoint over this transcript slice. Carry forward
/// only the latest checkpoint's retained text, not the full original prompts.
/// Edit preparation uses a retained prefix rather than the discarded tail.
pub fn retained_user_candidates(items: &[TranscriptItem]) -> Vec<String> {
    let (mut candidates, start) = items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| match item {
            TranscriptItem::Compaction(checkpoint) => Some((index, checkpoint)),
            _ => None,
        })
        .map_or((Vec::new(), 0), |(index, checkpoint)| {
            (
                checkpoint.retained_user_messages.clone(),
                index.saturating_add(1),
            )
        });
    candidates.extend(items[start..].iter().filter_map(prompt_text));
    candidates
}

/// Retained instruction text is distinct from both editable prompt rows and
/// compaction anchors; in particular, ensemble evidence is not reinjected here.
fn prompt_text(item: &TranscriptItem) -> Option<String> {
    if !is_prompt_item(item) && !matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { .. })) {
        return None;
    }
    let Message::User { content } = item.message()? else {
        return None;
    };
    let text = content
        .iter()
        .filter_map(|content| match content {
            UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty() && !is_summary_message(&text)).then_some(text)
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tool_result_tests.rs"]
mod tool_result_tests;
