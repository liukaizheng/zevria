use std::{
    fs,
    io::{ErrorKind, Write as _},
    path::PathBuf,
};

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::FileChangeOutput;
use zevria_foundation::ToolResultDetail;

use crate::{
    FileToolError,
    shared::{MAX_EDIT_FILE_CHANGE_BYTES, resolve_path_for_write, update_file_change},
};

const DESCRIPTION: &str = r#"Edit one existing UTF-8 text file with sed-style literal replacements.

Call shape:
{"file_path":"path/to/file.rs","replacements":[{"old_string":"exact text to find","new_string":"replacement text","replace_all":false}],"move_to":"optional/new/path.rs"}

Usage:
- `file_path` may be absolute or relative to the startup workspace, but must resolve inside it.
- The target must be an existing UTF-8 file. Use `write` to create files and `delete` to remove files.
- `replacements` are applied sequentially to the in-memory file content before any filesystem mutation.
- `old_string` is a literal string, not a regex or shell/sed script. It may span multiple lines and must not be empty.
- `new_string` is inserted exactly as provided and may be empty to delete text.
- `replace_all` defaults to false. When false, `old_string` must match exactly once; when true, every match is replaced and at least one match is required.
- Include enough surrounding text in `old_string` to make a targeted replacement unique, or set `replace_all` to true for a deliberate global replacement.
- Optional `move_to` renames the file after replacements. The target must not already exist unless it is the same path as `file_path`.
- Empty `replacements` are allowed only when `move_to` performs a real move.
- Edits that do not change content or move the file fail. Content-only edits are atomically replaced. Edit-plus-move installs staged final content at the destination and attempts to roll the source move back if that install fails; any change surviving a failed rollback is reported as partial mutation metadata."#;

#[derive(Debug, Clone)]
pub struct EditTool {
    workspace: PathBuf,
    max_file_change_bytes: usize,
}

