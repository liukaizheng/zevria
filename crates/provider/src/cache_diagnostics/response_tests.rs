use super::*;
use serde_json::json;

fn observation(diagnostics: Option<Value>) -> Observation {
    let mut response = json!({"id":"resp_synthetic"});
    if let Some(value) = diagnostics {
        response["prompt_cache_diagnostics"] = value;
    }
    Observation::parse(&json!({"type":"response.completed", "response":response}).to_string())
        .unwrap()
}

#[test]
fn reported_input_arithmetic_is_not_a_provider_cache_verdict() {
    use Counter::*;
    use ReportedInput::*;
    for (input, cached, category, uncached, fraction) in [
        (Present(100), Present(0), ZeroCached, Some(100), Some(0.0)),
        (Present(100), Present(25), Partial, Some(75), Some(0.25)),
        (Present(100), Present(100), FullyCached, Some(0), Some(1.0)),
        (Present(0), Present(0), ZeroInput, Some(0), None),
        (Present(0), Present(1), CachedExceedsInput, None, None),
        (Present(100), Present(101), CachedExceedsInput, None, None),
        (
            Present(u64::MAX),
            Present(u64::MAX),
            FullyCached,
            Some(0),
            Some(1.0),
        ),
    ] {
        assert_eq!(
            Accounting::new(input, cached),
            Accounting {
                category,
                uncached,
                fraction
            }
        );
    }
    for unavailable in [Missing, Null, Malformed] {
        for (input, cached) in [
            (unavailable, Present(0)),
            (Present(100), unavailable),
            (unavailable, unavailable),
        ] {
            assert_eq!(
                Accounting::new(input, cached),
                Accounting {
                    category: Unavailable,
                    uncached: None,
                    fraction: None
                }
            );
        }
    }
}

#[test]
fn every_comparison_outcome_and_unavailable_shape_stays_distinct() {
    use ComparisonOutcome::*;
    for (value, expected) in [
        (None, Absent),
        (Some(Value::Null), Null),
        (Some(json!([])), Malformed),
        (Some(json!("PRIVATE_BODY")), Malformed),
        (Some(json!({})), Malformed),
        (Some(json!({"type":null})), Malformed),
        (Some(json!({"type":17})), Malformed),
        (Some(json!({"type":"cache_hit"})), CacheHit),
        (Some(json!({"type":"cache_miss"})), CacheMiss),
        (
            Some(json!({"type":"comparison_response_not_found"})),
            ComparisonResponseNotFound,
        ),
        (Some(json!({"type":"unavailable"})), Unavailable),
        (Some(json!({"type":"PRIVATE_FUTURE_OUTCOME"})), Unknown),
        (Some(json!({"type":"x".repeat(16384)})), Unknown),
    ] {
        let raw = observation(value);
        assert_eq!(raw.comparison.outcome, expected);
        assert_eq!(raw.comparison.conclusive(), expected == CacheHit);
        let encoded = serde_json::to_string(&raw).unwrap();
        assert!(!encoded.contains("PRIVATE_"));
        assert!(encoded.len() < 1200);
        assert_eq!(serde_json::from_str::<Observation>(&encoded).unwrap(), raw);
    }
}

