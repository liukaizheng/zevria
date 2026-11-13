use super::super::test_support::*;
use super::*;

#[test]
fn items_round_trip_through_a_session_file() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let sessions = sessions_dir(dir.path());
    let mut writer = TranscriptWriter::create(&sessions).expect("writer should create");

    let items = sample_items();
    for item in &items {
        writer.append(item).expect("append should succeed");
    }

    let loaded = load(writer.path()).expect("session file should load");
    assert_eq!(loaded, items);
}

#[test]
fn create_names_the_file_after_the_session_id() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let writer = TranscriptWriter::create(dir.path()).expect("writer should create");

    let stem = writer
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("session file should have a name");
    assert_eq!(stem, writer.session_id());
    Uuid::parse_str(stem).expect("the session id should be a uuid");
    assert_eq!(
        writer.path().extension().and_then(|e| e.to_str()),
        Some("jsonl")
    );
}

#[test]
fn append_to_reopens_a_session_and_keeps_its_id() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    writer.append(&mode_item()).expect("prefix");
    writer
        .append(&TranscriptItem::Message(Message::user("first")))
        .expect("append should succeed");
    let (path, session_id) = (writer.path().to_path_buf(), writer.session_id().to_string());
    drop(writer);

    let mut resumed = TranscriptWriter::append_to(path).expect("session should reopen");
    assert_eq!(resumed.session_id(), session_id);
    resumed
        .append(&TranscriptItem::Message(Message::user("second")))
        .expect("append should succeed");

    let loaded = load(resumed.path()).expect("session file should load");
    assert_eq!(
        loaded,
        vec![
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::user("second")),
        ]
    );
}

#[test]
fn created_and_reopened_handles_roll_back_partial_appends_and_keep_appending() {
    use std::io::{Seek as _, SeekFrom};

    for reopen in [false, true] {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        let mut expected = vec![mode_item(), TranscriptItem::Message(Message::user("first"))];
        for item in &expected {
            writer.append(item).expect("append should succeed");
        }
        let path = writer.path().to_path_buf();
        let original = std::fs::read(&path).expect("original bytes");
        if reopen {
            drop(writer);
            writer = TranscriptWriter::append_to(path.clone()).expect("session should reopen");
        }
        let file = writer.file.handle().expect("materialized transcript");
        let original_len = file.metadata().unwrap().len();
        let error = append_or_rollback(
            || {
                let mut file = file;
                file.seek(SeekFrom::End(0))?;
                file.write_all(b"{\"error\":\"partial")?;
                Err(std::io::Error::other("injected append failure"))
            },
            || file.set_len(original_len).and_then(|()| file.sync_data()),
        )
        .expect_err("injected append should fail");
        assert_eq!(error.to_string(), "injected append failure");
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // set_len leaves the cursor past EOF after shrinking. The next append
        // must seek back to EOF instead of leaving a gap or overwriting history.
        let next = TranscriptItem::Message(Message::user("after rollback"));
        writer.append(&next).expect("append after rollback");
        expected.push(next);
        assert!(std::fs::read(&path).unwrap().starts_with(&original));
        assert_eq!(load(&path).unwrap(), expected);
    }
}

