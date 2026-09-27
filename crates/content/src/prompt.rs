//! Validated, ordered user input. Display projections are never transport payloads.
//!
//! Image bytes are immutable and shared between attachment occurrences. Serialized
//! values contain only the MIME type and original base64 bytes; dimensions and
//! identity are derived again on load. Errors and Debug never contain image data.

use std::{
    fmt,
    io::{Cursor, Write},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{
    AnimationDecoder as _, ImageDecoder as _, ImageEncoder as _, ImageFormat, ImageReader, Limits,
};
use rig_core::message::{DocumentSourceKind, ImageMediaType, Message, UserContent};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest as _, Sha256};

pub const MAX_PROMPT_IMAGES: usize = 8;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PROMPT_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
/// Approximate fallback, not a conservative upper bound or a model tokenizer.
pub const ESTIMATED_IMAGE_TOKENS: usize = 1_600;
const MAX_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptError(&'static str);
impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for PromptError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Raster {
    Png,
    Jpeg,
    Webp,
    Gif,
}
impl Raster {
    fn from_mime(mime: &str) -> Result<Self, PromptError> {
        match mime {
            "image/png" => Ok(Self::Png),
            "image/jpeg" => Ok(Self::Jpeg),
            "image/webp" => Ok(Self::Webp),
            "image/gif" => Ok(Self::Gif),
            _ => Err(PromptError(
                "unsupported image MIME type; use PNG, JPEG, WebP, or GIF",
            )),
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }
    fn format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Webp => ImageFormat::WebP,
            Self::Gif => ImageFormat::Gif,
        }
    }
    fn media_type(self) -> ImageMediaType {
        match self {
            Self::Png => ImageMediaType::PNG,
            Self::Jpeg => ImageMediaType::JPEG,
            Self::Webp => ImageMediaType::WEBP,
            Self::Gif => ImageMediaType::GIF,
        }
    }
}

#[derive(PartialEq, Eq)]
struct ImageData {
    bytes: Vec<u8>,
    raster: Raster,
    width: u32,
    height: u32,
    digest: [u8; 32],
}

#[derive(Clone, PartialEq, Eq)]
pub struct PromptImage(Arc<ImageData>);

impl fmt::Debug for PromptImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptImage")
            .field("mime", &self.mime_type())
            .field("dimensions", &self.dimensions())
            .field("encoded_bytes", &self.encoded_len())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageFields {
    mime_type: String,
    data: String,
}
impl Serialize for PromptImage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ImageFields {
            mime_type: self.mime_type().into(),
            data: self.base64(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for PromptImage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = ImageFields::deserialize(deserializer)?;
        Self::from_base64(&fields.mime_type, &fields.data).map_err(D::Error::custom)
    }
}

