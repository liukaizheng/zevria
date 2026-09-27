use super::*;
use std::sync::Arc;

fn definition(name: &str, description: &str) -> SkillDefinition {
    SkillDefinition::new(
        name.parse().unwrap(),
        description,
        "PRIVATE BODY",
        SkillSource::Programmatic("PRIVATE SOURCE".into()),
    )
    .unwrap()
}
fn context(definitions: impl IntoIterator<Item = SkillDefinition>) -> SkillContext {
    SkillContext {
        catalog: Arc::new(SkillCatalog::new(definitions).unwrap()),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    }
}
#[test]
fn complete_catalog_escapes_and_shortens_utf8_without_omitting_names() {
    let description = format!("\"\\\0{}", "颜色".repeat(300));
    let ctx = context(
        (0..65)
            .rev()
            .map(|i| definition(&format!("skill-{i:02}"), &description)),
    );
    let catalog = ctx.prompt_catalog();
    assert_eq!(catalog, ctx.prompt_catalog());
    assert_eq!(catalog.entries.len(), 65);
    assert!(catalog.entries.windows(2).all(|p| p[0].name < p[1].name));
    assert!(
        catalog
            .entries
            .iter()
            .all(|e| e.description.len() <= 1024 && e.description.ends_with("..."))
    );
    let text = catalog.render();
    assert!(text.len() > 8192);
    assert!(text.contains("\\u0000"));
    assert!(!text.contains('\0'));
    for private in [
        "PRIVATE BODY",
        "PRIVATE SOURCE",
        "source_binding",
        "dependencies",
        "manifest",
        "default_prompt",
        "skill_list",
        "omitted_entries",
    ] {
        assert!(!text.contains(private));
    }
}
#[test]
fn explicit_only_completions_and_pinned_metadata_share_resolution() {
    let mut metadata = SkillMetadata::new("Pinned explicit description");
    metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
    let explicit = definition("explicit", "Original")
        .with_metadata(metadata, None)
        .unwrap();
    let removed = definition("removed", "Pinned removed description");
    let mut ctx = context([explicit.clone(), definition("ordinary", "Ordinary")]);
    assert_eq!(ctx.prompt_catalog().entries.len(), 1);
    assert_eq!(ctx.completions().len(), 2);
    assert!(
        ctx.resolve(explicit.name(), SkillInvocationOrigin::Model)
            .is_err()
    );
    assert!(
        ctx.resolve(explicit.name(), SkillInvocationOrigin::Explicit)
            .is_ok()
    );
    let before = ctx.prompt_catalog();
    ctx.pins = ActiveSkills::from_snapshots([explicit.snapshot(), removed.snapshot()]).unwrap();
    assert_eq!(before, ctx.prompt_catalog());
    ctx.catalog = Arc::new(
        SkillCatalog::new([
            definition("explicit", "Replacement metadata"),
            definition("ordinary", "Ordinary"),
        ])
        .unwrap(),
    );
    assert_eq!(ctx.completions().len(), 3);
    assert_eq!(
        ctx.completions()[0].description,
        "Pinned explicit description"
    );
    assert_eq!(
        ctx.prompt_catalog().entries[0].description,
        "Replacement metadata"
    );
    assert_eq!(
        ctx.resolve(explicit.name(), SkillInvocationOrigin::Model)
            .unwrap(),
        explicit.snapshot()
    );
    ctx.catalog = Arc::new(
        ctx.catalog
            .as_ref()
            .clone()
            .with_config(SkillsConfig {
                enabled: true,
                rules: vec![SkillEnableRule {
                    name: explicit.name().clone(),
                    enabled: false,
                }],
            })
            .unwrap(),
    );
    assert_eq!(ctx.prompt_catalog().entries.len(), 1);
    assert_eq!(ctx.completions().len(), 2);
    assert!(
        ctx.resolve(explicit.name(), SkillInvocationOrigin::Explicit)
            .is_err()
    );
    assert_eq!(ctx.pins.len(), 2);
    ctx.mode_enabled = false;
    assert!(ctx.prompt_catalog().entries.is_empty());
    assert!(ctx.completions().is_empty());
    assert!(ctx.render_active_context().is_none());
    ctx.mode_enabled = true;
    ctx.catalog = Arc::new(
        ctx.catalog
            .as_ref()
            .clone()
            .with_config(SkillsConfig::default())
            .unwrap(),
    );
    assert_eq!(
        ctx.resolve(explicit.name(), SkillInvocationOrigin::Model)
            .unwrap(),
        explicit.snapshot()
    );
}
#[test]
fn catalog_is_independent_of_activation_and_management_only_revision() {
    let mut ctx = context((0..25).map(|i| definition(&format!("skill-{i:02}"), "Description")));
    let before = ctx.prompt_catalog();
    ctx.pins
        .apply(&SkillApplication::Activate(
            ctx.catalog.get("skill-24").unwrap().snapshot(),
        ))
        .unwrap();
    assert_eq!(before, ctx.prompt_catalog());
    let active = ctx.prompt_catalog();
    ctx.catalog = Arc::new(
        ctx.catalog
            .as_ref()
            .clone()
            .with_config(SkillsConfig {
                enabled: true,
                rules: vec![SkillEnableRule {
                    name: "uninstalled".parse().unwrap(),
                    enabled: false,
                }],
            })
            .unwrap(),
    );
    assert_eq!(
        active,
        ctx.prompt_catalog(),
        "management-only changes emit no model replacement"
    );
}
