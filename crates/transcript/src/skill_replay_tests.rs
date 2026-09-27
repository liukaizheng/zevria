use crate::replay_active_skills;
use crate::skill::*;
use crate::{
    CompactionBackend, CompactionCheckpoint, CompactionTrigger, OwnedModelRequestItem,
    ToolCallOutcome, ToolResultMetadata,
    transcript::{TranscriptItem, model_history},
};
use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolCallId, ToolFunction, UserContent,
};
use serde_json::json;
use zevria_foundation::SKILL_TOOL_NAME;

fn snapshot(description: &str) -> SkillSnapshot {
    SkillSnapshot::new(
        "review".parse().unwrap(),
        description,
        "PRIVATE PINNED BODY",
    )
    .unwrap()
}

fn batch(applications: Vec<SkillApplication>) -> Vec<TranscriptItem> {
    let calls = applications
        .iter()
        .enumerate()
        .map(|(index, application)| {
            AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new_or_mint(format!("call-{index}")),
                ToolFunction::new(
                    SKILL_TOOL_NAME.into(),
                    json!({"skill": application.name(), "args": "inspect"}),
                ),
            ))
        })
        .collect();
    let message = Message::User {
        content: applications
            .iter()
            .enumerate()
            .map(|(index, _)| {
                UserContent::tool_result(
                    format!("call-{index}"),
                    SKILL_TOOL_NAME,
                    vec![rig_core::message::ToolResultContent::text(
                        "Arbitrary bodyless acknowledgement.",
                    )],
                )
            })
            .collect(),
    };
    let metadata = applications
        .iter()
        .enumerate()
        .map(|(index, _)| ToolResultMetadata {
            diagnostic: None,
            id: format!("call-{index}"),
            call_id: None,
            tool_name: SKILL_TOOL_NAME.into(),
            outcome: ToolCallOutcome::Success,
            detail: None,
        })
        .collect();
    let skill_applications = applications
        .into_iter()
        .enumerate()
        .map(|(index, application)| SkillToolApplication {
            call_id: format!("call-{index}"),
            application,
        })
        .collect();
    vec![
        TranscriptItem::Message(Message::Assistant {
            id: None,
            content: calls,
        }),
        TranscriptItem::ToolResults {
            message,
            metadata,
            skill_applications,
        },
    ]
}

#[test]
fn skill_replay_acknowledgement_wording_is_not_lifecycle_evidence() {
    let snapshot = snapshot("Original metadata");
    let items = batch(vec![
        SkillApplication::Activate(snapshot.clone()),
        SkillApplication::Reapply(snapshot.name().clone()),
    ]);
    let pins = replay_active_skills(&items).unwrap();
    assert_eq!(pins.snapshots().collect::<Vec<_>>(), [&snapshot]);
    let encoded = serde_json::to_string(&items).unwrap();
    assert!(encoded.contains("PRIVATE PINNED BODY"));
    let restored: Vec<TranscriptItem> = serde_json::from_str(&encoded).unwrap();
    assert_eq!(replay_active_skills(&restored).unwrap(), pins);
    assert!(
        !serde_json::to_string(&model_history(&restored))
            .unwrap()
            .contains("PRIVATE PINNED BODY")
    );
    let mut changed = items.clone();
    let TranscriptItem::ToolResults { message, .. } = &mut changed[1] else {
        panic!()
    };
    let Message::User { content } = message else {
        panic!()
    };
    for block in content {
        let UserContent::ToolResult(result) = block else {
            panic!()
        };
        result.content = vec![rig_core::message::ToolResultContent::text(
            "Different prose; no status parser can authorize this.",
        )];
    }
    assert_eq!(replay_active_skills(&changed).unwrap(), pins);
    let TranscriptItem::ToolResults {
        skill_applications, ..
    } = &mut changed[1]
    else {
        panic!()
    };
    skill_applications.clear();
    assert!(
        replay_active_skills(&changed).is_err(),
        "prose alone supplies no authority"
    );
}

