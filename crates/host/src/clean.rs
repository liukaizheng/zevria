//! Explicit offline workspace cleanup. Callers must stop all workspace sessions
//! and workers first; preflight is not protection against concurrent replacement.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::Context as _;

const DIRECTORIES: [&str; 5] = [
    "agent-runs",
    "plans",
    "sessions",
    "subsessions",
    "ensemble-sessions",
];

pub fn run(workspace: &Path) -> anyhow::Result<()> {
    run_with_output(workspace, &mut io::stdout().lock())
}

fn run_with_output(workspace: &Path, output: &mut impl Write) -> anyhow::Result<()> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace directory {workspace:?}"))?;
    anyhow::ensure!(
        fs::metadata(&workspace)
            .with_context(|| format!("failed to inspect workspace directory {workspace:?}"))?
            .is_dir(),
        "workspace {workspace:?} is not a directory"
    );
    let targets = preflight(&workspace).with_context(|| {
        format!("cleanup preflight failed in workspace {workspace:?}; no directories were removed")
    })?;

    writeln!(output, "Workspace: {workspace:?}")
        .context("failed to write cleanup results; no directories were removed")?;
    let mut removed = 0;
    let mut absent = 0;
    for (path, exists) in targets {
        let did_remove = if exists {
            // The standard library removes nested symlinks, not their referents.
            match fs::remove_dir_all(&path) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "cleanup stopped: failed to remove {path:?} in workspace {workspace:?}; \
                             directories removed: {removed}, already absent: {absent}; \
                             cleanup may be partial; later directories were not attempted"
                        )
                    });
                }
            }
        } else {
            false
        };
        let status = if did_remove {
            removed += 1;
            "Removed"
        } else {
            absent += 1;
            "Already absent"
        };
        writeln!(output, "{status}: {path:?}").with_context(|| {
            format!(
                "cleanup stopped: failed to report result for {path:?}; \
                 directories removed: {removed}, already absent: {absent}; \
                 cleanup may be partial; later directories were not attempted"
            )
        })?;
    }
    Ok(())
}

/// Validate every deletion root before the first removal. Do not canonicalize
/// these paths: that would hide symlinked roots and redirect the deletion scope.
fn preflight(workspace: &Path) -> anyhow::Result<Vec<(PathBuf, bool)>> {
    directory_exists(&workspace.join(".zevria"))?;
    let storage = zevria_foundation::runtime_paths::workspace_state_root(workspace);
    directory_exists(&storage)?;
    DIRECTORIES
        .iter()
        .map(|name| {
            let path = storage.join(name);
            let exists = directory_exists(&path)?;
            Ok((path, exists))
        })
        .collect()
}

