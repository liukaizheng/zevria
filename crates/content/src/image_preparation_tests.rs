use super::*;
use image::{ImageFormat, codecs::png::PngEncoder};
use sha2::{Digest as _, Sha256};

fn small_budget(bytes: usize) -> Budget {
    Budget { bytes, ..BUDGET }
}
fn png(image: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}
fn noisy(width: u32, height: u32) -> DynamicImage {
    let mut seed = 0x31415926u32;
    DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |_, _| {
        let mut pixel = [0; 4];
        for channel in &mut pixel {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *channel = seed as u8;
        }
        image::Rgba(pixel)
    }))
}
fn padded_png(size: usize) -> Vec<u8> {
    let mut bytes = png(&DynamicImage::new_rgba8(2, 1));
    let end = bytes.split_off(bytes.len() - 12);
    let len = size - bytes.len() - end.len() - 12;
    let mut chunk = b"raNd".to_vec();
    chunk.resize(len + 4, b'x');
    let mut crc = !0u32;
    for &byte in &chunk {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
        }
    }
    bytes.extend_from_slice(&(len as u32).to_be_bytes());
    bytes.extend_from_slice(&chunk);
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes.extend_from_slice(&end);
    assert_eq!(bytes.len(), size);
    bytes
}

#[test]
fn compliant_formats_keep_exact_bytes_mime_and_digest() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
        (ImageFormat::WebP, "image/webp"),
        (ImageFormat::Gif, "image/gif"),
    ] {
        let source = DynamicImage::ImageRgb8(noisy(16, 12).into_rgb8());
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, format).unwrap();
        let bytes = encoded.into_inner();
        let original = PromptImage::from_encoded(mime, bytes.clone()).unwrap();
        let prepared = prepare_base64(mime, &STANDARD.encode(&bytes)).unwrap();
        assert!(prepared.preparation.is_none());
        assert_eq!(prepared.image, original);
        assert_eq!(prepared.image.encoded_bytes(), bytes);
        assert_eq!(prepared.image.mime_type(), mime);
        assert_eq!(
            prepared.image.content_identity(),
            original.content_identity()
        );
    }
}

#[test]
fn exact_transport_boundary_and_padding_variants() {
    assert_eq!(MAX_PREPARED_IMAGE_BYTES, 3_932_112);
    assert_eq!(
        MAX_PREPARED_IMAGE_BYTES.div_ceil(3) * 4 + DATA_URL_RESERVE,
        MAX_IMAGE_TRANSPORT_BYTES
    );
    assert!(fits_transport(MAX_PREPARED_IMAGE_BYTES));
    assert!(!fits_transport(MAX_PREPARED_IMAGE_BYTES + 1));
    assert!(!fits_transport(usize::MAX));
    for len in MAX_PREPARED_IMAGE_BYTES - 2..=MAX_PREPARED_IMAGE_BYTES + 1 {
        let bytes = padded_png(len);
        let data = STANDARD.encode(&bytes);
        let prepared = prepare_base64("image/png", &data).unwrap();
        assert!(fits_transport(prepared.image.encoded_len()));
        assert_eq!(
            prepared.preparation.is_some(),
            len > MAX_PREPARED_IMAGE_BYTES
        );
        assert_eq!(prepared.image.dimensions(), (2, 1));
        if len <= MAX_PREPARED_IMAGE_BYTES {
            assert_eq!(prepared.image.encoded_bytes(), bytes);
        }
    }
}

#[test]
fn reported_base64_and_raster_sizes_reencode_without_reducing_pixels() {
    for len in [4_730_667, 6_307_556] {
        let bytes = padded_png(len);
        if len == 4_730_667 {
            assert_eq!(STANDARD.encode(&bytes).len(), 6_307_556);
        }
        let prepared = prepare_encoded("image/png", bytes).unwrap();
        assert_eq!(prepared.image.dimensions(), (2, 1));
        assert!(prepared.image.encoded_len() < 1024);
        assert!(prepared.image.base64().len() + DATA_URL_RESERVE <= MAX_IMAGE_TRANSPORT_BYTES);
        assert_eq!(prepared.preparation.unwrap().original_bytes, Some(len));
    }
}

