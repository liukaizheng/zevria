//! Explicit preparation of **new** embedded images. Never called by serde,
//! replay, accounting, or provider projection. The legacy lossless constructors
//! intentionally retain their historical limits.
#[cfg(test)]
#[path = "image_preparation_tests.rs"]
mod tests;

use std::io::{Cursor, Write};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, GenericImageView as _, ImageDecoder as _, ImageEncoder as _, ImageReader,
    imageops::FilterType, metadata::Orientation,
};

use crate::{
    PromptBlock, PromptError, PromptImage, UserPrompt,
    prompt::{
        MAX_IMAGE_PIXELS, Raster, decoder_limits, gif_frames, raster_dimensions,
        validate_container, validate_dimensions,
    },
};

/// Per-image transport policy, not a whole-request limit.
pub const MAX_IMAGE_TRANSPORT_BYTES: usize = 5 * 1024 * 1024;
pub const DATA_URL_RESERVE: usize = 64;
pub const MAX_PREPARED_IMAGE_BYTES: usize =
    3 * ((MAX_IMAGE_TRANSPORT_BYTES - DATA_URL_RESERVE) / 4);
pub const MAX_IMAGE_SOURCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGE_SOURCE_BASE64_BYTES: usize = MAX_IMAGE_SOURCE_BYTES.div_ceil(3) * 4;
const MAX_PROFILE_BYTES: usize = 1024 * 1024;
const MAX_ATTEMPTS: usize = 12;
const MAX_PIXEL_WORK: u64 = MAX_IMAGE_PIXELS * 16;
const MAX_BUFFER_BYTES: u64 = 512 * 1024 * 1024;
// Codec state, row/filter buffers, bounded metadata and output validation slack.
const CODEC_RESERVE: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePreparation {
    pub original_dimensions: (u32, u32),
    /// None for native RGBA, which has no original encoded representation.
    pub original_bytes: Option<usize>,
    pub output_dimensions: (u32, u32),
    pub output_bytes: usize,
    pub output_mime: &'static str,
}

#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub image: PromptImage,
    pub preparation: Option<ImagePreparation>,
}

#[derive(Clone, Copy)]
struct Budget {
    bytes: usize,
    attempts: usize,
    pixel_work: u64,
    buffers: u64,
}
const BUDGET: Budget = Budget {
    bytes: MAX_PREPARED_IMAGE_BYTES,
    attempts: MAX_ATTEMPTS,
    pixel_work: MAX_PIXEL_WORK,
    buffers: MAX_BUFFER_BYTES,
};

pub fn fits_transport(bytes: usize) -> bool {
    bytes <= MAX_PREPARED_IMAGE_BYTES
        && bytes
            .checked_add(2)
            .map(|n| n / 3)
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(DATA_URL_RESERVE))
            .is_some_and(|n| n <= MAX_IMAGE_TRANSPORT_BYTES)
}

pub fn prepare_base64(mime: &str, data: &str) -> Result<PreparedImage, PromptError> {
    Raster::from_mime(mime)?;
    if data.len() > MAX_IMAGE_SOURCE_BASE64_BYTES {
        return Err(PromptError("image exceeds the 32 MiB source/base64 limit"));
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| PromptError("invalid image base64"))?;
    prepare_encoded(mime, bytes)
}

pub fn prepare_file_bytes(bytes: Vec<u8>) -> Result<PreparedImage, PromptError> {
    check_source_len(bytes.len())?;
    let mime = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::WebP) => "image/webp",
        Ok(image::ImageFormat::Gif) => "image/gif",
        _ => {
            return Err(PromptError(
                "unsupported image format; use PNG, JPEG, WebP, or GIF",
            ));
        }
    };
    prepare_encoded(mime, bytes)
}

pub fn prepare_encoded(mime: &str, bytes: Vec<u8>) -> Result<PreparedImage, PromptError> {
    prepare_encoded_with_budget(mime, bytes, BUDGET)
}

fn check_source_len(len: usize) -> Result<(), PromptError> {
    if len == 0 {
        return Err(PromptError("empty image data"));
    }
    if len > MAX_IMAGE_SOURCE_BYTES {
        return Err(PromptError("image exceeds the 32 MiB encoded source limit"));
    }
    Ok(())
}

