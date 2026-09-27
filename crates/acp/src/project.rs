use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Diff, MessageId, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PlanUpdate, PlanUpdateContent, SessionUpdate as AcpSessionUpdate, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use serde_json::{Map, Value, json};
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOperation;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::ModelRole;
use zevria_foundation::TaskList;
use zevria_foundation::TaskStatus;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::ToolResultMetadata;
use zevria_foundation::TurnId;
use zevria_model::ContextTokenSnapshot;
use zevria_model::TokenUsage;
use zevria_workflow::PlanArtifact;

use crate::stream::assistant_reasoning_text;

const DIAGNOSTIC_MAX_CHARS: usize = 4_096;

#[cfg(test)]
#[path = "subtask_batch_tests.rs"]
mod subtask_batch_tests;

#[derive(Debug, Clone)]
pub(crate) struct KnownTool {
    pub arguments: Value,
    pub kind: ToolKind,
}

pub(crate) type KnownTools = HashMap<String, KnownTool>;

/// Display-only child lifecycles. These IDs never replace the provider's outer
/// call ID, and a child's completion never completes the outer batch.
#[derive(Default)]
pub(crate) struct SubtaskProjection {
    children: HashMap<
        zevria_foundation::SubtaskId,
        (String, usize, zevria_foundation::SubtaskDescriptor),
    >,
    statuses: HashMap<zevria_foundation::SubtaskId, zevria_foundation::SubtaskStatus>,
}

impl SubtaskProjection {
    pub(crate) fn launch(
        &mut self,
        call_id: &str,
        index: usize,
        mut child: zevria_foundation::SubtaskDescriptor,
    ) -> AcpSessionUpdate {
        if let Some(status) = self.statuses.get(&child.id) {
            child.status = *status;
        }
        if let Some((_, _, previous)) = self.children.get(&child.id)
            && (previous.status.is_terminal()
                || child.status == zevria_foundation::SubtaskStatus::Starting)
        {
            child.status = previous.status;
        }
        let previous = self
            .children
            .insert(child.id.clone(), (call_id.into(), index, child.clone()));
        let id = format!("zevria-subtask-{}", child.id);
        let title = format!("{} · {} · {}", child.kind, child.title, child.status);
        let status = subtask_status(child.status);
        let input = json!({"origin":"subtask", "parent_call_id":call_id, "entry_index":index, "child_id":child.id, "kind":child.kind, "workspace":child.workspace});
        if previous.is_some() {
            AcpSessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id,
                ToolCallUpdateFields::new()
                    .title(title)
                    .status(status)
                    .raw_input(input),
            ))
        } else {
            AcpSessionUpdate::ToolCall(
                AcpToolCall::new(id, title)
                    .kind(ToolKind::Think)
                    .status(status)
                    .raw_input(input),
            )
        }
    }

    pub(crate) fn status(
        &mut self,
        id: &zevria_foundation::SubtaskId,
        status: zevria_foundation::SubtaskStatus,
    ) -> Option<AcpSessionUpdate> {
        let current = self.statuses.entry(id.clone()).or_insert(status);
        if !current.is_terminal() && status != zevria_foundation::SubtaskStatus::Starting {
            *current = status;
        }
        let (call, index, mut child) = self.children.get(id)?.clone();
        child.status = *current;
        Some(self.launch(&call, index, child))
    }

    pub(crate) fn results(&mut self, metadata: &[ToolResultMetadata]) -> Vec<AcpSessionUpdate> {
        let mut updates = Vec::new();
        for batch in metadata {
            for entry in batch.subtasks() {
                let Some(launch) = &entry.launch else {
                    continue;
                };
                self.statuses.insert(launch.id.clone(), entry.status);
                updates.push(self.launch(
                    &batch.id,
                    entry.index,
                    zevria_foundation::SubtaskDescriptor {
                        id: launch.id.clone(),
                        parent_session_id: String::new(),
                        title: launch.title.clone(),
                        kind: launch.kind,
                        workspace: launch.workspace.clone(),
                        status: entry.status,
                    },
                ));
            }
        }
        updates
    }
}

