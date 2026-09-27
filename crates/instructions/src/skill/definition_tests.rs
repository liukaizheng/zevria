use super::*;

fn definition(name: &str, body: &str) -> SkillDefinition {
    SkillDefinition::new(
        name.parse().unwrap(),
        "Description",
        body,
        SkillSource::Programmatic("test".into()),
    )
    .unwrap()
}
#[test]
fn names_and_locators_revalidate_when_deserialized() {
    for name in ["commit", "fix_ci-2", "a"] {
        assert_eq!(SkillName::parse(name).unwrap().as_str(), name);
    }
    for name in ["", "UPPER", "has space", "slash/name"] {
        assert!(SkillName::parse(name).is_err());
    }
    assert!(SkillName::parse("a".repeat(65)).is_err());
    assert!(serde_json::from_str::<SkillName>("\"Spoof\"").is_err());
    for path in [
        "",
        "/outside/SKILL.md",
        "../a/SKILL.md",
        "a/./SKILL.md",
        "a//SKILL.md",
        "C:/a/SKILL.md",
        "a\\SKILL.md",
        "a/\0",
    ] {
        assert!(SkillRelativePath::new(path).is_err(), "{path:?}");
    }
    assert!(serde_json::from_str::<FixedSkillScope>("\"imported\"").is_err());
}
#[test]
fn complete_snapshot_roundtrips_and_rejects_all_tampering() {
    let snapshot = definition("commit", " Body ").snapshot();
    let value = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(snapshot.body(), "Body");
    assert!(value.get("description").is_none());
    assert!(value.get("extension").is_none());
    assert!(value.get("source").is_none());
    assert_eq!(value["metadata"]["description"], "Description");
    assert_eq!(
        serde_json::from_value::<SkillSnapshot>(value.clone()).unwrap(),
        snapshot
    );
    assert_eq!(
        snapshot
            .digest()
            .to_string()
            .parse::<SkillDigest>()
            .unwrap(),
        snapshot.digest()
    );
    for case in 0..8 {
        let mut corrupt = value.clone();
        match case {
            0 => corrupt["name"] = serde_json::json!("other"),
            1 => corrupt["body"] = serde_json::json!("different"),
            2 => corrupt["body"] = serde_json::json!(" Body "),
            3 => corrupt["metadata"]["description"] = serde_json::json!("Changed"),
            4 => corrupt["metadata"]["interface"]["display_name"] = serde_json::json!("Changed"),
            5 => corrupt["metadata"]["invocation_policy"] = serde_json::json!("explicit_only"),
            6 => corrupt["digest"] = serde_json::json!("0".repeat(64)),
            _ => corrupt["unknown"] = serde_json::json!(true),
        }
        assert!(
            serde_json::from_value::<SkillSnapshot>(corrupt).is_err(),
            "case {case}"
        );
    }
    let mut metadata = SkillMetadata::new("Authoritative description");
    metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
    let changed = definition("commit", "Body")
        .with_metadata(metadata, None)
        .unwrap();
    assert_eq!(changed.description(), "Authoritative description");
    assert_ne!(changed.digest(), snapshot.digest());
}
#[test]
fn snapshot_and_catalog_v1_hashes_are_deterministic_and_old_digests_are_rejected() {
    let snapshot = definition("commit", "Body").snapshot();
    // Independently calculated from the length-delimited canonical fields.
    assert_eq!(
        snapshot.digest().to_string(),
        "09b4ad772755b416ee13c31e446a55bbfee88671d82a9d6a59b50f851e5a52b7"
    );
    assert_eq!(definition("commit", " Body ").digest(), snapshot.digest());
    let mut old = serde_json::to_value(&snapshot).unwrap();
    old["digest"] =
        serde_json::json!("c4451c498023f41c7266b24a7e749f0bcf9534bef2be43e204e794910de562e3");
    assert!(
        serde_json::from_value::<SkillSnapshot>(old)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
    );
    assert_eq!(
        SkillCatalog::default().revision(),
        "5e9ca029e45fe0f640b77b1ebd422b1879f06727ea38ae84d62d11f43072e3d6"
    );
    let definitions = [definition("review", "Review"), definition("commit", "Body")];
    let first = SkillCatalog::new(definitions.clone()).unwrap();
    let second = SkillCatalog::new(definitions.into_iter().rev()).unwrap();
    assert_eq!(first.revision(), second.revision());
    assert_ne!(first.revision(), SkillCatalog::default().revision());
}

