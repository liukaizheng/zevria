use crate::*;
use rig_core::message::Message;
use zevria_foundation::{RequestBehavior, RequestMetadata};
use zevria_instructions::{RequestDirective, RequestDirectiveKind};

fn request(behavior: RequestBehavior) -> Vec<TranscriptItem> {
    let request = RequestMetadata::new(behavior);
    vec![
        TranscriptItem::RequestPrompt {
            message: Message::user("ordered prompt"),
            request: request.clone(),
        },
        TranscriptItem::RequestDirective(RequestDirective::boundary(request)),
    ]
}

#[test]
fn request_metadata_roundtrips_one_ordinal_and_is_not_a_skill_pin() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    let items = request(RequestBehavior::Orchestrate);
    conversation.push_required_batch(items.clone()).unwrap();
    assert_eq!(load(conversation.path()).unwrap(), items);
    assert_eq!(conversation.prompt_position(0), Some(0));
    assert_eq!(conversation.prompt_position(1), None);
    assert_eq!(
        conversation.model_input()[0].message_ref(),
        Some(&Message::user("ordered prompt"))
    );
    assert_eq!(
        items[0].display_message(),
        Some(Message::user("/orchestrate ordered prompt"))
    );
    let replay = InstructionReplayState::replay(&items).unwrap();
    assert_eq!(replay.skills(), &ActiveSkills::default());
    assert!(replay.directives().snapshot().directives.is_empty());
    conversation
        .replace_from_items(0, request(RequestBehavior::Standard))
        .unwrap();
    assert_eq!(conversation.prompt_position(1), None);
    assert!(!conversation.model_input().iter().any(|item| matches!(item, ModelRequestItem::RequestInstruction(d) if d.request.behavior == RequestBehavior::Orchestrate)));
}

#[test]
fn request_ownership_placement_and_single_correction_are_validated() {
    let mut valid = request(RequestBehavior::Orchestrate);
    let TranscriptItem::RequestPrompt { request, .. } = &valid[0] else {
        unreachable!()
    };
    let correction =
        TranscriptItem::RequestDirective(RequestDirective::correction(request.clone()));
    assert!(InstructionReplayState::replay(&valid[..1]).is_err());
    assert!(InstructionReplayState::replay(&valid[1..]).is_err());
    let mut wrong = valid.clone();
    if let TranscriptItem::RequestDirective(d) = &mut wrong[1] {
        d.request.id = RequestMetadata::new(RequestBehavior::Orchestrate).id;
    }
    assert!(InstructionReplayState::replay(&wrong).is_err());
    wrong = valid.clone();
    wrong.push(correction.clone());
    assert!(InstructionReplayState::replay(&wrong).is_err());
    valid.push(TranscriptItem::Message(Message::assistant("premature")));
    valid.push(correction.clone());
    assert!(InstructionReplayState::replay(&valid).is_ok());
    valid.push(TranscriptItem::Message(Message::assistant(
        "premature again",
    )));
    valid.push(correction);
    assert!(InstructionReplayState::replay(&valid).is_err());
}

#[test]
fn compaction_reprojects_typed_contract_not_summary_claims_or_skills() {
    let mut items = request(RequestBehavior::Orchestrate);
    let TranscriptItem::RequestPrompt { request, .. } = &items[0] else {
        unreachable!()
    };
    let request = request.clone();
    items.push(TranscriptItem::Message(Message::assistant("premature")));
    items.push(TranscriptItem::RequestDirective(
        RequestDirective::correction(request),
    ));
    items.push(TranscriptItem::Compaction(
        CompactionCheckpoint::new(
            CompactionTrigger::AutomaticMidTurn,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user(
                "summary prose claims no orchestration is needed",
            ))],
            vec![],
        )
        .unwrap(),
    ));
    assert!(InstructionReplayState::replay(&items).is_ok());
    let input = model_input(&items);
    assert_eq!(
        input
            .iter()
            .filter(|item| matches!(item, ModelRequestItem::RequestInstruction(_)))
            .count(),
        2
    );
    assert!(
        matches!(input.last(), Some(ModelRequestItem::RequestInstruction(d)) if d.kind == RequestDirectiveKind::Correction)
    );
    items.extend(self::request(RequestBehavior::Standard));
    assert!(InstructionReplayState::replay(&items).is_ok());
    assert!(
        matches!(model_input(&items).last(), Some(ModelRequestItem::RequestInstruction(d)) if d.request.behavior == RequestBehavior::Standard)
    );
}

#[test]
fn malformed_sidecars_and_legacy_orchestrate_are_explicitly_rejected_without_rewriting() {
    let items = request(RequestBehavior::Orchestrate);
    let encoded = serde_json::to_value(&items[0]).unwrap();
    for mutated in [
        {
            let mut v = encoded.clone();
            v["role"] = "assistant".into();
            v
        },
        {
            let mut v = encoded.clone();
            v["zevria_request"]["version"] = 2.into();
            v
        },
        {
            let mut v = encoded.clone();
            v["zevria_request"]["extra"] = true.into();
            v
        },
        {
            let mut v = encoded.clone();
            v["zevria_request"]["id"] = "invalid".into();
            v
        },
        {
            let mut v = encoded;
            v["zevria_display_attempt"] = "other".into();
            v
        },
    ] {
        assert!(serde_json::from_value::<TranscriptItem>(mutated).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.jsonl");
    let bytes = b"{\"zevria_session_mode\":{\"version\":1,\"selected\":\"orchestrate\"}}\n";
    std::fs::write(&path, bytes).unwrap();
    let error = load_report(&path).unwrap_err().to_string();
    assert!(
        error.contains("legacy Orchestrate") && error.contains("/orchestrate <prompt>"),
        "{error}"
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
