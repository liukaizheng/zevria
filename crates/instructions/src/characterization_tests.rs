//! Fixed model-facing bytes and identities, intentionally reviewed when changed.
use crate::*;
use zevria_foundation::{ModelRole, TurnPolicy, WorkspaceBinding, WorkspaceContract};

fn fixture_set() -> InstructionSet {
    InstructionSet {
        application: "Application\nverbatim".into(),
        system: vec![
            ("guidance:project".into(), "Scope: project\nSource: \"/startup/AGENTS.md\"\n--- BEGIN USER-CONTROLLED AGENTS.md BODY ---\nProject guidance\n--- END USER-CONTROLLED AGENTS.md BODY ---".into()),
            ("guidance:global".into(), "Scope: global\nSource: \"/home/.zevria/AGENTS.md\"\n--- BEGIN USER-CONTROLLED AGENTS.md BODY ---\nGlobal guidance\n--- END USER-CONTROLLED AGENTS.md BODY ---".into()),
        ],
        workflow: DirectivePolicy::new("plan", &TurnPolicy::new(
            "Investigate and plan", Some(vec!["command".into(), "skill".into(), "web_search".into(), "launch_subtasks".into()]), ModelRole::Plan, true,
        ).with_contract(WorkspaceContract::SourceReadOnlyScratch)),
        catalog: Some(SkillPromptCatalog { enabled: true, entries: vec![SkillPromptEntry { name: "review".parse().unwrap(), description: "Review changes".into() }] }),
    }
}

fn fixture(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    assert_eq!(actual, std::fs::read_to_string(path).unwrap());
}

