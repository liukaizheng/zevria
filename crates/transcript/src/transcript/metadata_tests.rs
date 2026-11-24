use super::*;
use crate::{
    SessionMode,
    models::{ModelSelection, SessionModels},
};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};
use zevria_foundation::{ModelProfileRef, ReasoningLevel};

fn models(build: &str) -> SessionModels {
    let selection = |model| {
        ModelSelection::new(
            ModelProfileRef::new("provider", model),
            ReasoningLevel::Medium,
        )
    };
    SessionModels::new(selection(build), selection("plan")).unwrap()
}

#[test]
fn selections_append_without_changing_provider_prefix_and_survive_edit_of_first_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::allocate(dir.path()).unwrap());
    let initial = models("initial");
    let selected = models("selected");
    conversation
        .replace_session_models(initial.clone())
        .unwrap();
    conversation
        .replace_session_mode(SessionMode::Build)
        .unwrap();
    assert!(!conversation.path().exists());
    conversation
        .commit_anchored(
            None,
            vec![TranscriptItem::Message(Message::user("one"))],
            None,
        )
        .unwrap();
    let unchanged_prefix = std::fs::read(conversation.path()).unwrap();
    let identity = conversation.input_identity().unwrap().clone();
    let change = || {
        assert!(
            conversation
                .replace_session_mode(SessionMode::Plan)
                .unwrap()
        );
        conversation
            .replace_session_models(selected.clone())
            .unwrap();
        assert!(
            !conversation
                .replace_session_mode(SessionMode::Plan)
                .unwrap()
        );
        conversation
            .replace_session_models(selected.clone())
            .unwrap();
    };
    #[cfg(feature = "test-support")]
    assert_eq!(
        crate::replay_probe::measure_instruction_replay(change).1,
        Default::default()
    );
    #[cfg(not(feature = "test-support"))]
    {
        let mut change = change;
        change();
    }
    let bytes = std::fs::read(conversation.path()).unwrap();
    assert!(bytes.starts_with(&unchanged_prefix));
    assert_eq!(
        bytes[unchanged_prefix.len()..]
            .split_inclusive(|byte| *byte == b'\n')
            .count(),
        2
    );
    assert_eq!(conversation.input_identity(), Some(&identity));
    let outcome = load_report(conversation.path()).unwrap();
    assert_eq!(outcome.session_models().unwrap(), Some(&selected));
    assert_eq!(
        outcome.replay().instructions.persisted_mode(),
        Some(SessionMode::Plan)
    );

    let first_prompt = conversation.prompt_position(0).unwrap();
    conversation
        .commit_anchored(
            Some(first_prompt),
            vec![TranscriptItem::Message(Message::user("revised"))],
            None,
        )
        .unwrap();
    let edited_bytes = std::fs::read_to_string(conversation.path()).unwrap();
    assert!(
        edited_bytes
            .lines()
            .last()
            .unwrap()
            .starts_with(r#"{"type":"batch","version":2,"records":[{"type":"session_models""#)
    );
    let outcome = load_report(conversation.path()).unwrap();
    assert_eq!(outcome.items(), conversation.items());
    assert_eq!(outcome.session_models().unwrap(), Some(&selected));
    assert_eq!(
        outcome.replay().instructions.persisted_mode(),
        Some(SessionMode::Plan)
    );
}

#[test]
fn metadata_cannot_split_a_typed_prompt_and_its_boundary() {
    use zevria_foundation::{RequestBehavior, RequestMetadata};
    use zevria_instructions::RequestDirective;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let request = RequestMetadata::new(RequestBehavior::Standard);
    let prompt = TranscriptItem::RequestPrompt {
        message: Message::user("typed prompt"),
        request: request.clone(),
    };
    let boundary = TranscriptItem::RequestDirective(Box::new(RequestDirective::boundary(request)));
    let initial = TranscriptItem::SessionModels(Box::new(models("initial")));
    for metadata in [
        TranscriptItem::SessionMode(SessionMode::Plan),
        TranscriptItem::SessionModels(Box::new(models("next"))),
    ] {
        let items = [
            initial.clone(),
            prompt.clone(),
            metadata.clone(),
            boundary.clone(),
        ];
        assert!(crate::InstructionReplayState::replay(&items).is_err());
        let mut bytes = Vec::new();
        for item in items {
            serde_json::to_writer(&mut bytes, &item).unwrap();
            bytes.push(b'\n');
        }
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_report(&path).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            crate::InstructionReplayState::replay(&[prompt.clone(), boundary.clone(), metadata])
                .is_ok()
        );
    }
}

#[test]
fn metadata_cannot_split_an_open_tool_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(dir.path()).unwrap());
    let call = ToolCall::new(
        CallId::from_wire("call"),
        ToolFunction::new(ToolName::new("example").unwrap(), serde_json::json!({})),
    );
    conversation
        .commit_anchored(
            None,
            vec![TranscriptItem::Message(Message::Assistant(
                AssistantMessage::new(vec![AssistantContent::ToolCall(call)]),
            ))],
            None,
        )
        .unwrap();
    let before = std::fs::read(conversation.path()).unwrap();
    assert!(
        conversation
            .replace_session_mode(SessionMode::Plan)
            .is_err()
    );
    assert!(
        conversation
            .replace_session_models(models("changed"))
            .is_err()
    );
    assert_eq!(std::fs::read(conversation.path()).unwrap(), before);
    assert_eq!(conversation.items().len(), 1);
}
