use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

fn command(home: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zevria"));
    command
        .current_dir(workspace)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("ZEVRIA_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn without_dependencies(home: &Path, workspace: &Path) -> Command {
    let mut command = command(home, workspace);
    command
        .env("ZEVRIA_CONFIG", home.join("missing/config.toml"))
        .env("ZEVRIA_GIT_BASH", home.join("missing/bash.exe"))
        .env("SHELL", home.join("missing/sh"));
    command
}

fn assert_empty(path: &Path) {
    assert!(
        std::fs::read_dir(path).unwrap().next().is_none(),
        "{path:?}"
    );
}

fn plain_success(output: &Output) -> &str {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert!(!output.stdout.contains(&0x1b));
    std::str::from_utf8(&output.stdout).unwrap()
}

#[test]
fn help_version_and_offline_commands_need_no_shell_or_model_configuration() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        without_dependencies(home.path(), workspace.path())
            .args(args)
            .output()
            .unwrap()
    };
    for flag in ["--help", "-h"] {
        let output = run(&[flag]);
        let help = plain_success(&output);
        assert!(help.starts_with("usage: zevria"));
        assert!(!help.contains("--runtime"));
        assert!(!help.contains("--wsl-distro"));
    }
    for flag in ["--version", "-V"] {
        assert_eq!(
            plain_success(&run(&[flag])).trim_end(),
            concat!("zevria ", env!("CARGO_PKG_VERSION"))
        );
    }
    let output = run(&["skills", "list", "--json"]);
    let view: serde_json::Value = serde_json::from_str(plain_success(&output)).unwrap();
    assert_eq!(view["entries"], serde_json::json!([]));
    assert_eq!(
        Path::new(view["global_location"].as_str().unwrap()),
        home.path().join(".zevria/skills")
    );
    assert_eq!(
        Path::new(view["project_location"].as_str().unwrap()),
        workspace.path().join(".zevria/skills")
    );
    assert!(plain_success(&run(&["theme", "reset"])).starts_with("Theme selection reset"));
    assert!(plain_success(&run(&["clean"])).contains("Already absent:"));
    assert_empty(home.path());
    assert_empty(workspace.path());
}

#[test]
fn removed_options_and_probes_are_cli_errors_before_shell_or_config() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    for (flag, args) in [
        ("--runtime", &["--runtime", "native", "--acp"][..]),
        ("--runtime", &["--runtime", "auto", "skills", "list"]),
        ("--runtime=wsl", &["--runtime=wsl", "--help"]),
        ("--runtime=native", &["skills", "list", "--runtime=native"]),
        ("--wsl-distro", &["--wsl-distro", "Ubuntu", "clean"]),
        ("--wsl-distro=Ubuntu", &["--acp", "--wsl-distro=Ubuntu"]),
        ("--__launcher-probe", &["--__launcher-probe"]),
        ("--__launcher-probe-tools", &["--__launcher-probe-tools"]),
    ] {
        let output = without_dependencies(home.path(), workspace.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unknown") && stderr.contains(flag),
            "{stderr}"
        );
        assert!(stderr.contains("usage: zevria"), "{stderr}");
        assert!(!stderr.contains("zevria: runtime"), "{stderr}");
    }
    assert_empty(home.path());
    assert_empty(workspace.path());
}

#[test]
fn private_environment_export_bypasses_shell_and_application_initialization() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = command(home.path(), workspace.path())
        .env_clear()
        .env("CAPTURE_TEST", "spaces = and\nnewlines")
        .env("ZEVRIA_CONFIG", home.path().join("missing/config.toml"))
        .env("ZEVRIA_GIT_BASH", home.path().join("missing-bash.exe"))
        .args(["--__print-env", "test-marker"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(output.stdout.starts_with(b"test-marker\0"));
    assert!(
        output
            .stdout
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"CAPTURE_TEST=spaces = and\nnewlines")
    );
    assert_empty(home.path());
    assert_empty(workspace.path());
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
        .args(["skills", "list", "--json"])
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
    let config = home.path().join("missing").join("config.toml");
    let system_bash = zevria_foundation::windows_process::system_executable("bash.exe").unwrap();
    let output = command(home.path(), workspace.path())
        .env("ZEVRIA_CONFIG", &config)
        .env("ZEVRIA_GIT_BASH", system_bash)
        .arg("--acp")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let state = workspace.path().join(".zevria").join("windows");
    assert!(
        stderr.contains(&format!(
            "zevria: runtime windows; configuration {}; state {}",
            config.display(),
            state.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("Invalid ZEVRIA_GIT_BASH override"),
        "{stderr}"
    );
    assert_empty(home.path());
    assert_empty(workspace.path());
}

#[cfg(windows)]
#[test]
fn native_clean_leaves_linux_history_and_shared_project_inputs_untouched() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let preserved = [
        ".zevria/sessions/linux.jsonl",
        ".zevria/skills/shared.md",
        "AGENTS.md",
    ];
    for path in preserved
        .into_iter()
        .chain([".zevria/windows/sessions/native.jsonl"])
    {
        let path = workspace.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "keep-or-remove").unwrap();
    }
    let output = without_dependencies(home.path(), workspace.path())
        .arg("clean")
        .output()
        .unwrap();
    plain_success(&output);
    assert!(!workspace.path().join(".zevria/windows/sessions").exists());
    for path in preserved {
        assert_eq!(
            std::fs::read(workspace.path().join(path)).unwrap(),
            b"keep-or-remove"
        );
    }
    assert_empty(home.path());
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
