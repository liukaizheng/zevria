use super::*;
use crate::{InstructionReplayState, SessionMode};
use zevria_foundation::{RequestBehavior, RequestMetadata};

fn request(text: &str) -> Vec<TranscriptItem> {
    let request = RequestMetadata::new(RequestBehavior::Standard);
    vec![
        TranscriptItem::RequestPrompt {
            message: Message::user(text),
            request: request.clone(),
        },
        TranscriptItem::RequestDirective(zevria_instructions::RequestDirective::boundary(request)),
    ]
}

#[test]
fn validated_header_and_historical_request_facts_survive_suffix_replay() {
    let mut items = vec![TranscriptItem::SessionMode(SessionMode::Plan)];
    items.extend(request("typed owner"));
    let replay = InstructionReplayState::replay(&items).unwrap();
    assert_eq!(replay.persisted_mode(), Some(SessionMode::Plan));
    assert!(replay.has_request_boundaries());
    let replay = replay
        .apply_suffix(&[TranscriptItem::Message(Message::user("untyped owner"))])
        .unwrap();
    assert!(
        replay.has_request_boundaries(),
        "historical capability survives a cleared request contract"
    );
    let prefix = InstructionReplayState::replay(&items[..1]).unwrap();
    assert!(!prefix.has_request_boundaries());
    assert_eq!(prefix.persisted_mode(), Some(SessionMode::Plan));
    let empty = InstructionReplayState::replay(&[]).unwrap();
    assert_eq!(empty.persisted_mode(), None);
    assert!(!empty.has_request_boundaries());
}

#[cfg(feature = "test-support")]
#[test]
fn authoritative_proposal_is_built_and_validated_once_and_returns_the_installed_replay() {
    for edit in [false, true] {
        for mode in [None, Some(SessionMode::Build)] {
            let directory = tempfile::tempdir().unwrap();
            let writer = TranscriptWriter::create(directory.path()).unwrap();
            let mut conversation = Conversation::new(writer);
            conversation
                .commit_anchored(None, request("old"), mode)
                .unwrap();
            let end = conversation.leading_metadata_len();
            let (result, counts) = crate::replay_probe::measure_instruction_replay(|| {
                conversation.commit_anchored(edit.then_some(end), request("new"), mode)
            });
            let replay = result.unwrap();
            assert_eq!(counts.proposals, 1);
            assert_eq!(counts.validations, 1);
            assert_eq!(counts.full_replays, 1);
            assert_eq!(counts.full_records, conversation.items().len());
            assert_eq!(counts.header_scans, 0);
            assert_eq!(
                replay.instructions,
                InstructionReplayState::replay(conversation.items()).unwrap()
            );
            assert_eq!(replay.instructions.persisted_mode(), mode);
            assert!(replay.instructions.has_request_boundaries());
            assert_eq!(load(conversation.path()).unwrap(), conversation.items());
        }
    }
}

#[test]
fn invalid_proposals_never_write_or_poison_the_healthy_branch() {
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let mut conversation = Conversation::new(writer);
    conversation
        .commit_anchored(None, request("kept"), Some(SessionMode::Build))
        .unwrap();
    let original = conversation.items().to_vec();
    let bytes = std::fs::read(conversation.path()).unwrap();
    let missing_pin = TranscriptItem::SkillInvocation(crate::SkillInvocation::new(
        "missing".parse().unwrap(),
        "apply",
        crate::SkillApplication::Reapply("missing".parse().unwrap()),
    ));
    for records in [
        vec![TranscriptItem::SessionMode(SessionMode::Plan)],
        vec![missing_pin],
        vec![request("missing boundary").remove(0)],
    ] {
        for index in [None, Some(1)] {
            assert!(
                conversation
                    .commit_anchored(index, records.clone(), Some(SessionMode::Plan))
                    .is_err()
            );
            assert_eq!(conversation.items(), original);
            assert_eq!(std::fs::read(conversation.path()).unwrap(), bytes);
            assert!(conversation.persistence_error().is_none());
        }
    }
    conversation
        .commit_anchored(None, request("still healthy"), Some(SessionMode::Plan))
        .unwrap();
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
}

#[test]
fn required_failure_installs_neither_items_nor_replay_and_preserves_writer_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let mut conversation = Conversation::new(writer);
    conversation
        .commit_anchored(None, request("kept"), None)
        .unwrap();
    let original = conversation.items().to_vec();
    let path = conversation.path().to_path_buf();
    let bytes = std::fs::read(&path).unwrap();
    // An already-open read-only handle reliably fails an append on all platforms.
    conversation.writer.file = std::fs::File::open(&path).unwrap();
    assert!(
        conversation
            .commit_anchored(
                None,
                vec![TranscriptItem::Message(Message::user("not accepted"))],
                None
            )
            .is_err()
    );
    assert!(conversation.persistence_error().is_some());
    assert_eq!(conversation.items(), original);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    // A mode transaction does not silently repair a degraded writer.
    assert!(
        conversation
            .commit_anchored(None, request("also rejected"), Some(SessionMode::Plan))
            .is_err()
    );
    assert_eq!(conversation.items(), original);
    assert!(conversation.ensure_durable().unwrap());
    conversation
        .commit_anchored(None, request("after repair"), Some(SessionMode::Plan))
        .unwrap();
    assert_eq!(load(&path).unwrap(), conversation.items());
}

#[test]
fn borrowed_request_projection_matches_flattened_history_at_every_split() {
    let checkpoint = CompactionCheckpoint::new(
        zevria_model::compaction::CompactionTrigger::AutomaticPreTurn,
        zevria_model::compaction::CompactionBackend::LocalSummary,
        vec![zevria_model::OwnedModelRequestItem::message(Message::user(
            "summary",
        ))],
        vec![],
    )
    .unwrap();
    let mut items = request("old");
    items.push(TranscriptItem::Message(Message::assistant("answer")));
    items.push(TranscriptItem::Compaction(checkpoint));
    items.extend(request("new"));
    let expected = model_input(&items)
        .into_iter()
        .map(|item| item.to_owned_item().unwrap())
        .collect::<Vec<_>>();
    let TranscriptItem::Compaction(checkpoint) = &items[3] else {
        unreachable!()
    };
    let mut folded = model_input_with_checkpoint(items[..3].iter(), checkpoint);
    folded.extend(
        items[4..]
            .iter()
            .filter_map(TranscriptItem::model_request_item),
    );
    assert_eq!(
        folded
            .into_iter()
            .map(|item| item.to_owned_item().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    for split in 0..=items.len() {
        let projected =
            model_input_from_records(items[..split].iter().chain(items[split..].iter()))
                .into_iter()
                .map(|item| item.to_owned_item().unwrap())
                .collect::<Vec<_>>();
        assert_eq!(projected, expected);
    }
}
