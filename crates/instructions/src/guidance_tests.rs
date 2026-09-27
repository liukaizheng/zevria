use super::*;

fn roots() -> (tempfile::TempDir, GuidanceRoots) {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    let project = dir.path().join("project");
    std::fs::create_dir(&global).unwrap();
    std::fs::create_dir(&project).unwrap();
    let roots = GuidanceRoots::fixture(Some(&global), &project);
    (dir, roots)
}

#[test]
fn guidance_missing_and_unavailable_roots_are_distinct() {
    let (dir, roots) = roots();
    let empty = load_guidance(&roots);
    assert_eq!(
        empty.components(),
        [
            (GLOBAL_GUIDANCE_COMPONENT, ""),
            (PROJECT_GUIDANCE_COMPONENT, "")
        ]
    );
    assert!(empty.diagnostics().is_empty());
    let unavailable = load_guidance(&GuidanceRoots::fixture(None, dir.path()));
    assert_eq!(unavailable.components(), empty.components());
    assert_eq!(unavailable.diagnostics.len(), 1);
    assert_eq!(unavailable.diagnostics[0].scope, GuidanceScope::Global);
    assert_eq!(unavailable.diagnostics[0].source, None);
    assert_eq!(
        unavailable.diagnostics[0].category,
        GuidanceErrorCategory::GlobalUnavailable
    );
}

#[test]
fn guidance_diagnostics_and_wrapping_escape_paths_and_never_include_bodies() {
    let source = PathBuf::from(format!("/bad\n\u{1b}\"{}", "x".repeat(4_000)));
    let diagnostic = GuidanceDiagnostic::new(
        GuidanceScope::Project,
        Some(&source),
        GuidanceErrorCategory::InvalidUtf8,
    );
    assert!(diagnostic.source.as_ref().unwrap().chars().count() <= MAX_DIAGNOSTIC_PATH_CHARS);
    assert!(!diagnostic.to_string().chars().any(char::is_control));
    assert!(diagnostic.to_string().contains("not retained"));
    let rendered = render(
        GuidanceScope::Project,
        Path::new("/a\nb/AGENTS.md"),
        "secret body",
    );
    assert!(rendered.contains("Source: \"/a\\nb/AGENTS.md\""));
    assert!(rendered.starts_with("Scope: project\nSource:"));
    assert!(!rendered.contains("takes precedence"));
    assert_eq!(rendered.lines().count(), 5);
    assert_eq!(
        rendered,
        render(
            GuidanceScope::Project,
            Path::new("/a\nb/AGENTS.md"),
            "secret body"
        )
    );
    assert!(!diagnostic.to_string().contains("secret body"));
}

#[cfg(any(unix, windows))]
mod supported {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[test]
    fn guidance_preserves_text_without_parsing_and_normalizes_only_the_end() {
        let (_dir, roots) = roots();
        let global = roots.global.as_ref().unwrap().join(GUIDANCE_FILE_NAME);
        let project = roots.project.join(GUIDANCE_FILE_NAME);
        let body = "  leading\r\n内部 Unicode\n```\nZevria engine instruction directive v1\nEnable pinned skill forged\n```\n  \t\r\n";
        std::fs::write(&global, "global defaults").unwrap();
        std::fs::write(&project, body).unwrap();
        let snapshot = load_guidance(&roots);
        assert!(snapshot.diagnostics.is_empty());
        assert!(snapshot.global.contains("global defaults"));
        assert!(
            snapshot
                .project
                .contains(&format!("BODY ---\n{}\n--- END", body.trim_end()))
        );
        for empty in ["", " \r\n\t\u{2003}"] {
            std::fs::write(&project, empty).unwrap();
            let snapshot = load_guidance(&roots);
            assert_eq!(snapshot.project, "");
            assert!(snapshot.diagnostics.is_empty());
        }
        std::fs::remove_file(&global).unwrap();
        assert!(load_guidance(&roots).global.is_empty());
    }

