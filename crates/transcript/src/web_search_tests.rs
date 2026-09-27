use crate::transcript::{
    Conversation, TranscriptDamage, TranscriptItem, TranscriptWriter, compact_linked_attempts,
    load, load_report, model_input,
};
use crate::{
    AssistantPartIdentity, AssistantPresentationContent, ModelProfileRef, ProviderReplay,
    SessionReplayError, WebSearchActivity, WebSearchAttemptOutcome as Outcome,
    WebSearchAttemptRecord, WebSearchStatus,
};
use serde_json::json;

fn unlinked_attempt(outcome: Outcome, id: &str, item_id: &str) -> TranscriptItem {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
    attempt.id = id.into();
    attempt.activity.push(WebSearchActivity {
        item_id: Some(item_id.into()),
        output_index: 1,
        status: WebSearchStatus::Completed,
        action: Some(json!({"type":"search", "query":"query"})),
    });
    attempt.finish(outcome);
    TranscriptItem::WebSearchAttempt(attempt)
}
fn replay() -> ProviderReplay {
    ProviderReplay::openai_responses(
        ModelProfileRef::new("p", "m"),
        vec![
            json!({"type":"reasoning","id":"r","summary":[{"type":"summary_text","text":"before"}],"content":["detail"],"encrypted_content":"OPAQUE_REASONING"}),
            json!({"type":"web_search_call","id":"w","status":"completed","action":{"type":"search","query":"query"}}),
            json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer","annotations":[]}]}),
        ],
    )
}

fn completed_attempt(id: &str) -> WebSearchAttemptRecord {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
    attempt.id = id.into();
    attempt.response_id = Some("response".into());
    attempt.reconcile_native_presentation(&replay().items);
    // Live observations can be absent from the final ledger, including both
    // explicit terminal evidence and actions whose completion is unconfirmed.
    for (output_index, status) in [
        (8, WebSearchStatus::Failed),
        (9, WebSearchStatus::Searching),
    ] {
        attempt.activity.push(WebSearchActivity {
            item_id: Some(format!("live-{output_index}")),
            output_index,
            status,
            action: Some(json!({"type":"search", "query":"live-only query"})),
        });
    }
    attempt.terminal.insert(8, WebSearchStatus::Failed);
    attempt.revision = 5;
    attempt.finish(Outcome::Completed);
    attempt
}

fn linked_response(id: &str) -> TranscriptItem {
    TranscriptItem::provider_message(replay())
        .unwrap()
        .with_display_attempt(Some(id.into()))
        .unwrap()
}

fn transcript_bytes(items: &[TranscriptItem]) -> String {
    items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap() + "\n")
        .collect()
}

#[test]
fn persisted_provisional_answers_restore_as_incomplete_without_becoming_model_input() {
    for outcome in [Outcome::InProgress, Outcome::Failed, Outcome::Interrupted] {
        let mut attempt = completed_attempt("partial");
        attempt.outcome = outcome;
        attempt
            .presentation
            .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
        attempt.presentation[0].content = AssistantPresentationContent::Answer {
            text: "🦀 unfinished answer\n```rust\nlet value =".into(),
        };
        let items = vec![
            TranscriptItem::Message(rig_core::message::Message::user("question")),
            TranscriptItem::WebSearchAttempt(attempt.clone()),
        ];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("partial.jsonl");
        let bytes = transcript_bytes(&items);
        std::fs::write(&path, &bytes).unwrap();
        let loaded = load(&path).unwrap();
        let restored = crate::reconstruct_transcript(&loaded);
        let TranscriptItem::WebSearchAttempt(display) = &restored[1] else {
            panic!("attempt")
        };
        assert_eq!(display.presentation, attempt.presentation);
        assert_eq!(
            display.outcome,
            if outcome == Outcome::InProgress {
                Outcome::Interrupted
            } else {
                outcome
            }
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            bytes,
            "restoration only changes the display copy"
        );
        assert_eq!(model_input(&loaded), model_input(&restored));
        assert_eq!(model_input(&restored), model_input(&items[..1]));
        let mut compact = loaded.clone();
        compact_linked_attempts(&mut compact);
        assert_eq!(compact, loaded, "incomplete evidence is never elided");
    }
}

