use super::super::test_support::*;
use super::*;

#[test]
fn partial_trailing_compound_record_restores_neither_half() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let path = dir.path().join("session.jsonl");
    let compound = serde_json::to_string(&compound_tool_results()).expect("compound line");
    // Interrupt the ordinary result text, before any reserved sidecar key.
    // A tail that has begun a metadata boundary is intentionally rejected.
    let cut = compound.find("wrote 13 bytes").expect("plain result text") + "wrote".len();
    let partial = &compound[..cut];
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{partial}",
            mode_json(),
            serde_json::to_string(&TranscriptItem::Message(Message::user("kept")))
                .expect("message line")
        ),
    )
    .expect("session file should write");

    assert_eq!(
        load(&path).expect("session should load"),
        vec![mode_item(), TranscriptItem::Message(Message::user("kept"))]
    );
}

#[test]
fn skill_recovery_preserves_malformed_interior_lines_for_inspection() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        "{\"error\":\"first\"}\nnot json\n\n{\"error\":\"second\"}\n{\"role\":\"user\",\"cont",
    )
    .expect("session file should write");

    let original = std::fs::read(&path).expect("original bytes");
    assert!(load(&path).is_err());
    assert!(TranscriptWriter::append_to(path.clone()).is_err());
    let error = load_report(&path).unwrap_err();
    assert_eq!(
        error.downcast_ref::<UnsupportedHistory>().unwrap().line,
        Some(2)
    );
    assert_eq!(std::fs::read(&path).expect("preserved bytes"), original);
}

#[test]
fn recognized_stale_provider_replay_is_a_hard_resume_error() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("stale-replay.jsonl");
    let stale = json!({
        (PROVIDER_REPLAY_KEY): {
            "provider": OPENAI_RESPONSES_PROVIDER,
            "version": 2,
            "source_profile": {"provider":"p", "model":"m"},
            "items": [{
                "type": "message",
                "id": "msg_old",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "old"}]
            }]
        }
    });
    std::fs::write(&path, format!("{stale}\n")).expect("write stale replay");

    let error = load(&path).expect_err("v2 replay must not be silently dropped");
    let detail = format!("{error:#}");
    assert!(detail.contains("unsupported history"));
    assert!(detail.contains("provider replay v1"));
    assert!(TranscriptWriter::append_to(path).is_err());
}

#[test]
fn current_replay_without_source_profile_is_a_hard_resume_error() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("missing-source.jsonl");
    let missing_source = json!({
        (PROVIDER_REPLAY_KEY): {
            "provider": OPENAI_RESPONSES_PROVIDER,
            "version": PROVIDER_REPLAY_VERSION,
            "items": [{
                "type": "message",
                "id": "msg_missing_source",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "old"}]
            }]
        }
    });
    std::fs::write(&path, format!("{missing_source}\n")).expect("write source-less replay");

    let error = load(&path).expect_err("source identity is required");
    let detail = format!("{error:#}");
    assert!(detail.contains("nonblank source_profile"));
    assert!(detail.contains("unsupported history"));
}

#[test]
fn reopening_recovers_only_an_incomplete_final_record() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let path = dir.path().join("recovered.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{{\"error\":\"first\"}}\n{{\"error\":\"second\"}}\n{{\"role\":\"user\",\"cont",
            mode_json()
        ),
    )
    .expect("session file should write");

    let writer = TranscriptWriter::append_to(path.clone()).expect("session should recover");
    assert_eq!(writer.recovered_malformed_lines(), 1);
    assert_eq!(
        load(&path).expect("canonical transcript should load"),
        vec![
            mode_item(),
            TranscriptItem::Error {
                error: "first".to_string(),
            },
            TranscriptItem::Error {
                error: "second".to_string(),
            },
        ]
    );
    let canonical = std::fs::read_to_string(path).expect("canonical contents");
    assert_eq!(canonical.lines().count(), 3);
    assert!(!canonical.contains("not json"));
}

#[test]
fn skill_recovery_unknown_reserved_records_and_tampering_are_never_removed() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("blocked.jsonl");
    let snapshot = SkillSnapshot::new(
        SkillName::parse("review").expect("name"),
        "Review",
        "Exact body",
    )
    .expect("snapshot");
    let invocation = TranscriptItem::SkillInvocation(SkillInvocation::new(
        SkillName::parse("review").expect("name"),
        "",
        SkillApplication::Activate(snapshot),
    ));
    let mut tampered = serde_json::to_value(&invocation).expect("value");
    tampered[SKILL_INVOCATION_KEY]["application"]["activate"]["body"] = json!("Different body");
    let mut unknown_invocation = serde_json::to_value(&invocation).expect("value");
    unknown_invocation[SKILL_INVOCATION_KEY]["version"] = json!(999);
    let mut checkpoint =
        serde_json::to_value(TranscriptItem::Compaction(local_checkpoint("summary")))
            .expect("checkpoint");
    checkpoint[COMPACTION_RECORD_KEY]["version"] = json!(999);
    for value in [
        json!({"zevria_skill_activation_v999": {}}),
        json!({"role": "user", "content": [], "zevria_skill_activation_v999": {}}),
        unknown_invocation,
        tampered,
        checkpoint,
    ] {
        let original = format!("{{\"error\":\"visible\"}}\n{value}\n{{\"partial\":");
        std::fs::write(&path, original.as_bytes()).expect("write fixture");
        let error = load_report(&path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<UnsupportedHistory>().unwrap().line,
            Some(2)
        );
        assert!(load(&path).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), original.as_bytes());
    }
}

