//! Procedural cover-art texture.
//!
//! The lyrics rail has a hole in it when a track has no lyrics. Rather than
//! leaving that hole flat, this renders a tileable texture out of the track's
//! cover: extract the cover's colour candidates, quilt small patches of the art
//! onto a torus with minimum-error seams, blur it, then remap the result through
//! the same black/white theme structure the rest of the UI uses.
//!
//! This is a port of the reference `quilt_texture` pipeline (the Python and
//! standalone Rust implementations behave identically). The pure rendering path
//! is kept free of GPUI so it can be tested directly; [`load_for_song`] is the
//! thin bridge that reads a song's embedded art and wraps the pixels for GPUI.

mod color;
mod rng;

use std::path::Path;
use std::sync::Arc;

use gpui::RenderImage;
use image::imageops;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use sha2::{Digest, Sha256};

use crate::model::DynamicBase;
use rng::Rng;

// ---- config ---------------------------------------------------------------

/// The texture's square edge. Rounded down to a multiple of `CHUNK_SIZE -
/// OVERLAP` so the quilting wraps seamlessly.
const RENDER_SIZE: usize = 240;
const GREYSCALE_SATURATION: f64 = 0.08;
const NUM_ACCENTS: usize = 2;
const MIN_HUE_SEPARATION: f64 = 0.06;
const ACCENT_MARK_STRENGTH: f32 = 0.55;
const ACCENT_MARK_HUE_WIDTH: f32 = 0.08;
const STRUCTURE_GAMMA: f32 = 0.8;
const BLUR_RADIUS: f32 = 2.0;
const STRUCTURE_LEVELS: i32 = 4;
const MARK_LEVELS: i32 = 3;
const CHUNK_SIZE: usize = 100;
const NUM_CHUNKS: usize = 512;
const OVERLAP: usize = CHUNK_SIZE / 4;
const BEST_K: usize = 10;
const FLAT_DISCARD_FRACTION: f64 = 0.7;
const CANDIDATE_OVERSAMPLE: usize = 20;

/// Covers larger than this are downscaled before sampling, so generating a
/// texture from a 4000px artwork stays cheap. The seed still comes from the
/// original encoded bytes.
const MAX_SOURCE_EDGE: u32 = 1024;

/// The theme half of the palette. Mirrors `dynamic_dark` / `dynamic_light`:
/// black is a dark panel with a bright accent, white is its inverse.
#[derive(Clone, Copy)]
struct ThemeParams {
    bg_sat: f64,
    bg_l: f64,
    ink_sat: f64,
    ink_l: f64,
    accent_l: f64,
}

impl ThemeParams {
    fn for_base(base: DynamicBase) -> Self {
        match base {
            DynamicBase::Black => Self {
                bg_sat: 0.5,
                bg_l: 0.08,
                ink_sat: 0.7,
                ink_l: 0.21,
                accent_l: 0.66,
            },
            DynamicBase::White => Self {
                bg_sat: 0.35,
                bg_l: 0.97,
                ink_sat: 0.5,
                ink_l: 0.81,
                accent_l: 0.34,
            },
        }
    }
}

struct Image {
    w: usize,
    h: usize,
    px: Vec<[f32; 3]>,
}

impl Image {
    #[inline]
    fn at(&self, y: usize, x: usize) -> [f32; 3] {
        self.px[y * self.w + x]
    }
}

#[derive(Clone)]
struct Chunk {
    data: Vec<f32>,
    strips: [Vec<f32>; 4],
}

#[derive(Clone, Copy)]
struct Candidate {
    hue: f64,
    sat: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
            Side::Top => 2,
            Side::Bottom => 3,
        }
    }
}

// ---- entry points ---------------------------------------------------------

/// The result of loading a song's cover texture.
pub enum LoadedTexture {
    /// The cover hashes to the seed the panel already has a texture for, so
    /// the existing image still applies.
    Unchanged,
    /// A newly rendered texture, and the seed it was rendered from.
    Rendered { seed: u64, image: Arc<RenderImage> },
}

/// The raw bytes of a song's first embedded picture.
fn cover_bytes(path: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(path).ok()?.guess_file_type().ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    Some(tag.pictures().first()?.data().to_vec())
}

fn seed_for_bytes(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(
        Sha256::digest(bytes)[..8]
            .try_into()
            .expect("sha256 digest is 32 bytes"),
    )
}

