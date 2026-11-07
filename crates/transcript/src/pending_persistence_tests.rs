//! New-session persistence is independent of historical abandonment/discovery.
use super::*;
use crate::{
    DirectiveContent, ModelProfileRef, SessionMode, SkillApplication, SkillInvocation,
    SkillSnapshot,
};
use zevria_foundation::{ReasoningLevel, RequestBehavior, RequestMetadata};
use zevria_model::models::ModelSelection;

fn models() -> SessionModels {
    SessionModels::new(
        ModelSelection::new(ModelProfileRef::new("p", "build"), ReasoningLevel::Medium),
        ModelSelection::new(ModelProfileRef::new("p", "plan"), ReasoningLevel::Medium),
    )
    .unwrap()
}

fn pending(directory: &Path) -> Conversation {
    let writer = TranscriptWriter::allocate(directory).unwrap();
    assert!(writer.is_pending());
    assert!(!writer.is_read_only());
    let mut conversation = Conversation::new(writer);
    conversation.replace_session_models(models()).unwrap();
    conversation
        .replace_session_mode(SessionMode::Build)
        .unwrap();
    conversation
}

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
fn metadata_notices_replacements_and_shutdown_repair_stay_pending() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = pending(directory.path());
    let path = conversation.path().to_path_buf();
    for mode in [SessionMode::Plan, SessionMode::Build, SessionMode::Plan] {
        conversation.replace_session_mode(mode).unwrap();
        conversation.replace_session_models(models()).unwrap();
        conversation
            .push_required(TranscriptItem::Error {
                error: "notice".into(),
            })
            .unwrap();
        conversation
            .push_required_batch(vec![TranscriptItem::Error {
                error: "batch notice".into(),
            }])
            .unwrap();
        conversation
            .push_completed(TranscriptItem::Error {
                error: "completed notice".into(),
            })
            .unwrap();
        conversation
            .push_completed_batch(vec![TranscriptItem::Error {
                error: "completed batch notice".into(),
            }])
            .unwrap();
        conversation
            .replace_from(
                2,
                TranscriptItem::Error {
                    error: "replacement".into(),
                },
            )
            .unwrap();
        conversation.truncate(2).unwrap();
        conversation
            .commit_anchored(None, Vec::new(), Some(mode))
            .unwrap();
        assert!(!conversation.ensure_durable().unwrap());
        assert!(conversation.persistence_error().is_none());
        assert!(!path.exists());
        assert!(list_sessions(directory.path()).unwrap().is_empty());
        assert!(latest_session_file(directory.path()).unwrap().is_none());
    }
    drop(conversation);
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn first_commit_saves_current_headers_all_notices_and_ordered_directives_once() {
    for mutation in 0..6 {
        let directory = tempfile::tempdir().unwrap();
        let mut conversation = pending(directory.path());
        conversation
            .replace_session_mode(SessionMode::Plan)
            .unwrap();
        let selected = models()
            .with_selection(
                SessionMode::Build,
                ModelSelection::new(ModelProfileRef::new("p", "changed"), ReasoningLevel::Medium),
            )
            .unwrap();
        conversation
            .replace_session_models(selected.clone())
            .unwrap();
        conversation
            .push_required(TranscriptItem::Error {
                error: "startup notice".into(),
            })
            .unwrap();
        let mut expected = conversation.items().to_vec();
        let records = request("first accepted prompt");
        expected.extend(records.clone());
        match mutation {
            0 => {
                conversation.commit_anchored(None, records, None).unwrap();
            }
            1 => conversation.push_required_batch(records).unwrap(),
            2 => conversation.push_completed_batch(records).unwrap(),
            3 => conversation
                .replace_from_items(expected.len() - records.len(), records)
                .unwrap(),
            4 => {
                let message = TranscriptItem::Message(Message::user("first accepted prompt"));
                expected.truncate(expected.len() - records.len());
                expected.push(message.clone());
                conversation.push_required(message).unwrap();
            }
            5 => {
                let message = TranscriptItem::Message(Message::assistant("completed response"));
                expected.truncate(expected.len() - records.len());
                expected.push(message.clone());
                conversation.push_completed(message).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(!conversation.writer.is_pending());
        assert_eq!(conversation.items(), expected);
        assert_eq!(load(conversation.path()).unwrap(), expected);
        assert_eq!(conversation.session_models(), Some(&selected));
        assert_eq!(
            session_mode(conversation.items()).unwrap(),
            Some(SessionMode::Plan)
        );
        conversation
            .push_required(TranscriptItem::Message(Message::user("next")))
            .unwrap();
        conversation
            .replace_session_mode(SessionMode::Build)
            .unwrap();
        assert_eq!(load(conversation.path()).unwrap(), conversation.items());
        // Removing every prompt later retains the existing log, including its
        // historical mode-only discovery semantics. There is no retrospective deletion.
        conversation.truncate(0).unwrap();
        assert!(conversation.path().is_file());
        assert_eq!(list_sessions(directory.path()).unwrap().len(), 1);
    }
}

#[test]
fn direct_skill_plan_handoff_ensemble_start_and_response_evidence_materialize() {
    let pin = SkillSnapshot::new("review".parse().unwrap(), "Review", "exact skill body").unwrap();
    let invocation = TranscriptItem::SkillInvocation(SkillInvocation::new(
        pin.name().clone(),
        "review this",
        SkillApplication::Activate(pin.clone()),
    ));
    let handoff = crate::PlanHandoff::new(
        crate::PlanArtifact {
            version: crate::PlanVersion {
                id: crate::PlanId::new(),
                revision: 1,
            },
            title: "Implement".into(),
            markdown: "# Implement\n\nApproved plan".into(),
            source_turn_id: crate::TurnId::new(1),
        },
        "source-session",
    );
    let mut attempt = crate::ResponseAttempt::new(ModelProfileRef::new("p", "build"));
    attempt.hosted_activity.push(crate::HostedActivity {
        kind: crate::HostedActivityKind::WebSearch,
        output_index: 0,
        item_id: None,
        status: crate::HostedActivityStatus::Searching,
        action: None,
    });
    attempt.close(crate::ResponseAttemptOutcome::Interrupted);
    let evidence = TranscriptItem::IncompleteResponse(
        crate::IncompleteResponse::from_attempt(&attempt)
            .unwrap()
            .unwrap(),
    );
    for records in [
        vec![
            invocation,
            TranscriptItem::Directive(DirectiveContent::skill(&pin)),
        ],
        vec![TranscriptItem::Plan(PlanRecord::Handoff { handoff })],
        vec![TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: crate::EnsembleStart {
                run_id: EnsembleRunId::new(),
                workflow: crate::EnsembleWorkflow::Review,
                prompt: "review".into(),
                agents: vec![crate::AgentRunDescriptor {
                    id: crate::AgentRunId::new(),
                    agent: "worker".into(),
                    label: "Worker".into(),
                    safe_mode: "read-only".into(),
                }],
            },
        })],
        vec![evidence],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut conversation = pending(directory.path());
        assert!(records.iter().any(is_conversation_item));
        conversation.commit_anchored(None, records, None).unwrap();
        assert_eq!(load(conversation.path()).unwrap(), conversation.items());
    }
    assert!(!is_conversation_item(&TranscriptItem::Directive(
        DirectiveContent::skill(&pin)
    )));
    assert!(!is_conversation_item(&request("owner")[1]));
}

