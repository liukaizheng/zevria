//! Opt-in real-machine handoff coverage, not part of mocked launcher tests.
//! Requires an already configured WSL 2 distribution and matching Linux Zevria.
#![cfg(windows)]

#[test]
#[ignore = "requires a real Windows/WSL 2 host and ZEVRIA_WSL_ACCEPTANCE_DISTRO; never installs a distro"]
fn whole_application_wsl_handoff_preserves_json_paths_and_failure_status() {
    use std::process::Command;
    let distro = std::env::var("ZEVRIA_WSL_ACCEPTANCE_DISTRO")
        .expect("select an existing prepared distribution explicitly");
    let temp = tempfile::Builder::new()
        .prefix("Zevria WSL space 雪")
        .tempdir()
        .unwrap();
    let workspace = temp.path().join("checkout space 雪'; &");
    std::fs::create_dir_all(workspace.join(".zevria/skills")).unwrap();
    let manifest = workspace.join(".zevria/skills/handoff.md");
    std::fs::write(
        &manifest,
        "---\ndescription: Handoff fixture\n---\nOnly validate this fixture.",
    )
    .unwrap();
    let config = temp.path().join("config space 雪'; &.toml");
    let invoke = |path: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_zevria"))
            .args([
                "--runtime",
                "wsl",
                "--wsl-distro",
                &distro,
                "skills",
                "validate",
            ])
            .arg(path)
            .arg("--json")
            .env("ZEVRIA_CONFIG", &config)
            .current_dir(&workspace)
            .output()
            .unwrap()
    };
    let output = invoke(&manifest);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("runtime WSL"));
    let output = invoke(&workspace.join(".zevria/skills/absent.md"));
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("using native Windows"));
    assert!(
        !config.exists(),
        "offline validation must not create configuration"
    );
    assert!(!workspace.join(".zevria/windows").exists());
}