#[test]
fn high_entropy_pixels_require_bicubic_downscaling_and_are_idempotent() {
    let source = noisy(256, 192);
    let original = png(&source);
    let prepared =
        prepare_encoded_with_budget("image/png", original, small_budget(24_000)).unwrap();
    let image = &prepared.image;
    let (w, h) = image.dimensions();
    assert!(w < 256 && h < 192);
    assert!((w as f64 / h as f64 - 256.0 / 192.0).abs() < 2.0 / h as f64);
    assert!(image.encoded_len() <= 24_000);
    assert_eq!(image.mime_type(), "image/png");
    image::load_from_memory(image.encoded_bytes()).unwrap();
    image.to_user_content(); // seed immutable facts from the accepted attachment
    let derivations = crate::image_facts::thread_fact_derivations();
    let facts = crate::image_facts::embedded(Some(image.mime_type()), &image.base64());
    assert_eq!(facts.dimensions, Some(image.dimensions()));
    assert_eq!(
        crate::image_facts::thread_fact_derivations(),
        derivations,
        "accounting must reuse prepared dimensions without raster decoding"
    );
    let again = prepare_encoded("image/png", image.encoded_bytes().to_vec()).unwrap();
    assert!(again.preparation.is_none());
    assert_eq!(&again.image, image);
    assert_eq!(
        image.content_identity(),
        &<[u8; 32]>::from(Sha256::digest(image.encoded_bytes()))
    );
    let prompt = UserPrompt::new(vec![
        PromptBlock::Text("a".into()),
        PromptBlock::Image(image.clone()),
        PromptBlock::Image(image.clone()),
        PromptBlock::Text("b".into()),
    ])
    .unwrap();
    assert!(!prompt_needs_preparation(&prompt));
    assert_eq!(prepare_prompt(prompt.clone()).unwrap(), prompt);
    assert_eq!(
        serde_json::from_str::<UserPrompt>(&serde_json::to_string(&prompt).unwrap()).unwrap(),
        prompt
    );
}

#[test]
fn resized_png_transparency_has_no_dark_fringe() {
    let mut source = image::RgbaImage::new(256, 192);
    for (x, y, pixel) in source.enumerate_pixels_mut() {
        let alpha = if x > 64 && x < 192 && y > 48 && y < 144 {
            255
        } else {
            0
        };
        // Invisible black must not contaminate the white shape's border.
        *pixel = if alpha == 0 {
            image::Rgba([0, 0, 0, 0])
        } else {
            image::Rgba([255, 255, 255, alpha])
        };
    }
    let rgba = source.into_raw();
    let result = prepare_rgba_with_budget(256, 192, &rgba, small_budget(300)).unwrap();
    let decoded = image::load_from_memory(result.image.encoded_bytes())
        .unwrap()
        .into_rgba8();
    assert!(decoded.width() < 256);
    assert!(decoded.pixels().any(|p| p[3] > 0 && p[3] < 255));
    for pixel in decoded.pixels().filter(|p| p[3] > 0) {
        assert_eq!(&pixel.0[..3], &[255; 3]);
    }
}

#[test]
fn low_alpha_colors_do_not_quantize_to_dark_fringes() {
    let rgba = image::RgbaImage::from_fn(32, 32, |x, y| {
        if (8..24).contains(&x) && (8..24).contains(&y) {
            image::Rgba([127, 80, 33, 1])
        } else {
            image::Rgba([0; 4])
        }
    });
    let resized = resize_pixels(&DynamicImage::ImageRgba8(rgba), 11, 11).into_rgba8();
    assert!(resized.pixels().any(|p| p[3] > 0));
    for pixel in resized.pixels().filter(|p| p[3] > 0) {
        for (actual, expected) in pixel.0[..3].iter().zip([127, 80, 33]) {
            assert!(actual.abs_diff(expected) <= 1);
        }
    }
    let rgba = image::ImageBuffer::from_fn(32, 32, |x, y| {
        if (8..24).contains(&x) && (8..24).contains(&y) {
            image::Rgba([32767u16, 12345, 60000, 1])
        } else {
            image::Rgba([0; 4])
        }
    });
    let resized = resize_pixels(&DynamicImage::ImageRgba16(rgba), 11, 11).into_rgba16();
    assert!(resized.pixels().any(|p| p[3] > 0));
    for pixel in resized.pixels().filter(|p| p[3] > 0) {
        for (actual, expected) in pixel.0[..3].iter().zip([32767, 12345, 60000]) {
            assert!(actual.abs_diff(expected) <= 1);
        }
    }
    let gray = DynamicImage::ImageLumaA8(image::ImageBuffer::from_pixel(
        32,
        32,
        image::LumaA([127u8, 1]),
    ));
    let resized = resize_pixels(&gray, 11, 11);
    assert_eq!(resized.color(), image::ColorType::La8);
    assert!(
        resized
            .as_luma_alpha8()
            .unwrap()
            .pixels()
            .all(|p| p.0 == [127, 1])
    );
}

