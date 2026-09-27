use super::*;
use std::sync::{Mutex, OnceLock};

const REQUIRED_CONFIG: &str = r#"[modes]
build = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "medium" }
plan = { provider = "gateway", model = "vendor.model.v2", reasoning_level = "low" }
review = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "high" }
explore = { provider = "gateway", model = "vendor.model.v2", reasoning_level = "medium" }
builder = { provider = "openai", model = "gpt-5.6-sol", reasoning_level = "low" }
"#;
const REQUIRED_MODELS: &str = r#"{
  // Literal credentials and stable, case-sensitive catalog identities.
  "providers": {
    "openai": {
      "base_url": "https://api.openai.test/v1/responses",
      "api_key": "literal-openai-key", "supports_websockets": true,
      "models": { "gpt-5.6-sol": {
        "context_window_tokens": 272000, "retained_user_tokens": 20000,
        "reasoning_levels": ["low", "medium", "high"],
        "reasoning_summary_level": "detailed",
      } }
    },
    "gateway": {
      "base_url": "http://gateway.test/custom/responses",
      "api_key": "literal-gateway-key", "supports_websockets": false,
      "compatibility": { "send_reasoning": false, "send_reasoning_encrypted_content": false,
        "strict_tools": false, "send_prompt_cache_key": false, "send_store": false },
      "additional_params": { "gateway_routing": "pool-a" },
      "compaction": { "request_timeout_seconds": 17 },
      "models": { "vendor.model.v2": {
        "context_window_tokens": 128000, "input_token_limit": 100000, "retained_user_tokens": 10000,
        "reasoning_levels": ["low", "medium", "high"],
        "reasoning_summary_level": "concise"
      } }
    }
  }
}"#;

fn parse(extra: &str) -> anyhow::Result<Config> {
    Config::parse(&format!("{REQUIRED_CONFIG}\n{extra}"), REQUIRED_MODELS)
}
fn models_value() -> serde_json::Value {
    jsonc_parser::parse_to_serde_value(REQUIRED_MODELS, &MODELS_PARSE_OPTIONS).unwrap()
}
fn parse_value(models: &serde_json::Value) -> anyhow::Result<Config> {
    Config::parse(REQUIRED_CONFIG, &models.to_string())
}
fn environment_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("environment lock")
}

#[test]
fn theme_selector_is_name_only_strict_and_never_resolved_by_config_parsing() {
    assert!(parse("").unwrap().theme.is_none());
    assert_eq!(
        parse("[theme]\nname = 'not-installed'")
            .unwrap()
            .theme
            .unwrap()
            .name,
        "not-installed"
    );
    for selector in [
        "[theme]",
        "[theme]\nname = 'Ocean'",
        "[theme]\nname = '../path'",
        "[theme]\nname = 'ocean'\nbackground = '#000000'",
        "[theme]\nname = 'ocean.toml'",
        "[theme]\nname = 123",
        "[theme]\npath = '/tmp/theme'",
    ] {
        assert!(parse(selector).is_err(), "{selector}");
        crate::skills::parse_skill_config(selector).unwrap();
    }
}

#[test]
fn skill_settings_are_defaulted_strict_and_cannot_change_roots() {
    assert_eq!(parse("").unwrap().skills, SkillsConfig::default());
    for fields in [
        "roots = []",
        "global_path = '/tmp'",
        "extra_roots = []",
        "[skills.roots]\nproject = '/tmp'",
        "catalog_max_tokens = 0",
    ] {
        let source = format!("[skills]\n{fields}");
        assert!(parse(&source).is_err(), "{source}");
        assert!(crate::skills::parse_skill_config(&source).is_err());
    }
    let source = "[skills]\nenabled = true\n[[skills.rules]]\nname = 'review'\nenabled = false";
    let config = parse(source).unwrap();
    assert!(config.skills.enabled);
    assert!(
        !config
            .skills
            .name_enabled(&zevria_instructions::SkillName::parse("review").unwrap())
    );
    assert_eq!(
        config.skills,
        crate::skills::parse_skill_config(source).unwrap()
    );
}

