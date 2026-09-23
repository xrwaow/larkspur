//! One-pass track analysis: waveform peaks, the frequency spectrogram the
//! visualizer draws from, and the live-bitrate profile.
//!
//! These used to be two passes — a full decode for the waveform and a separate
//! demux for the bitrate. They're computed together here, because the packets
//! are already being read for the decode, so the bitrate is just a by-product.
//! Opening a track now costs one pass instead of two.
//!
//! Opus is the exception to "one decoder": symphonia 0.5 demuxes Ogg/Opus but
//! ships no Opus decoder, so those packets go to libopus (see `opus.rs`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::bitrate::{units_to_duration, BitrateProfile};
use crate::decode::{open_track, PacketDecoder};

/// Samples per waveform peak, before bucketing down.
const PEAK_WINDOW: usize = 512;

/// FFT size per spectrogram column: ~93 ms of audio at 44.1 kHz. Long enough
/// to resolve the bass end (bins ~10.8 Hz apart), short enough to track the
/// beat; columns overlap because the hop between them is shorter.
const FFT_SIZE: usize = 4096;

/// Spectrogram columns per second of media time — the visualizer's time
/// resolution.
pub const SPECTRO_COLUMNS_PER_SEC: f32 = 20.0;

/// Log-spaced frequency bands per spectrogram column, low → high. Also the
/// visualizer's bar count: one bar per band, bass on the left.
pub const BAND_COUNT: usize = 128;

/// The band edges span this range, geometrically spaced so the bass (where
/// music lives on a linear scale) doesn't crush into the leftmost bars.
const F_MIN: f32 = 40.0;
const F_MAX: f32 = 16_000.0;

/// Everything the UI wants from a track, from a single pass.
pub struct TrackAnalysis {
    /// Normalized (0.0–1.0) waveform peaks. `Arc` so views clone cheaply.
    pub peaks: Arc<Vec<f32>>,
    /// FFT magnitudes over media time — the visualizer's spectrum. `Arc` so
    /// views clone cheaply.
    pub spectrogram: Arc<Spectrogram>,
    pub bitrate: BitrateProfile,
}

/// FFT magnitudes over media time: one column per `1/columns_per_sec`
/// seconds, each holding [`BAND_COUNT`] log-spaced frequency bands (low →
/// high), normalized to 0.0–1.0 with a sqrt curve so the mid field stays
/// visible next to the bass, which dominates raw FFT energy.
pub struct Spectrogram {
    pub columns_per_sec: f32,
    pub columns: Vec<[f32; BAND_COUNT]>,
}

impl Spectrogram {
    /// The band magnitudes at `secs` into the track, interpolated between the
    /// two surrounding columns so the display slides smoothly. Past the end
    /// (and before the start) it holds the nearest column; `None` only when
    /// the track produced no spectrogram at all.
    pub fn at(&self, secs: f32) -> Option<[f32; BAND_COUNT]> {
        let last = self.columns.len().checked_sub(1)?;
        let x = (secs * self.columns_per_sec).max(0.0);
        let index = x.floor();
        if index >= last as f32 {
            return Some(self.columns[last]);
        }
        let index = index as usize;
        let frac = x - index as f32;
        let a = self.columns[index];
        let b = self.columns[(index + 1).min(last)];
        let mut out = [0.0; BAND_COUNT];
        for (o, (a, b)) in out.iter_mut().zip(a.iter().zip(b)) {
            *o = a + (b - a) * frac;
        }
        Some(out)
    }
}

