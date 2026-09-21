use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use super::identity::SongId;
use super::playlist::{Playlist, PlaylistId, PlaylistKind, PlaylistMeta, PlaylistOrigin};
use super::song::SongMetadata;

/// Metadata for the library itself — the "list of playlists" as a whole.
///
/// Defaults to nothing, so an unscanned library renders without a title or
/// cover until something sets them.
#[derive(Debug, Clone, Default)]
pub struct LibraryMeta {
    pub title: Option<String>,
    pub cover: Option<SongId>,
}

/// The single source of truth for static library state.
///
/// Holds every song and every playlist. Playlists reference songs by
/// [`SongId`] rather than owning `SongMetadata` copies — a tag edit happens
/// once and every playlist that references the song sees it. Autogen album
/// playlists and custom playlists share one [`Playlist`] schema; the
/// per-artist groupings ("discography", "artist appears in") are derived on
/// demand, so there's no second copy of the library to drift.
#[derive(Default)]
pub struct Library {
    pub songs: HashMap<SongId, SongMetadata>,
    pub playlists: HashMap<PlaylistId, Playlist>,
    pub meta: LibraryMeta,
    /// Monotonic allocator for [`PlaylistId`]s. Persisted with the library so
    /// ids are never reused across launches.
    pub(crate) next_playlist_id: u64,
}

impl Library {
    // --- songs ---------------------------------------------------------

    pub fn insert_song(&mut self, song: SongMetadata) -> SongId {
        let id = song.id;
        self.songs.insert(id, song);
        id
    }

    pub fn get(&self, id: SongId) -> Option<&SongMetadata> {
        self.songs.get(&id)
    }