/// Load a texture from a song's embedded cover.
///
/// `keep_seed` is the seed of the texture already on screen: when the cover
/// hashes to it, no rendering is needed and [`LoadedTexture::Unchanged`] is
/// returned. Returns `None` when the file has no readable art (or the art is
/// too small to quilt), in which case the lyrics panel keeps its ordinary empty
/// hint.
pub fn load_for_song(
    path: &Path,
    base: DynamicBase,
    keep_seed: Option<u64>,
) -> Option<LoadedTexture> {
    let bytes = cover_bytes(path)?;
    let seed = seed_for_bytes(&bytes);
    if Some(seed) == keep_seed {
        return Some(LoadedTexture::Unchanged);
    }
    let decoded = image::load_from_memory(&bytes).ok()?;
    let decoded = if decoded.width() > MAX_SOURCE_EDGE || decoded.height() > MAX_SOURCE_EDGE {
        decoded.resize(MAX_SOURCE_EDGE, MAX_SOURCE_EDGE, imageops::FilterType::Lanczos3)
    } else {
        decoded
    };
    let rgb = decoded.to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);

    let mut px = Vec::with_capacity(w * h);
    for p in rgb.pixels() {
        px.push([p[0] as f32, p[1] as f32, p[2] as f32]);
    }

    let (size, out) = render_raster(&Image { w, h, px }, seed, base)?;
    Some(LoadedTexture::Rendered { seed, image: to_render_image(size, &out) })
}

/// The pure path: loaded RGB pixels in, square RGB pixels out.
fn render_raster(img: &Image, seed: u64, base: DynamicBase) -> Option<(usize, Vec<[u8; 3]>)> {
    if img.w < CHUNK_SIZE || img.h < CHUNK_SIZE {
        return None;
    }

    let mut rng = Rng::new(seed);
    let (hue, sat, marks) = resolve_palette(img);
    let chunks = sample_chunks(img, &mut rng);
    if chunks.is_empty() {
        return None;
    }
    let (size, canvas) = quilt(&chunks, &mut rng);

    // Quantize to u8, blur, then read the blurred texture back as f32 — the
    // same order as the reference, so the blur sees exactly the quilted
    // integer pixels.
    let q8: Vec<[u8; 3]> = canvas
        .iter()
        .map(|p| {
            [
                p[0].clamp(0.0, 255.0) as u8,
                p[1].clamp(0.0, 255.0) as u8,
                p[2].clamp(0.0, 255.0) as u8,
            ]
        })
        .collect();
    let blurred = gaussian_blur(&q8, size, size);

    let structure = posterize(&structure_map(&blurred), size, STRUCTURE_LEVELS, &bayer8());
    let hsv: Vec<(f32, f32, f32)> = blurred
        .iter()
        .map(|p| color::hsv(p[0], p[1], p[2]))
        .collect();
    let mark_maps: Vec<Vec<f32>> = marks
        .iter()
        .map(|&(mark_hue, _)| {
            let raw: Vec<f32> = hsv
                .iter()
                .map(|&(h, s, v)| accent_mask(h, s, v, mark_hue as f32))
                .collect();
            posterize(&raw, size, MARK_LEVELS, &bayer8())
        })
        .collect();

    // Palette remap: background -> ink by the structure map, then each accent
    // mark blended over the result.
    let params = ThemeParams::for_base(base);
    let bg = color::hls_rgb255(hue, params.bg_l, sat * params.bg_sat);
    let ink = color::hls_rgb255(hue, params.ink_l, sat * params.ink_sat);
    let mut out = vec![[0u8; 3]; structure.len()];
    for (i, dst) in out.iter_mut().enumerate() {
        let s = structure[i];
        let mut c = [0.0f32; 3];
        for ch in 0..3 {
            c[ch] = bg[ch] + (ink[ch] - bg[ch]) * s;
        }
        for (mark_index, &(mark_hue, mark_sat)) in marks.iter().enumerate() {
            let color = color::hls_rgb255(mark_hue, params.accent_l, mark_sat);
            let m = mark_maps[mark_index][i] * ACCENT_MARK_STRENGTH;
            for ch in 0..3 {
                c[ch] += (color[ch] - c[ch]) * m;
            }
        }
        *dst = [
            c[0].clamp(0.0, 255.0) as u8,
            c[1].clamp(0.0, 255.0) as u8,
            c[2].clamp(0.0, 255.0) as u8,
        ];
    }

    Some((size, out))
}

/// `RenderImage` expects BGRA, so swap R and B while flattening to RGBA.
fn to_render_image(size: usize, px: &[[u8; 3]]) -> Arc<RenderImage> {
    let mut rgba = Vec::with_capacity(size * size * 4);
    for p in px {
        rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    let buffer = image::RgbaImage::from_raw(size as u32, size as u32, rgba)
        .expect("texture buffer has the right dimensions");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

// ---- palette extraction ---------------------------------------------------

#[inline]
fn hue_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).abs();
    d.min(1.0 - d)
}

