//! Binary-level checks that must never initialize GPUI or load provider credentials.
use std::process::Command;
use zevria_desktop::cli::USAGE;

#[test]
fn help_prints_the_desktop_usage_without_launching() {
    let cwd = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"))
            .current_dir(cwd.path())
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{USAGE}\n")
        );
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn private_environment_export_does_not_initialize_gpui_or_configuration() {
    let cwd = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"))
        .current_dir(cwd.path())
        .env_clear()
        .env("CAPTURE_TEST", "desktop value")
        .env("ZEVRIA_CONFIG", cwd.path().join("missing/config.toml"))
        .args(["--__print-env", "desktop-marker"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(output.stdout.starts_with(b"desktop-marker\0"));
    assert!(
        output
            .stdout
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"CAPTURE_TEST=desktop value")
    );
    assert!(std::fs::read_dir(cwd.path()).unwrap().next().is_none());
}

#[test]
fn invalid_arguments_are_rejected_before_launching() {
    for (args, message) in [
        (vec!["--bogus"], "unknown option: --bogus"),
        (
            vec!["--", "--help", "extra"],
            "only one workspace may be opened",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_platform_reports_the_cli_alternative() {
    let output = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "Zevria Desktop currently supports macOS only; use the zevria CLI on this platform"
        ),
        "{output:?}"
    );
}
