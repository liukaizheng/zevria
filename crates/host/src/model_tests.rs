use super::*;
use zevria_foundation::{ModelProfileRef, ReasoningLevel as Level};

fn source() -> &'static str {
    r#"// catalog must remain byte-for-byte unchanged
{
  "providers": { "p": {
    "base_url": "http://127.0.0.1:1/v1/responses", "api_key": "untouched-secret",
    "supports_websockets": false,
    "models": {
      "old": { "context_window_tokens": 100000, "retained_user_tokens": 1000,
        "reasoning_levels": ["low", "medium", "high"], /* capabilities */ "reasoning_summary_level": "detailed" },
      "new/with:separators": { "context_window_tokens": 50000, "input_token_limit": 40000,
        "retained_user_tokens": 1000, "reasoning_levels": ["low", "high"],
        "reasoning_summary_level": "detailed" }
    }
  } },
}
"#
}
fn ordinary(table: Option<ModelRole>) -> String {
    let mut text = "# preserve ordinary settings\n[modes]\n".to_string();
    for role in ModelRole::ALL {
        if Some(role) != table {
            text.push_str(&format!("{} = {{ provider = 'p', model = \"old\", reasoning_level = \"medium\" }} # {} comment\n", role.name(), role.name()));
        }
    }
    if let Some(role) = table {
        text.push_str(&format!("\n[modes.{}] # table comment\nprovider = 'p' # preserve literal decoration\nmodel = \"old\" # model comment\nreasoning_level = \"medium\" # level comment\n", role.name()));
    }
    text
}
fn fixture(path: &Path) -> LocalModelSettings {
    std::fs::write(path, ordinary(None)).unwrap();
    let models_path = models_path_for(path);
    std::fs::write(&models_path, source()).unwrap();
    LocalModelSettings {
        config_path: path.into(),
        models_path,
    }
}
fn selection(model: &str, level: Level) -> ModelSelection {
    ModelSelection::new(ModelProfileRef::new("p", model), level)
}

#[test]
fn saves_only_selected_toml_assignment_preserving_both_styles_permissions_and_noop_mtime() {
    for role in [ModelRole::Build, ModelRole::Plan] {
        for table in [None, Some(role)] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("custom.toml");
            let service = fixture(&path);
            let before = ordinary(table);
            std::fs::write(&path, &before).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
                // Catalog readability, not writability, is the only requirement.
                std::fs::set_permissions(
                    &service.models_path,
                    std::fs::Permissions::from_mode(0o444),
                )
                .unwrap();
            }
            let permissions = std::fs::metadata(&path).unwrap().permissions();
            let expected = revision(&load(&path).unwrap()).unwrap();
            let target = selection("new/with:separators", Level::High);
            let next = service.save(&expected, role, &target).unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            let old = if table.is_some() {
                format!(
                    "[modes.{}] # table comment\nprovider = 'p' # preserve literal decoration\nmodel = \"old\" # model comment\nreasoning_level = \"medium\" # level comment",
                    role.name()
                )
            } else {
                format!(
                    "{} = {{ provider = 'p', model = \"old\", reasoning_level = \"medium\" }}",
                    role.name()
                )
            };
            let new = old
                .replace("\"old\"", "\"new/with:separators\"")
                .replace("\"medium\"", "\"high\"");
            assert_eq!(after, before.replace(&old, &new));
            assert_eq!(
                std::fs::read_to_string(&service.models_path).unwrap(),
                source()
            );
            assert_eq!(permissions, std::fs::metadata(&path).unwrap().permissions());
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(service.save(&next, role, &target).unwrap(), next);
            assert_eq!(
                modified,
                std::fs::metadata(&path).unwrap().modified().unwrap()
            );
            assert!(service.save(&expected, role, &target).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        }
    }
}

#[test]
fn reasoning_is_mode_owned_even_when_every_role_shares_the_same_profile() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let service = fixture(&path);
    let expected = revision(&load(&path).unwrap()).unwrap();
    let next = service
        .save(&expected, ModelRole::Plan, &selection("old", Level::High))
        .unwrap();
    assert_ne!(expected, next);
    let config = load(&path).unwrap();
    for role in ModelRole::ALL {
        assert_eq!(
            config.modes.for_role(role).reasoning_level,
            if role == ModelRole::Plan {
                Level::High
            } else {
                Level::Medium
            }
        );
    }
    assert!(
        service
            .save(&next, ModelRole::Plan, &selection("old", Level::Max))
            .is_err()
    );
    assert!(
        service
            .save(&next, ModelRole::Review, &selection("old", Level::High))
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&service.models_path).unwrap(),
        source()
    );
}

