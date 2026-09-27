use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Output, Stdio},
};
use zevria_theme::{ThemeDefinition, validate_theme};

fn command(home: &Path, workspace: &Path, config: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zevria"));
    command
        .args(["--runtime", "native"])
        .env("HOME", home)
        .env_remove("ZEVRIA_CONFIG")
        .env_remove("TERM")
        .env_remove("RUST_LOG")
        .current_dir(workspace)
        .stdin(Stdio::null());
    if let Some(config) = config {
        command.env("ZEVRIA_CONFIG", config);
    }
    command
}
fn generate(
    home: &Path,
    workspace: &Path,
    config: Option<&Path>,
    name: &str,
    background: &str,
) -> Output {
    command(home, workspace, config)
        .args([
            "theme",
            "generate",
            "--name",
            name,
            "--background",
            background,
        ])
        .output()
        .unwrap()
}
fn result(output: &Output, success: bool) -> String {
    assert_eq!(
        output.status.success(),
        success,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("PRIVATE-CREDENTIAL"));
    text
}
fn document(path: &Path) -> toml::Table {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}
fn no_runtime_side_effects(home: &Path, workspace: &Path) {
    assert!(!workspace.join(".zevria").exists());
    for name in ["logs", "sessions", "subsessions", "agent-runs", "plans"] {
        assert!(!home.join(".zevria").join(name).exists());
    }
}

#[test]
fn first_run_saves_complete_palette_and_only_selects_name_offline() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = generate(home.path(), workspace.path(), None, "ocean", "#1e1e2e");
    let stdout = result(&output, true);
    let path = home
        .path()
        .join(".zevria")
        .join("themes")
        .join("ocean.toml");
    let config = home.path().join(".zevria").join("config.toml");
    for required in [
        "ocean",
        "#1E1E2E",
        &path.display().to_string(),
        &config.display().to_string(),
        "Restart",
        "unchanged",
    ] {
        assert!(stdout.contains(required), "{stdout}");
    }
    let theme: ThemeDefinition = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    validate_theme(&theme).unwrap();
    assert_eq!(theme.source_background.to_string(), "#1E1E2E");
    assert_eq!(document(&path)["palette"].as_table().unwrap().len(), 27);
    let selected = document(&config);
    assert_eq!(selected["theme"].as_table().unwrap().len(), 1);
    assert_eq!(selected["theme"]["name"].as_str(), Some("ocean"));
    assert!(!selected.contains_key("palette"));
    assert!(!selected.contains_key("providers")); // provider catalogs belong in models.jsonc
    assert!(
        fs::read_to_string(config)
            .unwrap()
            .contains("sibling models.jsonc")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    no_runtime_side_effects(home.path(), workspace.path());
}

