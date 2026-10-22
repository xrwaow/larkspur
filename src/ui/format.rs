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

/// `format_duration` padded to a minimum five-character width. The transport's
/// left-hand position readout uses this so growing from `9:59` to `10:00`
/// doesn't resize the seek bar next to it.
pub fn format_duration_fixed(duration: Duration) -> String {
    format!("{:>5}", format_duration(duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_readout_stays_the_same_width_across_ten_minutes() {
        assert_eq!(format_duration_fixed(Duration::from_secs(59)), " 0:59");
        assert_eq!(format_duration_fixed(Duration::from_secs(9 * 60 + 59)), " 9:59");
        assert_eq!(format_duration_fixed(Duration::from_secs(10 * 60)), "10:00");
    }
}
