//! Tab identity — what a center tab is showing.
//!
//! Kept as data rather than a pure UI concern because a saved "view" (which
//! tabs are open, which is active, where they sit) is fundamentally a list of
//! these. [`TabId::kind`] is the stable, persisted form.

use super::playlist::PlaylistId;

/// Which container a center tab shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabId {
    /// The library browse list — every album, grouped, with its cover.
    Browse,
    /// One playlist opened as its own tab.
    Playlist(PlaylistId),
    /// The library scoped to one artist's discography, opened from a song's
    /// "Go to {artist}" menu item. Its own tab rather than a filtered library,
    /// so it sits beside the library instead of replacing it.
    Artist(String),
}

impl TabId {
    /// The kind of tab, as a stable string — what a persisted view would store
    /// alongside the playlist id (or artist name).
    pub fn kind(&self) -> &'static str {
        match self {
            TabId::Browse => "browse",
            TabId::Playlist(_) => "playlist",
            TabId::Artist(_) => "artist",
        }
    }
}
