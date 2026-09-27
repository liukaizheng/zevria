use std::{fs, io::ErrorKind, path::PathBuf};

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::FileChange;
use zevria_foundation::FileChangeOperation;
use zevria_foundation::FileChangeOutput;
use zevria_foundation::ToolResultDetail;

use crate::{FileToolError, shared::resolve_path_for_write};

const DESCRIPTION: &str = r#"Delete an existing file from the local filesystem.

Usage:
- `file_path` may be an absolute path, or a path relative to the startup workspace, but must resolve inside it.
- This tool deletes files only. It rejects missing paths and directories.
- Use this tool for source file removals to record structured deletion metadata."#;

#[derive(Debug, Clone)]
pub struct DeleteTool {
    workspace: PathBuf,
}

impl DeleteTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn execute(&self, args: DeleteArgs) -> Result<(String, Vec<FileChangeOutput>), FileToolError> {
        let path = resolve_path_for_write(&self.workspace, &args.file_path)?;
        let metadata = fs::metadata(&path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                FileToolError::io(format!("file {} does not exist", path.display()))
            } else {
                FileToolError::io(format!("failed to inspect {}: {error}", path.display()))
            }
        })?;
        if metadata.is_dir() {
            return Err(FileToolError::invalid_arguments(format!(
                "{} is a directory; delete only removes files",
                path.display()
            )));
        }

        let previous_content = match fs::read_to_string(&path) {
            Ok(content) => PreviousContent::Utf8(content),
            Err(error) => PreviousContent::Unreadable(error.to_string()),
        };
        let bytes = metadata.len() as usize;
        fs::remove_file(&path).map_err(|error| {
            FileToolError::io(format!("failed to delete {}: {error}", path.display()))
        })?;

        let change = match previous_content {
            PreviousContent::Utf8(content) => FileChange::Delete { content },
            PreviousContent::Unreadable(reason) => FileChange::Omitted {
                operation: FileChangeOperation::Delete,
                reason: format!("deleted file content unavailable: {reason}"),
                added: 0,
                removed: 0,
                bytes,
            },
        };
        let model_output = format!("deleted {} ({bytes} bytes)", path.display());
        Ok((model_output, vec![FileChangeOutput { path, change }]))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// Absolute path to the file, or a path relative to the startup workspace.
    pub file_path: String,
}

impl Tool for DeleteTool {
    const NAME: &'static str = "delete";

    type Error = FileToolError;
    type Args = DeleteArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(DeleteArgs).to_value()
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
    Utf8(String),
    Unreadable(String),
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn args(file_path: impl Into<String>) -> DeleteArgs {
        DeleteArgs {
            file_path: file_path.into(),
        }
    }

    #[test]
    fn schema_is_strict_and_requires_file_path() {
        let schema = DeleteTool::new(PathBuf::from(".")).parameters();
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[tokio::test]
    async fn deletes_utf8_file_and_reports_content() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("file.txt"), "hello\nworld\n").expect("file");
        let tool = DeleteTool::new(workspace.path().to_path_buf());
        let mut context = ToolContext::new();

        let result = tool
            .call(&mut context, args("file.txt"))
            .await
            .expect("delete");
        assert!(!workspace.path().join("file.txt").exists());
        assert!(result.contains("(12 bytes)"));
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert_eq!(
            changes[0].change,
            FileChange::Delete {
                content: "hello\nworld\n".to_string()
            }
        );
    }

    #[tokio::test]
    async fn deletes_non_utf8_file_with_omitted_metadata() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("binary.dat"), [0xff, 0xfe]).expect("file");
        let tool = DeleteTool::new(workspace.path().to_path_buf());
        let mut context = ToolContext::new();

        tool.call(&mut context, args("binary.dat"))
            .await
            .expect("delete");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        let FileChange::Omitted {
            operation,
            reason,
            bytes,
            ..
        } = &changes[0].change
        else {
            panic!("expected omitted binary deletion: {:?}", changes[0].change);
        };
        assert_eq!(*operation, FileChangeOperation::Delete);
        assert_eq!(*bytes, 2);
        assert!(reason.contains("deleted file content unavailable:"));
    }

    #[tokio::test]
    async fn large_utf8_content_is_deleted_with_exact_metadata() {
        let workspace = TempDir::new().expect("workspace");
        let content = "removed line\n".repeat(512 * 1024 / 13 + 1);
        assert!(content.len() > 512 * 1024);
        fs::write(workspace.path().join("large.txt"), &content).expect("file");
        let tool = DeleteTool::new(workspace.path().to_path_buf());
        let mut context = ToolContext::new();

        tool.call(&mut context, args("large.txt"))
            .await
            .expect("delete");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert_eq!(
            changes[0].change,
            FileChange::Delete {
                content: content.clone()
            }
        );
    }

    #[tokio::test]
    async fn rejects_missing_paths_directories_and_outside_paths() {
        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        fs::create_dir(workspace.path().join("dir")).expect("directory");
        fs::write(outside.path().join("file.txt"), "outside").expect("outside file");
        let tool = DeleteTool::new(workspace.path().to_path_buf());

        for arguments in [
            args("missing.txt"),
            args("dir"),
            args(outside.path().join("file.txt").to_string_lossy()),
        ] {
            let mut context = ToolContext::new();
            assert!(tool.call(&mut context, arguments).await.is_err());
            assert!(context.result::<ToolResultDetail>().is_none());
        }
        assert!(outside.path().join("file.txt").exists());
    }
}
