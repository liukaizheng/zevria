use crate::stream::{StreamSegments, display_message, session_notification};
use agent_client_protocol::schema::v1::{SessionId, SessionNotification, SessionUpdate};
use serde_json::json;
use zevria_content::AssistantPartIdentity;
use zevria_content::AssistantPresentationContent;
use zevria_content::AssistantPresentationPart;
use zevria_content::AssistantSourceAddress;
use zevria_content::AssistantStreamSnapshot;
use zevria_content::WebSearchActivity;
use zevria_content::WebSearchAttemptOutcome as Outcome;
use zevria_content::WebSearchAttemptRecord;
use zevria_content::WebSearchStatus;
use zevria_content::web_search::DisplayProjectionBinding as Binding;
use zevria_content::web_search::RESPONSE_DISPLAY_META_KEY;
use zevria_content::web_search::ResponseDisplay;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::TurnId;

fn attempt() -> WebSearchAttemptRecord {
    let mut attempt = WebSearchAttemptRecord::new(ModelProfileRef::new("p", "m"));
    attempt.presentation = vec![
        AssistantPresentationPart {
            source: AssistantSourceAddress {
                output_index: 0,
                part: AssistantPartIdentity::Summary(0),
                item_id: Some("r0".into()),
            },
            content: AssistantPresentationContent::Reasoning {
                text: "before".into(),
            },
        },
        AssistantPresentationPart {
            source: AssistantSourceAddress {
                output_index: 2,
                part: AssistantPartIdentity::Summary(0),
                item_id: Some("r2".into()),
            },
            content: AssistantPresentationContent::Reasoning {
                text: "after".into(),
            },
        },
        AssistantPresentationPart {
            source: AssistantSourceAddress {
                output_index: 3,
                part: AssistantPartIdentity::Content(0),
                item_id: Some("answer".into()),
            },
            content: AssistantPresentationContent::Answer {
                text: "answer [source](https://example.com)".into(),
            },
        },
    ];
    attempt.activity = vec![WebSearchActivity {
        output_index: 1,
        item_id: Some("web".into()),
        status: WebSearchStatus::Searching,
        action: Some(json!({"type":"search", "query":"query"})),
    }];
    attempt.touch();
    attempt
}
fn display(updates: Vec<SessionUpdate>) -> ResponseDisplay {
    let notification = updates
        .into_iter()
        .map(|update| session_notification(SessionId::new("session"), update))
        .find(|notification| {
            notification
                .meta
                .as_ref()
                .is_some_and(|meta| meta.contains_key(RESPONSE_DISPLAY_META_KEY))
        })
        .unwrap();
    let wire = serde_json::to_vec(&notification).unwrap();
    let decoded: SessionNotification = serde_json::from_slice(&wire).unwrap();
    assert_eq!(decoded, notification);
    let value = serde_json::to_value(&decoded).unwrap();
    assert!(value["_meta"][RESPONSE_DISPLAY_META_KEY].is_object());
    assert!(value["update"].get("_meta").is_none());
    let display: ResponseDisplay =
        serde_json::from_value(decoded.meta.unwrap()[RESPONSE_DISPLAY_META_KEY].clone()).unwrap();
    display.validate().unwrap();
    display
}

