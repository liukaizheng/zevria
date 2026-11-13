use super::super::test_support::*;
use super::*;

#[test]
fn skill_invocations_preview_as_their_compact_form() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    writer
        .append(&TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship it",
            SkillApplication::Activate(
                SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Commit body").unwrap(),
            ),
        )))
        .expect("append should succeed");

    let summaries = list_sessions(dir.path()).expect("directory should list");
    assert_eq!(summaries[0].preview.as_deref(), Some("$commit ship it"));
}

#[test]
fn workspace_directories_live_inside_dot_zevria() {
    let workspace = Path::new("workspace");
    let state = zevria_foundation::runtime_paths::workspace_state_root(workspace);
    assert_eq!(sessions_dir(workspace), state.join("sessions"));
    assert_eq!(plans_dir(workspace), state.join("plans"));
}

#[test]
fn subsession_helpers_isolate_child_transcripts_from_the_root_listing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = sessions_dir(workspace.path());
    let mut root = TranscriptWriter::create(&sessions).expect("root writer");
    root.append(&mode_item()).expect("root prefix");
    root.append(&TranscriptItem::Message(Message::user("root question")))
        .unwrap();
    let root_id = root.session_id().to_string();

    let children = subsessions_dir(workspace.path(), &root_id);
    assert_eq!(
        subsession_files(&children).expect("missing dir is empty"),
        Vec::new()
    );

    let first_id = pick_session_id();
    let mut first =
        TranscriptWriter::create_with_id(&children, &first_id).expect("first child transcript");
    assert_eq!(first.session_id(), first_id);
    assert!(
        TranscriptWriter::create_with_id(&children, &first_id).is_err(),
        "an existing child id must not be truncated or reopened"
    );
    first.append(&mode_item()).expect("child prefix");
    first
        .append(&TranscriptItem::Message(Message::user("child question")))
        .expect("child append");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second_id = pick_session_id();
    TranscriptWriter::create_with_id(&children, &second_id).expect("second child transcript");

    let discovered = subsession_files(&children).expect("children should list");
    assert_eq!(
        discovered
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [first_id.as_str(), second_id.as_str()]
    );
    let loaded = load(&discovered[0].1).expect("child transcript should load");
    assert_eq!(
        loaded,
        vec![
            mode_item(),
            TranscriptItem::Message(Message::user("child question"))
        ]
    );

    // Child files never surface as resumable root sessions.
    assert_eq!(
        latest_session_file(&sessions)
            .expect("root listing")
            .as_deref(),
        Some(root.path())
    );
}

#[test]
fn latest_session_file_prefers_the_most_recently_modified() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    assert_eq!(
        latest_session_file(&dir.path().join("missing")).expect("missing dir should be fine"),
        None
    );

    let mut first = TranscriptWriter::create(dir.path()).expect("first writer should create");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = TranscriptWriter::create(dir.path()).expect("second writer should create");
    assert_eq!(
        latest_session_file(dir.path())
            .expect("directory should list")
            .as_deref(),
        None
    );
    assert!(is_abandoned_root(second.path()));

    // Appending bumps the first session's mtime, making it the latest
    // again; non-session files are ignored regardless of their mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    first
        .append(&TranscriptItem::Error {
            error: "bump".to_string(),
        })
        .expect("append should succeed");
    std::fs::write(dir.path().join("notes.txt"), "not a session").expect("decoy file should write");
    assert_eq!(
        latest_session_file(dir.path())
            .expect("directory should list")
            .as_deref(),
        Some(first.path())
    );
}

#[test]
fn list_sessions_orders_newest_first_and_skips_empty_files() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    assert_eq!(
        list_sessions(&dir.path().join("missing")).expect("missing dir should be fine"),
        Vec::new()
    );

    let mut first = TranscriptWriter::create(dir.path()).expect("first writer should create");
    first
        .append(&TranscriptItem::Message(Message::user("first prompt")))
        .expect("append should succeed");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut second = TranscriptWriter::create(dir.path()).expect("second writer should create");
    second
        .append(&TranscriptItem::Message(Message::user("second prompt")))
        .expect("append should succeed");
    // A created-then-abandoned session stays empty and must not be listed.
    let abandoned = TranscriptWriter::create(dir.path()).expect("third writer should create");
    std::fs::write(dir.path().join("notes.txt"), "not a session").expect("decoy file should write");

    let summaries = list_sessions(dir.path()).expect("directory should list");
    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>(),
        [second.session_id(), first.session_id()]
    );
    assert_eq!(summaries[0].path, second.path());
    assert_eq!(summaries[0].preview.as_deref(), Some("second prompt"));
    assert_eq!(summaries[1].preview.as_deref(), Some("first prompt"));
    assert!(summaries[0].modified >= summaries[1].modified);
    assert!(
        !summaries
            .iter()
            .any(|summary| summary.id == abandoned.session_id())
    );
}

#[test]
fn ensemble_image_previews_use_actual_commands_without_raster_decoding() {
    let directory = tempfile::tempdir().unwrap();
    for workflow in [
        crate::EnsembleWorkflow::Plan,
        crate::EnsembleWorkflow::Review,
    ] {
        let path = directory.path().join("preview.jsonl");
        // Picker projection is intentionally independent of image validation;
        // opening this corrupt history still rejects at the durable boundary.
        std::fs::write(&path, serde_json::json!({(ENSEMBLE_RECORD_KEY): {"start": {"workflow": workflow, "prompt": [{"type":"image", "value":{"mime_type":"image/png", "data":"not a valid bitmap"}}]}}}).to_string()).unwrap();
        assert_eq!(
            session_preview(&path).unwrap(),
            format!("{} [image]", workflow.slash_command())
        );
    }
}

#[test]
fn session_previews_use_the_first_user_prompt_flattened_and_truncated() {
    let dir = tempfile::tempdir().expect("temp dir should create");

    // Leading engine traffic (an error, a compound tool result) is skipped
    // in favor of the first plain user prompt; only its first line shows.
    let mut with_noise = TranscriptWriter::create(dir.path()).expect("writer should create");
    with_noise
        .append(&TranscriptItem::Error {
            error: "transient".to_string(),
        })
        .expect("append should succeed");
    with_noise
        .append(&compound_tool_results())
        .expect("append should succeed");
    with_noise
        .append(&TranscriptItem::Message(Message::user(
            "multi line prompt\nsecond line",
        )))
        .expect("append should succeed");

    let long_prompt = "x".repeat(PREVIEW_MAX_CHARS + 10);
    let mut with_long_prompt = TranscriptWriter::create(dir.path()).expect("writer should create");
    with_long_prompt
        .append(&TranscriptItem::Message(Message::user(long_prompt.clone())))
        .expect("append should succeed");

    // A transcript with no user prompt in it stays listed, preview-less.
    let mut without_prompt = TranscriptWriter::create(dir.path()).expect("writer should create");
    without_prompt
        .append(&TranscriptItem::Error {
            error: "only an error".to_string(),
        })
        .expect("append should succeed");

    let summaries = list_sessions(dir.path()).expect("directory should list");
    let preview_of = |writer: &TranscriptWriter| {
        summaries
            .iter()
            .find(|summary| summary.id == writer.session_id())
            .expect("session should be listed")
            .preview
            .clone()
    };
    assert_eq!(
        preview_of(&with_noise).as_deref(),
        Some("multi line prompt")
    );
    assert_eq!(
        preview_of(&with_long_prompt),
        Some(format!("{}…", "x".repeat(PREVIEW_MAX_CHARS)))
    );
    assert_eq!(preview_of(&without_prompt), None);
}
