use super::*;
use serde_json::json;

fn attempt() -> ResponseAttempt {
    ResponseAttempt::new(crate::ModelProfileRef::new("p", "m"))
}
fn activity(status: HostedActivityStatus) -> HostedActivity {
    HostedActivity {
        kind: HostedActivityKind::WebSearch,
        item_id: Some("search".into()),
        output_index: 1,
        status,
        action: None,
    }
}

#[test]
fn native_display_derives_every_readable_item_without_mutating_replay() {
    let items = vec![
        json!({"type":"reasoning","id":"r","summary":[{"type":"summary_text","text":"think\u{001b}[31m"},{"type":"opaque","text":"hidden"}],"content":[{"type":"reasoning_text","text":"detail"},"plain",{"type":"text","text":"\u{202e}"}],"encrypted_content":"secret"}),
        json!({"type":"web_search_call","id":"s","status":"cancelled","action":{"type":"search","queries":["first","second"]}}),
        json!({"type":"function_call","id":"f","call_id":"call","arguments":"{}"}),
        json!({"type":"message","id":"m","content":[{"type":"output_text","text":"🦀é","annotations":[{"type":"url_citation","start_index":0,"end_index":2,"url":"https://example.org","title":"Source"}]},{"type":"refusal","refusal":"No\u{202e}"},{"type":"unknown","text":"hidden"}]}),
        json!({"type":"future","text":"opaque"}),
    ];
    let original = items.clone();
    let (parts, hosted) = responses_native_display(&items);
    assert_eq!(items, original);
    assert_eq!(
        parts
            .iter()
            .map(|p| (p.source.output_index, p.source.part))
            .collect::<Vec<_>>(),
        vec![
            (0, AssistantPartIdentity::Summary(0)),
            (0, AssistantPartIdentity::Content(0)),
            (0, AssistantPartIdentity::Content(1)),
            (2, AssistantPartIdentity::Tool),
            (3, AssistantPartIdentity::Content(0)),
            (3, AssistantPartIdentity::Content(1))
        ]
    );
    assert_eq!(
        parts[0].content,
        AssistantPresentationContent::Reasoning {
            text: "think".into()
        }
    );
    assert_eq!(
        parts[4].content,
        AssistantPresentationContent::Answer {
            text: "🦀é [Source](https://example.org/)".into()
        }
    );
    assert_eq!(
        parts[5].content,
        AssistantPresentationContent::Answer { text: "No".into() }
    );
    assert_eq!(hosted[0].status, HostedActivityStatus::Interrupted);
    assert_eq!(hosted[0].details(), ["first", "second"]);
}

#[test]
fn completion_replaces_live_only_activity_and_closure_is_sticky() {
    let mut attempt = attempt();
    attempt
        .hosted_activity
        .push(activity(HostedActivityStatus::Searching));
    assert_eq!(
        attempt.action_display_status(&attempt.hosted_activity[0]),
        "searching"
    );
    attempt.close(ResponseAttemptOutcome::Failed);
    assert_eq!(
        attempt.action_display_status(&attempt.hosted_activity[0]),
        "completion unconfirmed"
    );
    let failed = attempt.clone();
    attempt.close(ResponseAttemptOutcome::Interrupted);
    attempt.close_uncommitted(ResponseAttemptOutcome::Interrupted);
    assert_eq!(attempt, failed);
    assert_eq!(
        attempt.hosted_activity[0].status,
        HostedActivityStatus::Searching
    );
    attempt.hosted_activity[0].status = HostedActivityStatus::Completed;
    assert_eq!(
        attempt.action_display_status(&attempt.hosted_activity[0]),
        "completed"
    );
    attempt.complete_with(Vec::new(), Vec::new());
    assert_eq!(attempt.outcome, ResponseAttemptOutcome::Completed);
    assert!(!attempt.has_display());
    let revision = attempt.revision;
    attempt.close_uncommitted(ResponseAttemptOutcome::Interrupted);
    assert_eq!(attempt.outcome, ResponseAttemptOutcome::Interrupted);
    assert_eq!(attempt.revision, revision + 1);
}