#[test]
fn custom_configs_share_global_files_retries_preserve_bytes_and_reset_keeps_every_theme() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let configs = tempfile::tempdir().unwrap();
    let one = configs.path().join("one.toml");
    let two = configs.path().join("nested/two.toml");
    let before = "# retain comment\n[providers.incomplete]\napi_key = 'PRIVATE-CREDENTIAL' # keep\n[theme]\nbad = ['old selector']\n";
    fs::write(&one, before).unwrap();
    result(
        &generate(
            home.path(),
            workspace.path(),
            Some(&one),
            "ocean",
            "#1E1E2E",
        ),
        true,
    );
    let ocean = home.path().join(".zevria/themes/ocean.toml");
    let original = format!("# user comment\n{}", fs::read_to_string(&ocean).unwrap());
    fs::write(&ocean, &original).unwrap();
    let modified = fs::metadata(&ocean).unwrap().modified().unwrap();
    result(
        &generate(
            home.path(),
            workspace.path(),
            Some(&two),
            "ocean",
            "#1E1E2E",
        ),
        true,
    );
    assert_eq!(fs::read_to_string(&ocean).unwrap(), original);
    assert_eq!(fs::metadata(&ocean).unwrap().modified().unwrap(), modified);
    assert_eq!(document(&two)["theme"]["name"].as_str(), Some("ocean"));
    assert!(!configs.path().join("themes").exists());
    assert!(!home.path().join(".zevria/config.toml").exists());
    let selected_before = fs::read_to_string(&one).unwrap();
    assert!(selected_before.contains("api_key = 'PRIVATE-CREDENTIAL' # keep"));
    let collision = result(
        &generate(
            home.path(),
            workspace.path(),
            Some(&one),
            "ocean",
            "#FFFFFF",
        ),
        false,
    );
    assert!(collision.contains("choose another name"));
    assert_eq!(fs::read_to_string(&one).unwrap(), selected_before);
    assert_eq!(fs::read_to_string(&ocean).unwrap(), original);
    result(
        &generate(
            home.path(),
            workspace.path(),
            Some(&one),
            "paper",
            "#FFFFFF",
        ),
        true,
    );
    result(
        &command(home.path(), workspace.path(), Some(&one))
            .args(["theme", "reset"])
            .output()
            .unwrap(),
        true,
    );
    assert!(!document(&one).contains_key("theme"));
    assert_eq!(document(&two)["theme"]["name"].as_str(), Some("ocean"));
    assert!(home.path().join(".zevria/themes/paper.toml").is_file());
    assert_eq!(fs::read_to_string(ocean).unwrap(), original);
    no_runtime_side_effects(home.path(), workspace.path());
}

#[test]
fn reset_noops_and_malformed_selector_recovery_need_no_theme_store() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    result(
        &command(home.path(), workspace.path(), Some(&path))
            .args(["theme", "reset"])
            .output()
            .unwrap(),
        true,
    );
    assert!(!path.exists());
    assert!(!home.path().join(".zevria").exists());
    for source in [
        "# empty\n",
        "theme = 123\n",
        "[theme]\nname = '../broken'\nextra = 1\n",
    ] {
        fs::write(&path, source).unwrap();
        result(
            &command(home.path(), workspace.path(), Some(&path))
                .env_remove("HOME")
                .env_remove("USERPROFILE")
                .env_remove("HOMEDRIVE")
                .env_remove("HOMEPATH")
                .args(["theme", "reset"])
                .output()
                .unwrap(),
            true,
        );
        assert!(!document(&path).contains_key("theme"));
    }
    assert!(!home.path().join(".zevria").exists());
}

#[test]
fn missing_home_argument_errors_and_search_failure_never_persist() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let custom = home.path().join("custom.toml");
    let error = result(
        &command(home.path(), workspace.path(), Some(&custom))
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("HOMEDRIVE")
            .env_remove("HOMEPATH")
            .args([
                "theme",
                "generate",
                "--name",
                "ocean",
                "--background",
                "#1E1E2E",
            ])
            .output()
            .unwrap(),
        false,
    );
    assert!(error.contains("HOME"));
    let cases = [
        vec!["theme"],
        vec!["theme", "generate"],
        vec!["theme", "generate", "--name", "ocean"],
        vec!["theme", "generate", "--background", "#000000"],
        vec!["theme", "generate", "--name"],
        vec![
            "theme",
            "generate",
            "--name",
            "ocean",
            "--name",
            "other",
            "--background",
            "#000000",
        ],
        vec![
            "theme",
            "generate",
            "--name",
            "ocean",
            "--background",
            "#000000",
            "--background",
            "#FFFFFF",
        ],
        vec![
            "theme",
            "generate",
            "--name",
            "ocean",
            "--background",
            "#000000",
            "--apply",
        ],
        vec!["theme", "reset", "--name", "ocean"],
    ];
    for args in cases {
        result(
            &command(home.path(), workspace.path(), None)
                .args(args)
                .output()
                .unwrap(),
            false,
        );
    }
    for flag in [
        "--acp",
        "--ensemble-worker",
        "--continue",
        "-c",
        "clean",
        "skills",
    ] {
        for args in [vec!["theme", "reset", flag], vec![flag, "theme", "reset"]] {
            result(
                &command(home.path(), workspace.path(), None)
                    .args(args)
                    .output()
                    .unwrap(),
                false,
            );
        }
    }
    for background in ["#123", "#FFFFFFFF", "red", "282C34", "#12345Z"] {
        result(
            &generate(home.path(), workspace.path(), None, "ocean", background),
            false,
        );
    }
    for name in [
        "../escape",
        "/absolute",
        "a/b",
        "a\\b",
        "a.toml",
        ".",
        "",
        "Ocean",
        "_ocean",
        "ocean\n",
        "é",
    ] {
        result(
            &generate(home.path(), workspace.path(), None, name, "#000000"),
            false,
        );
    }
    result(
        &generate(
            home.path(),
            workspace.path(),
            None,
            &"a".repeat(65),
            "#000000",
        ),
        false,
    );
    let error = result(
        &generate(home.path(), workspace.path(), None, "difficult", "#777777"),
        false,
    );
    assert!(error.contains("within the search budget"));
    result(
        &command(home.path(), workspace.path(), None)
            .args(["theme", "--help"])
            .output()
            .unwrap(),
        true,
    );
    assert!(!custom.exists());
    assert!(!home.path().join(".zevria").exists());
    no_runtime_side_effects(home.path(), workspace.path());
}

