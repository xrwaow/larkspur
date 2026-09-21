//! Rolling live-bitrate estimation.
//!
//! A song's declared bitrate ([`SongMetadata::nominal_bitrate`]) is an
//! average baked into the file. This tracks the *actual* moving bitrate by
//! summing decoded packet sizes over a sliding time window — on VBR
//! Opus/MP3 the two diverge a lot, which is why the UI wants the live
//! number rather than the declared one.
//!
//! [`SongMetadata::nominal_bitrate`]: crate::model::SongMetadata::nominal_bitrate

use std::time::Duration;

/// Windowed bitrate estimator.
///
/// Feed every decoded packet's byte length (and the time since the previous
/// packet) to [`feed`](BitrateTracker::feed); every `window` of accumulated
/// time it returns a refreshed bits-per-second estimate.
///
/// The caller supplies the elapsed time rather than the tracker reading the
/// wall clock, so the estimate can be driven by *media* time (which is what
/// you want — pausing or buffering shouldn't drag the average down) and so
/// it can be tested deterministically.
pub struct BitrateTracker {
    window_bytes: usize,
    elapsed: Duration,
    window: Duration,
}

impl BitrateTracker {
    /// A tracker with a 1-second window.
    pub fn new() -> Self {
        Self::with_window(Duration::from_secs(1))
    }

    pub fn with_window(window: Duration) -> Self {
        Self {
            window_bytes: 0,
            elapsed: Duration::ZERO,
            window,
        }
    }

    /// Feed a packet's byte length and the time elapsed since the previous
    /// packet. Returns `Some(bps)` when the window rolls over and the
    /// estimate refreshes.
    pub fn feed(&mut self, bytes: usize, since_last: Duration) -> Option<u32> {
        self.window_bytes += bytes;
        self.elapsed += since_last;

        if self.elapsed >= self.window {
            let bps = (self.window_bytes as f64 * 8.0 / self.elapsed.as_secs_f64()) as u32;
            self.window_bytes = 0;
            self.elapsed = Duration::ZERO;
            Some(bps)
        } else {
            None
        }
    }
}

impl Default for BitrateTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_estimate_until_the_window_fills() {
        let mut t = BitrateTracker::with_window(Duration::from_secs(1));
        assert_eq!(t.feed(100, Duration::from_millis(400)), None);
        assert_eq!(t.feed(100, Duration::from_millis(400)), None);
    }

    #[test]
    fn reports_bits_per_second_when_the_window_rolls() {
        let mut t = BitrateTracker::with_window(Duration::from_secs(1));
        // 1000 bytes over 0.5s, then 1000 more over 0.5s -> 2000 bytes / 1s.
        assert_eq!(t.feed(1000, Duration::from_millis(500)), None);
        assert_eq!(t.feed(1000, Duration::from_millis(500)), Some(16_000));
    }

    #[test]
    fn resets_after_rolling() {
        let mut t = BitrateTracker::with_window(Duration::from_secs(1));
        assert_eq!(t.feed(1000, Duration::from_secs(1)), Some(8_000));
        // Fresh window: a single small packet shouldn't inherit the old bytes.
        assert_eq!(t.feed(10, Duration::from_millis(100)), None);
        assert_eq!(t.feed(0, Duration::from_millis(900)), Some(80));
    }

    #[test]
    fn default_window_is_one_second() {
        let mut t = BitrateTracker::new();
        assert_eq!(t.feed(1000, Duration::from_millis(999)), None);
        assert_eq!(t.feed(0, Duration::from_millis(1)), Some(8_000));
    }
}
