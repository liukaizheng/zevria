//! App-owned LRU: eviction releases both the decoded pixels and GPUI atlas tile.
use super::{raster::rasterize, typeset::Formula};
use gpui_kit::{App, Global, RenderImage, Window};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

pub(super) const MAX_RASTER_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_RASTER_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Key {
    fingerprint: u64,
    display: bool,
    em: u32,
    scale: u32,
    color: [u32; 4],
}
impl Key {
    fn new(formula: &Formula, em: f32, scale: f32, rgba: [f32; 4]) -> Self {
        Self {
            fingerprint: formula.fingerprint,
            display: formula.display,
            em: em.to_bits(),
            scale: scale.to_bits(),
            color: rgba.map(f32::to_bits),
        }
    }
}
#[derive(Clone)]
pub(super) struct PaintedRaster {
    pub image: Arc<RenderImage>,
    pub offset: [f32; 2],
    pub width: f32,
    pub height: f32,
}
struct Entry {
    tex: Arc<str>,
    raster: PaintedRaster,
    bytes: usize,
}
pub(super) struct MathRasters {
    entries: HashMap<Key, Entry>,
    order: VecDeque<Key>,
    bytes: usize,
    byte_limit: usize,
    entry_limit: usize,
}
impl Global for MathRasters {}
impl Default for MathRasters {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            byte_limit: MAX_RASTER_BYTES,
            entry_limit: MAX_RASTER_ENTRIES,
        }
    }
}
impl MathRasters {
    fn get(&mut self, key: Key, tex: &str) -> Option<PaintedRaster> {
        let entry = self.entries.get(&key)?;
        // Fingerprints are an index, never identity. Verify the full source.
        if &*entry.tex != tex {
            return None;
        }
        let raster = entry.raster.clone();
        self.order.retain(|k| *k != key);
        self.order.push_back(key);
        Some(raster)
    }
    fn remove(&mut self, key: Key) -> Option<Arc<RenderImage>> {
        let entry = self.entries.remove(&key)?;
        self.bytes -= entry.bytes;
        self.order.retain(|k| *k != key);
        Some(entry.raster.image)
    }
    fn insert(&mut self, key: Key, entry: Entry) -> Vec<Arc<RenderImage>> {
        let mut evicted = Vec::new();
        if let Some(old) = self.remove(key) {
            evicted.push(old);
        }
        while !self.entries.is_empty()
            && (self.bytes + entry.bytes > self.byte_limit
                || self.entries.len() >= self.entry_limit)
        {
            if let Some(old) = self.remove(*self.order.front().expect("nonempty LRU")) {
                evicted.push(old);
            }
        }
        if entry.bytes <= self.byte_limit && self.entry_limit > 0 {
            self.bytes += entry.bytes;
            self.entries.insert(key, entry);
            self.order.push_back(key);
        } else {
            evicted.push(entry.raster.image);
        }
        evicted
    }
    /// Called only by the canvas paint step, never by layout or row measurement.
    pub(super) fn paint_raster(
        formula: &Formula,
        em: f32,
        scale: f32,
        rgba: [f32; 4],
        window: &mut Window,
        cx: &mut App,
    ) -> Option<PaintedRaster> {
        let key = Key::new(formula, em, scale, rgba);
        if let Some(hit) = cx.default_global::<Self>().get(key, &formula.tex) {
            return Some(hit);
        }
        let raster = rasterize(formula, em, scale, rgba)?;
        let bytes = raster.bgra.len();
        let pixels = image::RgbaImage::from_raw(raster.width, raster.height, raster.bgra)?;
        let painted = PaintedRaster {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
            offset: raster.offset,
            width: raster.width as f32 / scale,
            height: raster.height as f32 / scale,
        };
        let evicted = cx.default_global::<Self>().insert(
            key,
            Entry {
                tex: formula.tex.clone(),
                raster: painted.clone(),
                bytes,
            },
        );
        for image in evicted {
            cx.drop_image(image, Some(window));
        }
        Some(painted)
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::typeset::typeset;
    use super::*;

    fn entry(tex: &str, bytes: usize) -> Entry {
        Entry {
            tex: tex.into(),
            bytes,
            raster: PaintedRaster {
                image: Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::new(1, 1),
                )])),
                offset: [0.; 2],
                width: 1.,
                height: 1.,
            },
        }
    }
    #[test]
    fn lru_bytes_entries_and_evicted_images() {
        let a = Key::new(&typeset("a", false).unwrap(), 20., 2., [1.; 4]);
        let b = Key::new(&typeset("b", false).unwrap(), 20., 2., [1.; 4]);
        let c = Key::new(&typeset("c", false).unwrap(), 20., 2., [1.; 4]);
        let mut cache = MathRasters {
            byte_limit: 12,
            entry_limit: 2,
            ..Default::default()
        };
        let e = entry("b", 4);
        let b_image = e.raster.image.clone();
        assert!(cache.insert(a, entry("a", 4)).is_empty());
        assert!(cache.insert(b, e).is_empty());
        assert!(cache.get(a, "a").is_some());
        assert!(cache.get(a, "collision").is_none());
        let evicted = cache.insert(c, entry("c", 4));
        assert_eq!(evicted.len(), 1);
        assert!(Arc::ptr_eq(&evicted[0], &b_image));
        assert_eq!(cache.order, [a, c]);
        let evicted = cache.insert(b, entry("b", 12));
        assert_eq!(evicted.len(), 2);
        assert_eq!(cache.bytes, 12);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.insert(c, entry("c", 13)).len(), 2);
        assert_eq!(cache.bytes, 0);
    }
    #[test]
    fn keys_include_mode_size_scale_and_color_and_collisions_replace() {
        let f = typeset("x", false).unwrap();
        let key = Key::new(&f, 20., 2., [1.; 4]);
        assert_ne!(key, Key::new(&f, 21., 2., [1.; 4]));
        assert_ne!(key, Key::new(&f, 20., 1., [1.; 4]));
        assert_ne!(key, Key::new(&f, 20., 2., [0.; 4]));
        let mut cache = MathRasters::default();
        cache.insert(key, entry("x", 4));
        assert!(cache.get(key, "y").is_none());
        assert_eq!(cache.insert(key, entry("y", 4)).len(), 1);
        assert!(cache.get(key, "x").is_none());
        assert!(cache.get(key, "y").is_some());
        assert_eq!(cache.order.len(), 1);
        assert_eq!(cache.bytes, 4);
    }
}
