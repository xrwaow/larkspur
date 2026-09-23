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
pub mod identity;
pub mod input;
pub mod library;
pub mod lyrics;
pub mod playlist;
pub mod scan;
pub mod search;
pub mod select;
pub mod song;
pub mod view;

pub use config::{Config, DynamicBase, FontKind, ThemeKind, WindowPlacement};
pub use cover::{CoverCache, DecodedImage};
pub use identity::{generate_song_id, SongId};
pub use input::InputAction;
pub use library::{Library, Release};
pub use lyrics::{parse_lrc, LyricLine, Lyrics};
pub use playlist::{Playlist, PlaylistId, PlaylistKind, PlaylistMeta, PlaylistOrigin};
pub use scan::{LibraryCache, SyncReport};
pub use select::{Order, Scope, Selection};
pub use song::SongMetadata;
pub use view::TabId;
