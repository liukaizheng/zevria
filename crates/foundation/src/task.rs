//! Provider-neutral task-list contract used by the Build-mode `task` tool and
//! frontend rendering.

use std::{collections::HashSet, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

/// Registered name of the Build-only task-list tool.
pub use crate::tool_names::TASK_TOOL_NAME;
/// Maximum number of items in one complete task-list snapshot.
pub const MAX_TASK_ITEMS: usize = 20;
/// Maximum length of one task step, measured in Unicode scalar values.
pub const MAX_TASK_STEP_CHARS: usize = 160;
/// Maximum length of the optional update explanation.
pub const MAX_TASK_EXPLANATION_CHARS: usize = 500;

/// Prefix for the engine-owned task snapshot appended after a context
/// checkpoint. It is model context, not a user-authored prompt.
pub const TASK_SNAPSHOT_CONTEXT_PREFIX: &str =
    "Engine-preserved current task-list snapshot. The next `task` call must replace it completely:";

/// Lifecycle state of one task-list item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::InProgress => "in progress",
            Self::Completed => "completed",
        })
    }
}

/// One concise implementation step in a task-list snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct TaskItem {
    #[schemars(length(min = 1, max = MAX_TASK_STEP_CHARS))]
    pub step: String,
    pub status: TaskStatus,
}

/// Complete replacement snapshot supplied to the `task` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskList {
    /// Optional short reason for this update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = MAX_TASK_EXPLANATION_CHARS))]
    pub explanation: Option<String>,
    #[schemars(length(min = 1, max = MAX_TASK_ITEMS))]
    pub tasks: Vec<TaskItem>,
}

impl TaskList {
    /// Decode provider arguments in either Rig's structured or JSON-string
    /// representation, then apply the same validation used by the tool.
    pub fn from_tool_arguments(arguments: &serde_json::Value) -> Result<Self, TaskValidationError> {
        let arguments = match arguments {
            serde_json::Value::String(raw) => serde_json::from_str(raw).map_err(|error| {
                TaskValidationError(format!("task arguments must be valid JSON: {error}"))
            })?,
            arguments => arguments.clone(),
        };
        serde_json::from_value::<Self>(arguments)
            .map_err(|error| TaskValidationError(format!("invalid task arguments: {error}")))?
            .validate()
    }

    /// Normalize user-visible strings and enforce the compact-list contract.
    pub fn validate(mut self) -> Result<Self, TaskValidationError> {
        if !(1..=MAX_TASK_ITEMS).contains(&self.tasks.len()) {
            return Err(TaskValidationError(format!(
                "tasks must contain 1-{MAX_TASK_ITEMS} items, got {}",
                self.tasks.len()
            )));
        }

        if let Some(explanation) = self.explanation.take() {
            let explanation = normalize_display_text(&explanation, "explanation")?;
            let characters = explanation.chars().count();
            if explanation.is_empty() {
                return Err(TaskValidationError(
                    "explanation must not be blank when provided".to_string(),
                ));
            }
            if characters > MAX_TASK_EXPLANATION_CHARS {
                return Err(TaskValidationError(format!(
                    "explanation must contain at most {MAX_TASK_EXPLANATION_CHARS} characters, got {characters}"
                )));
            }
            self.explanation = Some(explanation);
        }

        let mut seen = HashSet::new();
        let mut in_progress = 0usize;
        for (index, task) in self.tasks.iter_mut().enumerate() {
            if task.step.trim().is_empty() || task.step.lines().count() != 1 {
                return Err(TaskValidationError(format!(
                    "tasks[{index}].step must be one non-empty line"
                )));
            }
            task.step = normalize_display_text(&task.step, &format!("tasks[{index}].step"))?;
            let characters = task.step.chars().count();
            if characters > MAX_TASK_STEP_CHARS {
                return Err(TaskValidationError(format!(
                    "tasks[{index}].step must contain at most {MAX_TASK_STEP_CHARS} characters, got {characters}"
                )));
            }
            if !seen.insert(task.step.to_lowercase()) {
                return Err(TaskValidationError(format!(
                    "task step {:?} is duplicated",
                    task.step
                )));
            }
            if task.status == TaskStatus::InProgress {
                in_progress += 1;
            }
        }
        if in_progress > 1 {
            return Err(TaskValidationError(format!(
                "at most one task may be in_progress, got {in_progress}"
            )));
        }
        Ok(self)
    }

