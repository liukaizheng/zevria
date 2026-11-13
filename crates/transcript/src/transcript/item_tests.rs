use super::super::test_support::*;
use super::*;

#[test]
fn messages_serialize_bare_and_errors_as_error_objects() {
    let message = serde_json::to_value(TranscriptItem::Message(Message::user("hi")))
        .expect("message should serialize");
    assert_eq!(message["role"], "user");

    let error = serde_json::to_value(TranscriptItem::Error {
        error: "boom".to_string(),
    })
    .expect("error should serialize");
    assert_eq!(error, json!({"error": "boom"}));
}

#[test]
fn provider_messages_serialize_as_replay_only_and_round_trip_canonically() {
    let arguments = r#"{"path": "x", "content": "y\n\"z\"", "n": 1.0}"#;
    let items = vec![
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "summary"}],
            "content": [],
            "encrypted_content": "opaque",
            "status": null,
            "future_reasoning_field": {"kept": true}
        }),
        json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "write",
            "arguments": arguments,
            "status": "completed",
            "future_call_field": [1, null, "x"]
        }),
        json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [
                {"type": "output_text", "text": "one"},
                {"type": "output_text", "text": "two", "future_text_field": 3}
            ],
            "future_message_field": {"unknown": "survives"}
        }),
    ];
    let replay = ProviderReplay::openai_responses(test_profile(), items.clone());
    let canonical_message = replay.to_message().expect("replay should convert");
    let item = TranscriptItem::provider_message(replay.clone())
        .expect("provider message should derive from replay");

    let line = serde_json::to_string(&item).expect("provider message should serialize");
    let raw: serde_json::Value = serde_json::from_str(&line).expect("line should be JSON");
    assert_eq!(
        raw.as_object().expect("record should be an object").len(),
        1
    );
    assert_eq!(raw, json!({"zevria_provider_replay": replay}));
    assert!(raw.get("role").is_none());
    assert_eq!(raw[PROVIDER_REPLAY_KEY]["provider"], "openai.responses");
    assert_eq!(raw[PROVIDER_REPLAY_KEY]["version"], 1);
    assert_eq!(
        raw[PROVIDER_REPLAY_KEY]["source_profile"],
        json!({"provider": "test-provider", "model": "test-model"})
    );
    assert_eq!(raw[PROVIDER_REPLAY_KEY]["items"], json!(items));

    let restored: TranscriptItem =
        serde_json::from_str(&line).expect("provider message should deserialize");
    assert_eq!(restored, item);
    let replay = restored.provider_replay().expect("replay should survive");
    assert_eq!(replay.items[1]["arguments"], arguments);
    assert_eq!(
        replay.items[2]["future_message_field"]["unknown"],
        "survives"
    );
    assert_eq!(restored.message(), Some(&canonical_message));
}

#[test]
fn mixed_malformed_unsupported_and_contentless_provider_records_are_rejected() {
    let message = Message::assistant("kept answer");
    let valid_replay = json!({
        "provider": "openai.responses",
        "version": 1,
        "items": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "answer"}]
        }]
    });

    let mut mixed = serde_json::to_value(&message).expect("message should serialize");
    mixed
        .as_object_mut()
        .expect("message should be an object")
        .insert(PROVIDER_REPLAY_KEY.to_string(), valid_replay.clone());
    assert!(serde_json::from_value::<TranscriptItem>(mixed).is_err());

    for envelope in [
        json!("not an envelope"),
        json!({"provider": "future.provider", "version": 1, "items": [{"type": "x"}]}),
        json!({"provider": "openai.responses", "version": 99, "items": [{"type": "x"}]}),
        json!({"provider": "openai.responses", "version": 1, "items": "not an array"}),
        json!({"provider": "openai.responses", "version": 1, "items": []}),
        json!({"provider": "openai.responses", "version": 1, "items": [{
            "type": "message",
            "id": "msg_empty",
            "role": "assistant",
            "status": "completed",
            "content": []
        }]}),
        json!({"provider": "openai.responses", "version": 1, "items": [{
            "type": "future_output_type",
            "payload": true
        }]}),
    ] {
        let line = json!({"zevria_provider_replay": envelope});
        assert!(
            serde_json::from_value::<TranscriptItem>(line).is_err(),
            "invalid provider record should be rejected"
        );
    }

    let ordinary: TranscriptItem = serde_json::from_value(serde_json::to_value(&message).unwrap())
        .expect("an ordinary assistant line should still load");
    assert_eq!(ordinary, TranscriptItem::Message(message));
}

