use std::time::Duration;

use super::identity::SongId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongStatus {
    /// Nothing has been selected yet.
    NoSelection,
    Playing,
    Paused,
    /// The queue has played out — play/next are inert.
    Ended,
}

/// Live, per-frame/per-packet playback state.
///
/// Distinct from [`SongMetadata`](super::song::SongMetadata): this is the
/// small, fast-changing slice the UI actually re-reads each frame, while
/// metadata is loaded once per file.
pub struct StreamingInfo {
    pub status: SongStatus,
    /// Reference into `Library::songs`, not an owned copy — avoids the
    /// metadata drifting out of sync with the library's copy.
    pub current_song: Option<SongId>,
    pub elapsed: Duration,
    /// Rolling live bitrate, computed from decoded packet sizes.
    pub live_bitrate_bps: Option<u32>,
}

impl Default for StreamingInfo {
    fn default() -> Self {
        Self {
            status: SongStatus::NoSelection,
            current_song: None,
            elapsed: Duration::ZERO,
            live_bitrate_bps: None,
        }
    }
}