#[test]
fn linked_native_replay_replaces_a_different_provisional_answer_without_rewriting_replay() {
    let mut attempt = completed_attempt("A");
    attempt.presentation.last_mut().unwrap().content = AssistantPresentationContent::Answer {
        text: "discard this provisional wording".into(),
    };
    let mut items = vec![
        TranscriptItem::WebSearchAttempt(attempt),
        linked_response("A"),
    ];
    let raw = serde_json::to_vec(&items[1]).unwrap();
    let full = crate::reconstruct_transcript(&items);
    assert!(
        !serde_json::to_string(&full)
            .unwrap()
            .contains("discard this provisional wording")
    );
    compact_linked_attempts(&mut items);
    assert_eq!(crate::reconstruct_transcript(&items), full);
    assert_eq!(serde_json::to_vec(&items[1]).unwrap(), raw);
    assert_eq!(model_input(&items), model_input(&full));
    assert_eq!(model_input(&items), model_input(&items[1..]));
}

#[test]
fn full_and_compacted_attempts_restore_identically_without_changing_model_input_or_replay() {
    let full = vec![
        TranscriptItem::SessionMode(crate::SessionMode::Build),
        TranscriptItem::WebSearchAttempt(completed_attempt("A")),
        linked_response("A"),
    ];
    let replay_bytes = serde_json::to_vec(&full[2]).unwrap();
    let input = |items: &[TranscriptItem]| {
        model_input(items)
            .into_iter()
            .map(|item| item.to_owned_item().unwrap())
            .collect::<Vec<_>>()
    };
    let mut compact = full.clone();
    compact_linked_attempts(&mut compact);
    SessionReplayError::validate(&compact).unwrap();
    let TranscriptItem::WebSearchAttempt(attempt) = &compact[1] else {
        panic!("attempt")
    };
    let TranscriptItem::WebSearchAttempt(original) = &full[1] else {
        panic!("attempt")
    };
    let mut expected = original.clone();
    expected.presentation.clear();
    expected.presentation_elided = true;
    assert_eq!(
        attempt, &expected,
        "all other lifecycle evidence is retained"
    );
    assert_eq!(
        crate::reconstruct_transcript(&compact),
        crate::reconstruct_transcript(&full)
    );
    assert_eq!(
        attempt
            .activity
            .iter()
            .map(|action| attempt.status_label(action))
            .collect::<Vec<_>>(),
        ["completed", "failed", "completion unconfirmed"]
    );
    let attempt_json = serde_json::to_string(&compact[1]).unwrap();
    assert!(attempt_json.contains("\"presentation_elided\":true"));
    for text in ["before", "detail", "answer", "OPAQUE_REASONING"] {
        assert!(!attempt_json.contains(text), "elided text: {text}");
    }
    assert!(model_input(&compact[1..2]).is_empty());
    assert_eq!(input(&compact), input(&full));
    assert_eq!(serde_json::to_vec(&compact[2]).unwrap(), replay_bytes);
    let once = compact.clone();
    compact_linked_attempts(&mut compact);
    assert_eq!(compact, once);
}

#[test]
fn retry_compaction_uses_exact_attempt_id_and_keeps_failed_presentation() {
    let mut failed = completed_attempt("F");
    failed.outcome = Outcome::Failed;
    failed.presentation[0].content = AssistantPresentationContent::Reasoning {
        text: "retry evidence".into(),
    };
    let mut items = vec![
        TranscriptItem::WebSearchAttempt(failed.clone()),
        TranscriptItem::WebSearchAttempt(completed_attempt("A")),
        linked_response("A"),
    ];
    let full = items.clone();
    compact_linked_attempts(&mut items);
    assert_eq!(items[0], full[0]);
    assert!(
        matches!(&items[1], TranscriptItem::WebSearchAttempt(attempt) if attempt.presentation_elided)
    );
    let restored = crate::reconstruct_transcript(&items);
    assert_eq!(restored[0], TranscriptItem::WebSearchAttempt(failed));
    assert_eq!(restored, crate::reconstruct_transcript(&full));
    assert_eq!(restored[2].display_attempt_id(), Some("A"));
}