#[test]
fn provisional_answers_emit_suffixes_then_authoritative_citation_segments() {
    use crate::tests::{chunk_id, chunk_text};
    let turn = TurnId::new(1);
    let mut streams = StreamSegments::default();
    let mut attempt = attempt();
    attempt.activity.clear();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    let update =
        |streams: &mut StreamSegments, attempt: &mut WebSearchAttemptRecord, text: &str| {
            attempt.presentation[0].content =
                AssistantPresentationContent::Answer { text: text.into() };
            attempt.touch();
            streams.display_snapshot(
                turn,
                AssistantStreamSnapshot {
                    message: None,
                    attempt: Some(attempt.clone()),
                },
            )
        };
    let first = update(&mut streams, &mut attempt, "Claim.");
    assert_eq!(attempt.outcome, Outcome::InProgress);
    assert_eq!(chunk_text(&first[0]), "Claim.");
    let first_id = chunk_id(&first[0]).to_string();
    display(first);
    let grown = update(&mut streams, &mut attempt, "Claim. More prose.");
    assert_eq!(chunk_text(&grown[0]), " More prose.");
    assert_eq!(chunk_id(&grown[0]), first_id);
    display(grown);
    let corrected = "Claim. [Source](https://example.org/) More prose.";
    let correction = update(&mut streams, &mut attempt, corrected);
    assert_eq!(chunk_text(&correction[0]), corrected);
    let correction_id = chunk_id(&correction[0]).to_string();
    assert_ne!(correction_id, first_id);
    let metadata = display(correction);
    assert!(metadata.bindings.iter().any(|binding| matches!(binding, Binding::Text { message_id, end, .. } if message_id == &correction_id && *end == corrected.len())));
    attempt.finish(Outcome::Completed);
    let terminal = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(
        terminal.len(),
        1,
        "completion metadata does not repeat the answer"
    );
    let canonical =
        rig_core::message::Message::assistant(format!("\u{1b}[31m{corrected}\u{1b}[0m"));
    let bound = streams.terminal_bound(turn, &canonical, Some(&attempt.id));
    assert_eq!(bound.len(), 1, "terminal display stays sanitized and bound");
    assert_eq!(display(bound).attempt, attempt);
}

#[test]
fn repeated_attempt_revision_can_finish_an_ordinary_projection_and_its_binding() {
    let turn = TurnId::new(1);
    let mut attempt = attempt();
    attempt.activity.clear();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    attempt.presentation[0].content = AssistantPresentationContent::Answer {
        text: "complete text".into(),
    };
    let mut streams = StreamSegments::default();
    let incomplete = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: Some(rig_core::message::Message::assistant("complete")),
            attempt: Some(attempt.clone()),
        },
    );
    assert!(display(incomplete).bindings.is_empty());
    let complete = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: Some(rig_core::message::Message::assistant("complete text")),
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(crate::tests::chunk_text(&complete[0]), " text");
    assert_eq!(display(complete).bindings.len(), 1);
    assert!(
        streams
            .display_snapshot(
                turn,
                AssistantStreamSnapshot {
                    message: Some(rig_core::message::Message::assistant("complete text")),
                    attempt: Some(attempt)
                }
            )
            .is_empty()
    );
}

#[test]
fn sdk_notification_round_trip_preserves_source_coverage_and_standard_text() {
    let mut attempt = attempt();
    attempt.activity[0].status = WebSearchStatus::Completed;
    attempt.terminal.insert(1, WebSearchStatus::Completed);
    attempt.touch();
    let message = display_message(&attempt).unwrap();
    let mut ordinary = StreamSegments::default();
    let expected = ordinary.snapshot(TurnId::new(1), &message);
    let mut inline = StreamSegments::default();
    let updates = inline.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: Some(message),
            attempt: Some(attempt.clone()),
        },
    );
    let visible = updates
        .iter()
        .filter(|update| !matches!(update, SessionUpdate::SessionInfoUpdate(_)))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        visible, expected,
        "the extension must not redefine standard ACP projections"
    );
    let display = display(updates);
    assert_eq!(display.attempt, attempt);
    assert!(display.bindings.iter().any(|binding| matches!(binding, Binding::Text { sources, start: 0, end: 12, .. } if sources.len() == 2)));
    let record = zevria_transcript::AgentRunTranscriptRecord::Event {
        event: zevria_workflow::AgentRunEvent::ResponseDisplay {
            display: Box::new(display),
        },
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        record,
        serde_json::from_slice::<zevria_transcript::AgentRunTranscriptRecord>(&bytes).unwrap()
    );
}

#[test]
fn status_and_terminal_metadata_publish_without_a_new_text_suffix() {
    let mut attempt = attempt();
    let mut streams = StreamSegments::default();
    streams.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    );
    attempt.finish(Outcome::Failed);
    let updates = streams.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(updates.len(), 1);
    assert_eq!(
        display(updates).attempt.status_label(&attempt.activity[0]),
        "completion unconfirmed"
    );
    assert!(
        streams
            .display_snapshot(
                TurnId::new(1),
                AssistantStreamSnapshot {
                    message: None,
                    attempt: Some(attempt.clone())
                }
            )
            .is_empty()
    );
    attempt.revision = 0;
    assert!(
        streams
            .display_snapshot(
                TurnId::new(1),
                AssistantStreamSnapshot {
                    message: None,
                    attempt: Some(attempt)
                }
            )
            .is_empty()
    );
}