impl PromptImage {
    pub fn from_base64(mime: &str, data: &str) -> Result<Self, PromptError> {
        Raster::from_mime(mime)?;
        if data.len() > MAX_BASE64_BYTES {
            return Err(PromptError("image exceeds the 5 MiB encoded-byte limit"));
        }
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| PromptError("invalid image base64"))?;
        Self::from_encoded(mime, bytes)
    }

    pub fn from_encoded(mime: &str, bytes: Vec<u8>) -> Result<Self, PromptError> {
        let raster = Raster::from_mime(mime)?;
        if bytes.is_empty() {
            return Err(PromptError("empty image data"));
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(PromptError("image exceeds the 5 MiB encoded-byte limit"));
        }
        if image::guess_format(&bytes).ok() != Some(raster.format()) {
            return Err(PromptError("image content does not match its MIME type"));
        }
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), raster.format());
        reader.limits(decoder_limits());
        let decoder = reader.into_decoder().map_err(|_| {
            PromptError("invalid image header or decoder allocation limit exceeded")
        })?;
        let (width, height) = decoder.dimensions();
        validate_dimensions(width, height)?;
        drop(decoder);
        validate_raster(&bytes, raster, width, height)?;
        let digest = Sha256::digest(&bytes).into();
        Ok(Self(Arc::new(ImageData {
            bytes,
            raster,
            width,
            height,
            digest,
        })))
    }

    /// Validate native RGBA before any additional bitmap allocation. Never resize.
    pub fn from_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<Self, PromptError> {
        let width =
            u32::try_from(width).map_err(|_| PromptError("image width exceeds 16,384 pixels"))?;
        let height =
            u32::try_from(height).map_err(|_| PromptError("image height exceeds 16,384 pixels"))?;
        let pixels = validate_dimensions(width, height)?;
        let expected = pixels
            .checked_mul(4)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(PromptError("image buffer size overflow"))?;
        if rgba.len() != expected {
            return Err(PromptError("clipboard RGBA buffer has an invalid length"));
        }
        let mut output = BoundedOutput(Vec::new());
        image::codecs::png::PngEncoder::new(&mut output)
            .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|_| {
                PromptError("PNG encoding failed or image exceeds the 5 MiB encoded-byte limit")
            })?;
        Self::from_encoded("image/png", output.0)
    }

    pub fn mime_type(&self) -> &'static str {
        self.0.raster.mime()
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }
    pub fn encoded_len(&self) -> usize {
        self.0.bytes.len()
    }
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.0.bytes
    }
    pub fn content_identity(&self) -> &[u8; 32] {
        &self.0.digest
    }
    pub fn base64(&self) -> String {
        STANDARD.encode(&self.0.bytes)
    }
    pub fn label(&self, ordinal: usize) -> String {
        format!(
            "[image {ordinal} · {} · {}×{}]",
            self.mime_type().trim_start_matches("image/"),
            self.0.width,
            self.0.height
        )
    }
    pub fn to_user_content(&self) -> UserContent {
        UserContent::image_base64(self.base64(), Some(self.0.raster.media_type()), None)
    }
    pub fn from_user_content(content: &UserContent) -> Result<Self, PromptError> {
        let UserContent::Image(image) = content else {
            return Err(PromptError("expected an image content block"));
        };
        if image.detail.is_some() || image.additional_params.is_some() {
            return Err(PromptError(
                "image has unsupported provider-specific fields; cannot recall losslessly",
            ));
        }
        let mime = match image.media_type {
            Some(ImageMediaType::PNG) => "image/png",
            Some(ImageMediaType::JPEG) => "image/jpeg",
            Some(ImageMediaType::WEBP) => "image/webp",
            Some(ImageMediaType::GIF) => "image/gif",
            _ => return Err(PromptError("unsupported or missing image MIME type")),
        };
        match &image.data {
            DocumentSourceKind::Base64(data) => Self::from_base64(mime, data),
            _ => Err(PromptError(
                "only embedded base64 images can be edited; image URLs and paths are never loaded",
            )),
        }
    }
}

fn png_crc32(bytes: &[u8]) -> u32 {
    // Reuse the table: many tiny ancillary chunks must not multiply setup work.
    static TABLE: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(|| {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut crc = index as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
            }
            *entry = crc;
        }
        table
    });
    let mut crc = !0u32;
    for byte in bytes {
        crc = (crc >> 8) ^ TABLE[((crc ^ u32::from(*byte)) & 255) as usize];
    }
    !crc
}

