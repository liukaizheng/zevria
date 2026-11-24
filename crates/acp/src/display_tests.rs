use crate::stream::{StreamSegments, display_message, session_notification};
use agent_client_protocol::schema::v1::{SessionId, SessionNotification, SessionUpdate};
use serde_json::json;
use zevria_content::AssistantPartIdentity;
use zevria_content::AssistantPresentationContent;
use zevria_content::AssistantPresentationPart;
use zevria_content::AssistantSourceAddress;
use zevria_content::AssistantStreamSnapshot;
use zevria_content::HostedActivity;
use zevria_content::HostedActivityStatus;
use zevria_content::ResponseAttempt;
use zevria_content::ResponseAttemptOutcome as Outcome;
use zevria_content::response::DisplayProjectionBinding as Binding;
use zevria_content::response::RESPONSE_DISPLAY_META_KEY;
use zevria_content::response::ResponseDisplay;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::TurnId;

fn attempt() -> ResponseAttempt {
    let mut attempt = ResponseAttempt::new(ModelProfileRef::new("p", "m"));
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
            content: AssistantPresentationContent::answer("answer [source](https://example.com)"),
        },
    ];
    attempt.hosted_activity = vec![HostedActivity {
        kind: zevria_content::HostedActivityKind::WebSearch,
        output_index: 1,
        item_id: Some("web".into()),
        status: HostedActivityStatus::Searching,
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
fn v2_metadata_carries_sources_without_changing_ordinary_text_or_source_less_json() {
    let turn = TurnId::new(1);
    let mut attempt = attempt();
    let mut streams = StreamSegments::default();
    let snapshot = |attempt: &ResponseAttempt| AssistantStreamSnapshot {
        message: None,
        attempt: Some(attempt.clone()),
    };
    let plain = display(streams.display_snapshot(turn, snapshot(&attempt)));
    let mut plain_json = serde_json::to_value(plain).unwrap();
    let content = &mut plain_json["attempt"]["presentation"][2]["content"];
    assert_eq!(
        *content,
        json!({"type":"answer", "text":"answer [source](https://example.com)"})
    );
    let pages = vec![zevria_content::CitedSource {
        url: "https://example.com/".into(),
        title: Some("Source".into()),
    }];
    attempt.presentation[2].content = AssistantPresentationContent::Answer {
        text: "answer [source](https://example.com)".into(),
        sources: pages.clone(),
    };
    attempt.touch();
    let updates = streams.display_snapshot(turn, snapshot(&attempt));
    assert_eq!(
        updates.len(),
        1,
        "the sources-only update is metadata, not a new text segment"
    );
    let sourced = display(updates);
    plain_json["attempt"]["presentation"][2]["content"]["sources"] =
        serde_json::to_value(pages).unwrap();
    plain_json["attempt"]["revision"] = json!(attempt.revision);
    assert_eq!(serde_json::to_value(sourced).unwrap(), plain_json);
}

#[test]
fn provisional_answers_emit_suffixes_then_authoritative_citation_segments() {
    use crate::tests::{chunk_id, chunk_text};
    let turn = TurnId::new(1);
    let mut streams = StreamSegments::default();
    let mut attempt = attempt();
    attempt.hosted_activity.clear();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    let update = |streams: &mut StreamSegments, attempt: &mut ResponseAttempt, text: &str| {
        attempt.presentation[0].content = AssistantPresentationContent::answer(text);
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
    assert_eq!(first.len(), 1);
    let grown = update(&mut streams, &mut attempt, "Claim. More prose.");
    assert_eq!(chunk_text(&grown[0]), " More prose.");
    assert_eq!(chunk_id(&grown[0]), first_id);
    assert_eq!(grown.len(), 1);
    let corrected = "Claim. [Source](https://example.org/) More prose.";
    let correction = update(&mut streams, &mut attempt, corrected);
    assert_eq!(chunk_text(&correction[0]), corrected);
    let correction_id = chunk_id(&correction[0]).to_string();
    assert_ne!(correction_id, first_id);
    assert_eq!(correction.len(), 1);
    attempt.close(Outcome::Completed);
    let terminal = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(
        terminal.len(),
        0,
        "ordinary completion adds no display metadata"
    );
    let canonical =
        rig_core::message::Message::assistant(format!("\u{1b}[31m{corrected}\u{1b}[0m"));
    let bound = streams.terminal_bound(turn, &canonical, Some(&attempt.id));
    assert!(
        bound.is_empty(),
        "ordinary terminal display stays sanitized without metadata"
    );
}

#[test]
fn hosted_display_is_not_reissued_for_each_token_and_covers_the_replaced_segment() {
    let turn = TurnId::new(1);
    let mut streams = StreamSegments::default();
    let mut attempt = attempt();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    let mut text = String::from("Draft");
    let now = tokio::time::Instant::now();
    let publish = |streams: &mut StreamSegments, attempt: &mut ResponseAttempt, text: &str| {
        attempt.presentation[0].content = AssistantPresentationContent::answer(text);
        attempt.touch();
        streams.display_snapshot_at(
            turn,
            AssistantStreamSnapshot {
                message: None,
                attempt: Some(attempt.clone()),
            },
            now,
        )
    };
    let initial = publish(&mut streams, &mut attempt, &text);
    let id = crate::tests::chunk_id(&initial[0]).to_string();
    assert_eq!(display(initial).bindings.len(), 2);
    for _ in 0..64 {
        text.push_str("界");
        let updates = publish(&mut streams, &mut attempt, &text);
        assert!(
            updates.is_empty(),
            "the suffix waits for its matching display"
        );
    }
    let deadline = streams.preview_deadline().unwrap();
    let mut live = Vec::new();
    streams.flush_preview(deadline, &mut live);
    assert_eq!(crate::tests::chunk_text(&live[0]), "界".repeat(64));
    assert_eq!(crate::tests::chunk_id(&live[0]), id);
    assert_eq!(display(live).attempt, attempt);
    assert!(streams.preview_deadline().is_none());

    text.push('!');
    assert!(publish(&mut streams, &mut attempt, &text).is_empty());
    let corrected = publish(&mut streams, &mut attempt, "Corrected");
    assert_eq!(
        corrected.len(),
        4,
        "prior bytes and coverage, replacement bytes and coverage"
    );
    assert_eq!(crate::tests::chunk_text(&corrected[0]), "!");
    assert_ne!(crate::tests::chunk_id(&corrected[2]), id);
    let outgoing = display(vec![corrected[1].clone()]);
    assert!(outgoing.bindings.iter().any(|binding| matches!(binding,
        Binding::Text { message_id, end, .. } if message_id == &id && *end == text.len()
    )));
}

#[test]
fn repeated_attempt_revision_can_finish_an_ordinary_projection_and_its_binding() {
    let turn = TurnId::new(1);
    let mut attempt = attempt();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    attempt.presentation[0].content = AssistantPresentationContent::answer("complete text");
    let mut streams = StreamSegments::default();
    let incomplete = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: Some(rig_core::message::Message::assistant("complete")),
            attempt: Some(attempt.clone()),
        },
    );
    assert!(
        display(incomplete)
            .bindings
            .iter()
            .all(|b| matches!(b, Binding::Tool { .. }))
    );
    let complete = streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: Some(rig_core::message::Message::assistant("complete text")),
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(crate::tests::chunk_text(&complete[0]), " text");
    assert_eq!(display(complete).bindings.len(), 2);
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
fn higher_revision_binds_a_caught_up_channel_and_reset_flushes_pending_coverage() {
    use rig_core::message::Message;
    let turn = TurnId::new(1);
    let mut attempt = attempt();
    attempt
        .presentation
        .retain(|part| matches!(part.content, AssistantPresentationContent::Answer { .. }));
    attempt.presentation[0].content = AssistantPresentationContent::answer("**claim**");
    let now = tokio::time::Instant::now();
    let mut streams = StreamSegments::default();
    let publish = |streams: &mut StreamSegments, attempt: &ResponseAttempt, raw| {
        streams.display_snapshot_at(
            turn,
            AssistantStreamSnapshot {
                message: Some(Message::assistant(raw)),
                attempt: Some(attempt.clone()),
            },
            now,
        )
    };
    assert_eq!(
        display(publish(&mut streams, &attempt, "**claim"))
            .bindings
            .len(),
        1
    );
    attempt.touch();
    let caught_up = publish(&mut streams, &attempt, "**claim**");
    let id = crate::tests::chunk_id(&caught_up[0]).to_string();
    assert_eq!(display(caught_up).bindings.len(), 2);
    let next = "**claim** and more";
    attempt.presentation[0].content = AssistantPresentationContent::answer(next);
    attempt.touch();
    assert!(publish(&mut streams, &attempt, next).is_empty());
    let outgoing = streams.reset();
    assert_eq!(crate::tests::chunk_text(&outgoing[0]), " and more");
    assert!(
        display(outgoing)
            .bindings
            .iter()
            .any(|binding| matches!(binding,
                Binding::Text { message_id, end, .. } if message_id == &id && *end == next.len()
            ))
    );
}

#[test]
fn terminal_uses_the_completed_attempt_after_a_reset_and_a_late_failure() {
    use rig_core::message::Message;
    let turn = TurnId::new(1);
    let mut winning = attempt();
    winning.close(Outcome::Completed);
    let mut streams = StreamSegments::default();
    streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(winning.clone()),
        },
    );
    streams.reset();
    let mut failed = attempt();
    failed.close(Outcome::Failed);
    streams.display_snapshot(
        turn,
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(failed),
        },
    );
    let terminal =
        streams.terminal_bound(turn, &Message::assistant("raw fallback"), Some(&winning.id));
    assert_eq!(
        crate::tests::chunk_text(&terminal[0]),
        "answer [source](https://example.com)"
    );
    assert_eq!(display(terminal).attempt, winning);
}

