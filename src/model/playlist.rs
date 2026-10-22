//! Playlists — the uniform container for a list of songs.
//!
//! Autogen (album) and custom playlists share one [`Playlist`] struct; the
//! variant-specific data lives in [`PlaylistKind`]. They differ in where their
//! title comes from and whether a rescan rebuilds them: autogen playlists are
//! derived from song tags and read-only, custom ones are user-owned and
//! persisted.
//!
//! [`PlaylistStore`] separates the three lifetimes a playlist can have:
//! persisted content (albums + custom playlists), ephemeral snapshots (the
//! ad-hoc "play these songs" views), and the three built-in smart views. Only
//! persisted content participates in indexing or persistence; the separation
//! makes that a property of the store rather than a filter every caller has to
//! remember.
//!
//! An *artist* is not a playlist — it's a list of playlists (their albums).
//! Those groupings are derived on demand from [`Library`](super::library::Library)
//! rather than stored, so there's exactly one song store to keep in sync.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::identity::SongId;
use super::song::SongMetadata;
use super::stats::{SongStats, Stats};

/// Stable playlist identity.
///
/// Persisted playlists (custom and album) allocate from a monotonic counter
/// stored with the user data, so an id is never reused even after a delete.
/// The three smart views use the top `u64` values, which no allocator can
/// reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistId(pub u64);

impl PlaylistId {
    /// Reserved ids for [`SmartKind::ALL`], counting down from `u64::MAX`.
    pub(crate) const SMART_BASE: u64 = u64::MAX - (SmartKind::ALL.len() as u64 - 1);
}

/// Whether a playlist is derived from the library or owned by the user, plus
/// the data only that variant carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistKind {
    /// Rebuilt from song tags on every rescan; read-only. The [`AlbumOrigin`]
    /// is the natural key a rescan matches on, so a rebuilt album keeps its id.
    Auto(AlbumOrigin),
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

    /// The small leading glyph the rail draws before the title.
    pub fn glyph(self) -> &'static str {
        match self {
            SmartKind::RecentlyAdded => "✦",
            SmartKind::RecentlyPlayed => "↻",
            SmartKind::MostPlayed => "▲",
        }
    }

    /// The hint a smart playlist shows while it has no songs, explaining how
    /// its contents are produced.
    pub fn empty_hint(self) -> &'static str {
        match self {
            SmartKind::RecentlyAdded => "Add music — new songs land here.",
            SmartKind::RecentlyPlayed | SmartKind::MostPlayed => {
                "Play something to fill this in."
            }
        }
    }

    fn slot(self) -> usize {
        match self {
            SmartKind::RecentlyAdded => 0,
            SmartKind::RecentlyPlayed => 1,
            SmartKind::MostPlayed => 2,
        }
    }

    /// The reserved id for this kind, so a rebuild never has to match by kind
    /// or allocate a counter value.
    pub fn id(self) -> PlaylistId {
        PlaylistId(PlaylistId::SMART_BASE + self.slot() as u64)
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
        // Decorate each entry with its stats and display title once, so the
        // comparators only read precomputed values (and a Cow makes the title
        // borrow rather than allocate).
        let mut entries: Vec<(&'a SongMetadata, SongStats, std::borrow::Cow<'a, str>)> = songs
            .map(|song| (song, stats.get(song.id), song.display_title()))
            .filter(|(_, s, _)| match self {
                SmartKind::MostPlayed => s.plays > 0,
                SmartKind::RecentlyPlayed => s.last_played.is_some(),
                SmartKind::RecentlyAdded => true,
            })
            .collect();

        // `None` stamps sort below every stamp (the stable place for
        // "unknown"); every ordering has the same title-then-id tiebreak so
        // equal plays or equal stamps never reshuffle between rebuilds.
        let compare = |(a, sa, ta): &(&SongMetadata, SongStats, std::borrow::Cow<'_, str>),
                       (b, sb, tb): &(&SongMetadata, SongStats, std::borrow::Cow<'_, str>)| {
            match self {
                SmartKind::MostPlayed => sb
                    .plays
                    .cmp(&sa.plays)
                    .then_with(|| ta.cmp(tb))
                    .then(a.id.cmp(&b.id)),
                SmartKind::RecentlyAdded => sb
                    .added
                    .cmp(&sa.added)
                    .then_with(|| ta.cmp(tb))
                    .then(a.id.cmp(&b.id)),
                SmartKind::RecentlyPlayed => sb
                    .last_played
                    .cmp(&sa.last_played)
                    .then_with(|| ta.cmp(tb))
                    .then(a.id.cmp(&b.id)),
            }
        };

        // Only the best `SMART_CAP` are ever shown. Partition first so a
        // 50k-song library costs O(n) plus a 100-entry sort per rebuild,
        // rather than a full O(n log n) sort of every song.
        if entries.len() > SMART_CAP {
            entries.select_nth_unstable_by(SMART_CAP, |a, b| compare(a, b));
            entries.truncate(SMART_CAP);
        }
        entries.sort_by(compare);

        entries.into_iter().map(|(song, _, _)| song.id).collect()
    }
}