#[test]
fn jpeg_orientation_and_bounded_icc_profile_survive_preparation() {
    // TIFF/EXIF orientation 6 = rotate 90 degrees clockwise.
    let exif = vec![
        b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
    ];
    let source = noisy(160, 80).into_rgb8();
    let mut bytes = Vec::new();
    let profile = b"test bounded profile".to_vec();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
    encoder.set_exif_metadata(exif).unwrap();
    encoder.set_icc_profile(profile.clone()).unwrap();
    encoder
        .write_image(source.as_raw(), 160, 80, image::ExtendedColorType::Rgb8)
        .unwrap();
    let result = prepare_encoded_with_budget("image/jpeg", bytes, small_budget(6000)).unwrap();
    assert_eq!(result.image.mime_type(), "image/jpeg");
    let (w, h) = result.image.dimensions();
    assert!(w <= 80 && h <= 160 && h > w);
    let mut decoder =
        image::codecs::jpeg::JpegDecoder::new(Cursor::new(result.image.encoded_bytes())).unwrap();
    assert_eq!(decoder.orientation().unwrap(), Orientation::NoTransforms);
    assert_eq!(decoder.icc_profile().unwrap(), Some(profile));
}

#[test]
fn static_webp_keeps_its_format_and_gif_animation_is_never_flattened() {
    let source = noisy(128, 96);
    let mut bytes = Cursor::new(Vec::new());
    source.write_to(&mut bytes, ImageFormat::WebP).unwrap();
    let result =
        prepare_encoded_with_budget("image/webp", bytes.into_inner(), small_budget(6000)).unwrap();
    assert_eq!(result.image.mime_type(), "image/webp");
    assert!(result.image.dimensions().0 < 128);
    for count in [1, 2] {
        let mut bytes = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut bytes)
            .encode_frames((0..count).map(|_| image::Frame::new(source.to_rgba8())))
            .unwrap();
        let untouched = prepare_encoded("image/gif", bytes.clone()).unwrap();
        assert_eq!(untouched.image.encoded_bytes(), bytes);
        let prepared = prepare_encoded_with_budget("image/gif", bytes, small_budget(6000));
        if count == 1 {
            assert_eq!(prepared.unwrap().image.mime_type(), "image/png");
        } else {
            assert!(
                prepared
                    .unwrap_err()
                    .to_string()
                    .contains("oversized animated GIF")
            );
        }
    }
}

