use rig_core::message::{AssistantContent, Message, ToolResultContent};
use serde_json::json;
use zevria_foundation::ModelProfileRef;
use zevria_model::{ModelRequestItem, ProviderReplay, ReplayMessage};

#[test]
fn foreign_messages_replay_is_portable_even_when_provider_key_was_repointed() {
    let target = ModelProfileRef::new("repointed", "model");
    let replay=ReplayMessage::new(ProviderReplay::anthropic_messages(target.clone(),vec![
        json!({"type":"thinking","thinking":"secret","signature":"opaque"}),
        json!({"type":"text","text":"visible"}),
        json!({"type":"tool_use","id":"toolu_native","name":"command","input":{"command":"pwd"}}),
    ],None)).unwrap();
    let Message::Assistant(assistant) = replay.message() else {
        panic!()
    };
    let AssistantContent::ToolCall(call) = &assistant.content[2] else {
        panic!()
    };
    let result = Message::from(call.result(vec![ToolResultContent::text("done")]));
    let (items, _) = crate::replay::project(
        &[
            ModelRequestItem::replay_backed(&replay),
            ModelRequestItem::message(&result),
        ],
        &target,
    )
    .unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["content"], "visible");
    assert_eq!(items[1]["call_id"], items[2]["call_id"]);
    assert_ne!(items[1]["call_id"], "toolu_native");
    assert_eq!(items[1]["type"], "function_call");
    let json = serde_json::to_string(&items).unwrap();
    for absent in [
        "secret",
        "opaque",
        "toolu_native",
        "signature",
        "prefix_binding",
    ] {
        assert!(!json.contains(absent));
    }
}
