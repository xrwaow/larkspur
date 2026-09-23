use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use super::identity::SongId;
use super::playlist::{Playlist, PlaylistId, PlaylistKind, PlaylistMeta, PlaylistOrigin};
use super::song::SongMetadata;

/// The single source of truth for static library state.
///
/// Holds every song and every playlist. Playlists reference songs by
/// [`SongId`] rather than owning `SongMetadata` copies — a tag edit happens
/// once and every playlist that references the song sees it. Autogen album
/// playlists and custom playlists share one [`Playlist`] schema; the
/// per-artist groupings ("discography", "artist appears in") are derived on
/// demand, so there's no second copy of the library to drift.
///
/// Alongside the songs and playlists it keeps a small derived [`Index`],
/// rebuilt by [`reindex`](Self::reindex) whenever either changes, so the hot
/// queries (`album_playlist_of`, `discography`, `playlist_year`) are lookups
/// rather than scans. The fields are private so that index can't be bypassed:
/// every mutation goes through a method that keeps it fresh.
#[derive(Default)]
pub struct Library {
    songs: HashMap<SongId, SongMetadata>,
    playlists: HashMap<PlaylistId, Playlist>,
    /// Monotonic allocator for [`PlaylistId`]s. Persisted with the library so
    /// ids are never reused across launches.
    next_playlist_id: u64,
    /// Derived lookups, rebuilt on mutation.
    index: Index,
}

/// A playlist's release stamp: the year, and the full date (year, month,
/// day) — each taken from the first song that declares one, so the two may
/// come from different songs. The month/day is what breaks year ties in a
/// discography.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub year: Option<u16>,
    pub date: Option<(u16, u8, u8)>,
}

/// The derived lookups a [`Library`] answers from instead of rescanning.
#[derive(Default)]
struct Index {
    /// Every artist credited on any song — `BTreeSet` so `artists()` comes out
    /// sorted and deduped without a second pass.
    artists: BTreeSet<String>,
    /// The album playlist each song belongs to.
    song_album: HashMap<SongId, PlaylistId>,
    /// The playlists each artist is credited on.
    artist_playlists: BTreeMap<String, Vec<PlaylistId>>,
    /// Each playlist's release stamp, taken from its first dated songs.
    playlist_release: HashMap<PlaylistId, Release>,
}

impl Library {
    // --- bulk replacement ----------------------------------------------

    /// Replace every song, re-deriving the index. The cache's bulk path.
    pub fn replace_songs(&mut self, songs: HashMap<SongId, SongMetadata>) {
        self.songs = songs;
        self.reindex();
    }

    /// Replace every playlist and the id counter, re-deriving the index.
    pub fn replace_playlists(
        &mut self,
        playlists: HashMap<PlaylistId, Playlist>,
        next_playlist_id: u64,
    ) {
        self.playlists = playlists;
        self.next_playlist_id = next_playlist_id;
        self.reindex();
    }

    /// Replace songs, playlists, and the id counter in one step — what the
    /// cache's instant-start path installs.
    pub fn install(
        &mut self,
        songs: HashMap<SongId, SongMetadata>,
        playlists: HashMap<PlaylistId, Playlist>,
        next_playlist_id: u64,
    ) {
        self.songs = songs;
        self.playlists = playlists;
        self.next_playlist_id = next_playlist_id;
        self.reindex();
    }

    /// The persisted id counter, so the cache can round-trip it.
    pub fn next_playlist_id(&self) -> u64 {
        self.next_playlist_id
    }

    // --- songs ---------------------------------------------------------

    pub fn insert_song(&mut self, song: SongMetadata) -> SongId {
        let id = song.id;
        self.songs.insert(id, song);
        self.reindex();
        id
    }

    pub fn get(&self, id: SongId) -> Option<&SongMetadata> {
        self.songs.get(&id)
    }

    /// Every song, in no particular order.
    pub fn songs(&self) -> impl Iterator<Item = &SongMetadata> {
        self.songs.values()
    }

    pub fn song_count(&self) -> usize {
        self.songs.len()
    }

