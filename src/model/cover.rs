use std::path::PathBuf;
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

impl DecodedImage {
    /// Size in bytes of the pixel buffer — what the cache budgets against.
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// Two-tier cover cache: bounded in-memory LRU plus a persistent on-disk
/// thumbnail cache keyed by [`SongId`](super::SongId).
///
/// Eviction is by a real **byte budget**, not entry count — image sizes
/// vary too much for a count-based cap to mean anything.
///
/// Generic over the payload so the framework-agnostic model can own the
/// budgeting while the UI stores whatever it renders from: the model stores
/// [`DecodedImage`], the GPUI layer stores its own `RenderImage`. The caller
/// passes each entry's byte size, since only it knows the payload's shape.
pub struct CoverCache<T> {
    /// Each entry keeps its own byte size, so a replaced or evicted entry can
    /// be subtracted from the running total.
    memory: lru::LruCache<u64, (Arc<T>, usize)>,
    memory_bytes: usize,
    memory_budget_bytes: usize,
    disk_dir: PathBuf,
}

impl<T> CoverCache<T> {
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

    pub fn get(&mut self, id: u64) -> Option<Arc<T>> {
        self.memory.get(&id).map(|(value, _)| value.clone())
    }

    /// Insert (or replace) the entry for `id`, evicting least-recently-used
    /// entries until the byte budget is met. `bytes` is the payload's size,
    /// as reported by the caller.
    ///
    /// Returns the ids it evicted, so a caller keeping a parallel map (the UI's
    /// GPU-ready images) can drop the same entries and stay in lockstep with
    /// this cache instead of growing a second, unbounded one.
    pub fn insert(&mut self, id: u64, value: Arc<T>, bytes: usize) -> Vec<u64> {
        let mut evicted = Vec::new();

        // `put` hands back the entry it displaced. Account for it, or a
        // re-insert of the same id leaves the counter permanently high and
        // the cache evicts entries it should have kept.
        if let Some((_, previous_bytes)) = self.memory.put(id, (value, bytes)) {
            self.memory_bytes -= previous_bytes;
            evicted.push(id);
        }
        self.memory_bytes += bytes;

        while self.memory_bytes > self.memory_budget_bytes {
            match self.memory.pop_lru() {
                Some((key, (_, evicted_bytes))) => {
                    self.memory_bytes -= evicted_bytes;
                    evicted.push(key);
                }
                None => break,
            }
        }
        evicted
    }

    /// Current accounted memory usage in bytes. Exposed for tests and
    /// diagnostics (a cheap invariant check on the byte-budget math).
    pub fn memory_usage(&self) -> usize {
        self.memory_bytes
    }

    pub fn disk_path(&self, id: u64) -> PathBuf {
        self.disk_dir.join(format!("{id:016x}.jpg"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(bytes: usize) -> Arc<DecodedImage> {
        Arc::new(DecodedImage { width: 1, height: 1, rgba: vec![0; bytes] })
    }

    fn cache(budget: usize) -> CoverCache<DecodedImage> {
        CoverCache::new(std::env::temp_dir().join("larkspur-test-covers"), budget)
    }

    /// Insert accounting for the payload's real size, the way the UI does.
    fn insert(c: &mut CoverCache<DecodedImage>, id: u64, bytes: usize) -> Vec<u64> {
        let image = image(bytes);
        let size = image.byte_len();
        c.insert(id, image, size)
    }

    #[test]
    fn evicts_least_recently_used_to_meet_budget() {
        let mut c = cache(100);
        insert(&mut c, 1, 60);
        let evicted = insert(&mut c, 2, 60); // 120 > 100, so id 1 goes first
        assert_eq!(evicted, vec![1], "the caller is told what was dropped");
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert_eq!(c.memory_usage(), 60);
    }

    #[test]
    fn reinsert_does_not_leak_budget() {
        let mut c = cache(100);
        insert(&mut c, 1, 60);
        insert(&mut c, 1, 60); // replace, not accumulate
        assert_eq!(c.memory_usage(), 60);
        assert!(c.get(1).is_some());
    }

    #[test]
    fn oversized_image_does_not_stay_cached() {
        let mut c = cache(10);
        let evicted = insert(&mut c, 1, 50);
        assert_eq!(evicted, vec![1], "it evicted itself");
        assert_eq!(c.memory_usage(), 0);
        assert!(c.get(1).is_none());
    }
}