    pub fn pending_count(&self) -> usize {
        self.count(TaskStatus::Pending)
    }

    pub fn in_progress_count(&self) -> usize {
        self.count(TaskStatus::InProgress)
    }

    pub fn completed_count(&self) -> usize {
        self.count(TaskStatus::Completed)
    }

    fn count(&self, status: TaskStatus) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == status)
            .count()
    }

    /// Concise model-visible acknowledgement of the installed snapshot.
    pub fn update_summary(&self) -> String {
        format!(
            "Task list updated: {}/{} completed, {} in progress, {} pending.",
            self.completed_count(),
            self.tasks.len(),
            self.in_progress_count(),
            self.pending_count()
        )
    }

    /// Exact compact state injected after lossy context compaction. JSON keeps
    /// statuses and display strings unambiguous while remaining small.
    pub fn checkpoint_context(&self) -> String {
        let snapshot = serde_json::to_string(self)
            .expect("a validated task list contains only serializable fields");
        format!("{TASK_SNAPSHOT_CONTEXT_PREFIX}\n{snapshot}")
    }
}

/// Keep concise display text stable across rendering, persistence, and
/// duplicate detection. NFC preserves ordinary Unicode spelling, while
/// whitespace folding prevents visually identical spacing variants.
fn normalize_display_text(value: &str, field: &str) -> Result<String, TaskValidationError> {
    let value = value.trim().nfc().collect::<String>();
    if let Some(character) = value
        .chars()
        .find(|character| character.is_control() || is_unsafe_format_character(*character))
    {
        return Err(TaskValidationError(format!(
            "{field} contains unsupported control or formatting character U+{:04X}",
            u32::from(character)
        )));
    }

    let mut normalized = String::with_capacity(value.len());
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(character);
    }
    Ok(normalized)
}

/// Reject terminal controls, line/paragraph separators, bidi overrides, and
/// default-ignorable separators that can make distinct steps look identical.
/// ZWNJ/ZWJ and variation selectors remain valid for scripts and emoji.
fn is_unsafe_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskValidationError(String);