/// Analyze `path` in one pass: `buckets` normalized waveform peaks, a
/// spectrogram for the visualizer, and a bitrate profile bucketed into
/// `bucket`-sized slices of media time.
pub fn analyze_track(
    path: &Path,
    buckets: usize,
    bucket: Duration,
) -> anyhow::Result<TrackAnalysis> {
    let opened = open_track(path, false)?;
    let track_id = opened.track_id;
    let time_base = opened.time_base();
    let sample_rate = opened.sample_rate();
    let channels = opened.channels();

    // One decoder or the other, chosen by the shared `PacketDecoder` — Opus
    // goes to libopus, everything else to symphonia.
    let mut decoder = PacketDecoder::new(&opened.codec_params, channels)?;
    let mut format = opened.format;

    let bucket_secs = bucket.as_secs_f64().max(1e-6);
    let mut peaks: Vec<f32> = Vec::new();
    let mut byte_counts: Vec<u64> = Vec::new();
    let mut spectro = SpectrogramBuilder::new(sample_rate.unwrap_or(44_100));

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // End of stream (or a corrupt packet): stop; what we have covers
            // everything readable.
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }

        // Bitrate: bucket the compressed bytes by the packet's start time.
        let start = units_to_duration(packet.ts(), time_base, sample_rate);
        let index = (start.as_secs_f64() / bucket_secs).floor() as usize;
        if byte_counts.len() <= index {
            byte_counts.resize(index + 1, 0);
        }
        byte_counts[index] += packet.data.len() as u64;

        // Waveform: decode this packet to samples and fold them into peaks.
        if let Some(samples) = decoder.decode(&packet) {
            push_peaks(samples, &mut peaks);
            spectro.push(samples, channels);
        }
    }

    Ok(TrackAnalysis {
        peaks: Arc::new(bucket_peaks(&peaks, buckets)),
        spectrogram: Arc::new(spectro.finish()),
        bitrate: BitrateProfile::from_byte_counts(bucket, byte_counts),
    })
}

/// Per-[`PEAK_WINDOW`]-sample interleaved peaks.
fn push_peaks(samples: &[f32], peaks: &mut Vec<f32>) {
    for chunk in samples.chunks(PEAK_WINDOW) {
        peaks.push(chunk.iter().fold(0f32, |m, &s| m.max(s.abs())));
    }
}

/// Streams decoded (interleaved) samples into spectrogram columns: a rolling
/// mono buffer, Hann-windowed and transformed once per column.
///
/// Columns sit on a fixed time grid — one every `1/columns_per_sec` seconds —
/// so a column's position in the track is known without counting samples. When
/// the hop between columns exceeds the window (high sample rates), the surplus
/// samples are skipped rather than letting the grid drift.
struct SpectrogramBuilder {
    sample_rate: f32,
    /// Mono samples not yet consumed into columns.
    pending: Vec<f32>,
    /// Samples to drop before the next column's window starts (the hop).
    skip: usize,
    hann: Vec<f32>,
    /// Per band: the inclusive FFT-bin range it folds, low → high.
    bands: Vec<(usize, usize)>,
    re: Vec<f32>,
    im: Vec<f32>,
    columns: Vec<[f32; BAND_COUNT]>,
    /// The loudest raw magnitude seen, for the final normalization.
    max: f32,
}

impl SpectrogramBuilder {
    fn new(sample_rate: u32) -> Self {
        let hann = (0..FFT_SIZE)
            .map(|i| {
                0.5 * (1.0 - (std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32).cos())
            })
            .collect();
        // Geometric band edges from F_MIN to F_MAX (capped at Nyquist), mapped
        // to FFT bins. A band always covers at least one bin, and never runs
        // past the last one.
        let f_max = F_MAX.min(sample_rate as f32 / 2.0);
        let ratio = (f_max / F_MIN).max(1.0);
        let bin = |hz: f32| hz * FFT_SIZE as f32 / sample_rate as f32;
        let bands = (0..BAND_COUNT)
            .map(|b| {
                let lo = F_MIN * ratio.powf(b as f32 / BAND_COUNT as f32);
                let hi = F_MIN * ratio.powf((b + 1) as f32 / BAND_COUNT as f32);
                let start = (bin(lo).floor() as usize).min(FFT_SIZE / 2 - 1);
                let end = ((bin(hi).ceil() as usize).max(start + 1)).min(FFT_SIZE / 2);
                (start, end)
            })
            .collect();
        Self {
            sample_rate: sample_rate as f32,
            pending: Vec::new(),
            skip: 0,
            hann,
            bands,
            re: vec![0.0; FFT_SIZE],
            im: vec![0.0; FFT_SIZE],
            columns: Vec::new(),
            max: 0.0,
        }
    }

