//! Deterministic non-sensitive raster fixtures. Not enabled in production.
use crate::{
    PromptImage,
    image_preparation::{self, PreparedImage},
};

pub fn entropy_rgba(width: usize, height: usize) -> Vec<u8> {
    let mut seed = 0x31415926u32;
    (0..width * height * 4)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect()
}

/// A genuine incompressible image between the new target and the old 5 MiB
/// validation cap, not merely a tiny image with large metadata.
pub fn oversized_image() -> PromptImage {
    let image = PromptImage::from_rgba(1100, 1000, &entropy_rgba(1100, 1000)).unwrap();
    assert!(!image_preparation::fits_transport(image.encoded_len()));
    image
}

pub fn prepared_image() -> PreparedImage {
    let original = oversized_image();
    let result =
        image_preparation::prepare_encoded(original.mime_type(), original.encoded_bytes().to_vec())
            .unwrap();
    assert!(result.preparation.is_some());
    assert!(result.image.dimensions().0 < original.dimensions().0);
    assert!(image_preparation::fits_transport(
        result.image.encoded_len()
    ));
    result
}

pub fn oversized_animated_gif() -> PromptImage {
    let mut bytes = Vec::new();
    image::codecs::gif::GifEncoder::new(&mut bytes)
        .encode_frames((0..2).map(|_| image::Frame::new(image::RgbaImage::new(2, 2))))
        .unwrap();
    assert_eq!(bytes.pop(), Some(0x3b));
    bytes.extend_from_slice(&[0x21, 0xfe]);
    while bytes.len() <= image_preparation::MAX_PREPARED_IMAGE_BYTES {
        bytes.push(255);
        bytes.extend_from_slice(&[b'x'; 255]);
    }
    bytes.extend_from_slice(&[0, 0x3b]);
    PromptImage::from_encoded("image/gif", bytes).unwrap()
}

pub fn padded_png(size: usize) -> Vec<u8> {
    let mut bytes = PromptImage::from_rgba(2, 1, &[0; 8])
        .unwrap()
        .encoded_bytes()
        .to_vec();
    let end = bytes.split_off(bytes.len() - 12);
    let length = size - bytes.len() - end.len() - 12;
    let mut chunk = b"raNd".to_vec();
    chunk.resize(length + 4, b'x');
    let mut crc = !0u32;
    for &byte in &chunk {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
        }
    }
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.extend_from_slice(&chunk);
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes.extend_from_slice(&end);
    assert_eq!(bytes.len(), size);
    bytes
}
