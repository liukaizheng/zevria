//! Ordered skill directives share the exact live and persisted sequence.
use super::*;
use crate::{DirectiveContent, DirectivePayload, DirectiveState, SessionMode};

fn directive(text: &str) -> TranscriptItem {
    TranscriptItem::Directive(
        DirectiveContent::new(DirectivePayload::SkillRevocation {
            name: "review".parse().unwrap(),
            reason: text.into(),
        })
        .unwrap(),
    )
}

fn models() -> SessionModels {
    SessionModels::new(
        zevria_model::models::ModelSelection::new(
            crate::ModelProfileRef::new("provider", "build"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            crate::ModelProfileRef::new("provider", "plan"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap()
}

fn assert_projection(conversation: &Conversation) {
    crate::SessionReplayError::validate(conversation.items()).unwrap();
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
    let text = std::fs::read_to_string(conversation.path()).unwrap();
    assert!(!text.contains("PRIVATE_APPLICATION"));
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value.get(INSTRUCTION_PREFIX_KEY).is_none());
        assert!(value.get(DIRECTIVE_KEY).is_none());
    }
}

#[test]
fn directive_append_inserts_newline_and_preserves_read_only_guards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("newline.jsonl");
    let pin = crate::SkillSnapshot::new("review".parse().unwrap(), "Review", "pin").unwrap();
    let invocation = TranscriptItem::SkillInvocation(crate::SkillInvocation::new(
        pin.name().clone(),
        "",
        crate::SkillApplication::Activate(pin.clone()),
    ));
    let original = serde_json::to_string(&invocation).unwrap();
    std::fs::write(&path, &original).unwrap();
    let item = TranscriptItem::Directive(DirectiveContent::skill(&pin));
    let record = serde_json::to_string(&item).unwrap();
    let owned = item.model_request_item().unwrap().to_owned_item().unwrap();
    assert!(serde_json::to_value(owned).is_ok());
    let mut writer = TranscriptWriter::append_to(path.clone()).unwrap();
    assert!(writer.needs_newline);
    writer.append(&item).unwrap();
    assert!(!writer.needs_newline);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{original}\n{record}\n")
    );
    assert_eq!(load(&path).unwrap(), vec![invocation, item.clone()]);
    writer
        .append(&TranscriptItem::Message(Message::user("next")))
        .unwrap();
    assert!(!writer.needs_newline);
    assert_eq!(load(&path).unwrap().len(), 3);
    let bytes = std::fs::read(&path).unwrap();
    let mut reader = TranscriptWriter::read_only(path.clone()).unwrap();
    assert!(reader.append(&item).is_err());
    assert!(reader.rewrite(&[item]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn rewrites_batches_edits_headers_and_degraded_repair_preserve_all_items() {
    let dir = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(dir.path()).unwrap());
    conversation.replace_session_models(models()).unwrap();
    conversation
        .replace_session_mode(SessionMode::Build)
        .unwrap();
    conversation
        .push_required_batch(vec![
            TranscriptItem::SkillInvocation(crate::SkillInvocation::new(
                "review".parse().unwrap(),
                "",
                crate::SkillApplication::Activate(
                    crate::SkillSnapshot::new("review".parse().unwrap(), "Review", "pin").unwrap(),
                ),
            )),
            directive("first revocation"),
            TranscriptItem::Message(Message::user("first")),
        ])
        .unwrap();
    conversation
        .push_completed_batch(vec![
            TranscriptItem::Message(Message::assistant("answer")),
            directive("second revocation"),
        ])
        .unwrap();
    assert_projection(&conversation);
    assert_eq!(conversation.prompt_position(1), Some(4));
    assert_eq!(
        load(conversation.path())
            .unwrap()
            .iter()
            .position(is_prompt_item),
        Some(2)
    );
    conversation
        .replace_session_mode(SessionMode::Plan)
        .unwrap();
    let changed = models()
        .with_selection(
            SessionMode::Build,
            zevria_model::models::ModelSelection::new(
                crate::ModelProfileRef::new("p", "changed"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        )
        .unwrap();
    conversation.replace_session_models(changed).unwrap();
    assert_projection(&conversation);
    let anchor = conversation.prompt_position(1).unwrap();
    conversation
        .replace_from_items(
            anchor,
            vec![
                directive("edited revocation"),
                TranscriptItem::Message(Message::user("edited")),
            ],
        )
        .unwrap();
    assert_eq!(conversation.prompt_position(1), Some(5));
    assert_projection(&conversation);
    // Fail a real persisted append, keeping truthful completed work in memory.
    conversation.writer.file = std::fs::File::open(conversation.path()).unwrap();
    assert!(
        conversation
            .push_completed(TranscriptItem::Message(Message::assistant(
                "completed externally"
            )))
            .is_err()
    );
    assert!(conversation.ensure_durable().unwrap());
    assert_projection(&conversation);
    conversation
        .truncate(conversation.prompt_position(1).unwrap())
        .unwrap();
    assert!(conversation.prompt_position(1).is_none());
    assert_projection(&conversation);
    // Directives cannot precede the physical or logical session header.
    let bytes = std::fs::read(conversation.path()).unwrap();
    let proposed = vec![
        directive("invalid header position"),
        TranscriptItem::SessionModels(models()),
        TranscriptItem::SessionMode(SessionMode::Build),
    ];
    assert!(crate::SessionReplayError::validate(&proposed).is_err());
    assert!(conversation.writer.rewrite(&proposed).is_err());
    assert_eq!(std::fs::read(conversation.path()).unwrap(), bytes);
    assert_projection(&conversation);
}

#[test]
fn borrowed_checkpoint_fold_matches_effective_state_through_replacements_and_revocations() {
    let skill = crate::SkillSnapshot::new("review".parse().unwrap(), "Review", "FULL_PIN").unwrap();
    let updates = vec![
        DirectiveContent::skill(&skill).payload,
        DirectivePayload::SkillRevocation {
            name: skill.name().clone(),
            reason: "disabled".into(),
        },
        DirectiveContent::skill(&skill).payload,
    ];
    let mut items = vec![TranscriptItem::SkillInvocation(
        crate::SkillInvocation::new(
            skill.name().clone(),
            "",
            crate::SkillApplication::Activate(skill),
        ),
    )];
    let mut state = DirectiveState::default();
    for (index, payload) in updates.into_iter().enumerate() {
        let update = DirectiveContent::new(payload).unwrap();
        state.apply(&update).unwrap();
        items.push(TranscriptItem::Directive(update));
        assert_eq!(
            crate::effective_directives(&items)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            state.snapshot().directives
        );
        let checkpoint = CompactionCheckpoint::new(
            crate::CompactionTrigger::Manual,
            crate::CompactionBackend::LocalSummary,
            vec![crate::OwnedModelRequestItem::message(Message::user(
                format!("summary {index}"),
            ))],
            vec![],
        )
        .unwrap();
        items.push(TranscriptItem::Compaction(checkpoint));
        let projected = model_input(&items);
        assert_eq!(
            projected[0].message_ref(),
            Some(&Message::user(format!("summary {index}")))
        );
        let actual = projected
            .into_iter()
            .filter_map(|item| match item {
                ModelRequestItem::DeveloperInstruction(d) => Some(d.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, state.snapshot().directives);
        assert_eq!(crate::replay_directives(&items).unwrap(), state);
    }
    let after = directive("after last checkpoint");
    items.push(after.clone());
    assert_eq!(
        model_input(&items).last().copied(),
        after.model_request_item()
    );
    let restored: Vec<TranscriptItem> =
        serde_json::from_slice(&serde_json::to_vec(&items).unwrap()).unwrap();
    assert_eq!(restored, items);
    assert_eq!(model_input(&restored), model_input(&items));
    assert_eq!(
        crate::replay_directives(&restored).unwrap(),
        crate::replay_directives(&items).unwrap()
    );
}

#[test]
fn abandoned_roots_are_only_empty_or_valid_models_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.jsonl");
    let header = serde_json::to_string(&TranscriptItem::SessionModels(models())).unwrap();
    for (bytes, abandoned) in [
        (String::new(), true),
        (format!("{header}\n"), true),
        (format!("{header}\n \t\n"), true),
        (
            format!(
                "{header}\n{{\"zevria_session_mode\":{{\"version\":1,\"selected\":\"build\"}}}}\n"
            ),
            false,
        ),
        (
            format!("{header}\n{{\"zevria_instruction_prefix\":{{\"text\":\"private\"}}}}\n"),
            false,
        ),
        (format!("{header}\n{{\"zevria_directive\":"), false),
        (format!("{header}\n{{\"partial\":"), false),
        (" \n".into(), false),
        ("broken".into(), false),
    ] {
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(is_abandoned_root(&path), abandoned, "{bytes}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
    for items in [
        vec![TranscriptItem::SessionMode(SessionMode::Build)],
        vec![
            TranscriptItem::SessionModels(models()),
            TranscriptItem::SessionMode(SessionMode::Plan),
        ],
    ] {
        crate::SessionReplayError::validate(&items).unwrap();
    }
}