#[test]
fn malformed_and_unsupported_inputs_and_budgets_fail_redacted() {
    let bytes = png(&noisy(32, 24));
    assert!(
        prepare_encoded("image/jpeg", bytes.clone())
            .unwrap_err()
            .to_string()
            .contains("MIME")
    );
    assert!(
        prepare_base64("image/png", "SECRET-not-base64")
            .unwrap_err()
            .to_string()
            .find("SECRET")
            .is_none()
    );
    assert!(prepare_base64("image/png", &"x".repeat(MAX_IMAGE_SOURCE_BASE64_BYTES + 1)).is_err());
    assert!(prepare_file_bytes(vec![0; MAX_IMAGE_SOURCE_BYTES + 1]).is_err());
    assert!(prepare_file_bytes(vec![]).is_err());
    assert!(prepare_file_bytes(b"BMunsupported BMP".to_vec()).is_err());
    assert!(prepare_rgba(0, 1, &[]).is_err());
    assert!(prepare_rgba(usize::MAX, 1, &[]).is_err());
    assert!(prepare_rgba(10_000, 10_000, &[]).is_err());
    assert!(prepare_rgba(16_385, 1, &[]).is_err());
    assert!(prepare_rgba(1, 1, &[1]).is_err());
    let mut corrupt = padded_png(1024);
    let index = corrupt.len() - 13;
    corrupt[index] ^= 1;
    assert!(prepare_encoded_with_budget("image/png", corrupt, small_budget(300)).is_err());
    for (budget, diagnostic) in [
        (
            Budget {
                attempts: 0,
                ..small_budget(300)
            },
            "attempt",
        ),
        (
            Budget {
                pixel_work: 1,
                ..small_budget(300)
            },
            "pixel-work",
        ),
        (
            Budget {
                buffers: 1,
                ..small_budget(300)
            },
            "buffer",
        ),
    ] {
        assert!(
            prepare_encoded_with_budget("image/png", bytes.clone(), budget)
                .unwrap_err()
                .to_string()
                .contains(diagnostic)
        );
    }
    let mut truncated = bytes;
    truncated.pop();
    assert!(prepare_encoded_with_budget("image/png", truncated, small_budget(300)).is_err());
    // Genuine encoder errors are not treated as size misses.
    assert!(
        encode(&DynamicImage::new_rgba16(2, 2), Raster::Webp, &[], 1)
            .err()
            .unwrap()
            .to_string()
            .contains("encoding failed")
    );
    let mut output = CountingOutput {
        bytes: Vec::new(),
        count: 0,
        limit: 2,
    };
    output.write_all(&[1, 2, 3, 4]).unwrap();
    assert_eq!(output.count, 4);
    assert!(output.bytes.is_empty());
}

#[test]
fn png_16_bit_color_and_profile_are_preserved_when_reencoding() {
    let source = DynamicImage::ImageRgba16(image::ImageBuffer::from_fn(64, 32, |x, y| {
        image::Rgba([(x * 1000) as u16, (y * 2000) as u16, 500, 65535])
    }));
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(vec![1; 128]).unwrap();
    encoder
        .write_image(source.as_bytes(), 64, 32, source.color().into())
        .unwrap();
    let result = prepare_encoded_with_budget("image/png", bytes, small_budget(500)).unwrap();
    let mut decoder =
        image::codecs::png::PngDecoder::new(Cursor::new(result.image.encoded_bytes())).unwrap();
    assert_eq!(decoder.color_type(), image::ColorType::Rgba16);
    assert_eq!(decoder.icc_profile().unwrap(), Some(vec![1; 128]));
}

fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = bytes.len();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    let mut crc = !0u32;
    for &byte in &bytes[start..] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
        }
    }
    bytes.extend_from_slice(&(!crc).to_be_bytes());
}

