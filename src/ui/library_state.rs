//! The shared, observable library state.
//!
//! Exactly one `Entity<LibraryState>` exists per window, alongside
//! [`PlaybackState`](super::state::PlaybackState). It owns the [`Library`],
//! its persistence cache, and the scan — and the views **observe** it, so
//! adding another library-reading module is a subscription, not a timer.
//!
//! The scan runs on the background executor: the cache is moved off the UI
//! thread, synced against the filesystem, and handed back. The library is
//! installed from the cache immediately, so a launch shows the last-known
//! library before the scan even finishes.
//!
//! Scan roots come from [`ConfigState`](super::config_state::ConfigState) (the
//! synced-path list), which this state observes: changing the list triggers a
//! rescan, and removing a root forgets whatever no remaining root still covers.
//!
//! What lives here is the *library*, not any one view's cursor: which row is
//! selected belongs to the playlist tab showing it, since each tab keeps its
//! own.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{Context, Entity, Subscription};

use crate::model::search::Query;
use crate::model::{
    shuffle, FolderMode, Library, LibraryCache, Order, PlaylistId, QueueEntry, Scope, Selection,
    SongId, UserStore,
};
use crate::ui::config_state::ConfigState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::playback_state::PlaybackState;

/// A cross-view intent raised by a row and consumed by the tab container — so
/// a row anywhere can ask for a tab without holding a handle to the container
/// that owns it.
#[derive(Clone)]
pub enum Request {
    /// Open (or focus) a playlist's tab.
    OpenPlaylist(PlaylistId),
    /// Open a library view scoped to an artist's discography.
    ArtistView(String),
    /// Play a folder and open its folder view.
    FolderPlay(PathBuf, FolderMode),
    /// Open a folder's view without playing it.
    FolderOpen(PathBuf, FolderMode),
    /// Show a song's context menu.
    SongMenu(SongMenuRequest),
}

pub struct LibraryState {
    library: Library,
    /// The disposable, file-derived song cache. Taken out while a scan is in
    /// flight; user writes go through [`UserStore`] and never touch it.
    cache: Option<LibraryCache>,
    /// The user-owned files (playlists, stats, identities) beside the cache.
    store: UserStore,
    /// The synced-path list, mirrored from the config so the scan and the
    /// cache-pruning agree on what's covered.
    roots: Vec<PathBuf>,
    /// The playlist whose tab is active, for the rail's highlight.
    selected: Option<PlaylistId>,
    /// Cross-view intents a row raised (open a playlist/artist/folder, show a
    /// song menu). The tab container drains them.
    requests: VecDeque<Request>,
    scanning: bool,
    /// A rescan was requested while one was already in flight.
    rescan_pending: bool,
    _observe_config: Subscription,
}

impl LibraryState {
    /// Build the state, install the cached library for instant display, and
    /// kick off a background sync. Roots come from `config`.
    pub fn new(cache: LibraryCache, config: Entity<ConfigState>, cx: &mut Context<Self>) -> Self {
        let roots = config.read(cx).roots().to_vec();

        // The config is the source of truth for roots, so drop any cached songs
        // a previous run left behind that no configured root covers.
        let mut cache = cache;
        cache.retain_roots(&roots);
        let store = UserStore::open(cache.path());
        let mut library = Library::default();
        // One batch: the cache install and the user-data load each refresh on
        // their own — collapsed to a single rebuild at the close of the batch.
        {
            let mut batch = library.batch();
            cache.install(&mut batch);
            batch.install_identities(store.load_identities());
            batch.install_user_data(store.load_user());
            // Cached songs from a root no longer configured, or user
            // references to songs the cache no longer holds.
            batch.prune();
        }

        let observe_config = cx.observe(&config, |this, config, cx| {
            let roots = config.read(cx).roots().to_vec();
            this.set_roots(roots, cx);
        });

        let mut this = Self {
            library,
            cache: Some(cache),
            store,
            roots,
            selected: None,
            requests: VecDeque::new(),
            scanning: false,
            rescan_pending: false,
            _observe_config: observe_config,
        };
        this.ensure_selection();
        this.rescan(cx);
        this
    }