impl fmt::Display for TaskValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TaskValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(step: &str, status: TaskStatus) -> TaskItem {
        TaskItem {
            step: step.to_string(),
            status,
        }
    }

    #[test]
    fn validation_normalizes_a_valid_snapshot_and_counts_states() {
        let list = TaskList {
            explanation: Some("  Started implementation.  ".to_string()),
            tasks: vec![
                item("  Inspect the code  ", TaskStatus::Completed),
                item("Implement the tool", TaskStatus::InProgress),
                item("Run tests", TaskStatus::Pending),
            ],
        }
        .validate()
        .expect("valid task list");

        assert_eq!(list.explanation.as_deref(), Some("Started implementation."));
        assert_eq!(list.tasks[0].step, "Inspect the code");
        assert_eq!(list.completed_count(), 1);
        assert_eq!(list.in_progress_count(), 1);
        assert_eq!(list.pending_count(), 1);
        assert_eq!(
            list.update_summary(),
            "Task list updated: 1/3 completed, 1 in progress, 1 pending."
        );
    }

    #[test]
    fn validation_rejects_bad_shapes_duplicates_and_multiple_active_items() {
        let error = TaskList {
            explanation: None,
            tasks: Vec::new(),
        }
        .validate()
        .expect_err("empty lists are not useful snapshots");
        assert!(error.to_string().contains("1-20"));

        let error = TaskList {
            explanation: None,
            tasks: vec![
                item("Run tests", TaskStatus::Pending),
                item("run TESTS", TaskStatus::Completed),
            ],
        }
        .validate()
        .expect_err("duplicates must fail");
        assert!(error.to_string().contains("duplicated"));

        let error = TaskList {
            explanation: None,
            tasks: vec![
                item("First", TaskStatus::InProgress),
                item("Second", TaskStatus::InProgress),
            ],
        }
        .validate()
        .expect_err("only one item can be active");
        assert!(error.to_string().contains("at most one"));

        let error = TaskList {
            explanation: None,
            tasks: vec![item("line one\nline two", TaskStatus::Pending)],
        }
        .validate()
        .expect_err("steps render on one logical line");
        assert!(error.to_string().contains("one non-empty line"));
    }

    #[test]
    fn validation_normalizes_unicode_and_visible_whitespace_before_comparison() {
        let list = TaskList {
            explanation: Some("  Review\u{00A0}\u{00A0}progress  ".to_string()),
            tasks: vec![item(
                "  Cafe\u{301}\u{00A0}\u{2003}review  ",
                TaskStatus::InProgress,
            )],
        }
        .validate()
        .expect("ordinary Unicode and spacing should normalize");

        assert_eq!(list.explanation.as_deref(), Some("Review progress"));
        assert_eq!(list.tasks[0].step, "Café review");

        let error = TaskList {
            explanation: None,
            tasks: vec![
                item("Cafe\u{301}", TaskStatus::Pending),
                item("Café", TaskStatus::Completed),
            ],
        }
        .validate()
        .expect_err("canonically equivalent steps are duplicates");
        assert!(error.to_string().contains("duplicated"));

        let emoji = TaskList {
            explanation: None,
            tasks: vec![item("Test 👩\u{200D}💻 mode", TaskStatus::Pending)],
        }
        .validate()
        .expect("emoji joiners remain supported");
        assert_eq!(emoji.tasks[0].step, "Test 👩‍💻 mode");
    }

    #[test]
    fn validation_rejects_embedded_controls_and_unsafe_invisible_formatting() {
        for step in [
            "tab\tinside",
            "bare\rcarriage",
            "zero\u{200B}width",
            "bidi\u{202E}override",
            "line\u{2028}separator",
        ] {
            let error = TaskList {
                explanation: None,
                tasks: vec![item(step, TaskStatus::Pending)],
            }
            .validate()
            .expect_err("unsafe display characters must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("unsupported control or formatting character"),
                "unexpected error for {step:?}: {error}"
            );
        }

        let error = TaskList {
            explanation: Some("hidden\u{2060}separator".to_string()),
            tasks: vec![item("Safe step", TaskStatus::Pending)],
        }
        .validate()
        .expect_err("explanations follow the same display policy");
        assert!(
            error
                .to_string()
                .contains("explanation contains unsupported")
        );
    }

    #[test]
    fn tool_arguments_and_checkpoint_context_use_the_validated_snapshot() {
        let arguments = serde_json::json!({
            "explanation": "  Working  ",
            "tasks": [{"step": "  Run tests  ", "status": "in_progress"}]
        });
        let structured = TaskList::from_tool_arguments(&arguments).expect("structured arguments");
        let encoded =
            TaskList::from_tool_arguments(&serde_json::Value::String(arguments.to_string()))
                .expect("encoded arguments");

        assert_eq!(structured, encoded);
        assert_eq!(structured.tasks[0].step, "Run tests");
        let context = structured.checkpoint_context();
        assert!(context.starts_with(TASK_SNAPSHOT_CONTEXT_PREFIX));
        assert!(context.contains("\"status\":\"in_progress\""));
        assert!(context.contains("\"step\":\"Run tests\""));
    }
}
