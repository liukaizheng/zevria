use super::super::test_support::*;
use super::*;

#[test]
fn failed_completed_batch_keeps_the_whole_batch_in_memory_and_marks_degradation() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let sessions_dir = dir.path().join("sessions");
    let writer = TranscriptWriter::create(&sessions_dir).expect("writer should create");
    let original_path = writer.path().to_path_buf();
    let mut conversation = Conversation::new(writer);
    conversation.push_required(mode_item()).expect("prefix");
    let original = TranscriptItem::Message(Message::user("durable request"));
    conversation
        .push_required(original.clone())
        .expect("persist original item");

    let original_bytes = std::fs::read(&original_path).unwrap();
    conversation.writer.file =
        TranscriptFile::Writable(std::fs::File::open(&original_path).unwrap());
    let batch = vec![
        TranscriptItem::Message(Message::assistant("completed response")),
        TranscriptItem::Error {
            error: "terminal metadata".to_string(),
        },
    ];

    let error = conversation
        .push_completed_batch(batch.clone())
        .expect_err("atomic append should fail");

    assert!(error.to_string().contains("session transcript is degraded"));
    assert!(conversation.persistence_error().is_some());
    assert_eq!(
        conversation.items(),
        &[mode_item(), original, batch[0].clone(), batch[1].clone()]
    );
    assert_eq!(
        load(&original_path).expect("original durable file remains readable"),
        vec![
            mode_item(),
            TranscriptItem::Message(Message::user("durable request"))
        ]
    );
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    assert!(conversation.ensure_durable().unwrap());
    assert!(conversation.persistence_error().is_none());
    assert_eq!(load(&original_path).unwrap(), conversation.items());
}

#[test]
fn completed_work_survives_an_append_failure_and_repairs_before_new_work() {
    let dir = tempfile::tempdir().expect("temp dir should create");
    let writer = TranscriptWriter::create(dir.path()).expect("writer should create");
    let path = writer.path().to_path_buf();
    let mut conversation = Conversation::new(writer);
    conversation.push_required(mode_item()).expect("prefix");
    conversation
        .push_required(TranscriptItem::Message(Message::user("question")))
        .expect("initial prompt should persist");

    // Replace only the writer handle with a read-only descriptor. The
    // path remains replaceable, so the subsequent full rewrite can heal
    // the simulated append failure exactly as a later submission would.
    conversation.writer.file =
        TranscriptFile::Writable(std::fs::File::open(&path).expect("read-only handle"));
    let completed = TranscriptItem::Message(Message::assistant("completed answer"));
    conversation
        .push_completed(completed.clone())
        .expect_err("the completed append should fail");
    assert_eq!(
        conversation.items(),
        &[
            mode_item(),
            TranscriptItem::Message(Message::user("question")),
            completed
        ]
    );
    assert!(conversation.persistence_error().is_some());
    assert_eq!(
        load(&path).expect("old disk prefix stays readable").len(),
        2
    );

    assert!(
        conversation
            .ensure_durable()
            .expect("full-history repair should succeed")
    );
    assert!(conversation.persistence_error().is_none());
    assert_eq!(
        load(&path).expect("repaired transcript"),
        conversation.items()
    );
    conversation
        .push_required(TranscriptItem::Message(Message::user("next question")))
        .expect("new work may proceed after repair");
    assert_eq!(load(&path).expect("extended transcript").len(), 4);
}

#[test]
fn completed_append_repairs_degraded_writer_before_appending_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let path = writer.path().to_path_buf();
    let mut conversation = Conversation::new(writer);
    conversation
        .push_required(TranscriptItem::Message(Message::user("prompt")))
        .unwrap();
    conversation.writer.file = TranscriptFile::Writable(std::fs::File::open(&path).unwrap());
    assert!(
        conversation
            .push_completed(TranscriptItem::Message(Message::assistant(
                "retained after failed append"
            )))
            .is_err()
    );
    assert!(conversation.persistence_error().is_some());
    let mut attempt = crate::ResponseAttempt::new(crate::ModelProfileRef::new("p", "m"));
    attempt.hosted_activity.push(crate::HostedActivity {
        kind: crate::HostedActivityKind::WebSearch,
        output_index: 0,
        item_id: None,
        status: crate::HostedActivityStatus::Searching,
        action: None,
    });
    attempt.close(crate::ResponseAttemptOutcome::Interrupted);
    let recovery = TranscriptItem::IncompleteResponse(
        crate::IncompleteResponse::from_attempt(&attempt)
            .unwrap()
            .unwrap(),
    );
    conversation.push_completed(recovery).unwrap();
    assert!(conversation.persistence_error().is_none());
    assert_eq!(load(&path).unwrap(), conversation.items());
    assert_eq!(conversation.items().len(), 3);
}

