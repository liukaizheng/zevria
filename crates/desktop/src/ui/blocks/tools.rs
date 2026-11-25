//! Structured, display-only views for known desktop tool steps.
use super::command::CommandOutput;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use gpui_kit::component::diff::DiffFile;
use rig_core::message::ToolCall;
use std::{ops::Range, sync::Arc, time::SystemTime};
use zevria_content::{ImageReadSummary, readable::sanitize_readable};
use zevria_foundation::{
    CommandJobId, CommandJobSnapshot, CommandJobStatus, FileChange, FileChangeOutput,
    QuestionResponse, READ_IMAGE_TOOL_NAME, WEB_SEARCH_TOOL_NAME, WebSearchResultDetail,
};
use zevria_presentation::{
    blocks::*,
    jobs::JobsInspector,
    shell::{self, ShellToken},
    status_icon::StatusIcon,
};

pub fn tool_label(name: &str) -> std::borrow::Cow<'_, str> {
    if let Some(label) = mcp_tool_label(name) {
        return label.into();
    }
    match name {
        "command" => "Command",
        "command_status" => "Check job",
        "command_terminate" => "Stop job",
        "edit" => "Edit",
        "write" => "Write",
        "delete" => "Delete",
        "read" => "Read",
        READ_IMAGE_TOOL_NAME => "Image",
        "skill" => "Skill",
        "skill_read" => "Read skill",
        "web_search" => "Search",
        "question" => "Question",
        "create_plan" | "update_plan" | "read_plan" | "submit_plan" => "Plan draft",
        "task" => "Tasks",
        "reconcile_reports" => "Reconcile",
        "launch_subtasks" => "Subtasks",
        _ => name,
    }
    .into()
}

#[derive(Debug)]
pub struct JobMeta {
    pub id: CommandJobId,
    pub running: bool,
    pub foreground: bool,
    pub status: CommandJobStatus,
    pub started_at: SystemTime,
}
impl From<&CommandJobSnapshot> for JobMeta {
    fn from(job: &CommandJobSnapshot) -> Self {
        Self {
            id: job.id.clone(),
            running: JobsInspector::can_terminate(job),
            foreground: job.mode == zevria_foundation::CommandJobMode::Foreground,
            status: job.status.clone(),
            started_at: job.started_at,
        }
    }
}

#[derive(Debug)]
pub enum ToolDetail {
    Command {
        input: String,
        duration: Option<String>,
        exit: Option<String>,
        lines: Option<usize>,
    },
    File,
    List {
        introduction: Option<String>,
        items: Vec<ListItem>,
    },
    Questions(Vec<QuestionDetail>),
    Image(Option<ImageReadSummary>),
    WebSearch {
        activity: Vec<WebActionPresentation>,
        running_label: Option<String>,
        completed: Option<Box<WebSearchResultDetail>>,
    },
    Standard {
        markdown: bool,
        show_output: bool,
    },
}

#[derive(Debug)]
pub struct ListItem {
    pub status: Option<StatusIcon>,
    pub text: String,
    pub muted: bool,
}

#[derive(Debug)]
pub struct QuestionDetail {
    pub question: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug)]
pub struct ToolPresentation {
    pub name: String,
    pub summary: String,
    pub status: StatusIcon,
    pub highlights: Vec<(Range<usize>, ShellToken)>,
    pub detail: ToolDetail,
    pub body: String,
    pub diagnostic: Option<String>,
    pub changes: Arc<[FileChangeOutput]>,
    pub added: usize,
    pub removed: usize,
    pub background: bool,
    pub job: Option<JobMeta>,
}

