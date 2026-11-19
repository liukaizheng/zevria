//! Binary-level checks that must never initialize GPUI or load provider credentials.
use std::{path::Path, process::Command};

fn isolated(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"));
    command
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home);
    command
}
use zevria_desktop::cli::USAGE;

#[test]
fn help_prints_the_desktop_usage_without_launching() {
    let cwd = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h"] {
        let output = isolated(cwd.path()).arg(flag).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, format!("{USAGE}\n").as_bytes());
        assert!(output.stderr.is_empty(), "{output:?}");
    }
    assert!(std::fs::read_dir(cwd.path()).unwrap().next().is_none());
}

#[test]
fn private_environment_export_does_not_initialize_gpui_or_configuration() {
    let cwd = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zevria-desktop"))
        .current_dir(cwd.path())
        .env_clear()
        .env("CAPTURE_TEST", "space = 雪\nnext\r\nline")
        .args(["--__print-env", "desktop-marker"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let mut entries = output.stdout.split(|&byte| byte == 0);
    assert_eq!(entries.next(), Some(b"desktop-marker".as_slice()));
    assert_eq!(entries.next_back(), Some(b"".as_slice()));

    // macOS can supply this variable at process startup even with env_clear().
    let entries: Vec<_> = entries
        .filter(|entry| {
            !(cfg!(target_os = "macos") && entry.starts_with(b"__CF_USER_TEXT_ENCODING="))
        })
        .collect();
    assert_eq!(
        entries,
        ["CAPTURE_TEST=space = 雪\nnext\r\nline".as_bytes()]
    );
    assert!(std::fs::read_dir(cwd.path()).unwrap().next().is_none());
}

#[test]
fn invalid_arguments_are_rejected_before_launching() {
    let cwd = tempfile::tempdir().unwrap();
    for (args, message) in [
        (vec!["--bogus"], "unknown option: --bogus"),
        (
            vec!["--__print-env"],
            "private environment exporter expects one marker",
        ),
        (
            vec!["--", "--help", "extra"],
            "only one workspace may be opened",
        ),
    ] {
        let output = isolated(cwd.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
    }
    assert!(std::fs::read_dir(cwd.path()).unwrap().next().is_none());
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[test]
fn unsupported_platform_reports_the_cli_alternative() {
    let cwd = tempfile::tempdir().unwrap();
    let output = isolated(cwd.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("use the zevria CLI on this platform"),
        "{output:?}"
    );
}