fn subtask_status(status: zevria_foundation::SubtaskStatus) -> ToolCallStatus {
    use zevria_foundation::SubtaskStatus;
    match status {
        SubtaskStatus::Starting => ToolCallStatus::Pending,
        SubtaskStatus::Running => ToolCallStatus::InProgress,
        SubtaskStatus::Completed => ToolCallStatus::Completed,
        SubtaskStatus::Failed | SubtaskStatus::Cancelled => ToolCallStatus::Failed,
    }
}

/// Kept separate from local function correlations, including in replay.
#[derive(Default)]
pub(crate) struct HostedSearchProjection(
    HashMap<String, (String, zevria_content::WebSearchActivity)>,
);
impl HostedSearchProjection {
    pub(crate) fn update(
        &mut self,
        attempt: &zevria_content::WebSearchAttemptRecord,
    ) -> Vec<AcpSessionUpdate> {
        attempt.activity.iter().filter_map(|activity| {
            let id = activity.client_id(&attempt.id);
            // Keep the existing standard ACP lifecycle projection. Richer
            // confirmed/unconfirmed evidence is carried only in response_display.
            let projected_status = if activity.status.is_terminal() { activity.status } else {
                match attempt.outcome {
                    zevria_content::WebSearchAttemptOutcome::InProgress => activity.status,
                    zevria_content::WebSearchAttemptOutcome::Failed => zevria_content::WebSearchStatus::Failed,
                    _ => zevria_content::WebSearchStatus::Interrupted,
                }
            };
            let title = format!("{} · {} · attempt {:?}", activity.label(), projected_status.label(), attempt.outcome);
            if self.0.get(&id).is_some_and(|prior| prior == &(title.clone(), activity.clone())) { return None; }
            let kind = match activity.action_kind() { Some("open_page") => ToolKind::Fetch, Some("search" | "find_in_page") => ToolKind::Search, _ => ToolKind::Other };
            let status = match projected_status {
                zevria_content::WebSearchStatus::InProgress | zevria_content::WebSearchStatus::Searching => ToolCallStatus::InProgress,
                zevria_content::WebSearchStatus::Completed => ToolCallStatus::Completed,
                zevria_content::WebSearchStatus::Failed | zevria_content::WebSearchStatus::Interrupted => ToolCallStatus::Failed,
            };
            let raw = json!({"origin":"provider_hosted_web_search","attempt_id":attempt.id,"provider":attempt.profile.provider,"model":attempt.profile.model,"response_id":attempt.response_id,"action":activity.action});
            let previous = self.0.insert(id.clone(), (title.clone(), activity.clone()));
            Some(if previous.is_none() {
                AcpSessionUpdate::ToolCall(AcpToolCall::new(id, title).kind(kind).status(status).raw_input(raw))
            } else {
                AcpSessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, ToolCallUpdateFields::new().title(title).kind(kind).status(status).raw_input(raw)))
            })
        }).collect()
    }
}

