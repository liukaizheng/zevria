use super::*;
use std::{fs, sync::mpsc, time::Duration};
use zevria_tui_input::completion::{CompletionKind, CompletionQuery, FileQueryIdentity};

fn files(root: &Path, names: &[&str]) {
    for name in names {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, [0, 255, 0, 127]).unwrap();
    }
}

fn isolated() -> ScanOptions {
    ScanOptions {
        parents: false,
        global: false,
        ..Default::default()
    }
}

fn discover(root: &Path) -> Index {
    scan(root, isolated(), &|| false).unwrap()
}

fn rank(path: &str, query: &str) -> Option<Rank> {
    super::rank(path, query, &query.chars().collect::<Vec<_>>(), &|| false)
}

#[test]
fn discovery_includes_binary_and_nonignored_hidden_files_but_not_git_metadata() {
    let fixture = tempfile::tempdir().unwrap();
    files(
        fixture.path(),
        &[
            "src/main.rs",
            "tests/main.rs",
            "binary.bin",
            ".config/tool/settings",
            ".visible",
            "ignored.txt",
            "ignored-dir/file",
            ".secret",
            "ignore-me.bin",
            "excluded.txt",
            ".git/objects/blob",
        ],
    );
    fs::write(
        fixture.path().join(".gitignore"),
        "ignored.txt\nignored-dir/\n.secret\n",
    )
    .unwrap();
    fs::write(fixture.path().join(".ignore"), "ignore-me.bin\n").unwrap();
    fs::create_dir_all(fixture.path().join(".git/info")).unwrap();
    fs::write(fixture.path().join(".git/info/exclude"), "excluded.txt\n").unwrap();
    let index = discover(fixture.path());
    assert_eq!(
        index.paths,
        [
            ".config/tool/settings",
            ".gitignore",
            ".ignore",
            ".visible",
            "binary.bin",
            "src/main.rs",
            "tests/main.rs"
        ]
    );
    assert_eq!(index.status, FileSearchStatus::default());
}

#[test]
fn gitignore_works_without_git_and_parent_rules_apply_to_workspace_roots() {
    let fixture = tempfile::tempdir().unwrap();
    files(fixture.path(), &["keep", "drop"]);
    fs::write(fixture.path().join(".gitignore"), "drop\n").unwrap();
    assert_eq!(discover(fixture.path()).paths, [".gitignore", "keep"]);

    // A nearest-parent whitelist isolates this parent-rule fixture from any
    // ambient ancestor rules, without mutating HOME or global git config.
    fs::write(fixture.path().join(".ignore"), "!**\n*.skip\n").unwrap();
    files(
        fixture.path(),
        &[
            "workspace/keep",
            "workspace/from-parent.skip",
            "workspace/nested/also.skip",
        ],
    );
    let options = ScanOptions {
        parents: true,
        ..isolated()
    };
    let index = scan(&fixture.path().join("workspace"), options, &|| false).unwrap();
    assert_eq!(index.paths, ["keep"]);
}

#[cfg(unix)]
#[test]
fn file_symlinks_must_resolve_to_regular_files_inside_root_and_directories_are_not_followed() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    files(fixture.path(), &["nested/file.bin"]);
    files(outside.path(), &["outside.bin"]);
    symlink("nested/file.bin", fixture.path().join("inside-link")).unwrap();
    symlink("nested", fixture.path().join("dir-link")).unwrap();
    symlink("does-not-exist", fixture.path().join("broken")).unwrap();
    symlink(
        outside.path().join("outside.bin"),
        fixture.path().join("outside-link"),
    )
    .unwrap();
    let index = discover(fixture.path());
    assert_eq!(index.paths, ["inside-link", "nested/file.bin"]);
    assert_eq!(index.status.omitted, 3);
}

