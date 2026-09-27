use super::*;
use crate::SessionMode;

fn header(mode: SessionMode) -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::SessionModels(
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
            .unwrap(),
        ),
        TranscriptItem::SessionMode(mode),
    ]
}

#[test]
fn mode_records_are_versioned_strict_unique_and_positioned() {
    for mode in SessionMode::ALL {
        let item = TranscriptItem::SessionMode(mode);
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"zevria_session_mode": {"version": 1, "selected": mode.name()}})
        );
        assert_eq!(
            serde_json::from_value::<TranscriptItem>(value).unwrap(),
            item
        );
        let items = header(mode);
        assert_eq!(session_mode(&items).unwrap(), Some(mode));
        crate::SessionReplayError::validate(&items).unwrap();
        assert!(model_input(&items).is_empty());
        assert!(model_history(&items).is_empty());
        assert!(items.iter().all(|item| !is_prompt_item(item)));
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&items).unwrap();
        assert_eq!(load(writer.path()).unwrap(), items);
        assert!(
            !is_abandoned_root(writer.path()),
            "even a canonical Build choice must survive closing"
        );
        assert_eq!(session_preview(writer.path()), None);
        for misplaced in [
            vec![items[1].clone(), items[0].clone()],
            vec![
                items[0].clone(),
                TranscriptItem::Message(Message::user("text")),
                items[1].clone(),
            ],
            vec![items[0].clone(), items[1].clone(), items[1].clone()],
            vec![
                TranscriptItem::Message(Message::user("text")),
                items[1].clone(),
            ],
        ] {
            let original = std::fs::read(writer.path()).unwrap();
            assert!(session_mode(&misplaced).is_err());
            assert!(crate::SessionReplayError::validate(&misplaced).is_err());
            assert!(writer.rewrite(&misplaced).is_err());
            assert_eq!(std::fs::read(writer.path()).unwrap(), original);
            let mut raw = String::new();
            for item in &misplaced {
                raw.push_str(&serde_json::to_string(item).unwrap());
                raw.push('\n');
            }
            let path = directory.path().join("misplaced.jsonl");
            std::fs::write(&path, &raw).unwrap();
            assert!(load(&path).is_err());
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        }
    }
}

#[test]
fn malformed_reserved_modes_never_recover_as_missing_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.jsonl");
    let prefix = serde_json::to_string(&header(SessionMode::Build)[0]).unwrap();
    for mode in [
        r#"{"zevria_session_mode":{"version":2,"selected":"orchestrate"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"Orchestrate"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"unknown"}}"#,
        r#"{"zevria_session_mode":{"selected":"build"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"build","extra":true}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"plan"},"role":"user"}"#,
        r#"{"zevria_session_mode":null}"#,
        r#"{"zevria_session_mode":{"version":1,"version":1,"selected":"build"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"build","selected":"orchestrate"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"build"},"zevria_session_mode":{"version":1,"selected":"orchestrate"}}"#,
        r#"{"zevria_session_mode":{"version":1,"selected":"orchestrate""#,
        r#"{"zevria_session_mode":{"version":1,"selected":"orche"#,
        r#"{"zevria_session_mode":"#,
        r#"{"zevria_session_mode"#,
        r#"{"zevria_session_mode_v2":{}"#,
    ] {
        assert!(
            serde_json::from_str::<TranscriptItem>(mode).is_err(),
            "deserialization must reject {mode}"
        );
        let raw = format!("{prefix}\n{mode}");
        std::fs::write(&path, &raw).unwrap();
        assert!(load_report(&path).is_err(), "must block {mode}");
        assert!(
            TranscriptWriter::append_to(path.clone()).is_err(),
            "must not repair {mode}"
        );
        assert!(TranscriptWriter::read_only(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }
    let text = r#"ordinary text: {\"zevria_session_mode\":{\"version\":999,\"selected\":\"orchestrate\"}}"#;
    let mut items = header(SessionMode::Build);
    items.push(TranscriptItem::Message(Message::user(text)));
    std::fs::write(
        &path,
        items
            .iter()
            .map(|item| format!("{}\n", serde_json::to_string(item).unwrap()))
            .collect::<String>(),
    )
    .unwrap();
    assert_eq!(load(&path).unwrap(), items);
    assert_eq!(session_preview(&path), Some(preview_line(text)));
}

#[test]
fn mode_transactions_preserve_models_edits_numbering_and_atomicity() {
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    let mut items = header(SessionMode::Build);
    items.push(TranscriptItem::Message(Message::user("first prompt")));
    items.push(TranscriptItem::Message(Message::assistant("first answer")));
    writer.rewrite(&items).unwrap();
    let path = writer.path().to_path_buf();
    let mut conversation = Conversation::new(writer);
    conversation.adopt_persisted(items);
    assert!(
        conversation
            .replace_session_mode(SessionMode::Plan)
            .unwrap()
    );
    assert!(
        !conversation
            .replace_session_mode(SessionMode::Plan)
            .unwrap()
    );
    assert_eq!(conversation.prompt_position(0), Some(2));
    conversation
        .replace_session_models(
            SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    crate::ModelProfileRef::new("other", "build"),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    crate::ModelProfileRef::new("other", "plan"),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        session_mode(conversation.items()).unwrap(),
        Some(SessionMode::Plan)
    );
    conversation
        .replace_from(2, TranscriptItem::Message(Message::user("edited")))
        .unwrap();
    assert_eq!(
        session_mode(conversation.items()).unwrap(),
        Some(SessionMode::Plan)
    );
    let checkpoint = CompactionCheckpoint::new(
        crate::CompactionTrigger::Manual,
        crate::CompactionBackend::LocalSummary,
        vec![crate::OwnedModelRequestItem::message(Message::user(
            "summary",
        ))],
        vec![],
    )
    .unwrap();
    conversation
        .push_required(TranscriptItem::Compaction(checkpoint))
        .unwrap();
    assert_eq!(
        conversation.messages().cloned().collect::<Vec<_>>(),
        vec![Message::user("summary")]
    );
    assert_eq!(
        session_mode(&load(&path).unwrap()).unwrap(),
        Some(SessionMode::Plan)
    );
    let before = conversation.items().to_vec();
    let bytes = std::fs::read(&path).unwrap();
    let backup = path.with_extension("backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        conversation
            .commit_anchored(
                Some(3),
                vec![TranscriptItem::Message(Message::user("must not commit"))],
                Some(SessionMode::Plan)
            )
            .is_err()
    );
    assert_eq!(conversation.items(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), bytes);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    let mut readonly = Conversation::new(TranscriptWriter::read_only(path).unwrap());
    readonly.adopt_persisted(before);
    assert!(readonly.replace_session_mode(SessionMode::Plan).is_err());
}