#[test]
fn required_provider_catalog_and_independent_modes_resolve() {
    use zevria_foundation::{ModelProfileRef, ModelRole};
    let config = parse("").unwrap();
    assert_eq!(config.session, SessionConfig::default());
    assert_eq!(config.acp, AcpConfig::default());
    assert_eq!(config.command.timeout_seconds, 300);
    assert!(
        config.providers["openai"]
            .compatibility
            .send_reasoning_encrypted_content
    );
    assert!(
        !config.providers["gateway"]
            .compatibility
            .send_reasoning_encrypted_content
    );
    assert_eq!(
        config.routing().for_role(ModelRole::Build).profile,
        ModelProfileRef::new("openai", "gpt-5.6-sol")
    );
    assert_eq!(
        config.routing().for_role(ModelRole::Plan).profile,
        ModelProfileRef::new("gateway", "vendor.model.v2")
    );
    let plan = config.compaction_policy().for_role(ModelRole::Plan);
    assert_eq!(plan.context_window_tokens, 128_000);
    assert_eq!(plan.input_token_limit, 100_000);
    let build = config.compaction_policy().for_role(ModelRole::Build);
    assert_eq!(build.input_token_limit, 272_000);
    assert_eq!(build.retained_user_tokens, 20_000);
}

#[test]
fn session_id_header_is_optional_provider_scoped_and_strict() {
    assert!(
        parse("")
            .unwrap()
            .providers
            .values()
            .all(|p| p.session_id_header.is_none())
    );
    for name in ["x-opencode-session", "X-Conversation-ID", "session-id"] {
        let mut models = models_value();
        models["providers"]["gateway"]["session_id_header"] = name.into();
        let config = parse_value(&models).unwrap();
        assert_eq!(
            config.providers["gateway"].session_id_header.as_deref(),
            Some(name)
        );
        assert_eq!(
            config
                .routing()
                .for_role(zevria_foundation::ModelRole::Plan)
                .endpoint
                .session_id_header
                .as_deref(),
            Some(name)
        );
        assert!(
            config
                .routing()
                .for_role(zevria_foundation::ModelRole::Build)
                .endpoint
                .session_id_header
                .is_none()
        );
        assert!(
            !config.providers["gateway"]
                .compatibility
                .send_prompt_cache_key
        );
    }
    for invalid in [
        serde_json::json!(""),
        serde_json::json!(" "),
        serde_json::json!("bad:name"),
        serde_json::json!(" Authorization"),
        serde_json::json!("Authorization"),
        serde_json::json!("SEC-WEBSOCKET-KEY"),
        serde_json::json!("x-会话"),
        serde_json::json!(false),
        serde_json::json!(42),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        let mut models = models_value();
        models["providers"]["gateway"]["session_id_header"] = invalid.clone();
        assert!(parse_value(&models).is_err(), "accepted {invalid}");
    }
    for key in ["session_id_header", "send_opencode_session"] {
        let mut models = models_value();
        models["providers"]["gateway"]["compatibility"][key] = true.into();
        assert!(parse_value(&models).is_err());
    }
}

#[test]
fn providers_and_modes_are_required_without_catalog_fallback() {
    let mut models = models_value();
    models.as_object_mut().unwrap().remove("providers");
    assert!(format!("{:#}", parse_value(&models).unwrap_err()).contains("providers"));
    assert!(
        Config::parse("", REQUIRED_MODELS)
            .unwrap_err()
            .to_string()
            .contains("modes")
    );
    let config: toml::Table = toml::from_str(REQUIRED_CONFIG).unwrap();
    for role in ["build", "plan", "review", "explore", "builder"] {
        let mut missing = config.clone();
        missing["modes"].as_table_mut().unwrap().remove(role);
        assert!(
            format!(
                "{:#}",
                Config::parse(&toml::to_string(&missing).unwrap(), REQUIRED_MODELS).unwrap_err()
            )
            .contains(role)
        );
        for field in ["provider", "model", "reasoning_level"] {
            let mut missing = config.clone();
            missing["modes"][role].as_table_mut().unwrap().remove(field);
            let error =
                Config::parse(&toml::to_string(&missing).unwrap(), REQUIRED_MODELS).unwrap_err();
            assert!(format!("{error:#}").contains(field), "{error:#}");
        }
        for (field, value) in [
            ("provider", "unknown"),
            ("model", "unknown"),
            ("reasoning_level", "max"),
        ] {
            let mut invalid = config.clone();
            invalid["modes"][role][field] = value.into();
            let error =
                Config::parse(&toml::to_string(&invalid).unwrap(), REQUIRED_MODELS).unwrap_err();
            assert!(
                format!("{error:#}").contains(&format!("modes.{role}")),
                "{error:#}"
            );
        }
    }
}

