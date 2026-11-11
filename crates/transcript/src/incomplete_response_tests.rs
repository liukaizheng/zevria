use crate::*;
use rig_core::message::Message;
use serde_json::json;

fn attempt() -> ResponseAttempt {
    let mut attempt = ResponseAttempt::new(ModelProfileRef::new("p", "m"));
    attempt.id = "abandoned".into();
    attempt.presentation.push(AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Summary(0),
            item_id: Some("r".into()),
        },
        content: AssistantPresentationContent::Reasoning {
            text: "unfinished thought".into(),
        },
    });
    attempt.hosted_activity.push(HostedActivity {
        kind: HostedActivityKind::WebSearch,
        output_index: 1,
        item_id: Some("s".into()),
        status: HostedActivityStatus::Searching,
        action: Some(json!({"type":"search","query":"query"})),
    });
    attempt.close(ResponseAttemptOutcome::Interrupted);
    attempt
}
fn record() -> IncompleteResponse {
    IncompleteResponse::from_attempt(&attempt())
        .unwrap()
        .unwrap()
}
fn item() -> TranscriptItem {
    TranscriptItem::IncompleteResponse(record())
}
fn replay() -> TranscriptItem {
    TranscriptItem::provider_message(ProviderReplay::openai_responses(ModelProfileRef::new("p", "m"), vec![
        json!({"type":"reasoning","id":"r","summary":[{"type":"summary_text","text":"committed thought"}]}),
        json!({"type":"web_search_call","id":"s","status":"completed","action":{"type":"search","query":"query"}}),
        json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer","annotations":[]}]}),
    ])).unwrap()
}

#[test]
fn immutable_recovery_round_trip_is_model_inert_and_restore_is_pure() {
    let item = item();
    let bytes = serde_json::to_vec(&item).unwrap();
    let loaded: TranscriptItem = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(loaded, item);
    assert!(!String::from_utf8(bytes).unwrap().contains("response_id"));
    assert!(loaded.message().is_none());
    assert!(loaded.provider_replay().is_none());
    assert!(loaded.model_request_item().is_none());
    assert!(!is_prompt_item(&loaded));
    let restored = restored_attempt(&loaded, 10).unwrap();
    assert_eq!(restored, record().to_attempt());
    assert_eq!(restored.revision, 1);
    assert_eq!(
        restored.action_display_status(&restored.hosted_activity[0]),
        "completion unconfirmed"
    );
    let history = vec![
        TranscriptItem::Message(Message::user("prompt")),
        loaded,
        replay(),
    ];
    let without = vec![history[0].clone(), history[2].clone()];
    let input = |items: &[TranscriptItem]| {
        model_input(items)
            .iter()
            .map(|item| item.to_owned_item().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(input(&history), input(&without));
    SessionReplayError::validate(&history).unwrap();
}

#[test]
fn recovery_conversion_requires_abandonment_and_drops_tool_bindings() {
    let mut attempt = ResponseAttempt::new(ModelProfileRef::new("p", "m"));
    assert!(IncompleteResponse::from_attempt(&attempt).is_err());
    attempt.close(ResponseAttemptOutcome::Completed);
    assert!(IncompleteResponse::from_attempt(&attempt).is_err());
    attempt.close_uncommitted(ResponseAttemptOutcome::Failed);
    assert_eq!(IncompleteResponse::from_attempt(&attempt).unwrap(), None);
    attempt.presentation.push(AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Tool,
            item_id: None,
        },
        content: AssistantPresentationContent::NativeTool {
            call_id: "call".into(),
        },
    });
    assert_eq!(IncompleteResponse::from_attempt(&attempt).unwrap(), None);
    attempt.presentation[0].content =
        AssistantPresentationContent::NativeTool { call_id: "".into() };
    attempt.hosted_activity = record().hosted_activity;
    assert!(
        IncompleteResponse::from_attempt(&attempt)
            .unwrap()
            .unwrap()
            .presentation
            .is_empty()
    );
}

