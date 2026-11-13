use super::test_zevria_executable;

#[test]
fn helper_discovery_supports_deps_and_per_package_artifacts_in_any_profile() {
    for profile in ["debug", "release", "custom-profile"] {
        for relative in [
            "deps/ensemble-test",
            "build/zevria-ensemble/40a0c40d3ad73f80/bin/ensemble-test",
            "build/zevria-ensemble/40a0c40d3ad73f80/deps/ensemble-test",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let profile_directory = directory.path().join("target").join(profile);
            let test_executable = profile_directory.join(relative);
            std::fs::create_dir_all(test_executable.parent().unwrap()).unwrap();
            std::fs::write(&test_executable, b"test harness").unwrap();
            let helper = profile_directory.join(format!("zevria{}", std::env::consts::EXE_SUFFIX));
            std::fs::write(&helper, b"real helper").unwrap();

            assert_eq!(test_zevria_executable(&test_executable), Some(helper));
        }
    }
}

#[test]
fn helper_discovery_rejects_missing_helpers_and_directories() {
    let directory = tempfile::tempdir().unwrap();
    let profile_directory = directory.path().join("target/release");
    let test_executable = profile_directory.join("deps/ensemble-test");
    std::fs::create_dir_all(test_executable.parent().unwrap()).unwrap();
    std::fs::write(&test_executable, b"test harness").unwrap();
    assert!(test_zevria_executable(&test_executable).is_none());

    let helper = profile_directory.join(format!("zevria{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir(helper).unwrap();
    assert!(test_zevria_executable(&test_executable).is_none());
}

#[test]
fn helper_discovery_never_selects_the_test_harness_itself() {
    let directory = tempfile::tempdir().unwrap();
    let test_executable = directory
        .path()
        .join(format!("zevria{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&test_executable, b"test harness").unwrap();

    assert!(test_zevria_executable(&test_executable).is_none());
}