pub(crate) fn project_completed_message(
    message: &Message,
    message_id_prefix: &str,
) -> Vec<AcpSessionUpdate> {
    let mut updates = Vec::new();
    match message {
        Message::User { content } => {
            for (index, item) in content.iter().enumerate() {
                let id = format!("{message_id_prefix}-user-{index}");
                match item {
                    UserContent::Text(text) => updates.push(AcpSessionUpdate::UserMessageChunk(
                        content_chunk(&text.text, id),
                    )),
                    UserContent::Image(_) => {
                        match zevria_content::PromptImage::from_user_content(item) {
                            Ok(image) => updates.push(AcpSessionUpdate::UserMessageChunk(
                                ContentChunk::new(ContentBlock::Image(
                                    agent_client_protocol::schema::v1::ImageContent::new(
                                        image.base64(),
                                        image.mime_type(),
                                    ),
                                ))
                                .message_id(MessageId::new(id)),
                            )),
                            Err(_) => updates.push(AcpSessionUpdate::UserMessageChunk(
                                content_chunk("[unsupported user image]", id),
                            )),
                        }
                    }
                    _ => {}
                }
            }
        }
        Message::Assistant { content, .. } => {
            let mut text_index = 0usize;
            let mut thought_index = 0usize;
            for item in content {
                match item {
                    AssistantContent::Text(text) => {
                        updates.push(AcpSessionUpdate::AgentMessageChunk(content_chunk(
                            zevria_content::citations::render_text(text),
                            format!("{message_id_prefix}-agent-{text_index}"),
                        )));
                        text_index += 1;
                    }
                    AssistantContent::Reasoning(_) => {
                        let reasoning = assistant_reasoning_text(&Message::Assistant {
                            id: None,
                            content: vec![item.clone()],
                        });
                        if !reasoning.is_empty() {
                            updates.push(AcpSessionUpdate::AgentThoughtChunk(content_chunk(
                                &reasoning,
                                format!("{message_id_prefix}-thought-{thought_index}"),
                            )));
                            thought_index += 1;
                        }
                    }
                    AssistantContent::ToolCall(_) | AssistantContent::Image(_) => {}
                }
            }
        }
        Message::System { content } => {
            updates.push(AcpSessionUpdate::AgentThoughtChunk(content_chunk(
                content,
                format!("{message_id_prefix}-system"),
            )));
        }
    }
    updates
}

pub(crate) fn project_tool_calls(
    message: &Message,
    workspace: &Path,
    known_tools: &mut KnownTools,
) -> Vec<AcpSessionUpdate> {
    assistant_tool_calls(message)
        .into_iter()
        .flat_map(|call| {
            let id = call.id.to_string();
            let kind = tool_kind(&call.function.name, &call.function.arguments);
            let title = tool_title(&call.function.name, &call.function.arguments);
            let locations = infer_locations(workspace, &call.function.arguments);
            known_tools.insert(
                id.clone(),
                KnownTool {
                    arguments: call.function.arguments.clone(),
                    kind,
                },
            );
            let mut updates = vec![AcpSessionUpdate::ToolCall(
                AcpToolCall::new(id, title)
                    .kind(kind)
                    .status(ToolCallStatus::InProgress)
                    .locations(locations)
                    .raw_input(call.function.arguments.clone()),
            )];
            if call.function.name == zevria_foundation::TASK_TOOL_NAME
                && let Ok(tasks) = TaskList::from_tool_arguments(&call.function.arguments)
            {
                updates.push(AcpSessionUpdate::Plan(task_plan(&tasks)));
            }
            updates
        })
        .collect()
}

