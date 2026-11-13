use std::{
    fs::{self, File},
    io::Read as _,
    path::PathBuf,
};

use rig_agent::tool::{Tool, ToolContext};
use rig_core::{
    message::ToolResultContent,
    tool::{ToolExecutionError, ToolOutput},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_content::{PromptImage, prompt::MAX_IMAGE_BYTES};
use zevria_foundation::READ_IMAGE_TOOL_NAME;

use crate::FileToolError;

const DESCRIPTION: &str = r#"Read a local raster image and return it as viewable image content, not base64 text.

Usage:
- `file_path` may be an absolute path anywhere readable, or relative to the startup workspace. Symlinks are followed; the target must be a regular file.
- Formats are detected from content, not the extension: PNG, JPEG, WebP, and GIF. Animated WebP and APNG are rejected.
- Limits: 5 MiB of encoded raster bytes, at most 16,384 pixels per side, and a 40 million decoded-pixel/work budget per image. Images are never resized, cropped, transcoded, or replaced by an animation's first frame.
- Convert SVG/BMP/TIFF/HEIC/PDF or unsupported or oversized images with `command` into a temporary PNG/JPEG first; downscale if needed, then read that file.
- Each image costs substantial model context. One response may return at most 8 images or 20 MiB of image data across all tool calls; request fewer images per response if needed.
- Use `command` for text files."#;

const CONVERSION_GUIDANCE: &str =
    "convert or downscale it with a command-line tool into a temporary PNG/JPEG and read that file";

#[derive(Debug, Clone)]
pub struct ReadImageTool {
    workspace: PathBuf,
}

impl ReadImageTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn execute(&self, args: ReadImageArgs) -> Result<ToolOutput, FileToolError> {
        if args.file_path.is_empty() {
            return Err(FileToolError::invalid_arguments(
                "file_path must not be empty",
            ));
        }
        let requested = self.workspace.join(&args.file_path);
        let path = requested.canonicalize().map_err(|error| {
            FileToolError::path_resolution(format!(
                "failed to resolve {}: {error}",
                requested.display()
            ))
        })?;
        let metadata = fs::metadata(&path).map_err(|error| {
            FileToolError::io(format!("failed to inspect {}: {error}", path.display()))
        })?;
        if !metadata.is_file() {
            return Err(FileToolError::invalid_arguments(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let too_large = || {
            FileToolError::invalid_arguments(format!(
                "{}: image exceeds the 5 MiB encoded-byte limit ({MAX_IMAGE_BYTES} bytes); {CONVERSION_GUIDANCE}",
                path.display()
            ))
        };
        if metadata.len() > MAX_IMAGE_BYTES as u64 {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| {
                file.take(MAX_IMAGE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|error| {
                FileToolError::io(format!("failed to read {}: {error}", path.display()))
            })?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(too_large());
        }
        let image = PromptImage::from_file_bytes(bytes).map_err(|error| {
            FileToolError::invalid_arguments(format!(
                "{}: {error}; {CONVERSION_GUIDANCE}",
                path.display()
            ))
        })?;
        let (width, height) = image.dimensions();
        let summary = format!(
            "image: {}\nformat: {}\ndimensions: {width}×{height}\nbytes: {}",
            path.display(),
            image.mime_type(),
            image.encoded_len()
        );
        ToolOutput::content(vec![
            ToolResultContent::text(summary),
            image.to_tool_result_content(),
        ])
        .map_err(|error| FileToolError::io(error.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadImageArgs {
    /// Absolute path to any readable image file, or a path relative to the startup workspace.
    pub file_path: String,
}

impl Tool for ReadImageTool {
    const NAME: &'static str = READ_IMAGE_TOOL_NAME;

    type Error = FileToolError;
    type Args = ReadImageArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(ReadImageArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        error.classify()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let tool = self.clone();
        tokio::task::spawn_blocking(move || tool.execute(args))
            .await
            .map_err(|error| FileToolError::io(format!("image read task failed: {error}")))?
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::{DocumentSourceKind, ImageMediaType};
    use tempfile::TempDir;

    use super::*;

    fn args(file_path: impl Into<String>) -> ReadImageArgs {
        ReadImageArgs {
            file_path: file_path.into(),
        }
    }

    fn image() -> PromptImage {
        PromptImage::from_rgba(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap()
    }

    #[test]
    fn schema_is_strict_and_requires_file_path() {
        let schema = ReadImageTool::new(PathBuf::from(".")).parameters();
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert!(serde_json::from_value::<ReadImageArgs>(serde_json::json!({})).is_err());
        assert!(
            serde_json::from_value::<ReadImageArgs>(serde_json::json!({
                "file_path": "image.png", "resize": true
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn reads_relative_absolute_and_outside_paths_as_ordered_image_content() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let image = image();
        // The extension is deliberately unrelated to the magic-byte format.
        let relative = workspace.path().join("capture.bin");
        let external = outside.path().join("external.png");
        fs::write(&relative, image.encoded_bytes()).unwrap();
        fs::write(&external, image.encoded_bytes()).unwrap();
        let tool = ReadImageTool::new(workspace.path().to_path_buf());
        for (file_path, expected_path) in [
            ("capture.bin".to_string(), relative.clone()),
            (relative.to_string_lossy().into_owned(), relative),
            (external.to_string_lossy().into_owned(), external),
        ] {
            let output = tool
                .call(&mut ToolContext::new(), args(file_path))
                .await
                .unwrap();
            let [
                ToolResultContent::Text(text),
                ToolResultContent::Image(block),
            ] = output.as_content()
            else {
                panic!("expected exactly text then image");
            };
            assert_eq!(
                text.text,
                format!(
                    "image: {}\nformat: image/png\ndimensions: 2×1\nbytes: {}",
                    expected_path.canonicalize().unwrap().display(),
                    image.encoded_len()
                )
            );
            assert_eq!(block.media_type, Some(ImageMediaType::PNG));
            let DocumentSourceKind::Base64(data) = &block.data else {
                panic!("expected base64 source");
            };
            assert_eq!(data, &image.base64());
            assert_eq!(
                PromptImage::from_base64("image/png", data)
                    .unwrap()
                    .encoded_bytes(),
                fs::read(expected_path).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn rejects_missing_paths_directories_and_empty_file_path() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        let tool = ReadImageTool::new(workspace.path().to_path_buf());
        let missing = tool
            .call(&mut ToolContext::new(), args("missing.png"))
            .await
            .unwrap_err();
        assert!(matches!(missing, FileToolError::PathResolution(_)));
        assert!(missing.to_string().contains("failed to resolve"));
        for (path, expected) in [
            ("directory", "is not a regular file"),
            ("", "must not be empty"),
        ] {
            let error = tool
                .call(&mut ToolContext::new(), args(path))
                .await
                .unwrap_err();
            assert!(matches!(error, FileToolError::InvalidArguments(_)));
            assert!(error.to_string().contains(expected));
        }
    }

    #[tokio::test]
    async fn rejects_unsupported_truncated_and_empty_images_with_conversion_guidance() {
        let workspace = TempDir::new().unwrap();
        let mut truncated = image().encoded_bytes().to_vec();
        truncated.truncate(truncated.len() - 1);
        for (name, bytes) in [
            ("unsupported.bmp", b"BMunsupported BMP".to_vec()),
            ("truncated.png", truncated),
            ("empty.png", Vec::new()),
        ] {
            fs::write(workspace.path().join(name), bytes).unwrap();
            let tool = ReadImageTool::new(workspace.path().to_path_buf());
            let error = tool
                .call(&mut ToolContext::new(), args(name))
                .await
                .unwrap_err();
            assert!(matches!(error, FileToolError::InvalidArguments(_)));
            assert!(error.to_string().contains(name));
            assert!(error.to_string().contains(CONVERSION_GUIDANCE));
            if name == "unsupported.bmp" {
                assert!(error.to_string().contains("unsupported image format"));
            }
        }
    }

    #[tokio::test]
    async fn rejects_oversized_files_before_decoding() {
        let workspace = TempDir::new().unwrap();
        // Invalid image bytes would give a format error if decoding were attempted.
        File::create(workspace.path().join("oversized.png"))
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        let tool = ReadImageTool::new(workspace.path().to_path_buf());
        let error = tool
            .call(&mut ToolContext::new(), args("oversized.png"))
            .await
            .unwrap_err();
        assert!(matches!(error, FileToolError::InvalidArguments(_)));
        assert!(error.to_string().contains("5 MiB encoded-byte limit"));
        assert!(error.to_string().contains(CONVERSION_GUIDANCE));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follows_symlinks_and_summarizes_the_canonical_target() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("target.png");
        fs::write(&target, image().encoded_bytes()).unwrap();
        std::os::unix::fs::symlink(&target, workspace.path().join("link.png")).unwrap();
        let tool = ReadImageTool::new(workspace.path().to_path_buf());
        let output = tool
            .call(&mut ToolContext::new(), args("link.png"))
            .await
            .unwrap();
        assert!(
            output.as_content()[0]
                .as_text()
                .unwrap()
                .starts_with(&format!(
                    "image: {}\n",
                    target.canonicalize().unwrap().display()
                ))
        );
    }
}
