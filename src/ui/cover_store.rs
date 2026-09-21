//! The shared cover store — one owner of decoded cover art per window.
//!
//! Both cover consumers read from here: the now-playing square and the browse
//! view's album thumbnails. Decoding runs off the UI thread and reports back
//! into this entity, so a library of covers populates progressively instead of
//! blocking the first frame.
//!
//! Two tiers, as the model's [`CoverCache`] describes them: decoded pixels are
//! downscaled once and held in a byte-budgeted LRU, and each downscaled image
//! is written to `~/.cache/larkspur/covers/<id>.jpg` so a later launch skips
//! re-decoding embedded art entirely.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, RenderImage};
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;

use crate::model::{CoverCache, DecodedImage, SongId};

/// The largest edge a cached cover is downscaled to. Covers are decoded at
/// multi-megapixel sizes; the UI never needs more than a few hundred pixels.
const MAX_COVER_PX: u32 = 300;

/// Byte budget for decoded covers held in memory (~180 covers at 300²).
const COVER_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// What the UI can draw for a song's cover right now.
#[derive(Clone)]
pub enum CoverImage {
    /// Requested; the decode is in flight.
    Loading,
    /// Decoded and GPU-ready — hand it straight to `img()`.
    Ready(Arc<RenderImage>),
    /// The file has no embedded art.
    Missing,
}

pub struct CoverStore {
    cache: CoverCache<RenderImage>,
    /// What the UI can draw, keyed by song. `Ready` entries mirror the cache's
    /// contents; the `Loading`/`Missing` markers carry no pixels and are tiny.
    images: HashMap<SongId, CoverImage>,
    /// Ids with a decode in flight, so a second ask doesn't spawn a second one.
    in_flight: HashSet<SongId>,
}

impl CoverStore {
    pub fn new(disk_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self {
            cache: CoverCache::new(disk_dir, COVER_MEMORY_BUDGET),
            images: HashMap::new(),
            in_flight: HashSet::new(),
        }
    }

    /// The cover for `id`, if anything is known about it yet.
    pub fn cover(&self, id: SongId) -> Option<CoverImage> {
        self.images.get(&id).cloned()
    }

    /// The cover for an optional song id — `None` in, `None` out.
    pub fn cover_of(&self, id: Option<SongId>) -> Option<CoverImage> {
        id.and_then(|id| self.cover(id))
    }

    /// Ask for `id`'s cover, decoding it off-thread the first time.
    ///
    /// `has_art` is the song's declared availability, so a song with no
    /// embedded picture is recorded as [`CoverImage::Missing`] without ever
    /// opening the file.
    pub fn request(&mut self, id: SongId, path: PathBuf, has_art: bool, cx: &mut Context<Self>) {
        if self.images.contains_key(&id) || self.in_flight.contains(&id) {
            return;
        }
        if !has_art {
            self.images.insert(id, CoverImage::Missing);
            return;
        }

        // Memory hit: nothing to decode.
        if let Some(image) = self.cache.get(id) {
            self.keep(id, image);
            return;
        }

        self.images.insert(id, CoverImage::Loading);
        self.in_flight.insert(id);

        let disk_path = self.cache.disk_path(id);
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move { load_cover(&path, &disk_path) })
                .await;

            this.update(cx, |store, cx| {
                store.in_flight.remove(&id);
                match decoded {
                    Some(image) => store.keep(id, image),
                    None => {
                        store.images.insert(id, CoverImage::Missing);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Put a decoded image in the byte-budgeted cache and mirror the result —
    /// including any evictions — into the UI map, so the two never diverge.
    fn keep(&mut self, id: SongId, image: Arc<RenderImage>) {
        let bytes = image.as_bytes(0).map(<[u8]>::len).unwrap_or(0);
        for evicted in self.cache.insert(id, image.clone(), bytes) {
            self.images.remove(&evicted);
        }

        // An image bigger than the whole budget evicts itself; don't advertise
        // a cover we didn't keep.
        if self.cache.get(id).is_some() {
            self.images.insert(id, CoverImage::Ready(image));
        } else {
            self.images.remove(&id);
        }
    }
}

/// Decode a song's cover, preferring the disk tier over the file's tags.
///
/// Runs off the UI thread. Returns `None` when there's genuinely no usable art,
/// which the caller records as `Missing`.
fn load_cover(path: &Path, disk_path: &Path) -> Option<Arc<RenderImage>> {
    let decoded = match read_disk(disk_path) {
        Some(cached) => cached,
        None => {
            let decoded = decode_embedded(path)?;
            write_disk(disk_path, &decoded);
            decoded
        }
    };
    to_render_image(&decoded)
}

/// Read a previously downscaled thumbnail from the disk tier.
fn read_disk(disk_path: &Path) -> Option<DecodedImage> {
    let bytes = std::fs::read(disk_path).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(DecodedImage { width: image.width(), height: image.height(), rgba: image.into_raw() })
}

/// Persist a downscaled thumbnail. Best-effort: a failed write only costs a
/// re-decode next launch.
fn write_disk(disk_path: &Path, decoded: &DecodedImage) {
    let Some(buffer) = rgba_buffer(decoded) else { return };
    // JPEG has no alpha, and cover art doesn't need one.
    let _ = image::DynamicImage::ImageRgba8(buffer).to_rgb8().save(disk_path);
}

/// Decode the first embedded picture in the file and downscale it.
fn decode_embedded(path: &Path) -> Option<DecodedImage> {
    let tagged = Probe::open(path).ok()?.guess_file_type().ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let picture = tag.pictures().first()?;

    let image = image::load_from_memory(picture.data()).ok()?;
    // `thumbnail` preserves the aspect ratio and only ever shrinks.
    let image = image.thumbnail(MAX_COVER_PX, MAX_COVER_PX).to_rgba8();
    Some(DecodedImage { width: image.width(), height: image.height(), rgba: image.into_raw() })
}

fn rgba_buffer(decoded: &DecodedImage) -> Option<image::RgbaImage> {
    image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba.clone())
}

/// Wrap decoded pixels in GPUI's GPU-ready image type.
///
/// `RenderImage` is **BGRA** — GPUI's own asset loader swaps R and B after
/// decoding — so the same swap has to happen here, or every cover renders with
/// the red and blue channels exchanged.
fn to_render_image(decoded: &DecodedImage) -> Option<Arc<RenderImage>> {
    let mut buffer = rgba_buffer(decoded)?;
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}