/// The natural key an autogen playlist is matched by on rescan, so a rebuilt
/// album keeps its [`PlaylistId`] instead of churning a new one each launch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AlbumOrigin {
    /// The album title.
    pub name: String,
    /// The album artist. Part of the key so two albums that share a title
    /// don't collide.
    pub artist: String,
    /// The album's directory. Part of the key so two *releases* with the same
    /// title and artist (an original and a re-release, each in its own folder)
    /// stay separate instead of merging into one.
    pub dir: PathBuf,
}

impl AlbumOrigin {
    pub fn new(name: impl Into<String>, artist: impl Into<String>, dir: PathBuf) -> Self {
        Self { name: name.into(), artist: artist.into(), dir }
    }
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
            PlaylistKind::Auto(origin) => &origin.name,
            PlaylistKind::Smart(kind) => kind.title(),
            PlaylistKind::Custom { title } | PlaylistKind::Temporary { title } => title,
        }
    }

    /// The album artist, for an autogen album playlist.
    pub fn artist(&self) -> Option<&str> {
        match &self.kind {
            PlaylistKind::Auto(origin) => Some(&origin.artist),
            _ => None,
        }
    }

    /// The natural key a rescan matches an autogen playlist on.
    pub fn origin(&self) -> Option<&AlbumOrigin> {
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

/// The three playlist lifetimes, held apart so indexing and persistence can
/// never accidentally see the wrong one.
pub struct PlaylistStore {
    /// Autogen albums and user-owned playlists. The only content that is
    /// indexed, listed in the rail, or saved.
    persisted: HashMap<PlaylistId, Playlist>,
    /// Ad-hoc snapshots. Reachable by id, but never indexed or persisted.
    ephemeral: HashMap<PlaylistId, Playlist>,
    /// The three built-in views, updated from [`Stats`].
    smart: [Playlist; SmartKind::ALL.len()],
    /// Stable album id per origin, persisted with the user data so an album
    /// keeps its id across rescans and launches.
    album_ids: BTreeMap<AlbumOrigin, PlaylistId>,
    /// Monotonic allocator for persisted/ephemeral playlist ids.
    next_id: u64,
}

impl Default for PlaylistStore {
    fn default() -> Self {
        let smart = SmartKind::ALL.map(|kind| Playlist {
            id: kind.id(),
            kind: PlaylistKind::Smart(kind),
            song_ids: Vec::new(),
        });
        Self {
            persisted: HashMap::new(),
            ephemeral: HashMap::new(),
            smart,
            album_ids: BTreeMap::new(),
            next_id: 0,
        }
    }
}

impl PlaylistStore {
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    pub fn album_ids(&self) -> &BTreeMap<AlbumOrigin, PlaylistId> {
        &self.album_ids
    }

    pub fn set_album_ids(&mut self, album_ids: BTreeMap<AlbumOrigin, PlaylistId>) {
        self.album_ids = album_ids;
    }

    pub fn load_persisted(
        &mut self,
        playlists: impl IntoIterator<Item = Playlist>,
        next_id: u64,
    ) {
        self.persisted = playlists.into_iter().map(|playlist| (playlist.id, playlist)).collect();
        self.next_id = next_id;
    }

    /// Every playlist by id — persisted, ephemeral, or smart.
    pub fn get(&self, id: PlaylistId) -> Option<&Playlist> {
        self.persisted
            .get(&id)
            .or_else(|| self.ephemeral.get(&id))
            .or_else(|| self.smart.iter().find(|p| p.id == id))
    }

    pub fn get_persisted(&self, id: PlaylistId) -> Option<&Playlist> {
        self.persisted.get(&id)
    }

    pub fn get_persisted_mut(&mut self, id: PlaylistId) -> Option<&mut Playlist> {
        self.persisted.get_mut(&id)
    }

    /// Persisted playlists only: the content that is indexed and saved.
    pub fn iter_persisted(&self) -> impl Iterator<Item = &Playlist> {
        self.persisted.values()
    }

    pub fn insert_persisted(&mut self, playlist: Playlist) -> Option<Playlist> {
        self.persisted.insert(playlist.id, playlist)
    }

    pub fn insert_ephemeral(&mut self, playlist: Playlist) -> Option<Playlist> {
        self.ephemeral.insert(playlist.id, playlist)
    }

    pub fn remove(&mut self, id: PlaylistId) -> Option<Playlist> {
        self.persisted.remove(&id).or_else(|| self.ephemeral.remove(&id))
    }

    pub fn drop_ephemeral(&mut self) {
        self.ephemeral.clear();
    }

    pub fn custom_playlists(&self) -> Vec<&Playlist> {
        let mut custom: Vec<&Playlist> =
            self.persisted.values().filter(|p| p.is_custom()).collect();
        custom.sort_by_key(|p| p.id.0);
        custom
    }

    pub fn smart_playlists(&self) -> &[Playlist] {
        &self.smart
    }

    pub fn smart_mut(&mut self, kind: SmartKind) -> &mut Playlist {
        &mut self.smart[kind.slot()]
    }

    pub fn album_id(&self, origin: &AlbumOrigin) -> Option<PlaylistId> {
        self.album_ids.get(origin).copied()
    }

    pub fn insert_album_id(&mut self, origin: AlbumOrigin, id: PlaylistId) -> Option<PlaylistId> {
        self.album_ids.insert(origin, id)
    }

    pub fn remove_album_id(&mut self, origin: &AlbumOrigin) -> Option<PlaylistId> {
        self.album_ids.remove(origin)
    }

    pub fn alloc_id(&mut self) -> PlaylistId {
        let id = PlaylistId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Remove song references that no longer resolve. Returns how many
    /// references were dropped.
    pub fn prune(&mut self, mut keep: impl FnMut(SongId) -> bool) -> usize {
        let mut dropped = 0;
        for playlist in self.persisted.values_mut().chain(self.ephemeral.values_mut()) {
            let before = playlist.song_ids.len();
            playlist.song_ids.retain(|id| keep(*id));
            dropped += before - playlist.song_ids.len();
        }
        dropped
    }

    /// The user-owned playlists that are saved. Autogen albums are rebuilt
    /// from tags and smart views from stats, so neither is persisted.
    pub fn user_playlists(&self) -> Vec<Playlist> {
        self.custom_playlists().into_iter().cloned().collect()
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
            ..Default::default()
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
        let songs = [song(SongId(1), "A"), song(SongId(2), "B")];
        let stats = Stats::default();
        // RecentlyAdded is deliberately the exception: with no stats yet
        // every song is "added" (seeded from mtimes), so it lists them all.
        for kind in SmartKind::ALL {
            let picked = ids(kind, songs.iter(), &stats);
            if kind == SmartKind::RecentlyAdded {
                assert_eq!(picked, vec![SongId(1), SongId(2)], "{} lists the whole library", kind.title());
            } else {
                assert!(picked.is_empty(), "{}", kind.title());
            }
        }
    }

    #[test]
    fn most_played_sorts_by_plays_desc_then_title() {
        let songs = [song(SongId(1), "B"), song(SongId(2), "A"), song(SongId(3), "C")];
        let mut stats = Stats::default();
        stats.record_play(SongId(1), epoch());
        stats.record_play(SongId(1), epoch());
        stats.record_play(SongId(2), epoch());
        stats.record_play(SongId(3), epoch());
        // Song 1 leads with two plays; songs 2 and 3 tie on one and the
        // title breaks it.
        assert_eq!(
            ids(SmartKind::MostPlayed, songs.iter(), &stats),
            vec![SongId(1), SongId(2), SongId(3)]
        );
    }

    #[test]
    fn recently_added_sorts_by_stamp_desc_with_unknown_last() {
        let songs = [song(SongId(1), "Old"), song(SongId(2), "New"), song(SongId(3), "Never")];
        let mut stats = Stats::default();
        stats.ensure_added(SongId(1), epoch());
        stats.ensure_added(SongId(2), epoch() + Duration::from_secs(10));
        assert_eq!(
            ids(SmartKind::RecentlyAdded, songs.iter(), &stats),
            vec![SongId(2), SongId(1), SongId(3)],
            "newest stamp first, never-added last"
        );
    }

    #[test]
    fn recently_played_needs_a_last_played_stamp() {
        let songs = [song(SongId(1), "A"), song(SongId(2), "B")];
        let mut stats = Stats::default();
        stats.record_play(SongId(1), epoch());
        assert_eq!(ids(SmartKind::RecentlyPlayed, songs.iter(), &stats), vec![SongId(1)]);
        stats.record_play(SongId(2), epoch());
        // Same stamp: the deterministic title tiebreak decides, not luck.
        assert_eq!(
            ids(SmartKind::RecentlyPlayed, songs.iter(), &stats),
            vec![SongId(1), SongId(2)]
        );
    }

    #[test]
    fn recently_played_sorts_newer_stamps_first() {
        let songs = [song(SongId(1), "A"), song(SongId(2), "B")];
        let mut stats = Stats::default();
        stats.record_play(SongId(1), epoch());
        stats.record_play(SongId(2), epoch() + Duration::from_secs(10));
        assert_eq!(ids(SmartKind::RecentlyPlayed, songs.iter(), &stats), vec![SongId(2), SongId(1)]);
    }

    #[test]
    fn smart_lists_are_capped() {
        let songs: Vec<SongMetadata> =
            (0..150).map(|i| song(SongId(i as u64), &format!("Song {:03}", 149 - i))).collect();
        let mut stats = Stats::default();
        for s in &songs {
            stats.record_play(s.id, epoch());
        }
        assert_eq!(ids(SmartKind::MostPlayed, songs.iter(), &stats).len(), 100);
    }

    #[test]
    fn selection_is_deterministic_across_rebuilds() {
        let songs = [song(SongId(1), "A"), song(SongId(2), "B"), song(SongId(3), "C")];
        let mut stats = Stats::default();
        for id in [SongId(1), SongId(2), SongId(3)] {
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

    #[test]
    fn store_keeps_the_three_lifetimes_apart() {
        let mut store = PlaylistStore::default();
        let id = store.alloc_id();
        store.insert_persisted(Playlist {
            id,
            kind: PlaylistKind::Custom { title: "Faves".into() },
            song_ids: vec![SongId(1)],
        });
        let temp = store.alloc_id();
        store.insert_ephemeral(Playlist {
            id: temp,
            kind: PlaylistKind::Temporary { title: "Mix".into() },
            song_ids: vec![SongId(1)],
        });

        assert_eq!(store.custom_playlists().len(), 1);
        assert!(store.get(temp).is_some());
        assert_eq!(store.user_playlists().len(), 1, "only the custom one is saved");
        store.drop_ephemeral();
        assert!(store.get(temp).is_none());
    }
}
