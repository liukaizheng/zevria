use super::*;
use std::sync::Arc;
fn definition(name: &str, description: &str) -> SkillDefinition {
    SkillDefinition::new(
        name.parse().unwrap(),
        description,
        "PRIVATE INSTRUCTIONS",
        SkillSource::Programmatic("test".into()),
    )
    .unwrap()
}
fn context(catalog: SkillCatalog) -> SkillContext {
    SkillContext {
        catalog: Arc::new(catalog),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    }
}
#[test]
fn management_returns_complete_filtered_views_with_bounded_fields() {
    let ctx = context(
        SkillCatalog::new(
            (0..65).map(|i| definition(&format!("skill-{i:02}"), &"\u{1b}\"\\".repeat(1000))),
        )
        .unwrap(),
    );
    let view = ctx
        .management_view(&SkillManagementRequest::List {
            query: String::new(),
        })
        .unwrap();
    assert_eq!(view.entries.len(), 65);
    assert_eq!(view.completions.len(), 65);
    assert!(
        view.entries
            .iter()
            .all(|e| e.metadata_shortened && e.metadata.description.len() <= 1024)
    );
    assert!(serde_json::to_vec(&view).unwrap().len() > 32768);
    let encoded = serde_json::to_string(&view).unwrap();
    for private in [
        "PRIVATE INSTRUCTIONS",
        "next_cursor",
        "source_id",
        "binding",
    ] {
        assert!(!encoded.contains(private));
    }
    let filtered = ctx
        .management_view(&SkillManagementRequest::List {
            query: "skill-6".into(),
        })
        .unwrap();
    assert_eq!(filtered.entries.len(), 5);
    assert_eq!(
        filtered.completions.len(),
        65,
        "filtering candidate inspection must not erase completions"
    );
}
#[test]
fn completions_resolve_explicit_only_and_pins_not_enabled_candidate_rows() {
    let mut metadata = SkillMetadata::new("Pinned description");
    metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
    let original = definition("review", "Review")
        .with_metadata(metadata, None)
        .unwrap();
    let mut ctx = context(SkillCatalog::new([original.clone()]).unwrap());
    assert!(
        ctx.resolve(original.name(), SkillInvocationOrigin::Model)
            .is_err()
    );
    assert_eq!(ctx.completions()[0].name, *original.name());
    let changed = original
        .clone()
        .with_metadata(SkillMetadata::new("Changed"), None)
        .unwrap();
    let before = ctx.catalog.revision().to_owned();
    ctx.catalog = Arc::new(SkillCatalog::new([changed]).unwrap());
    assert_ne!(before, ctx.catalog.revision());
    ctx.pins = ActiveSkills::from_snapshots([original.snapshot()]).unwrap();
    assert_eq!(ctx.completions()[0].description, "Pinned description");
    assert_eq!(
        ctx.resolve(original.name(), SkillInvocationOrigin::Model)
            .unwrap(),
        original.snapshot()
    );
}
#[test]
fn management_inspects_all_same_name_candidates_and_lists_nameless_failures() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let roots = FixedSkillRoots::fixture(Some(home.path()), workspace.path());
    for root in [roots.global().unwrap(), roots.project()] {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("review.md"),
            "---\ndescription: Review\n---\nInstalled body",
        )
        .unwrap();
    }
    std::fs::write(roots.project().join("broken.md"), "not a skill document").unwrap();
    std::fs::create_dir_all(roots.project().join("dupe")).unwrap();
    for path in [
        roots.project().join("dupe.md"),
        roots.project().join("dupe/SKILL.md"),
    ] {
        std::fs::write(path, "---\ndescription: Duplicate\n---\nBody").unwrap();
    }
    let mut ctx = context(
        SkillCatalog::from_discovery(discover_skills(&roots), SkillsConfig::default()).unwrap(),
    );
    ctx.pins =
        ActiveSkills::from_snapshots([definition("review", "Old description").snapshot()]).unwrap();
    let view = ctx
        .management_view(&SkillManagementRequest::List {
            query: String::new(),
        })
        .unwrap();
    assert!(view.entries.iter().any(|e| e.status == "shadowed"));
    assert!(view.entries.iter().any(|e| e.status == "selected"));
    assert!(
        view.entries
            .iter()
            .any(|e| e.status == "historical" && e.active && e.pinned)
    );
    assert_eq!(view.invalid_entries.len(), 1);
    assert_eq!(
        view.completions.len(),
        1,
        "ambiguous enabled candidates are not invocable"
    );
    assert_eq!(view.completions[0].description, "Old description");
    let inspect = ctx
        .management_view(&SkillManagementRequest::Inspect {
            name: "review".parse().unwrap(),
        })
        .unwrap();
    assert_eq!(inspect.entries.len(), 3);
    assert!(inspect.invalid_entries.is_empty());
    let invalid = ctx
        .management_view(&SkillManagementRequest::List {
            query: "broken.md".into(),
        })
        .unwrap();
    assert_eq!(invalid.invalid_entries, view.invalid_entries);
    assert!(invalid.entries.is_empty());
    assert!(
        serde_json::from_value::<SkillManagementRequest>(
            serde_json::json!({"operation":"list","query":"", "cursor":null})
        )
        .is_err()
    );
}
#[test]
fn validation_never_adopts_outside_roots_and_matches_native_layout() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let roots = FixedSkillRoots::fixture(Some(home.path()), workspace.path());
    let package = roots.project().join("review");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        "---\nname: review\ndescription: Review\n---\nReview body",
    )
    .unwrap();
    validate_skill_path(&roots, &package).unwrap();
    validate_skill_path(&roots, &package.join("SKILL.md")).unwrap();
    std::fs::write(outside.path().join("outside.md"), "PRIVATE OUTSIDE").unwrap();
    assert!(
        validate_skill_path(&roots, &outside.path().join("outside.md"))
            .unwrap_err()
            .to_string()
            .contains("outside")
    );
    assert!(validate_skill_path(&roots, &package.join("../review/SKILL.md")).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside.path().join("outside.md"),
            roots.project().join("escape.md"),
        )
        .unwrap();
        assert!(validate_skill_path(&roots, &roots.project().join("escape.md")).is_err());
    }
}