#[test]
fn current_writes_do_not_create_upgrade_backups() {
    use crate::skill::{SkillDefinition, SkillMetadata, SkillSource};
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("mixed.jsonl");
    let original = format!(
        "{}\n{{ \"error\" : \"noncanonical formatting\" }}\n",
        mode_json()
    );
    std::fs::write(&path, original).expect("fixture");
    let snapshot = SkillDefinition::new(
        SkillName::parse("review").expect("name"),
        "Review",
        "Exact body",
        SkillSource::Programmatic("test".into()),
    )
    .expect("definition")
    .with_metadata(SkillMetadata::new("Review"), None)
    .expect("metadata")
    .snapshot();
    let checkpoint = CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user("summary"))],
        vec![],
    )
    .expect("v1 checkpoint");
    let mut items = load(&path).expect("current prefix");
    items.extend([
        TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("review").expect("name"),
            "",
            SkillApplication::Activate(snapshot),
        )),
        TranscriptItem::Compaction(checkpoint),
    ]);
    let mut writer = TranscriptWriter::append_to(path.clone()).expect("open");
    assert!(!path.with_extension("jsonl.pre-v3").exists());
    writer.rewrite(&items).expect("upgrade");
    assert!(!path.with_extension("jsonl.pre-v3").exists());
    let upgraded = std::fs::read(&path).expect("upgraded bytes");
    assert_eq!(load(&path).expect("mixed history"), items);
    TranscriptWriter::append_to(path.clone()).expect("resume mixed history");
    assert_eq!(std::fs::read(&path).expect("unchanged on open"), upgraded);
    if let TranscriptItem::Compaction(checkpoint) = items.last_mut().expect("checkpoint") {
        checkpoint.version = 2;
    }
    assert!(
        replay_active_skills(&items).is_err(),
        "a body-only checkpoint cannot assert extended state"
    );
}

#[test]
fn saved_upgrade_backups_are_not_consulted_or_modified() {
    use crate::skill::{SkillDefinition, SkillMetadata, SkillSource};
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("blocked-upgrade.jsonl");
    let original = format!("{}\n{{\"error\":\"unchanged\"}}\n", mode_json());
    std::fs::write(&path, original).expect("fixture");
    let snapshot = SkillDefinition::new(
        SkillName::parse("review").expect("name"),
        "Review",
        "Body",
        SkillSource::Programmatic("test".into()),
    )
    .expect("definition")
    .with_metadata(SkillMetadata::new("Review"), None)
    .expect("metadata")
    .snapshot();
    let item = TranscriptItem::SkillInvocation(SkillInvocation::new(
        snapshot.name().clone(),
        "",
        SkillApplication::Activate(snapshot),
    ));
    std::fs::write(path.with_extension("jsonl.pre-v3"), "conflicting backup")
        .expect("block backup");
    let mut writer = TranscriptWriter::append_to(path.clone()).expect("writer");
    writer.append(&item).unwrap();
    assert_eq!(
        std::fs::read(path.with_extension("jsonl.pre-v3")).unwrap(),
        b"conflicting backup"
    );
}

#[test]
fn unsupported_history_cannot_be_opened_even_read_only() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("blocked.jsonl");
    let original = b"{\"error\":\"inspect\"}\n{\"zevria_skill_activation_v999\":{}}\n";
    std::fs::write(&path, original).expect("fixture");
    assert!(load_report(&path).is_err());
    assert!(TranscriptWriter::read_only(path.clone()).is_err());
    assert!(TranscriptWriter::append_to(path.clone()).is_err());
    assert_eq!(std::fs::read(&path).expect("unchanged"), original);
}

#[test]
fn skill_recovery_plain_marker_text_is_not_a_reserved_record() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("plain.jsonl");
    let item = TranscriptItem::Message(Message::user(
        "zevria_skill_activation_v999 and {\"zevria_skill_invocation\":{\"version\":999}}",
    ));
    let original = format!(
        "{}\n{}",
        mode_json(),
        serde_json::to_string(&item).expect("json")
    );
    std::fs::write(&path, &original).expect("write without final newline");
    let mut writer = TranscriptWriter::append_to(path.clone()).expect("plain message");
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), original);
    writer.append(&item).expect("newline-separated append");
    assert_eq!(
        load(&path).expect("load"),
        vec![mode_item(), item.clone(), item]
    );
}

#[test]
fn invocation_and_application_tails_are_never_recoverable_debris() {
    for tail in [
        format!("{{\"{SKILL_INVOCATION_KEY}\":{{\"name\":\"review\""),
        format!("{{\"{SKILL_INVOCATION_KEY}\":{{\"version\":4,\"invocation\":"),
        format!("{{\"{SKILL_APPLICATIONS_KEY}\":["),
    ] {
        assert!(has_unsupported_reserved_tail(tail.as_bytes()));
    }
}
