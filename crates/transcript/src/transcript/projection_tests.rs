use super::super::test_support::*;
use super::*;

#[test]
fn latest_task_snapshot_requires_a_correlated_successful_result() {
    let mut items = vec![
        task_call("first", "Keep the installed snapshot"),
        task_result("first", ToolCallOutcome::Success),
        task_call("failed", "Do not install the failed snapshot"),
        task_result("failed", ToolCallOutcome::Error),
        task_call("interrupted", "Do not install the interrupted snapshot"),
    ];

    let snapshot = latest_successful_task_snapshot(&items).expect("first snapshot survives");
    assert_eq!(snapshot.tasks[0].step, "Keep the installed snapshot");

    // A result with no success metadata is not authoritative.
    items.push(TranscriptItem::Message(Message::tool_result(
        rig_core::message::CallId::from_wire("interrupted"),
        rig_core::message::ToolName::new(TASK_TOOL_NAME).expect("nonempty tool name"),
        "Task list updated: 0/1 completed, 1 in progress, 0 pending.",
    )));
    let snapshot = latest_successful_task_snapshot(&items).expect("first snapshot survives");
    assert_eq!(snapshot.tasks[0].step, "Keep the installed snapshot");

    items.extend([
        task_call("second", "Install the newer snapshot"),
        task_result("second", ToolCallOutcome::Success),
    ]);
    let snapshot = latest_successful_task_snapshot(&items).expect("new snapshot installs");
    assert_eq!(snapshot.tasks[0].step, "Install the newer snapshot");
}

#[test]
fn plan_records_round_trip_and_only_handoffs_are_model_visible_and_compactable() {
    let handoff = sample_plan_handoff();
    let artifact = handoff.artifact.clone();
    let items = vec![
        mode_item(),
        TranscriptItem::Plan(PlanRecord::Started {
            id: artifact.version.id,
        }),
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::RevisionRequested {
            artifact: artifact.clone(),
        }),
        TranscriptItem::Plan(PlanRecord::Resolved {
            id: artifact.version.id,
            artifact: Some(artifact.clone()),
            resolution: crate::PlanResolution::ImplementedFresh,
        }),
        TranscriptItem::Plan(PlanRecord::Handoff {
            handoff: handoff.clone(),
        }),
    ];
    let directory = tempfile::tempdir().expect("directory");
    let mut writer =
        TranscriptWriter::create(&sessions_dir(directory.path())).expect("transcript writer");
    for item in &items {
        writer.append(item).expect("append Plan record");
    }

    let loaded = load(writer.path()).expect("load Plan records");
    assert_eq!(loaded, items);
    assert_eq!(model_history(&loaded), vec![handoff.prompt.clone()]);
    assert!(loaded.iter().all(|item| !is_prompt_item(item)));
    assert!(
        loaded[..5]
            .iter()
            .all(|item| !is_compaction_prompt_item(item))
    );
    assert!(retained_user_candidates(&loaded[..5]).is_empty());
    assert!(is_compaction_prompt_item(&loaded[5]));
    assert_eq!(
        retained_user_candidates(&loaded)
            .into_iter()
            .map(Message::user)
            .collect::<Vec<_>>(),
        vec![handoff.prompt]
    );
    let ready = serde_json::to_value(&loaded[2]).expect("serialize Ready");
    assert_eq!(
        ready[PLAN_RECORD_KEY]["artifact"]["markdown"],
        artifact.markdown
    );
    assert!(ready.get("role").is_none());

    let mut conversation = Conversation::new(writer);
    conversation.adopt_persisted(loaded);
    assert!(conversation.has_compaction_prompt());
    assert!(!conversation.has_real_user_prompt());
    assert_eq!(conversation.prompt_position(0), None);
    conversation
        .push_required(TranscriptItem::Message(Message::user(
            "first editable prompt",
        )))
        .expect("append prompt");
    assert_eq!(conversation.prompt_position(0), Some(6));
    assert_eq!(conversation.prompt_position(1), None);
}

#[test]
fn model_history_extracts_messages_without_exposing_metadata() {
    let items = vec![
        TranscriptItem::Message(Message::user("question")),
        compound_tool_results(),
        TranscriptItem::Error {
            error: "display only".to_string(),
        },
    ];

    let history = model_history(&items);
    assert_eq!(history.len(), 2);
    let serialized = serde_json::to_string(&history).expect("history should serialize");
    assert!(serialized.contains("wrote 13 bytes"));
    assert!(!serialized.contains(TOOL_RESULT_METADATA_KEY));
    assert!(!serialized.contains("fn main"));
}

#[test]
fn newest_checkpoint_replaces_only_model_history_not_the_transcript() {
    let first = local_checkpoint("first summary");
    let second = local_checkpoint("second summary");
    let items = vec![
        TranscriptItem::Message(Message::user("original prompt")),
        TranscriptItem::Message(Message::assistant("original answer")),
        TranscriptItem::Compaction(first),
        TranscriptItem::Message(Message::user("between checkpoints")),
        TranscriptItem::Compaction(second),
        TranscriptItem::Message(Message::assistant("suffix")),
    ];

    assert_eq!(items.len(), 6, "the readable transcript stays complete");
    assert_eq!(
        model_history(&items),
        vec![
            Message::user("retained prompt"),
            Message::user(format!("{SUMMARY_PREFIX}\nsecond summary")),
            Message::assistant("suffix"),
        ]
    );
}