#[test]
fn rejects_missing_readonly_and_symlink_targets_without_requiring_writable_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("custom.toml");
    let service = LocalModelSettings {
        config_path: path.clone(),
        models_path: models_path_for(&path),
    };
    let target = selection("new/with:separators", Level::Low);
    assert!(service.save("", ModelRole::Build, &target).is_err());
    assert!(!path.exists());
    fixture(&path);
    let expected = revision(&load(&path).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert!(service.save(&expected, ModelRole::Build, &target).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("linked.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            LocalModelSettings {
                config_path: link,
                models_path: service.models_path.clone()
            }
            .save(&expected, ModelRole::Build, &target)
            .is_err()
        );
        let link = directory.path().join("linked.jsonc");
        std::os::unix::fs::symlink(&service.models_path, &link).unwrap();
        assert!(
            LocalModelSettings {
                config_path: path.clone(),
                models_path: link
            }
            .save(&expected, ModelRole::Build, &target)
            .is_err()
        );
    }
}

#[test]
fn rechecks_both_source_snapshots_and_never_overwrites_noncooperating_edits() {
    for edit_catalog in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.toml");
        let service = fixture(&path);
        let expected = revision(&load(&path).unwrap()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = service
            .transaction(&expected, |_, ordinary, catalog| {
                if edit_catalog {
                    std::fs::write(
                        &service.models_path,
                        format!("{catalog}\n// external catalog edit\n"),
                    )
                    .unwrap();
                } else {
                    std::fs::write(&path, format!("{ordinary}\n[theme]\nname = 'external'\n"))
                        .unwrap();
                }
                Ok((
                    format!("{ordinary}\n# prepared but must not publish\n"),
                    expected.clone(),
                ))
            })
            .unwrap_err();
        assert!(error.to_string().contains("changed while preparing"));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("must not publish"));
        if edit_catalog {
            assert_eq!(after, before);
        } else {
            assert!(after.contains("name = 'external'"));
        }
    }
}

#[test]
fn skill_theme_and_model_writers_share_one_lock_and_preserve_unrelated_edits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("custom.toml");
    let service = fixture(&path);
    let expected = revision(&load(&path).unwrap()).unwrap();
    let target = selection("new/with:separators", Level::Low);
    let unrelated = "\n[skills]\nrules = [{ name = \"review\", enabled = false }]\n[theme]\nname = 'not-installed' # preserve selector\n";
    settings::transaction(&path, |before| {
        assert!(
            service
                .save(&expected, ModelRole::Build, &target)
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        Ok((format!("{before}{unrelated}"), ()))
    })
    .unwrap();
    assert_eq!(revision(&load(&path).unwrap()).unwrap(), expected);
    service.save(&expected, ModelRole::Build, &target).unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().ends_with(unrelated));
    assert_eq!(
        std::fs::read_to_string(&service.models_path).unwrap(),
        source()
    );
    assert!(directory.path().join("custom.toml.skills.lock").is_file());
    assert!(!directory.path().join("models.jsonc.skills.lock").exists());
}

#[test]
fn revisions_cover_catalog_assignments_and_compaction_but_not_file_formatting() {
    for change in ["catalog", "assignment", "compaction"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let service = fixture(&path);
        let expected = revision(&load(&path).unwrap()).unwrap();
        match change {
            "catalog" => std::fs::write(
                &service.models_path,
                source().replace("untouched-secret", "new-secret"),
            )
            .unwrap(),
            "assignment" => {
                std::fs::write(&path, ordinary(None).replacen("\"medium\"", "\"high\"", 1)).unwrap()
            }
            _ => std::fs::write(
                &path,
                format!(
                    "{}\n[session.compaction]\nauto_trigger_percent = 80\n",
                    ordinary(None)
                ),
            )
            .unwrap(),
        }
        let before = std::fs::read(&path).unwrap();
        assert!(service.validate(&expected).is_err());
        assert!(
            service
                .save(&expected, ModelRole::Build, &selection("old", Level::High))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
