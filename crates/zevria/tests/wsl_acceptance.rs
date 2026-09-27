//! Opt-in real-machine handoff coverage, not part of mocked launcher tests.
//! Requires an already configured WSL 2 distribution and matching Linux Zevria.
//! Installer acceptance additionally requires preinstalled Windows/Linux binaries
//! and an explicit custom Linux root; this test never installs/reconfigures WSL.
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

#[test]
#[ignore = "requires installer-produced matching companions and a provisioned custom-root WSL installation"]
fn installer_produced_companions_discover_the_provisioned_custom_root() {
    use std::process::Command;
    let distro = std::env::var("ZEVRIA_WSL_ACCEPTANCE_DISTRO").unwrap();
    let windows = std::path::PathBuf::from(
        std::env::var_os("ZEVRIA_WSL_ACCEPTANCE_WINDOWS_EXE")
            .expect("set the absolute installer-produced Windows executable path"),
    );
    let linux_root = std::env::var("ZEVRIA_WSL_ACCEPTANCE_LINUX_ROOT")
        .expect("set the preinstalled custom Linux root recorded by install.sh");
    assert!(windows.is_absolute() && windows.is_file());
    assert!(linux_root.starts_with('/') && !linux_root.contains(['\0', '\n', '\r']));
    let version = Command::new(&windows).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim_end(),
        format!("zevria {}", env!("CARGO_PKG_VERSION"))
    );
    let wsl = zevria_foundation::windows_process::system_executable("wsl.exe").unwrap();
    let check = Command::new(wsl)
        .args([
            "--distribution", &distro, "--cd", "/", "--exec", "/usr/bin/timeout",
            "--kill-after=1s", "10s", "/bin/sh", "-c",
            "set -eu; IFS= read -r root < \"$HOME/.zevria/install-root\"; [ \"$root\" = \"$1\" ]; [ \"$root\" != \"$HOME/.zevria\" ]; exec \"$root/bin/zevria\" --__launcher-probe",
            "zevria-installer-acceptance", &linux_root,
        ])
        .env_remove("HOME").env_remove("WSLENV").env_remove("ZEVRIA_INSTALL")
        .output().unwrap();
    assert!(check.status.success(), "{:?}", check);
    let probe: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(probe["os"], "linux");
    assert_eq!(probe["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(probe["launcher_revision"], 1);
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("no-config.toml");
    let result = Command::new(windows)
        .args([
            "--runtime",
            "wsl",
            "--wsl-distro",
            &distro,
            "skills",
            "list",
            "--json",
        ])
        .env("ZEVRIA_CONFIG", &config)
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
    serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap();
    assert!(String::from_utf8_lossy(&result.stderr).contains("runtime WSL"));
    assert!(!config.exists());
    assert!(!temp.path().join(".zevria/windows").exists());
}