pub(crate) fn project_tool_results(
    message: &Message,
    metadata: &[ToolResultMetadata],
    workspace: &Path,
    known_tools: &KnownTools,
) -> Vec<AcpSessionUpdate> {
    let outputs = tool_outputs(message);
    metadata
        .iter()
        .map(|metadata| {
            let output = outputs.get(&metadata.id);
            let known = known_tools.get(&metadata.id);
            let mut fields = ToolCallUpdateFields::new()
                .status(outcome_status(metadata.outcome))
                .kind(known.map(|known| known.kind))
                .raw_output(output.and_then(|output| output.raw.clone()));

            let mut content = Vec::new();
            if let Some(output) = output
                && !output.text.is_empty()
            {
                content.push(ToolCallContent::from(ContentBlock::Text(TextContent::new(
                    output.text.clone(),
                ))));
            }
            content.extend(file_change_content(metadata.file_changes(), workspace));
            if !content.is_empty() {
                fields = fields.content(content);
            }
            if let Some(known) = known {
                fields = fields.raw_input(known.arguments.clone());
            }
            let mut meta = Map::new();
            meta.insert(
                zevria_foundation::TOOL_RESULT_META_KEY.into(),
                serde_json::to_value(metadata).expect("tool-result sidecar is serializable"),
            );
            AcpSessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(metadata.id.clone(), fields).meta(meta),
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct ResponseUsageSnapshot {
    pub usage: TokenUsage,
    pub profile: ModelProfileRef,
    pub model_role: ModelRole,
    pub input_token_limit: u64,
    pub context_window_tokens: u64,
}

pub(crate) fn usage_update(
    response: Option<&ResponseUsageSnapshot>,
    context: &ContextTokenSnapshot,
) -> AcpSessionUpdate {
    let mut zevria = Map::new();
    zevria.insert(
        "projectedInputTokens".to_string(),
        json!(context.projected_input_tokens),
    );
    zevria.insert("countSource".to_string(), json!(context.source.label()));
    zevria.insert(
        "inputTokenLimit".to_string(),
        json!(context.input_token_limit),
    );
    zevria.insert(
        "contextWindowTokens".to_string(),
        json!(context.context_window_tokens),
    );
    zevria.insert(
        "projectedProvider".to_string(),
        json!(context.profile.provider),
    );
    zevria.insert("projectedModel".to_string(), json!(context.profile.model));
    zevria.insert(
        "projectedRole".to_string(),
        json!(context.model_role.name()),
    );
    if let Some(response) = response {
        zevria.insert(
            "inputTokens".to_string(),
            json!(response.usage.input_tokens),
        );
        zevria.insert(
            "cachedTokens".to_string(),
            json!(response.usage.cached_tokens),
        );
        zevria.insert(
            "outputTokens".to_string(),
            json!(response.usage.output_tokens),
        );
        zevria.insert(
            "totalTokens".to_string(),
            json!(response.usage.total_tokens),
        );
        zevria.insert("provider".to_string(), json!(response.profile.provider));
        zevria.insert("model".to_string(), json!(response.profile.model));
        zevria.insert("role".to_string(), json!(response.model_role.name()));
        zevria.insert(
            "responseInputTokenLimit".to_string(),
            json!(response.input_token_limit),
        );
        zevria.insert(
            "responseContextWindowTokens".to_string(),
            json!(response.context_window_tokens),
        );
    } else {
        zevria.insert("provider".to_string(), json!(context.profile.provider));
        zevria.insert("model".to_string(), json!(context.profile.model));
        zevria.insert("role".to_string(), json!(context.model_role.name()));
    }
    let mut meta = Map::new();
    meta.insert("zevria".to_string(), Value::Object(zevria));
    AcpSessionUpdate::UsageUpdate(
        UsageUpdate::new(context.projected_input_tokens, context.input_token_limit).meta(meta),
    )
}

pub(crate) fn diagnostic_update(
    turn_id: Option<TurnId>,
    category: &str,
    text: impl AsRef<str>,
) -> AcpSessionUpdate {
    let text = bounded(text.as_ref(), DIAGNOSTIC_MAX_CHARS);
    let id = turn_id.map_or_else(
        || format!("zevria-status-{category}"),
        |turn_id| format!("zevria-turn-{}-status-{category}", turn_id.get()),
    );
    AcpSessionUpdate::AgentThoughtChunk(content_chunk(text, id))
}

pub(crate) fn plan_artifact_update(artifact: &PlanArtifact) -> AcpSessionUpdate {
    AcpSessionUpdate::AgentMessageChunk(content_chunk(
        &artifact.markdown,
        format!("zevria-plan-{}-artifact", artifact.version),
    ))
}

/// Only authoritative, durably Ready worker artifacts use the proof channel.
/// Preserve canonical Markdown bytes; the parent owns synthesis and approval.
pub(crate) fn worker_plan_update(artifact: &PlanArtifact) -> AcpSessionUpdate {
    AcpSessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::markdown(
        format!("zevria-plan-{}", artifact.version.id),
        artifact.markdown.clone(),
    )))
}

pub(crate) fn plan_ready_instructions(artifact: &PlanArtifact) -> AcpSessionUpdate {
    AcpSessionUpdate::AgentThoughtChunk(content_chunk(
        "The Plan artifact is ready. Choose Implement in this session or Revise when prompted. If this client cannot show forms, send /implement to approve it; any other prompt revises it in Plan mode. Implement Fresh is not available through ACP v1.",
        format!("zevria-plan-{}-instructions", artifact.version),
    ))
}

pub(crate) fn content_chunk(text: impl Into<String>, id: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
        .message_id(MessageId::new(id.into()))
}

pub(crate) fn assistant_tool_calls(message: &Message) -> Vec<&ToolCall> {
    let Message::Assistant { content, .. } = message else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn task_plan(tasks: &TaskList) -> Plan {
    Plan::new(
        tasks
            .tasks
            .iter()
            .map(|task| {
                PlanEntry::new(
                    task.step.clone(),
                    PlanEntryPriority::Medium,
                    match task.status {
                        TaskStatus::Pending => PlanEntryStatus::Pending,
                        TaskStatus::InProgress => PlanEntryStatus::InProgress,
                        TaskStatus::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect(),
    )
}

fn tool_kind(name: &str, arguments: &Value) -> ToolKind {
    match name {
        "command" => ToolKind::Execute,
        "edit" | "write" => {
            if arguments
                .get("move_to")
                .is_some_and(|value| !value.is_null())
            {
                ToolKind::Move
            } else {
                ToolKind::Edit
            }
        }
        "delete" => ToolKind::Delete,
        "launch_subtasks" => ToolKind::Think,
        "task" | "reconcile_reports" | "question" | "submit_plan" | "skill" => ToolKind::Think,
        _ if name.contains("read") => ToolKind::Read,
        _ if name.contains("search") || name.contains("find") => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

fn tool_title(name: &str, arguments: &Value) -> String {
    let detail = ["file_path", "path", "title", "command", "name"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str))
        .map(|value| bounded(value, 120));
    detail.map_or_else(|| name.to_string(), |detail| format!("{name}: {detail}"))
}

fn infer_locations(workspace: &Path, arguments: &Value) -> Vec<ToolCallLocation> {
    let mut paths = Vec::new();
    collect_paths(arguments, None, &mut paths);
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .take(16)
        .map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            };
            ToolCallLocation::new(path)
        })
        .collect()
}

fn collect_paths(value: &Value, key: Option<&str>, paths: &mut Vec<PathBuf>) {
    match value {
        Value::Object(object) => {
            for (child_key, child) in object {
                collect_paths(child, Some(child_key), paths);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_paths(child, key, paths);
            }
        }
        Value::String(path)
            if key.is_some_and(|key| {
                matches!(key, "file_path" | "path" | "move_to" | "source" | "target")
                    || key.ends_with("_path")
            }) && !path.trim().is_empty() =>
        {
            paths.push(PathBuf::from(path));
        }
        _ => {}
    }
}

#[derive(Debug, Clone)]
struct ToolOutput {
    text: String,
    raw: Option<Value>,
}

fn tool_outputs(message: &Message) -> HashMap<String, ToolOutput> {
    let Message::User { content } = message else {
        return HashMap::new();
    };
    content
        .iter()
        .filter_map(|content| {
            let UserContent::ToolResult(result) = content else {
                return None;
            };
            Some((result.call.to_string(), tool_output(result)))
        })
        .collect()
}

fn tool_output(result: &ToolResult) -> ToolOutput {
    let mut text = Vec::new();
    let mut raw = Vec::new();
    for content in &result.content {
        match content {
            ToolResultContent::Text(value) => {
                text.push(value.text.clone());
                raw.push(Value::String(value.text.clone()));
            }
            ToolResultContent::Json { value } => {
                text.push(
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                );
                raw.push(value.clone());
            }
            ToolResultContent::Image(_) => {
                text.push("[image tool output omitted]".to_string());
                raw.push(Value::String("[image tool output omitted]".to_string()));
            }
        }
    }
    ToolOutput {
        text: text.join("\n"),
        raw: match raw.as_slice() {
            [] => None,
            [value] => Some(value.clone()),
            values => Some(Value::Array(values.to_vec())),
        },
    }
}

fn outcome_status(outcome: ToolCallOutcome) -> ToolCallStatus {
    match outcome {
        ToolCallOutcome::Success => ToolCallStatus::Completed,
        ToolCallOutcome::Error
        | ToolCallOutcome::Skipped
        | ToolCallOutcome::Denied
        | ToolCallOutcome::Cancelled
        | ToolCallOutcome::Partial => ToolCallStatus::Failed,
    }
}

fn file_change_content(
    changes: &[zevria_foundation::FileChangeOutput],
    workspace: &Path,
) -> Vec<ToolCallContent> {
    changes
        .iter()
        .map(|output| {
            let path = if output.path.is_absolute() {
                output.path.clone()
            } else {
                workspace.join(&output.path)
            };
            match &output.change {
                FileChange::Add { content } => ToolCallContent::from(Diff::new(path, content)),
                FileChange::Delete { content } => {
                    ToolCallContent::from(Diff::new(path, "").old_text(content.clone()))
                }
                FileChange::Update {
                    unified_diff,
                    move_path,
                } => {
                    let mut text = format!("```diff\n{}\n```", unified_diff.trim_end());
                    if let Some(move_path) = move_path {
                        text.push_str(&format!("\nMoved to: {}", move_path.display()));
                    }
                    ToolCallContent::from(ContentBlock::Text(TextContent::new(text)))
                }
                FileChange::Omitted {
                    operation,
                    reason,
                    added,
                    removed,
                    bytes,
                } => ToolCallContent::from(ContentBlock::Text(TextContent::new(format!(
                    "{} diff omitted: {} (+{}, -{}, {} bytes)",
                    operation_label(*operation),
                    bounded(reason, 1_024),
                    added,
                    removed,
                    bytes
                )))),
            }
        })
        .collect()
}

fn operation_label(operation: FileChangeOperation) -> &'static str {
    match operation {
        FileChangeOperation::Add => "add",
        FileChangeOperation::Delete => "delete",
        FileChangeOperation::Update => "update",
    }
}

fn bounded(value: &str, max_chars: usize) -> String {
    let mut bounded = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        bounded.push('…');
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_ids_kinds_statuses_and_replay_are_independent() {
        use zevria_content::WebSearchActivity;
        use zevria_content::WebSearchAttemptOutcome;
        use zevria_content::WebSearchAttemptRecord;
        use zevria_content::WebSearchStatus;
        let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
        attempt.activity.push(WebSearchActivity {
            item_id: Some("provider-id".into()),
            output_index: 1,
            status: WebSearchStatus::Searching,
            action: Some(json!({"type":"open_page","url":"https://example.com"})),
        });
        let mut projection = HostedSearchProjection::default();
        let updates = projection.update(&attempt);
        let AcpSessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("hosted tool projection")
        };
        assert_eq!(call.kind, ToolKind::Fetch);
        assert_eq!(call.status, ToolCallStatus::InProgress);
        assert!(call.tool_call_id.to_string().starts_with("hosted-search:"));
        assert!(projection.update(&attempt).is_empty());
        attempt.finish(WebSearchAttemptOutcome::Interrupted);
        let updates = projection.update(&attempt);
        let AcpSessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("update")
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
        assert!(
            update
                .fields
                .title
                .as_ref()
                .unwrap()
                .contains("interrupted")
        );
        let restored = crate::replay::replay_transcript(
            &[zevria_transcript::transcript::TranscriptItem::WebSearchAttempt(attempt)],
            Path::new("/tmp"),
        );
        assert!(
            matches!(&restored[0], AcpSessionUpdate::ToolCall(call) if call.status == ToolCallStatus::Failed)
        );
    }

    #[test]
    fn cited_live_blocks_terminal_and_replay_agree_without_appending_twice() {
        let replay = zevria_model::ProviderReplay::openai_responses(
            ModelProfileRef::new("p", "m"),
            vec![
                json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Claim","annotations":[{"type":"url_citation","start_index":0,"end_index":5,"title":"Source","url":"https://example.com"}]}]}),
            ],
        );
        let message = replay.to_message().unwrap();
        let mut stream = crate::stream::StreamSegments::default();
        let first = stream.snapshot(TurnId::new(1), &message);
        assert!(stream.terminal(TurnId::new(1), &message).is_empty());
        let final_projection = project_completed_message(&message, "replay");
        let content = |update: &AcpSessionUpdate| match update {
            AcpSessionUpdate::AgentMessageChunk(chunk) => {
                serde_json::to_value(&chunk.content).unwrap()
            }
            _ => panic!("text chunk"),
        };
        assert_eq!(content(&first[0]), content(&final_projection[0]));
        assert!(
            content(&first[0])
                .to_string()
                .contains("[Source](https://example.com/)")
        );
    }

    #[test]
    fn usage_projection_uses_event_specific_denominator_and_profile_metadata() {
        let response = ResponseUsageSnapshot {
            usage: TokenUsage {
                input_tokens: 80,
                cached_tokens: 20,
                output_tokens: 10,
                total_tokens: 90,
            },
            profile: ModelProfileRef::new("gateway", "vendor.model.v2"),
            model_role: ModelRole::Review,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        };
        let context = ContextTokenSnapshot {
            profile: response.profile.clone(),
            model_role: response.model_role,
            projected_input_tokens: 91,
            source: zevria_model::ContextTokenSource::UsagePlusDelta,
            automatic_trigger: 90_000,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        };
        let update = usage_update(Some(&response), &context);
        let value = serde_json::to_value(update).expect("ACP usage JSON");
        let wire = value.to_string();
        for expected in [
            "128000",
            "gateway",
            "vendor.model.v2",
            "review",
            "contextWindowTokens",
            "cachedTokens",
            "projectedInputTokens",
            "inputTokenLimit",
        ] {
            assert!(wire.contains(expected), "missing {expected}: {wire}");
        }
    }

    #[test]
    fn large_readable_file_changes_project_exactly_without_local_truncation() {
        let added = "complete ACP addition\n".repeat(512 * 1024 / 22 + 1);
        let deleted = "complete ACP deletion\n".repeat(512 * 1024 / 22 + 1);
        let context = (0..30_000)
            .map(|index| format!(" context ACP line {index:05}\n"))
            .collect::<String>();
        let unified_diff = format!(
            "--- original\n+++ modified\n@@ -1,30001 +1,30001 @@\n{context}-old ACP value\n+new ACP value\n"
        );
        assert!(added.len() > 512 * 1024);
        assert!(deleted.len() > 512 * 1024);
        assert!(unified_diff.len() > 512 * 1024);

        let workspace = PathBuf::from("workspace-root");
        let changes = vec![
            zevria_foundation::FileChangeOutput {
                path: "added.txt".into(),
                change: FileChange::Add {
                    content: added.clone(),
                },
            },
            zevria_foundation::FileChangeOutput {
                path: "deleted.txt".into(),
                change: FileChange::Delete {
                    content: deleted.clone(),
                },
            },
            zevria_foundation::FileChangeOutput {
                path: "updated.txt".into(),
                change: FileChange::Update {
                    unified_diff: unified_diff.clone(),
                    move_path: None,
                },
            },
        ];

        let projected = file_change_content(&changes, &workspace);
        let expected = vec![
            ToolCallContent::from(Diff::new(workspace.join("added.txt"), added)),
            ToolCallContent::from(Diff::new(workspace.join("deleted.txt"), "").old_text(deleted)),
            ToolCallContent::from(ContentBlock::Text(TextContent::new(format!(
                "```diff\n{}\n```",
                unified_diff.trim_end()
            )))),
        ];
        assert_eq!(
            serde_json::to_value(projected).expect("projected file changes"),
            serde_json::to_value(expected).expect("expected file changes")
        );
    }

    #[test]
    fn reconciliation_projects_as_a_thinking_tool() {
        assert_eq!(
            tool_kind("reconcile_reports", &serde_json::json!({})),
            ToolKind::Think
        );
    }
}
