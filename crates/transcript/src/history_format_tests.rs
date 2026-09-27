//! Persisted-format rejection matrix. Fixtures never use removed Rust variants.
use super::*;
use crate::{
    AgentElicitationOutcome, AgentRunDescriptor, AgentRunEvent, AgentRunId, AgentRunOutcome,
    AgentRunStatus, AgentRunTranscriptHeader, AgentRunTranscriptRecord, AgentRunTranscriptWriter,
    EnsembleRunId, EnsembleWorkflow, OwnedModelRequestItem,
    compaction::{CompactionBackend, CompactionTrigger},
    skill::{SkillApplication, SkillDefinition, SkillName, SkillSource},
};
use serde_json::{Value, json};

fn pinned_invocation() -> Vec<TranscriptItem> {
    let definition = SkillDefinition::new(
        SkillName::parse("review").unwrap(),
        "Review",
        "PRIVATE INSTRUCTIONS",
        SkillSource::Programmatic("test".into()),
    )
    .unwrap();
    let snapshot = definition.snapshot();
    // Independently calculated domain-separated, length-delimited snapshot encoding.
    assert_eq!(
        snapshot.digest().to_string(),
        "1da0f28b212b38be1e746ce68efa4a282c4a5d8685b9a4b8a3b1004842d6f905"
    );
    let restored = SkillDefinition::from_snapshot(&snapshot);
    assert!(restored.origin().is_none());
    assert!(restored.snapshot().provenance().is_none());
    assert_eq!(definition.digest(), restored.digest());
    vec![TranscriptItem::SkillInvocation(SkillInvocation::new(
        definition.name().clone(),
        "inspect",
        SkillApplication::Activate(snapshot),
    ))]
}

fn checkpoint(_items: &[TranscriptItem]) -> CompactionCheckpoint {
    CompactionCheckpoint::new(
        CompactionTrigger::Manual,
        CompactionBackend::LocalSummary,
        vec![OwnedModelRequestItem::message(Message::user("summary"))],
        vec![],
    )
    .unwrap()
}

#[test]
fn current_snapshots_and_checkpoints_restore_full_historical_pins() {
    for count in [0, 1, 2] {
        let mut items = pinned_invocation();
        if count == 0 {
            items.clear();
        }
        if count == 2 {
            let definition = SkillDefinition::new(
                SkillName::parse("second").unwrap(),
                "Second",
                "Other instructions",
                SkillSource::Programmatic("test".into()),
            )
            .unwrap();
            items.push(TranscriptItem::SkillInvocation(SkillInvocation::new(
                definition.name().clone(),
                "",
                SkillApplication::Activate(definition.snapshot()),
            )));
        }
        let active = replay_active_skills(&items).unwrap();
        let checkpoint = checkpoint(&items);
        assert_eq!(checkpoint.version, 1);
        assert_eq!(active.len(), count);
        items.push(TranscriptItem::Compaction(checkpoint));
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer.rewrite(&items).unwrap();
        assert_eq!(load(writer.path()).unwrap(), items);
        assert_eq!(replay_active_skills(&items).unwrap().len(), count);
    }
}

