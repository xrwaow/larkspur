//! Small formatting helpers the list views share, so the bitrate and duration
//! columns read identically wherever they appear.

use std::time::Duration;

/// Bits per second as kb/s — always kb/s, never Mb/s.
pub fn format_bitrate(bps: u32) -> String {
    format!("{} kb/s", (bps as f64 / 1000.0).round() as u32)
}

/// `m:ss` — minutes run past 59 rather than switching to hours, so two hours
/// reads `119:21`.
pub fn format_secs(secs: u64) -> String {
    let (minutes, seconds) = (secs / 60, secs % 60);
    format!("{minutes}:{seconds:02}")
}

pub fn format_duration(duration: Duration) -> String {
    format_secs(duration.as_secs())
}
