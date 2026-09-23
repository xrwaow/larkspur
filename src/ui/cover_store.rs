//! The shared cover store — one owner of decoded cover art per window.
//!
//! Both cover consumers read from here: the now-playing square and the browse
//! view's album thumbnails. Decoding runs off the UI thread and reports back
//! into this entity, so a library of covers populates progressively instead of
//! blocking the first frame.
//!
//! Two tiers, as the model's [`CoverCache`] describes them. A larger (Lanczos3)
//! **master** is written to `~/.cache/larkspur/covers-v2/<id>.jpg` so a later
//! launch skips decoding the file's embedded art, and each launch derives its
//! on-screen variants from that master at the window's physical pixel sizes, so
//! each is drawn at roughly 1:1 instead of being shrunk by the GPU from a single
//! oversized image.
//!
//! The two variants are cached separately, because their sizes differ by orders
//! of magnitude. The small **thumbnails** are the bulk of the entries and sit in
//! the model's byte-budgeted LRU; the large **full** now-playing cover is kept
//! only for the current track (and a couple of recent ones), in a tiny slot.
//!
//! The store also derives each cover's **dominant colour** at decode time (it
//! already has the pixels in hand, off-thread) and exposes it via
//! [`accent`](CoverStore::accent). That's what the dynamic theme tints from, so
//! no view has to analyse an image itself.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, Hsla, RenderImage, Rgba};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::DynamicImage;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;

use crate::model::{CoverCache, DecodedImage, SongId};
use crate::ui::albums::THUMB_PX;
use crate::ui::cover::CoverView;

/// The largest edge kept in the disk-tier master. Big enough to derive the
/// now-playing square at any display scale from, without upscaling before the
/// GPU draws it. Fixed rather than scale-derived: the master is written once and
/// shared across launches, so it must not bake in one display's assumptions.
/// `shrink` never upsizes, so small embedded art stays untouched.
const MASTER_PX: u32 = 1024;

/// JPEG quality for the disk-tier master. Covers are re-encoded here, so this
/// is the only lossy step; keep it high enough that it never shows.
const MASTER_QUALITY: u8 = 92;

/// Byte budget for decoded **thumbnails** held in memory. Thumbs are tiny
/// (~20 KB at 2x), so this holds thousands of them — the browse grid's working
/// set. The full now-playing cover is not in here; it lives in a tiny slot.
const COVER_MEMORY_BUDGET: usize = 64 * 1024 * 1024;

/// How many full-size now-playing covers to keep, so skipping back a track or
/// two is instant. Deliberately small: these are the only large images held.
const FULL_SLOTS: usize = 2;

/// The physical-pixel sizes to derive for one cover, resolved once from the
/// window's scale factor so each variant is drawn at roughly 1:1.
#[derive(Clone, Copy)]
struct Sizes {
    /// The now-playing square's long edge.
    full: u32,
    /// The browse thumbnail's (square) edge.
    thumb: u32,
}

impl Sizes {
    /// Resolve logical sizes to physical pixels for a display `scale`.
    fn for_scale(scale: f32) -> Self {
        Self {
            full: (CoverView::SIZE * scale).round().max(1.0) as u32,
            thumb: (THUMB_PX * scale).round().max(1.0) as u32,
        }
    }
}

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

/// The now-playing cover — the **full** variant, derived only for the track
/// that's on screen rather than kept for the whole library.
#[derive(Clone)]
pub enum FullCover {
    /// Requested; the decode is in flight.
    Loading,
    /// Decoded and GPU-ready — hand it straight to `img()`.
    Ready(Arc<RenderImage>),
    /// The file has no embedded art.
    Missing,
}

pub struct CoverStore {
    /// Byte-budgeted LRU of the small browse **thumbnails**, the bulk of the
    /// entries. Mirrored by `images`.
    cache: CoverCache<RenderImage>,
    /// The physical sizes every cover is derived at, fixed for this window.
    sizes: Sizes,
    /// What the UI can draw for a thumbnail, keyed by song. `Ready` entries
    /// mirror the cache's contents; the `Loading`/`Missing` markers carry no
    /// pixels and are tiny.
    images: HashMap<SongId, CoverImage>,
    /// The full now-playing covers, most-recent first, capped at [`FULL_SLOTS`].
    /// Only tracks just shown live here; a small `Vec` beats an LRU for the
    /// read-only borrow `full` needs.
    fulls: Vec<(SongId, FullCover)>,
    /// Each decoded cover's dominant colour, for the dynamic theme. Tiny, and
    /// kept even after the pixels are evicted.
    accents: HashMap<SongId, Rgba>,
    /// Ids with a thumbnail decode in flight, so a second ask doesn't spawn a
    /// second one.
    in_flight: HashSet<SongId>,
    /// Ids with a full-cover decode in flight, separately deduped.
    full_in_flight: HashSet<SongId>,
}

