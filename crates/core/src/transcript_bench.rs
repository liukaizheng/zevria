//! Synthetic, deterministic benchmark inputs. Never read private session logs.
//!
//! This module is available only with `test-support`; its surface deliberately
//! describes workloads rather than exposing private session admission methods.

use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolCallId, ToolFunction, UserContent,
};
use serde_json::json;

use zevria_content::PromptBlock;
use zevria_content::PromptImage;
use zevria_content::UserPrompt;
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOutput;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::SessionMode;
use zevria_foundation::ToolCallOutcome;
use zevria_foundation::ToolResultDetail;
use zevria_foundation::ToolResultMetadata;
use zevria_instructions::DirectiveContent;
use zevria_instructions::SkillApplication;
use zevria_instructions::SkillInvocation;
use zevria_instructions::SkillName;
use zevria_instructions::SkillSnapshot;
use zevria_instructions::SkillToolApplication;
use zevria_model::CompactionBackend;
use zevria_model::CompactionCheckpoint;
use zevria_model::CompactionTrigger;
use zevria_model::OwnedModelRequestItem;
use zevria_model::ProviderReplay;
use zevria_model::models::SessionModels;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;
use zevria_workflow::PlanId;
use zevria_workflow::PlanRecord;
use zevria_workflow::PlanResolution;

#[derive(Clone, Copy, Debug)]
pub enum Workload {
    Plain,
    Replay,
    Mixed,
    Images,
}
impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Replay => "replay",
            Self::Mixed => "mixed",
            Self::Images => "images",
        }
    }
}

/// A requested length is a minimum. Mixed histories finish their complete
/// lifecycle group, so callers must report `items.len()` and durable counts.
pub struct Fixture {
    pub items: Vec<TranscriptItem>,
    pub lines: Vec<Vec<u8>>,
    pub jsonl: Vec<u8>,
}

pub fn profile() -> ModelProfileRef {
    ModelProfileRef::new("transcript-bench", "fixed-model")
}

fn replay(index: usize, text: &str) -> TranscriptItem {
    TranscriptItem::provider_message(ProviderReplay::openai_responses(
        profile(),
        vec![
            json!({"type":"reasoning", "id":format!("reasoning-{index}"),
                "summary":[{"type":"summary_text","text":"check evidence"}],
                "encrypted_content":text, "future_reasoning":{"record":1}}),
            json!({"type":"message", "id":format!("message-{index}"), "role":"assistant",
                "status":"completed", "content":[{"type":"output_text","text":text,"annotations":[]}],
                "future_field":{"version":"preserve me"}}),
        ],
    ))
    .expect("synthetic replay")
}

fn tool_pair(index: usize, text: &str, success: bool) -> [TranscriptItem; 2] {
    let call = format!("call-{index}");
    let name = SkillName::parse(format!("tool-skill-{index}")).unwrap();
    let message = Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall::new(
            ToolCallId::new_or_mint(&call),
            ToolFunction::new("skill".into(), json!({"skill":name, "args":"benchmark"})),
        ))],
    };
    let result = Message::User {
        content: vec![UserContent::tool_result(
            &call,
            "skill",
            vec![rig_core::message::ToolResultContent::text(if success {
                "activated"
            } else {
                "rejected"
            })],
        )],
    };
    let applications = if success {
        vec![SkillToolApplication {
            call_id: call.clone(),
            application: SkillApplication::Activate(
                SkillSnapshot::new(name, "synthetic tool pin", text).unwrap(),
            ),
        }]
    } else {
        Vec::new()
    };
    [
        TranscriptItem::Message(message),
        TranscriptItem::ToolResults {
            message: result,
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: call,
                call_id: None,
                tool_name: "skill".into(),
                outcome: if success {
                    ToolCallOutcome::Success
                } else {
                    ToolCallOutcome::Error
                },
                detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
                    path: "synthetic.rs".into(),
                    change: FileChange::Add {
                        content: text.into(),
                    },
                }])),
            }],
            skill_applications: applications,
        },
    ]
}