#[test]
fn replay_metadata_uses_the_same_reconstructed_attempt_without_duplicating_standard_answers() {
    use zevria_transcript::transcript::TranscriptItem;
    let mut attempt = attempt();
    attempt.activity[0].status = WebSearchStatus::Completed;
    attempt.terminal.insert(1, WebSearchStatus::Completed);
    attempt.finish(Outcome::Completed);
    let message = display_message(&attempt).unwrap();
    let items = vec![
        TranscriptItem::WebSearchAttempt(attempt.clone()),
        TranscriptItem::Message(message)
            .with_display_attempt(Some(attempt.id.clone()))
            .unwrap(),
    ];
    let updates = crate::replay::replay_transcript(&items, std::path::Path::new("."));
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::AgentMessageChunk(_)))
            .count(),
        1
    );
    let replay = display(updates);
    assert_eq!(replay.attempt, attempt);
    assert_eq!(
        replay
            .bindings
            .iter()
            .filter(|binding| matches!(binding, Binding::Text { .. }))
            .count(),
        3
    );
}

#[test]
fn compacted_native_replay_restores_identical_acp_updates_and_display_bindings() {
    use zevria_transcript::transcript::{TranscriptItem, compact_linked_attempts};
    let mut attempt = attempt();
    let replay = zevria_model::ProviderReplay::openai_responses(
        attempt.profile.clone(),
        vec![
            json!({"type":"reasoning", "id":"r0", "summary":[{"type":"summary_text", "text":"before"}]}),
            json!({"type":"web_search_call", "id":"web", "status":"completed", "action":{"type":"search", "query":"query"}}),
            json!({"type":"reasoning", "id":"r2", "summary":[{"type":"summary_text", "text":"after"}]}),
            json!({"type":"message", "id":"answer", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"answer [source](https://example.com)", "annotations":[]}]}),
        ],
    );
    attempt.reconcile_native_presentation(&replay.items);
    attempt.finish(Outcome::Completed);
    let mut items = vec![
        TranscriptItem::WebSearchAttempt(attempt.clone()),
        TranscriptItem::provider_message(replay)
            .unwrap()
            .with_display_attempt(Some(attempt.id.clone()))
            .unwrap(),
    ];
    let full = crate::replay::replay_transcript(&items, std::path::Path::new("."));
    compact_linked_attempts(&mut items);
    assert!(
        matches!(&items[0], TranscriptItem::WebSearchAttempt(saved) if saved.presentation_elided)
    );
    let compact = crate::replay::replay_transcript(&items, std::path::Path::new("."));
    assert_eq!(compact, full);
    let restored = display(compact);
    assert_eq!(restored.attempt, attempt);
    assert!(!restored.attempt.presentation_elided);
    assert_eq!(
        restored
            .bindings
            .iter()
            .filter(|binding| matches!(binding, Binding::Text { .. }))
            .count(),
        3
    );
}

#[test]
fn current_v1_replay_requires_terminal_evidence_to_confirm_completion() {
    use zevria_transcript::transcript::TranscriptItem;
    let value = json!({"zevria_web_search_attempt":{"version":1,"id":"current","profile":{"provider":"p","model":"m"},"response_id":null,"outcome":"failed","activity":[{"item_id":"web","output_index":0,"status":"completed","action":null}],"revision":1,"presentation":[],"terminal":{}}});
    let item: TranscriptItem = serde_json::from_value(value).unwrap();
    let updates = crate::replay::replay_transcript(&[item], std::path::Path::new("."));
    let display = display(updates);
    assert_eq!(display.version, 1);
    assert_eq!(display.attempt.version, 1);
    assert!(display.attempt.terminal.is_empty());
    assert!(display.attempt.presentation.is_empty());
    assert_eq!(
        display.attempt.status_label(&display.attempt.activity[0]),
        "completion unconfirmed"
    );
}
