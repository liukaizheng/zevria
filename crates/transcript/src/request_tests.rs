use crate::*;
use rig_core::message::Message;
use zevria_foundation::{FileMutationScope, RequestBehavior, RequestMetadata};
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
fn scope_survives_resume_without_updating_legacy_request_bytes() {
    fn wire(input: Vec<ModelRequestItem<'_>>) -> Vec<u8> {
        let input = input
            .into_iter()
            .map(ModelRequestItem::to_owned_item)
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        serde_json::to_vec(&input).unwrap()
    }

    let directory = tempfile::tempdir().unwrap();
    let mut old = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    let legacy = request(RequestBehavior::Standard);
    let TranscriptItem::RequestDirective(boundary) = &legacy[1] else {
        unreachable!()
    };
    assert_eq!(
        boundary.render(),
        "Request directive:\nStandard request. Any earlier orchestration authorization and delegation obligation have ended. Use only this workflow's ordinary permissions; Builder delegation is not authorized."
    );
    assert!(
        !serde_json::to_string(&legacy[1])
            .unwrap()
            .contains("file_mutation_scope")
    );
    old.push_required_batch(legacy.clone()).unwrap();
    let path = old.path().to_path_buf();
    let original_records = std::fs::read(&path).unwrap();
    let original_input = wire(old.model_input());
    drop(old);

    let request = RequestMetadata::new(RequestBehavior::Standard);
    let scoped = vec![
        TranscriptItem::RequestPrompt {
            message: Message::user("edit a sibling file"),
            request: request.clone(),
        },
        TranscriptItem::RequestDirective(RequestDirective::boundary_with_file_mutation_scope(
            request,
            FileMutationScope::ProcessFilesystem,
        )),
    ];
    let mut writer = TranscriptWriter::append_to(path.clone()).unwrap();
    for item in &scoped {
        writer.append(item).unwrap();
    }
    drop(writer);
    let restored = load(&path).unwrap();
    assert_eq!(&restored[..legacy.len()], legacy);
    assert_eq!(&restored[legacy.len()..], scoped);
    assert!(std::fs::read(path).unwrap().starts_with(&original_records));
    let mut input = model_input(&restored);
    assert!(
        matches!(input.pop(), Some(ModelRequestItem::RequestInstruction(d)) if d.render().contains("Root Build file tools:"))
    );
    input.pop().unwrap();
    assert_eq!(wire(input), original_input);
}

#[test]
fn compaction_reprojects_typed_contract_not_summary_claims_or_skills() {
    let mut items = request(RequestBehavior::Orchestrate);
    let TranscriptItem::RequestPrompt { request, .. } = &items[0] else {
        unreachable!()
    };
    let request = request.clone();
    items[1] =
        TranscriptItem::RequestDirective(RequestDirective::boundary_with_file_mutation_scope(
            request.clone(),
            FileMutationScope::ProcessFilesystem,
        ));
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
        matches!(input[input.len()-2], ModelRequestItem::RequestInstruction(d) if d.file_mutation_scope == Some(FileMutationScope::ProcessFilesystem))
    );
    assert!(
        matches!(input.last(), Some(ModelRequestItem::RequestInstruction(d)) if d.kind == RequestDirectiveKind::Correction && d.file_mutation_scope.is_none())
    );
    items.extend(self::request(RequestBehavior::Standard));
    assert!(InstructionReplayState::replay(&items).is_ok());
    assert!(
        matches!(model_input(&items).last(), Some(ModelRequestItem::RequestInstruction(d)) if d.request.behavior == RequestBehavior::Standard && d.file_mutation_scope.is_none())
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
