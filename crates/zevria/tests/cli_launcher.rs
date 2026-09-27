use std::{
    path::Path,
    process::{Command, Stdio},
};

fn command(home: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zevria"));
    command
        .current_dir(workspace)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("ZEVRIA_CONFIG")
        .env_remove("_ZEVRIA_WSL_HANDOFF")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[test]
fn help_version_and_machine_probe_have_no_configuration_or_terminal_side_effects() {
    for flag in ["--help", "--version", "--__launcher-probe"] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let output = command(home.path(), workspace.path())
            .arg(flag)
            .env(
                "ZEVRIA_CONFIG",
                home.path().join("missing/nested/config.toml"),
            )
            .env("ZEVRIA_GIT_BASH", home.path().join("missing-bash.exe"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.contains(&0x1b));
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
        assert!(
            std::fs::read_dir(workspace.path())
                .unwrap()
                .next()
                .is_none()
        );
        if flag == "--__launcher-probe" {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["os"], std::env::consts::OS);
            assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
            assert_eq!(value["launcher_revision"], 1);
        }
    }
}

#[test]
fn explicit_native_offline_commands_do_not_require_a_command_backend() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = command(home.path(), workspace.path())
        .args(["--runtime", "native", "skills", "list", "--json"])
        .env("ZEVRIA_GIT_BASH", home.path().join("nonexistent.exe"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn native_home_falls_back_to_userprofile_without_home() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".zevria/skills")).unwrap();
    std::fs::write(
        home.path().join(".zevria/skills/local.md"),
        "---\ndescription: Native home fixture\n---\nFixture instructions",
    )
    .unwrap();
    let output = command(home.path(), workspace.path())
        .env_remove("HOME")
        .args(["--runtime", "native", "skills", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("local"));
    assert!(!home.path().join(".zevria/config.toml").exists());
}

#[cfg(windows)]
#[test]
fn invalid_explicit_git_bash_is_diagnosed_before_config_or_acp_stdout() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = command(home.path(), workspace.path())
        .env("ZEVRIA_GIT_BASH", r"C:\Windows\System32\bash.exe")
        .args(["--runtime", "native", "--acp"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Git Bash"));
    assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn native_clean_leaves_wsl_history_and_shared_project_inputs_untouched() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    for path in [
        ".zevria/sessions/wsl.jsonl",
        ".zevria/windows/sessions/native.jsonl",
        ".zevria/skills/shared.md",
    ] {
        let path = workspace.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "keep-or-remove").unwrap();
    }
    let output = command(home.path(), workspace.path())
        .args(["--runtime", "native", "clean"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!workspace.path().join(".zevria/windows/sessions").exists());
    assert!(workspace.path().join(".zevria/sessions/wsl.jsonl").exists());
    assert!(workspace.path().join(".zevria/skills/shared.md").exists());
}

#[cfg(windows)]
#[test]
fn acp_job_helper_preserves_stdio_status_and_reaps_native_grandchildren() {
    use std::io::Write;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("agent fixture.js");
    // Node is an explicit Windows ACP test prerequisite, not installed by Zevria.
    std::fs::write(&script, r#"
const cp = require('child_process');
const child = cp.spawn(process.execPath, ['-e', "setTimeout(() => require('fs').writeFileSync('leaked','bad'), 1500)"], {stdio:'inherit'});
child.unref();
process.stdin.once('data', data => { process.stdout.write(data); process.stderr.write('diagnostic'); process.exit(37); });
"#).unwrap();
    let mut child = command(home.path(), workspace.path())
        .args(["--__acp-job", "node"])
        .arg(&script)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stdout, b"{\"jsonrpc\":\"2.0\",\"id\":1}\n");
    assert_eq!(output.stderr, b"diagnostic");
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!workspace.path().join("leaked").exists());
    assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
}

#[cfg(windows)]
#[test]
fn killing_the_sdk_direct_child_helper_terminates_its_job() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut child = command(home.path(), workspace.path()).args(["--__acp-job", "node", "-e", "require('fs').writeFileSync('started','yes'); setTimeout(() => require('fs').writeFileSync('leaked','bad'), 1500)"])
        .spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !workspace.path().join("started").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "helper exited before the fixture started"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "helper startup timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!workspace.path().join("leaked").exists());
}
