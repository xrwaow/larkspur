//! One-pass track analysis: waveform peaks and the live-bitrate profile.
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

/// Everything the UI wants from a track, from a single pass.
pub struct TrackAnalysis {
    /// Normalized (0.0–1.0) waveform peaks. `Arc` so views clone cheaply.
    pub peaks: Arc<Vec<f32>>,
    pub bitrate: BitrateProfile,
}

/// Analyze `path` in one pass: `buckets` normalized waveform peaks, plus a
/// bitrate profile bucketed into `bucket`-sized slices of media time.
pub fn analyze_track(
    path: &Path,
    buckets: usize,
    bucket: Duration,
) -> anyhow::Result<TrackAnalysis> {
    let opened = open_track(path, false)?;
    let track_id = opened.track_id;
    let (time_base, sample_rate, channels) = {
        let track = opened
            .format
            .tracks()
            .iter()
            .find(|t| t.id == track_id)
            .ok_or_else(|| anyhow::anyhow!("no track {track_id}"))?;
        (
            track.codec_params.time_base,
            track.codec_params.sample_rate,
            track.codec_params.channels.map(|c| c.count()).unwrap_or(2),
        )
    };

    // One decoder or the other, chosen by the shared `PacketDecoder` — Opus
    // goes to libopus, everything else to symphonia.
    let mut decoder = PacketDecoder::new(&opened.codec_params, channels)?;
    let mut format = opened.format;

    let bucket_secs = bucket.as_secs_f64().max(1e-6);
    let mut peaks: Vec<f32> = Vec::new();
    let mut byte_counts: Vec<u64> = Vec::new();

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
        }
    }

    Ok(TrackAnalysis {
        peaks: Arc::new(bucket_peaks(&peaks, buckets)),
        bitrate: BitrateProfile::from_byte_counts(bucket, byte_counts),
    })
}

/// Convenience for callers that only want the waveform (e.g. `seek_probe`).
pub fn compute_waveform(path: &Path, buckets: usize) -> anyhow::Result<Vec<f32>> {
    let analysis = analyze_track(path, buckets, Duration::from_millis(250))?;
    Ok(analysis.peaks.as_ref().clone())
}

/// Per-[`PEAK_WINDOW`]-sample interleaved peaks.
fn push_peaks(samples: &[f32], peaks: &mut Vec<f32>) {
    for chunk in samples.chunks(PEAK_WINDOW) {
        peaks.push(chunk.iter().fold(0f32, |m, &s| m.max(s.abs())));
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
    use symphonia::core::units::TimeBase;

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
    fn peaks_group_every_peak_window() {
        let samples = vec![1.0f32; PEAK_WINDOW + 1];
        let mut peaks = Vec::new();
        push_peaks(&samples, &mut peaks);
        assert_eq!(peaks.len(), 2, "one full window plus a partial one");
        assert!(peaks.iter().all(|&p| p == 1.0));
    }

    #[test]
    fn units_convert_via_time_base_or_sample_rate() {
        let time_base = TimeBase { numer: 1, denom: 48_000 };
        assert_eq!(
            units_to_duration(48_000, Some(time_base), None),
            Duration::from_secs(1)
        );
        assert_eq!(units_to_duration(44_100, None, Some(44_100)), Duration::from_secs(1));
    }
}
