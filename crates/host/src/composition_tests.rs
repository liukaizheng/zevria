use rig_agent::tool::Tool;
use zevria_foundation::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::ModelRole;
use zevria_foundation::QUESTION_TOOL_NAME;
use zevria_foundation::RECONCILE_REPORTS_TOOL_NAME;
use zevria_foundation::SKILL_TOOL_NAME;
use zevria_foundation::SessionMode;
use zevria_foundation::TASK_TOOL_NAME;
use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, PLAN_AUTHORING_TOOL_NAMES, READ_PLAN_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME,
    UPDATE_PLAN_TOOL_NAME,
};
use zevria_instructions::prompts::BUILD_MODE_INSTRUCTIONS;
use zevria_instructions::prompts::PLAN_MODE_INSTRUCTIONS;
use zevria_instructions::skill::workspace_skills_dir;
use zevria_session_api::QuestionRequester;
use zevria_session_api::SubtaskLauncher;
use zevria_session_api::question_channels;
use zevria_session_api::session_event_channel;
use zevria_session_api::subtask_channels;
use zevria_tools::{
    CommandTool, CreatePlanTool, DeleteTool, EditTool, LaunchSubtasksTool, QuestionTool,
    ReadPlanTool, ReconcileReportsTool, SkillTool, SubmitPlanTool, TaskTool, UpdatePlanTool,
    WriteTool,
};

use crate::config::CommandConfig;
use crate::runtime::{
    build_explore_tools, build_session_policies, build_tools, command_tool, load_session_skills,
    transcript_recovery_notice,
};
use std::path::Path;

fn test_jobs() -> zevria_session_api::CommandJobs {
    zevria_session_api::CommandJobs::new(
        crate::runtime::command_job_limits(CommandConfig::default()),
        None,
    )
}

fn test_launcher() -> SubtaskLauncher {
    let (events_tx, _events_rx) = session_event_channel(32);
    subtask_channels("test-root", events_tx).launcher
}

fn test_question_requester() -> QuestionRequester {
    let (events_tx, _events_rx) = session_event_channel(32);
    question_channels(events_tx).requester
}