    // --- playlist lookup ----------------------------------------------

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.playlists.get(&id)
    }

    /// All playlists in a stable order: autogen albums by title, then custom
    /// playlists by creation order.
    pub fn playlists(&self) -> Vec<&Playlist> {
        let mut all: Vec<&Playlist> = self.playlists.values().collect();
        all.sort_by(|a, b| {
            (a.kind == PlaylistKind::Custom, &a.meta.title, a.id.0)
                .cmp(&(b.kind == PlaylistKind::Custom, &b.meta.title, b.id.0))
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
    /// playlist" on a song opens.
    pub fn album_playlist_of(&self, song: SongId) -> Option<PlaylistId> {
        self.playlists
            .values()
            .find(|p| p.kind == PlaylistKind::Auto && p.song_ids.contains(&song))
            .map(|p| p.id)
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
                meta: PlaylistMeta::new(title),
                kind: PlaylistKind::Custom,
                song_ids: Vec::new(),
                origin: None,
            },
        );
        id
    }

    /// Add a song to a custom playlist.
    ///
    /// A no-op (returns `false`) for autogen playlists — they're derived and
    /// rebuilt on every rescan, so an edit would just be overwritten — or if
    /// the song is already present.
    pub fn add_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom || p.contains(song) {
            return false;
        }
        p.song_ids.push(song);
        true
    }

    /// Remove a song from a custom playlist.
    pub fn remove_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom {
            return false;
        }
        let before = p.song_ids.len();
        p.song_ids.retain(|&s| s != song);
        p.song_ids.len() != before
    }

    pub fn rename(&mut self, playlist: PlaylistId, title: impl Into<String>) -> bool {
        let Some(p) = self.playlists.get_mut(&playlist) else { return false };
        if p.kind != PlaylistKind::Custom {
            return false;
        }
        p.meta.title = title.into();
        true
    }

    /// Delete a playlist. Only custom playlists can be removed; autogen ones
    /// are rebuilt from tags.
    pub fn remove_playlist(&mut self, playlist: PlaylistId) -> Option<Playlist> {
        if self.playlists.get(&playlist).is_some_and(Playlist::is_custom) {
            self.playlists.remove(&playlist)
        } else {
            None
        }
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
        let mut set = BTreeSet::new();
        for song in self.songs.values() {
            for artist in &song.artists {
                set.insert(artist.clone());
            }
        }
        set.into_iter().collect()
    }

    /// An artist's discography: the album playlists they appear on, by title.
    pub fn discography(&self, artist: &str) -> Vec<PlaylistId> {
        self.appears_in(artist, true)
    }

    /// Every playlist containing a song crediting `artist` — compilations
    /// and custom playlists included ("artist appears in").
    pub fn artist_appears_in(&self, artist: &str) -> Vec<PlaylistId> {
        self.appears_in(artist, false)
    }

    fn appears_in(&self, artist: &str, albums_only: bool) -> Vec<PlaylistId> {
        let mut ids: Vec<PlaylistId> = self
            .playlists
            .values()
            .filter(|p| {
                (!albums_only || p.kind == PlaylistKind::Auto)
                    && p.song_ids.iter().any(|s| self.credits(*s, artist))
            })
            .map(|p| p.id)
            .collect();
        ids.sort_by_key(|id| {
            self.playlists.get(id).map(|p| p.meta.title.clone()).unwrap_or_default()
        });
        ids
    }

    fn credits(&self, song: SongId, artist: &str) -> bool {
        self.songs.get(&song).is_some_and(|s| s.artists.iter().any(|a| a == artist))
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
            let origin = PlaylistOrigin::Album { name: album.clone(), artist: artist.clone(), dir };
            let meta = PlaylistMeta {
                title: album,
                artist: Some(artist),
                cover: ids.first().copied(),
            };

            match existing.remove(&origin) {
                Some(id) => {
                    if let Some(p) = self.playlists.get_mut(&id) {
                        p.song_ids = ids;
                        p.meta = meta;
                    }
                }
                None => {
                    let id = self.alloc_playlist_id();
                    self.playlists.insert(
                        id,
                        Playlist {
                            id,
                            meta,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CoverState, Lyrics};
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
            nominal_bitrate: None,
            lyrics: Lyrics::None,
            duration: Duration::ZERO,
            cover: CoverState::Missing,
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
        lib.insert_song(song(1, "Old"));
        lib.insert_song(song(1, "New"));
        assert_eq!(lib.songs.len(), 1);
        assert_eq!(lib.get(1).unwrap().song_name.as_deref(), Some("New"));
    }

    #[test]
    fn autogen_groups_songs_by_album_in_track_order() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "Two", "Grimes", "Visions", 2));
        lib.insert_song(tagged(2, "One", "Grimes", "Visions", 1));
        lib.insert_song(tagged(3, "Other", "Grimes", "Art Angels", 1));
        lib.rebuild_auto();

        let albums: Vec<&Playlist> = lib.playlists();
        assert_eq!(albums.len(), 2);
        // Sorted by title: "Art Angels" before "Visions".
        assert_eq!(albums[0].meta.title, "Art Angels");
        assert_eq!(albums[1].meta.title, "Visions");
        assert_eq!(albums[1].song_ids, vec![2, 1], "track order, not insertion order");
        assert_eq!(albums[1].meta.cover, Some(2), "cover is the first song");
        assert_eq!(albums[1].meta.artist.as_deref(), Some("Grimes"));
        assert!(albums.iter().all(|p| p.kind == PlaylistKind::Auto));
    }

    #[test]
    fn rebuild_is_idempotent_and_keeps_ids() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        let id = lib.playlists()[0].id;

        lib.insert_song(tagged(2, "B", "Grimes", "Visions", 2));
        lib.rebuild_auto();

        let album = &lib.playlists()[0];
        assert_eq!(album.id, id, "id survives a rescan");
        assert_eq!(album.song_ids, vec![1, 2]);
    }

    #[test]
    fn compilation_stays_one_playlist_via_album_artist() {
        let mut lib = Library::default();
        lib.insert_song(SongMetadata {
            album_name: Some("Mix".into()),
            album_artist: Some("Various Artists".into()),
            ..tagged(1, "A", "Artist One", "Mix", 1)
        });
        lib.insert_song(SongMetadata {
            album_name: Some("Mix".into()),
            album_artist: Some("Various Artists".into()),
            ..tagged(2, "B", "Artist Two", "Mix", 2)
        });
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1, "one album, not one per track artist");
        assert_eq!(lib.playlists()[0].song_ids, vec![1, 2]);
    }

    #[test]
    fn same_titled_albums_in_different_folders_stay_separate() {
        let mut lib = Library::default();
        lib.insert_song(album_in("/music/Book 1", 1, "A", "Grimes", "Book 1", 1));
        lib.insert_song(album_in("/music/(2022) Book 1", 2, "B", "Grimes", "Book 1", 1));
        lib.rebuild_auto();

        let albums = lib.playlists();
        assert_eq!(albums.len(), 2, "two folders, two playlists");
        assert!(albums.iter().all(|p| p.meta.title == "Book 1"));
    }

    #[test]
    fn untagged_songs_land_in_an_unknown_bucket() {
        let mut lib = Library::default();
        lib.insert_song(song(1, "Loose"));
        lib.rebuild_auto();
        let album = &lib.playlists()[0];
        assert_eq!(album.meta.title, "Unknown Album");
        assert_eq!(album.meta.artist.as_deref(), Some("Unknown Artist"));
    }

    #[test]
    fn custom_playlists_are_added_removed_and_deduped() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        let id = lib.create_custom("Faves");

        assert!(lib.add_song(id, 1));
        assert!(!lib.add_song(id, 1), "duplicate is rejected");
        assert_eq!(lib.songs_of(id).len(), 1);

        assert!(lib.remove_song(id, 1));
        assert!(!lib.remove_song(id, 1), "already gone");
        assert!(lib.songs_of(id).is_empty());
    }

    #[test]
    fn adopt_custom_from_keeps_user_edits_across_a_rescan() {
        // The live library is what the user has been editing...
        let mut live = Library::default();
        live.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        let id = live.create_custom("Faves");
        live.add_song(id, 1);
        live.rename(id, "Renamed");

        // ...while the scan rebuilt a fresh library from an older cache.
        let mut scanned = Library::default();
        scanned.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        scanned.create_custom("Stale");
        scanned.rebuild_auto();

        scanned.adopt_custom_from(&live);

        let customs = scanned.custom_playlists();
        assert_eq!(customs.len(), 1, "the stale custom playlist is dropped");
        assert_eq!(customs[0].id, id);
        assert_eq!(customs[0].meta.title, "Renamed", "the rename survived");
        assert_eq!(customs[0].song_ids, vec![1]);
        assert!(scanned.playlists().iter().any(|p| !p.is_custom()), "autogen kept");
    }

    #[test]
    fn album_playlist_of_finds_the_autogen_owner() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.insert_song(tagged(2, "B", "Grimes", "Art Angels", 1));
        lib.rebuild_auto();
        let custom = lib.create_custom("Faves");
        lib.add_song(custom, 1);

        let visions = lib.album_playlist_of(1).expect("song 1 is in an album playlist");
        assert_eq!(lib.playlist(visions).unwrap().meta.title, "Visions");
        assert_eq!(lib.album_playlist_of(2).map(|id| lib.playlist(id).unwrap().meta.title.as_str()), Some("Art Angels"));
        assert!(lib.album_playlist_of(999).is_none(), "unknown song");
    }

    #[test]
    fn autogen_playlists_are_read_only() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        let auto = lib.playlists()[0].id;

        assert!(!lib.add_song(auto, 1));
        assert!(!lib.remove_song(auto, 1));
        assert!(!lib.rename(auto, "Nope"));
        assert!(lib.remove_playlist(auto).is_none());
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
    fn prune_drops_references_to_missing_songs() {
        let mut lib = Library::default();
        lib.insert_song(tagged(1, "A", "Grimes", "Visions", 1));
        let id = lib.create_custom("Faves");
        lib.add_song(id, 1);
        lib.add_song(id, 999); // never existed

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

        let discography = lib.discography("Grimes");
        let titles: Vec<&str> = discography
            .iter()
            .map(|id| lib.playlist(*id).unwrap().meta.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Art Angels", "Visions"]);
        assert_eq!(lib.discography("Someone").len(), 1);
        assert!(lib.discography("Nobody").is_empty());
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
}