fn prepare_encoded_with_budget(
    mime: &str,
    bytes: Vec<u8>,
    budget: Budget,
) -> Result<PreparedImage, PromptError> {
    let raster = Raster::from_mime(mime)?;
    check_source_len(bytes.len())?;
    if bytes.len() <= budget.bytes && fits_transport(bytes.len()) {
        return Ok(PreparedImage {
            image: PromptImage::from_encoded(mime, bytes)?,
            preparation: None,
        });
    }
    let dimensions = raster_dimensions(&bytes, raster)?;
    validate_container(&bytes, raster, dimensions.0, dimensions.1)?;
    if raster == Raster::Gif && gif_frames(&bytes, dimensions.0, dimensions.1)? > 1 {
        return Err(PromptError(
            "oversized animated GIF cannot be prepared without losing animation; manually convert to a static PNG/JPEG or a smaller GIF",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), raster.format());
    reader.limits(decoder_limits());
    let mut decoder = reader.into_decoder().map_err(|_| decode_error())?;
    // Bound metadata and all major pixel buffers, including decoder output and
    // its allocation allowance. Never retain a second original in history.
    let source_bytes = bytes.len();
    // Include caller-held encoded/base64 input as well as this decoded source.
    let held = (source_bytes as u64) * 3 + CODEC_RESERVE;
    ensure_buffers(held + decoder.total_bytes() + MAX_IMAGE_PIXELS * 8, budget)?;
    let profile = decoder
        .icc_profile()
        .map_err(|_| decode_error())?
        .unwrap_or_default();
    if profile.len() > MAX_PROFILE_BYTES {
        return Err(PromptError(
            "image color profile exceeds the 1 MiB preparation limit; manually convert the image",
        ));
    }
    let orientation = decoder.orientation().map_err(|_| decode_error())?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(|_| decode_error())?;
    let output_raster = if raster == Raster::Gif {
        Raster::Png
    } else {
        raster
    };
    let result = prepare_pixels(decoded, output_raster, &profile, orientation, held, budget)?;
    Ok(changed(result, dimensions, Some(source_bytes)))
}

/// Native clipboard preparation. The existing caller owns the RGBA buffer;
/// validate it before allocating the working bitmap. No source file is written.
pub fn prepare_rgba(
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<PreparedImage, PromptError> {
    prepare_rgba_with_budget(width, height, rgba, BUDGET)
}

fn prepare_rgba_with_budget(
    width: usize,
    height: usize,
    rgba: &[u8],
    budget: Budget,
) -> Result<PreparedImage, PromptError> {
    let width =
        u32::try_from(width).map_err(|_| PromptError("image width exceeds 16,384 pixels"))?;
    let height =
        u32::try_from(height).map_err(|_| PromptError("image height exceeds 16,384 pixels"))?;
    let pixels = validate_dimensions(width, height)?;
    if pixels.checked_mul(4) != Some(rgba.len() as u64) {
        return Err(PromptError("clipboard RGBA buffer has an invalid length"));
    }
    let held = rgba.len() as u64 + CODEC_RESERVE;
    ensure_buffers(held + rgba.len() as u64, budget)?;
    let decoded = DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(width, height, rgba.to_vec()).expect("validated RGBA length"),
    );
    let image = prepare_pixels(
        decoded,
        Raster::Png,
        &[],
        Orientation::NoTransforms,
        held,
        budget,
    )?;
    if image.dimensions() == (width, height) {
        Ok(PreparedImage {
            image,
            preparation: None,
        })
    } else {
        Ok(changed(image, (width, height), None))
    }
}

fn changed(
    image: PromptImage,
    original_dimensions: (u32, u32),
    original_bytes: Option<usize>,
) -> PreparedImage {
    let preparation = Some(ImagePreparation {
        original_dimensions,
        original_bytes,
        output_dimensions: image.dimensions(),
        output_bytes: image.encoded_len(),
        output_mime: image.mime_type(),
    });
    PreparedImage { image, preparation }
}

pub fn prompt_needs_preparation(prompt: &UserPrompt) -> bool {
    prompt
        .images()
        .any(|image| !fits_transport(image.encoded_len()))
}

/// New submissions only, including deliberate edits/recall followed by submit.
/// Ordered occurrences and duplicates are retained; aggregate limits are checked
/// after individual preparation, never by silently dropping or shrinking a prompt.
pub fn prepare_prompt(prompt: UserPrompt) -> Result<UserPrompt, PromptError> {
    if !prompt_needs_preparation(&prompt) {
        prompt.validate()?;
        return Ok(prompt);
    }
    let mut prepared = Vec::with_capacity(prompt.blocks().len());
    for block in prompt.into_blocks() {
        prepared.push(match block {
            PromptBlock::Image(image) if !fits_transport(image.encoded_len()) => {
                PromptBlock::Image(
                    prepare_encoded(image.mime_type(), image.encoded_bytes().to_vec())?.image,
                )
            }
            other => other,
        });
    }
    UserPrompt::new(prepared)
}

fn decode_error() -> PromptError {
    PromptError("invalid or truncated image, or decoder allocation limit exceeded")
}
fn ensure_buffers(bytes: u64, budget: Budget) -> Result<(), PromptError> {
    if bytes > budget.buffers {
        Err(PromptError(
            "image exceeds the 512 MiB preparation-buffer budget; manually convert or downscale the image",
        ))
    } else {
        Ok(())
    }
}
fn charge(work: &mut u64, pixels: u64, budget: Budget) -> Result<(), PromptError> {
    *work = work.saturating_add(pixels);
    if *work > budget.pixel_work {
        Err(PromptError(
            "image preparation pixel-work budget exhausted; manually convert or downscale the image",
        ))
    } else {
        Ok(())
    }
}

// Includes the pinned resampler's width × candidate-height × RGBA32F
// intermediate, destination, optional orientation copy, PNG's 16-bit reorder,
// and WebP's internally buffered lossless bitstream (which precedes Write).
fn candidate_buffers(
    original: &DynamicImage,
    width: u32,
    height: u32,
    raster: Raster,
    orientation: Orientation,
) -> u64 {
    let same = original.dimensions() == (width, height);
    let pixels = u64::from(width) * u64::from(height);
    let bpp = u64::from(original.color().bytes_per_pixel());
    let resampling = if same {
        0
    } else {
        let destination = if original.color().has_alpha() {
            16 + bpp
        } else {
            bpp
        };
        u64::from(original.width()) * u64::from(height) * 16 + pixels * destination
    };
    let rotated = if orientation == Orientation::NoTransforms {
        0
    } else {
        pixels * bpp * 2
    };
    let encoder = match raster {
        Raster::Webp => pixels * 32,
        Raster::Png => pixels * bpp * 2,
        Raster::Jpeg => pixels * 8,
        Raster::Gif => unreachable!(),
    };
    original.as_bytes().len() as u64 + resampling + rotated + encoder
}

fn prepare_pixels(
    original: DynamicImage,
    raster: Raster,
    profile: &[u8],
    orientation: Orientation,
    held: u64,
    budget: Budget,
) -> Result<PromptImage, PromptError> {
    let (ow, oh) = original.dimensions();
    let source_pixels = u64::from(ow) * u64::from(oh);
    let mut work = 0;
    charge(&mut work, source_pixels, budget)?; // original decode / RGBA copy
    let (mut width, mut height) = (ow, oh);
    let mut attempts = 0;
    loop {
        let same = (width, height) == (ow, oh);
        if held + candidate_buffers(&original, width, height, raster, orientation) > budget.buffers
        {
            if (width, height) == (1, 1) {
                return Err(PromptError(
                    "image cannot fit the preparation-buffer budget; manually downscale the image",
                ));
            }
            (width, height) = smaller_dimensions(ow, oh, width, height, 0.75);
            continue;
        }
        if attempts >= budget.attempts {
            return Err(PromptError(
                "image preparation encoding-attempt budget exhausted; manually convert or downscale the image",
            ));
        }
        attempts += 1;
        charge(&mut work, u64::from(width) * u64::from(height), budget)?;
        let candidate;
        let pixels = if same && orientation == Orientation::NoTransforms {
            &original
        } else {
            charge(
                &mut work,
                source_pixels
                    + u64::from(ow) * u64::from(height)
                    + 2 * u64::from(width) * u64::from(height),
                budget,
            )?;
            let mut resized = if same {
                original.clone()
            } else {
                resize_pixels(&original, width, height)
            };
            resized.apply_orientation(orientation);
            candidate = resized;
            &candidate
        };
        let output = encode(pixels, raster, profile, budget.bytes)?;
        if output.count <= budget.bytes && fits_transport(output.count) {
            charge(&mut work, u64::from(width) * u64::from(height), budget)?;
            return PromptImage::from_encoded(raster.mime(), output.bytes);
        }
        if (width, height) == (1, 1) {
            return Err(PromptError(
                "image cannot fit the prepared-image transport budget; manually convert the image",
            ));
        }
        let scale = ((budget.bytes as f64 / output.count as f64).sqrt() * 0.95).min(0.9);
        (width, height) = smaller_dimensions(ow, oh, width, height, scale);
    }
}

fn smaller_dimensions(ow: u32, oh: u32, width: u32, height: u32, scale: f64) -> (u32, u32) {
    // Derive both dimensions from a single original-relative scale. Round once;
    // clamping one-pixel axes is the only unavoidable aspect-ratio deviation.
    let next = ((width as f64 / ow as f64).min(height as f64 / oh as f64) * scale).max(0.0);
    let w = ((ow as f64 * next).floor() as u32).max(1).min(width);
    let h = ((oh as f64 * next).floor() as u32).max(1).min(height);
    if (w, h) == (width, height) {
        if width >= height && width > 1 {
            (width - 1, h)
        } else {
            (w, height.saturating_sub(1).max(1))
        }
    } else {
        (w, h)
    }
}

// A virtual float-premultiplied view avoids another full source allocation and
// avoids dark fringes from quantizing low-alpha colors in 8/16-bit integers.
// Every candidate samples untouched original pixels. Only the destination is
// unpremultiplied and rounded back to its original depth/color model.
struct AlphaView<'a, I: image::GenericImageView, P: image::Pixel> {
    source: &'a I,
    convert: fn(I::Pixel) -> P,
}
impl<I: image::GenericImageView, P: image::Pixel> image::GenericImageView for AlphaView<'_, I, P> {
    type Pixel = P;
    fn dimensions(&self) -> (u32, u32) {
        self.source.dimensions()
    }
    fn get_pixel(&self, x: u32, y: u32) -> P {
        (self.convert)(self.source.get_pixel(x, y))
    }
}
fn resize_alpha<I, P, Q>(
    source: &I,
    width: u32,
    height: u32,
    premultiply: fn(I::Pixel) -> Q,
    unpremultiply: fn(Q) -> P,
) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    I: image::GenericImageView,
    P: image::Pixel + 'static,
    Q: image::Pixel<Subpixel = f32> + 'static,
{
    let view = AlphaView {
        source,
        convert: premultiply,
    };
    let sampled = image::imageops::resize(&view, width, height, FilterType::CatmullRom);
    image::ImageBuffer::from_fn(width, height, |x, y| {
        unpremultiply(*sampled.get_pixel(x, y))
    })
}
fn resize_pixels(original: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    macro_rules! sample {
        ($source:expr, $pixel:ident, $variant:ident, $native:ty, $channels:expr, $max:expr) => {{
            let output = resize_alpha::<_, image::$pixel<$native>, image::$pixel<f32>>(
                $source,
                width,
                height,
                |pixel| {
                    let a = pixel.0[$channels - 1] as f32 / $max;
                    let mut values = [0.0; $channels];
                    for (value, source) in values[..$channels - 1].iter_mut().zip(pixel.0) {
                        *value = source as f32 / $max * a;
                    }
                    values[$channels - 1] = a;
                    image::$pixel(values)
                },
                |pixel| {
                    let a = pixel.0[$channels - 1].clamp(0.0, 1.0);
                    let mut values = [0; $channels];
                    if a > 0.0 {
                        for (value, source) in values[..$channels - 1].iter_mut().zip(pixel.0) {
                            *value = ((source / a).clamp(0.0, 1.0) * $max).round() as $native;
                        }
                    }
                    values[$channels - 1] = (a * $max).round() as $native;
                    image::$pixel(values)
                },
            );
            DynamicImage::$variant(output)
        }};
    }
    // No gamma/color-space conversion: retain the original color model and ICC.
    match original {
        DynamicImage::ImageRgba8(i) => sample!(i, Rgba, ImageRgba8, u8, 4, 255.0),
        DynamicImage::ImageLumaA8(i) => sample!(i, LumaA, ImageLumaA8, u8, 2, 255.0),
        DynamicImage::ImageRgba16(i) => sample!(i, Rgba, ImageRgba16, u16, 4, 65535.0),
        DynamicImage::ImageLumaA16(i) => sample!(i, LumaA, ImageLumaA16, u16, 2, 65535.0),
        _ => original.resize_exact(width, height, FilterType::CatmullRom),
    }
}