#[inline]
fn clamp_mark_sat(s: f64) -> f64 {
    if s > 0.0 {
        s.clamp(0.45, 0.85)
    } else {
        0.0
    }
}

fn extract_hue_candidates(img: &Image, bins: usize, max_candidates: usize) -> Vec<Candidate> {
    // Match the palette stage's 64x64 Lanczos thumbnail.
    let mut rgb = Vec::with_capacity(img.w * img.h * 3);
    for p in &img.px {
        rgb.push(p[0].clamp(0.0, 255.0) as u8);
        rgb.push(p[1].clamp(0.0, 255.0) as u8);
        rgb.push(p[2].clamp(0.0, 255.0) as u8);
    }
    let src = image::RgbImage::from_raw(img.w as u32, img.h as u32, rgb)
        .expect("source buffer has the right dimensions");
    let thumb = imageops::resize(&src, 64, 64, imageops::FilterType::Lanczos3);

    let n = 64 * 64;
    let mut hues = Vec::with_capacity(n);
    let mut lights = Vec::with_capacity(n);
    let mut sats = Vec::with_capacity(n);
    for p in thumb.pixels() {
        let (h, l, s) = color::rgb_to_hls(
            p[0] as f64 / 255.0,
            p[1] as f64 / 255.0,
            p[2] as f64 / 255.0,
        );
        hues.push(h);
        lights.push(l);
        sats.push(s);
    }
    let weights: Vec<f64> = (0..n)
        .map(|i| sats[i] * (1.0 - (2.0 * lights[i] - 1.0).abs()))
        .collect();
    let total: f64 = weights.iter().sum();
    if total < 1e-6 {
        return vec![Candidate { hue: 0.0, sat: 0.0 }];
    }

    let mut hist = vec![0.0f64; bins];
    for i in 0..n {
        let b = ((hues[i] * bins as f64) as usize) % bins;
        hist[b] += weights[i];
    }
    let raw = hist.clone();
    for i in 0..bins {
        hist[i] = (raw[(i + bins - 1) % bins] + 2.0 * raw[i] + raw[(i + 1) % bins]) / 4.0;
    }
    let hmax = hist.iter().cloned().fold(0.0, f64::max);
    let mut peaks: Vec<usize> = (0..bins)
        .filter(|&i| {
            hist[i] >= hist[(i + bins - 1) % bins]
                && hist[i] >= hist[(i + 1) % bins]
                && hist[i] > 0.05 * hmax
        })
        .collect();
    peaks.sort_by(|&a, &b| hist[b].total_cmp(&hist[a]).then(a.cmp(&b)));

    let mut chosen: Vec<(usize, Candidate)> = Vec::new();
    for i in peaks {
        if chosen
            .iter()
            .any(|(j, _)| (*j as isize - i as isize).unsigned_abs() % bins < 3)
        {
            continue;
        }
        let center = (i as f64 + 0.5) / bins as f64;
        let mut sw = 0.0;
        let mut ss = 0.0;
        let mut sin_sum = 0.0;
        let mut cos_sum = 0.0;
        for k in 0..n {
            let d = (hues[k] - center).abs();
            if d.min(1.0 - d) <= 0.06 {
                let w = weights[k];
                sw += w;
                ss += w * sats[k];
                let ang = hues[k] * 2.0 * std::f64::consts::PI;
                sin_sum += w * ang.sin();
                cos_sum += w * ang.cos();
            }
        }
        if sw < 1e-6 {
            continue;
        }
        let hue = (sin_sum.atan2(cos_sum) / (2.0 * std::f64::consts::PI)).rem_euclid(1.0);
        let sat = ss / sw;
        chosen.push((i, Candidate { hue, sat }));
        if chosen.len() >= max_candidates {
            break;
        }
    }
    if chosen.is_empty() {
        vec![Candidate { hue: 0.0, sat: 0.0 }]
    } else {
        chosen.into_iter().map(|(_, c)| c).collect()
    }
}

fn resolve_palette(img: &Image) -> (f64, f64, Vec<(f64, f64)>) {
    let cands = extract_hue_candidates(img, 36, NUM_ACCENTS + 2);
    let pick = cands[0];
    let main_sat = if pick.sat < GREYSCALE_SATURATION {
        0.0
    } else {
        pick.sat.clamp(0.25, 0.65)
    };
    let main_hue = pick.hue;

    let mut marks = vec![(main_hue, clamp_mark_sat(main_sat))];
    for c in &cands {
        if hue_dist(c.hue, main_hue) < MIN_HUE_SEPARATION {
            continue;
        }
        marks.push((c.hue, clamp_mark_sat(c.sat)));
        if marks.len() > NUM_ACCENTS {
            break;
        }
    }
    (main_hue, main_sat, marks)
}

