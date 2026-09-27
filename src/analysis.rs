//! Track analysis: the sequential waveform/bitrate pass the transport draws
//! from, and the reusable spectrum core the real-time visualizer runs on the
//! live audio.
//!
//! The waveform is no longer a single whole-file pass. [`TrackAnalyzer`]
//! decodes a track from the start in bounded steps, folding decoded samples
//! into a fixed number of waveform buckets spread over the track's media time
//! and bucketing packet bytes into a bitrate profile. A caller advances it a
//! slice at a time and publishes the partial [`Waveform`] as it grows, so the
//! transport's bars fill in as the song plays instead of appearing all at once
//! after a full decode.
//!
//! The visualizer no longer reads a precomputed spectrogram: it runs
//! [`SpectrumAnalyzer`] over the samples the audio thread is actually playing
//! (see `audio::SampleTap`), so its bars are genuinely real-time.
//!
//! Opus is the exception to "one decoder": symphonia 0.5 demuxes Ogg/Opus but
//! ships no Opus decoder, so those packets go to libopus (see `opus.rs`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use symphonia::core::units::TimeBase;

use crate::bitrate::{units_to_duration, BitrateProfile};
use crate::decode::{open_track, OpenedTrack, PacketDecoder};

/// Samples per waveform peak, before bucketing down.
const PEAK_WINDOW: usize = 512;

/// FFT size for one spectrum frame: ~93 ms of audio at 44.1 kHz. Long enough
/// to resolve the bass end (bins ~10.8 Hz apart), short enough to track the
/// beat.
pub const FFT_SIZE: usize = 4096;

/// Log-spaced frequency bands per spectrum frame, low → high. Also the
/// visualizer's bar count: one bar per band, bass on the left.
pub const BAND_COUNT: usize = 128;

/// The band edges span this range, geometrically spaced so the bass (where
/// music lives on a linear scale) doesn't crush into the leftmost bars.
const F_MIN: f32 = 40.0;
const F_MAX: f32 = 16_000.0;

/// A progressively-generated waveform: `buckets` peak values spread evenly
/// over the track's media time, of which the first `generated` are filled in.
/// `max` is the loudest peak seen so far, so a caller can auto-scale the
/// display as the bars fill in.
#[derive(Clone)]
pub struct Waveform {
    pub buckets: Vec<f32>,
    pub generated: usize,
    pub max: f32,
}

impl Waveform {
    /// An empty waveform of `buckets` slots — nothing generated yet.
    pub fn empty(buckets: usize) -> Self {
        Self { buckets: vec![0.0; buckets], generated: 0, max: 0.0 }
    }

    /// Every bucket, normalized against the running max.
    pub fn normalized_all(&self) -> Vec<f32> {
        let max = self.max.max(1e-6);
        self.buckets.iter().map(|v| v / max).collect()
    }
}