#[test]
fn display_v2_round_trip_and_v1_rejection() {
    let mut attempt = attempt();
    attempt
        .hosted_activity
        .push(activity(HostedActivityStatus::Completed));
    let binding = DisplayProjectionBinding::Tool {
        tool_call_id: attempt.hosted_activity[0].client_id(&attempt.id),
        output_index: 1,
        native: false,
    };
    let mut display = ResponseDisplay {
        version: RESPONSE_DISPLAY_VERSION,
        attempt,
        bindings: vec![binding],
    };
    display.validate().unwrap();
    let bytes = serde_json::to_vec(&display).unwrap();
    assert_eq!(
        serde_json::from_slice::<ResponseDisplay>(&bytes).unwrap(),
        display
    );
    display.version = 1;
    assert!(display.validate().is_err());
    display.version = RESPONSE_DISPLAY_VERSION;
    display.bindings.push(DisplayProjectionBinding::Tool {
        tool_call_id: "bad".into(),
        output_index: 0,
        native: false,
    });
    assert!(display.validate().is_err());
    let mut value =
        serde_json::to_value(ResponseAttempt::new(crate::ModelProfileRef::new("p", "m"))).unwrap();
    value["version"] = json!(1);
    assert!(serde_json::from_value::<ResponseAttempt>(value).is_err());
}

#[test]
fn bounds_addresses_and_query_order_remain_validated() {
    let mut attempt = attempt();
    attempt
        .hosted_activity
        .push(activity(HostedActivityStatus::Searching));
    attempt.hosted_activity[0].action = Some(
        json!({"type":"search","query":"second","queries":["first","second","first"],"pattern":"日本語\u{001b}[31m"}),
    );
    assert_eq!(
        attempt.hosted_activity[0].details(),
        ["first", "second", "日本語"]
    );
    attempt.validate().unwrap();
    attempt
        .hosted_activity
        .push(attempt.hosted_activity[0].clone());
    assert!(attempt.validate().is_err());
    attempt.hosted_activity.pop();
    attempt.hosted_activity[0].output_index = 1_000_001;
    assert!(attempt.validate().is_err());
    assert!(HostedSearchAddress::parse("hosted-search:a:not-an-index").is_none());
    assert_eq!(
        HostedSearchAddress::parse("hosted-search:a:2")
            .unwrap()
            .output_index,
        2
    );
}

#[test]
fn one_native_status_mapping_covers_all_terminal_and_live_states() {
    for (native, expected) in [
        (Some("completed"), HostedActivityStatus::Completed),
        (Some("failed"), HostedActivityStatus::Failed),
        (Some("incomplete"), HostedActivityStatus::Interrupted),
        (Some("cancelled"), HostedActivityStatus::Interrupted),
        (Some("searching"), HostedActivityStatus::Searching),
        (None, HostedActivityStatus::InProgress),
        (Some("future"), HostedActivityStatus::InProgress),
    ] {
        assert_eq!(
            HostedActivityStatus::from_native_item_status(native),
            expected
        );
    }
}

#[test]
fn text_addresses_sizes_and_binding_coverage_remain_strict_in_v2() {
    let mut attempt = attempt();
    let (presentation, hosted) = responses_native_display(&[
        json!({"type":"message","id":"m","content":[{"type":"output_text","text":"answer"}]}),
        json!({"type":"function_call","id":"f","call_id":"call"}),
    ]);
    attempt.complete_with(presentation, hosted);
    let binding = DisplayProjectionBinding::Text {
        kind: DisplayProjectionKind::Message,
        message_id: "message".into(),
        start: 0,
        end: 6,
        sources: vec![attempt.presentation[0].source.clone()],
    };
    let display = ResponseDisplay {
        version: RESPONSE_DISPLAY_VERSION,
        attempt: attempt.clone(),
        bindings: vec![
            binding.clone(),
            DisplayProjectionBinding::Tool {
                tool_call_id: "call".into(),
                output_index: 1,
                native: true,
            },
        ],
    };
    display.validate().unwrap();
    let mut invalid = display.clone();
    invalid.bindings.push(binding);
    assert!(invalid.validate().is_err());
    let mut invalid = display;
    invalid.bindings[1] = DisplayProjectionBinding::Tool {
        tool_call_id: "call".into(),
        output_index: 1,
        native: false,
    };
    assert!(invalid.validate().is_err());
    let mut invalid = attempt.clone();
    invalid.presentation.push(invalid.presentation[0].clone());
    assert!(invalid.validate().is_err());
    let mut invalid = attempt.clone();
    invalid.presentation[0].source.part = AssistantPartIdentity::Summary(0);
    assert!(invalid.validate().is_err());
    let mut invalid = attempt.clone();
    invalid.presentation[0].content = AssistantPresentationContent::Answer {
        text: "\n\t".into(),
    };
    assert!(invalid.validate().is_err());
    let mut invalid = attempt.clone();
    invalid.presentation[0].content = AssistantPresentationContent::Answer {
        text: "x".repeat(4 * 1024 * 1024 + 1),
    };
    assert!(invalid.validate().is_err());
    let mut invalid = attempt;
    invalid.presentation[1].source.part = AssistantPartIdentity::Content(0);
    assert!(invalid.validate().is_err());
}