#[test]
fn plan_handoff_retention_is_bounded_and_carried_forward_once_after_reload() {
    use crate::compaction::{local_replacement_history, select_recent_user_messages};

    for budget in [0, 64, 10_000] {
        let mut artifact = sample_plan_handoff().artifact;
        artifact
            .markdown
            .push_str(&"\nDetailed approved instruction.".repeat(300));
        artifact.markdown.push_str("\napproved ending");
        let handoff = PlanHandoff::new(artifact, "source-session");
        let directory = tempfile::tempdir().expect("directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let path = writer.path().to_path_buf();
        let mut conversation = Conversation::new(writer);
        conversation.push_required(mode_item()).expect("prefix");
        let run_id = crate::EnsembleRunId::new();
        for item in [
            TranscriptItem::Message(Message::user("old instruction")),
            TranscriptItem::Plan(PlanRecord::Handoff {
                handoff: handoff.clone(),
            }),
            TranscriptItem::Message(Message::user("new instruction")),
            TranscriptItem::Message(Message::user(format!(
                "{SUMMARY_PREFIX}\nnot an instruction"
            ))),
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: crate::EnsembleStart {
                    run_id: run_id.clone(),
                    workflow: crate::EnsembleWorkflow::Review,
                    prompt: "review".into(),
                    agents: vec![crate::AgentRunDescriptor {
                        id: crate::AgentRunId::new(),
                        agent: "worker".into(),
                        label: "Worker".into(),
                        safe_mode: "read-only".into(),
                    }],
                },
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                run_id,
                synthesis_input: Message::user("worker evidence is not a retained instruction"),
                agents: Vec::new(),
            }),
            compound_tool_results(),
        ] {
            conversation.push_required(item).expect("seed history");
        }
        let candidates = conversation.retained_user_candidates();
        assert_eq!(
            candidates.iter().map(Message::user).collect::<Vec<_>>(),
            vec![
                Message::user("old instruction"),
                handoff.prompt.clone(),
                Message::user("new instruction")
            ]
        );
        let mut retained = select_recent_user_messages(&candidates, budget);
        match budget {
            0 => assert!(retained.is_empty()),
            64 => {
                assert_eq!(retained.len(), 2);
                assert!(retained[0].starts_with("Implement the approved plan"));
                assert!(retained[0].ends_with("approved ending"));
                assert!(retained[0].contains("tokens truncated"));
                assert!(retained[0].len() < candidates[1].len());
                assert_eq!(retained[1], "new instruction");
            }
            _ => assert_eq!(retained, candidates),
        }

        for summary in ["first summary", "second summary"] {
            let replacement = local_replacement_history(&retained, summary);
            let checkpoint = CompactionCheckpoint::new(
                CompactionTrigger::Manual,
                CompactionBackend::LocalSummary,
                replacement.clone(),
                retained.clone(),
            )
            .expect("checkpoint");
            conversation
                .push_required(TranscriptItem::Compaction(checkpoint))
                .expect("persist checkpoint");
            drop(conversation);
            let loaded = load(&path).expect("reload history");
            conversation = Conversation::new(
                TranscriptWriter::append_to(path.clone()).expect("reopen writer"),
            );
            conversation.adopt_persisted(loaded);
            assert_eq!(conversation.retained_user_candidates(), retained);
            assert_eq!(
                conversation.model_input(),
                replacement
                    .iter()
                    .map(OwnedModelRequestItem::as_borrowed)
                    .collect::<Vec<_>>()
            );
            assert!(matches!(
                &conversation.items()[2],
                TranscriptItem::Plan(PlanRecord::Handoff { handoff: durable }) if durable == &handoff
            ));
            if summary == "first summary" {
                conversation
                    .push_required(TranscriptItem::Message(Message::user(
                        "follow-up instruction",
                    )))
                    .expect("append follow-up");
                retained.push("follow-up instruction".to_string());
                // A larger next budget must not resurrect the full handoff
                // or duplicate text from before the latest checkpoint.
                assert_eq!(
                    select_recent_user_messages(&conversation.retained_user_candidates(), 10_000),
                    retained
                );
            }
        }
    }
}

#[test]
fn opaque_remote_compaction_output_projects_verbatim_without_a_message() {
    let replay = ProviderReplay::openai_responses(
        test_profile(),
        vec![json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        })],
    );
    let checkpoint = CompactionCheckpoint::new(
        CompactionTrigger::AutomaticPreTurn,
        CompactionBackend::OpenaiResponsesCompact,
        vec![OwnedModelRequestItem::replay_only(replay.clone()).expect("opaque replay")],
        vec!["question".to_string()],
    )
    .expect("checkpoint");
    let items = vec![TranscriptItem::Compaction(checkpoint)];

    assert!(model_history(&items).is_empty());
    assert_eq!(
        model_input(&items),
        vec![ModelRequestItem::replay_only(&replay)]
    );
}

#[test]
fn ensemble_evidence_is_compactable_but_not_an_editable_prompt_row() {
    let item = TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
        run_id: crate::EnsembleRunId::new(),
        synthesis_input: Message::user("bounded worker evidence"),
        agents: Vec::new(),
    });

    assert!(!is_prompt_item(&item));
    assert!(is_compaction_prompt_item(&item));
    assert!(retained_user_candidates(&[item]).is_empty());
}
