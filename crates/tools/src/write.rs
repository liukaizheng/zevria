use std::{fs, io::ErrorKind, path::PathBuf};

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOutput;
use zevria_foundation::ToolResultDetail;

use crate::{
    FileToolError,
    shared::{full_file_update_patch, resolve_path_for_write},
};

const DESCRIPTION: &str = r#"Write a UTF-8 text file to the local filesystem.

Usage:
- `file_path` may be an absolute path, or a path relative to the startup workspace, but must resolve inside it.
- If a file already exists at that path, it is overwritten.
- The parent directory must already exist; this tool does not create missing directories.
- Prefer using `edit` to modify existing files, `delete` to remove files, and `write` for new files or intentional full rewrites."#;

#[derive(Debug, Clone)]
pub struct WriteTool {
    workspace: PathBuf,
}

impl WriteTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn execute(&self, args: WriteArgs) -> Result<(String, Vec<FileChangeOutput>), FileToolError> {
        let path = resolve_path_for_write(&self.workspace, &args.file_path)?;
        let previous_content = match fs::metadata(&path) {
            Ok(_) => match fs::read_to_string(&path) {
                Ok(content) => PreviousContent::Utf8(content),
                Err(error) => PreviousContent::Unreadable(error.to_string()),
            },
            Err(error) if error.kind() == ErrorKind::NotFound => PreviousContent::Missing,
            Err(error) => PreviousContent::Unreadable(error.to_string()),
        };
        let bytes = args.content.len();
        zevria_foundation::atomic_file::replace(&path, args.content.as_bytes()).map_err(
            |error| {
                FileToolError::io(format!(
                    "failed to write {} atomically: {error:#}",
                    path.display()
                ))
            },
        )?;

        let (change, previous_content_caveat) = match previous_content {
            PreviousContent::Utf8(previous_content) => (
                FileChange::Update {
                    unified_diff: full_file_update_patch(&previous_content, &args.content),
                    move_path: None,
                },
                None,
            ),
            PreviousContent::Missing => (
                FileChange::Add {
                    content: args.content,
                },
                None,
            ),
            PreviousContent::Unreadable(reason) => (
                FileChange::Update {
                    unified_diff: full_file_update_patch("", &args.content),
                    move_path: None,
                },
                Some(format!("previous file content unavailable: {reason}")),
            ),
        };
        let mut model_output = format!("wrote {bytes} bytes to {}", path.display());
        if let Some(caveat) = previous_content_caveat {
            model_output.push('\n');
            model_output.push_str(&caveat);
        }
        Ok((model_output, vec![FileChangeOutput { path, change }]))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Absolute path to the file, or a path relative to the startup workspace.
    pub file_path: String,
    /// UTF-8 content to write to the file. Existing files at this path are overwritten.
    pub content: String,
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = FileToolError;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(WriteArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        error.classify()
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let (output, changes) = self.execute(args)?;
        context.insert_result(ToolResultDetail::FileChanges(changes));
        Ok(output)
    }
}

enum PreviousContent {
    Missing,
    Utf8(String),
    Unreadable(String),
}

#[cfg(test)]
mod tests {
    use rig_agent::tool::server::ToolServer;
    use rig_core::tool::ToolErrorKind;
    use tempfile::TempDir;

    use super::*;

    fn args(file_path: impl Into<String>, content: impl Into<String>) -> WriteArgs {
        WriteArgs {
            file_path: file_path.into(),
            content: content.into(),
        }
    }

    async fn run_with_context(
        tool: &WriteTool,
        args: WriteArgs,
    ) -> Result<(String, ToolContext), FileToolError> {
        let mut context = ToolContext::new();
        let output = tool.call(&mut context, args).await?;
        Ok((output, context))
    }

    #[test]
    fn schema_is_strict_and_requires_both_fields() {
        let schema = WriteTool::new(PathBuf::from(".")).parameters();
        assert_eq!(
            schema["required"],
            serde_json::json!(["file_path", "content"])
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    #[tokio::test]
    async fn creates_and_overwrites_files_with_structured_changes() {
        let workspace = TempDir::new().expect("workspace");
        let tool = WriteTool::new(workspace.path().to_path_buf());

        let (_created, context) = run_with_context(&tool, args("file.txt", "before\n"))
            .await
            .expect("create");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("file changes")
            .file_changes();
        assert!(matches!(changes[0].change, FileChange::Add { .. }));

        let (_updated, context) = run_with_context(&tool, args("file.txt", "after\n"))
            .await
            .expect("overwrite");
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt")).expect("contents"),
            "after\n"
        );
        let changes = context
            .result::<ToolResultDetail>()
            .expect("file changes")
            .file_changes();
        match &changes[0].change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-before"));
                assert!(unified_diff.contains("+after"));
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn writes_empty_content_to_an_absolute_path_inside_workspace() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("empty.txt");
        let tool = WriteTool::new(workspace.path().to_path_buf());

        let (result, context) = run_with_context(&tool, args(path.to_string_lossy(), ""))
            .await
            .expect("write");
        assert_eq!(fs::read_to_string(&path).expect("contents"), "");
        assert!(result.contains("wrote 0 bytes"));
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert_eq!(
            changes[0].change,
            FileChange::Add {
                content: String::new()
            }
        );
    }

    #[tokio::test]
    async fn byte_identical_overwrite_produces_an_empty_update() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("same.txt"), "unchanged\n").expect("file");
        let tool = WriteTool::new(workspace.path().to_path_buf());