// ---- quilting -------------------------------------------------------------

fn sample_chunks(img: &Image, rng: &mut Rng) -> Vec<Chunk> {
    let n = NUM_CHUNKS * CANDIDATE_OVERSAMPLE;
    let mut ys = vec![0u64; n];
    let mut xs = vec![0u64; n];
    rng.integers((img.h - CHUNK_SIZE + 1) as u64, &mut ys);
    rng.integers((img.w - CHUNK_SIZE + 1) as u64, &mut xs);

    let pos: Vec<(usize, usize)> = (0..n)
        .map(|i| (ys[i] as usize, xs[i] as usize))
        .collect();

    // Luminance std per candidate: drop the flattest patches.
    let gray: Vec<f32> = img
        .px
        .iter()
        .map(|p| (p[0] + p[1] + p[2]) / 3.0)
        .collect();
    let area = (CHUNK_SIZE * CHUNK_SIZE) as f64;
    let detail: Vec<f64> = pos
        .iter()
        .map(|&(y, x)| {
            let mut sum = 0.0f64;
            let mut sum2 = 0.0f64;
            for dy in 0..CHUNK_SIZE {
                let row = (y + dy) * img.w + x;
                for dx in 0..CHUNK_SIZE {
                    let v = gray[row + dx] as f64;
                    sum += v;
                    sum2 += v * v;
                }
            }
            let mean = sum / area;
            (sum2 / area - mean * mean).max(0.0).sqrt()
        })
        .collect();

    let mut order: Vec<usize> = (0..pos.len()).collect();
    order.sort_by(|&a, &b| detail[a].total_cmp(&detail[b]).then(a.cmp(&b)));
    let start = ((pos.len() as f64) * FLAT_DISCARD_FRACTION) as usize;
    let mut survivors: Vec<usize> = order[start..].to_vec();
    if survivors.len() > NUM_CHUNKS {
        let pick = rng.choice_without_replacement(survivors.len(), NUM_CHUNKS);
        survivors = pick.into_iter().map(|i| survivors[i]).collect();
    }

    let cells = [
        side_cells(Side::Left),
        side_cells(Side::Right),
        side_cells(Side::Top),
        side_cells(Side::Bottom),
    ];
    survivors
        .iter()
        .map(|&pi| {
            let (y, x) = pos[pi];
            let mut data = Vec::with_capacity(CHUNK_SIZE * CHUNK_SIZE * 3);
            for dy in 0..CHUNK_SIZE {
                for dx in 0..CHUNK_SIZE {
                    data.extend_from_slice(&img.at(y + dy, x + dx));
                }
            }
            let strips = [
                extract_strip(&data, &cells[0]),
                extract_strip(&data, &cells[1]),
                extract_strip(&data, &cells[2]),
                extract_strip(&data, &cells[3]),
            ];
            Chunk { data, strips }
        })
        .collect()
}

fn side_cells(side: Side) -> Vec<usize> {
    let mut v = Vec::new();
    for y in 0..CHUNK_SIZE {
        for x in 0..CHUNK_SIZE {
            if in_side(side, y, x) {
                v.push(y * CHUNK_SIZE + x);
            }
        }
    }
    v
}

#[inline]
fn in_side(side: Side, y: usize, x: usize) -> bool {
    match side {
        Side::Left => x < OVERLAP,
        Side::Right => x >= CHUNK_SIZE - OVERLAP,
        Side::Top => y < OVERLAP,
        Side::Bottom => y >= CHUNK_SIZE - OVERLAP,
    }
}

fn extract_strip(data: &[f32], cells: &[usize]) -> Vec<f32> {
    let mut out = Vec::with_capacity(cells.len() * 3);
    for &c in cells {
        out.extend_from_slice(&data[c * 3..c * 3 + 3]);
    }
    out
}

#[inline]
fn sqdiff_sum(a: &[f32], b: &[f32]) -> f32 {
    // Multiple accumulators expose ILP without relying on FP reassociation.
    let mut acc = [0.0f32; 8];
    let n = a.len();
    let mut it = 0;
    while it + 8 <= n {
        for j in 0..8 {
            let d = a[it + j] - b[it + j];
            acc[j] += d * d;
        }
        it += 8;
    }
    let mut tail = 0.0f32;
    while it < n {
        let d = a[it] - b[it];
        tail += d * d;
        it += 1;
    }
    acc[0] + acc[1] + acc[2] + acc[3] + acc[4] + acc[5] + acc[6] + acc[7] + tail
}