impl CoverStore {
    pub fn new(disk_dir: PathBuf, scale: f32, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        Self {
            cache: CoverCache::new(disk_dir, COVER_MEMORY_BUDGET),
            sizes: Sizes::for_scale(scale),
            images: HashMap::new(),
            fulls: Vec::new(),
            accents: HashMap::new(),
            in_flight: HashSet::new(),
            full_in_flight: HashSet::new(),
        }
    }

    /// The thumbnail for `id`, if anything is known about it yet.
    pub fn cover(&self, id: SongId) -> Option<CoverImage> {
        self.images.get(&id).cloned()
    }

    /// The thumbnail for an optional song id — `None` in, `None` out.
    pub fn cover_of(&self, id: Option<SongId>) -> Option<CoverImage> {
        id.and_then(|id| self.cover(id))
    }

    /// The full now-playing cover for `id`, if it's one of the recent tracks.
    pub fn full(&self, id: SongId) -> Option<FullCover> {
        self.fulls.iter().find(|(song, _)| *song == id).map(|(_, cover)| cover.clone())
    }

    /// A cover's dominant colour, once it's been decoded — the dynamic theme's
    /// seed.
    pub fn accent(&self, id: SongId) -> Option<Rgba> {
        self.accents.get(&id).copied()
    }

