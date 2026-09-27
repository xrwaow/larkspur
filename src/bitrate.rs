//! Per-track live-bitrate profiling.
//!
//! A song's declared bitrate ([`SongMetadata::nominal_bitrate`]) is an average
//! baked into the file. [`BitrateProfile`] captures the *actual* moving
//! bitrate — on VBR Opus/MP3 the two diverge a lot, which is why the UI wants
//! the live number rather than the declared one.
//!
//! The profile is built by **demuxing** the file (no decoding) and bucketing
//! each packet's compressed byte count by the media time it occupies. The
//! one-pass track analysis (`crate::analysis`) collects those byte counts
//! while it's already reading packets for the waveform, so no extra pass is
//! needed. Playback then just indexes by position.
//!
//! [`SongMetadata::nominal_bitrate`]: crate::model::SongMetadata::nominal_bitrate

use std::sync::Arc;
use std::time::Duration;

use symphonia::core::units::TimeBase;

/// A track's compressed-bitrate curve over media time.
///
/// A **view** over the raw per-bucket byte counts, not a converted copy: the
/// analyzer publishes a shared slice and the bits-per-second value is computed
/// on read, so publishing a chunk costs one `Arc` bump rather than rebuilding a
/// parallel `Vec<u32>`. Playback indexes it by position, so the "live" number
/// is exact — per-bucket VBR movement included — for every codec, without
/// instrumenting the decode path or sharing state with the audio thread.
#[derive(Debug, Clone)]
pub struct BitrateProfile {
    bucket: Duration,
    /// Compressed bytes per bucket, in media-time order.
    bytes: Arc<[u64]>,
}

impl BitrateProfile {
    /// An empty profile — no buckets yet. The UI starts from this before the
    /// sequential analyzer has read any packets.
    pub fn empty(bucket: Duration) -> Self {
        Self { bucket, bytes: Arc::from([]) }
    }

    /// Build a profile from per-bucket byte counts. The one-pass track
    /// analysis collects the counts while decoding for the waveform, instead
    /// of demuxing a second time.
    pub(crate) fn from_byte_counts(bucket: Duration, bytes: Arc<[u64]>) -> Self {
        Self { bucket, bytes }
    }

    /// The live bitrate at `position`, if the profile covers it.
    pub fn at(&self, position: Duration) -> Option<u32> {
        let index = (position.as_secs_f64() / self.bucket.as_secs_f64()).floor() as usize;
        let bytes = *self.bytes.get(index)?;
        let bps = (bytes as f64 * 8.0 / self.bucket.as_secs_f64().max(1e-6)) as u32;
        (bps > 0).then_some(bps)
    }

    /// The per-bucket bitrates, in media-time order — for diagnostics.
    pub fn bitrates(&self) -> Vec<u32> {
        let bucket_secs = self.bucket.as_secs_f64().max(1e-6);
        self.bytes.iter().map(|&bytes| (bytes as f64 * 8.0 / bucket_secs) as u32).collect()
    }

    /// The media-time width of one bucket.
    pub fn bucket(&self) -> Duration {
        self.bucket
    }
}

/// Convert a Symphonia timestamp/duration (in `TimeBase` units) to wall time,
/// falling back to the sample rate when a container omits a time base.
pub(crate) fn units_to_duration(
    units: u64,
    time_base: Option<TimeBase>,
    sample_rate: Option<u32>,
) -> Duration {
    if let Some(time_base) = time_base {
        let time = time_base.calc_time(units);
        Duration::from_secs(time.seconds) + Duration::from_secs_f64(time.frac)
    } else if let Some(rate) = sample_rate.filter(|r| *r > 0) {
        Duration::from_secs_f64(units as f64 / rate as f64)
    } else {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile over 0.25s buckets, from bits-per-second values.
    fn profile(bps: &[u32]) -> BitrateProfile {
        let bytes: Vec<u64> = bps.iter().map(|&b| (b as f64 * 0.25 / 8.0) as u64).collect();
        BitrateProfile::from_byte_counts(Duration::from_millis(250), Arc::from(bytes))
    }

    #[test]
    fn profile_indexes_by_media_time() {
        // Two 0.25s buckets: 8000 bps then 16000 bps.
        let profile = profile(&[8_000, 16_000]);
        assert_eq!(profile.at(Duration::ZERO), Some(8_000));
        assert_eq!(profile.at(Duration::from_millis(249)), Some(8_000));
        assert_eq!(profile.at(Duration::from_millis(250)), Some(16_000));
        assert_eq!(profile.at(Duration::from_millis(500)), None, "past the end");
    }

    #[test]
    fn profile_skips_empty_buckets() {
        let profile = profile(&[0, 0, 8_000]);
        assert_eq!(profile.at(Duration::ZERO), None);
        assert_eq!(profile.at(Duration::from_millis(500)), Some(8_000));
    }

    #[test]
    fn byte_counts_become_bits_per_second() {
        // 1000 bytes in a 0.25s bucket -> 32000 bps.
        let profile = BitrateProfile::from_byte_counts(
            Duration::from_millis(250),
            Arc::from(vec![1000u64, 0, 2000]),
        );
        assert_eq!(profile.bitrates(), vec![32_000, 0, 64_000]);
    }

    #[test]
    fn units_convert_via_time_base_or_sample_rate() {
        let time_base = TimeBase { numer: 1, denom: 48_000 };
        assert_eq!(
            units_to_duration(48_000, Some(time_base), None),
            Duration::from_secs(1)
        );
        assert_eq!(units_to_duration(44_100, None, Some(44_100)), Duration::from_secs(1));
        assert_eq!(units_to_duration(9_999, None, None), Duration::ZERO);
    }
}
