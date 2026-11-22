use std::{
    path::Path,
    process::{Command, Output},
};

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zevria"))
        .arg("mcp")
        .args(args)
        .env("ZEVRIA_CONFIG", config)
        .current_dir(config.parent().unwrap())
        .output()
        .unwrap()
}

#[test]
fn mcp_cli_needs_no_provider_configuration_and_preserves_comments() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("config.toml");
    let before = r#"# Keep this comment and the unrelated settings.
[log]

[mcp.servers.example]
command = "not-a-real-mcp-program"
enabled = true # Keep this too.
env = { TOKEN = "test-private-token" }

[mcp.servers.web]
url = "https://example.invalid/mcp?token=private-query"
headers = { X-Private = "test-private-header" }
enabled = false
"#;
    std::fs::write(&path, before).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let output = run(&path, &["list", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["servers"].as_array().unwrap().len(), 2);
    for secret in ["test-private-token", "private-query", "test-private-header"] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    }
    assert!(run(&path, &["disable", "example"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before.replace("enabled = true #", "enabled = false #")
    );
    let output = run(&path, &["inspect", "example", "--json"]);
    assert!(output.status.success());
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["selected"]["summary"]["status"]["status"], "disabled");
    assert!(run(&path, &["enable", "example"]).status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    let output = run(&path, &["test", "example", "--json"]);
    assert!(!output.status.success());
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["servers"][0]["status"]["status"], "failed");
    assert!(!workspace.path().join("models.jsonc").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