fn directory_exists(path: &Path) -> anyhow::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {path:?}"));
        }
    };
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "refusing to clean symlinked root {path:?}"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "refusing to clean reparse-point root {path:?}"
        );
    }
    anyhow::ensure!(
        metadata.is_dir(),
        "refusing to clean non-directory root {path:?}"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::runtime_paths::workspace_state_root as storage;

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn populate(workspace: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for name in DIRECTORIES {
            for artifact in [
                ".gitignore",
                ".leases.lock",
                ".hidden/nested/secret",
                "nested/artifact.bin",
                "malformed.jsonl",
                "root.jsonl.lock",
            ] {
                let path = storage(workspace).join(name).join(artifact);
                write(&path, b"customized or malformed data\n\xff");
                files.push(path);
            }
        }
        files
    }

    fn clean(workspace: &Path) -> String {
        let mut output = Vec::new();
        run_with_output(workspace, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn assert_absent(path: &Path) {
        assert_eq!(
            fs::symlink_metadata(path).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "{path:?}"
        );
    }

    #[test]
    fn removes_whole_directories_and_preserves_unlisted_siblings() {
        let workspace = tempfile::tempdir().unwrap();
        populate(workspace.path());
        let preserved = [
            ".zevria/skills/demo.md",
            ".zevria/config.toml",
            ".zevria/models.jsonc",
            ".zevria/unknown/nested/file",
            ".zevria/notes",
            ".cazean/sessions/legacy.jsonl",
            "ordinary.txt",
        ];
        for path in preserved {
            write(&workspace.path().join(path), b"keep exactly");
        }

        let output = clean(workspace.path());
        assert!(output.starts_with(&format!(
            "Workspace: {:?}\n",
            workspace.path().canonicalize().unwrap()
        )));
        assert_eq!(output.lines().count(), DIRECTORIES.len() + 1);
        assert_eq!(output.matches("Removed:").count(), DIRECTORIES.len());
        for name in DIRECTORIES {
            assert_absent(&storage(workspace.path()).join(name));
        }
        assert!(workspace.path().join(".zevria").is_dir());
        for path in preserved {
            assert_eq!(
                fs::read(workspace.path().join(path)).unwrap(),
                b"keep exactly"
            );
        }
    }

    #[test]
    fn absent_storage_stays_absent_across_repeated_cleanup() {
        let workspace = tempfile::tempdir().unwrap();
        for _ in 0..2 {
            let output = clean(workspace.path());
            assert_eq!(output.matches("Already absent:").count(), DIRECTORIES.len());
            assert!(!output.contains("Removed:"));
            assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
        }
    }

    #[test]
    fn partially_populated_storage_is_idempotent_without_replacement_guards() {
        let workspace = tempfile::tempdir().unwrap();
        write(&storage(workspace.path()).join("sessions/.gitignore"), b"*");
        fs::create_dir_all(storage(workspace.path()).join("agent-runs")).unwrap();
        let output = clean(workspace.path());
        assert_eq!(output.matches("Removed:").count(), 2);
        assert_eq!(
            output.matches("Already absent:").count(),
            DIRECTORIES.len() - 2
        );
        for name in DIRECTORIES {
            let path = storage(&workspace.path().canonicalize().unwrap()).join(name);
            let status = if matches!(name, "sessions" | "agent-runs") {
                "Removed"
            } else {
                "Already absent"
            };
            assert!(output.contains(&format!("{status}: {path:?}")));
        }
        assert_eq!(
            clean(workspace.path()).matches("Already absent:").count(),
            DIRECTORIES.len()
        );
        assert!(
            fs::read_dir(storage(workspace.path()))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn workspace_must_resolve_to_an_existing_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("file");
        fs::write(&file, b"keep").unwrap();
        for path in [fixture.path().join("missing"), file.clone()] {
            let mut output = Vec::new();
            let error = run_with_output(&path, &mut output).unwrap_err();
            let expected = path.canonicalize().unwrap_or_else(|_| path.clone());
            assert!(format!("{error:#}").contains(&format!("{expected:?}")));
            assert!(output.is_empty());
        }
        assert_absent(&fixture.path().join("missing"));
        assert_eq!(fs::read(file).unwrap(), b"keep");
        assert_absent(&fixture.path().join(".zevria"));
    }

    #[test]
    fn non_directory_roots_fail_preflight_without_deleting_any_target() {
        let state = if cfg!(windows) {
            ".zevria/windows"
        } else {
            ".zevria"
        };
        for invalid in std::iter::once(".zevria".to_string())
            .chain(cfg!(windows).then(|| state.to_string()))
            .chain(DIRECTORIES.map(|name| format!("{state}/{name}")))
        {
            let workspace = tempfile::tempdir().unwrap();
            let invalid_path = workspace.path().join(&invalid);
            write(&invalid_path, b"not a directory");
            let mut preserved = Vec::new();
            if invalid != ".zevria" && invalid != state {
                for name in DIRECTORIES {
                    let root = storage(workspace.path()).join(name);
                    if root != invalid_path {
                        let path = root.join("keep");
                        write(&path, b"keep");
                        preserved.push(path);
                    }
                }
            }
            let mut output = Vec::new();
            let error = run_with_output(workspace.path(), &mut output).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("non-directory root"), "{message}");
            assert!(
                message.contains(&format!("{:?}", invalid_path.canonicalize().unwrap())),
                "{message}"
            );
            assert!(message.contains("no directories were removed"), "{message}");
            assert!(output.is_empty());
            assert_eq!(fs::read(invalid_path).unwrap(), b"not a directory");
            for path in preserved {
                assert_eq!(fs::read(path).unwrap(), b"keep");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_and_dangling_last_roots_do_not_allow_earlier_deletion() {
        use std::os::unix::fs::symlink;

        for dangling in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let referent = outside.path().join("referent");
            if !dangling {
                write(&referent.join("keep"), b"keep");
            }
            let mut preserved = Vec::new();
            for name in ["agent-runs", "plans", "sessions", "subsessions"] {
                let path = storage(workspace.path()).join(name).join("keep");
                write(&path, b"keep");
                preserved.push(path);
            }
            let link = workspace.path().join(".zevria/ensemble-sessions");
            symlink(&referent, &link).unwrap();
            let mut output = Vec::new();
            let error = run_with_output(workspace.path(), &mut output).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("symlinked root"), "{message}");
            assert!(message.contains(&link.display().to_string()), "{message}");
            assert!(message.contains("no directories were removed"), "{message}");
            assert!(output.is_empty());
            assert_eq!(fs::read_link(link).unwrap(), referent);
            for path in preserved {
                assert_eq!(fs::read(path).unwrap(), b"keep");
            }
            if dangling {
                assert_absent(&referent);
            } else {
                assert_eq!(fs::read(referent.join("keep")).unwrap(), b"keep");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonicalizes_the_workspace_but_does_not_follow_nested_symlinks() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        populate(workspace.path());
        write(&outside.path().join("directory/keep"), b"keep");
        write(&outside.path().join("file"), b"keep file");
        let alias = outside.path().join("workspace-alias");
        symlink(workspace.path(), &alias).unwrap();
        for name in DIRECTORIES {
            let root = storage(workspace.path()).join(name);
            symlink(
                outside.path().join("directory"),
                root.join("directory-link"),
            )
            .unwrap();
            symlink(outside.path().join("file"), root.join("file-link")).unwrap();
            symlink(outside.path().join("missing"), root.join("dangling-link")).unwrap();
            symlink("../..", root.join("workspace-link")).unwrap();
        }
        let output = clean(&alias);
        assert!(output.contains(&format!(
            "Workspace: {:?}",
            workspace.path().canonicalize().unwrap()
        )));
        assert!(!output.contains("workspace-alias"));
        for name in DIRECTORIES {
            assert_absent(&storage(workspace.path()).join(name));
        }
        assert_eq!(
            fs::read(outside.path().join("directory/keep")).unwrap(),
            b"keep"
        );
        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"keep file");
        assert_absent(&outside.path().join("missing"));
        assert_eq!(fs::read_link(alias).unwrap(), workspace.path());
        assert!(workspace.path().join(".zevria").is_dir());
    }

    #[test]
    fn output_failure_stops_cleanup_and_reports_completed_removals() {
        for fail_after_header in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let files = populate(workspace.path());
            let header = format!(
                "Workspace: {:?}\n",
                workspace.path().canonicalize().unwrap()
            );
            let mut buffer = vec![0; if fail_after_header { header.len() } else { 0 }];
            let error = run_with_output(
                workspace.path(),
                &mut io::Cursor::new(buffer.as_mut_slice()),
            )
            .unwrap_err();
            let message = format!("{error:#}");
            if fail_after_header {
                assert!(message.contains("failed to report result"), "{message}");
                assert!(message.contains("directories removed: 1"), "{message}");
                assert!(message.contains("cleanup may be partial"), "{message}");
                assert_absent(&storage(workspace.path()).join("agent-runs"));
            } else {
                assert!(message.contains("no directories were removed"), "{message}");
            }
            for path in files {
                if !fail_after_header
                    || !path.starts_with(storage(workspace.path()).join("agent-runs"))
                {
                    assert_eq!(
                        fs::read(path).unwrap(),
                        b"customized or malformed data\n\xff"
                    );
                }
            }
        }
    }
}
