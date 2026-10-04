//! Tab identity — what a center tab is showing.
//!
//! Kept as data rather than a pure UI concern because a saved "view" (which
//! tabs are open, which is active, where they sit) is fundamentally a list of
//! these.

use std::path::PathBuf;

use super::playlist::PlaylistId;

/// How much of a folder a folder view covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderMode {
    /// The folder and every subfolder under it, flattened — "Play full folder".
    Full,
    /// Only the folder's own songs, no subfolders — "Play folder".
    Here,
}

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
    /// A folder played from the rail's folder view: the albums and singles
    /// under it, flattened. Identified by path, so re-playing the same folder —
    /// in either mode — re-scopes the tab that's already open.
    Folder(PathBuf),
}