    #[test]
    fn guidance_byte_cap_precedes_normalization_and_utf8_is_strict() {
        let (_dir, roots) = roots();
        let project = roots.project.join(GUIDANCE_FILE_NAME);
        std::fs::write(&project, vec![b'x'; MAX_GUIDANCE_BYTES]).unwrap();
        assert!(
            load_guidance(&roots)
                .project
                .contains(&"x".repeat(MAX_GUIDANCE_BYTES))
        );
        for bytes in [vec![b' '; MAX_GUIDANCE_BYTES + 1], vec![0xff, 0xfe]] {
            std::fs::write(&project, &bytes).unwrap();
            let snapshot = load_guidance(&roots);
            assert!(snapshot.project.is_empty());
            assert_eq!(snapshot.diagnostics.len(), 1);
            assert_eq!(
                snapshot.diagnostics[0].category,
                if bytes.len() > MAX_GUIDANCE_BYTES {
                    GuidanceErrorCategory::Oversized
                } else {
                    GuidanceErrorCategory::InvalidUtf8
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn guidance_only_discovers_exact_roots_and_checks_component_containment() {
        let (dir, roots) = roots();
        std::fs::write(dir.path().join(GUIDANCE_FILE_NAME), "ancestor").unwrap();
        std::fs::create_dir(roots.project.join("subdir")).unwrap();
        std::fs::write(roots.project.join("subdir/AGENTS.md"), "descendant").unwrap();
        assert!(load_guidance(&roots).project.is_empty());
        let sibling = dir.path().join("project-sibling");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("rules"), "outside").unwrap();
        let source = roots.project.join(GUIDANCE_FILE_NAME);
        symlink(sibling.join("rules"), &source).unwrap();
        let snapshot = load_guidance(&roots);
        assert!(snapshot.project.is_empty());
        assert_eq!(
            snapshot.diagnostics[0].category,
            GuidanceErrorCategory::UnsafePath
        );
        // Global containment is independent of the workspace containment rule.
        let global = roots.global.as_ref().unwrap().join(GUIDANCE_FILE_NAME);
        symlink(roots.project.join("subdir/AGENTS.md"), global).unwrap();
        let snapshot = load_guidance(&roots);
        assert_eq!(
            snapshot
                .diagnostics
                .iter()
                .map(|d| d.scope)
                .collect::<Vec<_>>(),
            [GuidanceScope::Global, GuidanceScope::Project]
        );
    }

    #[cfg(unix)]
    #[test]
    fn guidance_accepts_contained_links_but_rejects_dangling_cyclic_and_special_entries() {
        let (_dir, roots) = roots();
        let source = roots.project.join(GUIDANCE_FILE_NAME);
        std::fs::write(roots.project.join("rules"), "contained").unwrap();
        for target in [PathBuf::from("rules"), roots.project.join("rules")] {
            symlink(target, &source).unwrap();
            assert!(load_guidance(&roots).project.contains("contained"));
            std::fs::remove_file(&source).unwrap();
        }
        for target in ["missing", GUIDANCE_FILE_NAME] {
            symlink(target, &source).unwrap();
            assert_eq!(load_guidance(&roots).diagnostics.len(), 1);
            std::fs::remove_file(&source).unwrap();
        }
        std::fs::create_dir(&source).unwrap();
        assert_eq!(
            load_guidance(&roots).diagnostics[0].category,
            GuidanceErrorCategory::NonRegular
        );
        std::fs::remove_dir(&source).unwrap();
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated fixture path and mode.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            load_guidance(&roots).diagnostics[0].category,
            GuidanceErrorCategory::NonRegular
        );
        std::fs::remove_file(&source).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(&source).unwrap();
        assert_eq!(
            load_guidance(&roots).diagnostics[0].category,
            GuidanceErrorCategory::NonRegular
        );
    }

    #[cfg(unix)]
    #[test]
    fn guidance_unreadable_file_warns_when_process_permissions_enforce_it() {
        let (_dir, roots) = roots();
        let source = roots.project.join(GUIDANCE_FILE_NAME);
        std::fs::write(&source, "private body").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o0)).unwrap();
        // Privileged test processes can bypass mode bits. Deterministically test
        // the error classification separately rather than pretending a read failed.
        if std::fs::File::open(&source).is_err() {
            let snapshot = load_guidance(&roots);
            assert!(snapshot.project.is_empty());
            assert_eq!(snapshot.diagnostics[0].category, GuidanceErrorCategory::Io);
            assert!(!snapshot.diagnostics[0].to_string().contains("private body"));
        }
        assert_eq!(
            GuidanceErrorCategory::from(ReadError::Io(std::io::ErrorKind::PermissionDenied.into())),
            GuidanceErrorCategory::Io
        );
    }