#[test]
fn documented_reasons_and_estimates_are_separate_presence_aware_evidence() {
    use ComparisonReason::*;
    for (reason, expected) in [
        ("model_changed", ModelChanged),
        ("prompt_cache_key_changed", PromptCacheKeyChanged),
        ("tools_changed", ToolsChanged),
        ("text_format_changed", TextFormatChanged),
        ("reasoning_effort_changed", ReasoningEffortChanged),
        ("verbosity_changed", VerbosityChanged),
        ("context_compacted", ContextCompacted),
        ("input_changed", InputChanged),
        ("service_tier_changed", ServiceTierChanged),
        ("PRIVATE_FUTURE_REASON", Unknown),
    ] {
        let raw = observation(Some(
            json!({"type":"cache_miss", "reason":reason, "comparison_reusable_tokens":5000, "cache_missed_tokens":1000}),
        ));
        assert_eq!(raw.comparison.reason, expected);
        assert_eq!(
            raw.comparison.comparison_reusable_tokens,
            Counter::Present(5000)
        );
        assert_eq!(raw.comparison.cache_missed_tokens, Counter::Present(1000));
        assert_eq!(raw.comparison.conclusive(), expected != Unknown);
        assert_eq!(
            raw.input,
            Counter::Missing,
            "estimates cannot fill usage counters"
        );
    }
    for (reason, expected) in [
        (Value::Null, Null),
        (json!({"PRIVATE_REASON":"secret"}), Malformed),
        (json!("x".repeat(8192)), Unknown),
    ] {
        let raw = observation(Some(
            json!({"type":"cache_miss", "reason":reason, "cache_missed_tokens":0}),
        ));
        assert_eq!(raw.comparison.reason, expected);
        assert!(!raw.comparison.conclusive());
        assert!(serde_json::to_string(&raw).unwrap().len() < 1200);
    }
    for name in ["comparison_reusable_tokens", "cache_missed_tokens"] {
        for (value, expected) in [
            (None, Counter::Missing),
            (Some(Value::Null), Counter::Null),
            (Some(json!(-1)), Counter::Malformed),
            (Some(json!(1.5)), Counter::Malformed),
            (Some(json!("PRIVATE_COUNTER")), Counter::Malformed),
            (Some(json!([])), Counter::Malformed),
            (Some(json!(0)), Counter::Present(0)),
            (Some(json!(u64::MAX)), Counter::Present(u64::MAX)),
        ] {
            let mut diagnostics = json!({"type":"cache_miss", "reason":"input_changed"});
            if let Some(value) = value {
                diagnostics[name] = value;
            }
            let comparison = observation(Some(diagnostics)).comparison;
            let counter = if name == "cache_missed_tokens" {
                comparison.cache_missed_tokens
            } else {
                comparison.comparison_reusable_tokens
            };
            assert_eq!(counter, expected);
        }
    }
    assert_eq!(
        observation(Some(Value::Null))
            .comparison
            .cache_missed_tokens,
        Counter::Null
    );
    assert_eq!(
        observation(Some(json!([]))).comparison.cache_missed_tokens,
        Counter::Malformed
    );
    assert_eq!(observation(None).comparison.reason, Missing);
    assert_eq!(observation(Some(Value::Null)).comparison.reason, Null);
    for malformed in [json!([]), json!("PRIVATE_BODY"), json!(false)] {
        assert_eq!(observation(Some(malformed)).comparison.reason, Malformed);
    }
}

#[test]
fn native_counts_never_retain_search_reasoning_or_opaque_content() {
    let items = vec![
        json!({"type":"reasoning", "summary":[{"text":"PRIVATE_REASONING"}], "encrypted_content":"PRIVATE_ENCRYPTED"}),
        json!({"type":"web_search_call", "action":{"type":"search", "queries":["PRIVATE_QUERY"]}}),
        json!({"type":"web_search_call", "action":{"type":"open_page", "url":"https://PRIVATE_URL"}}),
        json!({"type":"web_search_call", "action":{"type":"find_in_page", "pattern":"PRIVATE_PATTERN"}}),
        json!({"type":"web_search_call", "action":{"type":"PRIVATE_FUTURE_ACTION"}}),
        json!({"type":"web_search_call"}),
        json!({"type":"PRIVATE_TYPE"}),
    ];
    let counts = NativeCounts::from_items(&items);
    assert_eq!(
        counts,
        NativeCounts {
            search: 1,
            open_page: 1,
            find_in_page: 1,
            other_search: 2,
            reasoning: 1
        }
    );
    assert!(counts.valid(items.len()));
    assert!(!counts.valid(5));
    assert!(
        !NativeCounts {
            search: u64::MAX,
            reasoning: 1,
            ..Default::default()
        }
        .valid(7)
    );
    assert!(!serde_json::to_string(&counts).unwrap().contains("PRIVATE_"));
    let raw = observation(Some(
        json!({"type":"cache_hit", "opaque":{"encrypted_content":"PRIVATE_ENCRYPTED"}, "reason":"PRIVATE_REASON", "queries":["PRIVATE_QUERY"]}),
    ));
    assert!(!serde_json::to_string(&raw).unwrap().contains("PRIVATE_"));
}

#[test]
fn malformed_or_contradictory_hit_fields_do_not_resolve_an_investigation() {
    for (field, value) in [
        ("reason", json!("input_changed")),
        ("reason", Value::Null),
        ("cache_missed_tokens", json!(17)),
        ("cache_missed_tokens", json!("PRIVATE_COUNTER")),
        ("comparison_reusable_tokens", Value::Null),
    ] {
        let mut value_with_hit = json!({"type":"cache_hit"});
        value_with_hit[field] = value;
        let comparison = observation(Some(value_with_hit)).comparison;
        assert_eq!(comparison.outcome, ComparisonOutcome::CacheHit);
        assert!(!comparison.conclusive());
    }
}

#[test]
fn observation_inspection_cap_is_unchanged() {
    assert!(Observation::parse(&" ".repeat(16 * 1024 * 1024 + 1)).is_none());
}
