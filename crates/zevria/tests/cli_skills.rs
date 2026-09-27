use std::{
    path::Path,
    process::{Command, Output},
};

fn cli(home: &Path, workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args(["--runtime", "native"])
        .args(args)
        .env("HOME", home)
        .env_remove("ZEVRIA_CONFIG")
        .current_dir(workspace)
        .output()
        .unwrap()
}
fn install(root: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join(format!("{name}.md")),
        format!("---\ndescription: {name} description\n---\n{body}"),
    )
    .unwrap();
}
fn view(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::str::from_utf8(&output.stdout).unwrap().lines().count(),
        1
    );
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(view.get("next_cursor").is_none());
    view
}

#[test]
fn skills_offline_read_commands_need_no_config_providers_or_network() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    install(
        &home.path().join(".zevria/skills"),
        "review",
        "GLOBAL BODY SECRET",
    );
    install(
        &workspace.path().join(".zevria/skills"),
        "review",
        "PROJECT BODY SECRET",
    );
    let output = cli(home.path(), workspace.path(), &["skills", "list", "--json"]);
    let listed = view(&output);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
    assert_eq!(listed["completions"].as_array().unwrap().len(), 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("BODY SECRET"));
    assert!(!home.path().join(".zevria/config.toml").exists());
    assert!(listed["entries"][0].get("source_id").is_none());
    let inspected = cli(
        home.path(),
        workspace.path(),
        &["skills", "inspect", "review", "--json"],
    );
    assert_eq!(view(&inspected)["entries"].as_array().unwrap().len(), 2);
    let valid = cli(
        home.path(),
        workspace.path(),
        &["skills", "validate", ".zevria/skills/review.md", "--json"],
    );
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&valid.stdout).unwrap()["valid"],
        true
    );
    assert!(!home.path().join(".zevria/config.toml").exists());
    assert!(
        !cli(
            home.path(),
            workspace.path(),
            &["skills", "enable", "review"]
        )
        .status
        .success()
    );
    assert!(!home.path().join(".zevria/config.toml").exists());
}

#[test]
fn skills_json_listing_returns_one_complete_view_beyond_old_page_limits() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().join(".zevria/skills");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..40 {
        std::fs::write(
            root.join(format!("demo-{i:02}.md")),
            format!(
                "---\ndescription: {}\n---\nPRIVATE BODY\n",
                "Matching metadata ".repeat(60)
            ),
        )
        .unwrap();
    }
    let output = cli(home.path(), workspace.path(), &["skills", "list", "--json"]);
    let listed = view(&output);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 40);
    assert_eq!(listed["completions"].as_array().unwrap().len(), 40);
    assert!(
        output.stdout.len() > 32 * 1024,
        "management has no page-byte ceiling"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE BODY"));
}

#[test]
fn skills_config_path_does_not_change_roots_and_writes_preserve_unrelated_settings() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let custom = tempfile::tempdir().unwrap();
    install(
        &workspace.path().join(".zevria/skills"),
        "review",
        "WORKSPACE BODY",
    );
    install(
        &custom.path().join("skills"),
        "not-native",
        "DO NOT DISCOVER",
    );
    let path = custom.path().join("custom.toml");
    let before = "# Private settings\n[ensemble.agents.incomplete.env]\napi_key = 'SECRET' # untouched\n[skills]\nenabled = true # master capability\n";
    std::fs::write(&path, before).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_zevria"))
            .args(["--runtime", "native"])
            .args(args)
            .env("HOME", home.path())
            .env("ZEVRIA_CONFIG", &path)
            .current_dir(workspace.path())
            .output()
            .unwrap()
    };
    let initial = view(&run(&["skills", "list", "--json"]));
    assert_eq!(initial["entries"].as_array().unwrap().len(), 1);
    let disabled = run(&["skills", "disable", "review"]);
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("api_key = 'SECRET' # untouched"));
    assert!(updated.contains("enabled = true # master capability"));
    let disabled = view(&run(&["skills", "inspect", "review", "--json"]));
    assert_eq!(disabled["entries"][0]["enabled"], false);
    assert_ne!(disabled["revision"], initial["revision"]);
    assert!(run(&["skills", "enable", "review"]).status.success());
    assert_eq!(
        view(&run(&["skills", "list", "--json"]))["entries"][0]["enabled"],
        true
    );
    assert!(!home.path().join(".zevria/config.toml").exists());
}

#[test]
fn skills_cli_points_stale_provider_tables_to_models_jsonc() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = home.path().join("work.toml");
    let source = "[providers.private]\napi_key = 'do-not-print'\n";
    std::fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args(["--runtime", "native"])
        .args(["skills", "list"])
        .env("HOME", home.path())
        .env("ZEVRIA_CONFIG", &path)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("moved to models.jsonc next to this file"),
        "{error}"
    );
    assert!(!error.contains("do-not-print"));
    assert!(!home.path().join("models.jsonc").exists());
}

#[test]
fn skills_cli_accepts_incomplete_offline_mode_assignments_without_a_catalog() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = home.path().join("offline.toml");
    std::fs::write(&path, "[modes]\nplan = { model = 'not-configured-yet' }\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args(["--runtime", "native"])
        .args(["skills", "list"])
        .env("HOME", home.path())
        .env("ZEVRIA_CONFIG", &path)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join("models.jsonc").exists());
}

#[test]
fn skills_cli_rejects_root_overrides_mixed_modes_and_outside_validation() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = workspace.path().join("outside.md");
    std::fs::write(&outside, "OUTSIDE PRIVATE CONTENT").unwrap();
    for args in [
        vec!["skills", "list", "--root", "/tmp"],
        vec!["skills", "list", "--acp"],
        vec!["skills", "list", "--continue"],
        vec!["--acp", "skills", "list"],
        vec!["--continue", "skills", "list"],
        vec!["skills", "enable", "review", "--json"],
    ] {
        assert!(
            !cli(home.path(), workspace.path(), &args).status.success(),
            "{args:?}"
        );
    }
    let invalid = cli(
        home.path(),
        workspace.path(),
        &["skills", "validate", "outside.md", "--json"],
    );
    assert!(!invalid.status.success());
    let report: serde_json::Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert!(
        report["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("outside")
    );
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains("OUTSIDE PRIVATE CONTENT"));
    std::fs::create_dir_all(home.path().join(".zevria")).unwrap();
    std::fs::write(
        home.path().join(".zevria/config.toml"),
        "[skills]\nroots=[]\n",
    )
    .unwrap();
    assert!(
        !cli(home.path(), workspace.path(), &["skills", "list", "--json"])
            .status
            .success()
    );
}

#[test]
fn skills_config_mutations_respect_the_cross_process_lock() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let dir = home.path().join(".zevria");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "# untouched\n").unwrap();
    let lock = std::fs::File::create(dir.join("config.toml.skills.lock")).unwrap();
    lock.try_lock().unwrap();
    let output = cli(
        home.path(),
        workspace.path(),
        &["skills", "disable", "review"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# untouched\n");
    drop(lock);
    assert!(
        cli(
            home.path(),
            workspace.path(),
            &["skills", "disable", "review"]
        )
        .status
        .success()
    );
}
