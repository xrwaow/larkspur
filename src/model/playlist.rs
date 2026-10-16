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
use super::song::SongMetadata;
use super::stats::{SongStats, Stats};

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
    /// A built-in smart playlist: a *view* over the song stats, never
    /// persisted and rebuilt by [`Stats`](super::stats::Stats)-driven
    /// rebuilds. Its title comes from the kind, like an autogen album's
    /// comes from its tags.
    Smart(SmartKind),
}

/// The built-in smart playlists. One variant each, in the order the sidebar
/// pins them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SmartKind {
    RecentlyAdded,
    RecentlyPlayed,
    MostPlayed,
}

/// Smart playlists are capped — a "recent" list of thousands isn't
/// recent, and the sidebar lists are a starting point, not the whole
/// library.
const SMART_CAP: usize = 100;

impl SmartKind {
    pub const ALL: [SmartKind; 3] =
        [SmartKind::RecentlyAdded, SmartKind::RecentlyPlayed, SmartKind::MostPlayed];

    pub fn title(self) -> &'static str {
        match self {
            SmartKind::RecentlyAdded => "Recently Added",
            SmartKind::RecentlyPlayed => "Recently Played",
            SmartKind::MostPlayed => "Most Played",
        }
    }

    /// Pick and order the songs this kind stands for, from the library's
    /// songs and the current stats.
    ///
    /// Every ordering carries the same deterministic tiebreak — title, then
    /// id — so equal plays or equal stamps never reshuffle between rebuilds.
    pub fn select<'a>(
        self,
        songs: impl Iterator<Item = &'a SongMetadata>,
        stats: &Stats,
    ) -> Vec<SongId> {
        // (sort keys, id) pairs, sorted per kind, then truncated.
        let mut entries: Vec<(&'a SongMetadata, SongStats)> = songs
            .map(|song| (song, stats.get(song.id)))
            .filter(|(_, s)| match self {
                SmartKind::MostPlayed => s.plays > 0,
                SmartKind::RecentlyPlayed => s.last_played.is_some(),
                SmartKind::RecentlyAdded => true,
            })
            .collect();

        // `Reverse` on an `Option` sorts the stamps descending and pushes
        // `None` (never added/played) below every stamp — the stable place
        // for "unknown".
        match self {
            SmartKind::MostPlayed => entries.sort_by(|(a, sa), (b, sb)| {
                sb.plays.cmp(&sa.plays).then_with(|| title_id(a, b))
            }),
            SmartKind::RecentlyAdded => entries.sort_by(|(a, sa), (b, sb)| {
                sb.added.cmp(&sa.added).then_with(|| title_id(a, b))
            }),
            SmartKind::RecentlyPlayed => entries.sort_by(|(a, sa), (b, sb)| {
                sb.last_played.cmp(&sa.last_played).then_with(|| title_id(a, b))
            }),
        }

        entries.truncate(SMART_CAP);
        entries.into_iter().map(|(song, _)| song.id).collect()
    }
}