/// Turns a window of mono samples into [`BAND_COUNT`] log-spaced band
/// magnitudes via a Hann-windowed FFT.
///
/// Built once per sample rate and reused frame to frame — the window, the band
/// edges, and the scratch buffers are all precomputed, so a frame costs one
/// FFT and one fold. This is what makes the visualizer real-time: it runs on
/// whatever the audio thread last played, not on a precomputed spectrogram.
pub struct SpectrumAnalyzer {
    hann: Vec<f32>,
    /// Per band: the inclusive FFT-bin range it folds, low → high.
    bands: Vec<(usize, usize)>,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1) as f32;
        let hann = (0..FFT_SIZE)
            .map(|i| {
                0.5 * (1.0 - (std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32).cos())
            })
            .collect();
        // Geometric band edges from F_MIN to F_MAX (capped at Nyquist), mapped
        // to FFT bins. A band always covers at least one bin, and never runs
        // past the last one.
        let f_max = F_MAX.min(sample_rate / 2.0);
        let ratio = (f_max / F_MIN).max(1.0);
        let bin = |hz: f32| hz * FFT_SIZE as f32 / sample_rate;
        let bands = (0..BAND_COUNT)
            .map(|b| {
                let lo = F_MIN * ratio.powf(b as f32 / BAND_COUNT as f32);
                let hi = F_MIN * ratio.powf((b + 1) as f32 / BAND_COUNT as f32);
                let start = (bin(lo).floor() as usize).min(FFT_SIZE / 2 - 1);
                let end = ((bin(hi).ceil() as usize).max(start + 1)).min(FFT_SIZE / 2);
                (start, end)
            })
            .collect();
        Self { hann, bands, re: vec![0.0; FFT_SIZE], im: vec![0.0; FFT_SIZE] }
    }

    /// Raw band magnitudes for `mono`, the most recent samples (newest last).
    /// A slice shorter than [`FFT_SIZE`] is zero-padded at the front, so the
    /// newest sample stays at the end of the window.
    pub fn bands(&mut self, mono: &[f32]) -> [f32; BAND_COUNT] {
        let take = mono.len().min(FFT_SIZE);
        let pad = FFT_SIZE - take;
        for slot in &mut self.re[..pad] {
            *slot = 0.0;
        }
        for (i, &sample) in mono[mono.len() - take..].iter().enumerate() {
            self.re[pad + i] = sample * self.hann[pad + i];
        }
        self.im.iter_mut().for_each(|v| *v = 0.0);
        fft_in_place(&mut self.re, &mut self.im);

        let mut column = [0.0; BAND_COUNT];
        for (band, &(start, end)) in self.bands.iter().enumerate() {
            column[band] = self.re[start..end]
                .iter()
                .zip(&self.im[start..end])
                .fold(0f32, |m, (&r, &i)| m.max((r * r + i * i).sqrt()));
        }
        column
    }
}

/// A sequential, chunked track analyzer.
///
/// Decodes from the start, folding decoded samples into `buckets` waveform
/// peaks spread over the track's media time and bucketing packet bytes into a
/// bitrate profile. Call [`advance`](Self::advance) to make bounded progress:
/// each call decodes at most `max_secs` of media time, so a caller can publish
/// the partial [`waveform`](Self::waveform) and stay responsive.
pub struct TrackAnalyzer {
    opened: OpenedTrack,
    decoder: PacketDecoder,
    time_base: Option<TimeBase>,
    sample_rate: u32,
    bucket: Duration,
    bucket_secs: f64,
    peaks: PeakBuckets,
    byte_counts: Vec<u64>,
    finished: bool,
}

impl TrackAnalyzer {
    /// Open `path` and prepare to analyze it. `duration` is the track's length
    /// (the caller usually has it from the player); when it's unknown the
    /// container's own frame count is used, so the buckets still line up with
    /// media time.
    pub fn new(
        path: &Path,
        buckets: usize,
        bucket: Duration,
        duration: Option<Duration>,
    ) -> anyhow::Result<Self> {
        let opened = open_track(path, false)?;
        let sample_rate = opened.sample_rate().unwrap_or(44_100);
        let channels = opened.channels().max(1);
        let decoder = PacketDecoder::new(&opened.codec_params, channels)?;
        let time_base = opened.time_base();
        let duration = duration
            .filter(|d| *d > Duration::ZERO)
            .unwrap_or_else(|| track_duration(&opened));
        Ok(Self {
            opened,
            decoder,
            time_base,
            sample_rate,
            bucket,
            bucket_secs: bucket.as_secs_f64().max(1e-6),
            peaks: PeakBuckets::new(buckets, duration, sample_rate, channels),
            byte_counts: Vec::new(),
            finished: false,
        })
    }

    /// Media time decoded so far.
    pub fn position(&self) -> Duration {
        self.peaks.position()
    }

    /// Whether the whole stream has been read.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Decode up to `max_secs` of media time, or to the end of the stream.
    pub fn advance(&mut self, max_secs: f64) -> anyhow::Result<()> {
        if self.finished {
            return Ok(());
        }
        let start = self.peaks.position_secs();
        loop {
            if self.peaks.position_secs() - start >= max_secs {
                break;
            }
            let packet = match self.opened.format.next_packet() {
                Ok(packet) => packet,
                // End of stream (or a corrupt packet): stop; what we have
                // covers everything readable.
                Err(_) => {
                    self.finished = true;
                    break;
                }
            };
            if packet.track_id() != self.opened.track_id {
                continue;
            }

            // Bitrate: bucket the compressed bytes by the packet's start time.
            let start_time =
                units_to_duration(packet.ts(), self.time_base, Some(self.sample_rate));
            let index = (start_time.as_secs_f64() / self.bucket_secs).floor() as usize;
            if self.byte_counts.len() <= index {
                self.byte_counts.resize(index + 1, 0);
            }
            self.byte_counts[index] += packet.data.len() as u64;

            // Waveform: decode this packet to samples and fold them in.
            if let Some(samples) = self.decoder.decode(&packet) {
                self.peaks.push(samples);
            }
        }
        Ok(())
    }

