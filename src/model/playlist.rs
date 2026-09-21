//! Playlists — the uniform container for a list of songs.
//!
//! Autogen (album) and custom playlists share one schema. They differ only
//! in where their [`PlaylistMeta`] comes from and whether a rescan rebuilds
//! them: autogen playlists are derived from song tags and read-only, custom
//! ones are user-owned and persisted.
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

/// Whether a playlist is derived from the library or owned by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistKind {
    /// Rebuilt from song tags on every rescan; read-only.
    Auto,
    /// User-owned: add/remove/rename/delete; persisted.
    Custom,
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

/// What's *declared about* a playlist — the uniform schema autogen and
/// custom playlists share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistMeta {
    pub title: String,
    /// Album artist for an autogen album playlist; `None` for custom.
    pub artist: Option<String>,
    /// The song whose cover represents this playlist.
    ///
    /// `None` means "inherit": the UI shows an autogen playlist's first
    /// song's cover, and falls back to the now-playing cover for a custom
    /// playlist that has no art of its own.
    pub cover: Option<SongId>,
}

impl PlaylistMeta {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), artist: None, cover: None }
    }
}

/// A list of songs, referencing them by [`SongId`] rather than owning
/// `SongMetadata` copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: PlaylistId,
    pub meta: PlaylistMeta,
    pub kind: PlaylistKind,
    pub song_ids: Vec<SongId>,
    /// Present for [`PlaylistKind::Auto`]; the key a rescan matches on.
    pub origin: Option<PlaylistOrigin>,
}

impl Playlist {
    pub fn is_custom(&self) -> bool {
        self.kind == PlaylistKind::Custom
    }

    pub fn contains(&self, song: SongId) -> bool {
        self.song_ids.contains(&song)
    }

    pub fn len(&self) -> usize {
        self.song_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.song_ids.is_empty()
    }
}