#[cfg(unix)]
#[test]
fn unsafe_names_are_omitted_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = tempfile::tempdir().unwrap();
    files(
        fixture.path(),
        &[
            "normal",
            "has\nnewline",
            "has\u{1b}escape",
            "safe space\\quote\"",
        ],
    );
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(selectable_path(&invalid).is_none());
    // Some filesystems (including APFS) reject non-UTF-8 names themselves.
    let invalid_created = fs::write(fixture.path().join(invalid), b"x").is_ok();
    let index = discover(fixture.path());
    assert_eq!(index.paths, ["normal", "safe space\\quote\""]);
    assert_eq!(index.status.omitted, 2 + usize::from(invalid_created));
}

#[test]
fn limits_bound_files_path_storage_entries_depth_and_returned_matches() {
    assert_eq!(
        (
            LIMITS.files,
            LIMITS.path_bytes,
            LIMITS.entries,
            LIMITS.depth,
            LIMITS.matches
        ),
        (100_000, 32 * 1024 * 1024, 200_000, 64, 50)
    );
    let fixture = tempfile::tempdir().unwrap();
    files(fixture.path(), &["a.rs", "b.rs", "c.rs", "dir/deep/d.rs"]);
    for limits in [
        Limits { files: 2, ..LIMITS },
        Limits {
            path_bytes: 5,
            ..LIMITS
        },
        Limits {
            entries: 2,
            ..LIMITS
        },
        Limits { depth: 1, ..LIMITS },
        Limits { files: 0, ..LIMITS },
        Limits {
            entries: 0,
            ..LIMITS
        },
    ] {
        let index = scan(
            fixture.path(),
            ScanOptions {
                limits,
                ..isolated()
            },
            &|| false,
        )
        .unwrap();
        assert!(index.status.incomplete, "{limits:?}");
        assert!(index.paths.len() <= limits.files);
        assert!(index.paths.iter().map(String::len).sum::<usize>() <= limits.path_bytes);
        assert!(
            index
                .paths
                .iter()
                .all(|p| Path::new(p).components().count() <= limits.depth)
        );
    }
    let index = discover(fixture.path());
    assert_eq!(matches(&index, "", 2, &|| false).unwrap(), ["a.rs", "b.rs"]);
    assert_eq!(matches(&index, "rs", 2, &|| false).unwrap().len(), 2);
    assert!(matches(&index, "rs", 0, &|| false).unwrap().is_empty());
}

#[test]
fn matching_is_case_insensitive_ranked_and_has_stable_path_ties() {
    let mut index = Index {
        paths: [
            "z/APP",
            "a/app",
            "APPend.rs",
            "other/capplus.rs",
            "deep/a_p_p.txt",
            "long/axxxxxpxxxxxp.rs",
            "app.rs",
        ]
        .map(str::to_owned)
        .into(),
        ..Default::default()
    };
    index.paths.sort();
    assert_eq!(
        matches(&index, "aPP", 50, &|| false).unwrap(),
        [
            "a/app",
            "z/APP",
            "app.rs",
            "APPend.rs",
            "other/capplus.rs",
            "deep/a_p_p.txt",
            "long/axxxxxpxxxxxp.rs"
        ]
    );
    assert_eq!(
        matches(&index, "other/cap", 50, &|| false).unwrap(),
        ["other/capplus.rs"]
    );
    assert_eq!(matches(&index, "", 50, &|| false).unwrap(), index.paths);
    assert!(
        matches(&index, "missing", 50, &|| false)
            .unwrap()
            .is_empty()
    );
    assert!(rank("目录/文件.rs", "文件").is_some());
    assert_eq!(rank("axxxxxb-ab", "ab").unwrap().kind, 2);
    assert!(rank("axxbyy-azb", "ab").unwrap().looseness < rank("axxxxb", "ab").unwrap().looseness);
}

#[test]
fn discovered_references_use_slashes_and_rank_exact_basenames_before_prefixes() {
    let fixture = tempfile::tempdir().unwrap();
    files(
        fixture.path(),
        &[
            "z/app",
            "nested/exact/app",
            "app.rs",
            "nested/appended.rs",
            "a/APP",
        ],
    );
    let index = discover(fixture.path());
    assert_eq!(
        index.paths,
        [
            "a/APP",
            "app.rs",
            "nested/appended.rs",
            "nested/exact/app",
            "z/app"
        ]
    );
    assert_eq!(index.status, FileSearchStatus::default());
    assert_eq!(
        matches(&index, "aPP", 50, &|| false).unwrap(),
        [
            "a/APP",
            "z/app",
            "nested/exact/app",
            "app.rs",
            "nested/appended.rs"
        ]
    );
}