    /// The waveform generated so far.
    pub fn waveform(&self) -> Waveform {
        self.peaks.waveform()
    }

    /// The bitrate profile over what's been decoded so far.
    pub fn bitrate(&self) -> BitrateProfile {
        BitrateProfile::from_byte_counts(self.bucket, Arc::from(self.byte_counts.as_slice()))
    }

    /// Run to completion and return the finished analysis.
    pub fn finish(mut self) -> anyhow::Result<TrackAnalysis> {
        while !self.finished {
            self.advance(1.0)?;
        }
        Ok(TrackAnalysis {
            peaks: Arc::new(self.peaks.waveform().normalized_all()),
            bitrate: BitrateProfile::from_byte_counts(self.bucket, Arc::from(self.byte_counts)),
        })
    }
}

/// Everything the dev `probe` binary wants from a track, from a full pass.
pub struct TrackAnalysis {
    /// Normalized (0.0–1.0) waveform peaks.
    pub peaks: Arc<Vec<f32>>,
    pub bitrate: BitrateProfile,
}

/// Analyze `path` in one pass: `buckets` normalized waveform peaks and a
/// bitrate profile bucketed into `bucket`-sized slices of media time.
pub fn analyze_track(
    path: &Path,
    buckets: usize,
    bucket: Duration,
) -> anyhow::Result<TrackAnalysis> {
    TrackAnalyzer::new(path, buckets, bucket, None)?.finish()
}

/// The track's media length from the container's frame count and time base,
/// or zero when the container doesn't declare them.
fn track_duration(opened: &OpenedTrack) -> Duration {
    opened
        .codec_params
        .time_base
        .zip(opened.codec_params.n_frames)
        .map(|(base, frames)| {
            let time = base.calc_time(frames);
            Duration::from_secs(time.seconds) + Duration::from_secs_f64(time.frac)
        })
        .unwrap_or(Duration::ZERO)
}

/// Folds decoded samples into a fixed number of waveform buckets spread over
/// the track's media time, tracking how many buckets are generated and the
/// loudest peak seen (for auto-scaling).
struct PeakBuckets {
    buckets: usize,
    duration_secs: f64,
    sample_rate: f64,
    channels: usize,
    /// Interleaved samples decoded so far.
    samples_seen: u64,
    peaks: Vec<f32>,
    generated: usize,
    max: f32,
}

impl PeakBuckets {
    fn new(buckets: usize, duration: Duration, sample_rate: u32, channels: usize) -> Self {
        let buckets = buckets.max(1);
        Self {
            buckets,
            duration_secs: duration.as_secs_f64().max(1e-6),
            sample_rate: sample_rate.max(1) as f64,
            channels: channels.max(1),
            samples_seen: 0,
            peaks: vec![0.0; buckets],
            generated: 0,
            max: 0.0,
        }
    }

    fn position_secs(&self) -> f64 {
        self.samples_seen as f64 / (self.sample_rate * self.channels as f64)
    }

    fn position(&self) -> Duration {
        Duration::from_secs_f64(self.position_secs())
    }

    /// The bucket a media time falls in, clamped to the last bucket.
    fn bucket_of(&self, secs: f64) -> usize {
        let index = ((secs / self.duration_secs) * self.buckets as f64).floor() as usize;
        index.min(self.buckets - 1)
    }

    /// Fold one packet of interleaved samples in, filling every bucket the
    /// packet's media time spans.
    fn push(&mut self, samples: &[f32]) {
        for chunk in samples.chunks(PEAK_WINDOW) {
            let peak = chunk.iter().fold(0f32, |m, &s| m.max(s.abs()));
            let first = self.bucket_of(self.position_secs());
            self.samples_seen += chunk.len() as u64;
            let last = self.bucket_of(self.position_secs());
            for index in first..=last {
                if peak > self.peaks[index] {
                    self.peaks[index] = peak;
                }
            }
            if last + 1 > self.generated {
                self.generated = last + 1;
            }
            if peak > self.max {
                self.max = peak;
            }
        }
    }