    // --- playlist lookup ----------------------------------------------

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.playlists.get(&id)
    }

    /// All playlists in a stable order: autogen albums by title, then custom
    /// playlists by creation order.
    ///
    /// Temporary playlists are deliberately left out — they must not surface
    /// in the library browse, the rail, or the persisted cache. They're
    /// reached through [`playlist`](Self::playlist) by whatever holds their id
    /// (an open tab).
    pub fn playlists(&self) -> Vec<&Playlist> {
        let mut all: Vec<&Playlist> = self
            .playlists
            .values()
            .filter(|p| p.kind != PlaylistKind::Temporary)
            .collect();
        all.sort_by(|a, b| {
            (a.kind == PlaylistKind::Custom, a.meta().title, a.id.0)
                .cmp(&(b.kind == PlaylistKind::Custom, b.meta().title, b.id.0))
        });
        all
    }

    /// A playlist's songs, resolving ids to metadata. Ids no longer in the
    /// library (moved/deleted files) are skipped rather than yielding a
    /// dangling reference.
    pub fn songs_of(&self, id: PlaylistId) -> Vec<&SongMetadata> {
        self.playlist(id)
            .map(|p| p.song_ids.iter().filter_map(|s| self.songs.get(s)).collect())
            .unwrap_or_default()
    }

    /// The autogen album playlist a song belongs to, if any — what "go to
    /// playlist" on a song opens. An index lookup.
    pub fn album_playlist_of(&self, song: SongId) -> Option<PlaylistId> {
        self.index.song_album.get(&song).copied()
    }

    /// A playlist's release stamp: the first year and the first full date
    /// any of its songs declare. An index lookup.
    pub fn release(&self, id: PlaylistId) -> Release {
        self.index.playlist_release.get(&id).copied().unwrap_or_default()
    }

    /// A playlist's release year: the first year any of its songs declares.
    /// `None` when no song carries one. An index lookup.
    pub fn playlist_year(&self, id: PlaylistId) -> Option<u16> {
        self.release(id).year
    }

    /// A playlist's full release date, by the same rule as
    /// [`playlist_year`](Self::playlist_year). The month/day breaks year ties
    /// in a discography. An index lookup.
    pub fn playlist_date(&self, id: PlaylistId) -> Option<(u16, u8, u8)> {
        self.release(id).date
    }

    // --- custom playlist mutation -------------------------------------

    fn alloc_playlist_id(&mut self) -> PlaylistId {
        let id = PlaylistId(self.next_playlist_id);
        self.next_playlist_id += 1;
        id
    }

    /// Create an empty, user-owned playlist. Returns its id.
    pub fn create_custom(&mut self, title: impl Into<String>) -> PlaylistId {
        let id = self.alloc_playlist_id();
        self.playlists.insert(
            id,
            Playlist {
                id,
                meta: Some(PlaylistMeta::new(title)),
                kind: PlaylistKind::Custom,
                song_ids: Vec::new(),
                origin: None,
            },
        );
        self.reindex();
        id
    }

    /// Create a temporary playlist pre-filled with `songs` — the backing for
    /// an ad-hoc view ("Play folder", a selection played as one). Temporary
    /// playlists are immutable snapshots: never persisted, dropped by a
    /// rescan, and deleted when their tab closes.
    pub fn create_temporary(&mut self, title: impl Into<String>, songs: Vec<SongId>) -> PlaylistId {
        self.create_temporaries(vec![(title.into(), songs)]).remove(0)
    }

    /// Create several temporary playlists at once, re-indexing once — a
    /// folder view can need hundreds, and a reindex walks the whole library.
    pub fn create_temporaries(
        &mut self,
        groups: Vec<(String, Vec<SongId>)>,
    ) -> Vec<PlaylistId> {
        let mut ids = Vec::with_capacity(groups.len());
        for (title, songs) in groups {
            let id = self.alloc_playlist_id();
            self.playlists.insert(
                id,
                Playlist {
                    id,
                    meta: Some(PlaylistMeta::new(title)),
                    kind: PlaylistKind::Temporary,
                    song_ids: songs,
                    origin: None,
                },
            );
            ids.push(id);
        }
        self.reindex();
        ids
    }

    /// Add a song to a custom playlist.
    ///
    /// A no-op (returns `false`) for autogen playlists — they're derived and
    /// rebuilt on every rescan, so an edit would just be overwritten — or if
    /// the song is already present.
    pub fn add_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        let added = self.add_songs(playlist, &[song]);
        added
    }

    /// Add several songs to a custom playlist at once, re-indexing once.
    pub fn add_songs(&mut self, playlist: PlaylistId, songs: &[SongId]) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom {
            return false;
        }
        let mut added = false;
        for &song in songs {
            if !p.contains(song) {
                p.song_ids.push(song);
                added = true;
            }
        }
        if added {
            self.reindex();
        }
        added
    }

    /// Remove a song from a custom playlist.
    pub fn remove_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        self.remove_songs(playlist, &[song])
    }

    /// Remove several songs from a custom playlist at once, re-indexing once.
    pub fn remove_songs(&mut self, playlist: PlaylistId, songs: &[SongId]) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom {
            return false;
        }
        let before = p.song_ids.len();
        p.song_ids.retain(|s| !songs.contains(s));
        let removed = p.song_ids.len() != before;
        if removed {
            self.reindex();
        }
        removed
    }

    pub fn rename(&mut self, playlist: PlaylistId, title: impl Into<String>) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom {
            return false;
        }
        if let Some(meta) = p.meta.as_mut() {
            meta.title = title.into();
        }
        // A title isn't indexed, so no reindex is needed here.
        true
    }

    /// Delete a playlist. Custom playlists are removed from the rail; temporary
    /// ones are deleted when their tab closes. Autogen ones can't be removed —
    /// they're rebuilt from tags.
    pub fn remove_playlist(&mut self, playlist: PlaylistId) -> Option<Playlist> {
        if self.playlists.get(&playlist).is_some_and(|p| p.kind != PlaylistKind::Auto) {
            let removed = self.playlists.remove(&playlist);
            self.reindex();
            removed
        } else {
            None
        }
    }

    /// Delete several playlists at once, re-indexing once — closing a folder
    /// tab can remove hundreds of temporary playlists. Autogen playlists are
    /// skipped. Returns how many were removed.
    pub fn remove_playlists(&mut self, ids: &[PlaylistId]) -> usize {
        let mut removed = 0;
        for id in ids {
            if self.playlists.get(id).is_some_and(|p| p.kind != PlaylistKind::Auto) {
                self.playlists.remove(id);
                removed += 1;
            }
        }
        if removed > 0 {
            self.reindex();
        }
        removed
    }

    /// Every custom playlist, in creation order — the "add to playlist"
    /// picker.
    pub fn custom_playlists(&self) -> Vec<&Playlist> {
        let mut all: Vec<&Playlist> = self
            .playlists
            .values()
            .filter(|p| p.kind == PlaylistKind::Custom)
            .collect();
        all.sort_by_key(|p| p.id.0);
        all
    }

    // --- derived groupings --------------------------------------------

    /// Every artist credited anywhere in the library, deduped and sorted.
    pub fn artists(&self) -> Vec<String> {
        self.index.artists.iter().cloned().collect()
    }

    /// An artist's discography: the album playlists they appear on, newest
    /// release first, with undated albums below the dated ones.
    pub fn discography(&self, artist: &str) -> Vec<PlaylistId> {
        let mut ids = self.artist_playlists(artist, true);
        self.sort_by_year(&mut ids);
        ids
    }

    /// Every playlist containing a song crediting `artist` — compilations
    /// and custom playlists included ("artist appears in").
    pub fn artist_appears_in(&self, artist: &str) -> Vec<PlaylistId> {
        let mut ids = self.artist_playlists(artist, false);
        ids.sort_by(|a, b| self.title_of(*a).cmp(&self.title_of(*b)));
        ids
    }

    fn artist_playlists(&self, artist: &str, albums_only: bool) -> Vec<PlaylistId> {
        self.index
            .artist_playlists
            .get(artist)
            .map(|ids| {
                ids.iter()
                    .copied()
                    .filter(|id| {
                        !albums_only
                            || self.playlists.get(id).is_some_and(|p| p.kind == PlaylistKind::Auto)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn title_of(&self, id: PlaylistId) -> String {
        self.playlists.get(&id).map(|p| p.meta().title).unwrap_or_default()
    }

    /// Newest release first: by year, then — within a year — by the release
    /// date's month/day, then by title so the order is stable. Albums with no
    /// year sort below the dated ones.
    pub(crate) fn sort_by_year(&self, ids: &mut [PlaylistId]) {
        ids.sort_by(|a, b| {
            let title_a = self.title_of(*a);
            let title_b = self.title_of(*b);
            match (self.playlist_year(*a), self.playlist_year(*b)) {
                (Some(year_a), Some(year_b)) => year_b
                    .cmp(&year_a)
                    .then_with(|| self.playlist_date(*b).cmp(&self.playlist_date(*a)))
                    .then_with(|| title_a.cmp(&title_b)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => title_a.cmp(&title_b),
            }
        });
    }

    // --- maintenance ---------------------------------------------------

    /// Drop playlist references to songs no longer in the library. Returns
    /// how many references were dropped.
    pub fn prune(&mut self) -> usize {
        let Self { songs, playlists, .. } = self;
        let mut dropped = 0;
        for p in playlists.values_mut() {
            let before = p.song_ids.len();
            p.song_ids.retain(|id| songs.contains_key(id));
            dropped += before - p.song_ids.len();
        }
        if dropped > 0 {
            self.reindex();
        }
        dropped
    }

    /// Replace this library's user-owned playlists with `other`'s, keeping the
    /// monotonic id counter.
    ///
    /// A scan rebuilds autogen playlists from tags but never touches custom
    /// ones — so the *live* library's custom set is authoritative and has to
    /// survive a scan that was already in flight when the user edited it.
    /// Without this, creating or editing a playlist during the startup scan
    /// is silently undone when the scan lands.
    pub fn adopt_custom_from(&mut self, other: &Library) {
        self.playlists.retain(|_, p| !p.is_custom());
        for playlist in other.playlists.values().filter(|p| p.is_custom()) {
            self.playlists.insert(playlist.id, playlist.clone());
        }
        self.next_playlist_id = self.next_playlist_id.max(other.next_playlist_id);
        self.reindex();
    }

    /// Rebuild every autogen (album) playlist from the current song tags,
    /// leaving custom playlists untouched.
    ///
    /// Idempotent: a rebuilt album keeps its [`PlaylistId`] by matching on
    /// [`PlaylistOrigin`], so a rescan doesn't churn ids.
    pub fn rebuild_auto(&mut self) {
        // Group songs by (album title, album artist, album directory) — the
        // natural key an album playlist is rebuilt from. The directory keeps
        // two same-titled releases in separate folders apart; `BTreeMap`
        // keeps the walk ordered, so the result is deterministic.
        let mut groups: BTreeMap<(String, String, PathBuf), Vec<SongId>> = BTreeMap::new();
        for song in self.songs.values() {
            groups
                .entry((
                    song.album().to_string(),
                    song.album_artist().to_string(),
                    song.path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
                ))
                .or_default()
                .push(song.id);
        }

        // Album order: track number, then title.
        for ids in groups.values_mut() {
            ids.sort_by_key(|id| {
                self.songs
                    .get(id)
                    .map(|s| {
                        (
                            s.track_position.unwrap_or(u16::MAX),
                            s.song_name.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or((u16::MAX, String::new()))
            });
        }

        // Existing autogen playlists, indexed by origin, so a rebuilt album
        // can keep its id.
        let mut existing: HashMap<PlaylistOrigin, PlaylistId> = self
            .playlists
            .values()
            .filter_map(|p| p.origin.clone().map(|o| (o, p.id)))
            .collect();

        for ((album, artist, dir), ids) in groups {
            let origin = PlaylistOrigin::Album { name: album, artist, dir };

            match existing.remove(&origin) {
                Some(id) => {
                    if let Some(p) = self.playlists.get_mut(&id) {
                        p.song_ids = ids;
                    }
                }
                None => {
                    let id = self.alloc_playlist_id();
                    self.playlists.insert(
                        id,
                        Playlist {
                            id,
                            // An autogen playlist's meta is derived from its
                            // origin and first song — see `Playlist::meta`.
                            meta: None,
                            kind: PlaylistKind::Auto,
                            song_ids: ids,
                            origin: Some(origin),
                        },
                    );
                }
            }
        }

        // Whatever origin is left had no songs this time around.
        for id in existing.into_values() {
            self.playlists.remove(&id);
        }

        self.reindex();
    }

    // --- internals -----------------------------------------------------

    /// Rebuild the derived index from the current songs and playlists.
    fn reindex(&mut self) {
        let mut index = Index::default();
        for song in self.songs.values() {
            for artist in &song.artists {
                index.artists.insert(artist.clone());
            }
        }
        for playlist in self.playlists.values() {
            let mut release = Release::default();
            for song_id in &playlist.song_ids {
                let Some(song) = self.songs.get(song_id) else { continue };
                if release.year.is_none() {
                    release.year = song.year;
                }
                if release.date.is_none() {
                    release.date = song.release_date;
                }
                if playlist.kind == PlaylistKind::Auto {
                    index.song_album.insert(*song_id, playlist.id);
                }
                // Temporary playlists are ad-hoc views, not credits — an
                // artist's "appears in" grouping must not list them.
                if playlist.kind != PlaylistKind::Temporary {
                    for artist in &song.artists {
                        index
                            .artist_playlists
                            .entry(artist.clone())
                            .or_default()
                            .push(playlist.id);
                    }
                }
            }
            index.playlist_release.insert(playlist.id, release);
        }
        for ids in index.artist_playlists.values_mut() {
            ids.sort();
            ids.dedup();
        }
        self.index = index;
    }
}

/// Artists credited anywhere in the library, deduped — used by tests.
#[cfg(test)]
fn artist_set(library: &Library) -> std::collections::BTreeSet<String> {
    library.artists().into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lyrics;
    use std::collections::BTreeSet;
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
            year: None,
            release_date: None,
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            has_art: false,
        }
    }

    fn tagged(id: SongId, name: &str, artist: &str, album: &str, track: u16) -> SongMetadata {
        SongMetadata {
            song_name: Some(name.to_string()),
            artists: vec![artist.to_string()],
            album_name: Some(album.to_string()),
            track_position: Some(track),
            ..song(id, name)
        }
    }

    /// A tagged song living in a specific album folder.
    fn album_in(
        dir: &str,
        id: SongId,
        name: &str,
        artist: &str,
        album: &str,
        track: u16,
    ) -> SongMetadata {
        SongMetadata {
            path: PathBuf::from(dir).join(format!("{name}.flac")),
            ..tagged(id, name, artist, album, track)
        }
    }

    fn with_year(mut song: SongMetadata, year: u16) -> SongMetadata {
        song.year = Some(year);
        song
    }

    fn dated(id: SongId, name: &str, artist: &str, album: &str, date: (u16, u8, u8)) -> SongMetadata {
        let mut song = with_year(tagged(id, name, artist, album, 1), date.0);
        song.release_date = Some(date);
        song
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let mut lib = Library::default();
        let id = lib.insert_song(song(7, "Seven"));
        assert_eq!(id, 7);
        assert_eq!(lib.get(7).unwrap().song_name.as_deref(), Some("Seven"));
        assert!(lib.get(8).is_none());
    }

    #[test]
    fn reinserting_same_id_replaces() {
        let mut lib = Library::default();
        lib.insert_song(song(7, "Seven"));
        lib.insert_song(song(7, "Seven (Remastered)"));
        assert_eq!(lib.song_count(), 1);
        assert_eq!(lib.get(7).unwrap().song_name.as_deref(), Some("Seven (Remastered)"));
    }

    fn album_titles(lib: &Library) -> Vec<String> {
        lib.playlists()
            .into_iter()
            .filter(|p| !p.is_custom())
            .map(|p| p.meta().title)
            .collect()
    }

    #[test]
    fn autogen_groups_songs_by_album_in_track_order() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "B", "A", "Album", 2));
        lib.insert_song(tagged(2, "A", "A", "Album", 1));
        lib.insert_song(tagged(3, "C", "A", "Other", 1));
        lib.rebuild_auto();

        assert_eq!(album_titles(&lib), vec!["Album", "Other"]);
        let album = lib
            .playlists()
            .into_iter()
            .find(|p| p.meta().title == "Album")
            .unwrap();
        let order: Vec<&str> =
            lib.songs_of(album.id).iter().map(|s| s.song_name.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["A", "B"], "track order, not insertion order");
    }

    #[test]
    fn rebuild_is_idempotent_and_keeps_ids() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "X", "Album", 1));
        lib.rebuild_auto();
        let first = lib.playlists()[0].id;
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].id, first, "id survives a rebuild");
    }

    #[test]
    fn compilation_stays_one_playlist_via_album_artist() {
        let mut lib = Library::default();
        let mut one = tagged(1, "A", "Artist 1", "Comp", 1);
        one.album_artist = Some("Various Artists".to_string());
        let mut two = tagged(2, "B", "Artist 2", "Comp", 2);
        two.album_artist = Some("Various Artists".to_string());
        lib.insert_song(one);
        lib.insert_song(two);
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1, "one album, not one per track artist");
    }

    #[test]
    fn same_titled_albums_in_different_folders_stay_separate() {
        let mut lib = Library::default();
        lib.insert_song(album_in("/music/a", 1, "A", "X", "Same", 1));
        lib.insert_song(album_in("/music/b", 2, "B", "X", "Same", 1));
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 2, "same title, different folders");
    }

    #[test]
    fn untagged_songs_land_in_an_unknown_bucket() {
        let mut lib = Library::default();
        lib.insert_song(song(1, "mystery"));
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].meta().title, "Unknown Album");
    }

    #[test]
    fn custom_playlists_are_added_removed_and_deduped() {
        let mut lib = Library::default();
        let id = lib.create_custom("Faves");
        assert_eq!(lib.custom_playlists().len(), 1);
        assert!(lib.add_song(id, 1));
        assert!(!lib.add_song(id, 1), "already present");
        assert_eq!(lib.songs_of(id).len(), 0, "song 1 isn't in the library");
        lib.insert_song(song(1, "One"));
        assert_eq!(lib.songs_of(id).len(), 1);
        assert!(lib.remove_playlist(id).is_some());
        assert!(lib.custom_playlists().is_empty());
    }

    #[test]
    fn adopt_custom_from_keeps_user_edits_across_a_rescan() {
        // A scan rebuilds autogen playlists but must not drop a custom one the
        // user created while the scan was in flight.
        let mut live = Library::default();
        let id = live.create_custom("Faves");
        live.insert_song(tagged(1, "A", "X", "Album", 1));
        live.add_song(id, 1);

        let mut scanned = Library::default();
        scanned.insert_song(tagged(1, "A", "X", "Album", 1));
        scanned.rebuild_auto();

        scanned.adopt_custom_from(&live);
        assert!(scanned.custom_playlists().iter().any(|p| p.id == id));
        assert_eq!(scanned.songs_of(id).len(), 1);
    }

    #[test]
    fn album_playlist_of_finds_the_autogen_owner() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "X", "Album", 1));
        lib.insert_song(tagged(2, "B", "X", "Other", 1));
        lib.rebuild_auto();
        let album = lib
            .playlists()
            .into_iter()
            .find(|p| p.meta().title == "Album")
            .unwrap();
        assert_eq!(lib.album_playlist_of(1), Some(album.id));
        assert_eq!(lib.album_playlist_of(999), None);
    }

    #[test]
    fn autogen_playlists_are_read_only() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "X", "Album", 1));
        lib.rebuild_auto();
        let auto = lib.playlists()[0].id;
        assert!(!lib.add_song(auto, 1), "can't add to an autogen playlist");
        assert!(!lib.remove_song(auto, 1), "can't remove from one either");
        assert!(!lib.rename(auto, "Nope"));
    }

    #[test]
    fn duplicate_titles_get_distinct_ids() {
        let mut lib = Library::default();
        let a = lib.create_custom("Same");
        let b = lib.create_custom("Same");
        assert_ne!(a, b);
        assert_eq!(lib.custom_playlists().len(), 2);
    }

    #[test]
    fn temporary_playlists_are_ephemeral() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "X", "Album", 1));
        lib.rebuild_auto();

        let temp = lib.create_temporary("Folder", vec![1]);
        assert!(lib.playlist(temp).is_some_and(|p| p.is_temporary()));
        // Not listed with the real playlists — the rail, the browse, and the
        // persisted cache all read that list.
        assert!(!lib.playlists().iter().any(|p| p.id == temp));
        assert!(lib.custom_playlists().is_empty());
        // Reachable by id, and deletable (unlike an autogen playlist).
        assert_eq!(lib.playlist(temp).map(|p| p.song_ids.clone()), Some(vec![1]));
        assert!(lib.remove_playlist(temp).is_some());
        assert!(lib.playlist(temp).is_none());
    }

    #[test]
    fn temporary_playlists_stay_out_of_the_artist_index() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        lib.create_temporary("Mix", vec![1]);
        assert_eq!(lib.artist_appears_in("Grimes").len(), 1, "the album only");
    }

    #[test]
    fn prune_drops_references_to_missing_songs() {
        let mut lib = Library::default();
        let id = lib.create_custom("Faves");
        lib.insert_song(song(1, "One"));
        lib.add_song(id, 1);
        lib.add_song(id, 2);
        // Song 2 was never inserted, so it's dangling.
        assert_eq!(lib.prune(), 1);
        assert_eq!(lib.songs_of(id).len(), 1);
    }

    #[test]
    fn discography_lists_an_artists_album_playlists() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.insert_song(tagged(2, "B", "Grimes", "Art Angels", 1));
        lib.insert_song(tagged(3, "C", "Someone", "Other", 1));
        lib.rebuild_auto();

        let titles: Vec<String> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        assert_eq!(titles, vec!["Art Angels", "Visions"]);
        assert_eq!(lib.discography("Someone").len(), 1);
        assert!(lib.discography("Nobody").is_empty());
    }

    #[test]
    fn discography_sorts_newest_first_with_undated_last() {
        let mut lib = Library::default();
        lib.insert_song(with_year(tagged(1, "A", "Grimes", "Old", 1), 1999));
        lib.insert_song(with_year(tagged(2, "B", "Grimes", "New", 1), 2020));
        lib.insert_song(with_year(tagged(3, "C", "Grimes", "Middle", 1), 2010));
        lib.insert_song(tagged(4, "D", "Grimes", "Undated", 1));
        lib.rebuild_auto();

        let titles: Vec<String> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        assert_eq!(titles, vec!["New", "Middle", "Old", "Undated"]);
        assert_eq!(lib.playlist_year(lib.discography("Grimes")[0]), Some(2020));
    }

    #[test]
    fn discography_breaks_year_ties_on_month_and_day() {
        let mut lib = Library::default();
        lib.insert_song(dated(1, "A", "Grimes", "June", (2010, 6, 1)));
        lib.insert_song(dated(2, "B", "Grimes", "September", (2010, 9, 1)));
        lib.insert_song(dated(3, "C", "Grimes", "DayOnly", (2010, 6, 2)));
        lib.insert_song(with_year(tagged(4, "D", "Grimes", "NoDate", 1), 2010));
        lib.rebuild_auto();

        let titles: Vec<String> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        // Newest month/day first; an album with no full date falls under ones
        // that have it, all within the same year.
        assert_eq!(titles, vec!["September", "DayOnly", "June", "NoDate"]);
    }

    #[test]
    fn artist_appears_in_includes_custom_playlists() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        let custom = lib.create_custom("Faves");
        lib.add_song(custom, 1);

        assert_eq!(lib.discography("Grimes").len(), 1, "album playlists only");
        assert_eq!(lib.artist_appears_in("Grimes").len(), 2, "album + custom");
    }

    #[test]
    fn artists_are_deduped_and_sorted() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Zed", "Album", 1));
        lib.insert_song(tagged(2, "B", "Amy", "Album", 2));
        lib.insert_song(tagged(3, "C", "Zed", "Other", 1));
        assert_eq!(artist_set(&lib), BTreeSet::from(["Amy".to_string(), "Zed".to_string()]));
    }
}