#[test]
fn unsupported_records_fail_every_position_without_partial_projection_or_backups() {
    let pair = pinned_invocation();
    let invocation = serde_json::to_value(&pair[0]).unwrap();
    let mut rejected = vec![
        json!({"zevria_skill_activation_v2":{}}),
        json!({"zevria_skill_activation_v3":{}}),
        json!({"zevria_skill_activation_v99":{}}),
        json!({"zevria_instruction_prefix":{"text":"PRIVATE INSTRUCTIONS"}}),
        json!({"zevria_directive":{"text":"PRIVATE INSTRUCTIONS"}}),
        json!({"role":"user", "content":"ordinary", "zevria_directive":{"text":"PRIVATE INSTRUCTIONS"}}),
        json!({"error":"ordinary", "zevria_instruction_prefix":{"text":"PRIVATE INSTRUCTIONS"}}),
        json!({"zevria_session_mode":{"version":1,"selected":"build"}, "zevria_directive":{"text":"PRIVATE INSTRUCTIONS"}}),
    ];
    for field in ["metadata", "digest"] {
        let mut value = invocation.clone();
        value[SKILL_INVOCATION_KEY]["application"]["activate"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        rejected.push(value);
    }
    let mut old = invocation.clone();
    old[SKILL_INVOCATION_KEY]["version"] = json!(3);
    rejected.push(old);
    let mut value = invocation;
    value[SKILL_INVOCATION_KEY] = json!({"name":"review", "args":"inspect"});
    rejected.push(value);
    for metadata in [false, true] {
        let mut value = serde_json::to_value(Message::user("ordinary delivery")).unwrap();
        value[SUBTASK_RESULTS_KEY] = json!([]);
        if metadata {
            value[TOOL_RESULT_METADATA_KEY] = json!([]);
        }
        rejected.push(value);
    }
    for version in [0, 2, 3, 4, 5, 6, 7, 99] {
        let mut value = serde_json::to_value(TranscriptItem::Compaction(checkpoint(&[]))).unwrap();
        value[COMPACTION_RECORD_KEY]["version"] = json!(version);
        rejected.push(value);
    }
    let mut value = serde_json::to_value(TranscriptItem::Compaction(checkpoint(&pair))).unwrap();
    value[COMPACTION_RECORD_KEY]["active_skill_identities"] = json!([]);
    rejected.push(value);
    let mut value = serde_json::to_value(TranscriptItem::Compaction(checkpoint(&pair))).unwrap();
    value[COMPACTION_RECORD_KEY]["instruction_snapshot"] = json!({"text":"PRIVATE INSTRUCTIONS"});
    rejected.push(value);
    let directive = crate::DirectiveContent::skill(
        &crate::SkillSnapshot::new("review".parse().unwrap(), "Review", "PRIVATE INSTRUCTIONS")
            .unwrap(),
    );
    for history in [
        json!([]),
        json!([OwnedModelRequestItem::DeveloperInstruction(directive)]),
        json!([{"type":"message", "message":{"role":"system", "content":"PRIVATE INSTRUCTIONS"}}]),
    ] {
        let mut value =
            serde_json::to_value(TranscriptItem::Compaction(checkpoint(&pair))).unwrap();
        value[COMPACTION_RECORD_KEY]["replacement_history"] = history;
        rejected.push(value);
    }
    for value in rejected {
        assert!(serde_json::from_value::<TranscriptItem>(value.clone()).is_err());
        // A complete unsupported inner payload is not made recoverable by
        // interrupting the final outer object delimiter.
        let mut truncated = value.to_string();
        assert_eq!(truncated.pop(), Some('}'));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.jsonl");
        std::fs::write(&path, &truncated).unwrap();
        assert!(
            TranscriptWriter::append_to(path.clone()).is_err(),
            "{truncated}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), truncated);
        for prefix in ["", "{\"error\":\"before\"}\n"] {
            for suffix in ["", "\n{\"error\":\"after\"}\n", "\n{\"partial\":"] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("unsupported.jsonl");
                let original = format!("{prefix}{value}{suffix}");
                std::fs::write(&path, &original).unwrap();
                let error = load_report(&path).unwrap_err();
                let format = error.downcast_ref::<UnsupportedHistory>().unwrap();
                assert_eq!(format.path, path);
                assert_eq!(format.line, Some(if prefix.is_empty() { 1 } else { 2 }));
                assert!(!error.to_string().contains("PRIVATE INSTRUCTIONS"));
                assert!(TranscriptWriter::append_to(path.clone()).is_err());
                assert!(TranscriptWriter::read_only(path.clone()).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
                assert!(!path.with_extension("jsonl.pre-v3").exists());
                assert!(!path.with_extension("jsonl.pre-session-models-v1").exists());
            }
        }
    }
}

#[test]
fn truncated_reserved_boundaries_are_not_crash_debris() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tail.jsonl");
    for tail in [
        r#"{"zevria_skill_activation_v2":{"snapshot":"#,
        r#"{"zevria_skill_activation_v99":"#,
        r#"{"role":"user","content":[],"zevria_subtask_results":["#,
        r#"{"zevria_skill_invocation":{"name":"rev"#,
        r#"{"role":"user","content":[],"zevria_tool_result_metadata":["#,
        r#"{"role":"user","content":[],"zevria_skill_applications":["#,
        r#"{"zevria_compaction":{"version":1,"replacement_history":["#,
        r#"{"zevria_compaction":{"version":2,"replacement_history":["#,
        r#"{"zevria_compaction":{"version":4,"replacement_history":["#,
        r#"{"zevria_provider_replay":{"version":2,"items":["#,
        r#"{"zevria_instruction_prefix":{"text":"PRIVATE INSTRUCTIONS"#,
        r#"{"zevria_directive":{"text":"PRIVATE INSTRUCTIONS"#,
        r#"{"role":"user","content":"ordinary","zevria_directive":"#,
        r#"{"zevria_instruction_pref"#,
        r#"{"zevria_direct"#,
        r#"{"zevria_compaction":{"version":6,"replacement_history":["#,
        r#"{"zevria_compaction":{"version":7,"instruction_snapshot":{"text":"PRIVATE INSTRUCTIONS"#,
    ] {
        let original = format!("{{\"error\":\"before\"}}\n{tail}");
        std::fs::write(&path, &original).unwrap();
        assert!(load_report(&path).is_err(), "{tail}");
        assert!(TranscriptWriter::append_to(path.clone()).is_err(), "{tail}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
    // Ordinary crash tails remain recoverable; quoted marker text and
    // provider-like tool arguments never become reserved persisted envelopes.
    let ordinary = TranscriptItem::Message(Message::user(
        r#"{"zevria_subtask_results":[],"zevria_directive":{},"zevria_instruction_prefix":{},"instruction_snapshot":{}}"#,
    ));
    let original = serde_json::to_string(&ordinary).unwrap();
    std::fs::write(
        &path,
        format!("{original}\n{{\"role\":\"user\",\"content\":["),
    )
    .unwrap();
    assert_eq!(load(&path).unwrap(), vec![ordinary.clone()]);
    assert_eq!(
        TranscriptWriter::append_to(path.clone())
            .unwrap()
            .recovered_malformed_lines(),
        1
    );
    assert_eq!(load(&path).unwrap(), vec![ordinary]);
}

#[test]
fn tool_result_sidecars_cannot_turn_ordinary_messages_into_lifecycle_records() {
    for message in [
        Message::user("ordinary text"),
        Message::assistant("ordinary answer"),
    ] {
        let mut value = serde_json::to_value(&message).unwrap();
        value["zevria_tool_result_metadata"] = json!([]);
        assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
        let record = TranscriptItem::ToolResults {
            message,
            metadata: vec![],
            skill_applications: vec![],
        };
        assert!(serde_json::to_value(&record).is_err());
        assert!(crate::replay_active_skills(&[record]).is_err());
    }
}

#[test]
fn marker_shaped_tool_arguments_are_data_but_checkpoint_sidecars_are_not() {
    use rig_core::message::{AssistantContent, ToolCall, ToolCallId, ToolFunction};
    let message = Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall::new(
            ToolCallId::new_or_mint("ordinary-call"),
            ToolFunction::new(
                "inspect_json".into(),
                json!({
                    "zevria_skill_activation_v2": {}, "zevria_subtask_results": [],
                    "zevria_provider_replay": {"version": 1},
                    "zevria_instruction_prefix": {}, "zevria_directive": {},
                    "instruction_snapshot": {},
                    "replay": {"version": 1, "provider": "openai.responses"},
                }),
            ),
        ))],
    };
    let ordinary = TranscriptItem::Message(message.clone());
    let value = serde_json::to_value(&ordinary).unwrap();
    assert_eq!(
        serde_json::from_value::<TranscriptItem>(value).unwrap(),
        ordinary
    );
    let mut checkpoint = checkpoint(&[]);
    checkpoint.replacement_history = vec![OwnedModelRequestItem::message(message)];
    let item = TranscriptItem::Compaction(checkpoint);
    let mut value = serde_json::to_value(&item).unwrap();
    assert_eq!(
        serde_json::from_value::<TranscriptItem>(value.clone()).unwrap(),
        item
    );
    value[COMPACTION_RECORD_KEY]["replacement_history"][0]["message"][SUBTASK_RESULTS_KEY] =
        json!([]);
    assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
}

