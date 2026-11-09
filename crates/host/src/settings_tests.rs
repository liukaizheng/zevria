use super::*;

#[test]
fn configuration_lock_drop_releases_a_retained_duplicate() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let lock = lock_configuration(&path).unwrap();
    // Deterministically model a descriptor retained by a concurrent spawn.
    let inherited = lock.file.try_clone().unwrap();
    let error = lock_configuration(&path)
        .err()
        .expect("another owner must not acquire the active lock");
    assert!(error.to_string().contains("busy"));

    drop(lock);
    let reacquired = lock_configuration(&path)
        .expect("a retained duplicate must not extend the transaction's lock lifetime");
    drop(reacquired);
    drop(inherited);
}

#[test]
fn prepared_update_releases_a_retained_duplicate_after_commit_or_drop() {
    for commit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# before\n").unwrap();
        let prepared =
            prepare_create_or_update(&path, "", |_| Ok("# after\n".to_string())).unwrap();
        let inherited = prepared._lock.file.try_clone().unwrap();
        let error = transaction(&path, |before| Ok((before.to_string(), ())))
            .expect_err("a prepared update must retain the configuration lock");
        assert!(error.to_string().contains("busy"));

        if commit {
            prepared.commit().unwrap();
        } else {
            drop(prepared);
        }
        transaction(&path, |before| {
            assert_eq!(before, if commit { "# after\n" } else { "# before\n" });
            Ok((before.to_string(), ()))
        })
        .expect("finishing the prepared update must release its lock before duplicates close");
        drop(inherited);
    }
}