fn validate_png_container(bytes: &[u8]) -> Result<(), PromptError> {
    let mut offset = 8usize;
    let mut header = false;
    let mut data = false;
    while bytes.len().saturating_sub(offset) >= 12 {
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four bytes")) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|v| v.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or(PromptError("truncated PNG chunk"))?;
        let kind = &bytes[offset + 4..offset + 8];
        let expected = u32::from_be_bytes(bytes[end - 4..end].try_into().expect("four bytes"));
        if png_crc32(&bytes[offset + 4..end - 4]) != expected {
            return Err(PromptError("invalid PNG chunk checksum"));
        }
        if !header && kind != b"IHDR" {
            return Err(PromptError("missing PNG header"));
        }
        match kind {
            b"IHDR" if !header && length == 13 => header = true,
            b"IHDR" => return Err(PromptError("invalid or duplicate PNG header")),
            b"IDAT" => data = true,
            b"IEND" if length == 0 && end == bytes.len() && data => return Ok(()),
            b"IEND" => return Err(PromptError("invalid PNG end marker")),
            _ => {}
        }
        offset = end;
    }
    Err(PromptError("missing PNG end marker"))
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    // Includes 16-bit raster output. Codec limits are enforced in addition to
    // dimension and cumulative animation-work checks, not instead of them.
    limits.max_alloc = Some(MAX_IMAGE_PIXELS * 8);
    limits
}
fn validate_dimensions(width: u32, height: u32) -> Result<u64, PromptError> {
    if width == 0 || height == 0 {
        return Err(PromptError("image dimensions must be nonzero"));
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(PromptError("image width or height exceeds 16,384 pixels"));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(PromptError("image pixel count overflow"))?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(PromptError(
            "image exceeds the 40 million decoded-pixel limit",
        ));
    }
    Ok(pixels)
}
/// Inspect GIF framing without decoding or allocating pixel buffers. This lets
/// us reserve exactly the declared frame work before advancing the codec, rather
/// than charging another frame just to discover the terminating trailer.
fn gif_frames(bytes: &[u8], width: u32, height: u32) -> Result<usize, PromptError> {
    let invalid = PromptError("invalid or truncated GIF container");
    let skip_blocks = |offset: &mut usize| -> Result<(), PromptError> {
        loop {
            let length = *bytes.get(*offset).ok_or_else(|| invalid.clone())? as usize;
            *offset += 1;
            if length == 0 {
                return Ok(());
            }
            *offset = offset
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| invalid.clone())?;
        }
    };
    if bytes.len() < 13 {
        return Err(invalid);
    }
    let palette = |packed: u8| {
        if packed & 0x80 == 0 {
            0
        } else {
            3usize << ((packed & 7) + 1)
        }
    };
    let mut offset = 13 + palette(bytes[10]);
    let mut count = 0usize;
    let work = u64::from(width) * u64::from(height) * 4;
    loop {
        match bytes.get(offset) {
            Some(0x3b) if offset + 1 == bytes.len() && count > 0 => return Ok(count),
            Some(0x21) => {
                offset += 2; // extension introducer and label
                skip_blocks(&mut offset)?;
            }
            Some(0x2c) => {
                let descriptor = bytes
                    .get(offset..offset + 10)
                    .ok_or_else(|| invalid.clone())?;
                let word = |at| u32::from(u16::from_le_bytes([descriptor[at], descriptor[at + 1]]));
                let (left, top, frame_width, frame_height) = (word(1), word(3), word(5), word(7));
                validate_dimensions(frame_width, frame_height)?;
                if left + frame_width > width || top + frame_height > height {
                    return Err(PromptError("GIF frame exceeds its canvas"));
                }
                count += 1;
                if work.saturating_mul(count as u64) > MAX_IMAGE_PIXELS {
                    return Err(PromptError(
                        "GIF frame work exceeds the 40 million decoded-pixel budget",
                    ));
                }
                offset += 10 + palette(descriptor[9]) + 1; // descriptor, palette, LZW code size
                skip_blocks(&mut offset)?;
            }
            _ => return Err(invalid),
        }
    }
}