fn worker_header() -> AgentRunTranscriptHeader {
    AgentRunTranscriptHeader {
        version: crate::AGENT_RUN_TRANSCRIPT_VERSION,
        ensemble_run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        descriptor: AgentRunDescriptor {
            id: AgentRunId::new(),
            agent: "test".into(),
            label: "Test".into(),
            safe_mode: "read-only".into(),
        },
        prompt: "review".into(),
    }
}

#[test]
fn review_and_worker_contracts_emit_v1_and_reject_other_versions_without_repair() {
    let directory = tempfile::tempdir().unwrap();
    let root_path = directory.path().join("root.jsonl");
    let worker_path = directory.path().join("worker.jsonl");
    let header = worker_header();
    let start = TranscriptItem::Ensemble(crate::EnsembleRecord::Started {
        start: crate::EnsembleStart {
            run_id: header.ensemble_run_id.clone(),
            workflow: EnsembleWorkflow::Plan,
            prompt: header.prompt.clone(),
            agents: vec![header.descriptor.clone()],
        },
    });
    let review = TranscriptItem::Ensemble(crate::EnsembleRecord::ReviewStarted {
        run_id: header.ensemble_run_id.clone(),
        version: zevria_workflow::ENSEMBLE_REVIEW_VERSION,
    });
    let review_json = serde_json::to_value(&review).unwrap();
    let worker_json = serde_json::to_value(AgentRunTranscriptRecord::Header { header }).unwrap();
    assert_eq!(review_json["zevria_ensemble"]["version"], 1);
    assert_eq!(worker_json["header"]["version"], 1);
    let start = serde_json::to_string(&start).unwrap();
    std::fs::write(&root_path, format!("{start}\n{review_json}\n")).unwrap();
    assert_eq!(load(&root_path).unwrap().len(), 2);
    std::fs::write(&worker_path, format!("{worker_json}\n")).unwrap();
    assert_eq!(crate::load_agent_run(&worker_path).unwrap().len(), 1);
    for version in [0, 2, 3, 99] {
        let mut review = review_json.clone();
        review["zevria_ensemble"]["version"] = json!(version);
        let review = review.to_string();
        for tail in [
            format!("{review}\n"),
            review.clone(),
            review[..review.len() - 1].to_owned(),
            format!(
                r#"{{"zevria_ensemble":{{"version":{version},"state":"review_started","run_id":"#
            ),
        ] {
            // Even a valid surviving review lifecycle cannot make an invalid
            // final version disposable crash debris.
            let root = format!("{start}\n{review_json}\n{tail}");
            std::fs::write(&root_path, &root).unwrap();
            assert!(load(&root_path).is_err());
            assert!(TranscriptWriter::append_to(root_path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&root_path).unwrap(), root);
        }
        let mut worker = worker_json.clone();
        worker["header"]["version"] = json!(version);
        assert!(serde_json::from_value::<AgentRunTranscriptRecord>(worker.clone()).is_err());
        let worker = worker.to_string();
        for bytes in [
            format!("{worker}\n"),
            worker.clone(),
            worker[..worker.len() - 1].to_owned(),
        ] {
            std::fs::write(&worker_path, &bytes).unwrap();
            assert!(crate::load_agent_run(&worker_path).is_err());
            assert!(AgentRunTranscriptWriter::append_to(worker_path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&worker_path).unwrap(), bytes);
        }
    }
}

