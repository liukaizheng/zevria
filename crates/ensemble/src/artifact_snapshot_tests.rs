use super::*;

#[test]
fn snapshot_revalidates_file_and_directory_bindings_after_read() {
    for scenario in [
        "rewrite",
        "replace",
        #[cfg(unix)]
        "symlink",
        "hardlink",
        "directory",
        #[cfg(unix)]
        "ancestor",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("workspace");
        let config = root.join("config");
        let directory = config.join("plans");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("plan.md");
        std::fs::write(&path, "# Snapshot").unwrap();
        let snapshot = ArtifactSnapshot {
            path: path.clone(),
            artifact_tool_id: "write".into(),
            evidence: ArtifactEvidence {
                directory_identity: Some(
                    OpenedRoot::open_absolute(&directory)
                        .unwrap()
                        .identity()
                        .unwrap(),
                ),
                file_version: artifact_file_snapshot(&path),
                whole_file_content: None,
            },
            artifact_directory: directory.clone(),
            workspace_artifact_directory: workspace.join(".claude/plans"),
            workspace,
        };
        let result = snapshot.read_inner_with(|| match scenario {
            "rewrite" => std::fs::write(&path, "# Changed snapshot").unwrap(),
            "replace" => {
                std::fs::rename(&path, directory.join("old.md")).unwrap();
                std::fs::write(&path, "# Snapshot").unwrap();
            }
            #[cfg(unix)]
            "symlink" => {
                std::fs::rename(&path, directory.join("old.md")).unwrap();
                std::os::unix::fs::symlink("old.md", &path).unwrap();
            }
            "hardlink" => std::fs::hard_link(&path, directory.join("linked.md")).unwrap(),
            "directory" => {
                std::fs::rename(&directory, config.join("old-plans")).unwrap();
                std::fs::create_dir(&directory).unwrap();
                std::fs::write(&path, "# Snapshot").unwrap();
            }
            #[cfg(unix)]
            "ancestor" => {
                std::fs::rename(&config, root.join("old-config")).unwrap();
                std::os::unix::fs::symlink("old-config", &config).unwrap();
            }
            _ => unreachable!(),
        });
        assert!(result.is_err(), "{scenario}");
    }
}