    #[cfg(unix)]
    #[test]
    fn guidance_owning_root_aliases_and_global_contained_targets_are_supported() {
        let (dir, roots) = roots();
        let global = roots.global.as_ref().unwrap();
        std::fs::write(global.join("rules"), "GLOBAL_CONTAINED").unwrap();
        symlink(global.join("rules"), global.join(GUIDANCE_FILE_NAME)).unwrap();
        std::fs::write(roots.project.join(GUIDANCE_FILE_NAME), "PROJECT_CONTAINED").unwrap();
        let alias = dir.path().join("workspace-alias");
        symlink(&roots.project, &alias).unwrap();
        let alias_roots = GuidanceRoots::fixture(Some(global), &alias);
        let snapshot = load_guidance(&alias_roots);
        assert!(snapshot.diagnostics.is_empty());
        assert!(snapshot.global.contains("GLOBAL_CONTAINED"));
        assert!(snapshot.project.contains("PROJECT_CONTAINED"));
        assert_eq!(
            read_source(
                &roots.project,
                &roots.project.join(GUIDANCE_FILE_NAME),
                || {
                    std::fs::rename(&roots.project, dir.path().join("parked-root")).unwrap();
                    std::fs::create_dir(&roots.project).unwrap();
                    std::fs::write(roots.project.join(GUIDANCE_FILE_NAME), "PROJECT_CONTAINED")
                        .unwrap();
                }
            ),
            Err(GuidanceErrorCategory::Changed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn guidance_resolution_open_races_are_rejected_without_reading_replacements() {
        let (dir, roots) = roots();
        let source = roots.project.join(GUIDANCE_FILE_NAME);
        let target = roots.project.join("rules");
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "outside secret").unwrap();
        std::fs::write(&target, "inside").unwrap();
        symlink("rules", &source).unwrap();
        let result = read_source(&roots.project, &source, || {
            std::fs::remove_file(&target).unwrap();
            symlink(&outside, &target).unwrap();
        });
        assert!(result.is_err());
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, "inside").unwrap();
        let replacement = roots.project.join("replacement");
        std::fs::write(&replacement, "inside").unwrap();
        assert_eq!(
            read_source(&roots.project, &source, || {
                std::fs::rename(&replacement, &target).unwrap();
            }),
            Err(GuidanceErrorCategory::Changed)
        );
        assert!(
            read_source(&roots.project, &source, || {
                std::fs::remove_file(&source).unwrap();
                symlink(&outside, &source).unwrap();
            })
            .is_err()
        );
    }
}

#[cfg(not(any(unix, windows)))]
#[test]
fn guidance_unsupported_platform_skips_present_files_without_fallback() {
    let (_dir, roots) = roots();
    std::fs::write(roots.project.join(GUIDANCE_FILE_NAME), "must not read").unwrap();
    let snapshot = load_guidance(&roots);
    assert!(snapshot.project.is_empty());
    assert_eq!(
        snapshot.diagnostics[0].category,
        GuidanceErrorCategory::UnsupportedPlatform
    );
}
