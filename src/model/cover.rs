use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Decoded, already-downscaled RGBA pixel buffer ready for GPU upload.
/// Kept deliberately minimal/framework-agnostic — GPUI's own asset
/// system owns the actual texture upload.
#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Cover art for one song/album, as a state rather than an `Option`.
///
/// Distinguishes "haven't tried yet", "in flight", and "genuinely has no
/// art" so the UI doesn't re-attempt decodes every frame or flash a
/// placeholder→real swap mid-load.
#[derive(Debug, Clone)]
pub enum CoverState {
    NotRequested,
    Loading,
    Ready(Arc<DecodedImage>),
    Missing,
}

/// Two-tier cover cache: bounded in-memory LRU (decoded pixels) plus a
/// persistent on-disk thumbnail cache keyed by [`SongId`](super::SongId).
///
/// Eviction is by a real **byte budget**, not entry count — image sizes
/// vary too much for a count-based cap to mean anything.
pub struct CoverCache {
    memory: lru::LruCache<u64, Arc<DecodedImage>>,
    memory_bytes: usize,
    memory_budget_bytes: usize,
    disk_dir: PathBuf,
}

impl CoverCache {
    pub fn new(disk_dir: PathBuf, memory_budget_bytes: usize) -> Self {
        std::fs::create_dir_all(&disk_dir).ok();
        Self {
            // Capacity is a formality here — we evict by byte budget, not count.
            memory: lru::LruCache::unbounded(),
            memory_bytes: 0,
            memory_budget_bytes,
            disk_dir,
        }
    }

    pub fn get(&mut self, id: u64) -> Option<Arc<DecodedImage>> {
        self.memory.get(&id).cloned()
    }

    /// Insert (or replace) the decoded image for `id`, evicting
    /// least-recently-used entries until the byte budget is met.
    pub fn insert(&mut self, id: u64, image: Arc<DecodedImage>) {
        let size = image.rgba.len();

        // `put` hands back the entry it displaced. Account for it, or a
        // re-insert of the same id leaves the counter permanently high and
        // the cache evicts entries it should have kept.
        if let Some(previous) = self.memory.put(id, image) {
            self.memory_bytes -= previous.rgba.len();
        }
        self.memory_bytes += size;

        while self.memory_bytes > self.memory_budget_bytes {
            match self.memory.pop_lru() {
                Some((_, evicted)) => self.memory_bytes -= evicted.rgba.len(),
                None => break,
            }
        }
    }

    /// Current accounted memory usage in bytes. Exposed for tests and
    /// diagnostics (a cheap invariant check on the byte-budget math).
    pub fn memory_usage(&self) -> usize {
        self.memory_bytes
    }

    pub fn disk_path(&self, id: u64) -> PathBuf {
        self.disk_dir.join(format!("{id:016x}.jpg"))
    }

    /// Placeholder for the `folder.jpg`/`cover.png` fallback lookup —
    /// `lofty` only reads embedded pictures, so external cover files need
    /// this separate, album-level path.
    pub fn find_external_cover(album_dir: &Path) -> Option<PathBuf> {
        const CANDIDATES: &[&str] = &["cover.jpg", "cover.png", "folder.jpg", "folder.png"];
        CANDIDATES
            .iter()
            .map(|name| album_dir.join(name))
            .find(|p| p.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(bytes: usize) -> Arc<DecodedImage> {
        Arc::new(DecodedImage { width: 1, height: 1, rgba: vec![0; bytes] })
    }

    fn cache(budget: usize) -> CoverCache {
        CoverCache::new(std::env::temp_dir().join("larkspur-test-covers"), budget)
    }

    #[test]
    fn evicts_least_recently_used_to_meet_budget() {
        let mut c = cache(100);
        c.insert(1, image(60));
        c.insert(2, image(60)); // 120 > 100, so id 1 goes first
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert_eq!(c.memory_usage(), 60);
    }

    #[test]
    fn reinsert_does_not_leak_budget() {
        let mut c = cache(100);
        c.insert(1, image(60));
        c.insert(1, image(60)); // replace, not accumulate
        assert_eq!(c.memory_usage(), 60);
        assert!(c.get(1).is_some());
    }

    #[test]
    fn oversized_image_does_not_stay_cached() {
        let mut c = cache(10);
        c.insert(1, image(50));
        assert_eq!(c.memory_usage(), 0);
        assert!(c.get(1).is_none());
    }
}
