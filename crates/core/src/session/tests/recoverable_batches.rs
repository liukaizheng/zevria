//! Completed recovery groups share one existing JSONL batch boundary.
use super::*;

fn records() -> Vec<TranscriptItem> {
    (0..3)
        .map(|_| {
            let mut attempt = attempt();
            attempt.close(Outcome::Interrupted);
            recovery(&attempt)
        })
        .collect()
}

#[tokio::test]
async fn recovery_group_has_one_physical_append_and_one_replay_installation() {
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    engine
        .commit(TranscriptItem::Message(Message::user("durable prefix")))
        .unwrap();
    let path = engine.conversation.path().to_path_buf();
    let before = std::fs::read(&path).unwrap();
    let group = records();
    #[cfg(feature = "test-support")]
    let (result, counts) = crate::session::pipeline_probe::measure(async {
        recoverable(engine.record_completed(group.clone()))
    })
    .await;
    #[cfg(not(feature = "test-support"))]
    let result = recoverable(engine.record_completed(group.clone()));
    assert!(result.unwrap().is_none());
    #[cfg(feature = "test-support")]
    assert_eq!(counts.replay_installations, 1);
    let after = std::fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    let suffix = &after[before.len()..];
    assert_eq!(suffix.iter().filter(|byte| **byte == b'\n').count(), 1);
    let value: serde_json::Value = serde_json::from_slice(suffix).unwrap();
    assert_eq!(value["zevria_batch"]["version"], 1);
    assert_eq!(
        value["zevria_batch"]["records"].as_array().unwrap().len(),
        3
    );
    assert!(engine.conversation.items().ends_with(&group));
    assert_eq!(
        zevria_transcript::load(&path).unwrap(),
        engine.conversation.items()
    );
    assert_eq!(
        engine.conversation.model_input().len(),
        1,
        "recovery never enters model input"
    );
    assert!(
        recoverable(engine.record_completed(Vec::<TranscriptItem>::new()))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        after,
        "an empty group must not write"
    );
}

#[tokio::test]
async fn failed_recovery_group_retains_all_completed_work_and_can_repair() {
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::allocate(directory.path()).unwrap();
    let path = writer.path().to_path_buf();
    let mut engine = SessionEngine::new(
        ScriptedProvider::new([]),
        ToolServer::new().run(),
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    // A pending publication must not overwrite a competing file. This injects
    // failure without permissions, special devices or platform-specific I/O.
    let competing = b"competing file must remain untouched\n";
    std::fs::write(&path, competing).unwrap();
    let group = records();
    assert!(
        recoverable(engine.record_completed(group.clone()))
            .unwrap()
            .is_some()
    );
    assert!(engine.conversation.items().ends_with(&group));
    assert!(engine.conversation.persistence_error().is_some());
    engine.ensure_replay_valid().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), competing);
    assert!(engine.conversation.model_input().is_empty());
    std::fs::remove_file(&path).unwrap();
    assert!(engine.conversation.ensure_durable().unwrap());
    assert!(engine.conversation.persistence_error().is_none());
    assert_eq!(
        zevria_transcript::load(&path).unwrap(),
        engine.conversation.items()
    );
}