#[test]
fn read_only_handles_cannot_truncate_transcripts() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    writer.append(&mode_item()).expect("prefix");
    let path = writer.path().to_path_buf();
    drop(writer);
    let original = std::fs::read(&path).expect("original bytes");

    let reader = TranscriptWriter::read_only(path.clone()).expect("read-only session");
    assert!(reader.file.handle().unwrap().set_len(0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn rewrite_replaces_the_file_contents_and_keeps_the_session_id() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    let session_id = writer.session_id().to_string();
    for item in [
        mode_item(),
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Message(Message::assistant("answer")),
        TranscriptItem::Message(Message::user("second")),
    ] {
        writer.append(&item).expect("append should succeed");
    }

    writer
        .rewrite(&[
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Error {
                error: "tail dropped".to_string(),
            },
        ])
        .expect("rewrite should succeed");

    assert_eq!(writer.session_id(), session_id);
    let loaded = load(writer.path()).expect("session file should load");
    assert_eq!(
        loaded,
        vec![
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Error {
                error: "tail dropped".to_string()
            },
        ]
    );
    // Appending after a rewrite still appends to the new contents.
    writer
        .append(&TranscriptItem::Message(Message::user("third")))
        .expect("append should succeed");
    let loaded = load(writer.path()).expect("session file should load");
    assert_eq!(loaded.len(), 4);
    // The staging file never outlives a successful rewrite.
    assert!(!writer.path().with_extension("jsonl.rewrite").exists());
}

#[test]
fn a_failed_rewrite_leaves_the_original_file_whole() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let sessions_dir = dir.path().join("sessions");
    let mut writer = TranscriptWriter::create(&sessions_dir).expect("writer should create");
    let original = vec![
        mode_item(),
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Message(Message::assistant("answer")),
    ];
    for item in &original {
        writer.append(item).expect("append should succeed");
    }

    // Staging succeeds, but replacing the directory at the transcript
    // filename fails without moving any directory containing open files.
    let original_path = writer.path().to_path_buf();
    let original_bytes = std::fs::read(&original_path).unwrap();
    let mut blocker =
        TranscriptRewriteBlocker::new(&original_path).expect("block transcript replacement");
    let error = writer
        .rewrite(&[mode_item(), TranscriptItem::Message(Message::user("first"))])
        .expect_err("rewrite should fail");
    assert!(
        error
            .to_string()
            .contains("failed to replace the session file"),
        "unexpected error: {error:#}"
    );

    assert_eq!(
        std::fs::read(blocker.backup_path()).unwrap(),
        original_bytes
    );
    assert_eq!(load(blocker.backup_path()).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(&sessions_dir).unwrap().count(),
        2,
        "no leaked stage"
    );
    writer
        .append(&TranscriptItem::Message(Message::user("second")))
        .expect("the untouched append handle should remain usable");
    assert_eq!(load(blocker.backup_path()).unwrap().len(), 4);
    blocker.restore().expect("restore transcript filename");
    writer
        .rewrite(&original)
        .expect("rewrite after restoration");
    writer
        .append(&TranscriptItem::Message(Message::user("after repair")))
        .expect("append after restoration");
    assert_eq!(load(&original_path).unwrap().len(), 4);
    assert_eq!(std::fs::read_dir(&sessions_dir).unwrap().count(), 1);
}

#[test]
fn failed_staging_creation_leaves_the_original_file_and_handle_untouched() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    let original = vec![
        mode_item(),
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Message(Message::assistant("answer")),
    ];
    for item in &original {
        writer.append(item).expect("append should succeed");
    }
    let original_path = writer.path().to_path_buf();
    let original_bytes = std::fs::read(&original_path).unwrap();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, "block staging creation").unwrap();
    // Only redirect the destination, not the actual transcript or its live
    // handle. Unit-test access keeps this fault out of the production API.
    writer.path = blocker.join("session.jsonl");
    let error = writer.rewrite(&original[..2]).unwrap_err();
    writer.path = original_path.clone();
    assert!(
        error
            .to_string()
            .contains("failed to create a staged session file"),
        "unexpected error: {error:#}"
    );
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(load(&original_path).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        2,
        "no leaked stage"
    );
    writer
        .append(&TranscriptItem::Message(Message::user("second")))
        .expect("the untouched append handle should remain usable");
    assert_eq!(load(&original_path).unwrap().len(), 4);
    writer
        .rewrite(&original)
        .expect("rewrite after restoring destination");
    writer
        .append(&TranscriptItem::Message(Message::user("after repair")))
        .expect("append after rewrite");
    assert_eq!(load(&original_path).unwrap().len(), 4);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}