    /// Fold one packet of interleaved samples in, emitting every full column.
    fn push(&mut self, samples: &[f32], channels: usize) {
        let channels = channels.max(1);
        self.pending.extend(
            samples
                .chunks(channels)
                .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32),
        );
        let hop = ((self.sample_rate / SPECTRO_COLUMNS_PER_SEC) as usize).max(1);
        loop {
            if self.skip > 0 {
                let drop = self.skip.min(self.pending.len());
                self.pending.drain(..drop);
                self.skip -= drop;
                if self.skip > 0 {
                    break; // the gap runs past what we've buffered so far
                }
            }
            if self.pending.len() < FFT_SIZE {
                break;
            }
            for (i, &s) in self.pending[..FFT_SIZE].iter().enumerate() {
                self.re[i] = s * self.hann[i];
                self.im[i] = 0.0;
            }
            fft_in_place(&mut self.re, &mut self.im);
            let mut column = [0.0; BAND_COUNT];
            for (band, &(start, end)) in self.bands.iter().enumerate() {
                let magnitude = self.re[start..end]
                    .iter()
                    .zip(&self.im[start..end])
                    .fold(0f32, |m, (&r, &i)| m.max((r * r + i * i).sqrt()));
                column[band] = magnitude;
                if magnitude > self.max {
                    self.max = magnitude;
                }
            }
            self.columns.push(column);
            self.skip = hop;
        }
    }

    /// Normalize every band against the track's loudest raw magnitude, with a
    /// sqrt curve so the quiet bands stay visible.
    fn finish(self) -> Spectrogram {
        let max = self.max.max(1e-6);
        let columns = self
            .columns
            .into_iter()
            .map(|c| c.map(|v| (v / max).sqrt()))
            .collect();
        Spectrogram { columns_per_sec: SPECTRO_COLUMNS_PER_SEC, columns }
    }
}

/// In-place iterative radix-2 Cooley–Tukey FFT, `n` a power of two.
fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert_eq!(n, im.len());
    debug_assert!(n.is_power_of_two());

    // Bit-reversal permutation.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (step_re, step_im) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut w_re, mut w_im) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let a = start + k;
                let b = a + len / 2;
                let t_re = re[b] * w_re - im[b] * w_im;
                let t_im = re[b] * w_im + im[b] * w_re;
                re[b] = re[a] - t_re;
                im[b] = im[a] - t_im;
                re[a] += t_re;
                im[a] += t_im;
                let next_re = w_re * step_re - w_im * step_im;
                w_im = w_re * step_im + w_im * step_re;
                w_re = next_re;
            }
        }
        len <<= 1;
    }
}

