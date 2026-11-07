//! Bounded tool-card display; raw copy payloads remain exact.
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
            "Diff exceeds the 128 KiB desktop display bound. Copy the tool output for details."
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
            format!("{} · +{added} −{removed}", file.path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
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
                let options = question.get("options").and_then(|v|v.as_array()).map(|options| options.iter().filter_map(|option| option.get("label").and_then(|v|v.as_str())).collect::<Vec<_>>().join(" · ")).unwrap_or_default();
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