/// Cheapest vertical seam; true where the new patch is used (columns right of
/// the seam).
fn min_cut_mask(err: &[f64], h: usize, w: usize) -> Vec<bool> {
    let mut cost = err.to_vec();
    for i in 1..h {
        for j in 0..w {
            let lo = j.saturating_sub(1);
            let hi = (j + 2).min(w);
            let mut m = cost[(i - 1) * w + lo];
            for jj in lo + 1..hi {
                let v = cost[(i - 1) * w + jj];
                if v < m {
                    m = v;
                }
            }
            cost[i * w + j] += m;
        }
    }
    let mut path = vec![0usize; h];
    let last = &cost[(h - 1) * w..h * w];
    path[h - 1] = last
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(b.1))
        .unwrap()
        .0;
    for i in (0..h - 1).rev() {
        let j = path[i + 1];
        let lo = j.saturating_sub(1);
        let hi = (j + 2).min(w);
        let mut best = lo;
        for jj in lo + 1..hi {
            if cost[i * w + jj] < cost[i * w + best] {
                best = jj;
            }
        }
        path[i] = best;
    }
    let mut mask = vec![false; h * w];
    for i in 0..h {
        for j in 0..w {
            mask[i * w + j] = j >= path[i];
        }
    }
    mask
}

/// Seam mask for one overlap strip; `e` has the strip's (h, w) shape.
fn side_mask(e: &[f64], h: usize, w: usize, side: Side) -> Vec<bool> {
    match side {
        Side::Left => min_cut_mask(e, h, w),
        Side::Right => {
            let mut oriented = vec![0.0; h * w];
            for r in 0..h {
                for c in 0..w {
                    oriented[r * w + c] = e[r * w + (w - 1 - c)];
                }
            }
            let m = min_cut_mask(&oriented, h, w);
            let mut out = vec![false; h * w];
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = m[r * w + (w - 1 - c)];
                }
            }
            out
        }
        Side::Top => {
            let (oh, ow) = (w, h);
            let mut oriented = vec![0.0; oh * ow];
            for r in 0..h {
                for c in 0..w {
                    oriented[c * ow + r] = e[r * w + c];
                }
            }
            let m = min_cut_mask(&oriented, oh, ow);
            let mut out = vec![false; h * w];
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = m[c * ow + r];
                }
            }
            out
        }
        Side::Bottom => {
            let (oh, ow) = (w, h);
            let mut oriented = vec![0.0; oh * ow];
            for r in 0..h {
                for c in 0..w {
                    oriented[c * ow + r] = e[(h - 1 - r) * w + c];
                }
            }
            let m = min_cut_mask(&oriented, oh, ow);
            let mut out = vec![false; h * w];
            for r in 0..h {
                for c in 0..w {
                    out[r * w + c] = m[c * ow + (h - 1 - r)];
                }
            }
            out
        }
    }
}