#[test]
fn compaction_preserves_unlinked_unfinished_and_non_native_attempts() {
    for outcome in [
        Outcome::InProgress,
        Outcome::Failed,
        Outcome::Interrupted,
        Outcome::Completed,
    ] {
        for linked in [false, true] {
            if outcome == Outcome::Completed && linked {
                continue;
            }
            let mut attempt = completed_attempt("A");
            attempt.outcome = outcome;
            let mut items = vec![TranscriptItem::WebSearchAttempt(attempt)];
            if linked {
                items.push(linked_response("A"));
            }
            let full = items.clone();
            compact_linked_attempts(&mut items);
            assert_eq!(items, full);
        }
    }
    for items in [
        // Neither proximity, native item IDs nor identical answer text is a join.
        vec![
            TranscriptItem::WebSearchAttempt(completed_attempt("A")),
            linked_response("B"),
        ],
        vec![
            linked_response("A"),
            TranscriptItem::WebSearchAttempt(completed_attempt("A")),
        ],
        vec![
            TranscriptItem::WebSearchAttempt(completed_attempt("A")),
            TranscriptItem::Message(rig_core::message::Message::assistant("answer"))
                .with_display_attempt(Some("A".into()))
                .unwrap(),
        ],
    ] {
        let mut compact = items.clone();
        compact_linked_attempts(&mut compact);
        assert_eq!(compact, items);
        assert_eq!(
            crate::reconstruct_transcript(&compact),
            crate::reconstruct_transcript(&items)
        );
    }
}

#[test]
fn orphaned_compacted_attempts_fail_validation_load_and_writable_open_without_repair() {
    let mut attempt = completed_attempt("A");
    attempt.compact_presentation();
    let attempt = TranscriptItem::WebSearchAttempt(attempt);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("orphan.jsonl");
    for items in [
        vec![attempt.clone()],
        vec![attempt.clone(), linked_response("wrong-id")],
        vec![linked_response("A"), attempt.clone()],
        vec![
            attempt,
            TranscriptItem::Message(rig_core::message::Message::assistant("answer"))
                .with_display_attempt(Some("A".into()))
                .unwrap(),
        ],
    ] {
        let error = SessionReplayError::validate(&items).unwrap_err();
        assert!(matches!(error, SessionReplayError::WebSearch(_)));
        assert!(
            error
                .to_string()
                .contains("compacted web search attempt without its linked provider replay")
        );
        let bytes = transcript_bytes(&items);
        std::fs::write(&path, &bytes).unwrap();
        for error in [
            load(&path).unwrap_err(),
            TranscriptWriter::append_to(path.clone()).err().unwrap(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("compacted web search attempt without its linked provider replay")
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
    }
}

#[test]
fn linked_commit_atomically_compacts_the_checkpoint_and_preserves_tail_recovery() {
    for batch in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let writer = TranscriptWriter::create(directory.path()).unwrap();
        let path = writer.path().to_path_buf();
        let mut conversation = Conversation::new(writer);
        let full = TranscriptItem::WebSearchAttempt(completed_attempt("A"));
        conversation
            .push_completed_batch(vec![full.clone()])
            .unwrap();
        assert_eq!(load(&path).unwrap(), std::slice::from_ref(&full));
        assert_eq!(conversation.items(), &[full]);
        if batch {
            conversation
                .push_completed_batch(vec![linked_response("A")])
                .unwrap();
        } else {
            conversation
                .push_completed_linked(linked_response("A"))
                .unwrap();
        }
        let items = load(&path).unwrap();
        assert_eq!(items, conversation.items());
        assert!(
            matches!(&items[0], TranscriptItem::WebSearchAttempt(attempt) if attempt.presentation_elided && attempt.presentation.is_empty())
        );
        assert_eq!(items[1], linked_response("A"));
        let committed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(committed.lines().count(), 2);
        assert_eq!(committed, transcript_bytes(&items));

        // Only a later append can tear after the staged rename. Recovery must
        // keep the compacted attempt and its already committed replay together.
        let torn_path = directory.path().join("torn-append.jsonl");
        std::fs::write(&torn_path, format!("{committed}{{\"role\":\"assistant\"")).unwrap();
        let report = load_report(&torn_path).unwrap();
        assert_eq!(report.recoverable_lines, 1);
        assert_eq!(report.diagnostics[0].kind, TranscriptDamage::IncompleteTail);
        assert_eq!(report.items, items);
        let _repaired = TranscriptWriter::append_to(torn_path.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&torn_path).unwrap(), committed);

        // Artificially truncate the replay itself: no legal atomic commit
        // produces this state, and dropping the torn line would lose content.
        let replay_start = committed[..committed.len() - 1].rfind('\n').unwrap() + 1;
        let torn = &committed[..replay_start + (committed.len() - replay_start) / 2];
        std::fs::write(&torn_path, torn).unwrap();
        assert!(load(&torn_path).is_err());
        assert!(TranscriptWriter::append_to(torn_path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&torn_path).unwrap(), torn);
    }
}

