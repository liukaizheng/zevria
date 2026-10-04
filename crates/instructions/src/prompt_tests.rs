use super::*;
use crate::{DirectivePolicy, InstructionSet, SkillPromptCatalog};
use zevria_foundation::{ModelRole, TurnPolicy, WorkspaceBinding, WorkspaceContract};

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

const COMMAND_CALL_CONTRACT_HEADING: &str = "### Command calls and batching\n";

fn command_enabled_policies() -> [TurnPolicy; 5] {
    [
        TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, true),
        TurnPolicy::new(BUILD_MODE_INSTRUCTIONS, None, ModelRole::Build, true).with_orchestration(),
        TurnPolicy::new(
            PLAN_MODE_INSTRUCTIONS,
            Some(vec!["command".into()]),
            ModelRole::Plan,
            false,
        )
        .with_contract(WorkspaceContract::SourceReadOnlyScratch),
        TurnPolicy::new(
            EXPLORE_AGENT_INSTRUCTIONS,
            Some(vec!["command".into(), "web_search".into()]),
            ModelRole::Explore,
            false,
        )
        .with_contract(WorkspaceContract::SourceReadOnlyScratch),
        TurnPolicy::new(
            BUILD_SUBTASK_INSTRUCTIONS,
            Some(vec!["command".into(), "write".into()]),
            ModelRole::Builder,
            false,
        )
        .with_workspace(WorkspaceBinding {
            root: "/abs/child".into(),
            startup: "/abs/startup".into(),
        }),
    ]
}

fn command_disabled_policies() -> Vec<TurnPolicy> {
    command_enabled_policies()
        .into_iter()
        .map(|mut policy| {
            policy
                .allowed_tool_names
                .get_or_insert_with(|| vec!["web_search".into(), "launch_subtasks".into()])
                .retain(|name| name != "command");
            policy
        })
        .chain(std::iter::once(
            TurnPolicy::new(
                MAINTENANCE_INSTRUCTIONS,
                Some(vec![]),
                ModelRole::Build,
                false,
            )
            .with_scope("maintenance"),
        ))
        .collect()
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
    let cases = command_enabled_policies()
        .into_iter()
        .zip([
            [true, true, false],
            [true, true, false],
            [true, false, true],
            [true, true, true],
            [true, false, false],
        ])
        .chain(command_disabled_policies().into_iter().zip([
            [false, true, false],
            [false, true, false],
            [false, false, true],
            [false, true, true],
            [false, false, false],
            [false, false, false],
        ]));
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
fn command_call_contract_uses_advertised_direct_calls_and_sequential_batching() {
    let (_, contract) = COMMAND_CONVENTIONS_INSTRUCTIONS
        .split_once(COMMAND_CALL_CONTRACT_HEADING)
        .expect("command-call contract subsection");
    for required in [
        "Use the callable tool definitions advertised for the current request, including definitions supplied outside prose system guidance.",
        "Follow their exact invocation names and arguments; the workflow's registered tool names determine what is allowed.",
        "An advertised, workflow-allowed `command` tool needs no separate permission or tool-version probe merely because prose guidance omits it.",
        "Honor Plan/Explore restrictions, actual errors, and task-relevant version checks.",
        "For independent reads and searches, submit separate `command` calls together in one assistant response, with one focused RTK invocation per call.",
        "Native Zevria needs no generic parallel wrapper: call `command` directly instead of inventing an unadvertised wrapper or inferring one from model-specific habits.",
        "Do not combine unrelated reads into a shell script merely to avoid batching uncertainty.",
        "Batching reduces model round trips; it does not promise wall-clock overlap.",
        "Ordinary calls execute sequentially in assistant-call order.",
        "Wait for earlier results when later commands depend on them.",
        "Skill-only responses, subtask response-shape requirements, and Plan submission boundaries still apply.",
    ] {
        assert_eq!(contract.matches(required).count(), 1, "{required}");
    }
}

#[test]
fn command_call_contract_is_rendered_once_after_workflow() {
    for policy in command_enabled_policies() {
        for application in [DEFAULT_PREAMBLE, "Custom engineering guidance"] {
            let mut set = set(&policy);
            set.application = application.into();
            set.validate().unwrap();
            let text = set.render();
            assert_eq!(text.matches(COMMAND_CALL_CONTRACT_HEADING).count(), 1);
            assert_eq!(text.matches(COMMAND_CONVENTIONS_INSTRUCTIONS).count(), 1);
            let workflow = format!(
                "## Workflow policy: {}\n{}\n{}",
                set.workflow.scope,
                set.workflow.declaration(),
                set.workflow.instructions
            );
            let workflow_end = text.find(&workflow).unwrap() + workflow.len();
            let command_start = text.find("## Command conventions\n").unwrap();
            let contract_start = text.find(COMMAND_CALL_CONTRACT_HEADING).unwrap();
            assert!(workflow_end < command_start);
            assert!(command_start < contract_start);
        }
    }
}

#[test]
fn command_call_contract_is_absent_when_command_is_disallowed() {
    for policy in command_disabled_policies() {
        assert!(!policy.allows_tool("command"));
        for application in [DEFAULT_PREAMBLE, "Custom engineering guidance"] {
            let mut set = set(&policy);
            set.application = application.into();
            set.validate().unwrap();
            let text = set.render();
            assert!(!text.contains(COMMAND_CALL_CONTRACT_HEADING));
            assert!(!text.contains(COMMAND_CONVENTIONS_INSTRUCTIONS));
            assert!(!text.contains("## Command conventions\n"));
        }
    }
}

#[test]
fn workflow_changes_preserve_the_byte_identical_stable_prefix() {
    let policies = command_enabled_policies()
        .into_iter()
        .chain(command_disabled_policies())
        .collect::<Vec<_>>();
    for application in [DEFAULT_PREAMBLE, "Custom engineering guidance"] {
        let mut baseline = set(&policies[0]);
        baseline.application = application.into();
        let baseline = baseline.render();
        let stable_prefix = baseline
            .split_once("## Workflow policy:")
            .unwrap()
            .0
            .as_bytes();
        for policy in &policies[1..] {
            let mut set = set(policy);
            set.application = application.into();
            set.validate().unwrap();
            let text = set.render();
            assert_ne!(baseline, text);
            assert_eq!(
                stable_prefix,
                text.split_once("## Workflow policy:").unwrap().0.as_bytes()
            );
        }
    }
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
