use super::*;
use serde_json::json;
use zevria_content::{AssistantPartIdentity as Part, AssistantPresentationContent as Content};

fn state() -> AttemptReducer {
    AttemptReducer::new(zevria_foundation::ModelProfileRef::new("test", "test"))
}
fn answer(state: &AttemptReducer) -> String {
    let message = crate::accumulator::AssistantMessageAccumulator::default()
        .streaming_message_with_attempt(state)
        .unwrap();
    let text = zevria_content::assistant_plain_text(&message);
    let indexed = state
        .attempt()
        .presentation
        .iter()
        .filter_map(|part| match &part.content {
            Content::Answer { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text, indexed, "ordinary and indexed projections agree");
    text
}
#[test]
fn message_and_attempt_reasoning_parity_for_compatible_event_shapes() {
    use crate::{
        accumulator::AssistantMessageAccumulator, protocol::parse_server_event,
        wire::ResponsesWebSocketEvent,
    };
    use rig_core::message::{AssistantContent, Message};
    for events in [
        vec![
            json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"r","delta":"Read "}),
            json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"r","delta":"the source"}),
            json!({"type":"response.reasoning_text.done","output_index":0,"item_id":"r","content_index":0,"text":"Read the source"}),
        ],
        vec![
            json!({"type":"response.content_part.added","output_index":0,"item_id":"r","content_index":0,"part":{"type":"summary_text","text":"Read "}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"r","summary_index":0,"delta":"the source"}),
            json!({"type":"response.content_part.done","output_index":0,"item_id":"r","content_index":0,"part":{"type":"summary_text","text":"Read the source"}}),
        ],
    ] {
        let mut attempt = state();
        let mut message = AssistantMessageAccumulator::default();
        for (sequence, mut event) in events.into_iter().enumerate() {
            event["sequence_number"] = json!(sequence as u64);
            attempt.ingest(&event);
            let Some(ResponsesWebSocketEvent::Item(item)) =
                parse_server_event(&event.to_string()).unwrap()
            else {
                panic!("expected item")
            };
            message.record_item(item).unwrap();
            let Message::Assistant(assistant) =
                message.streaming_message_with_attempt(&attempt).unwrap()
            else {
                panic!("expected assistant")
            };
            let readable = assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Reasoning(reasoning) => Some(
                        zevria_content::reasoning::readable_parts(reasoning)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let indexed = attempt
                .attempt()
                .presentation
                .iter()
                .filter_map(|part| match &part.content {
                    Content::Reasoning { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(readable, indexed, "event: {event}");
        }
        let before = attempt.attempt().clone();
        if before.presentation[0].source.part == Part::Content(0) {
            attempt.ingest(&json!({"type":"response.reasoning_text.done","output_index":0,"text":"Read the source"}));
            assert_eq!(
                before,
                *attempt.attempt(),
                "missing done index defaults to zero"
            );
        }
        attempt.ingest(
            &json!({"type":"response.reasoning_text.delta","output_index":0,"delta":"stale"}),
        );
        // A done part must not be appended to by a late delta at that address.
        if before.presentation[0].source.part == Part::Content(0) {
            assert_eq!(before, *attempt.attempt());
        }
    }
}

#[test]
fn opaque_reasoning_parts_with_text_fields_do_not_become_display_evidence() {
    let mut state = state();
    for kind in [
        "unknown",
        "signature",
        "encrypted_content",
        "redacted_thinking",
    ] {
        for event in [
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_part.done",
            "response.content_part.added",
            "response.content_part.done",
        ] {
            state.ingest(&json!({"type":event,"output_index":0,"content_index":0,"summary_index":0,"part":{"type":kind,"text":"secret"}}));
        }
    }
    assert!(state.attempt().presentation.is_empty());
}

fn delta(text: &str, sequence: u64) -> Value {
    json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"sequence_number":sequence,"delta":text})
}
fn annotation(index: u64, sequence: u64, end: u64, url: &str) -> Value {
    json!({"type":"response.output_text.annotation.added","output_index":0,"content_index":0,"annotation_index":index,"sequence_number":sequence,"annotation":{"type":"url_citation","start_index":0,"end_index":end,"title":"Source","url":url}})
}

#[test]
fn growing_parts_reconcile_annotations_from_original_unicode_offsets() {
    let mut state = state();
    // Metadata can precede both the part setup and its text.
    let early = annotation(0, 3, 4, "https://example.org");
    state.ingest(&early);
    state.ingest(&json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}));
    state.ingest(&delta("🦀é", 4));
    assert_eq!(answer(&state), "🦀é", "future range is not a fallback link");
    assert!(state.completed_text().is_empty());
    let revision = state.attempt().revision;
    state.ingest(&delta("🦀é", 4));
    state.ingest(&delta("stale", 2));
    assert_eq!(state.attempt().revision, revision);
    state.ingest(&delta("你好", 5));
    assert_eq!(answer(&state), "🦀é你好 [Source](https://example.org/)");
    assert!(state.attempt().revision > revision);
    state.ingest(&json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"🦀é你好"}));
    state.ingest(&annotation(1, 6, 4, "javascript:alert(1)"));
    state.ingest(&annotation(2, 7, 999, "https://fallback.example.org"));
    assert_eq!(
        answer(&state),
        "🦀é你好 [Source](https://example.org/)\n\n[Source](https://fallback.example.org/)"
    );
    // Correct an annotation index, then reject its older duplicate without
    // rejecting an unrelated annotation that arrived out of sequence.
    state.ingest(&annotation(0, 10, 4, "https://correct.example.org"));
    state.ingest(&annotation(0, 8, 4, "https://stale.example.org"));
    state.ingest(&annotation(3, 9, 2, "https://other.example.org"));
    assert!(!answer(&state).contains("stale.example"));
    assert!(answer(&state).contains("other.example"));
    state.ingest(&json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"final","annotations":[early["annotation"]]}}));
    assert_eq!(answer(&state), "fina [Source](https://example.org/)l");
    let completed = state.attempt().clone();
    state.ingest(&delta("stale suffix", 11));
    state.ingest(&annotation(0, 12, 4, "https://late.example.org"));
    state.ingest(&json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"stale done"}));
    assert_eq!(state.attempt(), &completed);
    assert_eq!(state.completed_text()[&(0, 0)].text, "final");
}