    /// Ask for `id`'s **thumbnail**, decoding it off-thread the first time. This
    /// is what the browse rows request for every song.
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
            self.keep(id, image, None);
            return;
        }

        self.images.insert(id, CoverImage::Loading);
        self.in_flight.insert(id);

        let disk_path = self.cache.disk_path(id);
        let sizes = self.sizes;
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move { load_thumb(&path, &disk_path, sizes) })
                .await;

            this.update(cx, |store, cx| {
                store.in_flight.remove(&id);
                match decoded {
                    Some((image, accent)) => store.keep(id, image, accent),
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

    /// Ask for `id`'s **full** now-playing cover, deriving it from the disk
    /// master off-thread. Only the tracks currently on screen need this, so it
    /// lives in a tiny slot rather than the byte budget.
    pub fn request_full(&mut self, id: SongId, path: PathBuf, has_art: bool, cx: &mut Context<Self>) {
        if self.full(id).is_some() || self.full_in_flight.contains(&id) {
            return;
        }
        if !has_art {
            self.record_full(id, FullCover::Missing);
            return;
        }

        self.record_full(id, FullCover::Loading);
        self.full_in_flight.insert(id);

        let disk_path = self.cache.disk_path(id);
        let sizes = self.sizes;
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move { load_full(&path, &disk_path, sizes) })
                .await;

            this.update(cx, |store, cx| {
                store.full_in_flight.remove(&id);
                match decoded {
                    Some((image, accent)) => {
                        if let Some(accent) = accent {
                            store.accents.insert(id, accent);
                        }
                        store.record_full(id, FullCover::Ready(image));
                    }
                    None => store.record_full(id, FullCover::Missing),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Put a decoded thumbnail in the byte-budgeted cache and mirror the result
    /// — including any evictions — into the UI map, so the two never diverge.
    fn keep(&mut self, id: SongId, image: Arc<RenderImage>, accent: Option<Rgba>) {
        if let Some(accent) = accent {
            self.accents.insert(id, accent);
        }
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

    /// Record a full cover in the recent-tracks slot list, most-recent first,
    /// evicting beyond [`FULL_SLOTS`].
    fn record_full(&mut self, id: SongId, cover: FullCover) {
        self.fulls.retain(|(song, _)| *song != id);
        self.fulls.insert(0, (id, cover));
        self.fulls.truncate(FULL_SLOTS);
    }
}

/// The dominant, reasonably-saturated colour of a decoded cover — the seed for
/// the dynamic theme.
///
/// Pixels are bucketed by hue and weighted by saturation, and the heaviest
/// bucket's average wins; a cover with no saturated pixels falls back to its
/// overall average, and an empty image yields `None`.
pub fn dominant_color(image: &DecodedImage) -> Option<Rgba> {
    const BUCKETS: usize = 12;
    let mut weight = [0.0f32; BUCKETS];
    let mut sum = [[0.0f32; 3]; BUCKETS];
    let mut average = [0.0f32; 3];
    let mut counted = 0.0f32;

    for pixel in image.rgba.chunks_exact(4) {
        let (r, g, b, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        if a < 128 {
            continue;
        }
        let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let hsla: Hsla = Rgba { r, g, b, a: 1.0 }.into();
        if hsla.s > 0.15 && (0.15..=0.85).contains(&hsla.l) {
            let bucket = ((hsla.h * BUCKETS as f32) as usize).min(BUCKETS - 1);
            weight[bucket] += hsla.s;
            sum[bucket][0] += r * hsla.s;
            sum[bucket][1] += g * hsla.s;
            sum[bucket][2] += b * hsla.s;
        }
        average[0] += r;
        average[1] += g;
        average[2] += b;
        counted += 1.0;
    }

    let (best, &heaviest) = weight.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    if heaviest > 0.0 {
        let bucket = sum[best];
        Some(Rgba { r: bucket[0] / heaviest, g: bucket[1] / heaviest, b: bucket[2] / heaviest, a: 1.0 })
    } else if counted > 0.0 {
        Some(Rgba { r: average[0] / counted, g: average[1] / counted, b: average[2] / counted, a: 1.0 })
    } else {
        None
    }
}

/// Read the disk-tier master, or decode one from the file's tags and persist it.
///
/// Runs off the UI thread. `None` only when there's genuinely no usable art.
fn load_master(path: &Path, disk_path: &Path) -> Option<DynamicImage> {
    match read_disk(disk_path) {
        Some(master) => Some(master),
        None => {
            let master = decode_embedded(path)?;
            write_disk(disk_path, &master);
            Some(master)
        }
    }
}

/// Derive the browse thumbnail — cropped to a centred square — from the master,
/// with the dominant colour the dynamic theme seeds from.
fn load_thumb(
    path: &Path,
    disk_path: &Path,
    sizes: Sizes,
) -> Option<(Arc<RenderImage>, Option<Rgba>)> {
    let master = load_master(path, disk_path)?;
    // One pass crops to a centred square and resamples, so the thumbnail's
    // `ObjectFit::Cover` has nothing left to do.
    let thumb = master.resize_to_fill(sizes.thumb, sizes.thumb, FilterType::Lanczos3);
    let decoded = decoded_from(&thumb);
    let accent = dominant_color(&decoded);
    Some((to_render_image(&decoded)?, accent))
}

/// Derive the full now-playing cover from the master. Aspect-preserved, so
/// `ObjectFit::Cover` does the final crop; only ever shrinks, so small art stays
/// as-is for the GPU.
fn load_full(
    path: &Path,
    disk_path: &Path,
    sizes: Sizes,
) -> Option<(Arc<RenderImage>, Option<Rgba>)> {
    let master = load_master(path, disk_path)?;
    let full = shrink(master, sizes.full);
    let decoded = decoded_from(&full);
    let accent = dominant_color(&decoded);
    Some((to_render_image(&decoded)?, accent))
}

/// Downscale `image` so neither edge exceeds `max`, leaving smaller art alone.
fn shrink(image: DynamicImage, max: u32) -> DynamicImage {
    if image.width() <= max && image.height() <= max {
        image
    } else {
        image.resize(max, max, FilterType::Lanczos3)
    }
}

/// Flatten a decoded image into the framework-agnostic pixel type.
fn decoded_from(image: &DynamicImage) -> DecodedImage {
    let rgba = image.to_rgba8();
    DecodedImage { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }
}

/// Read the disk-tier master, if a previous launch wrote one.
fn read_disk(disk_path: &Path) -> Option<DynamicImage> {
    image::load_from_memory(&std::fs::read(disk_path).ok()?).ok()
}

/// Persist the disk-tier master. Best-effort: a failed write only costs a
/// re-decode next launch. Encoded at a high quality, since this copy is what
/// every later launch derives its variants from.
fn write_disk(disk_path: &Path, master: &DynamicImage) {
    let Ok(file) = std::fs::File::create(disk_path) else { return };
    let rgb = master.to_rgb8();
    let _ = JpegEncoder::new_with_quality(BufWriter::new(file), MASTER_QUALITY).encode_image(&rgb);
}

/// Decode the first embedded picture in the file and downscale it to the
/// disk-tier master size.
fn decode_embedded(path: &Path) -> Option<DynamicImage> {
    let tagged = Probe::open(path).ok()?.guess_file_type().ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let picture = tag.pictures().first()?;

    let image = image::load_from_memory(picture.data()).ok()?;
    Some(shrink(image, MASTER_PX))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-row RGBA image from RGB pixels.
    fn image_from(pixels: &[(u8, u8, u8)]) -> DecodedImage {
        let mut rgba = Vec::with_capacity(pixels.len() * 4);
        for (r, g, b) in pixels {
            rgba.extend_from_slice(&[*r, *g, *b, 255]);
        }
        DecodedImage { width: pixels.len() as u32, height: 1, rgba }
    }

    #[test]
    fn dominant_color_picks_the_saturated_hue() {
        // Mostly grey with a few strongly blue pixels: blue wins.
        let image = image_from(&[(128, 128, 128), (128, 128, 128), (10, 20, 220), (20, 30, 200)]);
        let accent = dominant_color(&image).expect("an accent");
        let hsla: Hsla = accent.into();
        // Blue sits around 240°, i.e. 0.66 in the 0..1 hue range.
        assert!(hsla.h > 0.55 && hsla.h < 0.75, "hue {hsla:?}");
    }

    #[test]
    fn dominant_color_falls_back_to_the_average_for_greys() {
        let image = image_from(&[(100, 100, 100), (100, 100, 100)]);
        let accent = dominant_color(&image).expect("an accent");
        let (r, g, b) = (accent.r, accent.g, accent.b);
        assert!((r - g).abs() < 0.02 && (g - b).abs() < 0.02, "grey in, grey out");
    }
}
