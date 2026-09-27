use super::*;
use fingerprint::*;
use serde_json::{Value, json};

fn items(input: &[Value]) -> Vec<ItemFingerprint> {
    input.iter().map(ItemFingerprint::new).collect()
}

#[test]
fn canonical_keys_but_exact_arrays_strings_arguments_and_opaque_native_values() {
    let a: Value = serde_json::from_str(r#"{"b":{"z":2,"a":1},"a":[2,1]}"#).unwrap();
    let b: Value = serde_json::from_str(r#"{"a":[2,1],"b":{"a":1,"z":2}}"#).unwrap();
    assert_eq!(fingerprint(&a), fingerprint(&b));
    assert_ne!(
        fingerprint(&a),
        fingerprint(&json!({"a":[1,2],"b":{"a":1,"z":2}}))
    );
    let native = json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque-exact", "unknown":[null, true, {"k":1}]});
    for (key, value) in [
        ("encrypted_content", json!("opaque-other")),
        ("id", json!("rs_2")),
        ("unknown", json!([true, null, {"k":1}])),
    ] {
        let mut changed = native.clone();
        changed[key] = value;
        assert_ne!(fingerprint(&native), fingerprint(&changed));
    }
    assert_ne!(
        fingerprint(&json!({"arguments":"{\"n\":1}"})),
        fingerprint(&json!({"arguments":"{ \"n\":1.0 }"}))
    );
    assert_ne!(fingerprint(&json!("a\nb")), fingerprint(&json!("a\\nb")));
    assert_eq!(
        ItemFingerprint::new(&json!({"type":"UNTRUSTED_SECRET_TYPE"})).kind,
        ItemKind::Other
    );
}

#[test]
fn completed_input_plus_native_output_is_the_baseline_not_the_whole_input_hash() {
    let properties = Properties::new(&json!({"instructions":"fixed", "tools":[], "model":"test"}));
    let old = items(&[
        json!({"role":"user", "content":"first"}),
        json!({"type":"reasoning", "encrypted_content":"opaque"}),
        json!({"role":"assistant", "content":"answer"}),
    ]);
    let mut next = old.clone();
    next.push(ItemFingerprint::new(
        &json!({"role":"user", "content":"second"}),
    ));
    assert_ne!(ordered(&old), ordered(&next));
    let extension = compare(Some((&old, properties)), &next, properties);
    assert_eq!(extension.status, "exact_extension");
    assert_eq!(extension.baseline_count, Some(3));
    assert_eq!(extension.matched_count, 3);
    assert_eq!(extension.first_difference, None);
    assert_eq!(extension.instructions_changed, Some(false));
    assert_eq!(
        compare(Some((&old, properties)), &old, properties).status,
        "exact_equal"
    );
    next[1] = ItemFingerprint::new(&json!({"type":"reasoning", "encrypted_content":"mutated"}));
    let changed = compare(Some((&old, properties)), &next, properties);
    assert_eq!(changed.status, "mismatch");
    assert_eq!(changed.first_difference, Some(1));
    assert_eq!(changed.previous_kind, Some(ItemKind::Reasoning));
    assert_eq!(changed.next_kind, Some(ItemKind::Reasoning));
    let truncated = compare(Some((&old, properties)), &old[..2], properties);
    assert_eq!(truncated.status, "truncated");
    assert_eq!(truncated.first_difference, Some(2));
    assert_eq!(truncated.matched_count, 2);
    assert_eq!(truncated.next_kind, None);
    let missing = compare(None, &old, properties);
    assert_eq!(missing.status, "unknown");
    assert_eq!(missing.baseline_count, None);
    assert_eq!(missing.instructions_changed, None);
}

#[test]
fn request_properties_have_independent_comparison_categories() {
    let original = json!({"instructions":"fixed", "tools":[{"name":"one"},{"name":"two"}], "model":"test", "reasoning":{"effort":"high"}});
    let properties = Properties::new(&original);
    for (key, value, expected) in [
        ("instructions", json!("new"), (true, false, false)),
        (
            "tools",
            json!([{"name":"two"},{"name":"one"}]),
            (false, true, false),
        ),
        ("reasoning", json!({"effort":"low"}), (false, false, true)),
    ] {
        let mut changed = original.clone();
        changed[key] = value;
        let comparison = compare(Some((&[], properties)), &[], Properties::new(&changed));
        assert_eq!(comparison.status, "exact_equal");
        assert_eq!(
            (
                comparison.instructions_changed.unwrap(),
                comparison.tools_changed.unwrap(),
                comparison.properties_changed.unwrap()
            ),
            expected
        );
    }
    assert_ne!(
        Properties::new(&json!({})),
        Properties::new(&json!({"instructions":null}))
    );
}

#[test]
fn comparison_projection_does_not_hide_cache_sensitive_changes_or_wire_differences() {
    let ordinary = json!({"model":"synthetic", "instructions":"fixed", "tools":[]});
    let mut compared = ordinary.clone();
    compared["prompt_cache_options"] = json!({"comparison_response_id":"resp_one"});
    assert_eq!(Properties::new(&ordinary), Properties::new(&compared));
    assert_ne!(fingerprint(&ordinary), fingerprint(&compared));
    let first_wire = fingerprint(&compared);
    compared["prompt_cache_options"]["comparison_response_id"] = json!("resp_two");
    assert_eq!(Properties::new(&ordinary), Properties::new(&compared));
    assert_ne!(first_wire, fingerprint(&compared));
    for options in [
        json!({}),
        json!({"comparison_response_id":"resp_two", "mode":"explicit"}),
        json!({"comparison_response_id":"resp_two", "ttl":"30m"}),
        Value::Null,
    ] {
        compared["prompt_cache_options"] = options;
        assert_ne!(
            Properties::new(&ordinary).remaining,
            Properties::new(&compared).remaining
        );
    }
}

#[test]
fn socket_sidecar_keeps_bounded_cause_without_changing_terminal_precedence() {
    use crate::websocket_session::OpenAiWebSocketTerminalCategory as Category;
    let observation = socket::SocketDiagnostics::default();
    assert!(observation.observation().is_none());
    let error = tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "SECRET_UNDERLYING_ERROR_BODY",
    ));
    observation.error(Category::ReadError, &error);
    let first = observation.observation().unwrap();
    observation.finished(Category::InboundOverflow);
    let final_observation = observation.observation().unwrap();
    assert_eq!(final_observation.at, first.at);
    assert_eq!(final_observation.category, Category::InboundOverflow);
    assert_eq!(final_observation.error_class, Some("io"));
    assert_eq!(
        final_observation.io_kind,
        Some(std::io::ErrorKind::ConnectionReset)
    );
    assert!(!format!("{final_observation:?}").contains("SECRET"));
    let inferred = socket::SocketDiagnostics::disconnected(Category::StartupConnectFailed)
        .observation()
        .unwrap();
    assert_eq!(inferred.at, None);
    assert_eq!(inferred.source, "inferred");
    assert_eq!(identifier("body\nsecret"), "<redacted>");
    assert_eq!(identifier(&"x".repeat(129)), "<redacted>");
}