pub fn fixture(workload: Workload, minimum_records: usize, payload_bytes: usize) -> Fixture {
    assert!((1..=1000).contains(&minimum_records));
    assert!((1..=8192).contains(&payload_bytes));
    assert!(!matches!(workload, Workload::Images) || minimum_records <= 100);
    let text = "x".repeat(payload_bytes);
    let mut items = vec![
        TranscriptItem::SessionModels(
            SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    profile(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    profile(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        ),
        TranscriptItem::SessionMode(SessionMode::Build),
    ];
    while items.len() < minimum_records {
        let index = items.len();
        match workload {
            Workload::Plain => items.push(TranscriptItem::Message(if index % 2 == 0 {
                Message::user(text.clone())
            } else {
                Message::assistant(text.clone())
            })),
            Workload::Replay => items.push(if index % 2 == 0 {
                TranscriptItem::Message(Message::user(text.clone()))
            } else {
                replay(index, &text)
            }),
            Workload::Images => {
                // Fixed valid raster, independent of text size. No large base64/RSS surprises.
                let image = PromptImage::from_rgba(8, 8, &[127; 8 * 8 * 4]).unwrap();
                let prompt = UserPrompt::new(vec![
                    PromptBlock::Text(text.clone()),
                    PromptBlock::Image(image),
                ])
                .unwrap();
                items.push(TranscriptItem::Message(prompt.to_message()));
                items.push(replay(index, &text));
            }
            Workload::Mixed => {
                items.push(TranscriptItem::Message(Message::user(text.clone())));
                items.push(replay(index, &text));
                items.extend(tool_pair(index, &text, true));
                items.extend(tool_pair(index + 1, &text, false));
                let name = SkillName::parse(format!("direct-skill-{index}")).unwrap();
                let application = SkillApplication::Activate(
                    SkillSnapshot::new(name.clone(), "synthetic direct pin", &text).unwrap(),
                );
                items.push(TranscriptItem::SkillInvocation(SkillInvocation::new(
                    name.clone(),
                    "direct args",
                    application,
                )));
                items.push(TranscriptItem::Directive(DirectiveContent::skill(
                    &SkillSnapshot::new(name.clone(), "synthetic direct pin", &text).unwrap(),
                )));
                items.push(TranscriptItem::SkillInvocation(SkillInvocation::new(
                    name.clone(),
                    "reapply",
                    SkillApplication::Reapply(name),
                )));
                let id = PlanId::from_uuid(uuid::Uuid::from_u128(index as u128));
                items.push(TranscriptItem::Plan(PlanRecord::Started { id }));
                items.push(TranscriptItem::Plan(PlanRecord::Resolved {
                    id,
                    artifact: None,
                    resolution: PlanResolution::Abandoned,
                }));
                let run_id = EnsembleRunId::from_string(format!("ensemble-{index}"));
                items.push(TranscriptItem::Ensemble(EnsembleRecord::Started {
                    start: EnsembleStart {
                        run_id: run_id.clone(),
                        workflow: EnsembleWorkflow::Plan,
                        prompt: text.clone().into(),
                        agents: vec![zevria_workflow::AgentRunDescriptor {
                            id: zevria_workflow::AgentRunId::from_string(format!("worker-{index}")),
                            agent: "synthetic".into(),
                            label: "Benchmark worker".into(),
                            safe_mode: "read-only".into(),
                        }],
                    },
                }));
                items.push(TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
                    run_id: run_id.clone(),
                    version: zevria_workflow::ENSEMBLE_REVIEW_VERSION,
                }));
                items.push(TranscriptItem::Ensemble(EnsembleRecord::Cancelled {
                    run_id,
                }));
                items.push(TranscriptItem::Compaction(
                    CompactionCheckpoint::new(
                        CompactionTrigger::Manual,
                        CompactionBackend::LocalSummary,
                        vec![OwnedModelRequestItem::message(Message::user(format!(
                            "{}\n{text}",
                            zevria_model::SUMMARY_PREFIX
                        )))],
                        vec![text.clone()],
                    )
                    .unwrap(),
                ));
                items.push(TranscriptItem::Compaction(CompactionCheckpoint::new(
                    CompactionTrigger::Manual, CompactionBackend::OpenaiResponsesCompact,
                    vec![OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(profile(),
                        vec![json!({"type":"compaction", "encrypted_content":text, "opaque_future":true})],
                    )).unwrap()], vec![],
                ).unwrap()));
                let mut attempt = zevria_content::WebSearchAttemptRecord::new(profile());
                attempt.id = format!("attempt-{index}");
                attempt.finish(zevria_content::WebSearchAttemptOutcome::Interrupted);
                items.push(TranscriptItem::WebSearchAttempt(attempt));
                items.push(TranscriptItem::Error {
                    error: "synthetic failure".into(),
                });
            }
        }
    }
    SessionReplayError::validate(&items).expect("valid synthetic lifecycle");
    let lines: Vec<_> = items
        .iter()
        .map(|item| serde_json::to_vec(item).unwrap())
        .collect();
    let mut jsonl = Vec::new();
    for line in &lines {
        jsonl.extend(line);
        jsonl.push(b'\n');
    }
    Fixture {
        items,
        lines,
        jsonl,
    }
}