/// Reduce raw peaks to `buckets` values, normalized so the loudest is 1.0.
fn bucket_peaks(peaks: &[f32], buckets: usize) -> Vec<f32> {
    if peaks.is_empty() {
        return vec![0.0; buckets];
    }
    let chunk_size = ((peaks.len() as f32 / buckets as f32).ceil() as usize).max(1);
    let bucketed: Vec<f32> = peaks
        .chunks(chunk_size)
        .map(|c| c.iter().copied().fold(0f32, f32::max))
        .collect();

    let max = bucketed.iter().copied().fold(0f32, f32::max).max(0.0001);
    bucketed
        .into_iter()
        .map(|v| v / max)
        .chain(std::iter::repeat(0.0))
        .take(buckets)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_normalized_and_padded() {
        let peaks = [0.25, 0.5, 0.75, 1.0];
        let normalized = bucket_peaks(&peaks, 2);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[1], 1.0, "loudest bucket normalizes to 1.0");
        assert!(normalized[0] < normalized[1]);
    }

    #[test]
    fn short_input_is_padded_to_the_bucket_count() {
        let normalized = bucket_peaks(&[1.0], 5);
        assert_eq!(normalized.len(), 5);
        assert_eq!(normalized[0], 1.0);
        assert!(normalized[1..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn empty_input_is_all_zero() {
        assert_eq!(bucket_peaks(&[], 3), vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn fft_matches_the_naive_dft() {
        // A small random signal, compared bin-by-bin against the O(n²) DFT.
        let n = 64;
        let re: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() + (i as f32 * 1.7).cos() * 0.3).collect();
        let mut im = vec![0.0f32; n];
        let mut spectrum = re.clone();
        fft_in_place(&mut spectrum, &mut im);
        for k in 0..n {
            let (expect_re, expect_im) = (0..n).fold((0.0f32, 0.0f32), |(er, ei), t| {
                let ang = -std::f32::consts::TAU * t as f32 * k as f32 / n as f32;
                (er + re[t] * ang.cos(), ei + re[t] * ang.sin())
            });
            assert!((spectrum[k] - expect_re).abs() < 1e-3, "bin {k} real");
            assert!((im[k] - expect_im).abs() < 1e-3, "bin {k} imag");
        }
    }

    #[test]
    fn a_low_tone_peaks_in_the_low_bands_and_a_high_tone_in_the_high_bands() {
        // One column of a 100 Hz sine, then of an 8 kHz sine: the energy must
        // land in the left (low) bands and the right (high) bands respectively.
        let sample_rate = 44_100u32;
        let tone = |hz: f32| {
            (0..FFT_SIZE)
                .map(|i| (std::f32::consts::TAU * hz * i as f32 / sample_rate as f32).sin())
                .collect::<Vec<f32>>()
        };
        let band_of = |samples: Vec<f32>| {
            let mut spectro = SpectrogramBuilder::new(sample_rate);
            spectro.push(&samples, 1);
            let spectro = spectro.finish();
            let column = spectro.at(0.0).unwrap();
            let (index, _) = column.iter().enumerate().fold(
                (0usize, 0f32),
                |(bi, bv), (i, &v)| if v > bv { (i, v) } else { (bi, bv) },
            );
            (index, column)
        };

        let (low_band, low) = band_of(tone(100.0));
        // 100 Hz sits in band ~9 of the 40 Hz–16 kHz geometric ladder.
        assert!((7..=12).contains(&low_band), "100 Hz in band {low_band}");
        assert!(low[low_band] > 0.9, "the tone's band normalizes to ~1.0");
        assert!(low[40..].iter().all(|&v| v < 0.05), "no energy in the highs");

        let (high_band, high) = band_of(tone(8_000.0));
        assert!((52..=57).contains(&high_band), "8 kHz in band {high_band}");
        assert!(high[..30].iter().all(|&v| v < 0.05), "the tone leaves the bass alone");
    }

    #[test]
    fn spectrogram_at_interpolates_between_columns() {
        let spectro = Spectrogram {
            columns_per_sec: SPECTRO_COLUMNS_PER_SEC,
            columns: vec![[0.0; BAND_COUNT], [1.0; BAND_COUNT], [1.0; BAND_COUNT]],
        };
        let mid = spectro.at(0.5 / SPECTRO_COLUMNS_PER_SEC).unwrap();
        assert!((mid[0] - 0.5).abs() < 0.01, "{}", mid[0]);
        assert_eq!(spectro.at(0.0).unwrap()[0], 0.0);
        assert_eq!(spectro.at(100.0).unwrap()[0], 1.0, "past the end holds the last column");
    }

    #[test]
    fn an_empty_spectrogram_has_no_columns() {
        let spectro = SpectrogramBuilder::new(44_100).finish();
        assert!(spectro.at(0.0).is_none());
    }

    #[test]
    fn peaks_group_every_peak_window() {
        let samples = vec![1.0f32; PEAK_WINDOW + 1];
        let mut peaks = Vec::new();
        push_peaks(&samples, &mut peaks);
        assert_eq!(peaks.len(), 2, "one full window plus a partial one");
        assert!(peaks.iter().all(|&p| p == 1.0));
    }
}