#[test]
fn unrelated_partial_sections_still_use_defaults() {
    let config = parse(
        "[session]\nevent_queue_capacity = 7\n[session.compaction]\nauto_trigger_percent = 80",
    )
    .unwrap();
    assert_eq!(config.session.event_queue_capacity, 7);
    assert_eq!(config.session.max_concurrent_subtasks, 10);
    assert_eq!(config.session.compaction.auto_trigger_percent, 80);
    assert_eq!(config.log.level, "info");
}

#[test]
fn removed_model_call_limit_is_unknown_for_positive_and_zero_values() {
    let generated: toml::Value = toml::from_str(DEFAULT_CONFIG).unwrap();
    let session: SessionConfig = generated["session"].clone().try_into().unwrap();
    assert_eq!(session, SessionConfig::default());
    session.validate().unwrap();
    let serialized = toml::to_string(&session).unwrap();
    assert!(!serialized.contains("max_model_calls"));
    assert!(!DEFAULT_CONFIG.contains("max_model_calls"));
    assert_eq!(
        toml::from_str::<SessionConfig>(&serialized).unwrap(),
        session
    );
    for value in [0, 7, 9999] {
        let error = parse(&format!("[session]\nmax_model_calls = {value}")).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown field `max_model_calls`"),
            "{error:#}"
        );
    }
}

#[test]
fn legacy_openai_and_removed_session_context_fields_fail_strictly() {
    for legacy in [
        "[openai]\nbase_url='https://legacy.test/responses'",
        "[openai.models]\nbuild='legacy'",
        "[openai.reasoning]\neffort='low'",
        "[session.compaction]\ncontext_window_tokens=1000",
        "[session.compaction]\nretained_user_tokens=100",
    ] {
        assert!(parse(legacy).is_err(), "{legacy}");
    }
    let moved = "[providers]";
    assert!(
        parse(moved)
            .unwrap_err()
            .to_string()
            .contains("moved to models.jsonc next to this file")
    );
    assert!(
        crate::skills::parse_skill_config(moved)
            .unwrap_err()
            .to_string()
            .contains("moved to models.jsonc")
    );
}

#[test]
fn provider_typos_and_reserved_additional_parameters_fail() {
    let typo = REQUIRED_MODELS.replace("supports_websockets", "supports_websocket");
    assert!(
        format!("{:#}", Config::parse(REQUIRED_CONFIG, &typo).unwrap_err())
            .contains("supports_websocket")
    );
    for key in ["model", "include"] {
        let mut models = models_value();
        models["providers"]["gateway"]["additional_params"][key] = "override".into();
        assert!(
            format!("{:#}", parse_value(&models).unwrap_err())
                .contains(&format!("providers.gateway.additional_params.{key}"))
        );
    }
}

#[test]
fn jsonc_accepts_comments_and_trailing_commas_but_not_loose_syntax() {
    parse("").unwrap();
    for (from, to) in [
        ("\"providers\":", "providers:"),
        ("\"providers\":", "'providers':"),
        ("272000,", "272000"),
        ("272000", "0x42680"),
        ("272000", "+272000"),
    ] {
        assert!(
            Config::parse(REQUIRED_CONFIG, &REQUIRED_MODELS.replacen(from, to, 1)).is_err(),
            "{to}"
        );
    }
}