/// Keep at most the target bytes, but count the full encoded stream. An actual
/// encoder error is never interpreted as a size miss or retried at a lower size.
struct CountingOutput {
    bytes: Vec<u8>,
    count: usize,
    limit: usize,
}
impl Write for CountingOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.count = self
            .count
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("encoded image size overflow"))?;
        if self.count <= self.limit {
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(
    image: &DynamicImage,
    raster: Raster,
    profile: &[u8],
    limit: usize,
) -> Result<CountingOutput, PromptError> {
    let mut output = CountingOutput {
        bytes: Vec::new(),
        count: 0,
        limit,
    };
    let encode_error =
        |_| PromptError("image preparation encoding failed; manually convert the image");
    macro_rules! write {
        ($encoder:expr) => {{
            let mut encoder = $encoder;
            if !profile.is_empty() {
                encoder
                    .set_icc_profile(profile.to_vec())
                    .map_err(|_| PromptError("image color profile cannot be preserved"))?;
            }
            encoder
                .write_image(
                    image.as_bytes(),
                    image.width(),
                    image.height(),
                    image.color().into(),
                )
                .map_err(encode_error)?;
        }};
    }
    match raster {
        Raster::Png => write!(image::codecs::png::PngEncoder::new(&mut output)),
        Raster::Jpeg => write!(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut output,
            90
        )),
        Raster::Webp => write!(image::codecs::webp::WebPEncoder::new_lossless(&mut output)),
        Raster::Gif => unreachable!(),
    }
    Ok(output)
}
