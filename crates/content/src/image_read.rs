//! The stable `read_image` result text and access to its embedded raster.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rig_core::message::{DocumentSourceKind, ImageMediaType, ToolResult, ToolResultContent};

use crate::prompt::Raster;

const ORIGINAL_UNCHANGED: &str = " (original file unchanged)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageReadSummary {
    pub path: String,
    pub mime: String,
    pub dimensions: (u32, u32),
    pub bytes: usize,
    pub original: Option<((u32, u32), usize)>,
}

impl fmt::Display for ImageReadSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (width, height) = self.dimensions;
        let bytes = self.bytes;
        write!(
            f,
            "image: {}\nformat: {}\ndimensions: {width}×{height}\nbytes: {bytes}",
            self.path, self.mime
        )?;
        if let Some(((ow, oh), original_bytes)) = self.original {
            write!(
                f,
                "\nprepared: {ow}×{oh}, {original_bytes} bytes → {width}×{height}, {bytes} bytes{ORIGINAL_UNCHANGED}"
            )?;
        }
        Ok(())
    }
}

impl ImageReadSummary {
    pub fn parse(text: &str) -> Option<Self> {
        // Parse the fixed fields from the end so newlines in a filename survive.
        let (summary, last) = text.rsplit_once('\n')?;
        let (summary, bytes, prepared) = if let Some(prepared) = last.strip_prefix("prepared: ") {
            let (summary, bytes) = summary.rsplit_once('\n')?;
            (summary, bytes, Some(prepared))
        } else {
            (summary, last, None)
        };
        let bytes: usize = bytes.strip_prefix("bytes: ")?.parse().ok()?;
        let mut lines = summary.rsplitn(3, '\n');
        let dimensions = parse_dimensions(lines.next()?.strip_prefix("dimensions: ")?)?;
        let mime = lines.next()?.strip_prefix("format: ")?;
        Raster::from_mime(mime).ok()?;
        let path = lines.next()?.strip_prefix("image: ")?;
        if path.is_empty() {
            return None;
        }
        let original = if let Some(prepared) = prepared {
            let (original, output) = prepared
                .strip_suffix(ORIGINAL_UNCHANGED)?
                .strip_suffix(" bytes")?
                .split_once(" bytes → ")?;
            let (original_dimensions, original_bytes) = original.split_once(", ")?;
            let (output_dimensions, output_bytes) = output.split_once(", ")?;
            if parse_dimensions(output_dimensions)? != dimensions
                || output_bytes.parse::<usize>().ok()? != bytes
            {
                return None;
            }
            Some((
                parse_dimensions(original_dimensions)?,
                original_bytes.parse().ok()?,
            ))
        } else {
            None
        };
        Some(Self {
            path: path.into(),
            mime: mime.into(),
            dimensions,
            bytes,
            original,
        })
    }
}

fn parse_dimensions(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('×')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// Borrow the first embedded raster's MIME and base64 without opening paths or URLs.
pub fn tool_result_image(result: &ToolResult) -> Option<(&'static str, &str)> {
    result.content.iter().find_map(|content| {
        let ToolResultContent::Image(image) = content else {
            return None;
        };
        let DocumentSourceKind::Base64(data) = &image.data else {
            return None;
        };
        let mime = match image.media_type {
            Some(ImageMediaType::PNG) => "image/png",
            Some(ImageMediaType::JPEG) => "image/jpeg",
            Some(ImageMediaType::WEBP) => "image/webp",
            Some(ImageMediaType::GIF) => "image/gif",
            _ => return None,
        };
        Some((mime, data.as_str()))
    })
}

/// Decode only the base64 envelope; the renderer handles the encoded raster.
pub fn decode_image_base64(data: &str) -> Option<Vec<u8>> {
    STANDARD.decode(data).ok()
}

#[cfg(test)]
mod tests {
    use rig_core::message::{CallId, ToolName};

    use super::*;
    use crate::PromptImage;

    const SUMMARY: &str =
        "image: /project/capture.png\nformat: image/png\ndimensions: 1440×900\nbytes: 312013";
    const PREPARED: &str =
        "prepared: 4000×3000, 6291456 bytes → 1440×900, 312013 bytes (original file unchanged)";

    #[test]
    fn summary_round_trips_with_and_without_preparation() {
        let mut summary = ImageReadSummary {
            path: "/project/capture.png".into(),
            mime: "image/png".into(),
            dimensions: (1440, 900),
            bytes: 312_013,
            original: None,
        };
        assert_eq!(summary.to_string(), SUMMARY);
        assert_eq!(ImageReadSummary::parse(SUMMARY), Some(summary.clone()));

        summary.original = Some(((4000, 3000), 6 * 1024 * 1024));
        let prepared = format!("{SUMMARY}\n{PREPARED}");
        assert_eq!(summary.to_string(), prepared);
        assert_eq!(ImageReadSummary::parse(&prepared), Some(summary.clone()));

        summary.path = "/project/first line\nsecond line.png".into();
        assert_eq!(ImageReadSummary::parse(&summary.to_string()), Some(summary));
    }

    #[test]
    fn summary_rejects_errors_and_malformed_fields() {
        for text in [
            "status: error\nerror: image not found".into(),
            SUMMARY.replace("1440×900", "1440x900"),
            format!("{SUMMARY}\n{PREPARED}\nextra"),
            format!("{SUMMARY}\n{}", PREPARED.replace("312013", "312014")),
            format!("{SUMMARY}\n{}", PREPARED.replace("1440×900", "1×1")),
        ] {
            assert_eq!(ImageReadSummary::parse(&text), None, "{text}");
        }
    }

    #[test]
    fn embedded_tool_result_image_round_trips_without_validating_pixels() {
        let image = PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
        let later = PromptImage::from_rgba(1, 1, &[4, 5, 6, 255]).unwrap();
        let result = ToolResult {
            call: CallId::from_wire("image-read"),
            name: ToolName::new("read_image").unwrap(),
            content: vec![
                ToolResultContent::text(SUMMARY),
                image.to_tool_result_content(),
                later.to_tool_result_content(),
            ],
            is_error: false,
        };
        let (mime, data) = tool_result_image(&result).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, image.base64());
        assert_eq!(decode_image_base64(data).unwrap(), image.encoded_bytes());
        assert_eq!(
            decode_image_base64("aGVsbG8=").as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(decode_image_base64("invalid!"), None);
    }
}