#[test]
fn duplicate_jsonc_keys_including_escaped_equivalents_are_rejected_before_editing() {
    for (from, to) in [
        ("\"providers\":", "\"providers\": {}, \"providers\":"),
        (
            "\"api_key\":",
            "\"api_key\": \"first-secret\", \"api_key\":",
        ),
        (
            "\"reasoning_levels\":",
            r#""reasoning_lev\u0065ls": ["low"], "reasoning_levels":"#,
        ),
        (
            "\"gateway_routing\":",
            "\"gateway_routing\": [{\"same\": 1, \"same\": 2}], \"other\":",
        ),
    ] {
        let error =
            Config::parse(REQUIRED_CONFIG, &REQUIRED_MODELS.replacen(from, to, 1)).unwrap_err();
        assert!(
            format!("{error:#}").contains("duplicate property"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("first-secret"));
    }
}

#[test]
fn reasoning_levels_are_required_nonempty_unique_and_support_assignments() {
    for levels in [
        None,
        Some(serde_json::json!([])),
        Some(serde_json::json!(["medium", "medium"])),
        Some(serde_json::json!(["low", "high"])),
    ] {
        let mut models = models_value();
        let model = models["providers"]["openai"]["models"]["gpt-5.6-sol"]
            .as_object_mut()
            .unwrap();
        match levels {
            Some(levels) => {
                model.insert("reasoning_levels".into(), levels);
            }
            None => {
                model.remove("reasoning_levels");
            }
        }
        assert!(format!("{:#}", parse_value(&models).unwrap_err()).contains("reasoning_levels"));
    }
}

#[test]
fn environment_api_key_override_is_removed() {
    let _guard = environment_lock();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, REQUIRED_CONFIG).unwrap();
    std::fs::write(models_path_for(&path), REQUIRED_MODELS).unwrap();
    // SAFETY: serialized by environment_lock and restored before release.
    unsafe {
        std::env::set_var("ZEVRIA_CONFIG", &path);
        std::env::set_var("ZEVRIA_API_KEY", "environment-secret");
    }
    let config = Config::load();
    unsafe {
        std::env::remove_var("ZEVRIA_API_KEY");
        std::env::remove_var("ZEVRIA_CONFIG");
    }
    assert_eq!(
        serde_json::to_value(&config.unwrap().providers["openai"]).unwrap()["api_key"],
        "literal-openai-key"
    );
}

