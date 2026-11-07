use crate::*;
use rig_core::message::{AssistantContent, Message, ToolResultContent};
use serde_json::json;

fn replay() -> ProviderReplay {
    ProviderReplay::anthropic_messages(
        zevria_foundation::ModelProfileRef::new("claude", "opus"),
        vec![
            json!({"type":"text","text":"visible","citations":[]}),
            json!({"type":"thinking","thinking":"", "signature":" exact\n"}),
            json!({"type":"redacted_thinking","data":"opaque"}),
            json!({"type":"future","payload":true}),
            json!({"type":"tool_use","id":"toolu_1","name":"command","input":{"command":"pwd"}}),
        ],
        Some("a".repeat(64)),
    )
}
#[test]
fn anthropic_ledger_round_trip_and_protocol_guard() {
    let replay = replay();
    assert_eq!(replay.protocol(), Some(ReplayProtocol::Anthropic));
    assert!(
        replay
            .native_items_for(&replay.source_profile, ReplayProtocol::Responses)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        replay
            .native_items_for(&replay.source_profile, ReplayProtocol::Anthropic)
            .unwrap()
            .unwrap(),
        replay.items
    );
    assert!(replay.openai_responses_items().is_none());
    let message = replay.to_message().unwrap();
    let Message::Assistant(assistant) = &message else {
        panic!()
    };
    assert_eq!(assistant.content.len(), 4);
    let AssistantContent::ToolCall(call) = &assistant.content[3] else {
        panic!()
    };
    assert_eq!(call.id.to_string(), "toolu_1");
    assert_eq!(call.native.as_ref().unwrap().item, replay.items[4]);
    let record = OwnedModelRequestItem::replay_backed(replay.clone()).unwrap();
    let stored = serde_json::to_string(&record).unwrap();
    let restored: OwnedModelRequestItem = serde_json::from_str(&stored).unwrap();
    assert_eq!(record, restored);
    let portable = replay.portable_projection().unwrap();
    let serialized = serde_json::to_string(&portable.message).unwrap();
    for absent in ["signature", "opaque", "redacted", "future", "toolu_1"] {
        assert!(!serialized.contains(absent), "{absent}");
    }
    assert!(portable.tool_correlations["toolu_1"].starts_with("zevria_call_"));
}
#[test]
fn strict_blocks_and_binding_validation_and_opaque_safe_estimates() {
    for block in [
        json!({"type":"thinking","thinking":"no signature"}),
        json!({"type":"tool_use","id":"t","name":"command","input":[]}),
        json!({"type":"tool_use","id":"","name":"command","input":{}}),
        json!({"type":"text","text":5}),
    ] {
        let mut replay = replay();
        replay.items = vec![block];
        assert!(replay.to_message().is_err());
    }
    for binding in ["A".repeat(64), "0".repeat(63), "g".repeat(64)] {
        let mut replay = replay();
        replay.prefix_binding = Some(binding);
        assert_eq!(replay.validate(), Err("invalid_prefix_binding"));
    }
    let mut replay = replay();
    let estimate = replay.replay_only_token_estimate();
    replay.items[1]["signature"] = "s".repeat(400000).into();
    replay.items[2]["data"] = "s".repeat(400000).into();
    assert_eq!(estimate, replay.replay_only_token_estimate());
    let responses =
        ProviderReplay::openai_responses(replay.source_profile, vec![json!({"type":"future"})]);
    assert!(
        !serde_json::to_string(&responses)
            .unwrap()
            .contains("prefix_binding")
    );
}
#[test]
fn compaction_never_splits_anthropic_tool_exchange() {
    let user = Message::user("start");
    let replay = ReplayMessage::new(replay()).unwrap();
    let Message::Assistant(assistant) = replay.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[3] else {
        panic!()
    };
    let result = Message::from(call.result(vec![ToolResultContent::text("result")]));
    let items = [
        ModelRequestItem::message(&user),
        ModelRequestItem::replay_backed(&replay),
        ModelRequestItem::message(&result),
    ];
    assert_eq!(
        compaction::replay::replay_safe_prefixes(&items).unwrap(),
        vec![true, true, false, true]
    );
    assert!(compaction::replay::replay_safe_prefixes(&items[..2]).is_err());
}