    // --- read-only accessors: what views render from -------------------

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn selected(&self) -> Option<PlaylistId> {
        self.selected
    }

    /// A counter bumped whenever the songs or playlists change. Views compare it
    /// to their last-seen value, so a highlight-only notification (a tab
    /// switch) doesn't trigger a rebuild.
    pub fn revision(&self) -> u64 {
        self.library.revision()
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    // --- cross-view intents --------------------------------------------

    /// Raise a cross-view intent for the tab container to drain.
    pub fn request(&mut self, request: Request, cx: &mut Context<Self>) {
        self.requests.push_back(request);
        cx.notify();
    }

    /// Take the next pending intent, if any.
    pub fn take_request(&mut self) -> Option<Request> {
        self.requests.pop_front()
    }

    // --- actions -------------------------------------------------------

    /// Mark a playlist as the active one (the rail highlight follows the tab
    /// container's active tab).
    pub fn select(&mut self, id: PlaylistId, cx: &mut Context<Self>) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            cx.notify();
        }
    }

    /// Clear the active playlist — no tab is showing one, so the rail drops
    /// its highlight (the highlight follows the tab container's active tab).
    pub fn deselect(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.selected = None;
            cx.notify();
        }
    }

    /// Create a custom playlist and select it. Returns its id so the caller
    /// can open a tab for it.
    pub fn new_playlist(&mut self, cx: &mut Context<Self>) -> PlaylistId {
        let id = self.library.create_custom("New Playlist");
        self.selected = Some(id);
        self.persist();
        cx.notify();
        id
    }

    pub fn add_song(&mut self, playlist: PlaylistId, song: SongId, cx: &mut Context<Self>) {
        if self.library.add_song(playlist, song) {
            self.persist();
            cx.notify();
        }
    }

    /// Add several songs to a custom playlist at once (a bulk selection).
    pub fn add_songs(&mut self, playlist: PlaylistId, songs: Vec<SongId>, cx: &mut Context<Self>) {
        if self.library.add_songs(playlist, &songs) {
            self.persist();
            cx.notify();
        }
    }

    pub fn remove_song(&mut self, playlist: PlaylistId, song: SongId, cx: &mut Context<Self>) {
        if self.library.remove_song(playlist, song) {
            self.persist();
            cx.notify();
        }
    }

    /// Remove several songs from a custom playlist at once (a bulk selection).
    pub fn remove_songs(
        &mut self,
        playlist: PlaylistId,
        songs: Vec<SongId>,
        cx: &mut Context<Self>,
    ) {
        if self.library.remove_songs(playlist, &songs) {
            self.persist();
            cx.notify();
        }
    }

    /// Create a custom playlist holding `songs`, and ask the tab container to
    /// open it. Returns its id.
    pub fn new_playlist_from(&mut self, songs: Vec<SongId>, cx: &mut Context<Self>) -> PlaylistId {
        let id = self.library.create_custom("New Playlist");
        self.library.add_songs(id, &songs);
        self.selected = Some(id);
        self.requests.push_back(Request::OpenPlaylist(id));
        self.persist();
        cx.notify();
        id
    }

    /// Play an ad-hoc selection and open it as a temporary playlist — the
    /// song menu's "Play {N} songs". Nothing is persisted; the playlist lives
    /// exactly as long as the tab that shows it.
    pub fn play_selection(
        &mut self,
        songs: Vec<SongId>,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) {
        if songs.is_empty() {
            return;
        }
        let count = songs.len();
        let id = self.library.create_temporary(format!("{count} songs"), songs.clone());
        self.play(&songs, 0, playback, cx);
        self.requests.push_back(Request::OpenPlaylist(id));
        cx.notify();
    }

    /// Play a folder and open it as a folder view: the songs under `dir` —
    /// everything under it for [`FolderMode::Full`], only the folder's own for
    /// [`FolderMode::Here`] — grouped into their albums and singles exactly as
    /// the browse view draws the library, flattened across the subfolders.
    /// Returns whether there was anything to play.
    pub fn play_folder(
        &self,
        dir: &Path,
        mode: FolderMode,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) -> bool {
        let scope = Scope::Folder { dir: dir.to_path_buf(), mode };
        let songs = self.library.select_songs(&Selection {
            scope: &scope,
            query: &Query::default(),
            order: Order::TrackOrder,
        });
        if songs.is_empty() {
            return false;
        }
        self.play(&songs, 0, playback, cx);
        true
    }

    pub fn rename_playlist(&mut self, playlist: PlaylistId, title: String, cx: &mut Context<Self>) {
        if self.library.rename(playlist, title) {
            self.persist();
            cx.notify();
        }
    }

    pub fn delete_playlist(&mut self, playlist: PlaylistId, cx: &mut Context<Self>) {
        if self.library.remove_playlist(playlist).is_some() {
            self.ensure_selection();
            self.persist();
            cx.notify();
        }
    }

    /// Count one play of `song`, regenerate the smart playlists, and save the
    /// stats. Called once per playback — the caller owns the 30-seconds-or-
    /// half-duration rule — never per tick.
    pub fn record_play(&mut self, song: SongId, cx: &mut Context<Self>) {
        self.library.record_play(song, SystemTime::now());
        self.persist_stats();
        cx.notify();
    }

    /// Save the stats — also called on app exit, so a play counted in the
    /// last seconds still lands on disk.
    pub fn persist_stats(&mut self) {
        self.persist();
    }

    /// Replace the play queue with a playlist's songs, starting at `start`.
    pub fn play_playlist(
        &self,
        playlist: PlaylistId,
        start: usize,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) {
        let songs = self
            .library
            .playlist(playlist)
            .map(|playlist| playlist.song_ids.clone())
            .unwrap_or_default();
        self.play(&songs, start, playback, cx);
    }

    /// Replace the play queue with `scope`'s songs, shuffled — the whole
    /// library, or one artist's discography. The order the scope yields is
    /// irrelevant; the shuffle decides the sequence.
    pub fn play_shuffled(
        &self,
        scope: Scope,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) {
        let mut songs = self.library.select_songs(&Selection {
            scope: &scope,
            query: &Query::default(),
            order: Order::TrackOrder,
        });
        shuffle(&mut songs);
        self.play(&songs, 0, playback, cx);
    }

    /// Resolve song ids to queue entries (id + shared metadata). Ids the
    /// library no longer holds (a moved or deleted file) are skipped rather
    /// than yielding a dangling entry.
    pub fn queue_entries(&self, songs: &[SongId]) -> Vec<QueueEntry> {
        songs
            .iter()
            .filter_map(|id| {
                self.library.get_arc(*id).map(|meta| QueueEntry { song: meta.id, meta })
            })
            .collect()
    }

    /// Replace the play queue with `songs`, starting at `start`. Nothing is
    /// persisted — this is how a playlist, or an ad-hoc selection, becomes the
    /// queue.
    pub fn play(
        &self,
        songs: &[SongId],
        start: usize,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) {
        Self::start_queue(playback, self.queue_entries(songs), start, cx);
    }

    /// Hand `entries` to playback, starting at `start`, and notify only if the
    /// queue actually changed. Empty input is a no-op.
    fn start_queue(
        playback: &Entity<PlaybackState>,
        entries: Vec<QueueEntry>,
        start: usize,
        cx: &mut Context<Self>,
    ) {
        if entries.is_empty() {
            return;
        }
        playback.update(cx, |playback, cx| {
            if playback.play_paths(entries, start, cx) {
                cx.notify();
            }
        });
    }

    /// Queue `paths` — loading any the library hasn't scanned yet — and start
    /// at `start`. The command-line entry point.
    pub fn play_files(
        &mut self,
        paths: Vec<PathBuf>,
        start: usize,
        playback: &Entity<PlaybackState>,
        cx: &mut Context<Self>,
    ) {
        Self::start_queue(playback, self.ensure_songs(&paths), start, cx);
    }

    /// Ensure every path is in the library (loading tags for any that aren't),
    /// returning queue entries — so a queued file is always resolvable to
    /// metadata.
    fn ensure_songs(&mut self, paths: &[PathBuf]) -> Vec<QueueEntry> {
        let mut entries = Vec::new();
        for path in paths {
            // `load_song` canonicalizes, reuses a known id, or allocates one
            // and reads the tags. A file that can't be read is skipped.
            let Ok(id) = self.library.load_song(path) else { continue };
            if let Some(meta) = self.library.get_arc(id) {
                entries.push(QueueEntry { song: id, meta });
            }
        }
        entries
    }

    // --- internals -----------------------------------------------------

    /// Drop a selection that points at a playlist which no longer exists.
    fn ensure_selection(&mut self) {
        let valid = self.selected.is_some_and(|id| self.library.playlist(id).is_some());
        if !valid {
            self.selected = None;
        }
    }

    /// Adopt the synced-path list, rescanning (or deferring to the in-flight
    /// scan) when it actually changed.
    fn set_roots(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        if roots == self.roots {
            return;
        }
        self.roots = roots;
        if self.scanning {
            self.rescan_pending = true;
        } else {
            if let Some(cache) = self.cache.as_mut() {
                cache.retain_roots(&self.roots);
            }
            self.rescan(cx);
        }
    }

    /// Apply a scan's result in place, keeping the user's custom playlists,
    /// stats, and playlist id counter — the scan never touches them, and
    /// writes made while it ran are merged here, not overwritten.
    fn adopt_scanned(&mut self, result: crate::model::ScanResult) {
        self.library.adopt_scan(result.songs, result.identities, result.added);
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(cache) = self.cache.take() else {
            self.rescan_pending = true;
            return;
        };
        self.scanning = true;
        let roots = self.roots.clone();
        let identities = self.library.identities();

        cx.spawn(async move |this, cx| {
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    let mut cache = cache;
                    let result = cache.sync(&roots, false, &identities);
                    (cache, result)
                })
                .await;

            this.update(cx, |state, cx| {
                let (cache, result) = scanned;
                state.cache = Some(cache);
                let report = result.report.clone();
                state.adopt_scanned(result);
                state.scanning = false;
                // The synced-path list may have changed while we were scanning.
                if let Some(cache) = state.cache.as_mut() {
                    cache.retain_roots(&state.roots);
                }
                state.ensure_selection();
                state.persist_songs();
                state.persist_stats();
                if !report.errors.is_empty() {
                    eprintln!("scan: {} file(s) could not be read", report.errors.len());
                }
                if state.rescan_pending {
                    state.rescan_pending = false;
                    state.rescan(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Persist the user-owned playlists and stats — what a playlist edit or a
    /// counted play writes. Small, so it's fine on the UI thread; the song
    /// cache and identities are written separately.
    fn persist(&mut self) {
        if let Err(e) = self.store.save_user(&self.library.user_data()) {
            eprintln!("failed to save user data: {e}");
        }
    }

    /// Persist the scanned song cache and the path→id map — written after a
    /// scan, not per edit.
    fn persist_songs(&mut self) {
        if let Some(cache) = self.cache.as_ref() {
            if let Err(e) = cache.save() {
                eprintln!("failed to save library cache: {e}");
            }
        }
        if let Err(e) = self.store.save_identities(&self.library.identities()) {
            eprintln!("failed to save song identities: {e}");
        }
    }
}

/// The immediate subdirectories of `dir`, sorted by name. Empty when the
/// directory can't be read. The rail's folder view flattens the tree with it.
pub fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}