fn quilt(chunks: &[Chunk], rng: &mut Rng) -> (usize, Vec<[f32; 3]>) {
    if chunks.is_empty() {
        return (0, Vec::new());
    }
    let step = CHUNK_SIZE - OVERLAP;
    let n = RENDER_SIZE / step;
    debug_assert!(n >= 2, "RENDER_SIZE too small for CHUNK_SIZE / OVERLAP");
    let size = n * step;
    let mut canvas = vec![[0.0f32; 3]; size * size];
    let k = BEST_K.min(chunks.len());

    let cells_all = [
        side_cells(Side::Left),
        side_cells(Side::Right),
        side_cells(Side::Top),
        side_cells(Side::Bottom),
    ];
    let cell_count = CHUNK_SIZE * CHUNK_SIZE;
    let mut block = vec![0.0f32; cell_count * 3];
    let mut use_new = vec![false; cell_count];

    for gy in 0..n {
        for gx in 0..n {
            let rows: Vec<usize> = (0..CHUNK_SIZE).map(|s| (gy * step + s) % size).collect();
            let cols: Vec<usize> = (0..CHUNK_SIZE).map(|s| (gx * step + s) % size).collect();
            for (y, &ry) in rows.iter().enumerate() {
                let src = ry * size;
                for (x, &cx) in cols.iter().enumerate() {
                    let p = canvas[src + cx];
                    let i = (y * CHUNK_SIZE + x) * 3;
                    block[i] = p[0];
                    block[i + 1] = p[1];
                    block[i + 2] = p[2];
                }
            }

            let mut sides: Vec<Side> = Vec::with_capacity(4);
            if gx > 0 {
                sides.push(Side::Left);
            }
            if gx == n - 1 {
                sides.push(Side::Right); // wraps onto column 0
            }
            if gy > 0 {
                sides.push(Side::Top);
            }
            if gy == n - 1 {
                sides.push(Side::Bottom); // wraps onto row 0
            }

            let patch: &Chunk;
            if sides.is_empty() {
                patch = &chunks[rng.below(chunks.len() as u64) as usize];
                block.copy_from_slice(&patch.data);
            } else {
                let block_strips: Vec<Vec<f32>> = sides
                    .iter()
                    .map(|&side| extract_strip(&block, &cells_all[side.index()]))
                    .collect();

                // Rank every candidate by the summed overlap error and pick
                // randomly among the K best. (The reference pre-filters this
                // for speed; ranking all candidates chooses the same patches.)
                let mut order: Vec<(f32, usize)> = (0..chunks.len())
                    .map(|i| {
                        let mut err = 0.0f32;
                        for (j, &side) in sides.iter().enumerate() {
                            err += sqdiff_sum(&chunks[i].strips[side.index()], &block_strips[j]);
                        }
                        (err, i)
                    })
                    .collect();
                order.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let best = &order[..k];
                patch = &chunks[best[rng.below(k as u64) as usize].1];

                use_new.iter_mut().for_each(|v| *v = true);
                for &side in &sides {
                    let cells = &cells_all[side.index()];
                    let (h, w) = match side {
                        Side::Left | Side::Right => (CHUNK_SIZE, OVERLAP),
                        Side::Top | Side::Bottom => (OVERLAP, CHUNK_SIZE),
                    };
                    let mut e = vec![0.0f64; cells.len()];
                    for (kk, &cell) in cells.iter().enumerate() {
                        let i = cell * 3;
                        let mut d = 0.0f64;
                        for ch in 0..3 {
                            let d0 = (block[i + ch] - patch.data[i + ch]) as f64;
                            d += d0 * d0;
                        }
                        e[kk] = d;
                    }
                    let mask = side_mask(&e, h, w, side);
                    for (kk, &cell) in cells.iter().enumerate() {
                        use_new[cell] &= mask[kk];
                    }
                }
                for (cell, &new) in use_new.iter().enumerate() {
                    if new {
                        let i = cell * 3;
                        block[i] = patch.data[i];
                        block[i + 1] = patch.data[i + 1];
                        block[i + 2] = patch.data[i + 2];
                    }
                }
            }

            for (y, &ry) in rows.iter().enumerate() {
                let dst = ry * size;
                for (x, &cx) in cols.iter().enumerate() {
                    let i = (y * CHUNK_SIZE + x) * 3;
                    canvas[dst + cx] = [block[i], block[i + 1], block[i + 2]];
                }
            }
        }
    }
    (size, canvas)
}

// ---- post-processing ------------------------------------------------------

fn gaussian_blur(src: &[[u8; 3]], w: usize, h: usize) -> Vec<[f32; 3]> {
    // Exact port of Pillow's ImagingGaussianBlur: three fractional box-blur
    // passes per axis with the same fixed-point rounding, padded with wrap so
    // the texture stays tileable.
    if BLUR_RADIUS <= 0.0 {
        return src.iter().map(|p| [p[0] as f32, p[1] as f32, p[2] as f32]).collect();
    }
    let passes = 3i32;
    let fr = gaussian_blur_radius(BLUR_RADIUS, passes);
    let radius = fr as i32;
    let ww = ((1u32 << 24) as f32 / (fr * 2.0 + 1.0)) as u32;
    let fw = ((1u32 << 24) - (radius * 2 + 1) as u32 * ww) / 2;

    let pad = (BLUR_RADIUS * 3.0).ceil() as usize;
    let pw = w + pad * 2;
    let ph = h + pad * 2;
    let mut a = vec![[0u8; 3]; pw * ph];
    for y in 0..ph {
        let sy = (y as isize - pad as isize).rem_euclid(h as isize) as usize;
        for x in 0..pw {
            let sx = (x as isize - pad as isize).rem_euclid(w as isize) as usize;
            a[y * pw + x] = src[sy * w + sx];
        }
    }

    let mut b = vec![[0u8; 3]; pw * ph];
    blur_rows(&mut a, &mut b, pw, radius, ww, fw, passes as usize);

    let mut t = transpose3(&a, pw, ph);
    let mut tb = vec![[0u8; 3]; pw * ph];
    blur_rows(&mut t, &mut tb, ph, radius, ww, fw, passes as usize);
    a = transpose3(&t, ph, pw);

    let mut out = vec![[0.0f32; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let p = a[(y + pad) * pw + x + pad];
            out[y * w + x] = [p[0] as f32, p[1] as f32, p[2] as f32];
        }
    }
    out
}