impl EditTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            max_file_change_bytes: MAX_EDIT_FILE_CHANGE_BYTES,
        }
    }

    #[cfg(test)]
    fn with_max_file_change_bytes(mut self, max_file_change_bytes: usize) -> Self {
        self.max_file_change_bytes = max_file_change_bytes;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Absolute path to an existing UTF-8 file, or a path relative to the startup workspace.
    pub file_path: String,
    /// Ordered sed-style literal substitutions to apply before any optional move.
    #[serde(default)]
    pub replacements: Vec<ReplacementArgs>,
    /// Optional destination path for a rename/move. The target must not already exist unless it is the same path as file_path.
    pub move_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplacementArgs {
    /// Literal text to find. This is not a regex and may span multiple lines.
    pub old_string: String,
    /// Replacement text. May be empty to delete the matched text.
    pub new_string: String,
    /// Replace every match. If false or omitted, old_string must match exactly once.
    #[serde(default)]
    pub replace_all: bool,
}

impl Tool for EditTool {
    const NAME: &'static str = "edit";

    type Error = FileToolError;
    type Args = EditArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(EditArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        error.classify()
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let prepared = prepare_edit(&self.workspace, args)?;
        let changes =
            ToolResultDetail::FileChanges(vec![prepared.file_change(self.max_file_change_bytes)]);
        match prepared.apply() {
            Ok(()) => {
                context.insert_result(changes);
                Ok("applied edits (1 file changed)".to_string())
            }
            Err(error) => {
                let surviving = prepared.surviving_changes(self.max_file_change_bytes);
                if !surviving.is_empty() {
                    context.insert_result(ToolResultDetail::FileChanges(surviving));
                }
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedChange {
    path: PathBuf,
    move_to: Option<PathBuf>,
    original_content: String,
    new_content: String,
}

impl PreparedChange {
    fn apply(&self) -> Result<(), FileToolError> {
        self.apply_with_installer(|staged, target| {
            staged
                .persist(target)
                .map(|_| ())
                .map_err(|error| error.error)
        })
    }

    /// Apply the prepared change using an injectable final staged-file
    /// install. Production passes `NamedTempFile::persist`; tests can fail
    /// precisely after the source move to exercise both rollback outcomes.
    fn apply_with_installer(
        &self,
        install: impl FnOnce(tempfile::NamedTempFile, &std::path::Path) -> std::io::Result<()>,
    ) -> Result<(), FileToolError> {
        let content_changed = self.original_content != self.new_content;
        let Some(move_to) = self.move_to.as_ref().filter(|target| *target != &self.path) else {
            if content_changed {
                zevria_foundation::atomic_file::replace(&self.path, self.new_content.as_bytes())
                    .map_err(|error| {
                        FileToolError::io(format!(
                            "failed to replace {} atomically: {error:#}",
                            self.path.display()
                        ))
                    })?;
            }
            return Ok(());
        };

        if !content_changed {
            return fs::rename(&self.path, move_to).map_err(|error| {
                FileToolError::io(format!(
                    "failed to move {} to {}: {error}",
                    self.path.display(),
                    move_to.display()
                ))
            });
        }

        // Stage final contents beside the destination before moving the
        // source. The only multi-step mutation window is then two renames;
        // if the second fails, restore the first.
        let parent = move_to.parent().ok_or_else(|| {
            FileToolError::io(format!("move target {} has no parent", move_to.display()))
        })?;
        let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            FileToolError::io(format!(
                "failed to stage edited contents beside {}: {error}",
                move_to.display()
            ))
        })?;
        if let Ok(metadata) = fs::metadata(&self.path) {
            staged
                .as_file()
                .set_permissions(metadata.permissions())
                .map_err(|error| {
                    FileToolError::io(format!(
                        "failed to preserve permissions for {}: {error}",
                        self.path.display()
                    ))
                })?;
        }
        staged
            .write_all(self.new_content.as_bytes())
            .and_then(|()| staged.flush())
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|error| {
                FileToolError::io(format!(
                    "failed to write staged contents for {}: {error}",
                    move_to.display()
                ))
            })?;

        fs::rename(&self.path, move_to).map_err(|error| {
            FileToolError::io(format!(
                "failed to move {} to {}: {error}",
                self.path.display(),
                move_to.display()
            ))
        })?;
        if let Err(replacement_error) = install(staged, move_to) {
            return match fs::rename(move_to, &self.path) {
                Ok(()) => Err(FileToolError::io(format!(
                    "failed to install edited contents at {}: {replacement_error}; the move was rolled back",
                    move_to.display()
                ))),
                Err(rollback_error) => Err(FileToolError::io(format!(
                    "failed to install edited contents at {}: {replacement_error}; rollback to {} also failed: {rollback_error}",
                    move_to.display(),
                    self.path.display()
                ))),
            };
        }
        Ok(())
    }

    fn surviving_changes(&self, max_file_change_bytes: usize) -> Vec<FileChangeOutput> {
        if let Some(move_to) = &self.move_to
            && let Ok(content) = fs::read_to_string(move_to)
        {
            return vec![FileChangeOutput {
                path: self.path.clone(),
                change: update_file_change(
                    &self.original_content,
                    &content,
                    Some(move_to.clone()),
                    max_file_change_bytes,
                ),
            }];
        }
        match fs::read_to_string(&self.path) {
            Ok(content) if content != self.original_content => vec![FileChangeOutput {
                path: self.path.clone(),
                change: update_file_change(
                    &self.original_content,
                    &content,
                    None,
                    max_file_change_bytes,
                ),
            }],
            _ => Vec::new(),
        }
    }

    fn file_change(&self, max_file_change_bytes: usize) -> FileChangeOutput {
        FileChangeOutput {
            path: self.path.clone(),
            change: update_file_change(
                &self.original_content,
                &self.new_content,
                self.move_to.clone(),
                max_file_change_bytes,
            ),
        }
    }
}

fn prepare_edit(
    workspace: &std::path::Path,
    args: EditArgs,
) -> Result<PreparedChange, FileToolError> {
    validate_path_arg(&args.file_path, "file_path")?;
    if let Some(move_to) = args.move_to.as_deref() {
        validate_path_arg(move_to, "move_to")?;
    }

    let path = resolve_path_for_write(workspace, &args.file_path)?;
    validate_existing_file(&path)?;
    let original_content = read_utf8_file(&path)?;
    let mut new_content = original_content.clone();
    for (index, replacement) in args.replacements.iter().enumerate() {
        apply_replacement(&path, &mut new_content, replacement, index)?;
    }

    let move_to = args
        .move_to
        .as_deref()
        .map(|move_to| resolve_move_target(workspace, &path, move_to))
        .transpose()?
        .filter(|move_to| move_to != &path);

    if args.replacements.is_empty() && move_to.is_none() {
        return Err(FileToolError::invalid_arguments(
            "edit must include at least one replacement or move_to",
        ));
    }
    if move_to.is_none() && original_content == new_content {
        return Err(FileToolError::invalid_arguments(format!(
            "edit file {} produced no changes",
            path.display()
        )));
    }

    Ok(PreparedChange {
        path,
        move_to,
        original_content,
        new_content,
    })
}

fn validate_path_arg(path: &str, label: &str) -> Result<(), FileToolError> {
    if path.is_empty() {
        return Err(FileToolError::invalid_arguments(format!(
            "{label} must not be empty"
        )));
    }
    if path.trim() != path {
        return Err(FileToolError::invalid_arguments(format!(
            "{label} must not have leading or trailing whitespace: {path:?}"
        )));
    }
    Ok(())
}

fn validate_existing_file(path: &std::path::Path) -> Result<(), FileToolError> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            FileToolError::io(format!("file {} does not exist", path.display()))
        } else {
            FileToolError::io(format!("failed to inspect {}: {error}", path.display()))
        }
    })?;
    if metadata.is_dir() {
        return Err(FileToolError::invalid_arguments(format!(
            "{} is a directory; cannot edit",
            path.display()
        )));
    }
    Ok(())
}

