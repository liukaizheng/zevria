//! Tool calling mechanics and instruction modules must not repeat long sentences.
use rig_agent::tool::server::ToolServer;
use zevria_session_api::{question_channels, session_event_channel, subtask_channels};
use zevria_tools::*;

#[path = "../../instructions/src/prompt_test_support.rs"]
mod support;

#[tokio::test]
async fn tool_descriptions_and_instruction_modules_have_distinct_owners() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let (events, _receiver) = session_event_channel(32);
    let questions = question_channels(events.clone());
    let subtasks = subtask_channels("ownership-test", events);
    let tools = ToolServer::new()
        .tool(CommandTool::new(root.clone()))
        .tool(TaskTool)
        .tool(EditTool::new(root.clone()))
        .tool(WriteTool::new(root.clone()))
        .tool(DeleteTool::new(root.clone()))
        .tool(LaunchSubtasksTool::new(subtasks.launcher, root))
        .tool(SkillTool)
        .tool(SkillReadTool)
        .tool(QuestionTool::new(questions.requester))
        .tool(SubmitPlanTool::new(1024 * 1024))
        .tool(ReconcileReportsTool)
        .run();
    let definitions = tools.static_tool_defs();
    let modules = support::module_docs();
    assert!(
        modules
            .iter()
            .any(|(_, body)| *body == zevria_instructions::prompts::ENGINE_PROTOCOL_INSTRUCTIONS)
    );
    for tool in &definitions {
        support::assert_non_overlapping(modules.iter().copied().chain(std::iter::once((
            tool.name.as_str(),
            tool.description.as_str(),
        ))));
    }
}