#[test]
fn compound_tool_results_are_atomic_and_old_readers_see_a_bare_message() {
    let item = compound_tool_results();
    let line = serde_json::to_string(&item).expect("compound item should serialize");
    assert_eq!(line.lines().count(), 1);
    assert!(line.contains(TOOL_RESULT_METADATA_KEY));
    assert!(
        !line.contains(SUBTASK_RESULTS_KEY),
        "an empty completion list must not add the reserved key"
    );

    let restored: TranscriptItem =
        serde_json::from_str(&line).expect("compound item should deserialize");
    assert_eq!(restored, item);

    let old_reader_message: Message =
        serde_json::from_str(&line).expect("Rig should ignore the reserved top-level key");
    let old_reader_json = serde_json::to_string(&old_reader_message).expect("message");
    assert!(old_reader_json.contains("wrote 13 bytes"));
    assert!(!old_reader_json.contains(TOOL_RESULT_METADATA_KEY));
    assert!(!old_reader_json.contains("metadata-only content"));
}

#[test]
fn large_file_change_metadata_round_trips_exactly_in_one_record() {
    let added = "complete added line\n".repeat(512 * 1024 / 20 + 1);
    let deleted = "complete deleted line\n".repeat(512 * 1024 / 22 + 1);
    let context = (0..30_000)
        .map(|index| format!(" context line {index:05}\n"))
        .collect::<String>();
    let unified_diff = format!(
        "--- original\n+++ modified\n@@ -1,30001 +1,30001 @@\n{context}-old value\n+new value\n"
    );
    assert!(added.len() > 512 * 1024);
    assert!(deleted.len() > 512 * 1024);
    assert!(unified_diff.len() > 512 * 1024);

    let item = TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::User {
            content: vec![UserContent::tool_result(
                rig_core::message::CallId::from_wire("large-files"),
                rig_core::message::ToolName::new("write").expect("nonempty tool name"),
                vec![ToolResultContent::text("plain result remains available")],
            )],
        },
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: "large-files".to_string(),
            call_id: None,
            tool_name: "write".to_string(),
            outcome: ToolCallOutcome::Success,
            detail: Some(ToolResultDetail::FileChanges(vec![
                FileChangeOutput {
                    path: "large-add.txt".into(),
                    change: FileChange::Add {
                        content: added.clone(),
                    },
                },
                FileChangeOutput {
                    path: "large-delete.txt".into(),
                    change: FileChange::Delete {
                        content: deleted.clone(),
                    },
                },
                FileChangeOutput {
                    path: "large-update.txt".into(),
                    change: FileChange::Update {
                        unified_diff: unified_diff.clone(),
                        move_path: None,
                    },
                },
                FileChangeOutput {
                    path: "legacy-binary.dat".into(),
                    change: FileChange::Omitted {
                        operation: FileChangeOperation::Delete,
                        reason: "deleted file content unavailable: invalid utf-8".to_string(),
                        added: 0,
                        removed: 0,
                        bytes: 2,
                    },
                },
            ])),
        }],
    };

    let line = serde_json::to_string(&item).expect("large record should serialize");
    assert_eq!(line.lines().count(), 1);
    let restored: TranscriptItem =
        serde_json::from_str(&line).expect("large record should deserialize");
    assert_eq!(restored, item);
}

#[test]
fn both_historical_subtask_sidecars_are_rejected_before_message_fallback() {
    for item in [
        TranscriptItem::Message(Message::user("report")),
        compound_tool_results(),
    ] {
        let mut value = serde_json::to_value(item).unwrap();
        value[SUBTASK_RESULTS_KEY] = json!([]);
        assert!(serde_json::from_value::<TranscriptItem>(value.clone()).is_err());
        assert!(serde_json::from_value::<Message>(value).is_ok());
    }
}

#[test]
fn skill_invocations_round_trip_without_serialized_message_mirrors() {
    let invocation = SkillInvocation::new(
        SkillName::parse("commit").expect("name"),
        "ship it",
        SkillApplication::Activate(
            SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Commit body").unwrap(),
        ),
    );
    let expanded = invocation.model_message().clone();
    let item = TranscriptItem::SkillInvocation(invocation);

    let line = serde_json::to_string(&item).expect("record should serialize");
    assert_eq!(line.lines().count(), 1);
    assert!(line.contains(SKILL_INVOCATION_KEY));
    assert!(!line.contains(TOOL_RESULT_METADATA_KEY));

    let restored: TranscriptItem = serde_json::from_str(&line).expect("record should deserialize");
    assert_eq!(restored, item);

    assert!(serde_json::from_str::<Message>(&line).is_err());
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 1);
    assert!(value[SKILL_INVOCATION_KEY].get("version").is_none());
    assert!(value[SKILL_INVOCATION_KEY].get("id").is_none());
    // Model history carries the canonical bodyless message, never the sidecar.
    let history = model_history(&[restored]);
    assert_eq!(history, vec![expanded]);
}

