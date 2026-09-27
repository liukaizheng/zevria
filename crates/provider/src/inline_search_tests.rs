use super::*;
use zevria_content::AssistantPartIdentity;
use zevria_content::AssistantPresentationContent;
use zevria_content::WebSearchAttemptOutcome as Outcome;
use zevria_content::WebSearchStatus as Status;

#[tokio::test]
async fn websocket_answers_stream_before_completion_with_search_disabled_unused_and_used() {
    for (advertised, searched) in [(false, false), (true, false), (true, true)] {
        super::search_tests::held_answer_stream(true, advertised, searched).await;
    }
}

#[tokio::test]
async fn indexed_readable_evidence_survives_reset_without_opaque_payloads() {
    let progress = discard_updates();
    let mut state = AttemptState {
        search: Some(crate::search::SearchStream::new(
            test_profile_ref(),
            &progress,
        )),
        ..Default::default()
    };
    for event in [
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"output_index":0,"summary_index":0,"item_id":"r0","delta":"first\nthought"}),
        json!({"type":"response.web_search_call.searching","sequence_number":2,"output_index":1,"item_id":"w1"}),
        json!({"type":"response.output_item.done","sequence_number":3,"output_index":2,"item":{"type":"reasoning","id":"r2","summary":[{"type":"summary_text","text":"second thought"}],"content":["readable detail"],"encrypted_content":"PRIVATE_OPAQUE"}}),
        json!({"type":"response.web_search_call.completed","sequence_number":4,"output_index":1,"item_id":"w1","action":{"type":"search","queries":["one","two"],"query":"two"}}),
        json!({"type":"response.web_search_call.in_progress","sequence_number":5,"output_index":1,"item_id":"w1"}),
        json!({"type":"response.web_search_call.searching","sequence_number":6,"output_index":3,"item_id":"w3","action":{"type":"open_page","url":"https://example.org"}}),
        json!({"type":"response.output_text.done","sequence_number":7,"output_index":4,"content_index":0,"text":"not citation ready"}),
    ] {
        state.observe_search(&event.to_string(), &progress).await;
    }
    let search = state.search.as_ref().unwrap();
    assert_eq!(search.attempt().presentation.len(), 4);
    assert_eq!(
        search.attempt().presentation[0].source.part,
        AssistantPartIdentity::Summary(0)
    );
    assert_eq!(
        search.attempt().presentation[2].source.part,
        AssistantPartIdentity::Content(0)
    );
    assert_eq!(search.attempt().activity[0].status, Status::Completed);
    assert_eq!(search.attempt().activity[0].details(), vec!["one", "two"]);
    let serialized = serde_json::to_string(search.attempt()).unwrap();
    assert!(!serialized.contains("PRIVATE_OPAQUE"));
    assert!(serialized.contains("not citation ready"));
    assert!(serialized.contains("first\\nthought"));
    state.reset_result();
    state.finish_search(false, &progress).await.unwrap();
    let saved = progress.drain_web_search(Outcome::Interrupted);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].presentation.len(), 4);
    assert_eq!(saved[0].outcome, Outcome::Failed);
    assert_eq!(saved[0].status_label(&saved[0].activity[0]), "completed");
    assert_eq!(
        saved[0].status_label(&saved[0].activity[1]),
        "completion unconfirmed"
    );
}

#[tokio::test]
async fn duplicate_sequences_and_late_part_events_do_not_regress_native_answers() {
    let progress = discard_updates();
    let mut search = crate::search::SearchStream::new(test_profile_ref(), &progress);
    let reasoning = json!({"type":"response.reasoning_summary_text.delta","sequence_number":10,"output_index":0,"summary_index":0,"item_id":"r","delta":"once"});
    search.observe(&reasoning, &progress).await;
    let revision = search.attempt().revision;
    search.observe(&reasoning, &progress).await;
    assert_eq!(search.attempt().revision, revision);
    search.observe(&json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"m","content":[{"type":"output_text","text":"authoritative","annotations":[]}]}}), &progress).await;
    let revision = search.attempt().revision;
    search.observe(&json!({"type":"response.content_part.done","output_index":1,"content_index":0,"part":{"type":"output_text","text":"late provisional","annotations":[]}}), &progress).await;
    assert_eq!(search.attempt().revision, revision);
    assert!(search.attempt().presentation.iter().any(|part| matches!(&part.content, AssistantPresentationContent::Answer { text } if text == "authoritative")));
}

#[tokio::test]
async fn search_advertisement_does_not_require_an_action_to_retain_readable_content() {
    let progress = discard_updates();
    let mut search = crate::search::SearchStream::new(test_profile_ref(), &progress);
    assert!(!search.attempt().has_display());
    search.observe(&json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"readable failed response"}), &progress).await;
    search.finish(Outcome::Failed, &progress).await.unwrap();
    let attempt = progress
        .drain_web_search(Outcome::Interrupted)
        .pop()
        .unwrap();
    assert!(attempt.activity.is_empty());
    assert_eq!(attempt.presentation.len(), 1);
    assert_eq!(attempt.outcome, Outcome::Failed);
}

#[tokio::test]
async fn answers_stream_before_final_annotations_and_only_display_copies_are_sanitized() {
    let progress = discard_updates();
    let mut search = crate::search::SearchStream::new(test_profile_ref(), &progress);
    let text = "\u{1b}[31mClaim\u{1b}[0m\u{202e}";
    let annotation = json!({"type":"url_citation","start_index":0,"end_index":text.chars().count(),"title":"Source","url":"https://example.org"});
    for event in [
        json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":text}),
        json!({"type":"response.output_text.annotation.added","output_index":0,"content_index":0,"annotation_index":0,"annotation":annotation}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":text}}),
    ] {
        search.observe(&event, &progress).await;
        assert!(matches!(&search.attempt().presentation[0].content,
            AssistantPresentationContent::Answer { text } if text.starts_with("Claim") && !text.contains('\u{1b}') && !text.contains('\u{202e}')));
        assert_eq!(search.attempt().outcome, Outcome::InProgress);
    }
    search.observe(&json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":text,"annotations":[annotation]}}), &progress).await;
    let AssistantPresentationContent::Answer { text: display } =
        &search.attempt().presentation[0].content
    else {
        panic!("answer")
    };
    assert_eq!(display, "Claim [Source](https://example.org/)");
    assert_eq!(search.completed_text()[&(0, 0)].text, text);
    let before = search.attempt().clone();
    search.observe(&json!({"type":"response.output_text.annotation.added","output_index":0,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","start_index":0,"end_index":5,"title":"stale","url":"https://stale.example.org"}}), &progress).await;
    assert_eq!(
        search.attempt(),
        &before,
        "late deltas cannot revise native final annotations"
    );
}
