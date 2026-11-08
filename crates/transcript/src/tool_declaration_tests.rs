use crate::*;
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};

fn record(placement: ToolDeclarationPlacement, name: &str) -> TranscriptItem {
    TranscriptItem::ToolDeclarations(
        ToolDeclarationRecord::new(
            placement,
            ToolDeclarations::new(vec![ToolDeclaration {
                name: name.into(),
                description: "description".into(),
                parameters: serde_json::json!({"type":"object", "properties":{"z":{},"a":{}}}),
            }])
            .unwrap(),
        )
        .unwrap(),
    )
}
fn checkpoint() -> TranscriptItem {
    TranscriptItem::Compaction(
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user("summary"))],
            vec![],
        )
        .unwrap(),
    )
}

#[test]
fn ordered_declarations_roundtrip_and_window_projection() {
    let initial = record(ToolDeclarationPlacement::Initial, "a");
    let additional = record(ToolDeclarationPlacement::Additional, "b");
    let value = serde_json::to_value(&initial).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"zevria_tool_declarations":{"version":1,"placement":"initial","tools":[{"name":"a","description":"description","parameters":{"type":"object","properties":{"z":{},"a":{}}}}]}})
    );
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation.push_required(initial.clone()).unwrap();
    conversation
        .push_required(TranscriptItem::Message(Message::user("prompt")))
        .unwrap();
    conversation.push_required(additional.clone()).unwrap();
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
    let bytes = std::fs::read(conversation.path()).unwrap();
    conversation
        .replace_from_items(0, conversation.items().to_vec())
        .unwrap();
    assert_eq!(
        std::fs::read(conversation.path()).unwrap(),
        bytes,
        "rewrites preserve schema key order byte-for-byte"
    );
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
    assert_eq!(conversation.model_input().len(), 2);
    assert!(matches!(
        conversation.model_input()[1],
        ModelRequestItem::AdditionalTools(_)
    ));
    assert!(initial.message().is_none());
    assert!(additional.provider_replay().is_none());
    let window = tool_window(conversation.items());
    assert_eq!(window.initial.unwrap()[0].name, "a");
    assert_eq!(window.latest.len(), 2);
    conversation.push_required(checkpoint()).unwrap();
    assert!(tool_window(conversation.items()).initial.is_none());
    assert_eq!(conversation.model_input().len(), 1);
    conversation.push_required(initial).unwrap();
    assert!(InstructionReplayState::replay(conversation.items()).is_ok());
}

#[test]
fn malformed_envelopes_and_torn_declarations_are_rejected() {
    let value = serde_json::to_value(record(ToolDeclarationPlacement::Initial, "a")).unwrap();
    let envelope = &value["zevria_tool_declarations"];
    for json in [
        format!(
            "{{\"zevria_tool_declarations\":{envelope},\"role\":\"user\",\"content\":\"mixed\"}}"
        ),
        format!(
            "{{\"zevria_tool_declarations\":{envelope},\"zevria_tool_declarations\":{envelope}}}"
        ),
        value.to_string().replace("\"version\":1", "\"version\":2"),
        value
            .to_string()
            .replace("\"version\":1", "\"version\":1,\"version\":1"),
        value
            .to_string()
            .replace("\"version\":1", "\"version\":1,\"unknown\":true"),
    ] {
        assert!(
            serde_json::from_str::<TranscriptItem>(&json).is_err(),
            "{json}"
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("torn.jsonl");
    std::fs::write(
        &path,
        "{\"zevria_tool_declarations\":{\"version\":1,\"placement\":",
    )
    .unwrap();
    assert!(load(&path).is_err());
}

#[test]
fn invalid_mutated_checkpoint_cannot_serialize_or_partially_append() {
    let TranscriptItem::Compaction(mut checkpoint) = checkpoint() else {
        unreachable!()
    };
    checkpoint.replacement_history.push(
        record(ToolDeclarationPlacement::Additional, "b")
            .model_request_item()
            .unwrap()
            .to_owned_item()
            .unwrap(),
    );
    assert!(serde_json::to_value(&checkpoint).is_err());
    let invalid = TranscriptItem::Compaction(checkpoint);
    assert!(serde_json::to_value(&invalid).is_err());
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    writer
        .append(&TranscriptItem::Message(Message::user("unchanged")))
        .unwrap();
    let before = std::fs::read(writer.path()).unwrap();
    assert!(writer.append(&invalid).is_err());
    assert_eq!(std::fs::read(writer.path()).unwrap(), before);
    assert_eq!(
        load(writer.path()).unwrap(),
        vec![TranscriptItem::Message(Message::user("unchanged"))]
    );
}

#[test]
fn validates_window_sequences_pending_batches_and_checkpoint_contents() {
    assert!(
        ToolDeclarationRecord::new(
            ToolDeclarationPlacement::Initial,
            ToolDeclarations::default()
        )
        .is_ok()
    );
    assert!(
        ToolDeclarationRecord::new(
            ToolDeclarationPlacement::Additional,
            ToolDeclarations::default()
        )
        .is_err()
    );
    let initial = record(ToolDeclarationPlacement::Initial, "a");
    let additional = record(ToolDeclarationPlacement::Additional, "b");
    assert!(InstructionReplayState::replay(&[initial.clone(), initial.clone()]).is_err());
    assert!(InstructionReplayState::replay(std::slice::from_ref(&additional)).is_err());
    assert!(
        InstructionReplayState::replay(&[initial.clone(), checkpoint(), additional.clone()])
            .is_err()
    );
    let call = TranscriptItem::Message(Message::Assistant(AssistantMessage::new(vec![
        AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire("call"),
            ToolFunction::new(ToolName::new("a").unwrap(), serde_json::json!({})),
        )),
    ])));
    assert!(InstructionReplayState::replay(&[initial.clone(), call, additional.clone()]).is_err());
    assert!(
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![
                additional
                    .model_request_item()
                    .unwrap()
                    .to_owned_item()
                    .unwrap()
            ],
            vec![]
        )
        .is_err()
    );
    let request = RequestMetadata::new(RequestBehavior::Orchestrate);
    let boundary = zevria_instructions::RequestDirective::boundary(request.clone());
    let correction = zevria_instructions::RequestDirective::correction(request.clone());
    let sequence = vec![
        TranscriptItem::RequestPrompt {
            message: Message::user("prompt"),
            request,
        },
        initial,
        TranscriptItem::RequestDirective(boundary),
        TranscriptItem::Message(Message::assistant("done")),
        additional,
        TranscriptItem::RequestDirective(correction),
    ];
    InstructionReplayState::replay(&sequence).unwrap();
}
