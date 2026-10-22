use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::ops::{BitOr, BitOrAssign, Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::identity::SongId;
use super::playlist::{
    AlbumOrigin, Playlist, PlaylistId, PlaylistKind, PlaylistStore, SmartKind,
};
use super::song::{ReleaseDate, SongMetadata};
use super::stats::Stats;
use super::user::{SongIdentities, UserData};

/// Which derived view a mutation invalidated.
///
/// This replaces a single global reindex: a playlist edit no longer rebuilds
/// the song-artist set, and a song insert no longer re-sorts the playlist
/// directory. Batches OR the flags and refresh once at the outermost close.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Dirty(u8);

impl Dirty {
    const SONGS: Self = Self(1 << 0);
    const PLAYLISTS: Self = Self(1 << 1);

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Dirty {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Dirty {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// The single source of truth for static library state.
///
/// Holds every song, every playlist, the user's stats, and the persistent
/// path→song-id map. Everything else — artist groupings, album ownership,
/// release dates, the sorted playlist directory, smart views — is derived and
/// refreshed only when the mutation says which domain changed.
#[derive(Default)]
pub struct Library {
    songs: HashMap<SongId, Arc<SongMetadata>>,
    playlists: PlaylistStore,
    stats: Stats,
    /// Persistent path→id map. Used by the scanner to keep an id when a file
    /// moves, and by command-line playback to resolve a path.
    song_ids: BTreeMap<PathBuf, SongId>,
    next_song_id: u64,
    /// Derived lookups, refreshed by [`touch`](Self::touch).
    index: Index,
    /// Bumped on every content change. Views compare it to skip needless
    /// rebuilds; it is owned here so no mutation path can forget it.
    revision: u64,
    /// Nesting depth of an open [`LibraryBatch`].
    batch_depth: u32,
    /// Domains invalidated inside a batch, refreshed once when it closes.
    pending_dirty: Dirty,
    /// Test-only counter: how many times the derived index was refreshed.
    #[cfg(test)]
    reindex_count: usize,
}

/// The derived lookups a [`Library`] answers from instead of rescanning.
#[derive(Default)]
struct Index {
    /// Every artist credited on any song — sorted and deduped.
    artists: BTreeSet<String>,
    /// The album playlist each song belongs to.
    song_album: HashMap<SongId, PlaylistId>,
    /// The playlists each artist is credited on.
    artist_playlists: BTreeMap<String, Vec<PlaylistId>>,
    /// Each playlist's release stamp: the first date any of its songs
    /// declares. Absent for an undated playlist.
    playlist_release: HashMap<PlaylistId, ReleaseDate>,
    /// Persisted playlists in stable order: autogen albums by title, then
    /// custom playlists by title/id.
    sorted_playlists: Vec<PlaylistId>,
    /// Custom playlists in creation order — the "add to playlist" picker.
    custom_playlists: Vec<PlaylistId>,
}

impl Library {
    // --- bulk replacement ----------------------------------------------

    /// Open a batch: mutations made through the returned guard suppress the
    /// per-mutation refresh, and closing it (on drop) refreshes once.
    pub fn batch(&mut self) -> LibraryBatch<'_> {
        self.batch_depth += 1;
        LibraryBatch { lib: self }
    }

    /// Insert many songs, refreshing once instead of once per song.
    pub fn insert_songs(&mut self, songs: impl IntoIterator<Item = SongMetadata>) {
        let mut batch = self.batch();
        for song in songs {
            batch.insert_song(song);
        }
    }

    /// Replace every song. The cache's bulk path; identities are installed
    /// separately with [`install_identities`](Self::install_identities).
    pub fn replace_songs(&mut self, songs: HashMap<SongId, Arc<SongMetadata>>) {
        self.songs = songs;
        self.touch(Dirty::SONGS);
    }

    /// Install user-owned content: custom playlists, stats, and album ids.
    pub fn install_user_data(&mut self, user: UserData) {
        let UserData { playlists, next_playlist_id, album_ids, stats } = user;
        self.playlists.load_persisted(playlists, next_playlist_id);
        self.playlists.set_album_ids(album_ids.into_iter().collect());
        self.stats = stats;
        self.touch(Dirty::PLAYLISTS);
        self.rebuild_smart();
    }

    /// Install the persistent path→song-id map and allocator.
    pub fn install_identities(&mut self, identities: SongIdentities) {
        self.song_ids = identities.paths;
        self.next_song_id = identities.next_id;
    }

    pub fn identities(&self) -> SongIdentities {
        SongIdentities { paths: self.song_ids.clone(), next_id: self.next_song_id }
    }

    /// The persisted user-owned content, ready for `UserStore::save_user`.
    pub fn user_data(&self) -> UserData {
        UserData {
            playlists: self.playlists.user_playlists(),
            next_playlist_id: self.playlists.next_id(),
            album_ids: self.playlists.album_ids().iter().map(|(k, v)| (k.clone(), *v)).collect(),
            stats: self.stats.clone(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    // --- songs ---------------------------------------------------------

    pub fn insert_song(&mut self, song: SongMetadata) -> SongId {
        let id = song.id;
        self.song_ids.insert(song.path.clone(), id);
        self.songs.insert(id, Arc::new(song));
        self.touch(Dirty::SONGS);
        id
    }

    /// Read `path` (canonicalized) into the library, allocating an id when the
    /// path has none. This is the command-line playback entry point.
    pub fn load_song(&mut self, path: &Path) -> anyhow::Result<SongId> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(id) = self.song_ids.get(&path).copied() {
            if self.songs.contains_key(&id) {
                return Ok(id);
            }
        }
        let id = self.alloc_song_id();
        let song = SongMetadata::load(&path, id)?;
        self.insert_song(song);
        Ok(id)
    }

    fn alloc_song_id(&mut self) -> SongId {
        let id = SongId(self.next_song_id);
        self.next_song_id += 1;
        id
    }

    pub fn get(&self, id: SongId) -> Option<&SongMetadata> {
        self.songs.get(&id).map(Arc::as_ref)
    }

    /// The song as a shared handle, for a caller that needs to own it past the
    /// borrow — cheap (an `Arc` bump), not a metadata copy.
    pub fn get_arc(&self, id: SongId) -> Option<Arc<SongMetadata>> {
        self.songs.get(&id).cloned()
    }

    /// Every song, in no particular order.
    pub fn songs(&self) -> impl Iterator<Item = &SongMetadata> {
        self.songs.values().map(Arc::as_ref)
    }

    pub fn song_count(&self) -> usize {
        self.songs.len()
    }

    /// Count one play and refresh only the stats-derived views it affects.
    /// `RecentlyAdded` depends on the song set, not play counts, so a play
    /// never re-sorts it.
    pub fn record_play(&mut self, song: SongId, now: SystemTime) {
        self.stats.record_play(song, now);
        self.rebuild_smart_kinds(&[SmartKind::RecentlyPlayed, SmartKind::MostPlayed]);
    }

    /// Ensure every song in `songs` has an `added` stamp. `first_run` seeds
    /// from the file mtime; later arrivals stamp "now" so a copied file's old
    /// mtime doesn't bury it at the bottom of Recently Added.
    pub fn ensure_added(&mut self, added: impl IntoIterator<Item = (SongId, SystemTime)>) {
        let first_run = self.stats.is_empty();
        let now = SystemTime::now();
        for (id, modified) in added {
            self.stats.ensure_added(id, if first_run { modified } else { now });
        }
        self.revision += 1;
    }

    // --- playlist lookup ----------------------------------------------

    pub fn playlist(&self, id: PlaylistId) -> Option<&Playlist> {
        self.playlists.get(id)
    }

    /// Persisted playlists in stable order: autogen albums by title, then
    /// custom playlists by title/id. Ephemeral and smart views are reached by
    /// id, not listed here.
    pub fn playlists(&self) -> Vec<&Playlist> {
        self.index
            .sorted_playlists
            .iter()
            .filter_map(|id| self.playlists.get_persisted(*id))
            .collect()
    }

    /// A playlist's songs, resolving ids to metadata. Ids no longer in the
    /// library (moved/deleted files) are skipped rather than yielding a
    /// dangling reference.
    pub fn songs_of(&self, id: PlaylistId) -> Vec<&SongMetadata> {
        self.playlist(id)
            .map(|p| {
                p.song_ids
                    .iter()
                    .filter_map(|s| self.songs.get(s))
                    .map(Arc::as_ref)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The autogen album playlist a song belongs to, if any.
    pub fn album_playlist_of(&self, song: SongId) -> Option<PlaylistId> {
        self.index.song_album.get(&song).copied()
    }

    /// A playlist's release date — the first its songs declare, packed as
    /// `YYYYMMDD`. `None` when no song carries one.
    pub fn release(&self, id: PlaylistId) -> Option<ReleaseDate> {
        self.index.playlist_release.get(&id).copied()
    }

    // --- playlist mutation --------------------------------------------

    fn alloc_playlist_id(&mut self) -> PlaylistId {
        debug_assert!(
            self.playlists.next_id() < PlaylistId::SMART_BASE,
            "playlist id counter overflowed into the smart range"
        );
        self.playlists.alloc_id()
    }

    /// Create an empty, user-owned playlist. Returns its id.
    pub fn create_custom(&mut self, title: impl Into<String>) -> PlaylistId {
        let id = self.alloc_playlist_id();
        self.playlists.insert_persisted(Playlist {
            id,
            kind: PlaylistKind::Custom { title: title.into() },
            song_ids: Vec::new(),
        });
        self.touch(Dirty::PLAYLISTS);
        id
    }

    /// Create a temporary playlist pre-filled with `songs` — the backing for
    /// an ad-hoc view (a selection played as one). Never persisted or indexed;
    /// dropped by a rescan and when its tab closes.
    pub fn create_temporary(&mut self, title: impl Into<String>, songs: Vec<SongId>) -> PlaylistId {
        let id = self.alloc_playlist_id();
        self.playlists.insert_ephemeral(Playlist {
            id,
            kind: PlaylistKind::Temporary { title: title.into() },
            song_ids: songs,
        });
        self.revision += 1;
        id
    }

    pub fn add_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        self.add_songs(playlist, &[song])
    }

    /// Add several songs to a custom playlist at once, refreshing once.
    pub fn add_songs(&mut self, playlist: PlaylistId, songs: &[SongId]) -> bool {
        let Some(p) = self.playlists.get_persisted_mut(playlist) else { return false };
        if !p.is_custom() {
            return false;
        }
        let mut added = false;
        if songs.len() > 8 {
            let mut present: HashSet<SongId> = p.song_ids.iter().copied().collect();
            for &song in songs {
                if present.insert(song) {
                    p.song_ids.push(song);
                    added = true;
                }
            }
        } else {
            for &song in songs {
                if !p.contains(song) {
                    p.song_ids.push(song);
                    added = true;
                }
            }
        }
        if added {
            self.touch(Dirty::PLAYLISTS);
        }
        added
    }

    pub fn remove_song(&mut self, playlist: PlaylistId, song: SongId) -> bool {
        self.remove_songs(playlist, &[song])
    }

    /// Remove several songs from a custom playlist at once, refreshing once.
    pub fn remove_songs(&mut self, playlist: PlaylistId, songs: &[SongId]) -> bool {
        let Some(p) = self.playlists.get_persisted_mut(playlist) else { return false };
        if !p.is_custom() {
            return false;
        }
        let before = p.song_ids.len();
        if songs.len() > 8 {
            let doomed: HashSet<SongId> = songs.iter().copied().collect();
            p.song_ids.retain(|s| !doomed.contains(s));
        } else {
            p.song_ids.retain(|s| !songs.contains(s));
        }
        let removed = p.song_ids.len() != before;
        if removed {
            self.touch(Dirty::PLAYLISTS);
        }
        removed
    }

    pub fn rename(&mut self, playlist: PlaylistId, title: impl Into<String>) -> bool {
        let Some(p) = self.playlists.get_persisted_mut(playlist) else { return false };
        let PlaylistKind::Custom { title: current } = &mut p.kind else { return false };
        *current = title.into();
        // A title only affects the sorted directory.
        self.touch(Dirty::PLAYLISTS);
        true
    }

    /// Delete a custom or temporary playlist. Autogen and smart playlists
    /// can't be removed — both are rebuilt from their source of truth.
    pub fn remove_playlist(&mut self, playlist: PlaylistId) -> Option<Playlist> {
        match self.playlists.get(playlist) {
            Some(p) if p.is_temporary() => {
                let removed = self.playlists.remove(playlist);
                self.revision += 1;
                removed
            }
            Some(p) if !p.is_auto() && !p.is_smart() => {
                let removed = self.playlists.remove(playlist);
                self.touch(Dirty::PLAYLISTS);
                removed
            }
            _ => None,
        }
    }

    /// Every custom playlist, in creation order — the "add to playlist"
    /// picker.
    pub fn custom_playlists(&self) -> Vec<&Playlist> {
        self.index
            .custom_playlists
            .iter()
            .filter_map(|id| self.playlists.get_persisted(*id))
            .collect()
    }

    /// The built-in smart playlists, in [`SmartKind::ALL`] order.
    pub fn smart_playlists(&self) -> Vec<&Playlist> {
        self.playlists.smart_playlists().iter().collect()
    }

    /// Recompute every smart playlist from the current songs and stats.
    /// Existing ids are reserved constants, so open tabs survive a rebuild.
    pub fn rebuild_smart(&mut self) {
        self.rebuild_smart_kinds(&SmartKind::ALL);
    }

    /// Recompute only `kinds` — a play touches the two played views, not
    /// `RecentlyAdded`, which depends on the song set instead.
    fn rebuild_smart_kinds(&mut self, kinds: &[SmartKind]) {
        // Compute every selection while borrowing the songs immutably, then
        // apply them — no `Arc` clone of the whole library just to sidestep
        // the mutable borrow of the smart store.
        let selected: Vec<(SmartKind, Vec<SongId>)> = kinds
            .iter()
            .map(|&kind| {
                (
                    kind,
                    kind.select(self.songs.values().map(Arc::as_ref), &self.stats),
                )
            })
            .collect();

        let mut changed = false;
        for (kind, ids) in selected {
            let playlist = self.playlists.smart_mut(kind);
            if playlist.song_ids != ids {
                playlist.song_ids = ids;
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
        }
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

    /// Every playlist containing a song crediting `artist` — compilations and
    /// custom playlists included ("artist appears in").
    pub fn artist_appears_in(&self, artist: &str) -> Vec<PlaylistId> {
        let mut ids = self.artist_playlists(artist, false);
        ids.sort_by(|a, b| self.title_of(*a).cmp(self.title_of(*b)));
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
                            || self.playlists.get_persisted(*id).is_some_and(|p| p.is_auto())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn title_of(&self, id: PlaylistId) -> &str {
        self.playlist(id).map_or("", Playlist::title)
    }

    /// Newest release first, then by title for a stable order. The packed
    /// date sorts chronologically on its own.
    pub(crate) fn sort_by_year(&self, ids: &mut [PlaylistId]) {
        ids.sort_by(|a, b| {
            self.release(*b)
                .cmp(&self.release(*a))
                .then_with(|| self.title_of(*a).cmp(self.title_of(*b)))
        });
    }

    // --- maintenance ---------------------------------------------------

    /// Drop playlist/stats/id references to songs no longer in the library.
    /// Returns how many playlist references were dropped.
    pub fn prune(&mut self) -> usize {
        let songs = &self.songs;
        let dropped = self.playlists.prune(|id| songs.contains_key(&id));
        let stats_before = self.stats.len();
        self.stats.prune(|id| songs.contains_key(&id));
        self.song_ids.retain(|_, id| songs.contains_key(id));
        if dropped > 0 || stats_before != self.stats.len() {
            self.touch(Dirty::PLAYLISTS);
        }
        dropped
    }

    /// Drop every temporary playlist. A rescan invalidates their snapshots.
    pub fn drop_temporary(&mut self) {
        self.playlists.drop_ephemeral();
        self.revision += 1;
    }

    /// Replace the song set with a completed scan's result, keeping user
    /// playlists and ids intact. `added` carries file mtimes for songs the
    /// scan hadn't seen before.
    pub fn adopt_scan(
        &mut self,
        songs: HashMap<SongId, Arc<SongMetadata>>,
        identities: SongIdentities,
        added: Vec<(SongId, SystemTime)>,
    ) -> usize {
        let user = self.user_data();
        let mut batch = self.batch();
        batch.replace_songs(songs);
        batch.install_identities(identities);
        let dropped = batch.prune();
        batch.drop_temporary();
        batch.rebuild_auto();
        batch.ensure_added(added);
        batch.rebuild_smart();
        // Restore the user content that `replace_songs`/`prune` don't touch;
        // batch methods above are the only mutations that could have changed
        // it, and they preserve the store.
        debug_assert_eq!(batch.playlists.next_id(), user.next_playlist_id);
        dropped
    }

    /// Rebuild every autogen (album) playlist from the current song tags,
    /// leaving custom playlists untouched.
    ///
    /// Album ids are allocated on first sight and remembered by
    /// [`PlaylistStore`], so a rescan doesn't churn ids.
    pub fn rebuild_auto(&mut self) {
        // Group by borrowed keys so a rebuild doesn't allocate a tuple per
        // song. The directory keeps same-titled releases separate.
        let mut groups: BTreeMap<(&str, &str, &Path), Vec<SongId>> = BTreeMap::new();
        for song in self.songs.values() {
            let dir = song.path.parent().unwrap_or_else(|| Path::new(""));
            groups
                .entry((song.album(), song.album_artist(), dir))
                .or_default()
                .push(song.id);
        }

        for ids in groups.values_mut() {
            ids.sort_by_cached_key(|id| {
                self.songs
                    .get(id)
                    .map(|s| {
                        (
                            s.track_position.unwrap_or(u16::MAX),
                            s.display_title().into_owned(),
                        )
                    })
                    .unwrap_or((u16::MAX, String::new()))
            });
        }

        let grouped: Vec<(AlbumOrigin, Vec<SongId>)> = groups
            .into_iter()
            .map(|((name, artist, dir), ids)| {
                (AlbumOrigin::new(name, artist, dir.to_path_buf()), ids)
            })
            .collect();

        let mut live = HashSet::new();
        for (origin, ids) in grouped {
            let id = self.playlists.album_id(&origin).unwrap_or_else(|| {
                let id = self.alloc_playlist_id();
                self.playlists.insert_album_id(origin.clone(), id);
                id
            });
            live.insert(id);
            self.playlists.insert_persisted(Playlist {
                id,
                kind: PlaylistKind::Auto(origin),
                song_ids: ids,
            });
        }

        // Drop albums whose origin disappeared, and their id entries.
        let stale: Vec<PlaylistId> = self
            .playlists
            .iter_persisted()
            .filter(|p| p.is_auto() && !live.contains(&p.id))
            .map(|p| p.id)
            .collect();
        for id in stale {
            if let Some(removed) = self.playlists.remove(id) {
                if let Some(origin) = removed.origin() {
                    self.playlists.remove_album_id(origin);
                }
            }
        }

        self.touch(Dirty::SONGS | Dirty::PLAYLISTS);
    }

    /// Hand the song map over so a test can drop songs (simulating deletion).
    #[cfg(test)]
    pub(crate) fn take_songs_for_test(&mut self) -> HashMap<SongId, Arc<SongMetadata>> {
        std::mem::take(&mut self.songs)
    }

    // --- internals -----------------------------------------------------

    /// Invalidate `dirty`'s derived views. Inside a batch this only ORs the
    /// flags; the refresh happens once when the outermost batch closes.
    fn touch(&mut self, dirty: Dirty) {
        if dirty.is_empty() {
            return;
        }
        if self.batch_depth > 0 {
            self.pending_dirty |= dirty;
            return;
        }
        self.refresh(dirty);
        self.revision += 1;
    }

    fn refresh(&mut self, dirty: Dirty) {
        #[cfg(test)]
        {
            self.reindex_count += 1;
        }
        if dirty.contains(Dirty::SONGS) {
            self.refresh_artists();
            self.refresh_song_album();
        }
        if dirty.intersects(Dirty::SONGS | Dirty::PLAYLISTS) {
            self.refresh_credits_and_releases();
        }
        if dirty.contains(Dirty::PLAYLISTS) {
            self.refresh_directory();
        }
    }

    fn refresh_artists(&mut self) {
        self.index.artists = self
            .songs
            .values()
            .flat_map(|song| song.artists.iter().cloned())
            .collect();
    }

    fn refresh_song_album(&mut self) {
        let mut map = HashMap::new();
        for playlist in self.playlists.iter_persisted() {
            if !playlist.is_auto() {
                continue;
            }
            for song in &playlist.song_ids {
                map.insert(*song, playlist.id);
            }
        }
        self.index.song_album = map;
    }

    fn refresh_credits_and_releases(&mut self) {
        let mut credits: BTreeMap<String, Vec<PlaylistId>> = BTreeMap::new();
        let mut releases = HashMap::new();
        for playlist in self.playlists.iter_persisted() {
            // The first song that declares a date stamps the playlist.
            let release = playlist
                .song_ids
                .iter()
                .filter_map(|id| self.songs.get(id))
                .find_map(|song| song.date);
            for song_id in &playlist.song_ids {
                let Some(song) = self.songs.get(song_id) else { continue };
                for artist in &song.artists {
                    credits.entry(artist.clone()).or_default().push(playlist.id);
                }
            }
            if let Some(release) = release {
                releases.insert(playlist.id, release);
            }
        }
        for ids in credits.values_mut() {
            ids.sort();
            ids.dedup();
        }
        self.index.artist_playlists = credits;
        self.index.playlist_release = releases;
    }

    fn refresh_directory(&mut self) {
        let mut custom = Vec::new();
        let mut sorted = Vec::new();
        for playlist in self.playlists.iter_persisted() {
            if playlist.is_custom() {
                custom.push(playlist.id);
            }
            sorted.push(playlist.id);
        }
        custom.sort_by_key(|id| id.0);
        sorted.sort_by(|a, b| {
            let pa = self.playlists.get_persisted(*a).expect("indexed persisted playlist");
            let pb = self.playlists.get_persisted(*b).expect("indexed persisted playlist");
            (pa.is_custom(), pa.title(), a.0).cmp(&(pb.is_custom(), pb.title(), b.0))
        });
        self.index.custom_playlists = custom;
        self.index.sorted_playlists = sorted;
    }
}

/// RAII guard returned by [`Library::batch`]: suppresses per-mutation refreshes
/// and refreshes once when dropped (if anything changed).
pub struct LibraryBatch<'a> {
    lib: &'a mut Library,
}

impl Deref for LibraryBatch<'_> {
    type Target = Library;
    fn deref(&self) -> &Library {
        self.lib
    }
}

impl DerefMut for LibraryBatch<'_> {
    fn deref_mut(&mut self) -> &mut Library {
        self.lib
    }
}

impl Drop for LibraryBatch<'_> {
    fn drop(&mut self) {
        self.lib.batch_depth -= 1;
        if self.lib.batch_depth == 0 && !self.lib.pending_dirty.is_empty() {
            let dirty = std::mem::take(&mut self.lib.pending_dirty);
            self.lib.refresh(dirty);
            self.lib.revision += 1;
        }
    }
}

/// Shuffle `items` in place — Fisher–Yates driven by a xorshift64\* seeded
/// from the hasher's per-process randomness. One shuffle button doesn't
/// warrant a `rand` dependency.
pub fn shuffle<T>(items: &mut [T]) {
    let mut state = std::collections::hash_map::RandomState::new().build_hasher().finish() | 1;
    for i in (1..items.len()).rev() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let j = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) % (i as u64 + 1)) as usize;
        items.swap(i, j);
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
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::SystemTime;

    const EPOCH: SystemTime = SystemTime::UNIX_EPOCH;

    fn song(id: SongId, name: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: PathBuf::from(format!("/music/{name}.flac")),
            format: Some(crate::model::AudioFormat::Flac),
            song_name: Some(name.to_string()),
            ..Default::default()
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
        song.date = Some(ReleaseDate::new(year, None, None));
        song
    }

    fn dated(id: SongId, name: &str, artist: &str, album: &str, date: (u16, u8, u8)) -> SongMetadata {
        let mut song = tagged(id, name, artist, album, 1);
        song.date = Some(ReleaseDate::new(date.0, Some(date.1), Some(date.2)));
        song
    }

    #[test]
    fn insert_songs_reindexes_once() {
        let mut lib = Library::default();
        let before = lib.reindex_count;
        lib.insert_songs([
            song(SongId(1), "One"),
            song(SongId(2), "Two"),
            song(SongId(3), "Three"),
        ]);
        assert_eq!(lib.reindex_count - before, 1, "one rebuild for the whole batch");
        assert_eq!(lib.song_count(), 3);
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let mut lib = Library::default();
        let id = lib.insert_song(song(SongId(7), "Seven"));
        assert_eq!(id, SongId(7));
        assert_eq!(lib.get(SongId(7)).unwrap().song_name.as_deref(), Some("Seven"));
        assert!(lib.get(SongId(8)).is_none());
    }

    #[test]
    fn reinserting_same_id_replaces() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(7), "Seven"));
        lib.insert_song(song(SongId(7), "Seven (Remastered)"));
        assert_eq!(lib.song_count(), 1);
        assert_eq!(lib.get(SongId(7)).unwrap().song_name.as_deref(), Some("Seven (Remastered)"));
    }

    fn album_titles(lib: &Library) -> Vec<String> {
        lib.playlists()
            .into_iter()
            .filter(|p| !p.is_custom())
            .map(|p| p.title().to_string())
            .collect()
    }

    #[test]
    fn autogen_groups_songs_by_album_in_track_order() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "B", "A", "Album", 2));
        lib.insert_song(tagged(SongId(2), "A", "A", "Album", 1));
        lib.insert_song(tagged(SongId(3), "C", "A", "Other", 1));
        lib.rebuild_auto();

        assert_eq!(album_titles(&lib), vec!["Album", "Other"]);
        let album = lib.playlists().into_iter().find(|p| p.title() == "Album").unwrap();
        let order: Vec<&str> =
            lib.songs_of(album.id).iter().map(|s| s.song_name.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["A", "B"], "track order, not insertion order");
    }

    #[test]
    fn rebuild_is_idempotent_and_keeps_ids() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.rebuild_auto();
        let first = lib.playlists()[0].id;
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].id, first, "id survives a rebuild");
    }

    #[test]
    fn playlist_ids_are_stable_and_distinct() {
        let mut first = Library::default();
        let custom = first.create_custom("Faves");
        first.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        first.rebuild_auto();
        let album = first.playlists().into_iter().find(|p| p.is_auto()).unwrap().id;
        let smart = first.smart_playlists()[0].id;

        assert_ne!(custom, album);
        assert_ne!(custom, smart);
        assert!(smart.0 > u64::MAX - 8, "smart ids are the reserved top values");

        // A rebuild in place keeps the album id.
        let mut second = Library::default();
        second.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        second.rebuild_auto();
        second.rebuild_auto();
        let album_again = second.playlists()[0].id;
        assert_eq!(second.playlists()[0].id, album_again);
    }

    #[test]
    fn compilation_stays_one_playlist_via_album_artist() {
        let mut lib = Library::default();
        let mut one = tagged(SongId(1), "A", "Artist 1", "Comp", 1);
        one.album_artist = Some("Various Artists".to_string());
        let mut two = tagged(SongId(2), "B", "Artist 2", "Comp", 2);
        two.album_artist = Some("Various Artists".to_string());
        lib.insert_song(one);
        lib.insert_song(two);
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1, "one album, not one per track artist");
    }

    #[test]
    fn same_titled_albums_in_different_folders_stay_separate() {
        let mut lib = Library::default();
        lib.insert_song(album_in("/music/a", SongId(1), "A", "X", "Same", 1));
        lib.insert_song(album_in("/music/b", SongId(2), "B", "X", "Same", 1));
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 2, "same title, different folders");
    }

    #[test]
    fn untagged_songs_land_in_an_unknown_bucket() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "mystery"));
        lib.rebuild_auto();
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlists()[0].title(), "Unknown Album");
    }

    #[test]
    fn custom_playlists_are_added_removed_and_deduped() {
        let mut lib = Library::default();
        let id = lib.create_custom("Faves");
        assert_eq!(lib.custom_playlists().len(), 1);
        assert!(lib.add_song(id, SongId(1)));
        assert!(!lib.add_song(id, SongId(1)), "already present");
        assert_eq!(lib.songs_of(id).len(), 0, "song 1 isn't in the library");
        lib.insert_song(song(SongId(1), "One"));
        assert_eq!(lib.songs_of(id).len(), 1);
        assert!(lib.remove_playlist(id).is_some());
        assert!(lib.custom_playlists().is_empty());
    }

    #[test]
    fn rescanned_songs_keep_custom_playlists_and_album_ids_disjoint() {
        let mut lib = Library::default();
        let custom = lib.create_custom("Faves");
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.add_song(custom, SongId(1));

        let songs = lib.take_songs_for_test();
        lib.replace_songs(songs);
        lib.prune();
        lib.rebuild_auto();

        assert!(lib.custom_playlists().iter().any(|p| p.id == custom));
        assert_eq!(lib.songs_of(custom).len(), 1);
        let album = lib.playlists().into_iter().find(|p| p.title() == "Album").unwrap();
        assert_ne!(album.id, custom);
    }

    #[test]
    fn album_playlist_of_finds_the_autogen_owner() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.insert_song(tagged(SongId(2), "B", "X", "Other", 1));
        lib.rebuild_auto();
        let album = lib.playlists().into_iter().find(|p| p.title() == "Album").unwrap();
        assert_eq!(lib.album_playlist_of(SongId(1)), Some(album.id));
        assert_eq!(lib.album_playlist_of(SongId(999)), None);
    }

    #[test]
    fn autogen_playlists_are_read_only() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.rebuild_auto();
        let auto = lib.playlists()[0].id;
        assert!(!lib.add_song(auto, SongId(1)), "can't add to an autogen playlist");
        assert!(!lib.remove_song(auto, SongId(1)), "can't remove from one either");
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
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.rebuild_auto();

        let temp = lib.create_temporary("Folder", vec![SongId(1)]);
        assert!(lib.playlist(temp).is_some_and(|p| p.is_temporary()));
        assert!(!lib.playlists().iter().any(|p| p.id == temp));
        assert!(lib.custom_playlists().is_empty());
        assert_eq!(lib.playlist(temp).map(|p| p.song_ids.clone()), Some(vec![SongId(1)]));
        assert!(lib.remove_playlist(temp).is_some());
        assert!(lib.playlist(temp).is_none());
    }

    #[test]
    fn temporary_playlists_stay_out_of_the_artist_index() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        lib.create_temporary("Mix", vec![SongId(1)]);
        assert_eq!(lib.artist_appears_in("Grimes").len(), 1, "the album only");
    }

    #[test]
    fn prune_drops_references_to_missing_songs() {
        let mut lib = Library::default();
        let id = lib.create_custom("Faves");
        lib.insert_song(song(SongId(1), "One"));
        lib.add_song(id, SongId(1));
        lib.add_song(id, SongId(2));
        assert_eq!(lib.prune(), 1);
        assert_eq!(lib.songs_of(id).len(), 1);
    }

    #[test]
    fn discography_lists_an_artists_album_playlists() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "Grimes", "Visions", 1));
        lib.insert_song(tagged(SongId(2), "B", "Grimes", "Art Angels", 1));
        lib.insert_song(tagged(SongId(3), "C", "Someone", "Other", 1));
        lib.rebuild_auto();

        let titles: Vec<&str> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        assert_eq!(titles, vec!["Art Angels", "Visions"]);
        assert_eq!(lib.discography("Someone").len(), 1);
        assert!(lib.discography("Nobody").is_empty());
    }

    #[test]
    fn discography_sorts_newest_first_with_undated_last() {
        let mut lib = Library::default();
        lib.insert_song(with_year(tagged(SongId(1), "A", "Grimes", "Old", 1), 1999));
        lib.insert_song(with_year(tagged(SongId(2), "B", "Grimes", "New", 1), 2020));
        lib.insert_song(with_year(tagged(SongId(3), "C", "Grimes", "Middle", 1), 2010));
        lib.insert_song(tagged(SongId(4), "D", "Grimes", "Undated", 1));
        lib.rebuild_auto();

        let titles: Vec<&str> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        assert_eq!(titles, vec!["New", "Middle", "Old", "Undated"]);
        assert_eq!(lib.release(lib.discography("Grimes")[0]).map(ReleaseDate::year), Some(2020));
    }

    #[test]
    fn discography_breaks_year_ties_on_month_and_day() {
        let mut lib = Library::default();
        lib.insert_song(dated(SongId(1), "A", "Grimes", "June", (2010, 6, 1)));
        lib.insert_song(dated(SongId(2), "B", "Grimes", "September", (2010, 9, 1)));
        lib.insert_song(dated(SongId(3), "C", "Grimes", "DayOnly", (2010, 6, 2)));
        lib.insert_song(with_year(tagged(SongId(4), "D", "Grimes", "NoDate", 1), 2010));
        lib.rebuild_auto();

        let titles: Vec<&str> =
            lib.discography("Grimes").iter().map(|id| lib.title_of(*id)).collect();
        assert_eq!(titles, vec!["September", "DayOnly", "June", "NoDate"]);
    }

    #[test]
    fn artist_appears_in_includes_custom_playlists() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "Grimes", "Visions", 1));
        lib.rebuild_auto();
        let custom = lib.create_custom("Faves");
        lib.add_song(custom, SongId(1));

        assert_eq!(lib.discography("Grimes").len(), 1, "album playlists only");
        assert_eq!(lib.artist_appears_in("Grimes").len(), 2, "album + custom");
    }

    #[test]
    fn artists_are_deduped_and_sorted() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "Zed", "Album", 1));
        lib.insert_song(tagged(SongId(2), "B", "Amy", "Album", 2));
        lib.insert_song(tagged(SongId(3), "C", "Zed", "Other", 1));
        assert_eq!(artist_set(&lib), BTreeSet::from(["Amy".to_string(), "Zed".to_string()]));
    }

    fn smart(lib: &Library, kind: SmartKind) -> PlaylistId {
        lib.smart_playlists()
            .iter()
            .find(|p| p.title() == kind.title())
            .expect("smart playlist exists")
            .id
    }

    #[test]
    fn rebuild_smart_is_idempotent_and_keeps_ids() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "X", "Album", 1));
        lib.stats.record_play(SongId(1), EPOCH);
        lib.rebuild_smart();
        let before = lib.smart_playlists().iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(before.len(), 3, "one per SmartKind::ALL");

        lib.rebuild_smart();
        let after = lib.smart_playlists().iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(after, before, "a rebuild keeps every PlaylistId");
        let most = smart(&lib, SmartKind::MostPlayed);
        assert_eq!(lib.playlist(most).unwrap().song_ids, vec![SongId(1)]);
    }

    #[test]
    fn smart_playlists_are_absent_from_playlists_and_the_artist_index() {
        let mut lib = Library::default();
        lib.insert_song(tagged(SongId(1), "A", "Grimes", "Visions", 1));
        lib.stats.record_play(SongId(1), EPOCH);
        lib.rebuild_auto();
        lib.rebuild_smart();

        assert!(!lib.playlists().iter().any(|p| p.is_smart()));
        assert_eq!(lib.smart_playlists().len(), 3);
        assert_eq!(lib.artist_appears_in("Grimes").len(), 1);
        assert_eq!(lib.discography("Grimes").len(), 1);
    }

    #[test]
    fn smart_playlists_are_read_only() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.rebuild_smart();
        let id = smart(&lib, SmartKind::MostPlayed);

        assert!(!lib.add_song(id, SongId(1)));
        assert!(!lib.remove_song(id, SongId(1)));
        assert!(!lib.rename(id, "Nope"));
        assert!(lib.remove_playlist(id).is_none());
    }

    #[test]
    fn rebuild_smart_regenerates_from_current_stats() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.insert_song(song(SongId(2), "Two"));
        lib.rebuild_smart();
        let most = smart(&lib, SmartKind::MostPlayed);
        assert!(lib.playlist(most).unwrap().song_ids.is_empty());

        lib.stats.record_play(SongId(2), EPOCH);
        lib.rebuild_smart();
        assert_eq!(lib.playlist(most).unwrap().song_ids, vec![SongId(2)]);
    }

    #[test]
    fn rebuild_smart_drops_deleted_songs() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.stats.record_play(SongId(1), EPOCH);
        lib.rebuild_smart();

        let mut songs = lib.take_songs_for_test();
        songs.remove(&SongId(1));
        lib.replace_songs(songs);
        lib.rebuild_smart();
        let most = smart(&lib, SmartKind::MostPlayed);
        assert!(lib.playlist(most).unwrap().song_ids.is_empty());
    }

    #[test]
    fn prune_and_rebuild_drop_deleted_songs_from_smart_lists() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.insert_song(song(SongId(2), "Two"));
        lib.stats.record_play(SongId(1), EPOCH);
        lib.stats.record_play(SongId(2), EPOCH);
        lib.rebuild_smart();

        let mut songs = lib.take_songs_for_test();
        songs.remove(&SongId(1));
        lib.replace_songs(songs);
        lib.prune();
        lib.rebuild_smart();
        let most = smart(&lib, SmartKind::MostPlayed);
        assert_eq!(lib.playlist(most).unwrap().song_ids, vec![SongId(2)]);
    }

    #[test]
    fn a_recorded_play_fills_both_played_lists() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        let revision = lib.revision();
        lib.record_play(SongId(1), EPOCH);

        let recent = smart(&lib, SmartKind::RecentlyPlayed);
        let most = smart(&lib, SmartKind::MostPlayed);
        assert_eq!(lib.playlist(recent).unwrap().song_ids, vec![SongId(1)]);
        assert_eq!(lib.playlist(most).unwrap().song_ids, vec![SongId(1)]);
        assert!(lib.revision() > revision, "the smart views bump the revision");
    }

    #[test]
    fn plays_survive_a_user_data_round_trip_and_rebuild() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.stats.record_play(SongId(1), EPOCH);

        let bytes = serde_json::to_vec(&lib.user_data()).unwrap();
        let reloaded_user: UserData = serde_json::from_slice(&bytes).unwrap();
        let mut reloaded = Library::default();
        reloaded.insert_song(song(SongId(1), "One"));
        reloaded.install_user_data(reloaded_user);

        let recent = smart(&reloaded, SmartKind::RecentlyPlayed);
        let most = smart(&reloaded, SmartKind::MostPlayed);
        assert_eq!(reloaded.playlist(recent).unwrap().song_ids, vec![SongId(1)]);
        assert_eq!(reloaded.playlist(most).unwrap().song_ids, vec![SongId(1)]);
    }

    #[test]
    fn smart_playlists_are_never_persisted() {
        let mut lib = Library::default();
        lib.insert_song(song(SongId(1), "One"));
        lib.stats.record_play(SongId(1), EPOCH);
        lib.rebuild_smart();
        let persisted = lib.user_data().playlists;
        let mut reloaded = Library::default();
        reloaded.install_user_data(UserData { playlists: persisted, ..Default::default() });
        assert_eq!(reloaded.smart_playlists().len(), 3);
        assert!(reloaded.smart_playlists().iter().all(|p| p.song_ids.is_empty()));
    }

    #[test]
    fn shuffle_keeps_every_item_but_rarely_the_order() {
        let mut items: Vec<u32> = (0..64).collect();
        let original = items.clone();
        shuffle(&mut items);
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, original, "a shuffle is a permutation");
        assert_ne!(items, original, "64 items don't shuffle back to identity");
    }
}