#[test]
fn default_templates_use_requested_modes_models_and_search_setting() {
    let config: ConfigFile = toml::from_str(DEFAULT_CONFIG).unwrap();
    for (assignment, expected_model) in [
        (&config.modes.plan, "gpt-6-astra"),
        (&config.modes.build, "gpt-6-astra"),
        (&config.modes.review, "gpt-6-sol"),
        (&config.modes.explore, "gpt-6-luna"),
        (&config.modes.builder, "gpt-6-luna"),
    ] {
        assert_eq!(assignment.provider, "openai");
        assert_eq!(assignment.model, expected_model);
        assert_eq!(
            assignment.reasoning_level,
            zevria_foundation::ReasoningLevel::Max
        );
    }

    assert!(DEFAULT_MODELS.contains(r#""enabled": true, "external_web_access": true"#));
    let reasoning_levels = r#""reasoning_levels": ["low", "medium", "high", "xhigh", "max"]"#;
    assert_eq!(DEFAULT_MODELS.matches(reasoning_levels).count(), 3);
    for model in ["gpt-6-luna", "gpt-6-sol", "gpt-6-astra"] {
        assert!(
            DEFAULT_MODELS.contains(&format!(r#"//       "{model}": {{"#)),
            "missing model example {model}"
        );
    }
    for metadata in [
        r#""context_window_tokens": 272000"#,
        r#""input_token_limit": 272000"#,
        r#""retained_user_tokens": 20000"#,
        r#""reasoning_summary_level": "detailed""#,
    ] {
        assert_eq!(DEFAULT_MODELS.matches(metadata).count(), 3, "{metadata}");
    }
}

#[test]
fn first_run_creates_both_private_skeletons_and_missing_models_alone_stops() {
    let _guard = environment_lock();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/work.toml");
    let models_path = models_path_for(&path);
    // SAFETY: serialized by environment_lock and restored before release.
    unsafe {
        std::env::set_var("ZEVRIA_CONFIG", &path);
    }
    let error = Config::load().unwrap_err();
    unsafe {
        std::env::remove_var("ZEVRIA_CONFIG");
    }
    for file in [&path, &models_path] {
        assert!(error.to_string().contains(&file.display().to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    assert!(
        error
            .to_string()
            .contains("created the Zevria configuration skeleton")
    );
    assert!(error.to_string().contains("models.jsonc"));
    assert!(error.to_string().contains("start Zevria again"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
    assert_eq!(
        std::fs::read_to_string(&models_path).unwrap(),
        DEFAULT_MODELS
    );
    assert!(toml::from_str::<ConfigFile>(DEFAULT_CONFIG).is_ok());
    let skeleton: serde_json::Value =
        jsonc_parser::parse_to_serde_value(DEFAULT_MODELS, &MODELS_PARSE_OPTIONS).unwrap();
    assert_eq!(skeleton, serde_json::json!({}));
    assert!(
        parse_models(DEFAULT_MODELS, &models_path)
            .unwrap_err()
            .to_string()
            .contains(&models_path.display().to_string())
    );
    assert!(DEFAULT_MODELS.contains("replace-with-api-key"));
    assert!(DEFAULT_MODELS.contains("x-opencode-session"));
    assert!(DEFAULT_MODELS.contains("\"reasoning_levels\""));
    std::fs::write(
        &path,
        "# preserve my config\n[session]\nevent_queue_capacity=7\n",
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    std::fs::remove_file(&models_path).unwrap();
    unsafe {
        std::env::set_var("ZEVRIA_CONFIG", &path);
    }
    let error = Config::load().unwrap_err();
    unsafe {
        std::env::remove_var("ZEVRIA_CONFIG");
    }
    assert!(!error.to_string().contains(&path.display().to_string()));
    assert!(
        error
            .to_string()
            .contains(&models_path.display().to_string())
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(models_path).unwrap(),
        DEFAULT_MODELS
    );
}

#[test]
fn api_keys_and_endpoint_secrets_are_absent_from_debug() {
    let debug = format!("{:?}", parse("").unwrap());
    for secret in ["literal-openai-key", "literal-gateway-key"] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("<redacted>"));
}

#[test]
fn invalid_session_acp_command_and_ensemble_limits_fail_loading() {
    for invalid in [
        "[session]\nevent_queue_capacity=0",
        "[session]\nmax_concurrent_subtasks=0",
        "[acp]\nmax_sessions=0",
        "[command]\ntimeout_seconds=0",
        "[command]\ncapture_bytes=1",
        "[ensemble]\nmax_concurrent_agents=0",
    ] {
        assert!(parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn generated_ensemble_defaults_match_the_in_memory_definition() {
    let source: toml::Value = toml::from_str(DEFAULT_CONFIG).unwrap();
    let generated: EnsembleConfig = source["ensemble"].clone().try_into().unwrap();
    assert_eq!(generated, EnsembleConfig::default());
    assert_eq!(generated.agents["zevria"], zevria_ensemble::config::EnsembleAgentConfig {
        label: "Zevria".into(), command: "zevria".into(), args: vec!["--acp".into(), "--ensemble-worker".into()],
        plan_mode: Some("plan".into()), review_mode: Some("review".into()),
        login_hint: "Configure Zevria's providers in models.jsonc and all five mode assignments with reasoning_level in config.toml before running an ensemble.".into(),
        ..Default::default()
    });
    assert_eq!(generated.max_concurrent_agents, 4);
}

#[test]
fn selections_missing_from_a_supplied_agent_map_are_rejected() {
    let error = parse("[ensemble]\nplan_agents=['zevria']\nreview_agents=['zevria']\n[ensemble.agents.old]\nlabel='Old'\ncommand='old'").unwrap_err();
    assert!(error.to_string().contains("zevria"));
}

#[test]
fn obsolete_catalog_assignments_and_model_defaults_are_rejected_but_offline_modes_are_allowed() {
    let mut models = models_value();
    models["modes"] = serde_json::json!({});
    assert!(format!("{:#}", parse_value(&models).unwrap_err()).contains("unknown field `modes`"));
    let mut models = models_value();
    models["providers"]["openai"]["models"]["gpt-5.6-sol"]["reasoning_level"] = "medium".into();
    assert!(
        format!("{:#}", parse_value(&models).unwrap_err())
            .contains("unknown field `reasoning_level`")
    );
    crate::skills::parse_skill_config("[modes]\nplan = { model = 'offline-incomplete' }").unwrap();
    let config = parse("").unwrap();
    use zevria_foundation::{ModelRole, ReasoningLevel as Level};
    assert_eq!(
        config
            .routing()
            .selection_for_role(ModelRole::Build)
            .reasoning_level,
        Level::Medium
    );
    assert_eq!(
        config
            .routing()
            .selection_for_role(ModelRole::Review)
            .reasoning_level,
        Level::High
    );
    assert_eq!(
        config
            .routing()
            .selection_for_role(ModelRole::Builder)
            .reasoning_level,
        Level::Low
    );
    assert_eq!(
        config.routing().for_role(ModelRole::Build).profile,
        config.routing().for_role(ModelRole::Review).profile
    );
}
