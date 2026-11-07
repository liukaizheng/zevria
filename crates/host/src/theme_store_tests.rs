use super::*;

fn palette() -> &'static ThemeDefinition {
    static DEFINITION: std::sync::OnceLock<ThemeDefinition> = std::sync::OnceLock::new();
    DEFINITION.get_or_init(|| zevria_theme::generate_theme("#1E1E2E".parse().unwrap()).unwrap())
}
fn fixture() -> (tempfile::TempDir, ThemeStore, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let store = ThemeStore {
        root: directory.path().join("themes"),
    };
    let config = directory.path().join("config.toml");
    (directory, store, config)
}

#[test]
fn names_are_exactly_safe_filenames() {
    for name in ["0", "a", "ocean_1-night", &"a".repeat(64)] {
        validate_name(name).unwrap();
    }
    for name in [
        "",
        "A",
        "a.toml",
        "..",
        "../a",
        "/a",
        "a/b",
        "a\\b",
        "_a",
        "-a",
        "a\0",
        "a\n",
        "海",
        &"a".repeat(65),
    ] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn loader_rejects_oversized_invalid_non_utf8_and_special_documents() {
    let (_directory, store, _) = fixture();
    store.create_root().unwrap();
    let path = store.path("ocean").unwrap();
    assert!(
        store
            .load("ocean")
            .unwrap_err()
            .to_string()
            .contains("theme reset")
    );
    for bytes in [
        vec![b' '; 65537],
        vec![0xff],
        b"schema_version = 1\n".to_vec(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(store.load("ocean").is_err());
    }
    let mut invalid = palette().clone();
    invalid.palette.text_primary = invalid.palette.canvas;
    std::fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
    assert!(format!("{:#}", store.load("ocean").unwrap_err()).contains("text.primary on canvas"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(store.load("ocean").is_err());
    #[cfg(unix)]
    {
        std::fs::remove_dir(&path).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(store.load("ocean").is_err());
        drop(listener);
    }
}

#[cfg(unix)]
#[test]
fn symlink_roots_files_locks_and_configs_are_never_followed() {
    use std::os::unix::fs::symlink;
    let (directory, store, config) = fixture();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, &store.root).unwrap();
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    assert!(!config.exists());
    std::fs::remove_file(&store.root).unwrap();
    store.create_root().unwrap();
    let outside_file = outside.join("keep.toml");
    let original = toml::to_string(palette()).unwrap();
    std::fs::write(&outside_file, &original).unwrap();
    let path = store.path("ocean").unwrap();
    symlink(&outside_file, &path).unwrap();
    assert!(store.load("ocean").is_err());
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    std::fs::remove_file(&path).unwrap();
    symlink(&outside_file, &config).unwrap();
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    assert!(reset(&config).is_err());
    assert!(!path.exists());
    std::fs::remove_file(&config).unwrap();
    std::fs::remove_file(store.root.join(".themes.lock")).unwrap();
    symlink(&outside_file, store.root.join(".themes.lock")).unwrap();
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), original);
}

#[test]
fn configuration_cannot_alias_a_theme_or_store_lock() {
    let (_directory, store, _) = fixture();
    store.create_root().unwrap();
    let saved = store.path("shared").unwrap();
    let original = toml::to_string(palette()).unwrap();
    std::fs::write(&saved, &original).unwrap();
    for config in [
        store.path("ocean").unwrap(),
        saved.clone(),
        store.root.join(".themes.lock"),
        store.root.join("nested/config.toml"),
    ] {
        assert!(
            store
                .save_and_select("ocean", palette(), &config)
                .unwrap_err()
                .to_string()
                .contains("reserved theme directory")
        );
    }
    assert_eq!(std::fs::read_to_string(saved).unwrap(), original);
    assert!(!store.path("ocean").unwrap().exists());
}

#[test]
fn held_locks_prevent_publication_without_changing_existing_state() {
    let (_directory, store, config) = fixture();
    std::fs::write(&config, "# original\n").unwrap();
    store.create_root().unwrap();
    let lock = store.lock().unwrap();
    let error = store
        .save_and_select("ocean", palette(), &config)
        .unwrap_err();
    assert!(format!("{error:#}").contains("busy"));
    assert!(!store.path("ocean").unwrap().exists());
    drop(lock);
    settings::transaction(&config, |before| {
        let error = store
            .save_and_select("ocean", palette(), &config)
            .unwrap_err();
        assert!(format!("{error:#}").contains("busy"));
        assert!(!store.path("ocean").unwrap().exists());
        Ok((before.into(), ()))
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# original\n");
}

#[test]
fn failure_after_publication_keeps_config_and_theme_and_retry_is_safe() {
    let (_directory, store, config) = fixture();
    let before = "# credentials\n[ensemble.agents.incomplete.env]\napi_key = 'keep'\n[theme]\nname = 'previous'\n";
    std::fs::write(&config, before).unwrap();
    let error = store
        .save_and_select_with(
            "ocean",
            palette(),
            &config,
            || Ok(()),
            || anyhow::bail!("injected selector failure"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("saved but not selected"));
    assert!(error.to_string().contains("Retry"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    let path = store.path("ocean").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    store.load("ocean").unwrap();
    store.save_and_select("ocean", palette(), &config).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    let parsed: toml::Table = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(parsed["theme"]["name"].as_str(), Some("ocean"));
}

#[test]
fn noncooperating_config_edits_and_first_run_creation_are_not_clobbered() {
    for existing in [true, false] {
        let (_directory, store, config) = fixture();
        if existing {
            std::fs::write(&config, "# before\n").unwrap();
        }
        let error = store
            .save_and_select_with(
                "ocean",
                palette(),
                &config,
                || Ok(()),
                || {
                    std::fs::write(&config, "# external editor wins\n")?;
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("saved but not selected"));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "# external editor wins\n"
        );
        store.load("ocean").unwrap();
        store.save_and_select("ocean", palette(), &config).unwrap();
    }
}

#[test]
fn theme_creation_race_refuses_conflict_and_reuses_identical_comments() {
    for identical in [true, false] {
        let (_directory, store, config) = fixture();
        let path = store.path("ocean").unwrap();
        let bytes = if identical {
            format!(
                "# concurrent author's comment\n{}",
                toml::to_string(palette()).unwrap()
            )
        } else {
            "# invalid conflicting theme\n".into()
        };
        let result = store.save_and_select_with(
            "ocean",
            palette(),
            &config,
            || {
                std::fs::write(&path, &bytes)?;
                Ok(())
            },
            || Ok(()),
        );
        assert_eq!(result.is_ok(), identical);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        assert_eq!(config.exists(), identical);
    }
}

#[cfg(unix)]
#[test]
fn readonly_preparation_and_selector_failure_do_not_damage_contents() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_directory, store, config) = fixture();
    std::fs::write(&config, "# before\n").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    assert!(!store.path("ocean").unwrap().exists());
    // An already-default read-only config is still a successful reset no-op.
    reset(&config).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
    let error = store
        .save_and_select_with(
            "ocean",
            palette(),
            &config,
            || Ok(()),
            || {
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444))?;
                Ok(())
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("saved but not selected"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# before\n");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
    store.save_and_select("ocean", palette(), &config).unwrap();
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o640
    );
    std::fs::set_permissions(&store.root, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(
        store
            .save_and_select("another", palette(), &config)
            .is_err()
    );
    std::fs::set_permissions(&store.root, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn invalid_generation_or_toml_leaves_public_files_unchanged() {
    let (_directory, store, config) = fixture();
    let mut invalid = palette().clone();
    invalid.palette.canvas = zevria_theme::HexRgb(0, 0, 0);
    assert!(store.save_and_select("ocean", &invalid, &config).is_err());
    assert!(!store.root.exists());
    assert!(!config.exists());
    std::fs::write(&config, "[malformed").unwrap();
    assert!(store.save_and_select("ocean", palette(), &config).is_err());
    assert!(reset(&config).is_err());
    assert!(!store.path("ocean").unwrap().exists());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "[malformed");
}