fn read_utf8_file(path: &std::path::Path) -> Result<String, FileToolError> {
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == ErrorKind::InvalidData {
            FileToolError::invalid_arguments(format!(
                "edit input {} is not valid UTF-8",
                path.display()
            ))
        } else {
            FileToolError::io(format!("failed to read {}: {error}", path.display()))
        }
    })
}

fn resolve_move_target(
    workspace: &std::path::Path,
    source: &std::path::Path,
    move_to: &str,
) -> Result<PathBuf, FileToolError> {
    let target = resolve_path_for_write(workspace, move_to)?;
    if target != source {
        match fs::metadata(&target) {
            Ok(_) => {
                return Err(FileToolError::invalid_arguments(format!(
                    "move target {} already exists",
                    target.display()
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(FileToolError::io(format!(
                    "failed to inspect {}: {error}",
                    target.display()
                )));
            }
        }
    }
    Ok(target)
}

fn apply_replacement(
    path: &std::path::Path,
    content: &mut String,
    replacement: &ReplacementArgs,
    index: usize,
) -> Result<(), FileToolError> {
    if replacement.old_string.is_empty() {
        return Err(FileToolError::invalid_arguments(format!(
            "replacements[{index}].old_string must not be empty"
        )));
    }
    if replacement.old_string == replacement.new_string {
        return Err(FileToolError::invalid_arguments(format!(
            "replacements[{index}] old_string and new_string must differ"
        )));
    }

    let matches = content.matches(&replacement.old_string).count();
    if matches == 0 {
        return Err(FileToolError::invalid_arguments(format!(
            "replacement {index} old_string not found in {}",
            path.display()
        )));
    }
    if !replacement.replace_all && matches > 1 {
        return Err(FileToolError::invalid_arguments(format!(
            "replacement {index} old_string is ambiguous in {} ({matches} matches); provide more context or set replace_all to true",
            path.display()
        )));
    }

    *content = if replacement.replace_all {
        content.replace(&replacement.old_string, &replacement.new_string)
    } else {
        content.replacen(&replacement.old_string, &replacement.new_string, 1)
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zevria_foundation::FileChange;
    use zevria_foundation::FileChangeOperation;

    use super::*;

    fn replacement(
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> ReplacementArgs {
        ReplacementArgs {
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    fn args(
        file_path: impl Into<String>,
        replacements: Vec<ReplacementArgs>,
        move_to: Option<&str>,
    ) -> EditArgs {
        EditArgs {
            file_path: file_path.into(),
            replacements,
            move_to: move_to.map(str::to_string),
        }
    }

    async fn run(tool: &EditTool, args: EditArgs) -> Result<String, FileToolError> {
        tool.call(&mut ToolContext::new(), args).await
    }

    async fn run_with_context(
        tool: &EditTool,
        args: EditArgs,
    ) -> Result<(String, ToolContext), FileToolError> {
        let mut context = ToolContext::new();
        let output = tool.call(&mut context, args).await?;
        Ok((output, context))
    }

    #[test]
    fn schema_is_strict_and_preserves_optional_defaults() {
        let schema = EditTool::new(PathBuf::from(".")).parameters();
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["$defs"]["ReplacementArgs"]["additionalProperties"],
            false
        );
        let decoded: EditArgs = serde_json::from_value(serde_json::json!({
            "file_path": "file.txt",
            "replacements": [{"old_string": "a", "new_string": "b"}]
        }))
        .expect("arguments");
        assert!(!decoded.replacements[0].replace_all);
        assert_eq!(decoded.move_to, None);
    }

    #[tokio::test]
    async fn applies_sequential_multiline_and_global_replacements() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "alpha\nbeta beta\ngamma").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        run(
            &tool,
            args(
                "file.txt",
                vec![
                    replacement("alpha\nbeta", "first\nbeta"),
                    ReplacementArgs {
                        old_string: "beta".to_string(),
                        new_string: "B".to_string(),
                        replace_all: true,
                    },
                    replacement("gamma", ""),
                ],
                None,
            ),
        )
        .await
        .expect("edit");

        assert_eq!(fs::read_to_string(path).expect("contents"), "first\nB B\n");
    }

    #[tokio::test]
    async fn rejects_missing_ambiguous_empty_and_noop_replacements_without_mutation() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        let tool = EditTool::new(workspace.path().to_path_buf());

        for replacement in [
            replacement("missing", "new"),
            replacement("same", "changed"),
            replacement("", "new"),
            replacement("same", "same"),
        ] {
            fs::write(&path, "same\nsame\n").expect("reset");
            assert!(
                run(&tool, args("file.txt", vec![replacement], None))
                    .await
                    .is_err()
            );
            assert_eq!(
                fs::read_to_string(&path).expect("unchanged"),
                "same\nsame\n"
            );
        }
    }

    #[tokio::test]
    async fn preflights_all_replacements_before_writing() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "old\nactual\n").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        assert!(
            run(
                &tool,
                args(
                    "file.txt",
                    vec![replacement("old", "new"), replacement("missing", "changed")],
                    None,
                )
            )
            .await
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(path).expect("unchanged"),
            "old\nactual\n"
        );
    }

    #[tokio::test]
    async fn preserves_line_endings_and_missing_final_newline() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "alpha\r\nbeta\ngamma").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        run(
            &tool,
            args(
                "file.txt",
                vec![replacement("beta", "bee"), replacement("gamma", "GAMMA")],
                None,
            ),
        )
        .await
        .expect("edit");

        assert_eq!(
            fs::read_to_string(path).expect("contents"),
            "alpha\r\nbee\nGAMMA"
        );
    }

    #[tokio::test]
    async fn rejects_net_noop_after_sequential_replacements() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "alpha\n").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        let error = run(
            &tool,
            args(
                "file.txt",
                vec![replacement("alpha", "beta"), replacement("beta", "alpha")],
                None,
            ),
        )
        .await
        .expect_err("net no-op should fail");
        assert!(error.to_string().contains("produced no changes"));
        assert_eq!(fs::read_to_string(path).expect("unchanged"), "alpha\n");
    }

    #[tokio::test]
    async fn accepts_same_path_move_when_content_changes() {
        let workspace = TempDir::new().expect("workspace");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "before\n").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        let (_result, context) = run_with_context(
            &tool,
            args(
                "file.txt",
                vec![replacement("before", "after")],
                Some("file.txt"),
            ),
        )
        .await
        .expect("edit");
        assert_eq!(fs::read_to_string(path).expect("contents"), "after\n");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert!(matches!(
            changes[0].change,
            FileChange::Update {
                move_path: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn supports_edit_plus_move_and_rename_only() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("old.txt"), "before\n").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf());

        let (_result, context) = run_with_context(
            &tool,
            args(
                "old.txt",
                vec![replacement("before", "after")],
                Some("new.txt"),
            ),
        )
        .await
        .expect("edit and move");
        assert_eq!(
            fs::read_to_string(workspace.path().join("new.txt")).expect("moved"),
            "after\n"
        );
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        match &changes[0].change {
            FileChange::Update {
                unified_diff,
                move_path,
            } => {
                assert!(unified_diff.contains("+after"));
                assert!(
                    move_path
                        .as_ref()
                        .is_some_and(|path| path.ends_with("new.txt"))
                );
            }
            other => panic!("expected update, got {other:?}"),
        }

        let (_renamed, context) =
            run_with_context(&tool, args("new.txt", Vec::new(), Some("renamed.txt")))
                .await
                .expect("rename");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert!(matches!(
            &changes[0].change,
            FileChange::Update {
                unified_diff,
                move_path: Some(_)
            } if unified_diff.is_empty()
        ));
    }

    #[test]
    fn failed_edited_move_install_rolls_the_source_move_back() {
        let workspace = TempDir::new().expect("workspace");
        let source = workspace.path().join("old.txt");
        let destination = workspace.path().join("new.txt");
        fs::write(&source, "before\n").expect("source");
        let prepared = prepare_edit(
            workspace.path(),
            args(
                "old.txt",
                vec![replacement("before", "after")],
                Some("new.txt"),
            ),
        )
        .expect("prepared edit");

        let error = prepared
            .apply_with_installer(|_staged, _target| {
                Err(std::io::Error::other("injected install failure"))
            })
            .expect_err("final install should fail");
        assert!(error.to_string().contains("move was rolled back"));
        assert_eq!(
            fs::read_to_string(source).expect("restored source"),
            "before\n"
        );
        assert!(!destination.exists());
        assert!(
            prepared
                .surviving_changes(MAX_EDIT_FILE_CHANGE_BYTES)
                .is_empty()
        );
    }

    #[test]
    fn failed_edited_move_rollback_reports_the_surviving_partial_move() {
        let workspace = TempDir::new().expect("workspace");
        let source = workspace.path().join("old.txt");
        let destination = workspace.path().join("new.txt");
        fs::write(&source, "before\n").expect("source");
        let prepared = prepare_edit(
            workspace.path(),
            args(
                "old.txt",
                vec![replacement("before", "after")],
                Some("new.txt"),
            ),
        )
        .expect("prepared edit");
        let blocked_source = source.clone();

        let error = prepared
            .apply_with_installer(move |_staged, _target| {
                // The source path is vacant after the first rename. Occupy it
                // with a directory so restoring the destination must fail.
                fs::create_dir(&blocked_source).expect("block rollback target");
                Err(std::io::Error::other("injected install failure"))
            })
            .expect_err("install and rollback should fail");
        assert!(error.to_string().contains("rollback"));
        assert!(source.is_dir());
        assert_eq!(
            fs::read_to_string(&destination).expect("surviving moved file"),
            "before\n"
        );
        let surviving = prepared.surviving_changes(MAX_EDIT_FILE_CHANGE_BYTES);
        assert!(matches!(
            surviving.as_slice(),
            [FileChangeOutput {
                change: FileChange::Update {
                    move_path: Some(path),
                    ..
                },
                ..
            }] if path.ends_with("new.txt")
        ));
    }

    #[tokio::test]
    async fn rejects_existing_destination_binary_directory_and_noop_move() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("source.txt"), "source\n").expect("source");
        fs::write(workspace.path().join("target.txt"), "target\n").expect("target");
        fs::write(workspace.path().join("binary.dat"), [0xff, 0xfe]).expect("binary");
        fs::create_dir(workspace.path().join("dir")).expect("directory");
        let tool = EditTool::new(workspace.path().to_path_buf());

        for arguments in [
            args("source.txt", Vec::new(), Some("target.txt")),
            args("source.txt", Vec::new(), Some("source.txt")),
            args("binary.dat", vec![replacement("a", "b")], None),
            args("dir", vec![replacement("a", "b")], None),
        ] {
            let mut context = ToolContext::new();
            assert!(tool.call(&mut context, arguments).await.is_err());
            assert!(context.result::<ToolResultDetail>().is_none());
        }
        assert_eq!(
            fs::read_to_string(workspace.path().join("source.txt")).expect("source"),
            "source\n"
        );
    }

    #[tokio::test]
    async fn large_diff_is_applied_with_omitted_metadata() {
        let workspace = TempDir::new().expect("workspace");
        fs::write(workspace.path().join("file.txt"), "small\n").expect("file");
        let tool = EditTool::new(workspace.path().to_path_buf()).with_max_file_change_bytes(8);

        let (_result, context) = run_with_context(
            &tool,
            args("file.txt", vec![replacement("small", "x".repeat(32))], None),
        )
        .await
        .expect("edit");
        let changes = context
            .result::<ToolResultDetail>()
            .expect("changes")
            .file_changes();
        assert!(matches!(
            changes[0].change,
            FileChange::Omitted {
                operation: FileChangeOperation::Update,
                added: 1,
                removed: 1,
                ..
            }
        ));
    }
}