#[test]
fn apng_and_animated_webp_are_explicitly_rejected_without_first_frame_substitution() {
    let source = DynamicImage::new_rgba8(2, 1);
    let plain = png(&source);
    let mut idat = Vec::new();
    let mut offset = 8;
    while offset < plain.len() {
        let length = u32::from_be_bytes(plain[offset..offset + 4].try_into().unwrap()) as usize;
        if &plain[offset + 4..offset + 8] == b"IDAT" {
            idat.extend_from_slice(&plain[offset + 8..offset + 8 + length]);
        }
        offset += length + 12;
    }
    let mut apng = plain[..33].to_vec(); // signature and complete IHDR
    png_chunk(&mut apng, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
    for frame in 0..2u32 {
        let sequence = frame;
        let mut control = sequence.to_be_bytes().to_vec();
        for n in [2u32, 1, 0, 0] {
            control.extend_from_slice(&n.to_be_bytes());
        }
        control.extend_from_slice(&[0, 1, 0, 10, 0, 0]);
        png_chunk(&mut apng, b"fcTL", &control);
        if frame == 0 {
            png_chunk(&mut apng, b"IDAT", &idat);
        } else {
            // The first frame's IDAT does not consume a sequence number.
            let mut data = 2u32.to_be_bytes().to_vec();
            data.extend_from_slice(&idat);
            png_chunk(&mut apng, b"fdAT", &data);
        }
    }
    png_chunk(&mut apng, b"IEND", &[]);
    assert!(
        image::codecs::png::PngDecoder::new(Cursor::new(&apng))
            .unwrap()
            .is_apng()
            .unwrap()
    );
    assert!(
        prepare_encoded("image/png", apng)
            .unwrap_err()
            .to_string()
            .contains("animated PNG")
    );

    fn riff_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 != 0 {
            bytes.push(0);
        }
    }
    let mut still = Cursor::new(Vec::new());
    source.write_to(&mut still, ImageFormat::WebP).unwrap();
    let mut animated = b"RIFF\0\0\0\0WEBP".to_vec();
    riff_chunk(&mut animated, b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
    riff_chunk(&mut animated, b"ANIM", &[0; 6]);
    let mut frame = vec![0; 16];
    frame[6] = 1; // two-pixel width, one-pixel height
    frame[12] = 10;
    frame.extend_from_slice(&still.into_inner()[12..]);
    for _ in 0..2 {
        riff_chunk(&mut animated, b"ANMF", &frame);
    }
    let length = animated.len() as u32 - 8;
    animated[4..8].copy_from_slice(&length.to_le_bytes());
    assert!(
        image::codecs::webp::WebPDecoder::new(Cursor::new(&animated))
            .unwrap()
            .has_animation()
    );
    assert!(
        prepare_encoded("image/webp", animated)
            .unwrap_err()
            .to_string()
            .contains("animated WebP")
    );
}

#[test]
fn buffer_and_work_budgets_can_reduce_candidates_or_stop_them_safely() {
    let source = noisy(64, 64);
    let original_dimensions = source.dimensions();
    let prepared = prepare_pixels(
        source,
        Raster::Png,
        &[],
        Orientation::NoTransforms,
        0,
        Budget {
            buffers: 40_000,
            ..BUDGET
        },
    )
    .unwrap();
    assert!(prepared.dimensions().0 < original_dimensions.0);
    assert!(fits_transport(prepared.encoded_len()));
    let error = prepare_encoded_with_budget(
        "image/png",
        png(&noisy(64, 64)),
        Budget {
            attempts: 1,
            ..small_budget(300)
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("attempt"));
    let source = noisy(32, 24);
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder
        .set_icc_profile(vec![1; MAX_PROFILE_BYTES + 1])
        .unwrap();
    encoder
        .write_image(source.as_bytes(), 32, 24, source.color().into())
        .unwrap();
    assert!(
        prepare_encoded_with_budget("image/png", bytes, small_budget(300))
            .unwrap_err()
            .to_string()
            .contains("color profile")
    );
}

#[test]
fn historical_over_target_bytes_remain_lossless_until_explicit_new_submission() {
    let original =
        PromptImage::from_encoded("image/png", padded_png(MAX_PREPARED_IMAGE_BYTES + 100)).unwrap();
    let prompt = UserPrompt::new(vec![
        PromptBlock::Text("prefix".into()),
        PromptBlock::Image(original.clone()),
        PromptBlock::Image(original.clone()),
    ])
    .unwrap();
    let stored = serde_json::to_string(&prompt).unwrap();
    let recalled: UserPrompt = serde_json::from_str(&stored).unwrap();
    assert_eq!(recalled, prompt);
    assert_eq!(
        UserPrompt::from_message(&prompt.to_message()).unwrap(),
        prompt
    );
    assert!(prompt_needs_preparation(&recalled));
    let prepared = prepare_prompt(recalled).unwrap();
    assert_eq!(prepared.images().count(), 2);
    for image in prepared.images() {
        assert!(image.encoded_len() < 1024);
    }
    assert_eq!(prepared.images().next(), prepared.images().nth(1));
    assert_eq!(serde_json::to_string(&prompt).unwrap(), stored);
}