#[test]
fn editing_before_a_checkpoint_drops_it_while_editing_after_preserves_it() {
    fn conversation() -> (tempfile::TempDir, Conversation) {
        let directory = tempfile::tempdir().expect("temp directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let mut conversation = Conversation::new(writer);
        conversation
            .push_required(TranscriptItem::Message(Message::user("before")))
            .expect("first prompt");
        conversation
            .push_required(TranscriptItem::Compaction(local_checkpoint("summary")))
            .expect("checkpoint");
        conversation
            .push_required(TranscriptItem::Message(Message::user("after")))
            .expect("second prompt");
        (directory, conversation)
    }

    let (_directory, mut before) = conversation();
    let first = before.prompt_position(0).expect("first prompt");
    before
        .replace_from(
            first,
            TranscriptItem::Message(Message::user("edited first")),
        )
        .expect("edit before checkpoint");
    assert!(before.latest_compaction().is_none());

    let (_directory, mut after) = conversation();
    let second = after.prompt_position(1).expect("second prompt");
    after
        .replace_from(
            second,
            TranscriptItem::Message(Message::user("edited second")),
        )
        .expect("edit after checkpoint");
    assert!(after.latest_compaction().is_some());
}

#[test]
fn empty_and_tool_result_only_histories_have_no_compaction_anchor() {
    let directory = tempfile::tempdir().expect("directory");
    let writer = TranscriptWriter::create(directory.path()).expect("writer");
    let mut conversation = Conversation::new(writer);
    assert!(!conversation.has_compaction_prompt());
    assert!(conversation.retained_user_candidates().is_empty());

    conversation
        .push_required(compound_tool_results())
        .expect("tool results");
    assert!(!conversation.has_compaction_prompt());
    assert!(!conversation.has_real_user_prompt());
    assert_eq!(conversation.prompt_position(0), None);
    assert!(conversation.retained_user_candidates().is_empty());
}

#[test]
fn first_direct_invocation_prompt_ordinal_anchors_its_owned_pin() {
    let directory = tempfile::tempdir().expect("directory");
    let writer = TranscriptWriter::create(directory.path()).expect("writer");
    let mut conversation = Conversation::new(writer);
    conversation
        .push_required_batch(vec![
            TranscriptItem::Message(Message::user("first prompt")),
            TranscriptItem::SkillInvocation(SkillInvocation::new(
                SkillName::parse("review").expect("name"),
                "inspect",
                SkillApplication::Activate(
                    SkillSnapshot::new(
                        "review".parse().unwrap(),
                        "Review changes",
                        "Review instructions",
                    )
                    .unwrap(),
                ),
            )),
        ])
        .expect("fixture");

    assert_eq!(conversation.prompt_position(0), Some(0));
    assert_eq!(conversation.prompt_position(1), Some(1));
    assert!(is_prompt_item(&conversation.items()[1]));
}

#[test]
fn retained_prefix_replay_deactivates_only_when_activation_is_removed() {
    let invocation = TranscriptItem::SkillInvocation(SkillInvocation::new(
        SkillName::parse("review").expect("name"),
        "inspect",
        SkillApplication::Activate(
            SkillSnapshot::new(
                "review".parse().unwrap(),
                "Review changes",
                "Review instructions",
            )
            .unwrap(),
        ),
    ));
    let items = vec![
        TranscriptItem::Message(Message::user("before")),
        invocation,
        TranscriptItem::Message(Message::assistant("after activation")),
        TranscriptItem::Message(Message::user("later prompt")),
    ];
    assert!(
        replay_active_skills(&items[..1])
            .expect("before")
            .is_empty()
    );
    assert_eq!(
        replay_active_skills(&items[..3]).expect("retained").len(),
        1
    );
    assert_eq!(replay_active_skills(&items[..4]).expect("after").len(), 1);
    // The visible invocation row anchors index 1, so replacing it retains
    // only `items[..1]` and makes the name eligible again.
    let directory = tempfile::tempdir().expect("directory");
    let writer = TranscriptWriter::create(directory.path()).expect("writer");
    let mut conversation = Conversation::new(writer);
    conversation.adopt_persisted(items);
    assert_eq!(conversation.prompt_position(1), Some(1));
    assert_eq!(conversation.prompt_position(2), Some(3));
}

#[test]
fn ensemble_start_lookup_uses_run_ids_without_changing_prompt_numbering() {
    let directory = tempfile::tempdir().expect("temp directory");
    let writer = TranscriptWriter::create(directory.path()).expect("writer");
    let mut conversation = Conversation::new(writer);
    let first_run = EnsembleRunId::from_string("first-run");
    let second_run = EnsembleRunId::from_string("second-run");
    let items = vec![
        TranscriptItem::Message(Message::user("first prompt")),
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: crate::EnsembleStart {
                run_id: first_run.clone(),
                workflow: crate::EnsembleWorkflow::Review,
                prompt: "first review".into(),
                agents: Vec::new(),
            },
        }),
        TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: first_run.clone(),
            synthesis_input: Message::user("evidence"),
            agents: Vec::new(),
        }),
        TranscriptItem::Message(Message::user("second prompt")),
        TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: crate::EnsembleStart {
                run_id: second_run.clone(),
                workflow: crate::EnsembleWorkflow::Plan,
                prompt: "second plan".into(),
                agents: Vec::new(),
            },
        }),
    ];
    conversation
        .push_required_batch(items)
        .expect("fixture persists");

    assert_eq!(conversation.ensemble_start_position(&first_run), Some(1));
    assert_eq!(conversation.ensemble_start_position(&second_run), Some(4));
    assert_eq!(
        conversation.ensemble_start_position(&EnsembleRunId::from_string("missing")),
        None
    );
    assert_eq!(conversation.prompt_position(0), Some(0));
    assert_eq!(conversation.prompt_position(1), Some(3));
    assert_eq!(conversation.prompt_position(2), None);
    assert!(is_compaction_prompt_item(&conversation.items()[2]));
    assert!(!is_prompt_item(&conversation.items()[1]));
    assert!(!is_prompt_item(&conversation.items()[4]));
}