#[test]
fn invalid_pending_proposals_never_install_or_create_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = pending(directory.path());
    let original = conversation.items().to_vec();
    for records in [
        vec![TranscriptItem::SessionMode(SessionMode::Plan)],
        vec![request("missing boundary").remove(0)],
        vec![request("missing owner").remove(1)],
        vec![TranscriptItem::SkillInvocation(SkillInvocation::new(
            "missing".parse().unwrap(),
            "",
            SkillApplication::Reapply("missing".parse().unwrap()),
        ))],
    ] {
        assert!(
            conversation
                .commit_anchored(None, records.clone(), None)
                .is_err()
        );
        assert!(conversation.push_required_batch(records.clone()).is_err());
        assert!(conversation.push_completed_batch(records.clone()).is_err());
        assert!(conversation.push_completed(records[0].clone()).is_err());
        assert!(
            conversation
                .replace_from_items(original.len(), records)
                .is_err()
        );
        assert_eq!(conversation.items(), original);
        assert!(conversation.persistence_error().is_none());
        assert!(!conversation.ensure_durable().unwrap());
        assert!(!conversation.path().exists());
    }
}

#[test]
fn allocation_and_publication_collisions_never_clobber_and_required_input_can_retry() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = pending(directory.path());
    let original = conversation.items().to_vec();
    let path = conversation.path().to_path_buf();
    let other = b"another owner's history\n";
    std::fs::write(&path, other).unwrap();
    assert!(
        TranscriptWriter::allocate_with_id(directory.path(), conversation.session_id()).is_err()
    );
    assert!(
        conversation
            .commit_anchored(None, request("not accepted"), None)
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), other);
    assert_eq!(conversation.items(), original);
    assert!(conversation.writer.is_pending());
    assert!(conversation.persistence_error().is_none());
    assert!(!conversation.ensure_durable().unwrap());
    std::fs::remove_file(&path).unwrap();
    assert!(!conversation.ensure_durable().unwrap());
    assert!(!path.exists());
    conversation
        .commit_anchored(None, request("accepted on retry"), None)
        .unwrap();
    assert_eq!(load(&path).unwrap(), conversation.items());
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        1,
        "no leaked stage"
    );
}

