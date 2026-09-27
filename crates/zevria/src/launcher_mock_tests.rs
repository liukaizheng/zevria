//! Mocked launcher transport tests, deliberately not WSL acceptance coverage.
use super::*;

#[cfg(unix)]
fn transport(directory: &Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let executable = directory.join("mock wsl");
    std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
}
#[cfg(unix)]
#[tokio::test]
async fn probe_transport_reports_unavailable_distro_missing_dependencies_and_incompatibility() {
    let temp = tempfile::tempdir().unwrap();
    let controls = Controls::default();
    assert!(
        prepare(
            &temp.path().join("absent-wsl"),
            &controls,
            Path::new(r"C:\work space"),
            None
        )
        .await
        .is_err()
    );
    for (status, message) in [
        (1, "no installed distribution"),
        (33, "Linux Zevria is missing"),
        (35, "Linux RTK is missing"),
    ] {
        let exe = transport(
            temp.path(),
            &format!("printf '%s' '{message}' >&2; exit {status}"),
        );
        let error = prepare(&exe, &controls, Path::new(r"C:\work space"), None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(message),
            "expected {message:?} for mock exit status {status}; got: {error:#}"
        );
    }
    let exe = transport(temp.path(), "printf 'not a compatible probe'");
    assert!(
        prepare(&exe, &controls, Path::new(r"C:\work space"), None)
            .await
            .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn ready_probe_forwards_literal_arguments_config_and_final_exit_status() {
    let temp = tempfile::tempdir().unwrap();
    let exe = transport(
        temp.path(),
        "case \"$*\" in *--kill-after=1s*) cat \"$0.reply\";; *) printf 'handoff output'; printf 'handoff error' >&2; printf x >> \"$0.calls\"; exit 37;; esac",
    );
    let payload = format!(
        "/home/me/.cargo/bin/zevria\0Ubuntu\0/mnt/c/work space\0/mnt/c/settings 雪.toml\0/mnt/c/skill';&.md\0{}",
        serde_json::json!({"os":"linux","version":env!("CARGO_PKG_VERSION"),"launcher_revision":REVISION})
    );
    std::fs::write(exe.with_file_name("mock wsl.reply"), payload).unwrap();
    let controls = Controls::parse(
        [
            "skills",
            "--json",
            "validate",
            "skill';&.md",
            "--runtime",
            "wsl",
        ]
        .map(str::to_owned)
        .into(),
    )
    .unwrap();
    let handoff = prepare(
        &exe,
        &controls,
        Path::new(r"C:\work space"),
        Some(Path::new(r"C:\settings 雪.toml")),
    )
    .await
    .unwrap();
    assert_eq!(handoff.config, "/mnt/c/settings 雪.toml");
    assert_eq!(
        handoff.args,
        ["skills", "--json", "validate", "/mnt/c/skill';&.md"]
    );
    let output = handoff
        .command(&exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stdout, b"handoff output");
    assert_eq!(output.stderr, b"handoff error");
    assert_eq!(
        std::fs::read(exe.with_file_name("mock wsl.calls")).unwrap(),
        b"x"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn dependency_probe_bounds_time_and_output_and_reaps_background_pipe_holders() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf partial; sleep 10"]);
    let error = bounded_output(&mut command, Duration::from_millis(100), 1024)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 123456789"]);
    assert!(
        bounded_output(&mut command, Duration::from_secs(2), 4)
            .await
            .unwrap_err()
            .to_string()
            .contains("output limit")
    );
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 10 & printf foreground"]);
    let output = bounded_output(&mut command, Duration::from_secs(2), 1024)
        .await
        .unwrap();
    assert_eq!(output.stdout, b"foreground");
}