        let (_result, context) = run_with_context(&tool, args("same.txt", "unchanged\n"))
            .await
            .expect("write");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert!(matches!(
            &changes[0].change,
            FileChange::Update {
                unified_diff,
                move_path: None,
            } if unified_diff.is_empty()
        ));
    }

    #[tokio::test]
    async fn large_overwrite_captures_one_full_context_update() {
        let workspace = TempDir::new().expect("workspace");
        let original = (0..24_000)
            .map(|index| format!("line {index:05}: unchanged payload\n"))
            .collect::<String>();
        assert!(original.len() > 512 * 1024);
        let changed_line = "line 12000: unchanged payload\n";
        let replacement = "line 12000: replacement payload\n";
        let updated = original.replacen(changed_line, replacement, 1);
        fs::write(workspace.path().join("large.txt"), &original).expect("file");
        let tool = WriteTool::new(workspace.path().to_path_buf());

        let (_result, context) = run_with_context(&tool, args("large.txt", &updated))
            .await
            .expect("write");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        let FileChange::Update { unified_diff, .. } = &changes[0].change else {
            panic!("expected a complete update: {:?}", changes[0].change);
        };
        let patch = diffy::Patch::from_str(unified_diff).expect("valid patch");
        assert_eq!(patch.hunks().len(), 1, "full context should form one hunk");
        for expected in [
            "line 00000: unchanged payload",
            "line 06000: unchanged payload",
            "line 18000: unchanged payload",
            "line 23999: unchanged payload",
            "-line 12000: unchanged payload",
            "+line 12000: replacement payload",
        ] {
            assert!(
                unified_diff.contains(expected),
                "full update missing {expected:?}"
            );
        }
    }

    #[tokio::test]
    async fn overwrites_non_utf8_with_complete_additions_only_update_and_caveat() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("binary.dat"), [0xff, 0xfe]).expect("binary");
        let tool = WriteTool::new(workspace.path().to_path_buf());
        let replacement = "first\nsecond\nthird\n";

        let (result, context) = run_with_context(&tool, args("binary.dat", replacement))
            .await
            .expect("write");
        assert!(result.contains("previous file content unavailable:"));
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        let FileChange::Update { unified_diff, .. } = &changes[0].change else {
            panic!("expected additions-only update: {:?}", changes[0].change);
        };
        let patch = diffy::Patch::from_str(unified_diff).expect("valid patch");
        let lines = patch
            .hunks()
            .iter()
            .flat_map(diffy::Hunk::lines)
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), replacement.lines().count());
        assert!(
            lines
                .iter()
                .all(|line| matches!(line, diffy::Line::Insert(_)))
        );
        assert!(unified_diff.contains("+third"));
    }

    #[tokio::test]
    async fn large_new_file_preserves_exact_add_metadata() {
        let workspace = TempDir::new().expect("workspace");
        let tool = WriteTool::new(workspace.path().to_path_buf());
        let content = "x\n".repeat(512 * 1024 / 2 + 1);
        assert!(content.len() > 512 * 1024);

        let (_result, context) = run_with_context(&tool, args("large.txt", &content))
            .await
            .expect("write");
        assert_eq!(
            fs::read_to_string(workspace.path().join("large.txt")).expect("contents"),
            content
        );
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert_eq!(
            changes[0].change,
            FileChange::Add {
                content: content.clone()
            }
        );
    }

    #[tokio::test]
    async fn rejects_missing_parent_and_outside_path_as_invalid_args() {
        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        let tools = ToolServer::new()
            .tool(WriteTool::new(workspace.path().to_path_buf()))
            .run();

        for arguments in [
            serde_json::json!({"file_path": "missing/file.txt", "content": "x"}),
            serde_json::json!({
                "file_path": outside.path().join("file.txt"),
                "content": "x"
            }),
        ] {
            let mut context = ToolContext::new();
            let result = tools
                .execute("write", &arguments.to_string(), &mut context)
                .await;
            assert!(result.is_error_kind(ToolErrorKind::InvalidArgs));
            assert!(context.result::<ToolResultDetail>().is_none());
        }
    }
}
