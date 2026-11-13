use super::*;

fn messages() -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Message(Message::assistant("second")),
    ]
}

#[test]
fn batches_append_one_line_without_changing_the_durable_prefix() {
    for required in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut conversation =
            Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
        conversation
            .push_required(TranscriptItem::Message(Message::user("prefix")))
            .unwrap();
        let prefix = std::fs::read(conversation.path()).unwrap();
        if required {
            conversation.push_required_batch(messages()).unwrap();
        } else {
            conversation.push_completed_batch(messages()).unwrap();
        }
        let bytes = std::fs::read(conversation.path()).unwrap();
        assert!(bytes.starts_with(&prefix));
        assert_eq!(
            bytes[prefix.len()..]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        let loaded = load_report(conversation.path()).unwrap();
        assert_eq!(loaded.items, conversation.items());
        assert_eq!(loaded.source_lines, [1, 2, 2]);
    }
}

#[test]
fn torn_batch_line_restores_neither_half_and_repairs_before_appending() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation
        .push_required(TranscriptItem::Message(Message::user("prefix")))
        .unwrap();
    let path = conversation.path().to_path_buf();
    let prefix = std::fs::read(&path).unwrap();
    conversation.push_required_batch(messages()).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    // Exercise every torn position after the envelope's identity is complete,
    // including complete first children and interrupted strings/delimiters.
    let start = prefix.len() + b"{\"zevria_batch\":".len();
    for end in start..bytes.len() - 2 {
        std::fs::write(&path, &bytes[..end]).unwrap();
        let loaded = load_report(&path).unwrap_or_else(|error| panic!("cut {end}: {error:#}"));
        assert_eq!(loaded.items.len(), 1, "cut {end}");
        assert_eq!(loaded.recoverable_lines, 1, "cut {end}");
    }
    let writer = TranscriptWriter::append_to(path.clone()).unwrap();
    assert_eq!(writer.recovered_malformed_lines(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), prefix);
    let mut conversation = Conversation::new(writer);
    conversation.adopt_persisted(load(&path).unwrap());
    conversation.push_required_batch(messages()).unwrap();
    assert_eq!(load(&path).unwrap(), conversation.items());
}

#[test]
fn repairing_a_torn_tail_preserves_complete_batch_envelopes_and_prefix_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation.push_required_batch(messages()).unwrap();
    let original = std::fs::read(conversation.path()).unwrap();
    let mut damaged = original.clone();
    damaged.extend_from_slice(b"{\"role\":\"user\",\"cont");
    std::fs::write(conversation.path(), damaged).unwrap();
    let loaded = load_report(conversation.path()).unwrap();
    assert_eq!(loaded.recoverable_lines, 1);
    let writer = TranscriptWriter::append_loaded(&loaded).unwrap();
    assert_eq!(std::fs::read(conversation.path()).unwrap(), original);
    let mut restored = Conversation::from_loaded(writer, loaded).unwrap();
    restored.push_required_batch(messages()).unwrap();
    assert!(
        std::fs::read(restored.path())
            .unwrap()
            .starts_with(&original)
    );
    assert_eq!(load(restored.path()).unwrap(), restored.items());
}

#[test]
fn batches_reject_unknown_versions_nested_envelopes_and_invalid_complete_children() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("batch.jsonl");
    for line in [
        r#"{"zevria_batch":{"version":2,"records":[{"error":"x"}]}}"#,
        r#"{"zevria_batch":{"version":1,"records":[{"zevria_batch":{"version":1,"records":[]}}]}}"#,
        r#"{"zevria_batch":{"version":1,"records":[{"zevria_future":{}},"#,
        r#"{"zevria_batch":{"version":2,"records":["#,
        r#"{"zevria_batch":{"version":1,"version":1,"records":[{"error":"x"}]}}"#,
    ] {
        std::fs::write(&path, line).unwrap();
        assert!(load_report(&path).is_err(), "{line}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), line);
    }
}

