/// Coarse playback status for display and state projection.
///
/// Distinct from the live playback state the UI owns: this is the small,
/// display-oriented slice (`playing` / `paused` / `ended` / nothing selected)
/// that views match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongStatus {
    /// Nothing has been selected yet.
    NoSelection,
    Playing,
    Paused,
    /// The queue has played out — play/next are inert.
    Ended,
}