    fn waveform(&self) -> Waveform {
        Waveform { buckets: self.peaks.clone(), generated: self.generated, max: self.max }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // One frame of a 100 Hz sine, then of an 8 kHz sine: the energy must
        // land in the left (low) bands and the right (high) bands respectively.
        let sample_rate = 44_100u32;
        let tone = |hz: f32| {
            (0..FFT_SIZE)
                .map(|i| (std::f32::consts::TAU * hz * i as f32 / sample_rate as f32).sin())
                .collect::<Vec<f32>>()
        };
        let band_of = |samples: Vec<f32>| {
            let mut analyzer = SpectrumAnalyzer::new(sample_rate);
            let column = analyzer.bands(&samples);
            let (index, _) = column.iter().enumerate().fold(
                (0usize, 0f32),
                |(bi, bv), (i, &v)| if v > bv { (i, v) } else { (bi, bv) },
            );
            (index, column)
        };

        let (low_band, low) = band_of(tone(100.0));
        // 100 Hz sits in band ~18 of the 40 Hz–16 kHz geometric ladder.
        assert!((17..=19).contains(&low_band), "100 Hz in band {low_band}");
        assert!(low[40..].iter().all(|&v| v < 0.05 * low[low_band]), "no energy in the highs");

        let (high_band, high) = band_of(tone(8_000.0));
        assert!((112..=114).contains(&high_band), "8 kHz in band {high_band}");
        assert!(high[..30].iter().all(|&v| v < 0.05 * high[high_band]), "the tone leaves the bass alone");
    }

    #[test]
    fn a_short_window_is_padded_rather_than_panicking() {
        let mut analyzer = SpectrumAnalyzer::new(44_100);
        let column = analyzer.bands(&[0.5; 100]);
        assert_eq!(column.len(), BAND_COUNT);
    }

    #[test]
    fn waveform_buckets_fill_in_progressively() {
        // Four buckets over one second of mono audio at 1 kHz: 250 ms each.
        let mut peaks = PeakBuckets::new(4, Duration::from_secs(1), 1_000, 1);
        assert_eq!(peaks.waveform().generated, 0, "nothing generated yet");

        // A loud fifth of a second, then a quiet one.
        peaks.push(&vec![1.0f32; 200]);
        assert_eq!(peaks.waveform().generated, 1);
        peaks.push(&vec![0.1f32; 200]);
        assert_eq!(peaks.waveform().generated, 2);

        let waveform = peaks.waveform();
        assert_eq!(waveform.max, 1.0, "the loudest peak is tracked");
        assert!(waveform.buckets[0] > waveform.buckets[1], "loud then quiet");
        assert_eq!(waveform.buckets[2], 0.0, "the rest is still empty");
    }

    #[test]
    fn a_chunk_spanning_buckets_fills_them_all() {
        // One 400-sample chunk at 1 kHz spans two 250 ms buckets; both must be
        // filled so the generated prefix never has a hole in it.
        let mut peaks = PeakBuckets::new(4, Duration::from_secs(1), 1_000, 1);
        peaks.push(&vec![0.7f32; 400]);
        let waveform = peaks.waveform();
        assert_eq!(waveform.generated, 2);
        assert_eq!(waveform.buckets[0], 0.7);
        assert_eq!(waveform.buckets[1], 0.7);
    }

    #[test]
    fn normalized_scales_against_the_running_max() {
        let waveform = Waveform { buckets: vec![0.5, 1.0, 0.0], generated: 2, max: 1.0 };
        assert_eq!(&waveform.normalized_all()[..waveform.generated], &[0.5, 1.0]);
        assert_eq!(waveform.normalized_all(), vec![0.5, 1.0, 0.0]);
    }

    #[test]
    fn an_empty_waveform_has_nothing_generated() {
        let waveform = Waveform::empty(240);
        assert_eq!(waveform.buckets.len(), 240);
        assert_eq!(waveform.generated, 0);
    }
}
