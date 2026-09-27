#[path = "support/acp_automatic_skills.rs"]
mod acp_automatic_skills;
#[path = "support/acp_guidance.rs"]
mod acp_guidance;
#[path = "support/acp_sessions.rs"]
mod acp_sessions;
#[path = "support/acp_skills.rs"]
mod acp_skills;
#[path = "support/acp_worker.rs"]
mod acp_worker;

use std::{
    io::Write as _,
    process::{Command, Stdio},
};

fn write_test_config(home: &std::path::Path) {
    let directory = home.join(".zevria");
    std::fs::create_dir_all(&directory).expect("create test config directory");
    zevria_app::test_support::write_fixture(
        &directory.join("config.toml"),
        r#"
[providers.test]
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "test-key"
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
"#,
    )
    .expect("write test config");
}

#[test]
fn acp_stdio_initialize_keeps_stdout_json_rpc_only() {
    let home = tempfile::tempdir().expect("temporary home");
    write_test_config(home.path());
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .arg("--acp")
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zevria --acp");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}
"#,
        )
        .expect("write initialize request");
    let output = child.wait_with_output().expect("wait for ACP process");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8 JSON lines");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "unexpected stdout: {stdout:?}");
    let response: serde_json::Value =
        serde_json::from_str(lines[0]).expect("stdout line must be valid JSON-RPC");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert!(!stdout.contains('\u{1b}'));
    assert!(!stdout.contains("starting zevria"));
}

#[test]
fn first_run_writes_setup_skeleton_and_exits_before_protocol_stdout() {
    let home = tempfile::tempdir().expect("temporary home");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .arg("--acp")
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run first-use ACP process");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let path = home.path().join(".zevria").join("config.toml");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&path.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("start Zevria again"), "stderr: {stderr}");
    let models_path = home.path().join(".zevria").join("models.jsonc");
    assert!(stderr.contains(&models_path.display().to_string()));
    let generated = std::fs::read_to_string(&models_path).expect("generated model skeleton");
    assert!(generated.contains("// \"providers\":"));
    assert!(!generated.contains("// \"modes\":"));
    let assignments = std::fs::read_to_string(&path).unwrap();
    let assignments: toml::Value =
        toml::from_str(&assignments).expect("generated config should be valid TOML");
    let modes = assignments
        .get("modes")
        .and_then(toml::Value::as_table)
        .expect("generated config should contain active mode assignments");
    for mode in ["plan", "build", "review", "explore", "builder"] {
        assert_eq!(
            modes[mode]["reasoning_level"].as_str(),
            Some("max"),
            "reasoning level for {mode}"
        );
    }
    assert!(generated.contains("\"api_key\": \"replace-with-api-key\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [&path, &models_path] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn invalid_worker_flags_fail_before_configuration_or_protocol_output() {
    for flags in [
        vec!["--ensemble-worker"],
        vec!["--ensemble-worker", "--acp", "--continue"],
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_zevria"))
            .args(flags)
            .env("HOME", home.path())
            .env_remove("ZEVRIA_CONFIG")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!home.path().join(".zevria/config.toml").exists());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("requires --acp") || stderr.contains("cannot be combined"),
            "{stderr}"
        );
    }
}

#[test]
fn acp_and_continue_fail_before_writing_protocol_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args(["--acp", "--continue"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run invalid CLI combination");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be combined"), "stderr: {stderr}");
    assert!(!stderr.contains('\u{1b}'));
}