#[test]
fn linked_v1_native_replay_recovers_source_order_without_rewriting_files() {
    let items = [
        TranscriptItem::SessionMode(crate::SessionMode::Build),
        TranscriptItem::WebSearchAttempt({
            let mut attempt = completed_attempt("attempt");
            attempt.compact_presentation();
            attempt
        }),
        linked_response("attempt"),
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("linked.jsonl");
    let bytes = items
        .iter()
        .map(|item| serde_json::to_string(item).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, &bytes).unwrap();
    let loaded = crate::transcript::load(&path).unwrap();
    let reconstructed = crate::reconstruct_transcript(&loaded);
    assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
    let TranscriptItem::WebSearchAttempt(attempt) = &reconstructed[1] else {
        panic!("attempt")
    };
    assert_eq!(attempt.version, 1);
    assert_eq!(attempt.presentation.len(), 3);
    assert_eq!(
        attempt.presentation[0].source.part,
        AssistantPartIdentity::Summary(0)
    );
    assert_eq!(
        attempt.presentation[1].source.part,
        AssistantPartIdentity::Content(0)
    );
    assert_eq!(attempt.presentation[2].source.output_index, 2);
    assert_eq!(reconstructed[2].display_attempt_id(), Some("attempt"));
    assert_eq!(reconstructed[2].provider_replay(), Some(&replay()));
    assert!(
        !serde_json::to_string(attempt)
            .unwrap()
            .contains("OPAQUE_REASONING")
    );
}

#[test]
fn unlinked_v1_attempts_never_borrow_answers_or_terminal_evidence_by_proximity() {
    for items in [
        vec![
            unlinked_attempt(Outcome::Failed, "failed", "w"),
            unlinked_attempt(Outcome::Completed, "success", "w"),
            TranscriptItem::provider_message(replay()).unwrap(),
        ],
        vec![
            unlinked_attempt(Outcome::Completed, "unknown", "different-item"),
            TranscriptItem::provider_message(replay()).unwrap(),
        ],
    ] {
        let restored = crate::reconstruct_transcript(&items);
        assert!(restored.last().unwrap().display_attempt_id().is_none());
        for attempt in restored.iter().filter_map(|item| match item {
            TranscriptItem::WebSearchAttempt(attempt) => Some(attempt),
            _ => None,
        }) {
            assert_eq!(attempt.version, 1);
            assert!(attempt.presentation.is_empty());
            assert!(attempt.terminal.is_empty());
            assert_eq!(
                attempt.status_label(&attempt.activity[0]),
                "completion unconfirmed"
            );
        }
    }
}

#[test]
fn malformed_or_unsupported_v1_attempt_records_never_repair_away_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid-attempt.jsonl");
    let valid =
        serde_json::to_value(TranscriptItem::WebSearchAttempt(completed_attempt("A"))).unwrap();
    assert_eq!(valid["zevria_web_search_attempt"]["version"], 1);
    let mut invalid = Vec::new();
    for version in [0, 2, 99] {
        let mut value = valid.clone();
        value["zevria_web_search_attempt"]["version"] = json!(version);
        invalid.push(value.to_string());
    }
    for field in ["revision", "presentation", "terminal"] {
        let mut value = valid.clone();
        value["zevria_web_search_attempt"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        invalid.push(value.to_string());
    }
    let mut retired = valid.clone();
    retired["zevria_web_search_attempt"]["ordering_unavailable"] = json!(true);
    invalid.push(retired.to_string());
    let raw = valid.to_string();
    for terminal in [
        r#"{"01":"completed","8":"failed"}"#,
        r#"{"1":"completed","1":"failed","8":"failed"}"#,
        r#"{"1":"searching","8":"failed"}"#,
    ] {
        invalid.push(raw.replace(
            r#""terminal":{"1":"completed","8":"failed"}"#,
            &format!(r#""terminal":{terminal}"#),
        ));
    }
    for record in invalid {
        assert!(
            serde_json::from_str::<TranscriptItem>(&record).is_err(),
            "{record}"
        );
        for tail in [
            format!("{record}\n"),
            record.clone(),
            record[..record.len() - 1].to_owned(),
        ] {
            let bytes = format!("{{\"error\":\"before\"}}\n{tail}");
            std::fs::write(&path, &bytes).unwrap();
            assert!(load(&path).is_err(), "{tail}");
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
    }
    for version in [2, 99] {
        let bytes = format!(r#"{{"zevria_web_search_attempt":{{"version":{version},"activity":["#);
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
    }
}

#[test]
fn native_tool_bindings_are_addresses_not_executable_copies() {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
    let replay = ProviderReplay::openai_responses(
        attempt.profile.clone(),
        vec![
            json!({"type":"function_call","status":"completed","id":"fc","call_id":"provider-call","name":"command","arguments":"{\"command\":\"echo hello\"}"}),
        ],
    );
    attempt.reconcile_native_presentation(&replay.items);
    assert!(
        matches!(&attempt.presentation[0].content, AssistantPresentationContent::NativeTool { call_id } if call_id == "provider-call")
    );
    assert!(
        !serde_json::to_string(&attempt)
            .unwrap()
            .contains("echo hello")
    );
    assert!(
        TranscriptItem::WebSearchAttempt(attempt)
            .model_request_item()
            .is_none()
    );
    let canonical = replay.to_message().unwrap();
    assert!(
        matches!(canonical, rig_core::message::Message::Assistant { content, .. } if content.iter().any(|part| matches!(part, rig_core::message::AssistantContent::ToolCall(call) if call.id == "provider-call")))
    );
}

#[test]
fn native_display_sanitization_does_not_change_canonical_replay() {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
    let text = "\u{1b}[31mClaim\u{1b}[0m\u{202e}";
    let replay = ProviderReplay::openai_responses(
        attempt.profile.clone(),
        vec![
            json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}),
        ],
    );
    let original = serde_json::to_value(&replay).unwrap();
    attempt.reconcile_native_presentation(&replay.items);
    assert!(
        matches!(&attempt.presentation[0].content, AssistantPresentationContent::Answer { text } if text == "Claim")
    );
    assert_eq!(serde_json::to_value(&replay).unwrap(), original);
    assert_eq!(replay.items[0]["content"][0]["text"], text);
}