#[test]
fn malformed_configuration_diagnostics_do_not_echo_credentials() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = home.path().join("custom.toml");
    let before = "[providers.private]\napi_key = 'PRIVATE-CREDENTIAL\n";
    fs::write(&config, before).unwrap();
    result(
        &generate(
            home.path(),
            workspace.path(),
            Some(&config),
            "ocean",
            "#1E1E2E",
        ),
        false,
    );
    result(
        &command(home.path(), workspace.path(), Some(&config))
            .args(["theme", "reset"])
            .output()
            .unwrap(),
        false,
    );
    assert_eq!(fs::read_to_string(config).unwrap(), before);
    assert!(!home.path().join(".zevria/themes/ocean.toml").exists());
}

const PROVIDERS: &str = r#"
[providers.test]
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "PRIVATE-CREDENTIAL"
supports_websockets = false
[providers.test.models."test-model"]
context_window_tokens = 272000
retained_user_tokens = 20000
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[modes]
build = { provider = "test", model = "test-model", reasoning_level = "medium" }
plan = { provider = "test", model = "test-model", reasoning_level = "medium" }
review = { provider = "test", model = "test-model", reasoning_level = "medium" }
explore = { provider = "test", model = "test-model", reasoning_level = "medium" }
builder = { provider = "test", model = "test-model", reasoning_level = "medium" }
[log]
level = "off"
[theme]
name = "missing"
"#;

#[test]
fn broken_selection_fails_before_terminal_but_acp_does_not_open_it() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = home.path().join("custom.toml");
    zevria_app::test_support::write_fixture(&config, PROVIDERS).unwrap();
    let error = result(
        &command(home.path(), workspace.path(), Some(&config))
            .output()
            .unwrap(),
        false,
    );
    assert!(error.contains("theme reset"));
    assert!(error.contains("missing.toml"));
    assert!(!error.contains("terminal"));
    let root = home.path().join(".zevria/themes");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("missing.toml"), "not a valid document").unwrap();
    let error = result(
        &command(home.path(), workspace.path(), Some(&config))
            .output()
            .unwrap(),
        false,
    );
    assert!(error.contains("malformed theme"));
    for args in [vec!["--acp"], vec!["--acp", "--ensemble-worker"]] {
        let mut child = command(home.path(), workspace.path(), Some(&config))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1,\"clientCapabilities\":{}}}\n").unwrap();
        let output = child.wait_with_output().unwrap();
        result(&output, true);
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["id"], 1);
    }
}
