use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use zevria_foundation::runtime_paths::workspace_state_root as state_root;

const DIRECTORIES: [&str; 5] = [
    "agent-runs",
    "plans",
    "sessions",
    "subsessions",
    "ensemble-sessions",
];
const CONTENTS: &[u8] = b"custom artifact or malformed history\n\xff";

fn command(home: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zevria"));
    command
        .args(["--runtime", "native"])
        .env("HOME", home)
        .env_remove("ZEVRIA_CONFIG")
        .env_remove("TERM")
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn cli(home: &Path, workspace: &Path, args: &[&str]) -> Output {
    command(home, workspace).args(args).output().unwrap()
}

fn write_fixture(path: PathBuf) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, CONTENTS).unwrap();
    path
}

fn populate_root(root: &Path) -> Vec<PathBuf> {
    [
        ".gitignore",
        ".leases.lock",
        ".hidden/deep/secret",
        "stale.jsonl",
        "root.jsonl.lock",
        "nested/plan-projection.md",
        "nested/artifact.bin",
    ]
    .map(|artifact| write_fixture(root.join(artifact)))
    .into()
}

fn populate(workspace: &Path) -> Vec<PathBuf> {
    DIRECTORIES
        .iter()
        .flat_map(|name| populate_root(&state_root(workspace).join(name)))
        .collect()
}

fn assert_kept(paths: &[PathBuf]) {
    for path in paths {
        assert_eq!(fs::read(path).unwrap(), CONTENTS, "{path:?}");
    }
}

fn assert_absent(path: &Path) {
    assert_eq!(
        fs::symlink_metadata(path).unwrap_err().kind(),
        ErrorKind::NotFound,
        "{path:?}"
    );
}

fn assert_plain_output(output: &Output) {
    assert!(
        !output.stdout.contains(&0x1b),
        "stdout: {:?}",
        output.stdout
    );
    assert!(
        !output.stderr.contains(&0x1b),
        "stderr: {:?}",
        output.stderr
    );
}

fn success(output: &Output) -> &str {
    assert_plain_output(output);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::str::from_utf8(&output.stdout).unwrap()
}

fn failure(output: &Output) -> &str {
    assert_plain_output(output);
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    std::str::from_utf8(&output.stderr).unwrap()
}