#[test]
fn authoritative_items_remove_provisional_parts() {
    let mut state = state();
    state.ingest(&delta("provisional", 1));
    state.ingest(&json!({"type":"response.output_text.delta","output_index":0,"content_index":1,"delta":"trailing draft"}));
    assert_eq!(answer(&state), "provisional\ntrailing draft");
    let item = json!({"type":"message","id":"native","content":[{"type":"output_text","text":"authoritative","annotations":[]}]});
    state.ingest(&json!({"type":"response.output_item.done","output_index":0,"item":item}));
    assert_eq!(answer(&state), "authoritative");
    let completed = state.attempt().clone();
    for event in [
        json!({"type":"response.content_part.done","output_index":0,"content_index":1,"part":{"type":"output_text","text":"late trailing","annotations":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"stale-id","content":[{"type":"output_text","text":"stale setup"}]}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"another-stale-id","delta":"stale text"}),
        json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"late refusal"}),
    ] {
        state.ingest(&event);
    }
    assert_eq!(state.attempt(), &completed);
}

#[test]
fn opaque_markers_stay_hidden_until_annotations_arrive_even_after_text_done() {
    let mut state = state();
    let token = "\u{e200}cite\u{e202}turn0search0\u{e201}";
    let raw = format!("Claim {token} more prose.");
    state.ingest(&delta(&raw, 1));
    assert_eq!(answer(&state), "Claim  more prose.");
    state.ingest(
        &json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":raw}),
    );
    assert_eq!(answer(&state), "Claim  more prose.");
    let citation = json!({"type":"url_citation","start_index":6,"end_index":6+token.chars().count(),"title":"Source","url":"https://example.org"});
    state.ingest(&json!({"type":"response.output_text.annotation.added","output_index":0,"content_index":0,"annotation_index":0,"annotation":citation}));
    assert_eq!(
        answer(&state),
        "Claim [Source](https://example.org/) more prose."
    );
    state.ingest(&json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":raw,"annotations":[citation]}}));
    assert_eq!(
        answer(&state),
        "Claim [Source](https://example.org/) more prose."
    );
    assert_eq!(state.completed_text()[&(0, 0)].text, raw);
}

#[test]
fn later_output_text_does_not_wait_for_an_earlier_messages_item_done() {
    let mut state = state();
    state.ingest(&delta("early answer", 1));
    state.ingest(&json!({"type":"response.reasoning_summary_text.delta","output_index":1,"summary_index":0,"delta":"interleaved reasoning"}));
    state.ingest(&json!({"type":"response.output_text.delta","output_index":2,"content_index":0,"delta":"later answer"}));
    assert_eq!(answer(&state), "early answer\nlater answer");
    state.ingest(&delta(" grows", 2));
    assert_eq!(answer(&state), "early answer grows\nlater answer");
    assert_eq!(
        state
            .attempt()
            .presentation
            .iter()
            .map(|part| part.source.output_index)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(state.completed_text().is_empty());
}

#[test]
fn multiple_parts_and_mixed_activity_keep_source_order_as_the_current_part_grows() {
    let mut state = state();
    for event in [
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"thinking"}),
        json!({"type":"response.web_search_call.searching","output_index":1,"item_id":"search"}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call"}}),
        json!({"type":"response.output_text.delta","output_index":3,"content_index":1,"delta":"second"}),
    ] {
        state.ingest(&event);
    }
    assert!(
        !state
            .attempt()
            .presentation
            .iter()
            .any(|part| matches!(part.content, Content::Answer { .. }))
    );
    state.ingest(&json!({"type":"response.content_part.added","output_index":3,"content_index":0,"part":{"type":"output_text","text":"first","annotations":[]}}));
    assert_eq!(answer(&state), "first\nsecond");
    state.ingest(&json!({"type":"response.output_text.delta","output_index":3,"content_index":1,"delta":" grows"}));
    assert_eq!(answer(&state), "first\nsecond grows");
    assert_eq!(
        state
            .attempt()
            .presentation
            .iter()
            .map(|part| (part.source.output_index, part.source.part))
            .collect::<Vec<_>>(),
        vec![
            (0, Part::Summary(0)),
            (2, Part::Tool),
            (3, Part::Content(0)),
            (3, Part::Content(1))
        ]
    );
}

#[test]
fn refusals_and_split_controls_stream_without_mutating_raw_completed_parts() {
    let mut state = state();
    for event in [
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}),
        json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"Cannot "}),
        json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"help."}),
    ] {
        state.ingest(&event);
    }
    assert_eq!(answer(&state), "Cannot help.");
    state.ingest(&json!({"type":"response.refusal.done","output_index":0,"content_index":0,"refusal":"Cannot comply."}));
    state.ingest(&json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"stale"}));
    assert_eq!(answer(&state), "Cannot comply.");
    let mut state = super::tests::state();
    state.ingest(&delta("Before \u{1b}[", 1));
    state.ingest(&delta("31mred\u{1b}[0m\u{202e}", 2));
    assert_eq!(answer(&state), "Before red");
    let raw = "Before \u{1b}[31mred\u{1b}[0m\u{202e}";
    state.ingest(&json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":raw,"annotations":[]}}));
    assert_eq!(answer(&state), "Before red");
    assert_eq!(state.completed_text()[&(0, 0)].text, raw);
}