#[tokio::test]
async fn late_and_duplicate_comparison_cannot_overwrite_current_request() {
    let mut provider = crate::tests::connect_http_test_provider(
        "http://127.0.0.1:9/responses".into(),
        rig_agent::tool::server::ToolServer::new().run(),
    )
    .await;
    // No socket traffic: exercise the bounded ownership ledger directly.
    provider.transport = OpenAiTransport::WebSocket;
    let event = |id: &str, outcome: &str| {
        json!({"type":"response.completed", "response":{"id":id, "prompt_cache_diagnostics":{"type":outcome}}}).to_string()
    };
    provider.cache_diagnostics.current_request = Some(10);
    observe_raw(
        &mut provider,
        &event("resp_previous", "cache_hit"),
        None,
        None,
    );
    provider.cache_diagnostics.current_request = Some(20);
    provider.cache_diagnostics.raw = None;
    observe_raw(
        &mut provider,
        &event("resp_current", "unavailable"),
        None,
        None,
    );
    let current = provider.cache_diagnostics.raw.clone();
    observe_raw(
        &mut provider,
        &event("resp_previous", "cache_miss"),
        None,
        None,
    );
    assert_eq!(provider.cache_diagnostics.raw, current);
    let mut duplicate: Value = serde_json::from_str(&event("resp_current", "cache_hit")).unwrap();
    duplicate["type"] = json!("response.done");
    observe_raw(&mut provider, &duplicate.to_string(), None, None);
    assert_eq!(provider.cache_diagnostics.raw, current);
    assert_eq!(provider.cache_diagnostics.recent.len(), 2);
    assert_eq!(provider.cache_diagnostics.recent[0].request_id, 10);
    assert_eq!(provider.cache_diagnostics.recent[1].request_id, 20);
}

#[test]
fn raw_counter_states_and_echoes_survive_without_sdk_defaulting_or_content() {
    use response::{Counter, Echo, Observation, Usage};
    for (details, expected) in [
        (json!({}), Counter::Missing),
        (json!({"cached_tokens":null}), Counter::Null),
        (
            json!({"cached_tokens":"SECRET_BAD_COUNTER"}),
            Counter::Malformed,
        ),
        (json!({"cached_tokens":-1}), Counter::Malformed),
        (json!({"cached_tokens":0}), Counter::Present(0)),
        (json!({"cached_tokens":14848}), Counter::Present(14848)),
    ] {
        let event = json!({"type":"response.completed", "response": {
            "id":"unsafe\nPRIVATE_RESPONSE", "model":"unsafe?PRIVATE_MODEL", "status":"completed", "service_tier":"default",
            "instructions":"PRIVATE_INSTRUCTIONS", "tools":[{"name":"PRIVATE_TOOL"}], "prompt_cache_key":"PRIVATE_KEY",
            "usage":{"input_tokens":15979,"output_tokens":100,"total_tokens":16079,"input_tokens_details":details},
            "cookies":"PRIVATE_COOKIE", "authorization":"PRIVATE_CREDENTIAL", "output":[{"encrypted_content":"PRIVATE_ENCRYPTED"}]
        }});
        let raw = Observation::parse(&event.to_string()).unwrap();
        assert_eq!(raw.cached, expected);
        assert_eq!(raw.response_id.as_deref(), Some("<redacted>"));
        assert_eq!(
            raw.instructions,
            Echo::Present(Properties::new(&event["response"]).instructions)
        );
        assert!(raw.valid());
        assert_eq!(
            raw.projection_differs(
                Some(Usage {
                    input: 15979,
                    cached: expected.value().unwrap_or(0),
                    output: 100,
                    total: 16079
                }),
                None
            ),
            expected.value().is_none()
        );
        let encoded = serde_json::to_string(&raw).unwrap();
        assert!(
            !encoded.contains("PRIVATE_") && !encoded.contains("SECRET_"),
            "{encoded}"
        );
    }
    for (usage, expected) in [
        (Value::Null, Counter::Null),
        (json!(false), Counter::Malformed),
        (json!({}), Counter::Missing),
    ] {
        let raw = Observation::parse(
            &json!({"type":"response.done", "response":{"usage":usage}}).to_string(),
        )
        .unwrap();
        assert_eq!(raw.cached, expected);
    }
}
