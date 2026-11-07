use super::*;
use serde_json::{Value, json};

fn native_models() -> Value {
    let mut models = models_value();
    models["providers"]["claude"] = json!({"protocol":"anthropic.messages","base_url":"https://api.anthropic.com/v1/messages","api_key":"literal-claude-key","models":{"opus":{"context_window_tokens":1000000,"max_output_tokens":64000,"retained_user_tokens":20000,"reasoning_levels":["low","medium","high","xhigh","max"],"thinking_display":"summarized","refusal_fallback":"default"}}});
    models
}
#[test]
fn mixed_catalog_defaults_and_tagged_round_trip() {
    let config = parse_value(&native_models()).unwrap();
    assert_eq!(
        config
            .routing()
            .for_role(zevria_foundation::ModelRole::Build)
            .protocol
            .as_str(),
        "openai.responses"
    );
    let native = config
        .routing()
        .catalog()
        .find(|p| p.profile.provider == "claude")
        .unwrap();
    assert_eq!(native.protocol.as_str(), "anthropic.messages");
    assert_eq!(native.input_token_limit, 936000);
    let serialized = serde_json::to_value(&config.providers["openai"]).unwrap();
    assert_eq!(serialized["protocol"], "openai.responses");
    let round: ProviderConfig =
        serde_json::from_value(serde_json::to_value(&config.providers["claude"]).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(round).unwrap(),
        serde_json::to_value(&config.providers["claude"]).unwrap()
    );
}
#[test]
fn protocol_specific_strict_schemas_and_missing_max_output() {
    for (key, value) in [
        ("supports_websockets", json!(true)),
        ("compaction", json!({})),
        ("web_search", json!({})),
        ("typo", json!(true)),
    ] {
        let mut models = native_models();
        models["providers"]["claude"][key] = value;
        let error = parse_value(&models).unwrap_err();
        assert!(format!("{error:#}").contains("providers.claude"));
    }
    for (key, value) in [
        ("reasoning_summary_level", json!("detailed")),
        ("unknown", json!(true)),
    ] {
        let mut models = native_models();
        models["providers"]["claude"]["models"]["opus"][key] = value;
        assert!(parse_value(&models).is_err());
    }
    let mut models = native_models();
    models["providers"]["claude"]["models"]["opus"]
        .as_object_mut()
        .unwrap()
        .remove("max_output_tokens");
    assert!(parse_value(&models).is_err());
    for protocol in [json!("unknown"), Value::Null, json!(2)] {
        let mut models = native_models();
        models["providers"]["claude"]["protocol"] = protocol;
        assert!(parse_value(&models).is_err());
    }
    let mut models = models_value();
    models["providers"]["openai"]["auth"] = "bearer".into();
    assert!(parse_value(&models).is_err());
}
#[test]
fn anthropic_limits_reasoning_betas_and_reserved_params() {
    for levels in [
        json!([]),
        json!(["none"]),
        json!(["minimal"]),
        json!(["low", "low"]),
    ] {
        let mut models = native_models();
        models["providers"]["claude"]["models"]["opus"]["reasoning_levels"] = levels;
        assert!(parse_value(&models).is_err());
    }
    for (key, value) in [
        ("max_output_tokens", 0),
        ("max_output_tokens", 1000000),
        ("input_token_limit", 936001),
        ("input_token_limit", 0),
    ] {
        let mut models = native_models();
        models["providers"]["claude"]["models"]["opus"][key] = value.into();
        assert!(parse_value(&models).is_err());
    }
    for key in [
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "stream",
        "max_tokens",
        "thinking",
        "output_config",
        "cache_control",
        "fallbacks",
        "temperature",
        "top_p",
        "top_k",
        "context_management",
    ] {
        let mut models = native_models();
        models["providers"]["claude"]["additional_params"] = json!({key:true});
        assert!(parse_value(&models).is_err(), "{key}");
    }
    for beta in ["", "bad beta", "a,b", "injected\r\n"] {
        let mut models = native_models();
        models["providers"]["claude"]["beta_headers"] = json!([beta]);
        assert!(parse_value(&models).is_err());
    }
    for url in [
        "wss://gateway.test/messages",
        "ftp://gateway.test/messages",
        "not a URL",
    ] {
        let mut models = native_models();
        models["providers"]["claude"]["base_url"] = url.into();
        assert!(parse_value(&models).is_err());
    }
    let mut models = native_models();
    models["providers"]["claude"]["models"]["opus"]["input_token_limit"] = 936000.into();
    assert!(parse_value(&models).is_ok());
}
#[test]
fn commented_claude_template_is_a_valid_configured_catalog() {
    let uncommented = DEFAULT_MODELS
        .lines()
        .map(|line| line.replacen("// ", "", 1))
        .collect::<Vec<_>>()
        .join("\n");
    // The introductory prose is not part of the object.
    let json = &uncommented[uncommented.find('{').unwrap()..];
    let models = parse_models(json, Path::new("models.jsonc")).unwrap();
    let mut modes: ConfigFile = toml::from_str(DEFAULT_CONFIG).unwrap();
    modes.modes.build = zevria_provider::ModelAssignment {
        provider: "anthropic".into(),
        model: "claude-opus-5-5".into(),
        reasoning_level: zevria_foundation::ReasoningLevel::High,
    };
    modes.modes.plan = modes.modes.build.clone();
    let config = Config::from_files(modes, models).unwrap();
    assert_eq!(
        config
            .routing()
            .for_role(zevria_foundation::ModelRole::Build)
            .protocol
            .as_str(),
        "anthropic.messages"
    );
}
