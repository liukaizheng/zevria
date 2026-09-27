use std::path::Path;

use agent_client_protocol::schema::v1::SessionUpdate as AcpSessionUpdate;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::PlanRecord;

use crate::project::{
    KnownTools, diagnostic_update, project_completed_message, project_tool_calls,
    project_tool_results,
};

/// Project durable semantic conversation history into ordered ACP updates.
/// Metadata-only records remain hidden, and direct skill invocations use only
/// their compact display form rather than persisted activation bodies.
pub(crate) fn replay_transcript(
    items: &[TranscriptItem],
    workspace: &Path,
) -> Vec<AcpSessionUpdate> {
    let mut updates = Vec::new();
    let mut tools = KnownTools::new();
    let mut hosted = crate::project::HostedSearchProjection::default();
    let mut subtasks = crate::project::SubtaskProjection::default();

    let items = zevria_transcript::reconstruct_transcript(items);
    for (index, item) in items.iter().enumerate() {
        let prefix = format!("zevria-replay-{index}");
        match item {
            TranscriptItem::WebSearchAttempt(attempt) => {
                let mut projected = hosted.update(attempt);
                if !items
                    .iter()
                    .any(|item| item.display_attempt_id() == Some(attempt.id.as_str()))
                {
                    if let Some(message) = crate::stream::display_message(attempt) {
                        projected.extend(project_completed_message(&message, &prefix));
                    }
                    if let Some(display) = crate::stream::replay_display(attempt, &projected) {
                        projected.push(display);
                    }
                }
                updates.extend(projected);
            }
            TranscriptItem::RequestPrompt { .. } => {
                if let Some(message) = item.display_message() {
                    updates.extend(project_completed_message(&message, &prefix));
                }
            }
            TranscriptItem::Message(message) => {
                updates.extend(project_completed_message(message, &prefix));
                updates.extend(project_tool_calls(message, workspace, &mut tools));
            }
            TranscriptItem::ProviderMessage(_) | TranscriptItem::AssistantMessage { .. } => {
                if let Some(message) = item.message() {
                    let mut projected = project_completed_message(message, &prefix);
                    projected.extend(project_tool_calls(message, workspace, &mut tools));
                    if let Some(id) = item.display_attempt_id()
                        && let Some(attempt) = items
                            .iter()
                            .filter_map(|item| match item {
                                TranscriptItem::WebSearchAttempt(attempt) if attempt.id == id => {
                                    Some(attempt)
                                }
                                _ => None,
                            })
                            .max_by_key(|attempt| attempt.revision)
                        && let Some(display) = crate::stream::replay_display(attempt, &projected)
                    {
                        projected.push(display);
                    }
                    updates.extend(projected);
                }
            }
            TranscriptItem::ToolResults {
                message, metadata, ..
            } => {
                updates.extend(subtasks.results(metadata));
                updates.extend(project_tool_results(message, metadata, workspace, &tools));
            }
            TranscriptItem::SkillInvocation(invocation) => {
                updates.extend(project_completed_message(
                    &invocation.display_message(),
                    &format!("{prefix}-skill"),
                ));
            }
            TranscriptItem::Plan(PlanRecord::Handoff { handoff }) => {
                updates.extend(project_completed_message(&handoff.prompt, &prefix));
            }
            TranscriptItem::Error { error } => {
                updates.push(diagnostic_update(
                    None,
                    &format!("replay-error-{index}"),
                    error,
                ));
            }
            TranscriptItem::SessionModels(_)
            | TranscriptItem::SessionMode(_)
            | TranscriptItem::Directive(_)
            | TranscriptItem::RequestDirective(_)
            | TranscriptItem::Plan(_)
            | TranscriptItem::Ensemble(_)
            | TranscriptItem::Compaction(_) => {}
        }
    }

    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_command_replay_keeps_the_bounded_initial_title_and_full_payloads() {
        use rig_core::message::{AssistantContent, Message, ToolCall, ToolCallId, ToolFunction};
        use zevria_foundation::ToolCallOutcome;
        use zevria_foundation::ToolResultMetadata;

        let command = format!("rtk rg {}", "long-pattern".repeat(20));
        let input = serde_json::json!({"command": command});
        let items = [
            TranscriptItem::Message(Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall::new(
                    ToolCallId::new_or_mint("durable-call"),
                    ToolFunction::new("command".into(), input.clone()),
                ))],
            }),
            TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: Message::tool_result("durable-call", "command", "found"),
                metadata: vec![ToolResultMetadata {
                    diagnostic: None,
                    id: "durable-call".into(),
                    call_id: None,
                    tool_name: "command".into(),
                    outcome: ToolCallOutcome::Success,
                    detail: None,
                }],
            },
        ];
        // Round-trip the durable representation before projecting the loaded session.
        let items = items
            .iter()
            .map(|item| serde_json::from_str(&serde_json::to_string(item).unwrap()).unwrap())
            .collect::<Vec<_>>();
        let updates = replay_transcript(&items, Path::new("."));
        assert_eq!(updates.len(), 2);
        let initial = serde_json::to_value(&updates[0]).unwrap();
        let result = serde_json::to_value(&updates[1]).unwrap();
        let title = initial["title"].as_str().unwrap();
        assert!(title.starts_with("command: rtk rg "));
        assert!(title.len() < command.len());
        assert_eq!(initial["toolCallId"], "durable-call");
        assert_eq!(result["toolCallId"], initial["toolCallId"]);
        assert_eq!(initial["rawInput"], input);
        assert_eq!(result["rawInput"], input);
        assert_eq!(result["rawOutput"], "found");
        assert_eq!(result["kind"], "execute");
        assert_eq!(result["status"], "completed");
        assert!(result.get("title").is_none());
    }

    #[test]
    fn session_metadata_and_directives_emit_no_acp_updates() {
        let models = zevria_model::models::SessionModels::new(
            zevria_model::models::ModelSelection::new(
                zevria_foundation::ModelProfileRef::new("provider", "build"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            zevria_model::models::ModelSelection::new(
                zevria_foundation::ModelProfileRef::new("other", "plan"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
        let hidden = [
            TranscriptItem::SessionMode(zevria_foundation::SessionMode::Build),
            TranscriptItem::SessionModels(models),
            TranscriptItem::Directive(zevria_instructions::DirectiveContent::skill(
                &zevria_instructions::SkillSnapshot::new(
                    "hidden".parse().unwrap(),
                    "Hidden",
                    "HIDDEN_SKILL_BODY",
                )
                .unwrap(),
            )),
            TranscriptItem::Directive(
                zevria_instructions::DirectiveContent::new(
                    zevria_instructions::DirectivePayload::SkillRevocation {
                        name: "hidden".parse().unwrap(),
                        reason: "disabled".into(),
                    },
                )
                .unwrap(),
            ),
        ];
        assert!(replay_transcript(&hidden, Path::new(".")).is_empty());
        assert!(
            hidden
                .iter()
                .all(|item| !zevria_transcript::transcript::is_prompt_item(item))
        );
        assert!(zevria_transcript::transcript::retained_user_candidates(&hidden).is_empty());
    }
}