pub fn owned_snapshot(items: &[TranscriptItem]) -> Vec<OwnedModelRequestItem> {
    transcript::model_input(items)
        .into_iter()
        .map(|item| item.to_owned_item().unwrap())
        .collect()
}

pub fn layout() -> Vec<(&'static str, usize)> {
    use std::mem::size_of;
    vec![
        ("TranscriptItem", size_of::<TranscriptItem>()),
        ("Message", size_of::<Message>()),
        ("ProviderReplay", size_of::<ProviderReplay>()),
        ("ReplayMessage", size_of::<zevria_model::ReplayMessage>()),
        ("MessageRecord", size_of::<zevria_model::MessageRecord>()),
        (
            "ReplayBackedMessage",
            size_of::<transcript::ReplayBackedMessage>(),
        ),
        ("OwnedModelRequestItem", size_of::<OwnedModelRequestItem>()),
        ("SkillInvocation", size_of::<SkillInvocation>()),
        ("ToolResultMetadata", size_of::<ToolResultMetadata>()),
        ("PlanRecord", size_of::<PlanRecord>()),
        ("EnsembleRecord", size_of::<EnsembleRecord>()),
        ("CompactionCheckpoint", size_of::<CompactionCheckpoint>()),
        (
            "WebSearchAttemptRecord",
            size_of::<zevria_content::WebSearchAttemptRecord>(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_matrix_round_trips_and_replays() {
        for workload in [
            Workload::Plain,
            Workload::Replay,
            Workload::Mixed,
            Workload::Images,
        ] {
            for count in [10, 100] {
                for bytes in [128, 8192] {
                    let fixture = fixture(workload, count, bytes);
                    let decoded: Vec<TranscriptItem> = fixture
                        .lines
                        .iter()
                        .map(|line| serde_json::from_slice(line).unwrap())
                        .collect();
                    assert_eq!(fixture.items, decoded);
                    SessionReplayError::validate(&decoded).unwrap();
                }
            }
        }
    }

    #[test]
    fn deterministic_model_input_baseline() {
        use sha2::{Digest, Sha256};
        for workload in [
            Workload::Plain,
            Workload::Replay,
            Workload::Mixed,
            Workload::Images,
        ] {
            let f = fixture(workload, 10, 128);
            // Workspace feature unification can enable serde_json's preserve_order.
            // Canonicalize object ordering only; pin every value, string, array
            // position, correlation and profile without normalization. These
            // hashes include owned replay/directive envelope versions, not just
            // provider request bytes. The v1 reset changes Replay/Images through
            // replay.version and Mixed through replay/directive versions plus
            // its approved snapshot digest token; Plain remains byte-identical.
            let mut value = serde_json::to_value(owned_snapshot(&f.items)).unwrap();
            value.sort_all_objects();
            let bytes = serde_json::to_vec(&value).unwrap();
            let hash: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let expected = match workload {
                Workload::Plain => {
                    "dd0c684b33633a434473c06647a4aff175548d365b5486e6d123361356b389b3"
                }
                Workload::Replay => {
                    "a4380512992966671914ea8951401be64229c1b91bac439e752f67512160b978"
                }
                Workload::Mixed => {
                    "3b4e040a982001353829d4a0b26c87e39817287237696a3ef789d13c3c21d96a"
                }
                Workload::Images => {
                    "b306880476e54297efcba7504444c8a97c4e28d6b486cdbc211c4db82db8efa5"
                }
            };
            assert_eq!(
                hash,
                expected,
                "{} model projection changed",
                workload.name()
            );
        }
    }

    #[test]
    fn persisted_corpora_match_independent_bytes() {
        // Originally captured from the archived pre-split implementation, with
        // the skill directive line added explicitly to the mixed corpus and
        // session-model headers updated to complete selections. Owned schemas
        // now use v1 with v1-domain snapshot digests. Native items, ordering,
        // correlations, image bytes and unrelated values are unchanged.
        // Never generated by the code under test. Includes headers, native replay/
        // unknown fields, skills, Plan/ensemble, both checkpoints, images, and
        // display attempts.
        // Workspace feature unification can enable serde_json/preserve_order.
        // Pin both original configurations without normalizing the actual bytes.
        let order_probe = serde_json::json!({"z": 0, "a": 1});
        let preserve_order = order_probe.as_object().unwrap().keys().next().unwrap() == "z";
        let cases: [(Workload, [&[u8]; 2]); 4] = [
            (
                Workload::Plain,
                [
                    include_bytes!("../tests/fixtures/plain.jsonl"),
                    include_bytes!("../tests/fixtures/plain.preserve-order.jsonl"),
                ],
            ),
            (
                Workload::Replay,
                [
                    include_bytes!("../tests/fixtures/replay.jsonl"),
                    include_bytes!("../tests/fixtures/replay.preserve-order.jsonl"),
                ],
            ),
            (
                Workload::Mixed,
                [
                    include_bytes!("../tests/fixtures/mixed.jsonl"),
                    include_bytes!("../tests/fixtures/mixed.preserve-order.jsonl"),
                ],
            ),
            (
                Workload::Images,
                [
                    include_bytes!("../tests/fixtures/images.jsonl"),
                    include_bytes!("../tests/fixtures/images.preserve-order.jsonl"),
                ],
            ),
        ];
        for (workload, expected) in cases {
            assert_eq!(
                String::from_utf8(fixture(workload, 10, 128).jsonl).unwrap(),
                std::str::from_utf8(expected[usize::from(preserve_order)]).unwrap(),
                "{} persisted bytes changed (preserve_order={preserve_order})",
                workload.name()
            );
        }
    }

    #[test]
    fn instruction_set_bytes_are_pinned() {
        let set = zevria_instructions::InstructionSet {
            application: String::new(),
            system: vec![],
            catalog: None,
            workflow: zevria_instructions::DirectivePolicy::new(
                "build",
                &zevria_foundation::TurnPolicy::new(
                    "benchmark policy",
                    None,
                    zevria_foundation::ModelRole::Build,
                    true,
                ),
            ),
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/instruction_set.txt");
        if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
            std::fs::write(&path, set.render()).unwrap();
        }
        assert_eq!(set.render(), std::fs::read_to_string(path).unwrap());
    }
}
