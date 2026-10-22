//! The data model.
//!
//! Everything the rest of the app agrees on: song identity, per-file
//! metadata, lyrics, cover art, and the library.
//!
//! This module is deliberately framework-agnostic — no GPUI, no rodio, no
//! cpal. The audio and UI layers depend on the model; the model never
//! depends on them. That one-way dependency is what lets the model be
//! unit-tested without an audio device or a window.

pub mod config;
pub mod cover;
pub mod format;
pub mod identity;
pub mod input;
pub mod library;
pub mod lrclib;
pub mod lyrics;
pub mod persist;
pub mod playlist;
pub mod queue;
pub mod scan;
pub mod search;
pub mod select;
pub mod song;
pub mod stats;
pub mod user;
pub mod view;

pub use config::{Config, DynamicBase, FontKind, TextureMode, ThemeKind, WindowPlacement};
pub use cover::{CoverCache, DecodedImage};
pub use format::{AudioFormat, FormatSet};
pub use identity::SongId;
pub use input::InputAction;
pub use library::{shuffle, Library};
pub use lyrics::{parse_lrc, LyricLine, Lyrics};
pub use playlist::{
    AlbumOrigin, Playlist, PlaylistId, PlaylistKind, PlaylistStore, SmartKind,
};
pub use queue::QueueEntry;
pub use scan::{LibraryCache, ScanResult, SyncReport};
pub use select::{Order, Scope, Selection};
pub use song::{ReleaseDate, SongFingerprint, SongMetadata};
pub use stats::{SongStats, Stats};
pub use user::{SongIdentities, UserData, UserStore};
pub use view::{FolderMode, TabId};