#[tokio::test]
async fn production_registry_pins_names_order_and_schemas() {
    let workspace = tempfile::tempdir().expect("workspace");
    let definitions = build_tools(
        workspace.path(),
        test_launcher(),
        test_question_requester(),
        CommandConfig::default(),
        131_072,
        Some(test_jobs()),
    )
    .expect("tool server")
    .snapshot()
    .take_definitions();

    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        [
            "command",
            "command_status",
            "command_terminate",
            "task",
            "launch_subtasks",
            "edit",
            "write",
            "delete",
            "skill",
            "skill_read",
            "reconcile_reports",
            "question",
            "create_plan",
            "update_plan",
            "read_plan",
            "submit_plan",
        ]
    );
    let snapshot = definitions
        .iter()
        .map(|definition| {
            serde_json::json!({
                "name": definition.name,
                "description": definition.description,
                "parameters": definition.parameters,
            })
        })
        .collect::<Vec<_>>();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root-tools.json");
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(
            &fixture,
            format!("{}\n", serde_json::to_string_pretty(&snapshot).unwrap()),
        )
        .unwrap();
    }
    let expected: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    assert_eq!(snapshot, expected);
    let expected_parameters = vec![
        CommandTool::with_jobs(workspace.path().to_path_buf(), test_jobs()).parameters(),
        zevria_tools::CommandStatusTool::new(test_jobs()).parameters(),
        zevria_tools::CommandTerminateTool::new(test_jobs()).parameters(),
        TaskTool.parameters(),
        LaunchSubtasksTool::new(test_launcher(), workspace.path().to_path_buf()).parameters(),
        EditTool::new(workspace.path().to_path_buf()).parameters(),
        WriteTool::new(workspace.path().to_path_buf()).parameters(),
        DeleteTool::new(workspace.path().to_path_buf()).parameters(),
        SkillTool.parameters(),
        zevria_tools::SkillReadTool.parameters(),
        ReconcileReportsTool.parameters(),
        QuestionTool::new(test_question_requester()).parameters(),
        CreatePlanTool::new(131_072).parameters(),
        UpdatePlanTool::new(131_072).parameters(),
        ReadPlanTool.parameters(),
        SubmitPlanTool::new(131_072).parameters(),
    ];
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.parameters.clone())
            .collect::<Vec<_>>(),
        expected_parameters
    );
    let required = definitions
        .iter()
        .map(|definition| {
            (
                definition.name.as_str(),
                definition.parameters["required"].clone(),
                definition.parameters["additionalProperties"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        required,
        vec![
            (
                "command",
                serde_json::json!(["command"]),
                serde_json::json!(false)
            ),
            (
                "command_status",
                serde_json::json!([]),
                serde_json::json!(false)
            ),
            (
                "command_terminate",
                serde_json::json!(["id"]),
                serde_json::json!(false)
            ),
            (
                "task",
                serde_json::json!(["tasks"]),
                serde_json::json!(false)
            ),
            (
                "launch_subtasks",
                serde_json::json!(["tasks"]),
                serde_json::json!(false),
            ),
            (
                "edit",
                serde_json::json!(["file_path"]),
                serde_json::json!(false)
            ),
            (
                "write",
                serde_json::json!(["file_path", "content"]),
                serde_json::json!(false),
            ),
            (
                "delete",
                serde_json::json!(["file_path"]),
                serde_json::json!(false)
            ),
            (
                "skill",
                serde_json::json!(["skill"]),
                serde_json::json!(false)
            ),
            (
                "skill_read",
                serde_json::json!(["skill", "resource"]),
                serde_json::json!(false)
            ),
            (
                "reconcile_reports",
                serde_json::json!(["disagreements", "decisions", "unavailable_decisions"]),
                serde_json::json!(false)
            ),
            (
                "question",
                serde_json::json!(["questions"]),
                serde_json::json!(false)
            ),
            (
                "create_plan",
                serde_json::json!(["markdown", "replace"]),
                serde_json::json!(false)
            ),
            (
                "update_plan",
                serde_json::json!(["edits"]),
                serde_json::json!(false)
            ),
            ("read_plan", serde_json::json!([]), serde_json::json!(false)),
            (
                "submit_plan",
                serde_json::json!([]),
                serde_json::json!(false)
            ),
        ]
    );
    assert!(
        definitions
            .iter()
            .all(|definition| !definition.description.is_empty())
    );
}

#[tokio::test]
async fn empty_skill_registry_keeps_skill_capable_mode_tools_advertised() {
    let workspace = tempfile::tempdir().expect("workspace");
    let definitions = build_tools(
        workspace.path(),
        test_launcher(),
        test_question_requester(),
        CommandConfig::default(),
        131_072,
        Some(test_jobs()),
    )
    .expect("tool server")
    .snapshot()
    .take_definitions();
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == SKILL_TOOL_NAME)
    );
    let policies = build_session_policies(zevria_workflow::config::PlanConfig::default(), true);
    for mode in [SessionMode::Build, SessionMode::Plan] {
        let policy = policies.policy(mode);
        assert!(policy.skills_enabled);
        for tool in [SKILL_TOOL_NAME, "skill_read"] {
            assert!(policy.allows_tool(tool));
        }
    }
}

#[tokio::test]
async fn explore_registry_is_command_only_and_uses_configured_limits() {
    let workspace = tempfile::tempdir().expect("workspace");
    let command_config = CommandConfig {
        timeout_seconds: 17,
        capture_bytes: 42,
        ..CommandConfig::default()
    };
    let definitions = build_explore_tools(workspace.path(), command_config)
        .expect("tool server")
        .snapshot()
        .take_definitions();
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        ["command"]
    );
    assert_eq!(
        definitions[0].description,
        command_tool(workspace.path().to_path_buf(), command_config)
            .expect("configured command")
            .description()
    );
    // A bypassed policy still cannot reach any privileged workflow or
    // structured mutation tool because none is registered.
    for absent in [
        SKILL_TOOL_NAME,
        RECONCILE_REPORTS_TOOL_NAME,
        SUBMIT_PLAN_TOOL_NAME,
        CREATE_PLAN_TOOL_NAME,
        UPDATE_PLAN_TOOL_NAME,
        READ_PLAN_TOOL_NAME,
        LAUNCH_SUBTASKS_TOOL_NAME,
        QUESTION_TOOL_NAME,
        TASK_TOOL_NAME,
        "edit",
        "write",
        "delete",
    ] {
        assert!(
            !definitions
                .iter()
                .any(|definition| definition.name == absent),
            "{absent} must not be registered"
        );
    }
}