#[test]
fn failed_initial_staging_keeps_required_input_unaccepted_but_completed_evidence_repairs() {
    for completed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        let mut conversation = pending(&sessions);
        let original = conversation.items().to_vec();
        std::fs::remove_dir(&sessions).unwrap();
        std::fs::write(&sessions, b"obstruct staging").unwrap();
        let record = TranscriptItem::Message(Message::assistant("completed externally"));
        if completed {
            assert!(conversation.push_completed(record.clone()).is_err());
            assert_eq!(conversation.items().last(), Some(&record));
            assert!(conversation.persistence_error().is_some());
        } else {
            assert!(conversation.push_required(record.clone()).is_err());
            assert_eq!(conversation.items(), original);
            assert!(conversation.persistence_error().is_none());
        }
        assert!(conversation.writer.is_pending());
        assert!(!conversation.path().exists());
        std::fs::remove_file(&sessions).unwrap();
        std::fs::create_dir(&sessions).unwrap();
        assert_eq!(conversation.ensure_durable().unwrap(), completed);
        if !completed {
            assert!(!conversation.path().exists());
            conversation.push_required(record).unwrap();
        }
        assert_eq!(load(conversation.path()).unwrap(), conversation.items());
        assert!(conversation.persistence_error().is_none());
    }
}

#[test]
fn required_paths_repair_pending_completed_evidence_before_accepting_more_work() {
    for mutation in 0..3 {
        let directory = tempfile::tempdir().unwrap();
        let mut conversation = pending(directory.path());
        let path = conversation.path().to_path_buf();
        std::fs::create_dir(&path).unwrap();
        assert!(
            conversation
                .push_completed(TranscriptItem::Message(Message::assistant(
                    "completed evidence"
                )))
                .is_err()
        );
        assert!(conversation.persistence_error().is_some());
        std::fs::remove_dir(&path).unwrap();
        let next = TranscriptItem::Message(Message::user("next accepted prompt"));
        match mutation {
            0 => conversation.push_required(next).unwrap(),
            1 => {
                conversation
                    .commit_anchored(None, vec![next], None)
                    .unwrap();
            }
            2 => conversation.push_required_batch(vec![next]).unwrap(),
            _ => unreachable!(),
        }
        assert!(conversation.persistence_error().is_none());
        assert_eq!(conversation.items().len(), 4);
        assert_eq!(load(&path).unwrap(), conversation.items());
    }
}

#[test]
fn pending_record_limits_reject_metadata_and_clean_partially_written_initial_stages() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = pending(directory.path());
    let original = conversation.items().to_vec();
    let oversized = "x".repeat(MAX_ROOT_RECORD_BYTES);
    assert!(
        conversation
            .push_completed(TranscriptItem::Error {
                error: oversized.clone()
            })
            .is_err()
    );
    assert_eq!(conversation.items(), original);
    assert!(
        conversation
            .commit_anchored(
                None,
                vec![TranscriptItem::Message(Message::user(oversized))],
                None
            )
            .is_err()
    );
    assert_eq!(conversation.items(), original);
    assert!(conversation.persistence_error().is_none());
    assert!(!conversation.ensure_durable().unwrap());
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        0,
        "failed staged write must leave no canonical file or stage"
    );
    conversation
        .push_required(TranscriptItem::Message(Message::user("valid retry")))
        .unwrap();
    assert_eq!(load(conversation.path()).unwrap(), conversation.items());
}

#[cfg(feature = "test-support")]
#[test]
fn pending_anchored_acceptance_validates_exactly_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = pending(directory.path());
    let (result, counts) = crate::replay_probe::measure_instruction_replay(|| {
        conversation.commit_anchored(None, request("first"), Some(SessionMode::Plan))
    });
    result.unwrap();
    assert_eq!(counts.proposals, 1);
    assert_eq!(counts.validations, 1);
    assert_eq!(counts.full_replays, 1);
    assert_eq!(counts.header_scans, 0);
}