/// The shared deterministic tiebreak: display title, then id.
fn title_id(a: &SongMetadata, b: &SongMetadata) -> std::cmp::Ordering {
    a.display_title().cmp(&b.display_title()).then(a.id.cmp(&b.id))
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
    /// The playlist's title: an autogen album's name, a smart playlist's
    /// fixed kind title, or a custom/temporary playlist's stored title.
    pub fn title(&self) -> &str {
        match &self.kind {
            PlaylistKind::Auto(PlaylistOrigin::Album { name, .. }) => name,
            PlaylistKind::Smart(kind) => kind.title(),
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

    /// A built-in stats-driven view — not persisted, not user-editable, and
    /// not listed with the user's playlists.
    pub fn is_smart(&self) -> bool {
        matches!(self.kind, PlaylistKind::Smart(_))
    }

    pub fn contains(&self, song: SongId) -> bool {
        self.song_ids.contains(&song)
    }

    pub fn len(&self) -> usize {
        self.song_ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stats::Stats;
    use std::time::{Duration, SystemTime};

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

    fn ids<'a>(kind: SmartKind, songs: impl Iterator<Item = &'a SongMetadata>, stats: &Stats) -> Vec<SongId> {
        kind.select(songs, stats)
    }

    #[test]
    fn titles_are_the_sidebar_labels() {
        let titles: Vec<&str> = SmartKind::ALL.iter().map(|k| k.title()).collect();
        assert_eq!(
            titles,
            vec!["Recently Added", "Recently Played", "Most Played"]
        );
    }

    #[test]
    fn empty_stats_select_nothing() {
        let songs = [song(1, "A"), song(2, "B")];
        let stats = Stats::default();
        // RecentlyAdded is deliberately the exception: with no stats yet
        // every song is "added" (seeded from mtimes), so it lists them all.
        for kind in SmartKind::ALL {
            let picked = ids(kind, songs.iter(), &stats);
            if kind == SmartKind::RecentlyAdded {
                assert_eq!(picked, vec![1, 2], "{} lists the whole library", kind.title());
            } else {
                assert!(picked.is_empty(), "{}", kind.title());
            }
        }
    }

    #[test]
    fn most_played_sorts_by_plays_desc_then_title() {
        let songs = [song(1, "B"), song(2, "A"), song(3, "C")];
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        stats.record_play(1, epoch());
        stats.record_play(2, epoch());
        stats.record_play(3, epoch());
        // Song 1 leads with two plays; songs 2 and 3 tie on one and the
        // title breaks it.
        assert_eq!(
            ids(SmartKind::MostPlayed, songs.iter(), &stats),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn recently_added_sorts_by_stamp_desc_with_unknown_last() {
        let songs = [song(1, "Old"), song(2, "New"), song(3, "Never")];
        let mut stats = Stats::default();
        stats.ensure_added(1, epoch());
        stats.ensure_added(2, epoch() + Duration::from_secs(10));
        assert_eq!(
            ids(SmartKind::RecentlyAdded, songs.iter(), &stats),
            vec![2, 1, 3],
            "newest stamp first, never-added last"
        );
    }

    #[test]
    fn recently_played_needs_a_last_played_stamp() {
        let songs = [song(1, "A"), song(2, "B")];
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        assert_eq!(ids(SmartKind::RecentlyPlayed, songs.iter(), &stats), vec![1]);
        stats.record_play(2, epoch());
        // Same stamp: the deterministic title tiebreak decides, not luck.
        assert_eq!(
            ids(SmartKind::RecentlyPlayed, songs.iter(), &stats),
            vec![1, 2]
        );
    }

    #[test]
    fn recently_played_sorts_newer_stamps_first() {
        let songs = [song(1, "A"), song(2, "B")];
        let mut stats = Stats::default();
        stats.record_play(1, epoch());
        stats.record_play(2, epoch() + Duration::from_secs(10));
        assert_eq!(ids(SmartKind::RecentlyPlayed, songs.iter(), &stats), vec![2, 1]);
    }

    #[test]
    fn smart_lists_are_capped() {
        let songs: Vec<SongMetadata> =
            (0..150).map(|i| song(i as u64, &format!("Song {:03}", 149 - i))).collect();
        let mut stats = Stats::default();
        for s in &songs {
            stats.record_play(s.id, epoch());
        }
        assert_eq!(ids(SmartKind::MostPlayed, songs.iter(), &stats).len(), 100);
    }

    #[test]
    fn selection_is_deterministic_across_rebuilds() {
        let songs = [song(1, "A"), song(2, "B"), song(3, "C")];
        let mut stats = Stats::default();
        for id in [1u64, 2, 3] {
            stats.record_play(id, epoch());
        }
        let first = ids(SmartKind::MostPlayed, songs.iter(), &stats);
        for _ in 0..3 {
            assert_eq!(ids(SmartKind::MostPlayed, songs.iter(), &stats), first);
        }
    }

    #[test]
    fn smart_playlists_report_their_kind() {
        let playlist = Playlist {
            id: PlaylistId(1),
            kind: PlaylistKind::Smart(SmartKind::MostPlayed),
            song_ids: Vec::new(),
        };
        assert!(playlist.is_smart());
        assert_eq!(playlist.title(), "Most Played");
        assert!(!playlist.is_auto());
        assert!(!playlist.is_custom());
    }
}
