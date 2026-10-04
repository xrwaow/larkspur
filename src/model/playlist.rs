//! Playlists — the uniform container for a list of songs.
//!
//! Autogen (album) and custom playlists share one [`Playlist`] struct; the
//! variant-specific data lives in [`PlaylistKind`]. They differ in where their
//! title comes from and whether a rescan rebuilds them: autogen playlists are
//! derived from song tags and read-only, custom ones are user-owned and
//! persisted.
//!
//! An *artist* is not a playlist — it's a list of playlists (their albums).
//! Those groupings are derived on demand from [`Library`](super::library::Library)
//! rather than stored, so there's exactly one song store to keep in sync.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::identity::SongId;

/// Stable playlist identity.
///
/// A persisted, monotonically increasing counter — deliberately *not* a hash
/// of the title, since two playlists are allowed to share a name. Never
/// reused, so a reference to a deleted playlist can't silently resolve to a
/// different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistId(pub u64);

/// Whether a playlist is derived from the library or owned by the user, plus
/// the data only that variant carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistKind {
    /// Rebuilt from song tags on every rescan; read-only. The [`PlaylistOrigin`]
    /// is the natural key a rescan matches on, so a rebuilt album keeps its id.
    Auto(PlaylistOrigin),
    /// User-owned: add/remove/rename/delete; persisted.
    Custom { title: String },
    /// Created on the fly (a multi-row selection played as one). Never
    /// persisted and dropped by a rescan — it lives exactly as long as the
    /// tab that shows it.
    Temporary { title: String },
}

/// The natural key an autogen playlist is matched by on rescan, so a rebuilt
/// album keeps its [`PlaylistId`] instead of churning a new one each launch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaylistOrigin {
    /// One playlist per album tag. `artist` is part of the key so two albums
    /// that share a title don't collide, and `dir` so two *releases* with the
    /// same title and artist (e.g. an original and a re-release, each in its
    /// own folder) stay separate instead of merging into one.
    Album { name: String, artist: String, dir: PathBuf },
}

/// A list of songs, referencing them by [`SongId`] rather than owning
/// `SongMetadata` copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: PlaylistId,
    pub kind: PlaylistKind,
    pub song_ids: Vec<SongId>,
}

impl Playlist {
    /// The playlist's title: an autogen album's name, or a custom/temporary
    /// playlist's stored title.
    pub fn title(&self) -> &str {
        match &self.kind {
            PlaylistKind::Auto(PlaylistOrigin::Album { name, .. }) => name,
            PlaylistKind::Custom { title } | PlaylistKind::Temporary { title } => title,
        }
    }

    /// The album artist, for an autogen album playlist.
    pub fn artist(&self) -> Option<&str> {
        match &self.kind {
            PlaylistKind::Auto(PlaylistOrigin::Album { artist, .. }) => Some(artist),
            _ => None,
        }
    }

    /// The natural key a rescan matches an autogen playlist on.
    pub fn origin(&self) -> Option<&PlaylistOrigin> {
        match &self.kind {
            PlaylistKind::Auto(origin) => Some(origin),
            _ => None,
        }
    }

    /// The song whose cover represents the playlist — its first, when it has
    /// one.
    pub fn cover(&self) -> Option<SongId> {
        self.song_ids.first().copied()
    }

    pub fn is_auto(&self) -> bool {
        matches!(self.kind, PlaylistKind::Auto(_))
    }

    pub fn is_custom(&self) -> bool {
        matches!(self.kind, PlaylistKind::Custom { .. })
    }

    pub fn is_temporary(&self) -> bool {
        matches!(self.kind, PlaylistKind::Temporary { .. })
    }

    pub fn contains(&self, song: SongId) -> bool {
        self.song_ids.contains(&song)
    }

    pub fn len(&self) -> usize {
        self.song_ids.len()
    }
}
