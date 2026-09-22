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

use std::path::PathBuf;

use gpui::{Context, Entity, Subscription};

use crate::model::{Library, LibraryCache, PlaylistId, SongId};
use crate::ui::config_state::ConfigState;
use crate::ui::menu::SongMenuRequest;
use crate::ui::state::PlaybackState;

pub struct LibraryState {
    library: Library,
    /// Taken out while a scan is in flight, restored when it completes.
    cache: Option<LibraryCache>,
    /// The synced-path list, mirrored from the config so the scan and the
    /// cache-pruning agree on what's covered.
    roots: Vec<PathBuf>,
    /// The playlist whose tab is active, for the rail's highlight.
    selected: Option<PlaylistId>,
    /// Bumped whenever the songs or playlists actually change, so views can
    /// tell a real edit from a highlight-only notification and skip a needless
    /// rebuild.
    revision: u64,
    /// A playlist a view asked to open (the song menu's "Go to playlist"). The
    /// tab container picks it up — so a row in the browse view can open a tab
    /// without holding a handle to the container that owns it.
    pending_open_playlist: Option<PlaylistId>,
    /// An artist a view asked to open a library view for ("Go to artist").
    pending_artist_view: Option<String>,
    /// A song whose context menu a row asked to open. The tab container owns
    /// and renders it, so rows anywhere can raise it.
    pending_song_menu: Option<SongMenuRequest>,
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
        let mut library = Library::default();
        cache.install(&mut library);

        let observe_config = cx.observe(&config, |this, config, cx| {
            let roots = config.read(cx).roots().to_vec();
            this.set_roots(roots, cx);
        });

        let mut this = Self {
            library,
            cache: Some(cache),
            roots,
            selected: None,
            revision: 0,
            pending_open_playlist: None,
            pending_artist_view: None,
            pending_song_menu: None,
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
        self.revision
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    // --- cross-view intents --------------------------------------------

    /// Ask the tab container to open a playlist. Consumed by `TabsView`.
    pub fn request_open_playlist(&mut self, id: PlaylistId, cx: &mut Context<Self>) {
        self.pending_open_playlist = Some(id);
        cx.notify();
    }

    /// Take the pending "open playlist" request, if any.
    pub fn take_open_playlist(&mut self) -> Option<PlaylistId> {
        self.pending_open_playlist.take()
    }

    /// Ask the tab container to open a library view for `artist` — its
    /// discography. Consumed by `TabsView`.
    pub fn request_artist_view(&mut self, artist: String, cx: &mut Context<Self>) {
        self.pending_artist_view = Some(artist);
        cx.notify();
    }

    /// Take the pending "artist view" request, if any.
    pub fn take_artist_view(&mut self) -> Option<String> {
        self.pending_artist_view.take()
    }

    /// Ask the tab container to open a song's context menu at `request`.
    pub fn request_song_menu(&mut self, request: SongMenuRequest, cx: &mut Context<Self>) {
        self.pending_song_menu = Some(request);
        cx.notify();
    }

    /// The pending song-menu request, if any. Read (not taken) so the tab
    /// container can copy it into its own state; call [`clear_song_menu`] after.
    pub fn pending_song_menu(&self) -> Option<&SongMenuRequest> {
        self.pending_song_menu.as_ref()
    }

    pub fn clear_song_menu(&mut self) {
        self.pending_song_menu = None;
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

    /// Create a custom playlist and select it. Returns its id so the caller
    /// can open a tab for it.
    pub fn new_playlist(&mut self, cx: &mut Context<Self>) -> PlaylistId {
        let id = self.library.create_custom("New Playlist");
        self.selected = Some(id);
        self.revision += 1;
        self.persist();
        cx.notify();
        id
    }

    pub fn add_song(&mut self, playlist: PlaylistId, song: SongId, cx: &mut Context<Self>) {
        if self.library.add_song(playlist, song) {
            self.revision += 1;
            self.persist();
            cx.notify();
        }
    }

    /// Add several songs to a custom playlist at once (a bulk selection).
    pub fn add_songs(&mut self, playlist: PlaylistId, songs: Vec<SongId>, cx: &mut Context<Self>) {
        if self.library.add_songs(playlist, &songs) {
            self.revision += 1;
            self.persist();
            cx.notify();
        }
    }

    pub fn remove_song(&mut self, playlist: PlaylistId, song: SongId, cx: &mut Context<Self>) {
        if self.library.remove_song(playlist, song) {
            self.revision += 1;
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
            self.revision += 1;
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
        self.pending_open_playlist = Some(id);
        self.revision += 1;
        self.persist();
        cx.notify();
        id
    }

    pub fn rename_playlist(&mut self, playlist: PlaylistId, title: String, cx: &mut Context<Self>) {
        if self.library.rename(playlist, title) {
            self.revision += 1;
            self.persist();
            cx.notify();
        }
    }

    pub fn delete_playlist(&mut self, playlist: PlaylistId, cx: &mut Context<Self>) {
        if self.library.remove_playlist(playlist).is_some() {
            self.revision += 1;
            self.ensure_selection();
            self.persist();
            cx.notify();
        }
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
        let paths: Vec<PathBuf> = songs
            .iter()
            .filter_map(|id| self.library.get(*id))
            .map(|song| song.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        playback.update(cx, |playback, cx| {
            if playback.play_paths(paths, start, cx) {
                cx.notify();
            }
        });
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

    /// Take the scan's result, keeping the user's custom playlists — which the
    /// scan never touches, but which may have been edited while it ran.
    fn merge_scanned(&mut self, scanned: Library) {
        let previous = std::mem::take(&mut self.library);
        self.library = scanned;
        self.library.adopt_custom_from(&previous);
        // The adopted custom playlists may reference songs the scan dropped
        // (e.g. a removed root), so drop those references too.
        self.library.prune();
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let Some(cache) = self.cache.take() else {
            self.rescan_pending = true;
            return;
        };
        self.scanning = true;
        let roots = self.roots.clone();

        cx.spawn(async move |this, cx| {
            let scanned = cx
                .background_executor()
                .spawn(async move {
                    let mut cache = cache;
                    let mut library = Library::default();
                    cache.install(&mut library);
                    let report = cache.sync(&mut library, &roots, false);
                    (cache, library, report)
                })
                .await;

            this.update(cx, |state, cx| {
                let (cache, library, report) = scanned;
                state.cache = Some(cache);
                state.merge_scanned(library);
                state.scanning = false;
                state.revision += 1;
                // The synced-path list may have changed while we were scanning.
                if let Some(cache) = state.cache.as_mut() {
                    cache.retain_roots(&state.roots);
                }
                state.ensure_selection();
                state.persist();
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

    fn persist(&mut self) {
        // Field-split so the mutable cache and the immutable library don't
        // borrow all of `self` at once.
        let Self { cache, library, .. } = self;
        if let Some(cache) = cache.as_mut() {
            if let Err(e) = cache.save(library) {
                eprintln!("failed to save library cache: {e}");
            }
        }
    }
}