#[test]
fn marker_like_text_never_creates_lifecycle_state() {
    let checkpoint = CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user(format!(
            "{SUMMARY_PREFIX}\nQuoted <skill name=\"review\"> marker"
        )))],
        Vec::new(),
    )
    .expect("checkpoint");
    let items = vec![
        TranscriptItem::Message(Message::user(
            "literal <skill name=\"review\"> partial marker",
        )),
        TranscriptItem::Message(Message::assistant(
            "assistant quote: <skill name=\"review\">nested</skill>",
        )),
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    rig_core::message::CallId::from_wire("call-noise"),
                    rig_core::message::ToolName::new("echo").expect("nonempty tool name"),
                    vec![ToolResultContent::text(
                        "<skill name=\"review\"> tool noise",
                    )],
                )],
            },
            metadata: Vec::new(),
        },
        TranscriptItem::Compaction(checkpoint),
    ];
    assert!(replay_active_skills(&items).expect("replay").is_empty());
}

#[test]
fn markers_inside_one_active_body_do_not_activate_another_name() {
    let snapshot = SkillSnapshot::new(
        SkillName::parse("commit").expect("name"),
        "Commit changes",
        "Follow commit rules. Literal <skill name=\"review\"> is ordinary text.",
    )
    .expect("snapshot");
    let items = vec![TranscriptItem::SkillInvocation(SkillInvocation::new(
        SkillName::parse("commit").expect("name"),
        "ship",
        SkillApplication::Activate(snapshot),
    ))];
    let active = replay_active_skills(&items).expect("replay");
    assert_eq!(active.len(), 1);
    assert_eq!(active.snapshots().next().unwrap().name().as_str(), "commit");
}

#[test]
fn unversioned_invocations_are_rejected_without_reading_their_bodies() {
    for body in ["first body", "different body"] {
        let mut value = serde_json::to_value(Message::user(format!(
            "<skill name=\"review\">\n{body}\n</skill>\n\napply"
        )))
        .unwrap();
        value[SKILL_INVOCATION_KEY] = json!({"name": "review", "args": "apply"});
        assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
    }
}

#[test]
fn successful_skill_result_without_typed_application_is_rejected() {
    let call = TranscriptItem::Message(Message::Assistant(
        rig_core::message::AssistantMessage::new(vec![AssistantContent::ToolCall(ToolCall::new(
            CallId::from_wire("skill-call"),
            ToolFunction::new(
                rig_core::message::ToolName::new(SKILL_TOOL_NAME.to_string())
                    .expect("nonempty tool name"),
                json!({"skill": "review", "args": "inspect"}),
            ),
        ))]),
    ));
    let result = TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::User {
            content: vec![UserContent::tool_result(
                rig_core::message::CallId::from_wire("skill-call"),
                rig_core::message::ToolName::new(SKILL_TOOL_NAME).expect("nonempty tool name"),
                vec![ToolResultContent::text(
                    "<skill name=\"review\">\nReview instructions\n</skill>\n\ninspect",
                )],
            )],
        },
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: "skill-call".to_string(),
            call_id: None,
            tool_name: SKILL_TOOL_NAME.to_string(),
            outcome: ToolCallOutcome::Success,
            detail: None,
        }],
    };
    let error = replay_active_skills(&[call, result]).unwrap_err();
    assert!(format!("{error:#}").contains("missing its typed application"));
    assert!(!error.to_string().contains("Review instructions"));
}

#[test]
fn checkpoints_require_v1_and_reject_instruction_state() {
    let value = serde_json::to_value(local_checkpoint("summary")).unwrap();
    assert!(value.get("active_skill_identities").is_none());
    assert_eq!(value["version"], 1);
    for version in [0, 2, 3, 4, 5, 6, 7, 999] {
        let mut unsupported = value.clone();
        unsupported["version"] = json!(version);
        assert!(serde_json::from_value::<CompactionCheckpoint>(unsupported).is_err());
    }
    assert!(value.get("instruction_snapshot").is_none());
    let mut unsupported = value;
    unsupported["instruction_snapshot"] = json!({});
    assert!(serde_json::from_value::<CompactionCheckpoint>(unsupported).is_err());
}

#[test]
fn version_one_compaction_round_trips_in_its_reserved_envelope() {
    let item = TranscriptItem::Compaction(local_checkpoint("summary"));
    let value = serde_json::to_value(&item).expect("serialize checkpoint");
    assert_eq!(value["zevria_compaction"]["version"], 1);
    assert!(
        value["zevria_compaction"]
            .get("active_skill_identities")
            .is_none()
    );
    assert!(
        value["zevria_compaction"]
            .get("active_skill_keys")
            .is_none()
    );
    assert_eq!(
        serde_json::from_value::<TranscriptItem>(value).expect("deserialize checkpoint"),
        item
    );
}