#[test]
fn large_single_part_keeps_latest_text_not_a_delta_history() {
    let mut state = state();
    let chunk = "🦀 A readable line in an unfinished code fence.\n";
    for sequence in 0..1024 {
        state.ingest(&delta(chunk, sequence));
    }
    assert_eq!(answer(&state), chunk.repeat(1024));
    assert_eq!(state.live_text.len(), 1);
    assert_eq!(state.display_text.len(), 1);
    assert_eq!(state.attempt().presentation.len(), 1);
    assert!(state.completed_text().is_empty());
    assert!(state.dirty_text.is_empty());
}

#[test]
fn item_id_matching_cannot_confirm_a_different_output_index() {
    let mut state = state();
    for event in [
        json!({"type":"response.web_search_call.searching","output_index":0,"item_id":"first"}),
        json!({"type":"response.web_search_call.searching","output_index":1,"item_id":"second"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"first","status":"cancelled"}}),
    ] {
        state.ingest(&event);
    }
    assert_eq!(
        state.attempt().hosted_activity[0].status,
        HostedActivityStatus::Interrupted
    );
    assert_eq!(
        state.attempt().hosted_activity[1].status,
        HostedActivityStatus::Searching
    );
    state
        .attempt_mut()
        .close(zevria_content::ResponseAttemptOutcome::Failed);
    assert_eq!(
        state
            .attempt()
            .action_display_status(&state.attempt().hosted_activity[1]),
        "completion unconfirmed"
    );
}