#[test]
fn duplicate_owned_versions_cannot_hide_unsupported_records_or_tails() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("duplicate-version.jsonl");
    let replay = crate::ProviderReplay::openai_responses(
        crate::ModelProfileRef::new("p", "m"),
        vec![json!({
            "type":"message", "id":"answer", "status":"completed", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]
        })],
    );
    let directive = crate::DirectiveContent::skill(
        &crate::SkillSnapshot::new("review".parse().unwrap(), "Review", "Body").unwrap(),
    );
    for item in [
        TranscriptItem::Compaction(checkpoint(&[])),
        TranscriptItem::provider_message(replay).unwrap(),
        TranscriptItem::Directive(directive),
        TranscriptItem::Ensemble(crate::EnsembleRecord::ReviewStarted {
            run_id: EnsembleRunId::new(),
            version: 1,
        }),
    ] {
        let value = serde_json::to_value(item).unwrap();
        let (key, payload) = value.as_object().unwrap().iter().next().unwrap();
        let payload = payload.to_string();
        for duplicate in [1, 2, 99] {
            for record in [
                format!(
                    r#"{{"{key}":{{"version":{duplicate},{}}}}}"#,
                    &payload[1..payload.len() - 1]
                ),
                format!(
                    r#"{{"{key}":{{{},"version":{duplicate}}}}}"#,
                    &payload[1..payload.len() - 1]
                ),
            ] {
                assert!(
                    serde_json::from_str::<TranscriptItem>(&record).is_err(),
                    "{record}"
                );
                for bytes in [
                    format!("{record}\n"),
                    record.clone(),
                    record[..record.len() - 1].to_owned(),
                ] {
                    std::fs::write(&path, &bytes).unwrap();
                    assert!(load(&path).is_err(), "{bytes}");
                    assert!(TranscriptWriter::append_to(path.clone()).is_err());
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
                }
            }
        }
    }
}

