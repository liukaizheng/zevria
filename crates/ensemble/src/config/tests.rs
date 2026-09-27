use super::*;

#[test]
fn custom_agent_subsets_are_not_repaired() {
    let mut config: EnsembleConfig = toml::from_str(
        r#"
plan_agents = ["custom"]
review_agents = ["custom"]
[agents.custom]
label = "Custom"
command = "/custom/worker"
plan_mode = "plan"
review_mode = "review"
"#,
    )
    .unwrap();
    let custom = config.agents["custom"].clone();
    assert_eq!(config.plan_agents, ["custom"]);
    assert_eq!(config.review_agents, ["custom"]);
    assert_eq!(config.agents["custom"], custom);
    assert!(!config.agents.contains_key("codex"));
    assert!(!config.agents.contains_key("zevria"));
    config.validate().unwrap();
    config.agents.insert("zevria".into(), custom.clone());
    assert_eq!(config.agents["zevria"], custom);
}

#[test]
fn zevria_executable_resolution_is_distinctive_preserves_overrides_and_fails_closed() {
    let mut config = EnsembleConfig::default();
    let mut renamed = config.agents.remove("zevria").unwrap();
    renamed
        .env
        .insert("ZEVRIA_CONFIG".into(), "/custom/config.toml".into());
    config.agents.insert("renamed".into(), renamed.clone());
    config.plan_agents = vec!["renamed".into()];
    config
        .resolve_zevria_executable(|| Ok("/running/zevria".into()))
        .unwrap();
    renamed.command = "/running/zevria".into();
    #[cfg(windows)]
    renamed.args.extend(["--runtime".into(), "native".into()]);
    assert_eq!(config.agents["renamed"], renamed);
    let codex = config.agents["codex"].clone();
    let claude = config.agents["claude"].clone();
    config
        .resolve_zevria_executable(|| panic!("custom executable is not resolved"))
        .unwrap();
    assert_eq!(config.agents["codex"], codex);
    assert_eq!(config.agents["claude"], claude);
    let mut custom = EnsembleAgentConfig::built_in_zevria();
    custom.args.push("--custom".into());
    config.agents.insert("zevria".into(), custom.clone());
    config
        .resolve_zevria_executable(|| panic!("custom args are not resolved"))
        .unwrap();
    assert_eq!(config.agents["zevria"], custom);

    let mut opted_out = EnsembleConfig::default();
    opted_out.plan_agents = vec!["codex".into(), "claude".into()];
    opted_out.review_agents = opted_out.plan_agents.clone();
    opted_out
        .resolve_zevria_executable(|| {
            panic!("unselected worker must not require executable resolution")
        })
        .unwrap();
    assert_eq!(
        opted_out.agents["zevria"],
        EnsembleAgentConfig::built_in_zevria()
    );

    let mut defaults = EnsembleConfig::default();
    let error = defaults
        .resolve_zevria_executable(|| Err(std::io::Error::other("removed executable")))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("explicit ensemble agent executable")
    );
    assert_eq!(defaults.agents["zevria"].command, "zevria");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        let error = defaults
            .resolve_zevria_executable(|| Ok(std::ffi::OsString::from_vec(vec![0xff]).into()))
            .unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }
}

#[test]
fn missing_codex_settings_are_not_inserted_by_executable_resolution() {
    let mut config = EnsembleConfig::default();
    let claude = config.agents["claude"].clone();
    let codex = config.agents.get_mut("codex").unwrap();
    codex.env.clear();
    codex.plan_config_options.clear();
    assert!(config.agents["codex"].env.is_empty());
    assert!(config.agents["codex"].plan_config_options.is_empty());
    config.apply_resolved_codex_path("/resolved/codex");
    config.apply_resolved_codex_path("/different/codex");
    assert!(config.agents["codex"].env.is_empty());
    assert_eq!(config.agents["claude"], claude);
}

#[test]
fn ensemble_defaults_and_secret_debug_behavior_remain_typed() {
    let defaults = EnsembleConfig::default();
    assert_eq!(defaults.plan_agents, ["codex", "claude", "zevria"]);
    assert_eq!(defaults.review_agents, ["codex", "claude", "zevria"]);
    assert_eq!(defaults.max_synthesis_bytes_per_agent, 131_072);
    defaults.validate().expect("ensemble defaults");

    let agent = EnsembleAgentConfig {
        label: "Redacted".to_string(),
        command: "agent".to_string(),
        args: Vec::new(),
        plan_mode: None,
        review_mode: None,
        plan_config_options: BTreeMap::from([(
            "collaboration_mode".to_string(),
            "sensitive-plan-value".to_string(),
        )]),
        review_config_options: BTreeMap::new(),
        plan_handoff_transport: None,
        review_system_prompt_transport: None,
        env: BTreeMap::from([("API_TOKEN".to_string(), "super-secret-value".to_string())]),
        login_hint: String::new(),
    };
    let debug = format!("{agent:?}");
    assert!(debug.contains("API_TOKEN"));
    assert!(!debug.contains("super-secret-value"));
    assert!(!debug.contains("sensitive-plan-value"));
}
