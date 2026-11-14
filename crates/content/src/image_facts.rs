//! Accounting-side immutable facts. No user JSON or provider-native metadata is trusted.
//! Cache keys are content identities; occurrences are counted by the caller, not deduplicated.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSource {
    Embedded,
    Url,
    FileId,
    Other,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFacts {
    pub source: ImageSource,
    pub mime: Option<String>,
    pub dimensions: Option<(u32, u32)>,
    pub identity: [u8; 32],
}
const MAX_FACTS: usize = 2048;
static FACTS: LazyLock<Mutex<HashMap<[u8; 32], ImageFacts>>> = LazyLock::new(Default::default);
static DERIVATIONS: AtomicU64 = AtomicU64::new(0);
thread_local! { static THREAD_DERIVATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
/// Thread-local instrumentation permits deterministic cache tests in parallel suites.
pub fn thread_fact_derivations() -> u64 {
    THREAD_DERIVATIONS.get()
}
/// Instrumentation counts bounded header parses, never elapsed time or private bytes.
pub fn fact_derivations() -> u64 {
    DERIVATIONS.load(Ordering::Relaxed)
}
fn key(mime: Option<&str>, data: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(mime.unwrap_or_default());
    hash.update([0]);
    hash.update(data);
    hash.finalize().into()
}
fn insert(facts: ImageFacts) {
    if let Ok(mut cache) = FACTS.lock() {
        // Bound memory; eviction only loses an optimization, not correctness.
        if cache.len() >= MAX_FACTS {
            cache.clear();
        }
        cache.insert(facts.identity, facts);
    }
}
pub(crate) fn remember_validated(image: &crate::PromptImage, base64: &str) {
    insert(ImageFacts {
        source: ImageSource::Embedded,
        mime: Some(image.mime_type().into()),
        dimensions: Some(image.dimensions()),
        identity: key(Some(image.mime_type()), base64),
    });
}
pub fn embedded(mime: Option<&str>, data: &str) -> ImageFacts {
    let identity = key(mime, data);
    // Empty accounting substitutions carry no header to derive. Do not pollute
    // the fact index or count this as a media parse.
    if data.is_empty() {
        return ImageFacts {
            source: ImageSource::Embedded,
            mime: mime.map(str::to_owned),
            dimensions: None,
            identity,
        };
    }
    if let Some(facts) = FACTS
        .lock()
        .ok()
        .and_then(|cache| cache.get(&identity).cloned())
    {
        return facts;
    }
    DERIVATIONS.fetch_add(1, Ordering::Relaxed);
    THREAD_DERIVATIONS.set(THREAD_DERIVATIONS.get().saturating_add(1));
    // Only decode a bounded header (not the raster, nor a multi-MiB ancillary chunk).
    // Long/unsupported headers remain unknown. Validation is a separate ingestion contract.
    let prefix = &data.as_bytes()[..data.len().min(128 * 1024) / 4 * 4];
    let dimensions = STANDARD.decode(prefix).ok().and_then(|bytes| {
        let format = image::guess_format(&bytes).ok()?;
        let actual_mime = match format {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::Gif => "image/gif",
            image::ImageFormat::WebP => "image/webp",
            _ => return None,
        };
        if mime.is_some_and(|mime| mime != actual_mime) {
            return None;
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(crate::prompt::MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(crate::prompt::MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(2 * 1024 * 1024);
        reader.limits(limits);
        let dimensions = reader.into_dimensions().ok()?;
        let (w, h) = dimensions;
        (w > 0
            && h > 0
            && w <= crate::prompt::MAX_IMAGE_DIMENSION
            && h <= crate::prompt::MAX_IMAGE_DIMENSION
            && u64::from(w) * u64::from(h) <= crate::prompt::MAX_IMAGE_PIXELS)
            .then_some(dimensions)
    });
    let facts = ImageFacts {
        source: ImageSource::Embedded,
        mime: mime.map(str::to_owned),
        dimensions,
        identity,
    };
    insert(facts.clone());
    facts
}
pub fn url(data: &str) -> ImageFacts {
    if let Some((header, payload)) = data
        .strip_prefix("data:")
        .and_then(|data| data.split_once(','))
        && let Some(mime) = header.strip_suffix(";base64")
    {
        return embedded(Some(mime), payload);
    }
    ImageFacts {
        source: ImageSource::Url,
        mime: None,
        dimensions: None,
        identity: key(None, data),
    }
}
pub fn reference(source: ImageSource, data: &str) -> ImageFacts {
    ImageFacts {
        source,
        mime: None,
        dimensions: None,
        identity: key(None, data),
    }
}