fn blur_rows(
    a: &mut Vec<[u8; 3]>,
    b: &mut Vec<[u8; 3]>,
    w: usize,
    radius: i32,
    ww: u32,
    fw: u32,
    passes: usize,
) {
    for _ in 0..passes {
        b.chunks_mut(w)
            .enumerate()
            .for_each(|(y, row)| blur_line(&a[y * w..y * w + w], row, radius, ww, fw));
        std::mem::swap(a, b);
    }
}

fn transpose3(src: &[[u8; 3]], w: usize, h: usize) -> Vec<[u8; 3]> {
    let mut dst = vec![[0u8; 3]; w * h];
    const B: usize = 32;
    for by in (0..h).step_by(B) {
        for bx in (0..w).step_by(B) {
            for y in by..(by + B).min(h) {
                for x in bx..(bx + B).min(w) {
                    dst[x * h + y] = src[y * w + x];
                }
            }
        }
    }
    dst
}

fn gaussian_blur_radius(radius: f32, passes: i32) -> f32 {
    let sigma2 = radius * radius / passes as f32;
    let l_len = (12.0 * sigma2 + 1.0).sqrt();
    let l = ((l_len - 1.0) / 2.0).floor();
    let a = (2.0 * l + 1.0) * (l * (l + 1.0) - 3.0 * sigma2)
        / (6.0 * (sigma2 - (l + 1.0) * (l + 1.0)));
    l + a
}

fn blur_line(line_in: &[[u8; 3]], line_out: &mut [[u8; 3]], radius: i32, ww: u32, fw: u32) {
    let lastx = line_in.len() as i32 - 1;
    let edge_a = (radius + 1).min(lastx + 1);

    let mut acc = [0u32; 3];
    for c in 0..3 {
        acc[c] = line_in[0][c] as u32 * (radius + 1) as u32;
    }
    for x in 0..(edge_a - 1) {
        for c in 0..3 {
            acc[c] = acc[c].wrapping_add(line_in[x as usize][c] as u32);
        }
    }
    let remain = (radius - edge_a + 1) as u32;
    for c in 0..3 {
        acc[c] = acc[c].wrapping_add(line_in[lastx as usize][c] as u32 * remain);
    }

    let save = |x: i32, acc: &mut [u32; 3], out: &mut [[u8; 3]]| {
        let left = if x - radius - 1 < 0 { 0 } else { (x - radius - 1) as usize };
        let right = if x + radius + 1 > lastx {
            lastx as usize
        } else {
            (x + radius + 1) as usize
        };
        for c in 0..3 {
            let edges = (line_in[left][c] as u32 + line_in[right][c] as u32).wrapping_mul(fw);
            let bulk = acc[c].wrapping_mul(ww).wrapping_add(edges);
            out[x as usize][c] = ((bulk.wrapping_add(1 << 23)) >> 24) as u8;
        }
    };

    // Pillow's edge handling: move the accumulator, then save. Over every x the
    // removed sample is the one leaving the window (clamped to the first) and
    // the added sample is the one entering it (clamped to the last), which is
    // all six of the original edge/body/edge ranges collapsed into one.
    for x in 0..=lastx {
        let sub = line_in[(x - radius - 1).max(0) as usize];
        let add = line_in[(x + radius).min(lastx) as usize];
        for c in 0..3 {
            acc[c] = acc[c].wrapping_add(add[c] as u32).wrapping_sub(sub[c] as u32);
        }
        save(x, &mut acc, line_out);
    }
}

fn structure_map(texture: &[[f32; 3]]) -> Vec<f32> {
    let n = texture.len();
    let mut background = [0.0f32; 3];
    let mid = n / 2;
    for ch in 0..3 {
        let mut c: Vec<f32> = texture.iter().map(|p| p[ch]).collect();
        c.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        background[ch] = if n % 2 == 0 {
            let lo = c[..mid].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            (lo + c[mid]) / 2.0
        } else {
            c[mid]
        };
    }
    let mut dist: Vec<f32> = texture
        .iter()
        .map(|p| {
            let d0 = p[0] - background[0];
            let d1 = p[1] - background[1];
            let d2 = p[2] - background[2];
            (d0 * d0 + d1 * d1 + d2 * d2).sqrt()
        })
        .collect();

    let mut sorted = dist.clone();
    let scale = percentile_select(&mut sorted, 99.0);
    if scale < 1e-6 {
        return vec![0.0; dist.len()];
    }
    dist.iter_mut().for_each(|v| {
        *v = (*v / scale).clamp(0.0, 1.0).powf(STRUCTURE_GAMMA);
    });
    dist
}