#[test]
fn production_policies_pin_mode_directives_and_plan_tool_access() {
    let policies = build_session_policies(zevria_workflow::config::PlanConfig::default(), true);
    let build = policies.policy(SessionMode::Build);
    assert_eq!(build.model_role, ModelRole::Build);
    assert_eq!(build.instructions, BUILD_MODE_INSTRUCTIONS);
    assert_eq!(
        build.allowed_tool_names.as_deref(),
        Some(
            &[
                "web_search".to_string(),
                "command".to_string(),
                "command_status".to_string(),
                "command_terminate".to_string(),
                "task".to_string(),
                "edit".to_string(),
                "write".to_string(),
                "delete".to_string(),
                "launch_subtasks".to_string(),
                "skill".to_string(),
                "skill_read".to_string(),
            ][..]
        )
    );
    assert!(build.allows_tool("command"));
    assert!(build.allows_tool("task"));
    assert!(build.allows_tool("launch_subtasks"));
    assert!(build.allows_tool("write"));
    assert!(build.allows_tool("skill"));
    assert!(build.skills_enabled);
    for name in PLAN_AUTHORING_TOOL_NAMES {
        assert!(!build.allows_tool(name));
    }
    assert!(!build.allows_tool(RECONCILE_REPORTS_TOOL_NAME));
    assert!(build.orchestration);
    assert_eq!(
        build.subtask_kinds(),
        Some(&[zevria_foundation::SubtaskKind::Explore][..])
    );

    let plan = policies.policy(SessionMode::Plan);
    assert_eq!(plan.model_role, ModelRole::Plan);
    assert_eq!(plan.instructions, PLAN_MODE_INSTRUCTIONS);
    assert!(!plan.orchestration);
    assert_eq!(
        plan.contract,
        zevria_foundation::WorkspaceContract::SourceReadOnlyScratch
    );
    assert_eq!(
        plan.allowed_tool_names.as_deref(),
        Some(
            &[
                "command".to_string(),
                "command_status".to_string(),
                "command_terminate".to_string(),
                "web_search".to_string(),
                "launch_subtasks".to_string(),
                "skill".to_string(),
                "skill_read".to_string(),
                "question".to_string(),
                "create_plan".to_string(),
                "update_plan".to_string(),
                "read_plan".to_string(),
                "submit_plan".to_string(),
            ][..]
        )
    );
    assert!(plan.allows_tool("command"));
    assert!(plan.allows_tool("launch_subtasks"));
    assert!(plan.allows_tool("skill"));
    assert!(plan.skills_enabled);
    assert!(plan.allows_tool("question"));
    assert!(plan.allows_tool("submit_plan"));
    assert!(!plan.allows_tool(RECONCILE_REPORTS_TOOL_NAME));
    for denied in ["task", "edit", "write", "delete"] {
        assert!(!plan.allows_tool(denied));
    }
}

#[test]
fn plan_configuration_can_only_remove_optional_capabilities() {
    let policies = build_session_policies(
        zevria_workflow::config::PlanConfig {
            allow_subtasks: false,
            allow_skills: false,
            ..zevria_workflow::config::PlanConfig::default()
        },
        true,
    );
    let plan = policies.policy(SessionMode::Plan);
    assert_eq!(
        plan.allowed_tool_names.as_deref(),
        Some(
            &[
                "command".to_string(),
                "command_status".to_string(),
                "command_terminate".to_string(),
                "web_search".to_string(),
                "question".to_string(),
                "create_plan".to_string(),
                "update_plan".to_string(),
                "read_plan".to_string(),
                "submit_plan".to_string(),
            ][..]
        )
    );
    for denied in [
        "edit",
        "write",
        "delete",
        LAUNCH_SUBTASKS_TOOL_NAME,
        SKILL_TOOL_NAME,
    ] {
        assert!(!plan.allows_tool(denied));
    }
    assert!(plan.allows_tool("command"));
    assert!(plan.allows_tool(QUESTION_TOOL_NAME));
    assert!(plan.allows_tool(SUBMIT_PLAN_TOOL_NAME));

    let build = policies.policy(SessionMode::Build);
    assert!(build.allows_tool(LAUNCH_SUBTASKS_TOOL_NAME));
    assert!(build.allows_tool(SKILL_TOOL_NAME));
    assert!(build.skills_enabled);
    assert!(!build.allows_tool(QUESTION_TOOL_NAME));
    assert!(!build.allows_tool(SUBMIT_PLAN_TOOL_NAME));
}