#[test]
fn clean_purges_exactly_the_five_invocation_workspace_directories() {
    let home = tempfile::tempdir().unwrap();
    let ancestor = tempfile::tempdir().unwrap();
    let workspace = ancestor.path().join("project/nested");
    fs::create_dir_all(&workspace).unwrap();
    // Even a Git-root marker must not redirect cleanup to an ancestor workspace.
    fs::create_dir(ancestor.path().join(".git")).unwrap();
    populate(&workspace);
    let mut preserved = populate(home.path());
    preserved.extend(populate(ancestor.path()));
    for path in [
        ".zevria/.gitignore",
        ".zevria/skills/demo.md",
        ".zevria/themes/ocean.toml",
        ".zevria/config.toml",
        ".zevria/unknown/deep/keep",
        ".zevria/notes.txt",
        ".cazean/sessions/legacy.jsonl",
        "ordinary.txt",
    ] {
        preserved.push(write_fixture(workspace.join(path)));
    }
    for path in [
        ".zevria/config.toml",
        ".zevria/skills/global.md",
        ".zevria/themes/shared.toml",
        ".zevria/logs/keep.log",
        ".cazean/sessions/legacy.jsonl",
    ] {
        preserved.push(write_fixture(home.path().join(path)));
    }

    let output = cli(home.path(), &workspace, &["clean"]);
    let stdout = success(&output);
    let canonical = workspace.canonicalize().unwrap();
    let expected = std::iter::once(format!("Workspace: {canonical:?}"))
        .chain(DIRECTORIES.map(|name| format!("Removed: {:?}", state_root(&canonical).join(name))))
        .collect::<Vec<_>>();
    assert_eq!(stdout.lines().collect::<Vec<_>>(), expected);
    for name in DIRECTORIES {
        assert_absent(&state_root(&workspace).join(name));
    }
    assert_kept(&preserved);
    let mut siblings = fs::read_dir(workspace.join(".zevria"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    siblings.sort();
    let mut expected = vec![
        ".gitignore",
        "config.toml",
        "notes.txt",
        "skills",
        "themes",
        "unknown",
    ];
    if cfg!(windows) {
        expected.push("windows");
    }
    assert_eq!(siblings, expected);
}

#[test]
fn absent_empty_and_partial_storage_are_repeatable_no_ops_without_creation() {
    for (storage_exists, populated) in [
        (false, &[][..]),
        (true, &[][..]),
        (true, &["plans"][..]),
        (true, &["ensemble-sessions"][..]),
        (true, &["agent-runs", "sessions"][..]),
    ] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let storage = state_root(workspace.path());
        if storage_exists {
            fs::create_dir_all(&storage).unwrap();
        }
        for name in populated {
            populate_root(&storage.join(name));
        }
        for attempt in 0..2 {
            let output = cli(home.path(), workspace.path(), &["clean"]);
            let stdout = success(&output);
            let removed = if attempt == 0 { populated.len() } else { 0 };
            assert_eq!(stdout.matches("Removed:").count(), removed);
            assert_eq!(
                stdout.matches("Already absent:").count(),
                DIRECTORIES.len() - removed
            );
            assert_eq!(stdout.lines().count(), DIRECTORIES.len() + 1);
            for name in DIRECTORIES {
                assert_absent(&storage.join(name));
            }
            if storage_exists {
                assert!(storage.is_dir());
                assert!(fs::read_dir(&storage).unwrap().next().is_none());
            } else {
                assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
            }
            assert!(fs::read_dir(home.path()).unwrap().next().is_none());
        }
    }
}

#[test]
fn clean_ignores_missing_malformed_and_provider_free_configuration() {
    let configurations: [Option<&[u8]>; 3] =
        [None, Some(b"[providers\nmalformed ="), Some(b"[skills]\n")];
    for custom in [false, true] {
        for contents in configurations {
            let home = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let config_directory = tempfile::tempdir().unwrap();
            let config = if custom {
                config_directory.path().join("config/settings.toml")
            } else {
                home.path().join(".zevria/config.toml")
            };
            let preserved = populate(config_directory.path());
            if let Some(contents) = contents {
                fs::create_dir_all(config.parent().unwrap()).unwrap();
                fs::write(&config, contents).unwrap();
            }
            populate(workspace.path());
            let mut command = command(home.path(), workspace.path());
            if custom {
                command.env("ZEVRIA_CONFIG", &config);
            }
            let output = command.arg("clean").output().unwrap();
            assert_eq!(
                success(&output).matches("Removed:").count(),
                DIRECTORIES.len()
            );
            for name in DIRECTORIES {
                assert_absent(&state_root(workspace.path()).join(name));
            }
            assert!(
                fs::read_dir(state_root(workspace.path()))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert_kept(&preserved);
            if let Some(contents) = contents {
                assert_eq!(fs::read(&config).unwrap(), contents);
            } else {
                assert_absent(config.parent().unwrap());
            }
            if custom || contents.is_none() {
                assert!(fs::read_dir(home.path()).unwrap().next().is_none());
            } else {
                assert_eq!(
                    fs::read_dir(home.path().join(".zevria")).unwrap().count(),
                    1
                );
            }
        }
    }
}

#[test]
fn clean_does_not_even_require_a_home_environment_variable() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    populate(workspace.path());
    let output = command(home.path(), workspace.path())
        .env_remove("HOME")
        .arg("clean")
        .output()
        .unwrap();
    assert_eq!(
        success(&output).matches("Removed:").count(),
        DIRECTORIES.len()
    );
    assert!(
        fs::read_dir(state_root(workspace.path()))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(fs::read_dir(home.path()).unwrap().next().is_none());
}

#[test]
fn invalid_arguments_and_mixed_modes_fail_before_cleanup_or_configuration() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let preserved = populate(workspace.path());
    let mut combinations = vec![
        vec!["clean", "extra"],
        vec!["clean", "clean"],
        vec!["clean", "--force"],
        vec!["clean", "--yes"],
        vec!["clean", "--dry-run"],
        vec!["clean", "--path", "elsewhere"],
        vec!["clean", "sessions"],
        vec!["clean", "skills"],
        vec!["skills", "clean"],
        vec!["clean", "skills", "list"],
        vec!["skills", "list", "clean"],
        vec!["clean", "--acp", "--ensemble-worker"],
        vec!["--acp", "clean", "--ensemble-worker"],
        vec!["--acp", "--ensemble-worker", "clean"],
        vec!["--ensemble-worker", "--acp", "clean"],
    ];
    for flag in ["--continue", "-c", "--acp", "--ensemble-worker"] {
        combinations.push(vec!["clean", flag]);
        combinations.push(vec![flag, "clean"]);
    }
    for args in combinations {
        let output = cli(home.path(), workspace.path(), &args);
        let stderr = failure(&output);
        assert!(stderr.contains("usage:"), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_kept(&preserved);
        assert!(
            fs::read_dir(home.path()).unwrap().next().is_none(),
            "{args:?}"
        );
        assert_eq!(
            fs::read_dir(state_root(workspace.path())).unwrap().count(),
            DIRECTORIES.len()
        );
    }
}

#[test]
fn non_directory_storage_and_deletion_roots_fail_closed() {
    let state = if cfg!(windows) {
        ".zevria/windows"
    } else {
        ".zevria"
    };
    for invalid in std::iter::once(".zevria".to_string())
        .chain(cfg!(windows).then(|| state.to_string()))
        .chain(DIRECTORIES.map(|name| format!("{state}/{name}")))
    {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join(&invalid);
        let mut preserved = vec![write_fixture(path.clone())];
        if invalid != ".zevria" && invalid != state {
            for name in DIRECTORIES {
                let root = state_root(workspace.path()).join(name);
                if root != path {
                    preserved.extend(populate_root(&root));
                }
            }
        }
        let output = cli(home.path(), workspace.path(), &["clean"]);
        let stderr = failure(&output);
        assert!(stderr.contains("non-directory root"), "{stderr}");
        assert!(
            stderr.contains(&format!("{:?}", path.canonicalize().unwrap())),
            "{stderr}"
        );
        assert!(stderr.contains("no directories were removed"), "{stderr}");
        assert!(output.stdout.is_empty());
        assert_kept(&preserved);
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_storage_and_all_deletion_roots_including_dangling_links_fail_closed() {
    use std::os::unix::fs::symlink;

    for invalid in std::iter::once(".zevria".to_string())
        .chain(DIRECTORIES.map(|name| format!(".zevria/{name}")))
    {
        for kind in ["directory", "file", "missing"] {
            let home = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let directory = outside.path().join("directory");
            let mut preserved = Vec::new();
            for name in DIRECTORIES {
                preserved.extend(populate_root(&directory.join(name)));
            }
            preserved.push(write_fixture(outside.path().join("file")));
            let path = workspace.path().join(&invalid);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let referent = outside.path().join(kind);
            symlink(&referent, &path).unwrap();
            if invalid != ".zevria" {
                for name in DIRECTORIES {
                    let root = state_root(workspace.path()).join(name);
                    if root != path {
                        preserved.extend(populate_root(&root));
                    }
                }
            }
            let output = cli(home.path(), workspace.path(), &["clean"]);
            let stderr = failure(&output);
            assert!(stderr.contains("symlinked root"), "{stderr}");
            assert!(stderr.contains(&path.display().to_string()), "{stderr}");
            assert!(stderr.contains("no directories were removed"), "{stderr}");
            assert!(output.stdout.is_empty());
            assert_eq!(fs::read_link(path).unwrap(), referent);
            assert_kept(&preserved);
            assert_absent(&outside.path().join("missing"));
            assert!(fs::read_dir(home.path()).unwrap().next().is_none());
        }
    }
}

#[cfg(unix)]
#[test]
fn nested_symlinks_are_removed_without_touching_any_referents() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    populate(workspace.path());
    let preserved = [
        write_fixture(outside.path().join("directory/keep")),
        write_fixture(outside.path().join("file")),
        write_fixture(workspace.path().join(".zevria/skills/keep.md")),
    ];
    for name in DIRECTORIES {
        let root = state_root(workspace.path()).join(name);
        for kind in ["directory", "file", "missing"] {
            symlink(outside.path().join(kind), root.join(format!("{kind}-link"))).unwrap();
        }
        symlink("../skills", root.join("skills-link")).unwrap();
        symlink("../..", root.join("workspace-link")).unwrap();
        symlink("../sessions", root.join("peer-or-cycle-link")).unwrap();
    }
    let output = cli(home.path(), workspace.path(), &["clean"]);
    assert_eq!(
        success(&output).matches("Removed:").count(),
        DIRECTORIES.len()
    );
    for name in DIRECTORIES {
        assert_absent(&state_root(workspace.path()).join(name));
    }
    assert_kept(&preserved);
    assert_absent(&outside.path().join("missing"));
    assert_eq!(
        fs::read_dir(state_root(workspace.path())).unwrap().count(),
        1
    );
    assert!(fs::read_dir(home.path()).unwrap().next().is_none());
}

#[cfg(unix)]
struct RestrictedPermissions {
    path: PathBuf,
    original: fs::Permissions,
}

#[cfg(unix)]
impl RestrictedPermissions {
    fn new(path: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let original = fs::metadata(path).unwrap().permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        Self {
            path: path.to_path_buf(),
            original,
        }
    }
}

#[cfg(unix)]
impl Drop for RestrictedPermissions {
    fn drop(&mut self) {
        fs::set_permissions(&self.path, self.original.clone()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn inspection_permission_errors_report_the_path_without_any_deletion() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let preserved = populate(workspace.path());
    let storage = workspace.path().join(".zevria");
    let permissions = RestrictedPermissions::new(&storage);
    let first = storage.join("agent-runs");
    match fs::symlink_metadata(&first) {
        Ok(_) => {
            eprintln!(
                "skipping permission assertion: current privileges bypass directory permissions"
            );
            return;
        }
        Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
    }
    let output = cli(home.path(), workspace.path(), &["clean"]);
    drop(permissions);
    let stderr = failure(&output);
    assert!(stderr.contains("failed to inspect"), "{stderr}");
    assert!(stderr.contains(&first.display().to_string()), "{stderr}");
    assert!(stderr.contains("no directories were removed"), "{stderr}");
    assert!(output.stdout.is_empty());
    assert_kept(&preserved);
    assert!(fs::read_dir(home.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn removal_permission_errors_report_partial_progress_and_stop_before_later_roots() {
    for first_exists in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let storage = workspace.path().join(".zevria");
        if first_exists {
            populate_root(&storage.join("agent-runs"));
        }
        let mut preserved = Vec::new();
        for name in ["plans", "sessions", "subsessions", "ensemble-sessions"] {
            preserved.extend(populate_root(&storage.join(name)));
        }
        let blocked = storage.join("plans");
        let permissions = RestrictedPermissions::new(&blocked);
        match fs::read_dir(&blocked) {
            Ok(_) => {
                eprintln!(
                    "skipping permission assertion: current privileges bypass directory permissions"
                );
                return;
            }
            Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
        }
        let output = cli(home.path(), workspace.path(), &["clean"]);
        drop(permissions);
        let stderr = failure(&output);
        assert!(stderr.contains("failed to remove"), "{stderr}");
        assert!(stderr.contains(&blocked.display().to_string()), "{stderr}");
        assert!(stderr.contains("cleanup may be partial"), "{stderr}");
        assert!(
            stderr.contains("later directories were not attempted"),
            "{stderr}"
        );
        let removed = usize::from(first_exists);
        let absent = usize::from(!first_exists);
        assert!(
            stderr.contains(&format!(
                "directories removed: {removed}, already absent: {absent}"
            )),
            "{stderr}"
        );
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 2, "{stdout}");
        assert_eq!(stdout.matches("Removed:").count(), removed);
        assert_eq!(stdout.matches("Already absent:").count(), absent);
        let status = if first_exists {
            "Removed"
        } else {
            "Already absent"
        };
        let first = workspace
            .path()
            .canonicalize()
            .unwrap()
            .join(".zevria/agent-runs");
        assert_eq!(
            stdout.lines().nth(1).unwrap(),
            format!("{status}: {first:?}")
        );
        assert_absent(&storage.join("agent-runs"));
        assert_kept(&preserved);
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
    }
}

#[cfg(unix)]
#[test]
fn final_worker_removal_permission_errors_report_four_completed_removals() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let storage = workspace.path().join(".zevria");
    let blocked = storage.join("ensemble-sessions");
    let preserved = populate(workspace.path())
        .into_iter()
        .filter(|path| path.starts_with(&blocked))
        .collect::<Vec<_>>();
    let permissions = RestrictedPermissions::new(&blocked);
    match fs::read_dir(&blocked) {
        Ok(_) => {
            eprintln!(
                "skipping permission assertion: current privileges bypass directory permissions"
            );
            return;
        }
        Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
    }
    let output = cli(home.path(), workspace.path(), &["clean"]);
    drop(permissions);
    let stderr = failure(&output);
    assert!(stderr.contains("failed to remove"), "{stderr}");
    assert!(stderr.contains(&blocked.display().to_string()), "{stderr}");
    assert!(
        stderr.contains("directories removed: 4, already absent: 0"),
        "{stderr}"
    );
    assert!(stderr.contains("cleanup may be partial"), "{stderr}");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let canonical = workspace.path().canonicalize().unwrap();
    let earlier = &DIRECTORIES[..DIRECTORIES.len() - 1];
    let expected = std::iter::once(format!("Workspace: {canonical:?}"))
        .chain(
            earlier
                .iter()
                .map(|name| format!("Removed: {:?}", state_root(&canonical).join(name))),
        )
        .collect::<Vec<_>>();
    assert_eq!(stdout.lines().collect::<Vec<_>>(), expected);
    for name in earlier {
        assert_absent(&storage.join(name));
    }
    assert!(blocked.is_dir());
    assert_kept(&preserved);
    assert!(fs::read_dir(home.path()).unwrap().next().is_none());
}