#[test]
fn incomplete_records_reject_wrong_shape_version_outcome_and_native_bindings() {
    let value = serde_json::to_value(item()).unwrap();
    let mut invalid = Vec::new();
    for (key, field) in [
        ("version", json!(2)),
        ("outcome", json!("completed")),
        ("outcome", json!("in_progress")),
        ("unknown", json!(true)),
    ] {
        let mut changed = value.clone();
        changed["zevria_incomplete_response"][key] = field;
        invalid.push(changed);
    }
    let mut changed = value.clone();
    changed["role"] = json!("assistant");
    invalid.push(changed);
    let mut changed = value.clone();
    changed["zevria_incomplete_response"]["presentation"] = json!([]);
    changed["zevria_incomplete_response"]["hosted_activity"] = json!([]);
    invalid.push(changed);
    let mut changed = value;
    changed["zevria_incomplete_response"]["presentation"] = json!([{"source":{"output_index":0,"part":"tool","item_id":null},"content":{"type":"native_tool","call_id":"call"}}]);
    invalid.push(changed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.jsonl");
    for value in invalid {
        let raw = value.to_string();
        assert!(serde_json::from_str::<TranscriptItem>(&raw).is_err());
        for tail in [format!("{raw}\n"), raw[..raw.len() - 1].to_owned()] {
            std::fs::write(&path, &tail).unwrap();
            assert!(load(&path).is_err(), "{tail}");
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), tail);
        }
    }
}

#[test]
fn current_torn_tail_is_recoverable_but_duplicate_ids_and_retired_keys_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let prefix = format!("{}\n", serde_json::to_string(&item()).unwrap());
    for tail in [
        r#"{"zevria_incomplete_response":{"version":1,"attempt_id":"next","presentation":["#,
        r#"{"zevria_incomplete_response":"#,
    ] {
        std::fs::write(&path, format!("{prefix}{tail}")).unwrap();
        let report = load_report(&path).unwrap();
        assert_eq!(report.items().len(), 1);
        assert_eq!(report.recoverable_lines, 1);
        drop(TranscriptWriter::append_to(path.clone()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), prefix);
    }
    let original = format!("{prefix}{prefix}");
    std::fs::write(&path, &original).unwrap();
    assert!(load_report(&path).is_err());
    assert!(TranscriptWriter::append_to(path.clone()).is_err());
    for key in ["zevria_web_search_attempt", "zevria_display_attempt"] {
        for tail in [format!(r#"{{"{key}":{{}}}}"#), format!(r#"{{"{key}":"#)] {
            let original = format!("{prefix}{tail}");
            std::fs::write(&path, &original).unwrap();
            assert!(load_report(&path).is_err());
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
}

#[test]
fn request_replay_is_inert_but_recovery_cannot_split_prompt_and_boundary() {
    use crate::request_replay::RequestReplayState;
    use zevria_instructions::{RequestDirective, RequestDirectiveKind};
    let request =
        zevria_foundation::RequestMetadata::new(zevria_foundation::RequestBehavior::Orchestrate);
    let prompt = TranscriptItem::RequestPrompt {
        message: Message::user("prompt"),
        request: request.clone(),
    };
    let boundary = TranscriptItem::RequestDirective(RequestDirective {
        version: 1,
        request: request.clone(),
        kind: RequestDirectiveKind::Boundary,
    });
    let mut state = RequestReplayState::default();
    state.apply(&prompt, true).unwrap();
    assert!(state.apply(&item(), true).is_err());
    state.apply(&boundary, true).unwrap();
    state
        .apply(
            &TranscriptItem::Message(Message::assistant("premature final")),
            true,
        )
        .unwrap();
    let before = state.clone();
    state.apply(&item(), true).unwrap();
    assert_eq!(state, before);
    state
        .apply(
            &TranscriptItem::RequestDirective(RequestDirective {
                version: 1,
                request,
                kind: RequestDirectiveKind::Correction,
            }),
            true,
        )
        .unwrap();
}

#[test]
fn committed_restore_matches_shared_derivation_and_ordinary_messages_stay_ordinary() {
    let item = replay();
    let raw = serde_json::to_vec(&item).unwrap();
    let restored = restored_attempt(&item, 42).unwrap();
    assert_eq!(restored.id, "transcript-42");
    assert_eq!(restored.outcome, ResponseAttemptOutcome::Completed);
    let display =
        zevria_content::response::responses_native_display(&item.provider_replay().unwrap().items);
    assert_eq!((restored.presentation, restored.hosted_activity), display);
    assert_eq!(serde_json::to_vec(&item).unwrap(), raw);
    assert!(
        restored_attempt(&TranscriptItem::Message(Message::assistant("ordinary")), 0).is_none()
    );
}
