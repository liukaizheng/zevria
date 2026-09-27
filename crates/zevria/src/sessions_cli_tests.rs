//! Rejection and lease regressions after removing offline metadata initialization.
use super::*;

#[test]
fn removed_session_recovery_command_is_rejected() {
    for args in [
        vec!["sessions"],
        vec![
            "sessions",
            "recover-models",
            "root",
            "--build",
            "test",
            "model",
            "--plan",
            "test",
            "model",
        ],
    ] {
        let error = parse_args(args.into_iter().map(str::to_string)).unwrap_err();
        assert!(error.to_string().contains("unknown"), "{error}");
    }
}

#[test]
fn transcript_replacement_does_not_release_root_lease() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("root.jsonl");
    let original = b"{\"error\":\"history\"}\n";
    std::fs::write(&path, original).unwrap();
    let lease = zevria_app::test_support::RootSessionLease::acquire(&path).unwrap();
    zevria_foundation::atomic_file::replace(&path, original).unwrap();
    assert!(zevria_app::test_support::RootSessionLease::acquire(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!path.with_extension("jsonl.pre-session-models-v1").exists());
    drop(lease);
    assert!(!path.with_extension("jsonl.lock").exists());
    zevria_app::test_support::RootSessionLease::acquire(&path).unwrap();
}