#[test]
fn skill_replay_requires_one_to_one_correlated_accepted_applications() {
    let valid = batch(vec![SkillApplication::Activate(snapshot("Original"))]);
    for mutation in 0..10 {
        let mut items = valid.clone();
        let TranscriptItem::ToolResults {
            metadata,
            skill_applications,
            message,
        } = &mut items[1]
        else {
            panic!()
        };
        match mutation {
            0 => skill_applications.clear(),
            1 => skill_applications.push(skill_applications[0].clone()),
            2 => skill_applications[0].call_id = "forged-call".into(),
            3 => metadata[0].tool_name = "other".into(),
            4 => metadata[0].id = "unmatched-metadata".into(),
            5 => metadata[0].outcome = ToolCallOutcome::Denied,
            6 => metadata[0].outcome = ToolCallOutcome::Error,
            7 => metadata.push(metadata[0].clone()),
            8 => {
                let Message::User { content } = message else {
                    panic!()
                };
                content.push(content[0].clone());
            }
            9 => {
                let Message::User { content } = message else {
                    panic!()
                };
                let UserContent::ToolResult(result) = &mut content[0] else {
                    panic!()
                };
                result.name = "other".into();
            }
            _ => unreachable!(),
        }
        let pin_error = replay_active_skills(&items).unwrap_err();
        let combined_error = crate::InstructionReplayState::replay(&items).unwrap_err();
        assert_eq!(
            format!("{combined_error:#}"),
            format!("{pin_error:#}"),
            "mutation {mutation}"
        );
        let prefix = crate::InstructionReplayState::replay(&items[..1]).unwrap();
        let suffix_error = prefix.apply_suffix(&items[1..]).unwrap_err();
        assert_eq!(
            format!("{suffix_error:#}"),
            format!("{pin_error:#}"),
            "suffix mutation {mutation}"
        );
    }
    // Applications are reduced in result order, not sidecar or metadata order.
    let pin = snapshot("Ordered");
    let mut ordered = batch(vec![
        SkillApplication::Activate(pin.clone()),
        SkillApplication::Reapply(pin.name().clone()),
    ]);
    if let TranscriptItem::ToolResults {
        skill_applications,
        metadata,
        ..
    } = &mut ordered[1]
    {
        skill_applications.reverse();
        metadata.reverse();
    }
    assert_eq!(replay_active_skills(&ordered).unwrap().len(), 1);
    if let TranscriptItem::ToolResults {
        message: Message::User { content },
        ..
    } = &mut ordered[1]
    {
        content.reverse();
    }
    assert!(replay_active_skills(&ordered).is_err());
}

#[test]
fn skill_replay_reapplication_requires_pin_and_checkpoints_preserve_all_history() {
    let original = snapshot("Original");
    let changed = snapshot("Changed metadata");
    assert_eq!(original.body(), changed.body());
    assert_ne!(original.digest(), changed.digest());
    assert!(
        replay_active_skills(&batch(vec![SkillApplication::Reapply(
            original.name().clone()
        )]))
        .is_err()
    );
    assert!(
        replay_active_skills(&batch(vec![
            SkillApplication::Activate(original.clone()),
            SkillApplication::Activate(changed)
        ]))
        .is_err()
    );
    let mut items = batch(vec![SkillApplication::Activate(original.clone())]);
    items.push(TranscriptItem::Compaction(
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user("summary"))],
            vec![],
        )
        .unwrap(),
    ));
    assert_eq!(
        replay_active_skills(&items).unwrap().get(original.name()),
        Some(&original)
    );
    assert!(
        serde_json::to_value(items.last().unwrap()).unwrap()["zevria_compaction"]
            .get("active_skill_identities")
            .is_none()
    );
}

#[test]
fn skill_owning_records_reject_snapshot_and_invocation_tampering() {
    let snapshot = snapshot("Original");
    let direct = TranscriptItem::SkillInvocation(SkillInvocation::new(
        snapshot.name().clone(),
        "inspect",
        SkillApplication::Activate(snapshot.clone()),
    ));
    let mut value = serde_json::to_value(&direct).unwrap();
    value["zevria_skill_invocation"]["application"]["activate"]["body"] = json!("corrupt");
    assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
    let mut value = serde_json::to_value(direct).unwrap();
    value["zevria_skill_invocation"]["name"] = json!("other");
    assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
    let mut value =
        serde_json::to_value(&batch(vec![SkillApplication::Activate(snapshot)])[1]).unwrap();
    value["zevria_skill_applications"][0]["application"]["activate"]["metadata"]["description"] =
        json!("corrupt metadata");
    assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
}