impl ToolPresentation {
    pub fn new(tool: &PresentedTool, raw_output: Option<&str>) -> Self {
        match tool {
            PresentedTool::Native { call, state } => Self::native(call, state, raw_output),
            PresentedTool::Acp(tool) => {
                let summary = tool
                    .locations
                    .first()
                    .map(|l| l.path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let changes = tool.metadata.as_ref().map_or(&[][..], |m| m.file_changes());
                let (added, removed) = counts(changes);
                let diagnostic = tool
                    .metadata
                    .as_ref()
                    .and_then(|m| m.diagnostic.as_deref())
                    .map(sanitize_readable);
                Self {
                    name: tool.title.clone(),
                    summary,
                    status: tool.presented_status().icon(),
                    highlights: vec![],
                    detail: if changes.is_empty() {
                        ToolDetail::Standard {
                            markdown: false,
                            show_output: tool.content.is_empty(),
                        }
                    } else {
                        ToolDetail::File
                    },
                    body: bounded(&tool.content.join("\n")),
                    diagnostic,
                    changes: changes.into(),
                    added,
                    removed,
                    background: false,
                    job: None,
                }
            }
        }
    }

    fn native(call: &ToolCall, state: &NativeToolState, output: Option<&str>) -> Self {
        let name = call.function.name.as_str();
        let mut status = native_header_status(call, state);
        let mut summary = tool_summary(name, state.arguments.as_ref());
        let mut highlights = Vec::new();
        let mut body = String::new();
        let mut changes = state
            .metadata
            .as_ref()
            .map_or_else(Vec::new, |m| m.file_changes().to_vec());
        let mut diagnostic = state
            .metadata
            .as_ref()
            .and_then(|m| m.diagnostic.as_deref())
            .map(sanitize_readable);
        let failed = state.result.as_ref().is_some_and(|result| result.is_error);
        if failed && status == StatusIcon::Unknown {
            status = StatusIcon::Failed;
        }
        let mut detail = match name {
            "command" | "command_status" | "command_terminate" => {
                let parsed = output.map(CommandOutput::parse);
                // A successful tool invocation can still report a failed process.
                if matches!(status, StatusIcon::Done | StatusIcon::Unknown)
                    && let Some(CommandOutput::Envelope(result)) = &parsed
                {
                    let code = result.exit_code.and_then(|code| code.parse::<i32>().ok());
                    if result.signal.is_some() || code.is_some_and(|code| code != 0) {
                        status = StatusIcon::Failed;
                    } else if code == Some(0) {
                        status = StatusIcon::Done;
                    }
                }
                let input = if name == "command" {
                    let display = shell::display(&summary);
                    summary = display.text;
                    highlights = display.spans;
                    tool_argument(state, "command").unwrap_or_default()
                } else {
                    String::new()
                };
                ToolDetail::Command {
                    input,
                    duration: parsed
                        .as_ref()
                        .and_then(CommandOutput::duration)
                        .map(str::to_owned),
                    exit: parsed.as_ref().and_then(CommandOutput::exit_badge),
                    lines: parsed.as_ref().and_then(CommandOutput::output_line_count),
                }
            }
            "edit" | "write" | "delete" => {
                if changes.is_empty() || failed {
                    body = output.map(plain_diagnostic).unwrap_or_default();
                }
                ToolDetail::File
            }
            READ_IMAGE_TOOL_NAME => {
                changes.clear();
                if failed || native_tool_status(state).unsuccessful() {
                    body = output.map(plain_diagnostic).unwrap_or_default();
                }
                ToolDetail::Image(image_read_summary(state))
            }
            WEB_SEARCH_TOOL_NAME => {
                let completed = state.web_search_detail();
                if let Some(text) = output {
                    body = completed.map_or_else(
                        || plain_diagnostic(text),
                        |detail| web_search_answer(text, detail),
                    );
                }
                let activity = completed.map_or_else(
                    || web_search_actions(&state.web_search_activity),
                    |detail| {
                        detail
                            .actions
                            .iter()
                            .enumerate()
                            .map(|(index, action)| {
                                WebActionPresentation::from_summary(index as u64, action)
                            })
                            .collect()
                    },
                );
                ToolDetail::WebSearch {
                    activity,
                    running_label: (status == StatusIcon::Running)
                        .then(|| web_search_running_label(&state.web_search_activity)),
                    completed: completed.cloned().map(Box::new),
                }
            }
            "task" | "reconcile_reports" => {
                if let Some(list) = native_list(call, state) {
                    ToolDetail::List {
                        introduction: list.explanation,
                        items: list
                            .rows
                            .into_iter()
                            .map(|row| ListItem {
                                status: match row.marker {
                                    ListMarker::Status(s) => Some(s),
                                    ListMarker::Disagreement => None,
                                },
                                text: bounded(&format!("{}{}", row.text, row.annotation)),
                                muted: row.muted,
                            })
                            .collect(),
                    }
                } else {
                    ToolDetail::Standard {
                        markdown: false,
                        show_output: true,
                    }
                }
            }
            "question" => {
                let entries = question_details(state);
                if let Some(answer) = entries
                    .first()
                    .and_then(|question| question.answer.as_deref())
                {
                    summary = one_line(&format!("{summary} → {answer}"));
                }
                ToolDetail::Questions(entries)
            }
            "create_plan" => {
                body = tool_argument(state, "markdown").unwrap_or_default();
                ToolDetail::Standard {
                    markdown: true,
                    show_output: body.is_empty() || failed,
                }
            }
            "update_plan" | "submit_plan" | "read_plan" | "skill" | "skill_read" => {
                ToolDetail::Standard {
                    markdown: false,
                    show_output: true,
                }
            }
            "launch_subtasks" => {
                body = launch_batch_notice(state).map_or_else(String::new, |notice| match notice {
                    LaunchBatchNotice::Rejected { denied: true } => "Subtask batch denied".into(),
                    LaunchBatchNotice::Rejected { denied: false } => {
                        "Subtask batch rejected".into()
                    }
                    LaunchBatchNotice::Ended(_) => "Subtask batch ended".into(),
                    LaunchBatchNotice::Unlaunched { missing, requested } => {
                        format!("{missing} of {requested} requested subtasks were not launched")
                    }
                });
                ToolDetail::Standard {
                    markdown: false,
                    show_output: failed,
                }
            }
            _ => {
                if output.is_none() {
                    body = state
                        .arguments
                        .as_ref()
                        .and_then(serde_json::Value::as_object)
                        .into_iter()
                        .flatten()
                        .filter_map(|(k, v)| {
                            v.as_str()
                                .filter(|s| *s != summary)
                                .map(|s| format!("{k}: {s}"))
                        })
                        .take(8)
                        .collect::<Vec<_>>()
                        .join("\n");
                }
                ToolDetail::Standard {
                    markdown: false,
                    show_output: true,
                }
            }
        };
        if let Some(draft) = state
            .metadata
            .as_ref()
            .and_then(|m| m.detail.as_ref())
            .and_then(|d| d.plan_draft())
        {
            summary = one_line(&draft.title);
            body = format!(
                "Revision {} · {} · {}",
                draft.target_revision,
                draft
                    .sequence
                    .map_or_else(|| "Submitted".into(), |n| format!("Draft {n}")),
                zevria_presentation::jobs::byte_label(draft.bytes)
            );
            if let Some(change) = &draft.change {
                changes.push(FileChangeOutput {
                    path: draft
                        .projection
                        .clone()
                        .unwrap_or_else(|| "Plan working copy".into()),
                    change: change.clone(),
                });
            }
            detail = ToolDetail::Standard {
                markdown: false,
                show_output: failed,
            };
        }
        if diagnostic.as_deref() == Some(body.as_str()) {
            diagnostic = None;
        }
        let (added, removed) = counts(&changes);
        Self {
            name: name.to_owned(),
            summary,
            status,
            highlights,
            detail,
            body: bounded(&body),
            diagnostic,
            changes: changes.into(),
            added,
            removed,
            background: state.is_background_command(),
            job: state.command_job.as_ref().map(JobMeta::from),
        }
    }
}

pub fn bounded(text: &str) -> String {
    let mut value = text.chars().take(24_000).collect::<String>();
    if value.len() < text.len() {
        value.push_str("\nShowing the first 24,000 characters. Copy includes everything.");
    }
    value
}

pub fn plain_diagnostic(text: &str) -> String {
    sanitize_readable(text.strip_prefix("status: error\nerror: ").unwrap_or(text))
}

fn one_line(value: &str) -> String {
    let line = value.lines().next().unwrap_or_default();
    let mut chars = line.chars();
    let mut text: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn patch(changes: &[FileChangeOutput]) -> String {
    let mut patch = String::new();
    for file in changes {
        let path = file.path.to_string_lossy();
        match &file.change {
            FileChange::Update { unified_diff, .. } => {
                if !unified_diff.starts_with("--- ") {
                    patch.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
                }
                patch.push_str(unified_diff);
                patch.push('\n');
            }
            FileChange::Add { content } | FileChange::Delete { content } => {
                let add = matches!(file.change, FileChange::Add { .. });
                let lines = content.lines().take(1000).collect::<Vec<_>>();
                patch.push_str(&format!(
                    "--- a/{path}\n+++ b/{path}\n@@ -{} +{} @@\n",
                    if add {
                        "0,0".into()
                    } else {
                        format!("1,{}", lines.len())
                    },
                    if add {
                        format!("1,{}", lines.len())
                    } else {
                        "0,0".into()
                    }
                ));
                for line in lines {
                    patch.push(if add { '+' } else { '-' });
                    patch.push_str(line);
                    patch.push('\n');
                }
            }
            FileChange::Omitted { .. } => {}
        }
        if patch.len() > 131_072 {
            break;
        }
    }
    patch
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn parse(changes: &[FileChangeOutput]) -> Result<Vec<DiffFile>, String> {
    let patch = patch(changes);
    if patch.len() > 131_072 {
        return Err(
            "This diff is larger than 128 KiB, so it isn't shown. Copy the output to see it."
                .into(),
        );
    }
    DiffFile::parse(&patch).map_err(|e| e.to_string())
}
pub fn tool_summary(name: &str, arguments: Option<&serde_json::Value>) -> String {
    let empty = serde_json::Map::new();
    let args = arguments
        .and_then(serde_json::Value::as_object)
        .unwrap_or(&empty);
    let string = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let count = |key: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
    };
    let value = match name {
        "command" => string("command").unwrap_or_default().into(),
        "command_status" => string("id").unwrap_or("All jobs").into(),
        "command_terminate" => string("id").unwrap_or_default().into(),
        "read" => {
            let path = string("file_path")
                .or_else(|| string("path"))
                .unwrap_or_default();
            let number = |key| args.get(key).and_then(serde_json::Value::as_u64);
            let range = args.get("line_range");
            let start = number("start_line")
                .or_else(|| number("offset"))
                .or_else(|| range?.get(0)?.as_u64());
            let end = number("end_line")
                .or_else(|| range?.get(1)?.as_u64())
                .or_else(|| start?.checked_add(number("limit")?.checked_sub(1)?));
            match (start, end) {
                (Some(a), Some(b)) => format!("{path}:{a}–{b}"),
                (Some(a), None) | (None, Some(a)) => format!("{path}:{a}"),
                _ => path.into(),
            }
        }
        "edit" | "write" | "delete" | "read_image" => string("file_path")
            .or_else(|| string("path"))
            .unwrap_or_default()
            .into(),
        "skill" | "skill_read" => match (string("skill"), string("resource")) {
            (Some(skill), Some(resource)) => format!("{skill} / {resource}"),
            (Some(skill), None) => skill.into(),
            _ => String::new(),
        },
        "web_search" => string("query")
            .or_else(|| string("q"))
            .unwrap_or_default()
            .into(),
        "question" => args
            .get("questions")
            .and_then(|q| q.get(0))
            .and_then(|q| q.get("question"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .into(),
        "create_plan" | "update_plan" | "read_plan" | "submit_plan" => {
            string("title").unwrap_or("Plan working copy").into()
        }
        "task" => count("tasks")
            .map(|n| format!("{n} {}", if n == 1 { "item" } else { "items" }))
            .unwrap_or_default(),
        "reconcile_reports" => {
            let n: usize = ["disagreements", "decisions", "unavailable_decisions"]
                .iter()
                .filter_map(|k| count(k))
                .sum();
            format!("{n} {}", if n == 1 { "item" } else { "items" })
        }
        "launch_subtasks" => count("tasks")
            .map(|n| {
                format!(
                    "{n} {} in batch",
                    if n == 1 { "subtask" } else { "subtasks" }
                )
            })
            .unwrap_or_default(),
        _ => args
            .values()
            .filter_map(serde_json::Value::as_str)
            .find(|s| s.chars().count() <= 120)
            .unwrap_or_default()
            .into(),
    };
    one_line(&value)
}

pub fn counts(changes: &[FileChangeOutput]) -> (usize, usize) {
    changes.iter().fold((0, 0), |(a, r), file| {
        let (added, removed) = match &file.change {
            FileChange::Add { content } => (content.lines().count(), 0),
            FileChange::Delete { content } => (0, content.lines().count()),
            FileChange::Update { unified_diff, .. } => (
                unified_diff
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                    .count(),
                unified_diff
                    .lines()
                    .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                    .count(),
            ),
            FileChange::Omitted { added, removed, .. } => (*added, *removed),
        };
        (a + added, r + removed)
    })
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn diff_height(changes: &[FileChangeOutput]) -> f32 {
    let lines: usize = changes
        .iter()
        .map(|file| match &file.change {
            FileChange::Add { content } | FileChange::Delete { content } => content.lines().count(),
            FileChange::Update { unified_diff, .. } => unified_diff.lines().count(),
            FileChange::Omitted { .. } => 0,
        })
        .sum();
    (lines as f32 * 20. + 40.).clamp(120., 420.)
}

fn question_details(state: &NativeToolState) -> Vec<QuestionDetail> {
    let Some(questions) = state
        .arguments
        .as_ref()
        .and_then(|args| args.get("questions"))
        .and_then(|v| v.as_array())
    else {
        return vec![];
    };
    let answers = match question_response(state) {
        Some(QuestionResponse::Answered { answers }) => answers,
        _ => vec![],
    };
    questions
        .iter()
        .map(|prompt| {
            let question = prompt
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let options = prompt
                .get("options")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("label")?.as_str())
                .map(sanitize_readable)
                .collect();
            let answer = prompt
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|id| answers.iter().find(|answer| answer.id == id));
            QuestionDetail {
                question: sanitize_readable(question),
                options,
                answer: answer
                    .map(question_answer_text)
                    .map(|s| sanitize_readable(&s)),
                note: answer
                    .and_then(|a| a.note.as_deref())
                    .map(sanitize_readable),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_search_native_projects_pending_and_restored_details() {
        use rig_core::message::{CallId, ToolFunction, ToolName};
        use zevria_foundation::{
            ToolCallOutcome, ToolResultDetail, ToolResultMetadata, WebSearchActionSummary,
            WebSearchDepth, WebSearchSourceSummary,
        };

        let args = json!({"query":"Find the version"});
        let call = ToolCall::new(
            CallId::from_wire("search"),
            ToolFunction::new(ToolName::new(WEB_SEARCH_TOOL_NAME).unwrap(), args.clone()),
        );
        let mut state = NativeToolState::new(NativeToolCallStatus::Executing, &args);
        let pending = ToolPresentation::native(&call, &state, None);
        assert_eq!(pending.status, StatusIcon::Running);
        assert_eq!(pending.summary, "Find the version");
        assert!(matches!(pending.detail, ToolDetail::WebSearch {
            activity, running_label: Some(label), completed: None,
        } if activity.is_empty() && label == "Searching the web"));

        state.status = NativeToolCallStatus::Finished;
        state.metadata = Some(ToolResultMetadata {
            id: "search".into(),
            call_id: None,
            tool_name: WEB_SEARCH_TOOL_NAME.into(),
            outcome: ToolCallOutcome::Success,
            diagnostic: None,
            detail: Some(ToolResultDetail::WebSearch(WebSearchResultDetail {
                provider: "test".into(),
                model: "search-model".into(),
                depth: WebSearchDepth::Quick,
                actions: vec![
                    WebSearchActionSummary {
                        kind: "search".into(),
                        queries: vec!["current version".into()],
                        url: None,
                        pattern: None,
                    },
                    WebSearchActionSummary {
                        kind: "open_page".into(),
                        queries: vec![],
                        url: Some("https://example.com/changelog".into()),
                        pattern: None,
                    },
                ],
                sources: vec![WebSearchSourceSummary {
                    title: "Changelog".into(),
                    url: "https://example.com/changelog".into(),
                }],
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                total_tokens: None,
            })),
        });
        let restored = ToolPresentation::native(
            &call,
            &state,
            Some("Version 2.\n\nSources:\n- <https://example.com/changelog>"),
        );
        assert_eq!(restored.status, StatusIcon::Done);
        assert_eq!(restored.body, "Version 2.");
        let ToolDetail::WebSearch {
            activity,
            running_label: None,
            completed: Some(_),
        } = restored.detail
        else {
            panic!("expected persisted search details");
        };
        assert_eq!(activity.len(), 2);
        assert_eq!(activity[0].queries, ["current version"]);
        assert_eq!(activity[0].status, StatusIcon::Done);
        assert_eq!(
            activity[1].safe_url(),
            Some("https://example.com/changelog")
        );
    }

    #[test]
    fn job_and_read_summaries_use_structured_targets() {
        assert_eq!(
            tool_summary("command_status", Some(&json!({"id":null}))),
            "All jobs"
        );
        assert_eq!(
            tool_summary("command_terminate", Some(&json!({"id":"job-123"}))),
            "job-123"
        );
        assert_eq!(
            tool_summary(
                "read",
                Some(&json!({"file_path":"src/lib.rs","start_line":10,"end_line":21}))
            ),
            "src/lib.rs:10–21"
        );
        assert_eq!(
            tool_summary("read", Some(&json!({"path":"src/lib.rs"}))),
            "src/lib.rs"
        );
        assert_eq!(tool_summary("read", Some(&json!({"path":null}))), "");
    }
}
