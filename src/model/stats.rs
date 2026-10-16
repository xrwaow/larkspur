//! Per-user, per-song statistics — play counts and "added" stamps.
//!
//! These are deliberately **not** fields on
//! [`SongMetadata`](super::song::SongMetadata): the metadata is rebuilt from
//! the files on every scan and its cache is disposable, so user state stored
//! there would be wiped by a rescan. Instead the stats persist to
//! their own `stats.json`, a sibling of the playlists file, with the same
//! lifetime: user-owned, surviving rescans and cache invalidations.
//!
//! LIMITATION: a [`SongId`] is a hash of the file's path (see
//! [`identity`](super::identity)), so moving a file resets its stats — the
//! same limitation playlists already have. Copying the file elsewhere also
//! reads as a brand-new song.

use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::identity::SongId;
use super::Library;

/// One song's user-facing statistics.
///
/// Every field defaults independently so a `stats.json` from an older shape
/// (or a hand-edited one) still parses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SongStats {
    /// When the song entered the library. `None` for entries that predate the
    /// field (they can still be played).
    #[serde(default)]
    pub added: Option<SystemTime>,
    #[serde(default)]
    pub plays: u32,
    #[serde(default)]
    pub last_played: Option<SystemTime>,
}

/// All songs' statistics, keyed by [`SongId`].
///
/// A missing entry means "never played" — there is no row per
/// song until something actually happens to it, so the map stays small even
/// for a large library.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats(HashMap<SongId, SongStats>);

impl Stats {
    /// The stats for `id`, or the default (never played) when
    /// the song has none. Never fails, so callers need no `Option` handling.
    pub fn get(&self, id: SongId) -> SongStats {
        self.0.get(&id).cloned().unwrap_or_default()
    }

    /// Whether any song has stats yet — how "first run" is detected for the
    /// `added` seeding (see [`LibraryCache::sync`](super::scan::LibraryCache::sync)).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, id: SongId) -> bool {
        self.0.contains_key(&id)
    }

    /// Count one play of `id` at `now`, stamping `last_played` too.
    pub fn record_play(&mut self, id: SongId, now: SystemTime) {
        let entry = self.0.entry(id).or_default();
        entry.plays = entry.plays.saturating_add(1);
        entry.last_played = Some(now);
    }

    /// Stamp `added` for `id`, but only if it doesn't have one already —
    /// re-stamping on every sync would churn the "Recently Added" order.
    pub fn ensure_added(&mut self, id: SongId, stamp: SystemTime) {
        let entry = self.0.entry(id).or_default();
        if entry.added.is_none() {
            entry.added = Some(stamp);
        }
    }

    /// Drop every entry whose song is no longer in `library` — the stats
    /// counterpart of [`Library::prune`]. Returns how many entries were
    /// dropped.
    pub fn prune(&mut self, library: &Library) -> usize {
        let before = self.0.len();
        self.0.retain(|id, _| library.get(*id).is_some());
        before - self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SongMetadata;
    use std::path::PathBuf;
    use std::time::Duration;

    fn song(id: SongId, name: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: PathBuf::from(format!("/music/{name}.flac")),
            song_name: Some(name.to_string()),
            artists: Vec::new(),
            album_name: None,
            album_artist: None,
            track_position: None,
            date: None,
            nominal_bitrate: None,
            lyrics: crate::model::Lyrics::None,
            duration: Duration::ZERO,
            has_art: false,
        }
    }

    fn epoch() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    #[test]
    fn missing_entry_is_the_default() {
        let stats = Stats::default();
        assert_eq!(stats.get(1), SongStats::default());
        assert_eq!(stats.get(1).plays, 0);
        assert_eq!(stats.get(1).last_played, None);
    }

    #[test]
    fn record_play_counts_and_stamps() {
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        stats.record_play(1, epoch());
        let entry = stats.get(1);
        assert_eq!(entry.plays, 2);
        assert_eq!(entry.last_played, Some(epoch()));
        assert_eq!(stats.get(2), SongStats::default(), "other songs stay untouched");
    }

    #[test]
    fn ensure_added_stamps_once() {
        let mut stats = Stats::default();
        stats.ensure_added(1, epoch());
        stats.ensure_added(1, SystemTime::UNIX_EPOCH);
        assert_eq!(stats.get(1).added, Some(epoch()), "the first stamp wins");
    }

    #[test]
    fn plays_survive_a_round_trip_of_the_serialized_shape() {
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        stats.ensure_added(1, epoch());
        let bytes = serde_json::to_vec(&stats).unwrap();
        let reloaded: Stats = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reloaded.get(1).plays, 1);
        assert_eq!(reloaded.get(1).last_played, Some(epoch()));
    }

    #[test]
    fn prune_drops_entries_whose_song_is_gone() {
        let mut lib = Library::default();
        lib.insert_song(song(1, "One"));
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        stats.record_play(2, epoch());
        assert_eq!(stats.prune(&lib), 1);
        assert!(stats.contains(1));
        assert!(!stats.contains(2));
    }
}
