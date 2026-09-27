//! Execute the production POSIX discovery source on Linux, not a mocked WSL reply.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, process::Output};

fn install(root: &Path, program: &str) -> std::path::PathBuf {
    fs::create_dir_all(root.join("bin")).unwrap();
    let exe = root.join("bin/zevria");
    fs::copy(program, &exe).unwrap();
    exe
}
fn select(home: &Path) -> Output {
    std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &linux_script("zevria_select\nprintf '%s\\n%s\\n' \"$exe\" \"$PATH\""),
        ])
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("ZEVRIA_INSTALL", "/unrelated-windows-input")
        .output()
        .unwrap()
}
fn locator(home: &Path, value: &[u8]) {
    fs::create_dir_all(home.join(".zevria")).unwrap();
    fs::write(home.join(".zevria/install-root"), value).unwrap();
}
#[test]
fn defaults_prefer_installer_over_cargo_and_custom_locator_over_both() {
    let home = tempfile::tempdir().unwrap();
    let cargo = install(&home.path().join(".cargo"), "/bin/false");
    assert!(
        String::from_utf8_lossy(&select(home.path()).stdout).starts_with(cargo.to_str().unwrap())
    );
    let default = install(&home.path().join(".zevria"), "/bin/echo");
    assert!(
        String::from_utf8_lossy(&select(home.path()).stdout).starts_with(default.to_str().unwrap())
    );
    let custom = home.path().join("custom 雪 ' $(touch INJECTED); &");
    let exe = install(&custom, "/bin/echo");
    locator(home.path(), format!("{}\n", custom.display()).as_bytes());
    let result = select(home.path());
    assert!(result.status.success(), "{:?}", result);
    let output = String::from_utf8(result.stdout).unwrap();
    let mut lines = output.lines();
    assert_eq!(lines.next(), exe.to_str());
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with(custom.join("bin").to_str().unwrap())
    );
    assert!(!home.path().join("INJECTED").exists());
    assert!(!home.path().join(".zevria/config.toml").exists());
}
#[test]
fn malformed_stale_nonregular_and_non_elf_locators_do_not_fall_through() {
    let home = tempfile::tempdir().unwrap();
    install(&home.path().join(".cargo"), "/bin/echo");
    for value in [
        b"".as_slice(),
        b"relative\n",
        b"/missing\n",
        b"/missing",
        b"/a\n/b\n",
        b"/a\n\n",
        b"/a\r\n",
        b"/a\0\n",
        b"/a:b\n",
        b"/a\xc2\x85\n",
        &[b'/'; 4097],
    ] {
        locator(home.path(), value);
        let result = select(home.path());
        assert!(!result.status.success(), "accepted {:?}", value);
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("rerun the matching Linux installer")
        );
        assert!(result.stdout.is_empty());
    }
    let custom = home.path().join("non-elf");
    let exe = install(&custom, "/bin/echo");
    fs::write(&exe, b"#!/bin/sh\necho wrong\n").unwrap();
    locator(home.path(), format!("{}\n", custom.display()).as_bytes());
    assert!(!select(home.path()).status.success());
    fs::copy("/bin/echo", &exe).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!select(home.path()).status.success());
    let path = home.path().join(".zevria/install-root");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(!select(home.path()).status.success());
    fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink(home.path().join("dangling"), &path).unwrap();
    assert!(!select(home.path()).status.success());
    fs::remove_file(&path).unwrap();
    install(&custom, "/bin/echo");
    locator(home.path(), format!("{}\n", custom.display()).as_bytes());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    // Root can read mode-000 files; exercise actual unreadability when enforced.
    if fs::File::open(&path).is_err() {
        assert!(!select(home.path()).status.success());
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn readiness_and_handoff_use_identical_path_order_for_legacy_and_recorded_installs() {
    for (name, recorded) in [(".cargo", false), (".zevria", false), ("custom", true)] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(name);
        install(&root, "/bin/echo");
        if recorded {
            locator(home.path(), format!("{}\n", root.display()).as_bytes());
        }
        let result = std::process::Command::new("/bin/sh")
            .args(["-c", &linux_script("zevria_select; ready=$PATH; zevria_path \"${exe%/*}\"; [ \"$ready\" = \"$PATH\" ]")])
            .env("HOME", home.path()).env("PATH", "/usr/bin:/bin").output().unwrap();
        assert!(result.status.success(), "{name}: {result:?}");
    }
}

#[test]
fn linked_metadata_directory_is_not_an_alternate_locator_source() {
    let home = tempfile::tempdir().unwrap();
    let other = home.path().join("other");
    install(&other, "/bin/echo");
    fs::write(other.join("install-root"), format!("{}\n", other.display())).unwrap();
    std::os::unix::fs::symlink(&other, home.path().join(".zevria")).unwrap();
    install(&home.path().join(".cargo"), "/bin/echo");
    let result = select(home.path());
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("install-root"));
}

#[test]
fn legacy_relative_path_entries_are_resolved_in_the_mapped_workspace() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    let exe = install(&workspace.join("relative"), "/bin/echo");
    let result = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &linux_script(PROBE_SCRIPT),
            "probe",
            "linux",
            workspace.to_str().unwrap(),
            "linux",
            "",
            "linux",
            "",
            "offline",
        ])
        .env("HOME", home.path())
        .env("PATH", "relative/bin:/usr/bin:/bin")
        .env("WSL_DISTRO_NAME", "Fixture")
        .current_dir("/")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        result.stdout.split(|c| *c == 0).next().unwrap(),
        exe.to_str().unwrap().as_bytes()
    );
}

#[test]
fn actual_probe_and_handoff_keep_the_exact_selected_executable() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("custom ';&");
    let exe = install(&root, "/bin/echo");
    install(&home.path().join(".cargo"), "/bin/false");
    locator(home.path(), format!("{}\n", root.display()).as_bytes());
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &linux_script(PROBE_SCRIPT),
            "probe",
            "linux",
            home.path().to_str().unwrap(),
            "linux",
            "",
            "linux",
            "",
            "offline",
        ])
        .env("HOME", home.path())
        .env("WSL_DISTRO_NAME", "Fixture")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let fields: Vec<_> = output.stdout.split(|c| *c == 0).collect();
    assert_eq!(fields[0], exe.to_str().unwrap().as_bytes());
    assert_eq!(fields[1], b"Fixture");
    // Change discovery state after probing: handoff must not reselect from it.
    locator(home.path(), b"/now-stale\n");
    let handoff = Handoff {
        executable: exe.to_str().unwrap().into(),
        distro: "Fixture".into(),
        workspace: home.path().to_str().unwrap().into(),
        config: String::new(),
        args: vec!["literal ';& $(touch INJECTED)".into()],
    };
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &linux_script(HANDOFF_SCRIPT),
            "handoff",
            &handoff.executable,
            &handoff.workspace,
            &handoff.config,
        ])
        .args(&handoff.args)
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "literal ';& $(touch INJECTED)\n"
    );
    assert!(!home.path().join("INJECTED").exists());
}
