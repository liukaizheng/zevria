use super::*;
use crate::cache_diagnostics::{
    Completion, CompletionAuthority, Route,
    fingerprint::{ItemFingerprint, Properties},
};
use serde_json::json;

fn context(root: &Path) -> CacheDiagnosticContext {
    CacheDiagnosticContext::new(&root.join("session.jsonl"), "PRIVATE_SESSION")
}
fn profile() -> zevria_foundation::ModelProfileRef {
    zevria_foundation::ModelProfileRef::new("test", "model")
}
fn baseline(context: &CacheDiagnosticContext) -> Baseline {
    Baseline {
        authority: CompletionAuthority::ValidatedProviderCompletionNotTranscriptDurability,
        profile: profile_identity(&profile()),
        properties: Properties::new(
            &json!({"instructions":"PRIVATE_PROMPT", "tools":[{"description":"PRIVATE_TOOL"}]}),
        ),
        items: vec![ItemFingerprint::new(
            &json!({"type":"reasoning", "encrypted_content":"PRIVATE_ENCRYPTED"}),
        )],
        meta: Completion {
            runtime: context.runtime_id,
            request_id: 1,
            completed_at_ms: 1,
            input_count: 1,
            output_count: 0,
            historical_native: Default::default(),
            current_native: Default::default(),
            socket_generation: 0,
            wire: None,
            route: Route {
                endpoint: fingerprint::bytes(
                    b"https://USER:PASSWORD@host/URL_SECRET?token=QUERY_SECRET",
                ),
                header_name: Some("x-session-id".into()),
                header_identity: Some(fingerprint::bytes(b"x-session-id")),
                header_value: Some(fingerprint::bytes(b"PRIVATE_SESSION")),
            },
            raw: crate::cache_diagnostics::response::Observation::parse(&json!({"type":"response.completed", "response":{"id":"resp_synthetic", "prompt_cache_diagnostics":{"type":"cache_miss", "reason":"input_changed", "comparison_reusable_tokens":123, "cache_missed_tokens":17, "ignored":"PRIVATE_DIAGNOSTIC"}}}).to_string()),
            usage: None,
            upstream_request_id: None,
            connection_request_id: None,
        },
    }
}
#[cfg(unix)]
#[test]
fn private_roundtrip_profile_isolation_reset_and_interrupted_staging() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let context = context(dir.path());
    let store = context.store(&profile());
    assert_eq!(store.load().err(), Some("snapshot_missing"));
    let baseline = baseline(&context);
    store.save(&baseline).unwrap();
    let path = store.directory.join(&store.name);
    assert_eq!(
        std::fs::metadata(&store.directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let data = std::fs::read_to_string(&path).unwrap();
    for private in ["PRIVATE_", "USER", "PASSWORD", "URL_SECRET", "QUERY_SECRET"] {
        assert!(!data.contains(private), "{data}");
    }
    // A crash before atomic publication leaves the previous snapshot usable.
    std::fs::write(
        store.directory.join(".interrupted.tmp"),
        b"partial new data",
    )
    .unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.meta.runtime, context.runtime_id);
    assert_eq!(loaded.items, baseline.items);
    assert_eq!(loaded.properties, baseline.properties);
    assert_eq!(loaded.meta.raw, baseline.meta.raw);
    assert_eq!(
        context
            .store(&zevria_foundation::ModelProfileRef::new("other", "model"))
            .load()
            .err(),
        Some("snapshot_missing")
    );
    let next_context =
        CacheDiagnosticContext::new(&dir.path().join("session.jsonl"), "PRIVATE_SESSION");
    assert_ne!(next_context.runtime_id, context.runtime_id);
    assert_eq!(
        next_context.store(&profile()).load().unwrap().meta.runtime,
        context.runtime_id
    );
    store.invalidate(Invalidation::Reset).unwrap();
    assert_eq!(
        next_context.store(&profile()).load().err(),
        Some("persisted_reset")
    );
}
#[cfg(unix)]
#[test]
fn rejected_corrupt_version_identity_counts_oversize_and_unsafe_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let context = context(dir.path());
    let store = context.store(&profile());
    let baseline = baseline(&context);
    store.save(&baseline).unwrap();
    let path = store.directory.join(&store.name);
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (pointer, value, expected) in [
        ("/version", json!(999), "snapshot_version"),
        ("/session", json!("00".repeat(32)), "snapshot_identity"),
        ("/baseline/meta/input_count", json!(99), "snapshot_corrupt"),
        (
            "/baseline/meta/upstream_request_id",
            json!("unsafe\nCOOKIE_SECRET"),
            "snapshot_corrupt",
        ),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(store.load().err(), Some(expected));
    }
    // Real old schemas lack v2 fields, not just a changed version number.
    let mut old = original.clone();
    old["version"] = json!(1);
    old["baseline"]["meta"]
        .as_object_mut()
        .unwrap()
        .remove("historical_native");
    old["baseline"]["meta"]
        .as_object_mut()
        .unwrap()
        .remove("current_native");
    old["baseline"]["meta"]["raw"]
        .as_object_mut()
        .unwrap()
        .remove("comparison");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(store.load().err(), Some("snapshot_version"));
    std::fs::write(&path, b"{interrupted").unwrap();
    assert_eq!(store.load().err(), Some("snapshot_corrupt"));
    std::fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
    assert_eq!(store.load().err(), Some("snapshot_byte_limit"));
    let mut too_many = baseline.clone();
    too_many.items = vec![baseline.items[0]; MAX_ITEMS + 1];
    too_many.meta.input_count = MAX_ITEMS + 1;
    assert_eq!(store.save(&too_many), Err("snapshot_item_limit"));
    assert_eq!(store.load().err(), Some("snapshot_item_limit"));
}
#[cfg(unix)]
#[test]
fn reject_symlinks_hardlinks_special_files_and_unwritable_storage() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let context = context(dir.path());
    let store = context.store(&profile());
    let baseline = baseline(&context);
    store.save(&baseline).unwrap();
    let path = store.directory.join(&store.name);
    let outside = dir.path().join("outside");
    std::fs::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert_eq!(store.load().err(), Some("snapshot_symlink"));
    assert_eq!(store.save(&baseline), Err("snapshot_symlink"));
    assert!(
        std::fs::read_to_string(&outside)
            .unwrap()
            .contains("baseline")
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&outside, &path).unwrap();
    assert_eq!(store.load().err(), Some("snapshot_not_regular"));
    assert_eq!(store.save(&baseline), Err("snapshot_not_regular"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(store.save(&baseline), Err("snapshot_not_regular"));
    std::fs::remove_dir(&path).unwrap();
    std::fs::remove_dir(&store.directory).unwrap();
    symlink(dir.path(), &store.directory).unwrap();
    assert_eq!(store.load().err(), Some("storage_not_directory_or_symlink"));
    assert_eq!(
        store.save(&baseline),
        Err("storage_not_directory_or_symlink")
    );
    std::fs::remove_file(&store.directory).unwrap();
    std::fs::write(&store.directory, "not a directory").unwrap();
    assert_eq!(
        store.save(&baseline),
        Err("storage_not_directory_or_symlink")
    );
    std::fs::remove_file(&store.directory).unwrap();
    std::fs::create_dir(&store.directory).unwrap();
    std::fs::set_permissions(&store.directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root may override mode bits; ordinary users get a nonfatal write error.
    if unsafe { libc::geteuid() } != 0 {
        assert!(store.save(&baseline).is_err());
    }
    std::fs::set_permissions(&store.directory, std::fs::Permissions::from_mode(0o700)).unwrap();
}