#[test]
fn partial_append_and_sync_failures_roll_back_or_explicitly_report_indeterminate_durability() {
    use std::cell::RefCell;
    for partial in [false, true] {
        for rollback_fails in [false, true] {
            let disk = RefCell::new(b"durable prefix\n".to_vec());
            let original_len = disk.borrow().len();
            let line = b"{\"zevria_batch\":{\"version\":1,\"records\":[{\"error\":\"one\"},{\"error\":\"two\"}]}}\n";
            let error = append_or_rollback(
                || {
                    let length = if partial { line.len() / 2 } else { line.len() };
                    disk.borrow_mut().extend_from_slice(&line[..length]);
                    Err(std::io::Error::other(if partial {
                        "write failed"
                    } else {
                        "sync failed"
                    }))
                },
                || {
                    if rollback_fails {
                        return Err(std::io::Error::other("truncate failed"));
                    }
                    disk.borrow_mut().truncate(original_len);
                    Ok(())
                },
            )
            .unwrap_err();
            if rollback_fails {
                assert!(format!("{error:#}").contains("durability is indeterminate"));
                assert!(format!("{error:#}").contains("truncate failed"));
            } else {
                assert_eq!(&*disk.borrow(), b"durable prefix\n");
                assert!(!format!("{error:#}").contains("indeterminate"));
            }
        }
    }
}

#[test]
fn conversation_projection_caches_follow_append_compaction_edit_and_reload() {
    fn check(conversation: &Conversation) {
        let input = conversation.model_input();
        assert_eq!(
            conversation.input_identity().unwrap(),
            &zevria_model::ModelInputIdentity::from_items(&input).unwrap()
        );
        assert_eq!(
            conversation.has_images(),
            input
                .iter()
                .filter_map(|item| item.message_ref())
                .any(zevria_content::prompt::message_has_images)
        );
        assert_eq!(
            conversation.has_compaction_prompt(),
            conversation.items().iter().any(is_compaction_prompt_item)
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    check(&conversation);
    let image = crate::prompt::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let prompt =
        crate::prompt::UserPrompt::new(vec![crate::prompt::PromptBlock::Image(image)]).unwrap();
    conversation
        .push_required(TranscriptItem::Message(prompt.to_message()))
        .unwrap();
    check(&conversation);
    assert!(conversation.has_images());
    let checkpoint = CompactionCheckpoint::new(
        crate::compaction::CompactionTrigger::Manual,
        crate::compaction::CompactionBackend::LocalSummary,
        vec![zevria_model::OwnedModelRequestItem::message(Message::user(
            "compacted",
        ))],
        vec![],
    )
    .unwrap();
    conversation
        .push_required(TranscriptItem::Compaction(checkpoint))
        .unwrap();
    check(&conversation);
    assert!(!conversation.has_images());
    conversation.push_completed_batch(messages()).unwrap();
    check(&conversation);
    conversation
        .replace_from_items(
            0,
            vec![TranscriptItem::Error {
                error: "edited".into(),
            }],
        )
        .unwrap();
    check(&conversation);
    assert!(!conversation.has_compaction_prompt());
    let loaded = load_report(conversation.path()).unwrap();
    let writer = TranscriptWriter::append_loaded(&loaded).unwrap();
    let conversation = Conversation::from_loaded(writer, loaded).unwrap();
    check(&conversation);
}

#[test]
fn ensemble_load_error_is_not_attributed_to_an_unrelated_invalid_skill_line() {
    let directory = tempfile::tempdir().unwrap();
    let mut writer = TranscriptWriter::create(directory.path()).unwrap();
    writer
        .append(&TranscriptItem::SkillInvocation(
            crate::skill::SkillInvocation::new(
                "commit".parse().unwrap(),
                "",
                crate::skill::SkillApplication::Reapply("commit".parse().unwrap()),
            ),
        ))
        .unwrap();
    writer
        .append(&TranscriptItem::Ensemble(EnsembleRecord::ReviewStarted {
            run_id: EnsembleRunId::new(),
            version: crate::ENSEMBLE_REVIEW_VERSION,
        }))
        .unwrap();
    let error = load_report(writer.path()).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::SessionReplayError>(),
        Some(crate::SessionReplayError::Ensemble(_))
    ));
    assert_eq!(
        error.downcast_ref::<UnsupportedHistory>().unwrap().line,
        None
    );
}

#[test]
fn required_batch_append_failure_keeps_memory_and_disk_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::new(TranscriptWriter::create(directory.path()).unwrap());
    conversation.push_required(messages().remove(0)).unwrap();
    let original = std::fs::read(conversation.path()).unwrap();
    let items = conversation.items().to_vec();
    conversation.writer.file =
        TranscriptFile::Writable(std::fs::File::open(conversation.path()).unwrap());
    assert!(conversation.push_required_batch(messages()).is_err());
    assert_eq!(conversation.items(), items);
    assert_eq!(std::fs::read(conversation.path()).unwrap(), original);
    assert!(conversation.persistence_error().is_some());
    conversation.ensure_durable().unwrap();
    conversation.push_required_batch(messages()).unwrap();
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
}