fn validate_raster(
    bytes: &[u8],
    raster: Raster,
    width: u32,
    height: u32,
) -> Result<(), PromptError> {
    let invalid =
        |_| PromptError("invalid or truncated image, or decoder allocation limit exceeded");
    match raster {
        Raster::Png => {
            validate_png_container(bytes)?;
            let decoder =
                image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), decoder_limits())
                    .map_err(invalid)?;
            // This API has no pre-frame work admission and rejects 16-bit APNG
            // composition. Reject explicitly rather than validating a first frame.
            if decoder.is_apng().map_err(invalid)? {
                return Err(PromptError(
                    "animated PNG validation is unsupported; no first-frame substitution is performed",
                ));
            }
            if !bytes.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 174, 66, 96, 130]) {
                return Err(PromptError("missing PNG end marker"));
            }
        }
        Raster::Webp => {
            if bytes.len() < 12
                || u32::from_le_bytes(bytes[4..8].try_into().expect("RIFF length")) as u64 + 8
                    != bytes.len() as u64
            {
                return Err(PromptError("invalid WebP container length"));
            }
            let decoder =
                image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(invalid)?;
            // image's WebP animation iterator does not enforce allocation limits.
            if decoder.has_animation() {
                return Err(PromptError(
                    "animated WebP validation is unsupported; no first-frame substitution is performed",
                ));
            }
        }
        Raster::Jpeg => {
            if !bytes.ends_with(&[0xff, 0xd9]) {
                return Err(PromptError("missing JPEG end marker"));
            }
        }
        Raster::Gif => {
            // GIF's iterator does enforce allocation limits. Reserve the canvas,
            // compositing and output buffers before each frame; never collect frames.
            if bytes.last() != Some(&0x3b) {
                return Err(PromptError("missing GIF end marker"));
            }
            let mut decoder =
                image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(invalid)?;
            decoder.set_limits(decoder_limits()).map_err(invalid)?;
            let count = gif_frames(bytes, width, height)?;
            let mut frames = decoder.into_frames();
            for _ in 0..count {
                frames
                    .next()
                    .ok_or(PromptError("missing GIF frame"))?
                    .map_err(invalid)?;
            }
            return Ok(());
        }
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), raster.format());
    reader.limits(decoder_limits());
    reader.decode().map_err(invalid)?;
    Ok(())
}
struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("encoded image limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PromptBlock {
    Text(String),
    Image(PromptImage),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UserPrompt(Vec<PromptBlock>);
impl<'de> Deserialize<'de> for UserPrompt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PromptVisitor;
        impl<'de> serde::de::Visitor<'de> for PromptVisitor {
            type Value = UserPrompt;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("ordered text/image prompt blocks")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<UserPrompt, A::Error> {
                let mut blocks = Vec::new();
                let mut count = 0;
                let mut bytes = 0usize;
                while let Some(block) = sequence.next_element::<PromptBlock>()? {
                    if let PromptBlock::Image(image) = &block {
                        count += 1;
                        bytes = bytes
                            .checked_add(image.encoded_len())
                            .ok_or_else(|| A::Error::custom("image byte count overflow"))?;
                        if count > MAX_PROMPT_IMAGES || bytes > MAX_PROMPT_IMAGE_BYTES {
                            return Err(A::Error::custom(
                                "prompt exceeds eight images or the 20 MiB aggregate image limit",
                            ));
                        }
                    }
                    blocks.push(block);
                }
                Ok(UserPrompt(blocks))
            }
        }
        deserializer.deserialize_seq(PromptVisitor)
    }
}
impl From<String> for UserPrompt {
    fn from(text: String) -> Self {
        Self::from_text(text)
    }
}
impl From<&str> for UserPrompt {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}
impl UserPrompt {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self(vec![PromptBlock::Text(text.into())])
    }
    pub fn new(blocks: Vec<PromptBlock>) -> Result<Self, PromptError> {
        let prompt = Self(blocks);
        prompt.validate()?;
        Ok(prompt)
    }
    pub fn blocks(&self) -> &[PromptBlock] {
        &self.0
    }
    pub fn into_blocks(self) -> Vec<PromptBlock> {
        self.0
    }
    pub fn images(&self) -> impl Iterator<Item = &PromptImage> {
        self.0.iter().filter_map(|block| match block {
            PromptBlock::Image(image) => Some(image),
            _ => None,
        })
    }
    pub fn has_images(&self) -> bool {
        self.images().next().is_some()
    }
    pub fn is_blank(&self) -> bool {
        self.0
            .iter()
            .all(|block| matches!(block, PromptBlock::Text(text) if text.trim().is_empty()))
    }
    pub fn text_len(&self) -> usize {
        self.0
            .iter()
            .map(|b| match b {
                PromptBlock::Text(t) => t.len(),
                _ => 0,
            })
            .sum()
    }
    pub fn text_projection(&self) -> String {
        self.0
            .iter()
            .filter_map(|block| match block {
                PromptBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
    pub fn display_projection(&self) -> String {
        let mut ordinal = 0;
        self.0
            .iter()
            .map(|block| match block {
                PromptBlock::Text(text) => text.clone(),
                PromptBlock::Image(image) => {
                    ordinal += 1;
                    image.label(ordinal)
                }
            })
            .collect()
    }
    pub fn trimmed(mut self) -> Self {
        while matches!(self.0.first(), Some(PromptBlock::Text(t)) if t.trim_start().is_empty()) {
            self.0.remove(0);
        }
        if let Some(PromptBlock::Text(t)) = self.0.first_mut() {
            *t = t.trim_start().to_owned();
        }
        while matches!(self.0.last(), Some(PromptBlock::Text(t)) if t.trim_end().is_empty()) {
            self.0.pop();
        }
        if let Some(PromptBlock::Text(t)) = self.0.last_mut() {
            *t = t.trim_end().to_owned();
        }
        self
    }
    pub fn with_prefix(&self, prefix: impl Into<String>) -> Self {
        let mut blocks = self.0.clone();
        let mut prefix = prefix.into();
        if let Some(PromptBlock::Text(text)) = blocks.first_mut() {
            prefix.push_str(text);
            *text = prefix;
        } else {
            blocks.insert(0, PromptBlock::Text(prefix));
        }
        Self(blocks)
    }
    pub fn validate(&self) -> Result<(), PromptError> {
        let mut count = 0;
        let mut bytes = 0usize;
        for image in self.images() {
            count += 1;
            if count > MAX_PROMPT_IMAGES {
                return Err(PromptError("a prompt may contain at most eight images"));
            }
            bytes = bytes
                .checked_add(image.encoded_len())
                .ok_or(PromptError("image byte count overflow"))?;
            if bytes > MAX_PROMPT_IMAGE_BYTES {
                return Err(PromptError(
                    "prompt images exceed the 20 MiB aggregate encoded-byte limit",
                ));
            }
        }
        Ok(())
    }
    pub fn to_message(&self) -> Message {
        let content = self
            .0
            .iter()
            .map(|block| match block {
                PromptBlock::Text(text) => UserContent::text(text),
                PromptBlock::Image(image) => image.to_user_content(),
            })
            .collect::<Vec<_>>();
        Message::User { content }
    }
    pub fn from_message(message: &Message) -> Result<Self, PromptError> {
        let Message::User { content } = message else {
            return Err(PromptError("only user messages can be recalled as prompts"));
        };
        let blocks = content
            .iter()
            .map(|block| match block {
                UserContent::Text(text) if text.additional_params.is_none() => {
                    Ok(PromptBlock::Text(text.text.clone()))
                }
                UserContent::Image(_) => {
                    PromptImage::from_user_content(block).map(PromptBlock::Image)
                }
                _ => Err(PromptError(
                    "user message contains unsupported content; cannot recall losslessly",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(blocks)
    }
}

/// Cheap capability/accounting query; no decoding or base64 allocation.
pub fn message_has_images(message: &Message) -> bool {
    matches!(message, Message::User { content } if content.iter().any(|block| matches!(block, UserContent::Image(_))))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> PromptImage {
        PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap()
    }
    #[test]
    fn ordered_round_trip_and_duplicate_occurrences() {
        let image = image();
        let prompt = UserPrompt::new(vec![
            PromptBlock::Text(" before ".into()),
            PromptBlock::Image(image.clone()),
            PromptBlock::Text(" between ".into()),
            PromptBlock::Image(image),
        ])
        .unwrap()
        .trimmed();
        assert_eq!(
            UserPrompt::from_message(&prompt.to_message()).unwrap(),
            prompt
        );
        assert_eq!(
            serde_json::from_str::<UserPrompt>(&serde_json::to_string(&prompt).unwrap()).unwrap(),
            prompt
        );
        assert_eq!(prompt.images().count(), 2);
        assert_eq!(prompt.text_projection(), "before  between ");
        assert!(!prompt.is_blank());
    }
    #[test]
    fn metadata_only_debug_and_recall_failure() {
        let image = image();
        assert!(!format!("{image:?}").contains(&image.base64()));
        let message = Message::User {
            content: vec![UserContent::image_url(
                "https://example.com/a.png",
                Some(ImageMediaType::PNG),
                None,
            )],
        };
        assert!(UserPrompt::from_message(&message).is_err());
    }
    #[test]
    fn all_embedded_raster_formats_are_validated_without_transcoding() {
        for (mime, format) in [
            ("image/png", ImageFormat::Png),
            ("image/jpeg", ImageFormat::Jpeg),
            ("image/webp", ImageFormat::WebP),
            ("image/gif", ImageFormat::Gif),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2)
                .write_to(&mut encoded, format)
                .unwrap();
            let bytes = encoded.into_inner();
            let validated = PromptImage::from_encoded(mime, bytes.clone()).unwrap();
            assert_eq!(validated.encoded_bytes(), bytes);
            assert_eq!(validated.mime_type(), mime);
            assert_eq!(validated.dimensions(), (2, 2));
            assert!(PromptImage::from_encoded(mime, bytes[..bytes.len() / 2].to_vec()).is_err());
        }
    }

    fn padded_png(size: usize) -> PromptImage {
        let mut bytes = image().encoded_bytes().to_vec();
        let end = bytes.split_off(bytes.len() - 12);
        let length = size - bytes.len() - end.len() - 12;
        let mut chunk = Vec::with_capacity(length + 4);
        chunk.extend_from_slice(b"tEXtpadding\0");
        chunk.resize(length + 4, b'x');
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
            }
            *entry = crc;
        }
        let mut crc = !0u32;
        for byte in &chunk {
            crc = (crc >> 8) ^ table[((crc ^ u32::from(*byte)) & 255) as usize];
        }
        bytes.extend_from_slice(&(length as u32).to_be_bytes());
        bytes.extend_from_slice(&chunk);
        bytes.extend_from_slice(&(!crc).to_be_bytes());
        bytes.extend_from_slice(&end);
        assert_eq!(bytes.len(), size);
        PromptImage::from_encoded("image/png", bytes).unwrap()
    }

    #[test]
    fn gif_budget_does_not_reserve_a_phantom_terminal_frame() {
        for count in [1, 2] {
            let mut bytes = Vec::new();
            image::codecs::gif::GifEncoder::new(&mut bytes)
                .encode_frames((0..count).map(|_| image::Frame::new(image::RgbaImage::new(2, 2))))
                .unwrap();
            // Legal small subframe on a large transparent logical canvas.
            bytes[6..8].copy_from_slice(&3162u16.to_le_bytes());
            bytes[8..10].copy_from_slice(&3162u16.to_le_bytes());
            assert_eq!(
                PromptImage::from_encoded("image/gif", bytes).is_ok(),
                count == 1
            );
        }
    }

    #[test]
    fn complete_container_validation_rejects_bad_ancillary_crc_and_webp_lengths() {
        let mut png = padded_png(1024).encoded_bytes().to_vec();
        let checksum = png.len() - 13;
        png[checksum] ^= 1;
        assert!(PromptImage::from_encoded("image/png", png).is_err());
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut encoded, ImageFormat::WebP)
            .unwrap();
        let mut webp = encoded.into_inner();
        webp[4] ^= 1;
        assert!(PromptImage::from_encoded("image/webp", webp).is_err());
    }

    #[test]
    fn bad_input_and_limits() {
        assert!(PromptImage::from_base64("image/png", "invalid!").is_err());
        assert!(PromptImage::from_encoded("image/jpeg", image().encoded_bytes().to_vec()).is_err());
        assert!(PromptImage::from_rgba(0, 1, &[]).is_err());
        assert!(PromptImage::from_rgba(usize::MAX, 2, &[]).is_err());
        assert!(PromptImage::from_rgba(10_000, 10_000, &[]).is_err());
        assert!(PromptImage::from_rgba(1, 1, &[0]).is_err());
        assert!(PromptImage::from_encoded("image/png", vec![0; MAX_IMAGE_BYTES + 1]).is_err());
        let blocks = vec![PromptBlock::Image(image()); 8];
        assert!(UserPrompt::new(blocks.clone()).is_ok());
        let mut too_many = blocks;
        too_many.push(PromptBlock::Image(image()));
        assert!(UserPrompt::new(too_many).is_err());
    }
    #[test]
    fn blank_and_outer_trimming_do_not_flatten_blocks() {
        assert!(UserPrompt::from_text(" \n ").is_blank());
        assert!(
            !UserPrompt::new(vec![PromptBlock::Image(image())])
                .unwrap()
                .is_blank()
        );
        let prompt = UserPrompt::new(vec![
            PromptBlock::Text("  x ".into()),
            PromptBlock::Image(image()),
            PromptBlock::Text(" y  ".into()),
        ])
        .unwrap()
        .trimmed();
        assert_eq!(prompt.text_projection(), "x  y");
        assert_eq!(prompt.blocks().len(), 3);
    }
}
