//! Bounded tool-log display; raw copy payloads remain exact.
use gpui_kit::component::diff::DiffFile;
use zevria_foundation::{FileChange, FileChangeOutput};
pub fn patch(changes: &[FileChangeOutput]) -> String {
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
pub fn parse(changes: Vec<FileChangeOutput>) -> Result<Vec<DiffFile>, String> {
    let patch = patch(&changes);
    if patch.len() > 131_072 {
        return Err(
            "This diff is larger than 128 KiB, so it isn't shown. Copy the output to see it."
                .into(),
        );
    }
    DiffFile::parse(&patch).map_err(|e| e.to_string())
}
pub fn summary(changes: &[FileChangeOutput]) -> String {
    changes
        .iter()
        .map(|file| {
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
            format!("{}  +{added} −{removed}", file.path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A single literal summary, independent of display titles and tool lifecycle.
/// Malformed/partial arguments produce a quiet empty summary, never a panic or
/// an unbounded JSON preview. Copy payloads are not passed through this function.
pub fn tool_summary(name: &str, arguments: Option<&serde_json::Value>) -> String {
    let Some(args) = arguments.and_then(serde_json::Value::as_object) else {
        return String::new();
    };
    let string = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let count = |key: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
    };
    let value = match name {
        "command" => string("command").unwrap_or_default().into(),
        "edit" | "write" | "delete" => string("file_path")
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
    value
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(240)
        .collect()
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

/// Human-oriented views for known control tools, not a second tool schema or
/// dispatcher. Missing/malformed data falls back to exact generic JSON.
pub fn details(
    name: &str,
    state: &zevria_presentation::blocks::NativeToolState,
) -> Option<(String, bool)> {
    use zevria_presentation::blocks::{LaunchBatchNotice, launch_batch_notice, tool_argument};
    match name {
        "question" => {
            let questions = state.arguments.as_ref()?.get("questions")?.as_array()?;
            Some((questions.iter().map(|question| {
                let text = question.get("question").and_then(|v|v.as_str()).unwrap_or_default();
                let options = question.get("options").and_then(|v|v.as_array()).map(|options| options.iter().filter_map(|option| option.get("label").and_then(|v|v.as_str())).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                format!("{text}\n{options}")
            }).collect::<Vec<_>>().join("\n\n"), true))
        }
        "skill" | "skill_read" => {
            let skill = tool_argument(state,"skill")?;
            let resource = tool_argument(state,"resource").map(|r|format!("\nResource: {r}")).unwrap_or_default();
            Some((format!("Skill: {skill}{resource}"),true))
        }
        "create_plan" => tool_argument(state,"markdown").map(|markdown|(markdown,false)),
        "update_plan" => Some(("Updating the stored Plan working copy; no workspace file is being edited.".into(),true)),
        "read_plan" => Some(("Reading the stored Plan working copy.".into(),true)),
        "submit_plan" => Some(("Submitting the stored Plan for your review. Only an authoritative Ready snapshot enables implementation.".into(),true)),
        "launch_subtasks" => launch_batch_notice(state).map(|notice| (match notice {
            LaunchBatchNotice::Rejected { denied } => if denied { "Subtask batch denied".into() } else { "Subtask batch rejected".into() },
            LaunchBatchNotice::Ended(icon) => format!("{} Subtask batch ended", icon.glyph(0)),
            LaunchBatchNotice::Unlaunched { missing, requested } => format!("{missing} of {requested} requested subtasks were not launched"),
        },true)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn known_tool_summaries_and_malformed_arguments() {
        for (name, args, expected) in [
            (
                "command",
                json!({"command":"cargo test\necho done"}),
                "cargo test",
            ),
            ("edit", json!({"file_path":"src/lib.rs"}), "src/lib.rs"),
            ("write", json!({"path":"new.rs"}), "new.rs"),
            ("delete", json!({"file_path":"old.rs"}), "old.rs"),
            ("skill", json!({"skill":"design"}), "design"),
            (
                "skill_read",
                json!({"skill":"design", "resource":"guide.md"}),
                "design / guide.md",
            ),
            ("web_search", json!({"query":"GPUI text"}), "GPUI text"),
            (
                "question",
                json!({"questions":[{"question":"Which approach?"}]}),
                "Which approach?",
            ),
            (
                "create_plan",
                json!({"title":"Native desktop"}),
                "Native desktop",
            ),
            ("update_plan", json!({}), "Plan working copy"),
            ("read_plan", json!({}), "Plan working copy"),
            ("submit_plan", json!({}), "Plan working copy"),
            ("task", json!({"tasks":[{},{}]}), "2 items"),
            (
                "reconcile_reports",
                json!({"decisions":[{}],"disagreements":[{}]}),
                "2 items",
            ),
            (
                "launch_subtasks",
                json!({"tasks":[{},{}]}),
                "2 subtasks in batch",
            ),
            ("unknown", json!({"path":"literal"}), "literal"),
            ("command", json!({"command":123}), ""),
            ("question", json!({"questions":false}), ""),
            ("unknown", json!([]), ""),
        ] {
            assert_eq!(tool_summary(name, Some(&args)), expected, "{name}");
        }
        assert!(tool_summary("command", None).is_empty());
        assert!(tool_summary("unknown", Some(&json!({"huge":"x".repeat(1000)}))).is_empty());
        assert_eq!(diff_height(&[]), 120.);
    }
}