#[test]
fn instruction_set_directives_match_v1_bytes() {
    let skill = SkillSnapshot::new(
        "review".parse().unwrap(),
        "Review",
        "Exact body\nsecond line",
    )
    .unwrap();
    let directives = vec![
        DirectiveContent::skill(&skill),
        DirectiveContent::new(DirectivePayload::SkillRevocation {
            name: skill.name().clone(),
            reason: "disabled".into(),
        })
        .unwrap(),
    ];
    let actual = serde_json::to_value(directives).unwrap();
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/directives.json");
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn instruction_set_bytes_and_identity_are_pinned() {
    let set = fixture_set();
    set.validate().unwrap();
    fixture("instruction_set.txt", &set.render());
    fixture("instruction_set.sha256", &set.identity());
    let mut reordered = set.clone();
    reordered.system.reverse();
    assert_eq!(set.render(), reordered.render());
}

#[test]
fn optional_sections_and_disabled_catalog() {
    let mut set = fixture_set();
    set.application.clear();
    set.system.clear();
    set.catalog = None;
    let text = set.render();
    assert!(!text.contains("## Application guidance"));
    assert!(!text.contains("## File guidance"));
    assert!(text.ends_with("## Eligible skills\nSkill selection is unavailable."));
    set.catalog = Some(SkillPromptCatalog {
        enabled: false,
        entries: vec![],
    });
    assert!(
        set.render()
            .contains("## Eligible skills\nSkill selection is unavailable.")
    );
    set.catalog.as_mut().unwrap().enabled = true;
    assert!(set.render().ends_with("\n[]"));
}

#[test]
fn maintenance_is_tool_and_skill_free() {
    let set = InstructionSet::maintenance("Application", [("guidance:project", "Project")]);
    set.validate().unwrap();
    let text = set.render();
    assert!(text.contains("## Application guidance\nApplication"));
    assert!(text.contains("## File guidance\nProject"));
    assert!(text.contains(
        "## Workflow policy: maintenance\n{\"scope\":\"maintenance\",\"tools\":[],\"skills\":false}"
    ));
    assert!(text.ends_with("## Eligible skills\nSkill selection is unavailable."));
}

#[test]
fn invalid_policy_and_catalog_are_rejected() {
    let mut set = fixture_set();
    set.workflow.scope.clear();
    assert!(set.validate().is_err());
    set.workflow.scope = "build".into();
    for names in [vec!["".into()], vec!["skill".into(), "skill".into()]] {
        set.workflow.allowed_tool_names = Some(names);
        assert!(set.validate().is_err());
    }
    set.workflow.allowed_tool_names = Some(vec![]);
    set.catalog.as_mut().unwrap().enabled = false;
    assert!(set.validate().is_err());
}

#[test]
fn structured_policy_declarations_include_only_applicable_fields() {
    let mut set = fixture_set();
    let builder = TurnPolicy::new(
        "Build child",
        Some(vec!["command".into(), "write".into()]),
        ModelRole::Builder,
        false,
    )
    .with_workspace(WorkspaceBinding {
        root: "/abs/child".into(),
        startup: "/abs/startup".into(),
    });
    set.workflow = DirectivePolicy::new("builder", &builder);
    set.catalog = None;
    set.validate().unwrap();
    assert_eq!(
        set.workflow.declaration(),
        r#"{"scope":"builder","tools":["command","write"],"skills":false,"workspace":{"root":"/abs/child","startup":"/abs/startup"}}"#
    );
    let policy = TurnPolicy::new("Build", None, ModelRole::Build, true).with_orchestration();
    set.workflow = DirectivePolicy::new("build", &policy);
    set.validate().unwrap();
    assert_eq!(
        set.workflow.declaration(),
        r#"{"scope":"build","tools":"registered","skills":true,"subtasks":["explore"],"orchestration":"explicit_request_only: explore, build; concurrent_batch_required"}"#
    );
}

#[test]
fn invalid_capabilities_and_embedded_modules_are_rejected() {
    let base = fixture_set();
    let mut set = base.clone();
    set.workflow.allowed_tool_names = Some(vec!["command".into()]);
    set.workflow.orchestration = true;
    assert!(
        set.validate()
            .unwrap_err()
            .to_string()
            .contains("launch_subtasks")
    );
    for tool in ["edit", "write", "delete"] {
        let mut set = base.clone();
        set.workflow
            .allowed_tool_names
            .as_mut()
            .unwrap()
            .push(tool.into());
        assert!(
            set.validate()
                .unwrap_err()
                .to_string()
                .contains("mutation tools")
        );
    }
    let mut set = base.clone();
    set.workflow.allowed_tool_names = None;
    assert!(set.validate().is_err());
    for (root, startup) in [("", "/startup"), ("/child", " ")] {
        let mut set = base.clone();
        set.workflow.workspace = Some(WorkspaceBinding {
            root: root.into(),
            startup: startup.into(),
        });
        assert!(
            set.validate()
                .unwrap_err()
                .to_string()
                .contains("workspace binding")
        );
    }
    for module in [
        prompts::COMMAND_CONVENTIONS_INSTRUCTIONS,
        prompts::HOSTED_SEARCH_INSTRUCTIONS,
        prompts::INSPECTION_POLICY_INSTRUCTIONS,
    ] {
        for target in 0..3 {
            let mut set = base.clone();
            match target {
                0 => set.application.push_str(module),
                1 => set.workflow.instructions.push_str(module),
                _ => set.system[0].1.push_str(module),
            }
            assert!(
                set.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("embedded capability")
            );
        }
    }
}

#[test]
fn skill_snapshot_validation_and_reconciliation() {
    let skill = SkillSnapshot::new("review".parse().unwrap(), "Review", "Body").unwrap();
    let active = ActiveSkills::from_snapshots([skill.clone()]).unwrap();
    let mut state = DirectiveState::default();
    let updates = state.reconcile(&active, |_| true);
    assert_eq!(updates, vec![DirectiveContent::skill(&skill)]);
    state.apply(&updates[0]).unwrap();
    state.snapshot().validate().unwrap();
    assert!(state.reconcile(&active, |_| true).is_empty());
    let revoked = state.reconcile(&active, |_| false);
    assert!(matches!(
        revoked[0].payload,
        DirectivePayload::SkillRevocation { .. }
    ));
    state.apply(&revoked[0]).unwrap();
    assert!(state.snapshot().directives.is_empty());
    assert_eq!(state.reconcile(&active, |_| true), updates);
    assert_eq!(serde_json::to_value(&updates[0]).unwrap()["version"], 1);
    assert_eq!(
        serde_json::to_value(state.snapshot()).unwrap()["version"],
        1
    );
    for version in [0, 2, 3, 4, 99] {
        let mut invalid = updates[0].clone();
        invalid.version = version;
        assert!(invalid.validate().is_err());
        let mut invalid = state.snapshot();
        invalid.version = version;
        assert!(invalid.validate().is_err());
    }
    let mut invalid = updates[0].clone();
    invalid.text.push('!');
    assert!(invalid.validate().is_err());
    assert!(
        DirectiveSnapshot {
            version: directive::INSTRUCTION_VERSION,
            directives: revoked
        }
        .validate()
        .is_err()
    );
    assert!(
        DirectiveSnapshot {
            version: directive::INSTRUCTION_VERSION,
            directives: vec![updates[0].clone(), updates[0].clone()]
        }
        .validate()
        .is_err()
    );
}