#[test]
fn sdk_notification_round_trip_preserves_source_coverage_and_standard_text() {
    let mut attempt = attempt();
    attempt.hosted_activity[0].status = HostedActivityStatus::Completed;
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
    attempt.close(Outcome::Failed);
    let updates = streams.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    );
    assert_eq!(updates.len(), 1);
    assert_eq!(
        display(updates)
            .attempt
            .action_display_status(&attempt.hosted_activity[0]),
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

fn replay_item() -> zevria_transcript::TranscriptItem {
    zevria_transcript::TranscriptItem::provider_message(zevria_model::ProviderReplay::openai_responses(
        ModelProfileRef::new("p", "m"), vec![
            json!({"type":"reasoning", "id":"r0", "summary":[{"type":"summary_text", "text":"before"}]}),
            json!({"type":"web_search_call", "id":"web", "status":"completed", "action":{"type":"search", "query":"query"}}),
            json!({"type":"reasoning", "id":"r2", "summary":[{"type":"summary_text", "text":"after"}]}),
            json!({"type":"message", "id":"answer", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"answer [source](https://example.com)", "annotations":[]}]}),
        ],
    )).unwrap()
}

#[test]
fn replay_only_commit_derives_the_same_display_without_duplicating_standard_answers() {
    let item = replay_item();
    let attempt = zevria_transcript::restored_attempt(&item, 0).unwrap();
    let updates = crate::replay::replay_transcript(&[item], std::path::Path::new("."));
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::AgentMessageChunk(_)))
            .count(),
        1
    );
    let replay = display(updates);
    assert_eq!(replay.attempt, attempt);
    assert_eq!(replay.version, 2);
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
fn incomplete_replay_and_live_display_have_identical_content_and_status() {
    let mut attempt = attempt();
    attempt.close(Outcome::Interrupted);
    let record = zevria_transcript::IncompleteResponse::from_attempt(&attempt)
        .unwrap()
        .unwrap();
    let items = [zevria_transcript::TranscriptItem::IncompleteResponse(
        Box::new(record.clone()),
    )];
    let replay = display(crate::replay::replay_transcript(
        &items,
        std::path::Path::new("."),
    ));
    assert_eq!(replay.attempt, record.to_attempt());
    assert_eq!(replay.attempt.presentation, attempt.presentation);
    assert_eq!(
        replay
            .attempt
            .action_display_status(&replay.attempt.hosted_activity[0]),
        "completion unconfirmed"
    );
}

#[test]
fn metadata_once_emitted_keeps_reconciling_after_live_only_activity_is_dropped() {
    let mut attempt = attempt();
    let mut streams = StreamSegments::default();
    display(streams.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    ));
    attempt.complete_with(attempt.presentation.clone(), Vec::new());
    let updated = display(streams.display_snapshot(
        TurnId::new(1),
        AssistantStreamSnapshot {
            message: None,
            attempt: Some(attempt.clone()),
        },
    ));
    assert_eq!(updated.attempt, attempt);
    assert!(
        updated
            .bindings
            .iter()
            .all(|binding| !matches!(binding, Binding::Tool { native: false, .. }))
    );
}
