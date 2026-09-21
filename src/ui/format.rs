//! Small formatting helpers the list views share, so the bitrate and duration
//! columns read identically wherever they appear.

use std::time::Duration;

/// Bits per second as kb/s — always kb/s, never Mb/s.
pub fn format_bitrate(bps: u32) -> String {
    format!("{} kb/s", (bps as f64 / 1000.0).round() as u32)
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn format_secs(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub fn format_duration(duration: Duration) -> String {
    format_secs(duration.as_secs())
}