#[tokio::test]
async fn zero_background_cap_keeps_the_foreground_only_registry_and_policies() {
    let workspace = tempfile::tempdir().unwrap();
    let config = CommandConfig {
        max_background_jobs: 0,
        ..CommandConfig::default()
    };
    let definitions = build_tools(
        workspace.path(),
        test_launcher(),
        test_question_requester(),
        config,
        131_072,
        None,
    )
    .unwrap()
    .snapshot()
    .take_definitions();
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../tests/fixtures/root-tools.json")).unwrap();
    assert_eq!(definitions.len(), fixture.len() - 2);
    assert_eq!(
        definitions[0].description,
        command_tool(workspace.path().into(), config)
            .unwrap()
            .description()
    );
    assert_eq!(
        definitions[0].parameters,
        CommandTool::new(workspace.path().into()).parameters()
    );
    for (definition, expected) in definitions[1..].iter().zip(&fixture[3..]) {
        assert_eq!(
            serde_json::json!({"name": definition.name, "description": definition.description, "parameters": definition.parameters}),
            *expected
        );
    }
    let policies = build_session_policies(Default::default(), false);
    for mode in SessionMode::ALL {
        for name in zevria_foundation::COMMAND_JOB_TOOL_NAMES {
            assert!(!policies.policy(mode).allows_tool(name));
        }
    }
}

#[test]
fn session_skills_load_from_the_workspace_root() {
    let workspace = tempfile::tempdir().expect("workspace");
    let skills_dir = workspace_skills_dir(workspace.path());
    std::fs::create_dir_all(&skills_dir).expect("skills directory");
    std::fs::write(
        skills_dir.join("demo.md"),
        "---\ndescription: A demo skill\n---\nDemo instructions\n",
    )
    .expect("skill file");

    let skills = load_session_skills(workspace.path());
    assert!(skills.selected.contains_key("demo"));

    let empty = tempfile::tempdir().expect("empty workspace");
    // A workspace without skills may still surface user-global ones;
    // the load itself must simply not fail.
    let _ = load_session_skills(empty.path());
}

#[tokio::test]
async fn skill_tool_definition_has_a_stable_bodyless_contract() {
    let workspace = tempfile::tempdir().expect("workspace");
    let definitions = build_tools(
        workspace.path(),
        test_launcher(),
        test_question_requester(),
        CommandConfig::default(),
        131_072,
        Some(test_jobs()),
    )
    .expect("tool server")
    .snapshot()
    .take_definitions();
    let skill_definition = definitions
        .iter()
        .find(|definition| definition.name == SKILL_TOOL_NAME)
        .expect("skill definition");

    // Metadata lives in the complete prompt catalog, never in
    // the static tool description. Bodies remain engine-owned overlays.
    assert!(
        skill_definition
            .description
            .contains("complete engine-provided eligible catalog")
    );
    assert!(
        !skill_definition
            .description
            .contains("- demo: A demo skill")
    );
    assert!(!skill_definition.description.contains("Demo instructions"));
    assert!(
        !skill_definition
            .description
            .contains("At the start of a request")
    );
    assert!(
        !skill_definition
            .description
            .contains("remain authoritative")
    );
}

#[test]
fn built_in_prompt_stays_non_empty() {
    assert!(
        !zevria_instructions::prompts::DEFAULT_PREAMBLE
            .trim()
            .is_empty()
    );
}

#[test]
fn transcript_recovery_notice_is_visible_only_when_records_were_removed() {
    let path = Path::new("/workspace/.zevria/sessions/example.jsonl");
    assert_eq!(transcript_recovery_notice(path, 0), None);
    let notice = transcript_recovery_notice(path, 3).expect("recovery notice");
    assert!(notice.contains("example.jsonl"));
    assert!(notice.contains("3 malformed record(s)"));
}
