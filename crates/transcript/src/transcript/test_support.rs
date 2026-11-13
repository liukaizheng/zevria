#![allow(unused_imports)]
pub(super) use super::test_prelude::*;
pub(super) use rig_core::message::{
    AssistantContent, CallId, ToolCall, ToolFunction, ToolResultContent,
};
pub(super) use serde_json::json;

pub(super) use crate::skill::{SkillApplication, SkillName, SkillSnapshot};
pub(super) use crate::test_support::TranscriptRewriteBlocker;
pub(super) use crate::{
    CompactionBackend, CompactionTrigger, FileChange, FileChangeOperation, FileChangeOutput,
    OwnedModelRequestItem, PlanArtifact, PlanHandoff, PlanId, PlanRecord, PlanVersion,
    SUMMARY_PREFIX, ToolCallOutcome, ToolResultDetail, ToolResultMetadata, TurnId,
};
pub(super) use crate::{OPENAI_RESPONSES_PROVIDER, PROVIDER_REPLAY_VERSION};
pub(super) use zevria_foundation::SKILL_TOOL_NAME;

pub(super) fn test_profile() -> crate::ModelProfileRef {
    crate::ModelProfileRef::new("test-provider", "test-model")
}

pub(super) fn local_checkpoint(label: &str) -> CompactionCheckpoint {
    CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![
            OwnedModelRequestItem::message(Message::user("retained prompt")),
            OwnedModelRequestItem::message(Message::user(format!("{SUMMARY_PREFIX}\n{label}"))),
        ],
        vec!["retained prompt".to_string()],
    )
    .expect("checkpoint")
}

pub(super) fn sample_metadata() -> Vec<ToolResultMetadata> {
    vec![ToolResultMetadata {
        diagnostic: None,
        id: "provider_call_1".to_string(),
        call_id: Some("provider_call_1".to_string()),
        tool_name: "write".to_string(),
        outcome: ToolCallOutcome::Success,
        detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
            path: "src/new.rs".into(),
            change: FileChange::Add {
                content: "fn main() {}\n".to_string(),
            },
        }])),
    }]
}

pub(super) fn compound_tool_results() -> TranscriptItem {
    TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::User {
            content: vec![UserContent::tool_result(
                rig_core::message::CallId::from_wire("provider_call_1"),
                rig_core::message::ToolName::new("write").expect("nonempty tool name"),
                vec![ToolResultContent::text("wrote 13 bytes to src/new.rs")],
            )],
        },
        metadata: sample_metadata(),
    }
}

pub(super) fn task_call(id: &str, step: &str) -> TranscriptItem {
    TranscriptItem::Message(Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire(id),
            ToolFunction::new(
                rig_core::message::ToolName::new(TASK_TOOL_NAME.to_string())
                    .expect("nonempty tool name"),
                json!({
                    "tasks": [{"step": step, "status": "in_progress"}]
                }),
            ),
        ))]),
    ))
}

pub(super) fn task_result(id: &str, outcome: ToolCallOutcome) -> TranscriptItem {
    TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::User {
            content: vec![UserContent::tool_result(
                rig_core::message::CallId::from_wire(id),
                rig_core::message::ToolName::new(TASK_TOOL_NAME).expect("nonempty tool name"),
                vec![ToolResultContent::text(if outcome.is_success() {
                    "Task list updated: 0/1 completed, 1 in progress, 0 pending."
                } else {
                    "status: error\nerror: rejected"
                })],
            )],
        },
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: id.to_string(),
            call_id: None,
            tool_name: TASK_TOOL_NAME.to_string(),
            outcome,
            detail: None,
        }],
    }
}

pub(super) fn mode_item() -> TranscriptItem {
    TranscriptItem::SessionMode(crate::SessionMode::Build)
}

pub(super) fn mode_json() -> String {
    serde_json::to_string(&mode_item()).expect("mode JSON")
}

pub(super) fn sample_items() -> Vec<TranscriptItem> {
    vec![
        mode_item(),
        TranscriptItem::Message(Message::user("hello")),
        TranscriptItem::Message(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![
                AssistantContent::Reasoning(zevria_content::reasoning::summaries(vec![
                    "pondering".to_string(),
                ])),
                AssistantContent::ToolCall(ToolCall::new(
                    CallId::from_wire("call_1"),
                    ToolFunction::new(
                        rig_core::message::ToolName::new("lookup".to_string())
                            .expect("nonempty tool name"),
                        json!({"query": "rust"}),
                    ),
                )),
                AssistantContent::text("answer"),
            ]),
        )),
        compound_tool_results(),
        TranscriptItem::Message(Message::tool_result(
            rig_core::message::CallId::from_wire("legacy_call"),
            rig_core::message::ToolName::new("legacy_tool").expect("nonempty tool name"),
            "legacy result",
        )),
        TranscriptItem::Error {
            error: "network down".to_string(),
        },
    ]
}

pub(super) fn sample_plan_handoff() -> PlanHandoff {
    let title = "Durable approval workflow";
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: title.to_string(),
        markdown: format!(
            "# {title}\n\n## Goal\nShip durable approval.\n\n## Decisions\nKeep canonical records.\n\n## Implementation\nPersist the handoff.\n\n## Validation\nRound trip the lifecycle.\n\n## Risks\nInterrupted writes."
        ),
        source_turn_id: TurnId::new(7),
    };
    PlanHandoff::new(artifact, "source-session")
}
