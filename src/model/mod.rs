//! The data model.
//!
//! Everything the rest of the app agrees on: song identity, per-file
//! metadata, lyrics, cover art, the library, and live playback state.
//!
//! This module is deliberately framework-agnostic — no GPUI, no rodio, no
//! cpal. The audio and UI layers depend on the model; the model never
//! depends on them. That one-way dependency is what lets the model be
//! unit-tested without an audio device or a window.

pub mod config;
pub mod cover;
pub mod identity;
pub mod input;
pub mod library;
pub mod lyrics;
pub mod playlist;
pub mod scan;
pub mod search;
pub mod song;
pub mod streaming;
pub mod view;

pub use config::{Config, FontKind, ThemeKind};
pub use cover::{CoverCache, CoverState, DecodedImage};
pub use identity::{generate_song_id, SongId};
pub use input::InputAction;
pub use library::Library;
pub use lyrics::{parse_lrc, LyricLine, Lyrics};
pub use playlist::{Playlist, PlaylistId, PlaylistKind, PlaylistMeta, PlaylistOrigin};
pub use scan::{LibraryCache, ScanRoot, SyncReport};
pub use song::SongMetadata;
pub use streaming::SongStatus;
pub use view::TabId;
