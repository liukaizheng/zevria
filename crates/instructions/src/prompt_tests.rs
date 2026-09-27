use super::*;
use crate::{DirectivePolicy, InstructionSet, SkillPromptCatalog};
use zevria_foundation::{ModelRole, TurnPolicy, WorkspaceContract};

#[path = "prompt_test_support.rs"]
mod support;

fn set(policy: &TurnPolicy) -> InstructionSet {
    InstructionSet {
        application: DEFAULT_PREAMBLE.into(),
        system: vec![
            ("project".into(), "Scope: project\nProject body".into()),
            ("global".into(), "Scope: global\nGlobal body".into()),
        ],
        workflow: DirectivePolicy::new(&policy.scope, policy),
        catalog: Some(SkillPromptCatalog {
            enabled: policy.skills_enabled,
            entries: vec![],
        }),
    }
}

fn workflow_docs() -> Vec<&'static str> {
    vec![
        BUILD_MODE_INSTRUCTIONS,
        PLAN_MODE_INSTRUCTIONS,
        EXPLORE_AGENT_INSTRUCTIONS,
        BUILD_SUBTASK_INSTRUCTIONS,
        ENSEMBLE_WORKER_PLAN_INSTRUCTIONS,
        ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS,
        ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS,
        ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS,
        MAINTENANCE_INSTRUCTIONS,
    ]
}

#[test]
fn modules_have_one_owner_and_no_top_level_headings() {
    let docs = support::module_docs();
    support::assert_non_overlapping(docs.iter().copied());
    for (name, body) in docs {
        assert!(
            !body
                .lines()
                .any(|line| line.starts_with("# ") || line.starts_with("## ")),
            "module heading: {name}"
        );
    }
    for workflow in workflow_docs() {
        for capability in [
            COMMAND_CONVENTIONS_INSTRUCTIONS,
            HOSTED_SEARCH_INSTRUCTIONS,
            INSPECTION_POLICY_INSTRUCTIONS,
            SKILL_SELECTION_INSTRUCTIONS,
        ] {
            assert!(!workflow.contains(capability.trim()));
        }
        for forbidden in [
            "permitted tools",
            "is unavailable",
            "only local tool",
            "exactly the local tools",
            "never override",
            "remain authoritative",
            "rtk",
            "web_search",
            "skill catalog",
        ] {
            assert!(
                !workflow.to_lowercase().contains(forbidden),
                "workflow restates {forbidden}"
            );
        }
    }
}

#[test]
fn render_is_a_pure_module_join() {
    let policy = TurnPolicy::new(
        PLAN_MODE_INSTRUCTIONS,
        Some(
            [
                "command",
                "web_search",
                "launch_subtasks",
                "skill",
                "skill_read",
            ]
            .map(str::to_string)
            .to_vec(),
        ),
        ModelRole::Plan,
        true,
    )
    .with_contract(WorkspaceContract::SourceReadOnlyScratch);
    let set = set(&policy);
    set.validate().unwrap();
    let rendered = set.render();
    // The join adds two newlines; splitting at the heading consumes one.
    let modules = rendered.split("\n## ").collect::<Vec<_>>();
    assert_eq!(modules[0], format!("{ENGINE_PROTOCOL_INSTRUCTIONS}\n"));
    assert_eq!(
        modules[1],
        format!("Application guidance\n{}\n", set.application)
    );
    assert_eq!(
        modules[2],
        format!(
            "File guidance\n{}\n\n{}\n",
            set.system[1].1, set.system[0].1
        )
    );
    assert_eq!(
        modules[3],
        format!(
            "Workflow policy: plan\n{}\n{PLAN_MODE_INSTRUCTIONS}\n",
            set.workflow.declaration()
        )
    );
    assert_eq!(
        modules[4],
        format!("Command conventions\n{COMMAND_CONVENTIONS_INSTRUCTIONS}\n")
    );
    assert_eq!(
        modules[5],
        format!("Hosted search\n{HOSTED_SEARCH_INSTRUCTIONS}\n")
    );
    assert_eq!(
        modules[6],
        format!("Inspection and scratch policy\n{INSPECTION_POLICY_INSTRUCTIONS}\n")
    );
    assert_eq!(
        modules[7],
        format!("Eligible skills\n{SKILL_SELECTION_INSTRUCTIONS}\n[]")
    );
    assert_eq!(modules.len(), 8);
    let declaration: serde_json::Value =
        serde_json::from_str(modules[3].lines().nth(1).unwrap()).unwrap();
    assert_eq!(declaration["inspection"], "source_read_only_scratch");
    assert_eq!(declaration["subtasks"], serde_json::json!(["explore"]));
}

#[test]
fn capability_sections_are_selected_exactly_once() {
    let cases = [
        (
            TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, true),
            [true, true, false],
        ),
        (
            TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, true)
                .with_orchestration(),
            [true, true, false],
        ),
        (
            TurnPolicy::new(
                PLAN_MODE_INSTRUCTIONS,
                Some(vec!["command".into()]),
                ModelRole::Plan,
                false,
            )
            .with_contract(WorkspaceContract::SourceReadOnlyScratch),
            [true, false, true],
        ),
        (
            TurnPolicy::new(
                EXPLORE_AGENT_INSTRUCTIONS,
                Some(vec!["command".into(), "web_search".into()]),
                ModelRole::Explore,
                false,
            )
            .with_contract(WorkspaceContract::SourceReadOnlyScratch),
            [true, true, true],
        ),
        (
            TurnPolicy::new(
                MAINTENANCE_INSTRUCTIONS,
                Some(vec![]),
                ModelRole::Build,
                false,
            )
            .with_scope("maintenance"),
            [false, false, false],
        ),
    ];
    for (policy, expected) in cases {
        let set = set(&policy);
        set.validate().unwrap();
        let rendered = set.render();
        for ((title, body), enabled) in [
            ("Command conventions", COMMAND_CONVENTIONS_INSTRUCTIONS),
            ("Hosted search", HOSTED_SEARCH_INSTRUCTIONS),
            (
                "Inspection and scratch policy",
                INSPECTION_POLICY_INSTRUCTIONS,
            ),
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(
                rendered.matches(&format!("## {title}\n")).count(),
                usize::from(enabled)
            );
            assert_eq!(rendered.matches(body).count(), usize::from(enabled));
        }
    }
}

#[test]
fn workflow_changes_preserve_the_byte_identical_stable_prefix() {
    let build = TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, true);
    let orchestrate = TurnPolicy::new(
        PLAN_MODE_INSTRUCTIONS,
        Some(vec!["command".into()]),
        ModelRole::Plan,
        true,
    );
    let build = set(&build).render();
    let orchestrate = set(&orchestrate).render();
    assert_ne!(build, orchestrate);
    assert_eq!(
        build
            .split_once("## Workflow policy:")
            .unwrap()
            .0
            .as_bytes(),
        orchestrate
            .split_once("## Workflow policy:")
            .unwrap()
            .0
            .as_bytes()
    );
}

#[test]
fn custom_preamble_does_not_replace_engine_capabilities() {
    let policy = TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, false);
    let mut set = set(&policy);
    set.application = "Custom engineering guidance".into();
    let text = set.render();
    assert!(text.starts_with(ENGINE_PROTOCOL_INSTRUCTIONS));
    assert!(text.contains(COMMAND_CONVENTIONS_INSTRUCTIONS));
    assert!(!text.contains(DEFAULT_PREAMBLE));
}