#[test]
fn path_byte_limit_counts_the_stored_relative_reference() {
    let fixture = tempfile::tempdir().unwrap();
    let reference = "nested/app";
    files(fixture.path(), &[reference]);
    for path_bytes in [reference.len() - 1, reference.len()] {
        let index = scan(
            fixture.path(),
            ScanOptions {
                limits: Limits {
                    path_bytes,
                    ..LIMITS
                },
                ..isolated()
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(index.status.incomplete, path_bytes < reference.len());
        if path_bytes < reference.len() {
            assert!(index.paths.is_empty());
        } else {
            assert_eq!(index.paths, [reference]);
        }
    }
}

#[cfg(unix)]
#[test]
fn literal_backslashes_survive_discovery_ranking_and_completion() {
    use zevria_tui_input::completion::{file_query, file_reference};

    let fixture = tempfile::tempdir().unwrap();
    files(fixture.path(), &[r"nested/a\app", "other/app", "app.rs"]);
    let index = discover(fixture.path());
    assert_eq!(index.paths, ["app.rs", r"nested/a\app", "other/app"]);
    assert_eq!(index.status, FileSearchStatus::default());
    assert_eq!(
        matches(&index, "app", 50, &|| false).unwrap(),
        ["other/app", "app.rs", r"nested/a\app"]
    );
    let selected = matches(&index, r"a\app", 50, &|| false).unwrap();
    assert_eq!(selected, [r"nested/a\app"]);
    assert_eq!(rank(&selected[0], r"a\app").unwrap().kind, 0);
    assert_eq!(rank(&selected[0], "app").unwrap().kind, 2);
    let reference = file_reference(&selected[0]);
    assert_eq!(reference, r#"@"nested/a\\app""#);
    assert_eq!(
        file_query(&reference, reference.len(), []).unwrap().prefix,
        selected[0]
    );
}

#[test]
fn repetitive_fuzzy_queries_have_linear_bounded_work() {
    let path = "abx".repeat(4_000);
    let query = "ab".repeat(2_000);
    let chars: Vec<_> = query.chars().collect();
    let checks = std::cell::Cell::new(0);
    assert!(
        super::rank(&path, &query, &chars, &|| {
            checks.set(checks.get() + 1);
            false
        })
        .is_some()
    );
    // Count cooperative checkpoints rather than asserting wall-clock timings.
    assert!(
        checks.get() < path.len() / 16,
        "overlapping-window quadratic work"
    );
}

#[test]
fn unavailable_and_disappearing_roots_are_distinct_from_no_matches() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("removed");
    fs::create_dir(&root).unwrap();
    files(&root, &["file"]);
    assert_eq!(discover(&root).paths, ["file"]);
    fs::remove_dir_all(&root).unwrap();
    let index = discover(&root);
    assert!(index.status.unavailable);
    assert!(index.paths.is_empty());
    let normal = discover(fixture.path());
    assert_eq!(normal.status, FileSearchStatus::default());
}

#[cfg(unix)]
#[test]
fn unreadable_directories_report_partial_results_without_reading_file_contents() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    files(
        fixture.path(),
        &["locked/file", "unreadable.bin", "visible"],
    );
    let locked = fixture.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o0)).unwrap();
    fs::set_permissions(
        fixture.path().join("unreadable.bin"),
        fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let denied = fs::read_dir(&locked).is_err();
    let index = discover(fixture.path());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(index.paths.contains(&"unreadable.bin".into()));
    if denied {
        assert!(index.status.errors > 0);
        assert!(!index.paths.contains(&"locked/file".into()));
    }
}

#[test]
fn scan_and_match_cooperate_with_cancellation() {
    let fixture = tempfile::tempdir().unwrap();
    files(fixture.path(), &["a", "b", "c"]);
    let calls = std::cell::Cell::new(0);
    assert!(
        scan(fixture.path(), isolated(), &|| {
            calls.set(calls.get() + 1);
            calls.get() > 2
        })
        .is_none()
    );
    let index = discover(fixture.path());
    assert!(matches(&index, "a", 50, &|| true).is_none());
}