fn percentile_select(v: &mut [f32], q: f64) -> f32 {
    let n = v.len();
    let idx = (n as f64 - 1.0) * q / 100.0;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        v.select_nth_unstable_by(lo, |a, b| a.total_cmp(b));
        return v[lo];
    }
    v.select_nth_unstable_by(lo, |a, b| a.total_cmp(b));
    let a = v[lo];
    v[lo + 1..].select_nth_unstable_by(hi - lo - 1, |x, y| x.total_cmp(y));
    let b = v[hi];
    let frac = (idx - lo as f64) as f32;
    a + (b - a) * frac
}

fn accent_mask(h: f32, sat: f32, mx: f32, hue: f32) -> f32 {
    let d0 = (h - hue).abs();
    let d = d0.min(1.0 - d0);
    (1.0 - d / ACCENT_MARK_HUE_WIDTH).clamp(0.0, 1.0)
        * (sat / 0.5).clamp(0.0, 1.0)
        * ((mx - 0.2) / 0.3).clamp(0.0, 1.0)
}

fn bayer8() -> [[f32; 8]; 8] {
    let mut size = 1;
    let mut mtx = vec![vec![0u32]];
    while size < 8 {
        let mut next = vec![vec![0u32; size * 2]; size * 2];
        for i in 0..size {
            for j in 0..size {
                let v = mtx[i][j] * 4;
                next[i][j] = v;
                next[i][j + size] = v + 2;
                next[i + size][j] = v + 3;
                next[i + size][j + size] = v + 1;
            }
        }
        mtx = next;
        size *= 2;
    }
    let mut out = [[0.0f32; 8]; 8];
    for i in 0..8 {
        for j in 0..8 {
            out[i][j] = (mtx[i][j] as f32 + 0.5) / 64.0;
        }
    }
    out
}

fn posterize(v: &[f32], w: usize, levels: i32, bayer: &[[f32; 8]; 8]) -> Vec<f32> {
    if levels < 2 {
        return v.to_vec();
    }
    (0..v.len())
        .map(|i| {
            let val = v[i];
            let y = i / w;
            let x = i % w;
            let t = bayer[y % 8][x % 8];
            let q = (val * (levels - 1) as f32 + t).floor();
            q.clamp(0.0, (levels - 1) as f32) / (levels - 1) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A colourful test cover: hue bands plus a little texture, so both the
    /// palette and the quilting have something to work with.
    fn cover(w: usize, h: usize) -> Image {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let t = x as f32 / w as f32;
                let wave = ((x * 7 + y * 13) % 31) as f32 / 31.0;
                px.push([
                    80.0 + 150.0 * t + 20.0 * wave,
                    40.0 + 80.0 * (1.0 - t) + 70.0 * wave,
                    60.0 + 120.0 * (0.5 + 0.5 * wave),
                ]);
            }
        }
        Image { w, h, px }
    }

    #[test]
    fn renders_a_square_in_the_expected_size() {
        let (size, out) = render_raster(&cover(160, 128), 1234, DynamicBase::Black).unwrap();
        let step = CHUNK_SIZE - OVERLAP;
        let expected = (RENDER_SIZE / step) * step;
        assert_eq!(size, expected, "rounded down to a multiple of CHUNK_SIZE - OVERLAP");
        assert_eq!(out.len(), size * size);
    }

    #[test]
    fn the_same_cover_bytes_give_the_same_seed() {
        assert_eq!(seed_for_bytes(b"cover-a"), seed_for_bytes(b"cover-a"));
        assert_ne!(seed_for_bytes(b"cover-a"), seed_for_bytes(b"cover-b"));
    }

    #[test]
    fn the_same_cover_and_base_always_render_the_same_texture() {
        let img = cover(160, 128);
        let a = render_raster(&img, 99, DynamicBase::Black).unwrap().1;
        let b = render_raster(&img, 99, DynamicBase::Black).unwrap().1;
        assert_eq!(a, b);
    }

    #[test]
    fn white_and_black_bases_pick_different_lightness() {
        let img = cover(160, 128);
        let black = render_raster(&img, 7, DynamicBase::Black).unwrap().1;
        let white = render_raster(&img, 7, DynamicBase::White).unwrap().1;
        let mean = |px: &[[u8; 3]]| {
            px.iter()
                .map(|p| (p[0] as f64 + p[1] as f64 + p[2] as f64) / 3.0)
                .sum::<f64>()
                / px.len() as f64
        };
        assert!(mean(&black) < mean(&white));
        assert_ne!(black, white);
    }
}
