use super::*;
use serde_json::json;
use zevria_content::{AssistantPartIdentity as Part, AssistantPresentationContent as Content};

fn state() -> SearchState {
    SearchState::new(zevria_foundation::ModelProfileRef::new("test", "test"))
}
fn answer(state: &SearchState) -> String {
    let message = crate::accumulator::AssistantMessageAccumulator::default()
        .streaming_message_with_search(state)
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
fn authoritative_items_remove_provisional_parts_and_validated_replay_removes_outputs() {
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
    state.ingest(&json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"not in replay"}));
    state.reconcile(&[item]);
    assert_eq!(answer(&state), "authoritative");
    assert_eq!(state.attempt().presentation.len(), 1);
    let final_attempt = state.attempt().clone();
    state.ingest(&delta("late suffix", 100));
    state.ingest(&json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"late omitted output"}));
    assert_eq!(state.attempt(), &final_attempt);
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