fn request(
    service: ServiceId,
    pane: PaneId,
    activation: u64,
    id: u64,
    prefix: &str,
) -> SearchRequest {
    SearchRequest {
        service,
        pane,
        completion: FileCompletionRequest {
            activation,
            request: id,
            identity: FileQueryIdentity {
                generation: id,
                cursor: prefix.len() + 1,
                query: CompletionQuery {
                    kind: CompletionKind::File,
                    prefix: prefix.into(),
                    replacement: 0..prefix.len() + 1,
                },
            },
        },
    }
}

async fn result(
    service: &mut FileSearchService,
    request: &SearchRequest,
    loading: bool,
) -> SearchResult {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            service.receiver.changed().await.unwrap();
            let result = service.receiver.borrow_and_update().clone().unwrap();
            if result.request == *request && result.status.loading == loading {
                return result;
            }
        }
    })
    .await
    .expect("file worker did not respond")
}

#[tokio::test]
async fn one_lazy_worker_coalesces_queries_and_reuses_cache_while_refreshing() {
    let mut service = FileSearchService::new(PathBuf::from("/injected-only"), ServiceId::default());
    let (started, scans) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    service.scanner = Some(Box::new(move |_, cancelled| {
        started.send(()).unwrap();
        let paths: Vec<String> = gate.recv().ok()?;
        (!cancelled()).then_some(Index {
            paths,
            ..Default::default()
        })
    }));
    assert!(service.worker.is_none());
    let pane = PaneId::default();
    let first = request(service.id, pane, 1, 1, "");
    service.set_request(Some(first));
    scans.recv_timeout(Duration::from_secs(5)).unwrap();
    for id in 2..20 {
        service.set_request(Some(request(service.id, pane, 1, id, "ignored")));
    }
    let latest = request(service.id, pane, 1, 20, "b");
    service.set_request(Some(latest.clone()));
    release.send(vec!["a.rs".into(), "b.rs".into()]).unwrap();
    assert_eq!(result(&mut service, &latest, false).await.paths, ["b.rs"]);
    let edited = request(service.id, pane, 1, 21, "a");
    service.set_request(Some(edited.clone()));
    assert_eq!(result(&mut service, &edited, false).await.paths, ["a.rs"]);
    assert!(scans.try_recv().is_err(), "query edits must not scan");
    service.set_request(None);
    let reopened = request(service.id, pane, 2, 22, "");
    service.set_request(Some(reopened.clone()));
    scans.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        result(&mut service, &reopened, true).await.paths,
        ["a.rs", "b.rs"]
    );
    release.send(vec!["b.rs".into(), "c.rs".into()]).unwrap();
    assert_eq!(
        result(&mut service, &reopened, false).await.paths,
        ["b.rs", "c.rs"]
    );
    let handle = service.worker.take().unwrap();
    drop(service);
    handle.join().unwrap();
}

#[tokio::test]
async fn teardown_retires_blocked_work_and_foreign_service_requests_never_start() {
    let mut service = FileSearchService::new(PathBuf::new(), ServiceId::default());
    let pane = PaneId::default();
    service.set_request(Some(request(ServiceId::default(), pane, 1, 1, "")));
    assert!(service.worker.is_none());
    let (started, scans) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    service.scanner = Some(Box::new(move |_, cancelled| {
        started.send(()).unwrap();
        gate.recv().unwrap();
        assert!(cancelled());
        Some(Index {
            paths: vec!["retired".into()],
            ..Default::default()
        })
    }));
    service.set_request(Some(request(service.id, pane, 1, 1, "")));
    scans.recv_timeout(Duration::from_secs(5)).unwrap();
    let receiver = service.receiver.clone();
    let handle = service.worker.take().unwrap();
    drop(service); // Must not wait for the blocked scanner.
    release.send(()).unwrap();
    handle.join().unwrap();
    assert!(receiver.borrow().is_none());
}