#[test]
fn provenance_is_bound_to_canonical_scope_root_and_manifest() {
    let manifest = SkillRelativePath::new("team/review/SKILL.md").unwrap();
    let root = tempfile::tempdir().unwrap();
    let binding =
        SourceBinding::for_source(FixedSkillScope::Project, root.path(), &manifest).unwrap();
    let provenance = SkillProvenance {
        scope: FixedSkillScope::Project,
        layout: SkillLayout::Package,
        manifest,
        source_binding: binding,
    };
    provenance.validate().unwrap();
    let mut value = serde_json::to_value(&provenance).unwrap();
    value["root"] = serde_json::json!("/arbitrary");
    assert!(serde_json::from_value::<SkillProvenance>(value).is_err());
}
#[test]
fn catalog_rejects_duplicates_and_invalid_config_and_orders_names() {
    let catalog = SkillCatalog::new([
        definition("review", "Review"),
        definition("commit", "Commit"),
    ])
    .unwrap();
    assert_eq!(
        catalog.names().map(SkillName::as_str).collect::<Vec<_>>(),
        ["commit", "review"]
    );
    assert!(SkillCatalog::new([definition("commit", "one"), definition("commit", "two")]).is_err());
    let rule = SkillEnableRule {
        name: "commit".parse().unwrap(),
        enabled: false,
    };
    assert!(
        catalog
            .with_config(SkillsConfig {
                enabled: true,
                rules: vec![rule; 4097]
            })
            .is_err()
    );
    for source in [
        "roots = []",
        "extra_roots = []",
        "directory = '/tmp'",
        "catalog_max_tokens = 0",
        "rules = [{name = 'UPPER', enabled = false}]",
    ] {
        assert!(toml::from_str::<SkillsConfig>(source).is_err());
    }
    let config: SkillsConfig = toml::from_str(
        "rules = [{name = 'review', enabled = false}, {name = 'review', enabled = true}]",
    )
    .unwrap();
    assert!(config.name_enabled(&"review".parse().unwrap()));
}
#[cfg(unix)]
#[test]
fn catalog_revision_tracks_canonical_root_changes_even_without_candidates() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let roots = FixedSkillRoots::fixture(None, directory.path());
    std::fs::create_dir_all(roots.project().parent().unwrap()).unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    symlink(&first, roots.project()).unwrap();
    let before =
        SkillCatalog::from_discovery(discover_skills(&roots), SkillsConfig::default()).unwrap();
    std::fs::remove_file(roots.project()).unwrap();
    symlink(&second, roots.project()).unwrap();
    let after =
        SkillCatalog::from_discovery(discover_skills(&roots), SkillsConfig::default()).unwrap();
    assert!(before.entries.is_empty() && after.entries.is_empty());
    assert_eq!(before.diagnostics, after.diagnostics);
    assert_ne!(before.revision(), after.revision());
}

#[test]
fn active_ledger_orders_names_and_never_replaces_pins() {
    let review = definition("review", "Review body").snapshot();
    let commit = definition("commit", "Commit body").snapshot();
    let active = ActiveSkills::from_snapshots([review.clone(), commit]).unwrap();
    let rendered = active.render_context().unwrap();
    assert!(rendered.find("Commit body").unwrap() < rendered.find("Review body").unwrap());
    assert_eq!(rendered.matches("Review body").count(), 1);
    assert!(
        active
            .prepare(definition("review", "New body").snapshot())
            .is_err()
    );
    assert_eq!(active.prepare(review).unwrap().1, active);
    assert!(
        ActiveSkills::default()
            .with_application(&SkillApplication::Reapply("missing".parse().unwrap()))
            .is_err()
    );
}
