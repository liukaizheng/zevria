//! Root lease regression around transcript replacement.

#[test]
fn transcript_replacement_does_not_release_root_lease() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("root.jsonl");
    let original = b"{\"error\":\"history\"}\n";
    std::fs::write(&path, original).unwrap();
    let lease = zevria_host::test_support::RootSessionLease::acquire(&path).unwrap();
    zevria_foundation::atomic_file::replace(&path, original).unwrap();
    assert!(zevria_host::test_support::RootSessionLease::acquire(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!path.with_extension("jsonl.pre-session-models-v1").exists());
    drop(lease);
    assert!(!path.with_extension("jsonl.lock").exists());
    zevria_host::test_support::RootSessionLease::acquire(&path).unwrap();
}