fn worker_outcome(header: &AgentRunTranscriptHeader) -> AgentRunOutcome {
    AgentRunOutcome {
        confirmation: None,
        descriptor: header.descriptor.clone(),
        status: AgentRunStatus::Completed,
        report: "report".into(),
        plan: None,
        partial: false,
        failure: None,
        usage: None,
        acp_session_id: None,
        user_decisions: vec![],
        decision_ids: vec![],
        unavailable_decisions: vec![],
    }
}

#[test]
fn worker_rejection_precedes_projection_append_and_tail_repair() {
    let header = serde_json::to_string(&AgentRunTranscriptRecord::Header {
        header: worker_header(),
    })
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker.jsonl");
    for record in [
        r#"{"record":"event","event":{"type":"elicitation","field_count":1,"outcome":"accepted"}}"#,
        r#"{"record":"event","event":{"type":"agent_message","message_id":"zevria-claude-code-exit-plan-mode","text":"old plan"}}"#,
        r#"{"record":"event","event":{"type":"status","status":"completed","detail":null}}"#,
        r#"{"record":"event","event":{"type":"future_event"}}"#,
        r#"{"record":"event","event":{"type":"future_event","text":"#,
        r#"{"record":"event","event":{"type":"agent_message","message_id":"zevria-claude-code-exit-plan-mode","text":"#,
    ] {
        for suffix in ["", "\n{\"record\":\"event\",\"event\":"] {
            let original = format!("{header}\n{record}{suffix}");
            std::fs::write(&path, &original).unwrap();
            assert!(crate::load_agent_run_projection(&path).is_err(), "{record}");
            assert!(AgentRunTranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
}

fn display_record_json() -> String {
    use zevria_content::web_search::*;
    let mut attempt = WebSearchAttemptRecord::new(crate::ModelProfileRef::new("p", "m"));
    attempt.activity.push(WebSearchActivity {
        item_id: Some("search".into()),
        output_index: 1,
        status: WebSearchStatus::Completed,
        action: Some(json!({"query":"PRIVATE_QUERY"})),
    });
    attempt.terminal.insert(1, WebSearchStatus::Completed);
    let tool_call_id = attempt.activity[0].client_id(&attempt.id);
    serde_json::to_string(&AgentRunTranscriptRecord::Event {
        event: AgentRunEvent::ResponseDisplay {
            display: Box::new(ResponseDisplay {
                version: 1,
                attempt,
                bindings: vec![DisplayProjectionBinding::Tool {
                    tool_call_id,
                    output_index: 1,
                    native: false,
                }],
            }),
        },
    })
    .unwrap()
}

#[test]
fn incomplete_current_display_tails_are_read_only_until_writable_repair() {
    let header = serde_json::to_string(&AgentRunTranscriptRecord::Header {
        header: worker_header(),
    })
    .unwrap();
    let display = display_record_json();
    let terminal_value =
        display.find(r#""terminal":{"1":"completed"}"#).unwrap() + r#""terminal":{"1":"com"#.len();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("display.jsonl");
    for tail in [
        r#"{"record":"event","event":{"type":"response_display","display":"#,
        &display[..terminal_value],
        &display[..display.len() - 1],
    ] {
        let original = format!("{header}\n{tail}");
        std::fs::write(&path, &original).unwrap();
        assert_eq!(crate::load_agent_run(&path).unwrap().len(), 1);
        crate::load_agent_run_projection(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(AgentRunTranscriptWriter::append_to(path.clone()).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{header}\n")
        );
    }
    // A complete valid display lacking only a newline is evidence, not debris.
    let original = format!("{header}\n{display}");
    std::fs::write(&path, &original).unwrap();
    assert_eq!(crate::load_agent_run(&path).unwrap().len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    drop(AgentRunTranscriptWriter::append_to(path.clone()).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{original}\n")
    );
}

#[test]
fn corrupt_display_payloads_and_unsupported_tails_never_repair_away_evidence() {
    let header = serde_json::to_string(&AgentRunTranscriptRecord::Header {
        header: worker_header(),
    })
    .unwrap();
    let display = display_record_json();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-display.jsonl");
    let with_version = |path: &str, version: u32| {
        let mut value: Value = serde_json::from_str(&display).unwrap();
        *value.pointer_mut(path).unwrap() = json!(version);
        value.to_string()
    };
    let mut invalid = vec![
        display.replace(r#""terminal":{"1":"completed"}"#, r#""terminal":{"01":"completed"}"#),
        display.replace(r#""terminal":{"1":"completed"}"#, r#""terminal":{"1":"completed","1":"failed"}"#),
        display.replace(r#""terminal":{"1":"completed"}"#, r#""terminal":{"1":"searching"}"#),
        display.replace(r#""terminal":{"1":"completed"}"#, r#""terminal":{"11":"completed"}"#),
        with_version("/event/display/version", 99),
        with_version("/event/display/attempt/version", 2),
        display.replace(r#""type":"response_display""#, r#""type":"future_event""#),
        r#"{"record":"event","event":{"type":"status","status":"completed","detail":"PRIVATE_PROTOCOL"}}"#.into(),
    ];
    for field in ["revision", "presentation", "terminal"] {
        let mut value: Value = serde_json::from_str(&display).unwrap();
        value["event"]["display"]["attempt"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        invalid.push(value.to_string());
    }
    for record in invalid {
        // Missing only the outer delimiter must not disguise complete invalid
        // payloads, including duplicate keys, as an interrupted current append.
        for tail in [
            format!("{record}\n"),
            record.clone(),
            record[..record.len() - 1].to_owned(),
        ] {
            let original = format!("{header}\n{tail}");
            std::fs::write(&path, &original).unwrap();
            let error = crate::load_agent_run_projection(&path).unwrap_err();
            let history = error.downcast_ref::<UnsupportedHistory>().unwrap();
            assert_eq!(history.path, path);
            assert_eq!(history.line, Some(2));
            assert!(AgentRunTranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
    for tail in [
        r#"{"record":"event","event":{"type":"response_display","display":{"version":999,"attempt":"#,
        r#"{"record":"event","event":{"type":"response_display","display":{"version":1,"attempt":{"version":999,"id":"#,
        r#"{"record":"event","event":{"type":"future_event","display":"#,
    ] {
        let original = format!("{header}\n{tail}");
        std::fs::write(&path, &original).unwrap();
        assert!(crate::load_agent_run_projection(&path).is_err());
        assert!(AgentRunTranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn worker_diagnostics_distinguish_decoding_validation_and_versions_without_payloads() {
    let mut header = worker_header();
    header.prompt = "PRIVATE_PROMPT".into();
    let header_json = serde_json::to_string(&AgentRunTranscriptRecord::Header {
        header: header.clone(),
    })
    .unwrap();
    let display = display_record_json();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("diagnostic.jsonl");
    for record in [
        display.replace(
            r#""terminal":{"1":"completed"}"#,
            r#""terminal":{"PRIVATE_TERMINAL":"completed"}"#,
        ),
        display.replace(
            r#""terminal":{"1":"completed"}"#,
            r#""terminal":{"1":"PRIVATE_STATUS"}"#,
        ),
        display.replace(
            r#""type":"response_display""#,
            r#""type":"PRIVATE_VARIANT""#,
        ),
        display.replace(
            r#""version":1"#,
            &format!(r#""version":"{}""#, "PRIVATE_TYPE".repeat(1000)),
        ),
    ] {
        std::fs::write(&path, format!("{header_json}\n{record}\n")).unwrap();
        let error = crate::load_agent_run_projection(&path)
            .unwrap_err()
            .context("batch preflight");
        let history = error.downcast_ref::<UnsupportedHistory>().unwrap();
        assert!(matches!(
            history.failure,
            Some(HistoryFailure::Decode { .. })
        ));
        let rendered = format!("{history:?}\n{history}");
        assert!(!rendered.contains("PRIVATE_"));
        assert!(history.to_string().contains("decoding failure"));
        assert!(!history.to_string().contains("expected worker header"));
        assert!(!history.to_string().contains("older binary"));
        assert!(rendered.len() < 1500);
        assert_eq!(history.path, path);
        assert_eq!(history.line, Some(2));
    }
    for (record, detail) in [
        (r#"{"record":"event","event":{"type":"status","status":"completed","detail":"PRIVATE_PROTOCOL"}}"#.into(), "matching durable Outcome"),
        (display.replace(r#""terminal":{"1":"completed"}"#, r#""terminal":{"11":"completed"}"#), "invalid web action terminal evidence"),
        ({ let mut outcome = worker_outcome(&header); outcome.descriptor.label = "PRIVATE_IDENTITY".into(); serde_json::to_string(&AgentRunTranscriptRecord::Outcome { outcome }).unwrap() }, "identity differs"),
    ] {
        std::fs::write(&path, format!("{header_json}\n{record}\n")).unwrap();
        let error = crate::load_agent_run_projection(&path).unwrap_err();
        let history = error.downcast_ref::<UnsupportedHistory>().unwrap();
        assert!(matches!(history.failure, Some(HistoryFailure::Validation { .. })));
        assert!(history.to_string().contains(detail));
        assert!(!format!("{history:?}\n{history}").contains("PRIVATE_"));
        assert_eq!(history.line, Some(2));
    }
    header.version = 3;
    std::fs::write(
        &path,
        serde_json::to_vec(&AgentRunTranscriptRecord::Header { header }).unwrap(),
    )
    .unwrap();
    let error = crate::load_agent_run_projection(&path).unwrap_err();
    let history = error.downcast_ref::<UnsupportedHistory>().unwrap();
    assert!(matches!(
        history.failure,
        Some(HistoryFailure::Version {
            expected: 1,
            found: 3
        })
    ));
    assert!(
        history
            .to_string()
            .contains("worker version mismatch: found v3, expected v1")
    );
    assert_eq!(history.line, Some(1));
}

#[test]
fn worker_outcome_validation_is_per_prompt_not_global() {
    let header = worker_header();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("epochs.jsonl");
    let mut writer = AgentRunTranscriptWriter::create(path.clone(), header.clone()).unwrap();
    for continuation in [false, true, false] {
        writer
            .append(&AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Prompt {
                    text: "prompt".into(),
                    continuation,
                    repair: None,
                },
            })
            .unwrap();
        let status = AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Status {
                status: AgentRunStatus::Completed,
                detail: None,
            },
        };
        assert!(writer.append(&status).is_err());
        writer
            .append(&AgentRunTranscriptRecord::Outcome {
                outcome: worker_outcome(&header),
            })
            .unwrap();
        writer.append(&status).unwrap();
    }
    drop(writer);
    assert!(
        crate::load_agent_run_projection(&path)
            .unwrap()
            .recoverable_outcome()
            .is_some()
    );
    let mut writer = AgentRunTranscriptWriter::append_to(path.clone()).unwrap();
    writer
        .append(&AgentRunTranscriptRecord::Event {
            event: AgentRunEvent::Elicitation {
                field_count: 1,
                outcome: AgentElicitationOutcome::Declined,
                decision: None,
                decision_unavailable: None,
            },
        })
        .unwrap();
    assert!(crate::load_agent_run(&path).is_ok());
}

#[test]
fn accepted_decisions_and_summaries_have_no_compatibility_defaults() {
    let mut event = json!({"record":"event","event":{"type":"elicitation","field_count":1,"outcome":"accepted",
        "decision_unavailable":{"id":"marker","requestId":"request","fieldCount":1,"reason":"normalized_payload_too_large"}}});
    assert!(serde_json::from_value::<AgentRunTranscriptRecord>(event.clone()).is_ok());
    event["event"]["decision"] = json!({"requestId":"request","answers":[]});
    assert!(serde_json::from_value::<AgentRunTranscriptRecord>(event.clone()).is_err());
    event["event"].as_object_mut().unwrap().remove("decision");
    event["event"]["decision_unavailable"]
        .as_object_mut()
        .unwrap()
        .remove("requestId");
    assert!(serde_json::from_value::<AgentRunTranscriptRecord>(event).is_err());
    let summary = worker_outcome(&worker_header()).summary();
    let mut value: Value = serde_json::to_value(summary).unwrap();
    value.as_object_mut().unwrap().remove("has_plan_proof");
    assert!(serde_json::from_value::<crate::AgentRunSummary>(value).is_err());
}
